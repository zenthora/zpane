use super::catalog::{home_dir, non_empty_abs_env_path, read_tail_string, ShellDialect};
use crate::model::{Entry, EntryKind};
use crate::sanitize::sanitize_single_line;
use std::collections::HashSet;
use std::fs;
use std::path::PathBuf;

fn strip_matching_quotes(raw: &str) -> &str {
    let trimmed = raw.trim();
    if trimmed.len() >= 2
        && ((trimmed.starts_with('\'') && trimmed.ends_with('\''))
            || (trimmed.starts_with('"') && trimmed.ends_with('"')))
    {
        &trimmed[1..trimmed.len() - 1]
    } else {
        trimmed
    }
}

fn is_valid_command_ident(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 128
        && !name.starts_with('_')
        && !name.starts_with('-')
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | ':'))
}

/// Strips a trailing `# comment` when preceded by whitespace outside single/double quotes.
fn strip_unquoted_shell_comment(line: &str) -> &str {
    let mut in_single = false;
    let mut in_double = false;
    let mut escaped = false;
    let mut prev_ws = true;
    for (idx, c) in line.char_indices() {
        if escaped {
            escaped = false;
            prev_ws = false;
            continue;
        }
        if c == '\\' && !in_single {
            escaped = true;
            prev_ws = false;
        } else if c == '\'' && !in_double {
            in_single = !in_single;
            prev_ws = false;
        } else if c == '"' && !in_single {
            in_double = !in_double;
            prev_ws = false;
        } else if !in_single && !in_double && c == '#' && prev_ws {
            return line[..idx].trim_end();
        } else {
            prev_ws = c.is_whitespace();
        }
    }
    line.trim_end()
}

/// Extracts the next shell word from `input`, respecting single/double quotes and escapes,
/// stopping at unquoted whitespace or `;` (which terminates a command header).
fn next_shell_token(input: &str) -> Option<(&str, &str)> {
    let trimmed = input.trim_start();
    if trimmed.is_empty() || trimmed.starts_with(';') {
        return None;
    }
    let mut in_single = false;
    let mut in_double = false;
    let mut escaped = false;
    for (idx, c) in trimmed.char_indices() {
        if escaped {
            escaped = false;
            continue;
        }
        if c == '\\' && !in_single {
            escaped = true;
        } else if c == '\'' && !in_double {
            in_single = !in_single;
        } else if c == '"' && !in_single {
            in_double = !in_double;
        } else if !in_single && !in_double && (c.is_whitespace() || c == ';') {
            return Some((&trimmed[..idx], &trimmed[idx..]));
        }
    }
    Some((trimmed, ""))
}

fn strip_command_keyword<'a>(line: &'a str, kw: &str) -> Option<&'a str> {
    let rest = line.strip_prefix(kw)?;
    rest.starts_with(char::is_whitespace)
        .then(|| rest.trim_start())
}

fn extract_fish_function_desc(rest: &str) -> Option<String> {
    // Skip the first token (the function name) so substrings like `-d` inside
    // `my-d-func` or `--description` inside a name cannot trigger a false match.
    let (_, mut cursor) = next_shell_token(rest)?;
    while let Some((tok, next)) = next_shell_token(cursor) {
        cursor = next;
        if tok == "--description" || tok == "-d" {
            if let Some((val, _)) = next_shell_token(cursor) {
                let cleaned = sanitize_single_line(strip_matching_quotes(val));
                if !cleaned.is_empty() {
                    return Some(cleaned.into_owned());
                }
            }
            return None;
        }
        if let Some(val) = tok
            .strip_prefix("--description=")
            .or_else(|| tok.strip_prefix("-d="))
        {
            let cleaned = sanitize_single_line(strip_matching_quotes(val));
            if !cleaned.is_empty() {
                return Some(cleaned.into_owned());
            }
            return None;
        }
    }
    None
}

/// Scans POSIX (`.bashrc`, `.zshrc`, `.profile`) or Fish (`config.fish`, `functions/*.fish`)
/// initialization files from the user's home/config directories.
pub(super) fn collect_shell_rc_files(dialect: ShellDialect) -> Vec<(String, String)> {
    const MAX_RC_BYTES: u64 = 131_072;
    let mut out = Vec::new();
    let Some(home) = home_dir() else {
        return out;
    };
    match dialect {
        ShellDialect::Posix => {
            for name in [
                ".bashrc",
                ".bash_aliases",
                ".zshrc",
                ".zsh_aliases",
                ".profile",
            ] {
                let path = home.join(name);
                if let Some(body) = read_tail_string(&path, MAX_RC_BYTES) {
                    out.push((name.to_string(), body));
                }
            }
            if let Some(zdotdir) = non_empty_abs_env_path("ZDOTDIR").filter(|p| p != &home) {
                for name in [".zshrc", ".zsh_aliases"] {
                    let path = zdotdir.join(name);
                    if let Some(body) = read_tail_string(&path, MAX_RC_BYTES) {
                        out.push((format!("$ZDOTDIR/{name}"), body));
                    }
                }
            }
        }
        ShellDialect::Fish => {
            let config_base =
                non_empty_abs_env_path("XDG_CONFIG_HOME").unwrap_or_else(|| home.join(".config"));
            let fish_dir = config_base.join("fish");
            let main_cfg = fish_dir.join("config.fish");
            if let Some(body) = read_tail_string(&main_cfg, MAX_RC_BYTES) {
                out.push(("config.fish".to_string(), body));
            }
            let fn_dir = fish_dir.join("functions");
            if let Ok(rd) = fs::read_dir(&fn_dir) {
                let mut paths: Vec<PathBuf> = rd
                    .flatten()
                    .map(|e| e.path())
                    .filter(|p| {
                        p.is_file() && p.extension().and_then(|e| e.to_str()) == Some("fish")
                    })
                    .collect();
                paths.sort();
                for path in paths.into_iter().take(200) {
                    if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
                        if let Some(body) = read_tail_string(&path, MAX_RC_BYTES) {
                            out.push((name.to_string(), body));
                        }
                    }
                }
            }
        }
        ShellDialect::Nushell => {
            let mut dirs = Vec::new();
            if let Some(xdg) = non_empty_abs_env_path("XDG_CONFIG_HOME") {
                dirs.push(xdg.join("nushell"));
            }
            if cfg!(windows) {
                if let Some(appdata) = non_empty_abs_env_path("APPDATA") {
                    dirs.push(appdata.join("nushell"));
                }
            }
            dirs.push(home.join(".config").join("nushell"));
            if cfg!(target_os = "macos") {
                dirs.push(
                    home.join("Library")
                        .join("Application Support")
                        .join("nushell"),
                );
            }
            let mut seen_dirs = HashSet::new();
            for dir in dirs {
                if !seen_dirs.insert(dir.clone()) {
                    continue;
                }
                for name in ["config.nu", "env.nu"] {
                    let path = dir.join(name);
                    if let Some(body) = read_tail_string(&path, MAX_RC_BYTES) {
                        out.push((name.to_string(), body));
                    }
                }
            }
        }
        ShellDialect::PowerShell => {}
    }
    out
}

/// Parses aliases, abbreviations, and shell functions from POSIX (`.bashrc`/`.zshrc`/etc.)
/// or Fish (`config.fish` / `functions/*.fish`) configuration snippets.
pub fn parse_posix_fish_definitions(
    dialect: ShellDialect,
    files: &[(String, String)],
) -> Vec<Entry> {
    let mut entries = Vec::new();
    let mut seen = HashSet::new();

    for (source, content) in files {
        let source_clean = sanitize_single_line(source);
        let content = content.strip_prefix('\u{feff}').unwrap_or(content);
        for raw_line in content.lines() {
            let trimmed = raw_line.trim();
            if trimmed.is_empty() || trimmed.starts_with('#') {
                continue;
            }
            let line = strip_unquoted_shell_comment(trimmed);
            if line.is_empty() {
                continue;
            }
            match dialect {
                ShellDialect::Nushell => {
                    let body = strip_command_keyword(line, "export").unwrap_or(line);
                    if let Some(rest) = strip_command_keyword(body, "alias") {
                        if let Some((name_raw, target_raw)) = rest.split_once('=') {
                            let name = strip_matching_quotes(name_raw.trim());
                            let target = sanitize_single_line(
                                target_raw.trim().trim_end_matches(';').trim(),
                            );
                            if is_valid_command_ident(name)
                                && !target.is_empty()
                                && seen.insert(name.to_ascii_lowercase())
                            {
                                entries.push(Entry::new(
                                    name,
                                    name,
                                    EntryKind::Alias,
                                    format!("Alias -> {target}"),
                                ));
                            }
                        }
                        continue;
                    }
                    if let Some(mut rest) = strip_command_keyword(body, "def") {
                        while let Some((tok, next)) = next_shell_token(rest) {
                            if tok.starts_with('-') {
                                rest = next.trim_start();
                            } else {
                                break;
                            }
                        }
                        if let Some((raw_name, _)) = next_shell_token(rest) {
                            let name = raw_name
                                .split(['[', '{', '(', ';'])
                                .next()
                                .unwrap_or("")
                                .trim();
                            if is_valid_command_ident(name)
                                && seen.insert(name.to_ascii_lowercase())
                            {
                                entries.push(Entry::new(
                                    name,
                                    name,
                                    EntryKind::Func,
                                    format!("Nushell command ({source_clean})"),
                                ));
                            }
                        }
                    }
                }
                ShellDialect::PowerShell => {}
                ShellDialect::Posix => {
                    if let Some(mut rest) = strip_command_keyword(line, "alias") {
                        while let Some((tok, next)) = next_shell_token(rest) {
                            if tok == "--" {
                                rest = next.trim_start();
                                break;
                            }
                            if tok.starts_with('-') && !tok.contains('=') {
                                rest = next.trim_start();
                            } else {
                                break;
                            }
                        }
                        if let Some((name_raw, val_raw)) = rest.split_once('=') {
                            let name = strip_matching_quotes(name_raw.trim());
                            if is_valid_command_ident(name)
                                && seen.insert(name.to_ascii_lowercase())
                            {
                                let target = sanitize_single_line(strip_matching_quotes(
                                    val_raw.trim().trim_end_matches(';').trim(),
                                ));
                                entries.push(Entry::new(
                                    name,
                                    name,
                                    EntryKind::Alias,
                                    format!("Alias -> {target}"),
                                ));
                            }
                        }
                        continue;
                    }
                    if let Some(rest) = strip_command_keyword(line, "function") {
                        let name = rest
                            .split(|c: char| c.is_whitespace() || c == '(' || c == '{' || c == ';')
                            .next()
                            .unwrap_or("");
                        if is_valid_command_ident(name) && seen.insert(name.to_ascii_lowercase()) {
                            entries.push(Entry::new(
                                name,
                                name,
                                EntryKind::Func,
                                format!("Shell function ({source_clean})"),
                            ));
                        }
                        continue;
                    }
                    if let Some((head, after)) = line.split_once("()") {
                        let name = head.trim();
                        let after_trim = after.trim_start();
                        let body_follows = after_trim.is_empty()
                            || after_trim.starts_with('{')
                            || after_trim.starts_with('(')
                            || after_trim.starts_with('#');
                        if body_follows
                            && !name.contains(char::is_whitespace)
                            && is_valid_command_ident(name)
                            && seen.insert(name.to_ascii_lowercase())
                        {
                            entries.push(Entry::new(
                                name,
                                name,
                                EntryKind::Func,
                                format!("Shell function ({source_clean})"),
                            ));
                        }
                    }
                }
                ShellDialect::Fish => {
                    if let Some(mut rest) = strip_command_keyword(line, "alias") {
                        while let Some((tok, next)) = next_shell_token(rest) {
                            if tok == "--" {
                                rest = next.trim_start();
                                break;
                            }
                            if matches!(tok, "-s" | "--save") {
                                rest = next.trim_start();
                            } else {
                                break;
                            }
                        }
                        if let Some((first_tok, rem)) = next_shell_token(rest) {
                            let (name_raw, target_raw) =
                                if let Some((n, t)) = first_tok.split_once('=') {
                                    (n, t)
                                } else {
                                    (first_tok, rem.trim().trim_end_matches(';').trim())
                                };
                            let name = strip_matching_quotes(name_raw);
                            if is_valid_command_ident(name)
                                && !target_raw.is_empty()
                                && seen.insert(name.to_ascii_lowercase())
                            {
                                let target =
                                    sanitize_single_line(strip_matching_quotes(target_raw));
                                entries.push(Entry::new(
                                    name,
                                    name,
                                    EntryKind::Alias,
                                    format!("Alias -> {target}"),
                                ));
                            }
                        }
                        continue;
                    }
                    if let Some(rest) = strip_command_keyword(line, "abbr") {
                        let mut cursor = rest;
                        let mut add_mode = false;
                        while let Some((tok, next)) = next_shell_token(cursor) {
                            if tok == "--" {
                                cursor = next;
                                break;
                            }
                            if !tok.starts_with('-') {
                                break;
                            }
                            cursor = next;
                            if tok == "-a"
                                || tok == "--add"
                                || (!tok.starts_with("--") && tok.contains('a'))
                            {
                                add_mode = true;
                            }
                            // Fish abbr flags that consume the following token as a value argument.
                            if matches!(
                                tok,
                                "--position"
                                    | "-c"
                                    | "--command"
                                    | "-r"
                                    | "--regex"
                                    | "-f"
                                    | "--function"
                            ) {
                                if let Some((_, after_val)) = next_shell_token(cursor) {
                                    cursor = after_val;
                                }
                            }
                        }
                        if add_mode {
                            if let Some((first_tok, rem)) = next_shell_token(cursor) {
                                let (name_raw, target_raw) =
                                    if let Some((n, t)) = first_tok.split_once('=') {
                                        (n, t)
                                    } else {
                                        (first_tok, rem.trim().trim_end_matches(';').trim())
                                    };
                                let name = strip_matching_quotes(name_raw);
                                if is_valid_command_ident(name)
                                    && !target_raw.is_empty()
                                    && seen.insert(name.to_ascii_lowercase())
                                {
                                    let target =
                                        sanitize_single_line(strip_matching_quotes(target_raw));
                                    entries.push(Entry::new(
                                        name,
                                        name,
                                        EntryKind::Alias,
                                        format!("Abbr -> {target}"),
                                    ));
                                }
                            }
                        }
                        continue;
                    }
                    if let Some(rest) = strip_command_keyword(line, "function") {
                        if let Some((raw_name, _)) = next_shell_token(rest) {
                            let name = raw_name.trim_end_matches(';');
                            if is_valid_command_ident(name)
                                && seen.insert(name.to_ascii_lowercase())
                            {
                                let desc = extract_fish_function_desc(rest)
                                    .unwrap_or_else(|| format!("Fish function ({source_clean})"));
                                entries.push(Entry::new(name, name, EntryKind::Func, desc));
                            }
                        }
                    }
                }
            }
        }
    }

    entries
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_posix_and_fish_aliases_and_functions() {
        let posix_files = vec![(
            ".zshrc".to_string(),
            r#"
# comment
alias gs='git status'
alias -g L="| less"
mkcd() {
  mkdir -p "$1" && cd "$1"
}
function _helper() { echo skip; }
function deploy { echo deploy; }
"#
            .to_string(),
        )];
        let posix_defs = parse_posix_fish_definitions(ShellDialect::Posix, &posix_files);
        let posix_summary: Vec<(&str, EntryKind)> = posix_defs
            .iter()
            .map(|e| (e.text.as_str(), e.kind))
            .collect();
        assert_eq!(
            posix_summary,
            vec![
                ("gs", EntryKind::Alias),
                ("L", EntryKind::Alias),
                ("mkcd", EntryKind::Func),
                ("deploy", EntryKind::Func),
            ]
        );
        assert_eq!(posix_defs[0].desc, "Alias -> git status");

        let fish_files = vec![(
            "config.fish".to_string(),
            r#"
alias ll='ls -la'
abbr -a gst git status
function _zpane_fish
    echo internal
end
function gco --description 'Checkout git branch'
    git checkout $argv
end
"#
            .to_string(),
        )];
        let fish_defs = parse_posix_fish_definitions(ShellDialect::Fish, &fish_files);
        let fish_summary: Vec<(&str, EntryKind)> = fish_defs
            .iter()
            .map(|e| (e.text.as_str(), e.kind))
            .collect();
        assert_eq!(
            fish_summary,
            vec![
                ("ll", EntryKind::Alias),
                ("gst", EntryKind::Alias),
                ("gco", EntryKind::Func),
            ]
        );
        assert_eq!(fish_defs[1].desc, "Abbr -> git status");
        assert_eq!(fish_defs[2].desc, "Checkout git branch");
    }

    #[test]
    fn test_parse_posix_fish_definitions_handles_comments_flags_and_desc_substrings() {
        let posix_files = vec![(
            ".zshrc".to_string(),
            "\u{feff}alias -- ll='ls -la'; # show hidden\n\
             alias grep_hash='rg \"#todo\"' # inline comment\n\
             function  spaced_fn () { echo ok; }\n\
             not_fn=$(sub())\n\
             real_fn() { echo real; }\n"
                .to_string(),
        )];
        let posix_defs = parse_posix_fish_definitions(ShellDialect::Posix, &posix_files);
        let posix_items: Vec<(&str, EntryKind, &str)> = posix_defs
            .iter()
            .map(|e| (e.text.as_str(), e.kind, e.desc.as_str()))
            .collect();
        assert_eq!(
            posix_items,
            vec![
                ("ll", EntryKind::Alias, "Alias -> ls -la"),
                ("grep_hash", EntryKind::Alias, "Alias -> rg \"#todo\""),
                ("spaced_fn", EntryKind::Func, "Shell function (.zshrc)"),
                ("real_fn", EntryKind::Func, "Shell function (.zshrc)"),
            ]
        );

        let fish_files = vec![(
            "config.fish".to_string(),
            "\u{feff}alias mygrep 'rg --color=always' # space form with =\n\
             abbr -a --position anywhere -- gc git commit\n\
             abbr -a -c git co checkout\n\
             abbr --erase stale\n\
             function my-d-func\n\
                 echo $argv\n\
             end\n\
             function grep_it -d 'Find text'; echo -d ignored; end\n"
                .to_string(),
        )];
        let fish_defs = parse_posix_fish_definitions(ShellDialect::Fish, &fish_files);
        let fish_items: Vec<(&str, EntryKind, &str)> = fish_defs
            .iter()
            .map(|e| (e.text.as_str(), e.kind, e.desc.as_str()))
            .collect();
        assert_eq!(
            fish_items,
            vec![
                ("mygrep", EntryKind::Alias, "Alias -> rg --color=always"),
                ("gc", EntryKind::Alias, "Abbr -> git commit"),
                ("co", EntryKind::Alias, "Abbr -> checkout"),
                ("my-d-func", EntryKind::Func, "Fish function (config.fish)"),
                ("grep_it", EntryKind::Func, "Find text"),
            ]
        );
    }

    #[test]
    fn test_parse_nushell_aliases_and_custom_commands() {
        let nu_files = vec![(
            "config.nu".to_string(),
            r#"
# comment
alias gs = git status
export alias ll = ls -la # inline comment
def deploy [env: string] { print $env }
export def --env mkcd [dir: path] { mkdir $dir; cd $dir }
def --wrapped _private [args] { echo skip }
"#
            .to_string(),
        )];
        let nu_defs = parse_posix_fish_definitions(ShellDialect::Nushell, &nu_files);
        let summary: Vec<(&str, EntryKind, &str)> = nu_defs
            .iter()
            .map(|e| (e.text.as_str(), e.kind, e.desc.as_str()))
            .collect();
        assert_eq!(
            summary,
            vec![
                ("gs", EntryKind::Alias, "Alias -> git status"),
                ("ll", EntryKind::Alias, "Alias -> ls -la"),
                ("deploy", EntryKind::Func, "Nushell command (config.nu)"),
                ("mkcd", EntryKind::Func, "Nushell command (config.nu)"),
            ]
        );
    }
}
