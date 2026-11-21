use super::catalog::{home_dir, non_empty_abs_env_path, read_tail_string, ShellDialect};
use crate::sanitize::sanitize_single_line;
use std::borrow::Cow;
use std::collections::HashSet;
use std::env;
use std::path::PathBuf;

pub(super) const MAX_HISTORY_ITEMS: usize = 10_000;

fn unescape_fish_history_cmd(raw: &str) -> Cow<'_, str> {
    if !raw.contains('\\') {
        return Cow::Borrowed(raw);
    }
    let mut out = String::with_capacity(raw.len());
    let mut chars = raw.chars();
    while let Some(c) = chars.next() {
        if c == '\\' {
            match chars.next() {
                Some('n') => out.push(' '),
                Some('\\') => out.push('\\'),
                Some(other) => {
                    out.push('\\');
                    out.push(other);
                }
                None => out.push('\\'),
            }
        } else {
            out.push(c);
        }
    }
    Cow::Owned(out)
}

fn strip_zsh_extended_timestamp(line: &str) -> &str {
    let Some(rest) = line.strip_prefix(": ") else {
        return line;
    };
    let rest = rest.trim_start();
    let Some((header, cmd)) = rest.split_once(';') else {
        return line;
    };
    let Some((ts, elapsed)) = header.split_once(':') else {
        return line;
    };
    if !ts.is_empty()
        && ts.bytes().all(|b| b.is_ascii_digit())
        && !elapsed.is_empty()
        && elapsed.bytes().all(|b| b.is_ascii_digit())
    {
        cmd.trim()
    } else {
        line
    }
}

fn has_odd_trailing_escape(s: &str, escape_char: char) -> bool {
    let count = s.chars().rev().take_while(|&c| c == escape_char).count();
    count % 2 == 1
}

/// Parses recent command history lines (newest first, deduplicated case-insensitively)
/// for Bash, Zsh extended history, Fish YAML history, PowerShell PSReadLine history,
/// or Nushell plaintext `history.txt` (skipping SQLite binary history cleanly).
pub fn parse_shell_history_from_str(
    dialect: ShellDialect,
    raw: &str,
    max_items: usize,
) -> Vec<String> {
    if max_items == 0 {
        return Vec::new();
    }
    if dialect == ShellDialect::Nushell && raw.starts_with("SQLite format 3") {
        return Vec::new();
    }
    let max_items = max_items.min(MAX_HISTORY_ITEMS);
    const MAX_RAW_TAIL: usize = 2 * 1024 * 1024;
    let raw = if raw.len() > MAX_RAW_TAIL {
        let mut start = raw.len() - MAX_RAW_TAIL;
        while !raw.is_char_boundary(start) {
            start += 1;
        }
        match raw[start..].split_once('\n') {
            Some((_, rest)) => rest,
            None => &raw[start..],
        }
    } else {
        raw
    };

    let raw = raw.strip_prefix('\u{feff}').unwrap_or(raw);
    let mut extracted: Vec<(Cow<'_, str>, bool)> = Vec::new();
    let mut pending: Option<String> = None;

    for raw_line in raw.lines() {
        let trimmed_end = raw_line.trim_end();
        let trimmed = trimmed_end.trim_start();
        match dialect {
            ShellDialect::Nushell => {
                if trimmed.is_empty() || trimmed.starts_with('#') {
                    continue;
                }
                if trimmed.contains("<newline>") {
                    extracted.push((Cow::Owned(trimmed.replace("<newline>", " ")), false));
                } else {
                    extracted.push((Cow::Borrowed(trimmed), false));
                }
            }
            ShellDialect::Fish => {
                if trimmed.is_empty() {
                    continue;
                }
                if let Some(cmd) = trimmed.strip_prefix("- cmd:") {
                    let t = cmd.trim();
                    if !t.is_empty() {
                        extracted.push((Cow::Borrowed(t), true));
                    }
                } else if !trimmed.starts_with("when:")
                    && !trimmed.starts_with("paths:")
                    && !trimmed.starts_with('-')
                {
                    extracted.push((Cow::Borrowed(trimmed), false));
                }
            }
            ShellDialect::Posix | ShellDialect::PowerShell => {
                let escape_char = match dialect {
                    ShellDialect::Posix => '\\',
                    ShellDialect::PowerShell => '`',
                    ShellDialect::Fish | ShellDialect::Nushell => unreachable!(),
                };
                if let Some(mut buf) = pending.take() {
                    if has_odd_trailing_escape(trimmed, escape_char) {
                        let part = &trimmed[..trimmed.len() - escape_char.len_utf8()];
                        let part = part.trim_end();
                        if !part.is_empty() && buf.len() < 4096 {
                            if !buf.is_empty() {
                                buf.push(' ');
                            }
                            buf.push_str(part);
                        }
                        pending = Some(buf);
                    } else {
                        if !trimmed.is_empty() && buf.len() < 4096 {
                            if !buf.is_empty() {
                                buf.push(' ');
                            }
                            buf.push_str(trimmed);
                        }
                        if !buf.is_empty() {
                            extracted.push((Cow::Owned(buf), false));
                        }
                    }
                    continue;
                }

                if trimmed.is_empty() || trimmed.starts_with('#') {
                    continue;
                }
                let cmd = if dialect == ShellDialect::Posix {
                    strip_zsh_extended_timestamp(trimmed)
                } else {
                    trimmed
                };
                if cmd.is_empty() {
                    continue;
                }
                if has_odd_trailing_escape(cmd, escape_char) {
                    let prefix = cmd[..cmd.len() - escape_char.len_utf8()].trim_end();
                    pending = Some(prefix.to_string());
                } else {
                    extracted.push((Cow::Borrowed(cmd), false));
                }
            }
        }
    }

    if let Some(leftover) = pending.filter(|s| !s.is_empty()) {
        extracted.push((Cow::Owned(leftover), false));
    }

    let cap = max_items.min(256);
    let mut seen = HashSet::with_capacity(cap);
    let mut out = Vec::with_capacity(cap);
    for (raw_cmd, needs_fish_unescape) in extracted.into_iter().rev() {
        let unescaped = if needs_fish_unescape {
            unescape_fish_history_cmd(&raw_cmd)
        } else {
            raw_cmd
        };
        let cleaned = sanitize_single_line(unescaped.trim());
        if cleaned.is_empty() || cleaned.len() > 4096 {
            continue;
        }
        let lower = cleaned.to_ascii_lowercase();
        if seen.insert(lower) {
            out.push(cleaned.into_owned());
            if out.len() >= max_items {
                break;
            }
        }
    }
    out
}

#[allow(clippy::too_many_arguments)]
pub(super) fn detect_shell_history_with_source_from(
    dialect: ShellDialect,
    limit: usize,
    is_windows: bool,
    is_macos: bool,
    histfile: Option<&str>,
    shell: Option<&str>,
    appdata: Option<&std::path::Path>,
    xdg_data_home: Option<&std::path::Path>,
    xdg_config_home: Option<&std::path::Path>,
    home: Option<&std::path::Path>,
) -> (Option<PathBuf>, Vec<String>) {
    const MAX_HIST_BYTES: u64 = 262_144;
    if limit == 0 {
        return (None, Vec::new());
    }

    let mut candidates = Vec::new();
    if let Some(histfile) = histfile {
        let trimmed = histfile.trim();
        if !trimmed.is_empty() && !trimmed.contains('\0') {
            candidates.push(PathBuf::from(trimmed));
        }
    }

    match dialect {
        ShellDialect::PowerShell => {
            if is_windows {
                if let Some(appdata) = appdata {
                    candidates.push(
                        appdata
                            .join("Microsoft")
                            .join("Windows")
                            .join("PowerShell")
                            .join("PSReadLine")
                            .join("ConsoleHost_history.txt"),
                    );
                }
            } else if let Some(home) = home {
                candidates.push(
                    home.join(".local")
                        .join("share")
                        .join("powershell")
                        .join("PSReadLine")
                        .join("ConsoleHost_history.txt"),
                );
            }
        }
        ShellDialect::Fish => {
            if let Some(xdg) = xdg_data_home {
                candidates.push(xdg.join("fish").join("fish_history"));
            }
            if let Some(home) = home {
                candidates.push(
                    home.join(".local")
                        .join("share")
                        .join("fish")
                        .join("fish_history"),
                );
            }
        }
        ShellDialect::Nushell => {
            if let Some(xdg) = xdg_config_home {
                candidates.push(xdg.join("nushell").join("history.txt"));
            }
            if is_windows {
                if let Some(appdata) = appdata {
                    candidates.push(appdata.join("nushell").join("history.txt"));
                }
            }
            if let Some(home) = home {
                candidates.push(home.join(".config").join("nushell").join("history.txt"));
                if is_macos {
                    candidates.push(
                        home.join("Library")
                            .join("Application Support")
                            .join("nushell")
                            .join("history.txt"),
                    );
                }
            }
        }
        ShellDialect::Posix => {
            if let Some(home) = home {
                let prefers_zsh = shell
                    .map(|s| {
                        let t = s.trim();
                        t.ends_with("/zsh") || t == "zsh"
                    })
                    .unwrap_or(false);
                if prefers_zsh {
                    candidates.push(home.join(".zsh_history"));
                    candidates.push(home.join(".bash_history"));
                } else {
                    candidates.push(home.join(".bash_history"));
                    candidates.push(home.join(".zsh_history"));
                }
            }
        }
    }

    let mut first_readable: Option<PathBuf> = None;
    for path in candidates {
        if let Some(raw) = read_tail_string(&path, MAX_HIST_BYTES) {
            if first_readable.is_none() {
                first_readable = Some(path.clone());
            }
            let parsed = parse_shell_history_from_str(dialect, &raw, limit);
            if !parsed.is_empty() {
                return (Some(path), parsed);
            }
        }
    }

    (first_readable, Vec::new())
}

/// Reads the tail of the active shell's history file for `dialect` and returns up to `limit`
/// deduplicated recent commands (newest first).
pub(super) fn detect_shell_history(dialect: ShellDialect, limit: usize) -> Vec<String> {
    let histfile = env::var("HISTFILE").ok();
    let shell = env::var("SHELL").ok();
    detect_shell_history_with_source_from(
        dialect,
        limit,
        cfg!(windows),
        cfg!(target_os = "macos"),
        histfile.as_deref(),
        shell.as_deref(),
        non_empty_abs_env_path("APPDATA").as_deref(),
        non_empty_abs_env_path("XDG_DATA_HOME").as_deref(),
        non_empty_abs_env_path("XDG_CONFIG_HOME").as_deref(),
        home_dir().as_deref(),
    )
    .1
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_shell_history_across_zsh_bash_and_fish() {
        let zsh_raw = "\
: 1700000001:0;git status
: 1700000002:0;cargo test --locked
: 1700000003:0;git status
: 1700000004:0;rg --files src/
";
        assert_eq!(
            parse_shell_history_from_str(ShellDialect::Posix, zsh_raw, 10),
            vec!["rg --files src/", "git status", "cargo test --locked"]
        );

        let bash_raw = "\
#1700000001
ls -la
#1700000002
docker compose up -d
";
        assert_eq!(
            parse_shell_history_from_str(ShellDialect::Posix, bash_raw, 1),
            vec!["docker compose up -d"]
        );

        let fish_raw = "\
- cmd: git checkout main
  when: 1700000001
- cmd: echo a\\nb
  when: 1700000002
  paths:
    - src/main.rs
";
        assert_eq!(
            parse_shell_history_from_str(ShellDialect::Fish, fish_raw, 10),
            vec!["echo a b", "git checkout main"]
        );
    }

    #[test]
    fn test_parse_shell_history_handles_bom_continuations_unescaping_and_timestamps() {
        let fish_raw = "\u{feff}- cmd: echo C:\\\\new\\\\dir\\nnext\n  when: 1700000001\n";
        assert_eq!(
            parse_shell_history_from_str(ShellDialect::Fish, fish_raw, 10),
            vec!["echo C:\\new\\dir next"]
        );

        let posix_raw =
            "\u{feff}: 1700000001:0;git commit \\\n  -m \"fix\"\n: check; echo ok\necho foo\\\\\n";
        assert_eq!(
            parse_shell_history_from_str(ShellDialect::Posix, posix_raw, 10),
            vec!["echo foo\\\\", ": check; echo ok", "git commit -m \"fix\"",]
        );

        let ps_raw = "Get-ChildItem `\n  -Path src\n";
        assert_eq!(
            parse_shell_history_from_str(ShellDialect::PowerShell, ps_raw, 10),
            vec!["Get-ChildItem -Path src"]
        );
    }

    #[test]
    fn test_parse_shell_history_nushell_plaintext_multiline_and_sqlite_header() {
        let nu_raw = "\u{feff}\
# comment
git status
ls | where size > 1kb
echo a<newline>echo b
git status
";
        assert_eq!(
            parse_shell_history_from_str(ShellDialect::Nushell, nu_raw, 10),
            vec!["git status", "echo a echo b", "ls | where size > 1kb"]
        );

        let sqlite_header = "SQLite format 3\0binary payload\ngit status\n";
        assert!(
            parse_shell_history_from_str(ShellDialect::Nushell, sqlite_header, 10).is_empty(),
            "Nushell SQLite binary history must be skipped cleanly without garbage"
        );
    }
}
