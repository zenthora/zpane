use crate::model::{Entry, EntryKind};
use regex::Regex;
use std::borrow::Cow;
use std::collections::HashSet;
use std::sync::OnceLock;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HelpSection {
    None,
    Commands,
    Flags,
    Ignore,
}

fn strip_ansi_escapes(text: &str) -> Cow<'_, str> {
    if !text.contains('\x1b') {
        return Cow::Borrowed(text);
    }
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch == '\x1b' {
            match chars.peek().copied() {
                Some('[') => {
                    chars.next();
                    for c in chars.by_ref() {
                        if ('@'..='~').contains(&c) {
                            break;
                        }
                    }
                }
                Some(']') => {
                    chars.next();
                    while let Some(c) = chars.next() {
                        if c == '\x07' || (c == '\x1b' && chars.peek() == Some(&'\\')) {
                            if c == '\x1b' {
                                chars.next();
                            }
                            break;
                        }
                    }
                }
                Some(_) => {
                    chars.next();
                }
                None => {}
            }
        } else {
            out.push(ch);
        }
    }
    Cow::Owned(out)
}

fn leading_ws_count(line: &str) -> usize {
    line.chars().take_while(|c| c.is_whitespace()).count()
}

fn classify_section_header(line: &str) -> Option<HelpSection> {
    let trimmed = line.trim();
    if trimmed.is_empty() || trimmed.starts_with('-') {
        return None;
    }

    let leading_spaces = leading_ws_count(line);
    let has_column_gap = trimmed.contains("  ") || trimmed.contains('\t');
    let lower = trimmed.to_ascii_lowercase();

    if lower.starts_with("usage:")
        || (lower.starts_with("usage") && leading_spaces <= 1 && !has_column_gap)
    {
        return Some(HelpSection::Ignore);
    }

    let is_all_caps = trimmed.len() >= 3
        && trimmed
            .chars()
            .all(|c| !c.is_alphabetic() || c.is_uppercase());
    let ends_with_colon = trimmed.ends_with(':');

    if !has_column_gap
        && (leading_spaces <= 1 || ends_with_colon || is_all_caps)
        && (lower.starts_with("examples")
            || lower.starts_with("learn more")
            || lower.starts_with("see '")
            || lower.starts_with("help topics")
            || lower.starts_with("environment:")
            || lower.starts_with("topics"))
    {
        return Some(HelpSection::Ignore);
    }

    if has_column_gap {
        return None;
    }

    if lower.starts_with("start a working area")
        || lower.starts_with("work on the current change")
        || lower.starts_with("examine the history")
        || lower.starts_with("grow, mark and tweak")
        || lower.starts_with("collaborate")
        || lower.starts_with("main porcelain commands")
        || lower.starts_with("ancillary commands")
        || lower.starts_with("the following commands are available")
    {
        return Some(HelpSection::Commands);
    }

    if lower.starts_with("the following options are available") {
        return Some(HelpSection::Flags);
    }

    if ends_with_colon || is_all_caps {
        if lower.contains("command") || lower.contains("subcommand") {
            return Some(HelpSection::Commands);
        }
        if lower.contains("flag")
            || lower.contains("option")
            || lower.contains("argument")
            || lower.contains("selection")
            || lower.contains("switch")
            || lower.contains("parameter")
        {
            return Some(HelpSection::Flags);
        }
    }

    None
}

fn build_flag_entry(short: &str, raw_flag: &str, arg_hint: &str, desc: &str) -> Entry {
    let flag = raw_flag.replacen("--[no-]", "--", 1);
    let kind = if arg_hint.is_empty() {
        EntryKind::Flag
    } else {
        EntryKind::Param
    };

    let label = if !short.is_empty() && short != flag {
        if !arg_hint.is_empty() {
            format!("{flag} {arg_hint} ({short})")
        } else {
            format!("{flag} ({short})")
        }
    } else if !arg_hint.is_empty() {
        format!("{flag} {arg_hint}")
    } else {
        flag.clone()
    };

    let mut entry = Entry::new(&flag, label, kind, desc);
    if !short.is_empty() && short != flag {
        entry.aliases.push(short.to_string());
    }
    entry
}

fn populate_flag_metadata(entry: &mut Entry) {
    static RE_POSSIBLE_VALS: OnceLock<Regex> = OnceLock::new();
    static RE_DEFAULT_VAL: OnceLock<Regex> = OnceLock::new();

    let re_vals = RE_POSSIBLE_VALS.get_or_init(|| {
        Regex::new(r"(?:\[possible values:\s*([^\]]+)\]|Allowed values:\s*([^.\n]+))").unwrap()
    });
    let re_def = RE_DEFAULT_VAL.get_or_init(|| Regex::new(r"\[default:\s*([^\]]+)\]").unwrap());

    if let Some(v_caps) = re_vals.captures(&entry.desc) {
        let raw_vals = v_caps
            .get(1)
            .or_else(|| v_caps.get(2))
            .map(|m| m.as_str())
            .unwrap_or("");
        for v in raw_vals
            .split(',')
            .map(|s| s.trim())
            .filter(|s| !s.is_empty())
        {
            if !entry.children.iter().any(|c| c.text == v) {
                entry.children.push(Entry::new(
                    v,
                    v,
                    EntryKind::Enum,
                    format!("Allowed value for {}", entry.text),
                ));
            }
        }
        if entry.children.iter().any(|c| c.kind == EntryKind::Enum) {
            entry.kind = EntryKind::Param;
        }
    }

    if let Some(d_caps) = re_def.captures(&entry.desc) {
        let def_val = d_caps.get(1).map(|m| m.as_str().trim()).unwrap_or("");
        if !def_val.is_empty() && !entry.children.iter().any(|c| c.text == def_val) {
            entry.children.push(Entry::new(
                def_val,
                format!("{def_val} [default]"),
                EntryKind::Value,
                format!("Default value for {}", entry.text),
            ));
        }
    }
}

fn append_desc_line(entry: &mut Entry, trimmed_line: &str) {
    if trimmed_line.is_empty() {
        return;
    }
    if !entry.desc.is_empty() {
        entry.desc.push(' ');
    }
    entry.desc.push_str(trimmed_line);
}

/// Parses `--help` / `-h` output from Cobra, Clap, winget, git, and generic CLIs
/// into Pane 2/3 [`Entry`] values (subcommands, Parameters, Flags, enums, and defaults).
pub(super) fn parse_cli_help(help_text: &str) -> Vec<Entry> {
    static RE_SECTION_SUBCMD: OnceLock<Regex> = OnceLock::new();
    static RE_SECTION_SUBCMD_HEADER: OnceLock<Regex> = OnceLock::new();
    static RE_SECTION_FLAG: OnceLock<Regex> = OnceLock::new();
    static RE_SECTION_FLAG_HEADER: OnceLock<Regex> = OnceLock::new();
    static RE_LONG_FLAG_FALLBACK: OnceLock<Regex> = OnceLock::new();
    static RE_SHORT_FLAG_FALLBACK: OnceLock<Regex> = OnceLock::new();
    static RE_SUBCMD_FALLBACK: OnceLock<Regex> = OnceLock::new();

    let re_sec_sub = RE_SECTION_SUBCMD.get_or_init(|| {
        Regex::new(r"^\s{2,8}([a-zA-Z0-9][a-zA-Z0-9_-]*)(?::|,\s*([a-zA-Z0-9][a-zA-Z0-9_-]*))?(?:\s{2,}|\t|\s*:\s+)(.+)$")
            .unwrap()
    });
    let re_sec_sub_header = RE_SECTION_SUBCMD_HEADER.get_or_init(|| {
        Regex::new(
            r"^\s{2,6}([a-zA-Z0-9][a-zA-Z0-9_-]{1,24})(?:,\s*([a-zA-Z0-9][a-zA-Z0-9_-]*))?:?\s*$",
        )
        .unwrap()
    });

    let re_sec_flag = RE_SECTION_FLAG.get_or_init(|| {
        Regex::new(r"^\s{1,10}(?:(-[a-zA-Z0-9\?])(?:\s+(?:<[^>]+>|\[[^\]]+\]|[A-Za-z0-9_-]+)\s*(?:,|\|)|,?\s*(?:\||,)?)\s*)?(--(?:\[no-\])?[a-zA-Z0-9][a-zA-Z0-9_\.-]*|-[a-zA-Z0-9\?])(?:[ =]([^\s].*?)|(\[[^\]]+\]))?(?:\s{2,}|\t)(.+)$")
            .unwrap()
    });
    let re_sec_flag_header = RE_SECTION_FLAG_HEADER.get_or_init(|| {
        Regex::new(r"^\s{1,10}(?:(-[a-zA-Z0-9\?])(?:\s+(?:<[^>]+>|\[[^\]]+\]|[A-Za-z0-9_-]+)\s*(?:,|\|)|,?\s*(?:\||,)?)\s*)?(--(?:\[no-\])?[a-zA-Z0-9][a-zA-Z0-9_\.-]*|-[a-zA-Z0-9\?])(?: (<[^>]+>|\[[^\]]+\]|[A-Z][A-Z0-9_.-]*)|=([^\s]+)|(\[[^\]]+\]))?\s*$")
            .unwrap()
    });

    let clean_help = strip_ansi_escapes(help_text);

    let mut subcmds: Vec<Entry> = Vec::new();
    let mut flags: Vec<Entry> = Vec::new();
    let mut seen = HashSet::new();

    let mut current_sec = HelpSection::None;
    let mut detected_sections = false;
    let mut active_item: Option<(HelpSection, usize, usize)> = None;
    let mut section_cmd_indent: Option<usize> = None;

    for line in clean_help.lines() {
        if let Some(sec) = classify_section_header(line) {
            current_sec = sec;
            detected_sections = true;
            active_item = None;
            section_cmd_indent = None;
            continue;
        }

        let line_indent = leading_ws_count(line);
        let trimmed = line.trim();

        match current_sec {
            HelpSection::Commands => {
                let within_cmd_col = section_cmd_indent.is_none_or(|ci| line_indent <= ci + 2);
                if within_cmd_col {
                    if let Some(caps) = re_sec_sub.captures(line) {
                        let cmd = caps.get(1).map(|m| m.as_str().trim()).unwrap_or("");
                        let alias = caps.get(2).map(|m| m.as_str().trim()).unwrap_or("");
                        let desc = caps.get(3).map(|m| m.as_str().trim()).unwrap_or("");

                        if !cmd.is_empty() && seen.insert(cmd.to_string()) {
                            section_cmd_indent.get_or_insert(line_indent);
                            let label = if alias.is_empty() {
                                cmd.to_string()
                            } else {
                                format!("{cmd} ({alias})")
                            };
                            let mut entry = Entry::new(cmd, label, EntryKind::Subcmd, desc);
                            if !alias.is_empty() {
                                entry.aliases.push(alias.to_string());
                            }
                            subcmds.push(entry);
                            active_item =
                                Some((HelpSection::Commands, subcmds.len() - 1, line_indent));
                            continue;
                        }
                    } else if active_item
                        .as_ref()
                        .is_none_or(|(_, _, ai)| line_indent <= *ai)
                    {
                        if let Some(caps) = re_sec_sub_header.captures(line) {
                            let cmd = caps.get(1).map(|m| m.as_str().trim()).unwrap_or("");
                            let alias = caps.get(2).map(|m| m.as_str().trim()).unwrap_or("");
                            if !cmd.is_empty() && seen.insert(cmd.to_string()) {
                                section_cmd_indent.get_or_insert(line_indent);
                                let label = if alias.is_empty() {
                                    cmd.to_string()
                                } else {
                                    format!("{cmd} ({alias})")
                                };
                                let mut entry = Entry::new(cmd, label, EntryKind::Subcmd, "");
                                if !alias.is_empty() {
                                    entry.aliases.push(alias.to_string());
                                }
                                subcmds.push(entry);
                                active_item =
                                    Some((HelpSection::Commands, subcmds.len() - 1, line_indent));
                                continue;
                            }
                        }
                    }
                }

                if let Some((HelpSection::Commands, idx, item_indent)) = active_item {
                    if !trimmed.is_empty() {
                        if line_indent > item_indent {
                            append_desc_line(&mut subcmds[idx], trimmed);
                        } else {
                            active_item = None;
                        }
                    }
                }
            }
            HelpSection::Flags => {
                if let Some(caps) = re_sec_flag.captures(line) {
                    let short = caps.get(1).map(|m| m.as_str().trim()).unwrap_or("");
                    let raw_flag = caps.get(2).map(|m| m.as_str().trim()).unwrap_or("");
                    let arg_hint = caps
                        .get(3)
                        .or_else(|| caps.get(4))
                        .map(|m| m.as_str().trim())
                        .unwrap_or("");
                    let desc = caps.get(5).map(|m| m.as_str().trim()).unwrap_or("");
                    let entry = build_flag_entry(short, raw_flag, arg_hint, desc);
                    if seen.insert(entry.text.clone()) {
                        flags.push(entry);
                        active_item = Some((HelpSection::Flags, flags.len() - 1, line_indent));
                    } else {
                        active_item = None;
                    }
                } else if let Some(caps) = re_sec_flag_header.captures(line) {
                    let short = caps.get(1).map(|m| m.as_str().trim()).unwrap_or("");
                    let raw_flag = caps.get(2).map(|m| m.as_str().trim()).unwrap_or("");
                    let arg_hint = caps
                        .get(3)
                        .or_else(|| caps.get(4))
                        .or_else(|| caps.get(5))
                        .map(|m| m.as_str().trim())
                        .unwrap_or("");
                    let entry = build_flag_entry(short, raw_flag, arg_hint, "");
                    if seen.insert(entry.text.clone()) {
                        flags.push(entry);
                        active_item = Some((HelpSection::Flags, flags.len() - 1, line_indent));
                    } else {
                        active_item = None;
                    }
                } else if let Some((HelpSection::Flags, idx, item_indent)) = active_item {
                    if !trimmed.is_empty() {
                        if line_indent > item_indent {
                            append_desc_line(&mut flags[idx], trimmed);
                        } else {
                            active_item = None;
                        }
                    }
                }
            }
            HelpSection::Ignore | HelpSection::None => {
                active_item = None;
            }
        }
    }

    if !detected_sections || (subcmds.is_empty() && flags.is_empty()) {
        let re_long = RE_LONG_FLAG_FALLBACK.get_or_init(|| {
            Regex::new(r"^\s{1,10}(?:(-[a-zA-Z0-9\?])(?:\s+(?:<[^>]+>|\[[^\]]+\]|[A-Za-z0-9_-]+)\s*(?:,|\|)|,?\s*(?:\||,)?)\s*)?(--(?:\[no-\])?[a-zA-Z0-9][a-zA-Z0-9_-]*)(?:[ =](<[^>]+>|\[[^\]]+\]|[A-Z][A-Z0-9_.-]*|[^\s\]]+\]?)|(\[[^\]]+\]))?(?:\s{2,}(.+))?$").unwrap()
        });
        let re_short = RE_SHORT_FLAG_FALLBACK.get_or_init(|| {
            Regex::new(r"^\s{1,10}(-[a-zA-Z0-9\?])(?:\s+(<[^>]+>|\[[^\]]+\]))?\s{2,}(.+)$").unwrap()
        });
        let re_sub = RE_SUBCMD_FALLBACK.get_or_init(|| {
            Regex::new(r"^\s{2,8}([a-z][a-z0-9_-]{1,24})(?:,\s*([a-z0-9_-]+))?\s{2,}([A-Za-z].+)$")
                .unwrap()
        });

        let ignored_words: HashSet<&'static str> = [
            "usage",
            "options",
            "commands",
            "arguments",
            "flags",
            "examples",
            "env",
            "note",
            "default",
            "or",
            "see",
            "start",
            "work",
            "examine",
            "grow",
            "collaborate",
        ]
        .into_iter()
        .collect();

        let mut active_fallback_flag: Option<(usize, usize)> = None;

        for line in clean_help.lines() {
            let line_indent = leading_ws_count(line);
            let trimmed = line.trim();

            if let Some(caps) = re_long.captures(line) {
                let short = caps.get(1).map(|m| m.as_str().trim()).unwrap_or("");
                let raw_long = caps.get(2).map(|m| m.as_str().trim()).unwrap_or("");
                let arg_hint = caps
                    .get(3)
                    .or_else(|| caps.get(4))
                    .map(|m| m.as_str().trim())
                    .unwrap_or("");
                let desc = caps.get(5).map(|m| m.as_str().trim()).unwrap_or("");
                let entry = build_flag_entry(short, raw_long, arg_hint, desc);
                if seen.insert(entry.text.clone()) {
                    flags.push(entry);
                    active_fallback_flag = Some((flags.len() - 1, line_indent));
                } else {
                    active_fallback_flag = None;
                }
            } else if let Some(caps) = re_short.captures(line) {
                let short = caps.get(1).map(|m| m.as_str().trim()).unwrap_or("");
                let arg_hint = caps.get(2).map(|m| m.as_str().trim()).unwrap_or("");
                let desc = caps.get(3).map(|m| m.as_str().trim()).unwrap_or("");
                let entry = build_flag_entry("", short, arg_hint, desc);
                if seen.insert(entry.text.clone()) {
                    flags.push(entry);
                    active_fallback_flag = Some((flags.len() - 1, line_indent));
                } else {
                    active_fallback_flag = None;
                }
            } else if let Some(caps) = re_sub.captures(line) {
                active_fallback_flag = None;
                let sub = caps.get(1).map(|m| m.as_str().trim()).unwrap_or("");
                let alias = caps.get(2).map(|m| m.as_str().trim()).unwrap_or("");
                let desc = caps.get(3).map(|m| m.as_str().trim()).unwrap_or("");
                if !ignored_words.contains(sub) && seen.insert(sub.to_string()) {
                    let label = if alias.is_empty() {
                        sub.to_string()
                    } else {
                        format!("{sub} ({alias})")
                    };
                    let mut entry = Entry::new(sub, label, EntryKind::Subcmd, desc);
                    if !alias.is_empty() {
                        entry.aliases.push(alias.to_string());
                    }
                    subcmds.push(entry);
                }
            } else if let Some((idx, item_indent)) = active_fallback_flag {
                if !trimmed.is_empty() {
                    if line_indent > item_indent {
                        append_desc_line(&mut flags[idx], trimmed);
                    } else {
                        active_fallback_flag = None;
                    }
                }
            }
        }
    }

    for entry in &mut flags {
        if entry.desc.is_empty() {
            entry.desc = format!("CLI Flag {}", entry.text);
        }
        populate_flag_metadata(entry);
    }

    subcmds.extend(flags);
    subcmds
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_cobra_gh_help() {
        let help = r#"
USAGE
  gh <command> <subcommand> [flags]

CORE COMMANDS
  auth:          Authenticate gh and git with GitHub
  issue:         Manage issues
  pr:            Manage pull requests
  repo:          Manage repositories

FLAGS
  -R, --repo [HOST/]OWNER/REPO   Select another repository using the [HOST/]OWNER/REPO format
      --help                     Show help for command
      --version                  Show gh version
"#;
        let items = parse_cli_help(help);
        let subcmds: Vec<&str> = items
            .iter()
            .filter(|i| i.kind == EntryKind::Subcmd)
            .map(|i| i.text.as_str())
            .collect();
        assert!(subcmds.contains(&"auth"));
        assert!(subcmds.contains(&"issue"));
        assert!(subcmds.contains(&"pr"));
        assert!(subcmds.contains(&"repo"));

        let repo_param = items.iter().find(|i| i.text == "--repo").expect("repo");
        assert_eq!(repo_param.kind, EntryKind::Param);
        assert_eq!(repo_param.aliases, vec!["-R"]);

        let flags: Vec<&str> = items
            .iter()
            .filter(|i| matches!(i.kind, EntryKind::Flag | EntryKind::Param))
            .map(|i| i.text.as_str())
            .collect();
        assert!(flags.contains(&"--repo"));
        assert!(flags.contains(&"--help"));
        assert!(flags.contains(&"--version"));
    }

    #[test]
    fn test_parse_clap_prek_help() {
        let help = r#"
Usage: prek.exe [OPTIONS] [COMMAND]

Commands:
  init               Create a prek configuration and install Git hook shims
  install            Install prek Git hook shims
  run                Run configured hooks

Hook selection:
  --stage <STAGE>          The stage during which the hook is fired [possible values: manual, commit-msg, pre-commit, pre-push]
  -a, --all-files            Run hooks on all tracked files in the repository
  --color <COLOR>        Whether to use color in output [default: auto] [possible values: auto, always, never]
"#;
        let items = parse_cli_help(help);
        let subcmds: Vec<&str> = items
            .iter()
            .filter(|i| i.kind == EntryKind::Subcmd)
            .map(|i| i.text.as_str())
            .collect();
        assert!(subcmds.contains(&"init"));
        assert!(subcmds.contains(&"install"));
        assert!(subcmds.contains(&"run"));

        let stage_flag = items
            .iter()
            .find(|i| i.text == "--stage")
            .expect("stage flag");
        assert_eq!(stage_flag.kind, EntryKind::Param);
        let enum_vals: Vec<&str> = stage_flag
            .children
            .iter()
            .map(|c| c.text.as_str())
            .collect();
        assert!(enum_vals.contains(&"manual"));
        assert!(enum_vals.contains(&"pre-commit"));

        let color_flag = items
            .iter()
            .find(|i| i.text == "--color")
            .expect("color flag");
        let color_enums: Vec<&str> = color_flag
            .children
            .iter()
            .map(|c| c.text.as_str())
            .collect();
        assert!(color_enums.contains(&"auto"));
        assert!(color_enums.contains(&"always"));
    }

    #[test]
    fn test_parse_winget_help() {
        let help = r#"
usage: winget [<command>] [<options>]

The following commands are available:
  install    Installs the given package
  show       Shows information about a package
  source     Manage sources of packages

The following options are available:
  -v,--version                Display the version of the tool
  --info                      Display general info of the tool
  -?,--help                   Shows help about the selected command
"#;
        let items = parse_cli_help(help);
        let subcmds: Vec<&str> = items
            .iter()
            .filter(|i| i.kind == EntryKind::Subcmd)
            .map(|i| i.text.as_str())
            .collect();
        assert!(subcmds.contains(&"install"));
        assert!(subcmds.contains(&"show"));
        assert!(subcmds.contains(&"source"));

        let flags: Vec<&str> = items
            .iter()
            .filter(|i| matches!(i.kind, EntryKind::Flag | EntryKind::Param))
            .map(|i| i.text.as_str())
            .collect();
        assert!(flags.contains(&"--version"));
        assert!(flags.contains(&"--info"));
        assert!(flags.contains(&"--help"));
    }

    #[test]
    fn test_parse_git_help() {
        let help = r#"
start a working area (see also: git help tutorial)
   clone      Clone a repository into a new directory
   init       Create an empty Git repository or reinitialize an existing one

work on the current change (see also: git help everyday)
   add        Add file contents to the index
   mv         Move or rename a file, a directory, or a symlink
"#;
        let items = parse_cli_help(help);
        let subcmds: Vec<&str> = items
            .iter()
            .filter(|i| i.kind == EntryKind::Subcmd)
            .map(|i| i.text.as_str())
            .collect();
        assert!(subcmds.contains(&"clone"));
        assert!(subcmds.contains(&"init"));
        assert!(subcmds.contains(&"add"));
        assert!(subcmds.contains(&"mv"));
    }

    #[test]
    fn test_fallback_parser_preserves_aliases_and_param_kinds() {
        let help = "\n  -o, --output=<FILE>    Write output to file\n  -d <DIR>               Target directory\n  -q, --quiet            Quiet mode\n";
        let items = parse_cli_help(help);
        let out = items.iter().find(|i| i.text == "--output").expect("output");
        assert_eq!(out.kind, EntryKind::Param);
        assert_eq!(out.aliases, vec!["-o"]);

        let dir = items.iter().find(|i| i.text == "-d").expect("dir");
        assert_eq!(dir.kind, EntryKind::Param);

        let quiet = items.iter().find(|i| i.text == "--quiet").expect("quiet");
        assert_eq!(quiet.kind, EntryKind::Flag);
        assert_eq!(quiet.aliases, vec!["-q"]);
    }

    #[test]
    fn test_parse_multiline_clap_v4_ansi_and_section_collision_guards() {
        let help = "\x1b[1mUsage:\x1b[0m demo [OPTIONS] [COMMAND]\n\n\
\x1b[1mCommands:\x1b[0m\n  \
  topics     List messaging topics\n  \
  examples   Print runnable examples\n  \
  watch      Watch files\n\n\
\x1b[1mOptions:\x1b[0m\n  \
  -s, --stage <STAGE>\n          \
          Stage to execute\n\n          \
          [possible values: pre-commit, pre-push]\n          \
          [default: pre-commit]\n      \
      --all-files\n          \
          Run across all files\n  \
  -u, --untracked-files[=<mode>]   Show untracked files\n  \
  -f <FILE>, --file <FILE>         Read config from file\n  \
  -q, --quiet  Quiet mode   (suppresses info)\n  \
      --pick   Select an option:\n";

        let items = parse_cli_help(help);
        let subcmds: Vec<&str> = items
            .iter()
            .filter(|i| i.kind == EntryKind::Subcmd)
            .map(|i| i.text.as_str())
            .collect();
        assert_eq!(subcmds, vec!["topics", "examples", "watch"]);

        let stage = items.iter().find(|i| i.text == "--stage").expect("--stage");
        assert_eq!(stage.kind, EntryKind::Param);
        assert_eq!(stage.aliases, vec!["-s"]);
        assert!(stage
            .children
            .iter()
            .any(|c| c.text == "pre-commit" && c.kind == EntryKind::Enum));
        assert!(stage
            .children
            .iter()
            .any(|c| c.text == "pre-push" && c.kind == EntryKind::Enum));

        let all_files = items
            .iter()
            .find(|i| i.text == "--all-files")
            .expect("--all-files");
        assert_eq!(all_files.kind, EntryKind::Flag);
        assert!(all_files.desc.contains("Run across all files"));

        let untracked = items
            .iter()
            .find(|i| i.text == "--untracked-files")
            .expect("--untracked-files");
        assert_eq!(untracked.kind, EntryKind::Param);
        assert_eq!(untracked.aliases, vec!["-u"]);

        let file_opt = items.iter().find(|i| i.text == "--file").expect("--file");
        assert_eq!(file_opt.kind, EntryKind::Param);
        assert_eq!(file_opt.aliases, vec!["-f"]);

        let quiet = items.iter().find(|i| i.text == "--quiet").expect("--quiet");
        assert_eq!(quiet.kind, EntryKind::Flag);
        assert_eq!(quiet.desc, "Quiet mode   (suppresses info)");

        assert!(items.iter().any(|i| i.text == "--pick"));
    }

    #[test]
    fn test_fallback_parser_extracts_enums_defaults_and_ignores_internal_desc_gaps() {
        let help = concat!(
            "  --format <FMT>  Output format   [default: text] [possible values: text, json]\n",
            "  --verbose  Verbose output   (prints debug traces)\n",
            "  build, b   Build workspace\n",
        );

        let items = parse_cli_help(help);
        let format_param = items
            .iter()
            .find(|i| i.text == "--format")
            .expect("--format");
        assert_eq!(format_param.kind, EntryKind::Param);
        assert!(format_param
            .children
            .iter()
            .any(|c| c.text == "text" && c.kind == EntryKind::Enum));
        assert!(format_param
            .children
            .iter()
            .any(|c| c.text == "json" && c.kind == EntryKind::Enum));

        let verbose = items
            .iter()
            .find(|i| i.text == "--verbose")
            .expect("--verbose");
        assert_eq!(verbose.kind, EntryKind::Flag);

        let build = items.iter().find(|i| i.text == "build").expect("build");
        assert_eq!(build.kind, EntryKind::Subcmd);
        assert_eq!(build.aliases, vec!["b"]);
    }
}
