use crate::model::{Entry, EntryKind};
use regex::Regex;
use std::collections::HashSet;
use std::sync::OnceLock;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HelpSection {
    None,
    Commands,
    Flags,
    Ignore,
}

fn classify_section_header(line: &str) -> Option<HelpSection> {
    let trimmed = line.trim();
    if trimmed.is_empty() {
        return None;
    }

    let lower = trimmed.to_ascii_lowercase();

    if lower.starts_with("usage")
        || lower.starts_with("examples")
        || lower.starts_with("learn more")
        || lower.starts_with("see '")
        || lower.starts_with("help topics")
        || lower.starts_with("environment:")
        || lower.starts_with("topics")
    {
        return Some(HelpSection::Ignore);
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

    let is_all_caps = trimmed.len() >= 3
        && trimmed
            .chars()
            .all(|c| !c.is_alphabetic() || c.is_uppercase());
    let ends_with_colon = trimmed.ends_with(':');

    if ends_with_colon || is_all_caps {
        if lower.contains("command") || lower.contains("subcommand") {
            return Some(HelpSection::Commands);
        }
        if lower.contains("flag")
            || lower.contains("option")
            || lower.contains("argument")
            || lower.contains("selection")
        {
            return Some(HelpSection::Flags);
        }
    }

    None
}

/// Parses `--help` / `-h` output from Cobra, Clap, winget, git, and generic CLIs
/// into Pane 2/3 [`Entry`] values (subcommands, Parameters, Flags, enums, and defaults).
pub(super) fn parse_cli_help(help_text: &str) -> Vec<Entry> {
    static RE_SECTION_SUBCMD: OnceLock<Regex> = OnceLock::new();
    static RE_SECTION_FLAG: OnceLock<Regex> = OnceLock::new();
    static RE_POSSIBLE_VALS: OnceLock<Regex> = OnceLock::new();
    static RE_DEFAULT_VAL: OnceLock<Regex> = OnceLock::new();
    static RE_LONG_FLAG_FALLBACK: OnceLock<Regex> = OnceLock::new();
    static RE_SHORT_FLAG_FALLBACK: OnceLock<Regex> = OnceLock::new();
    static RE_SUBCMD_FALLBACK: OnceLock<Regex> = OnceLock::new();

    let re_sec_sub = RE_SECTION_SUBCMD.get_or_init(|| {
        Regex::new(r"^\s{2,8}([a-zA-Z0-9_-]+)(?::|,\s*([a-zA-Z0-9_-]+))?(?:\s{2,}|\t|\s*:\s+)(.+)$")
            .unwrap()
    });

    let re_sec_flag = RE_SECTION_FLAG.get_or_init(|| {
        Regex::new(r"^\s{1,10}(?:(-[a-zA-Z0-9\?]),?\s*(?:\||,)?\s*)?(--(?:\[no-\])?[a-zA-Z0-9_\.-]+|-[a-zA-Z0-9\?])(?:(?:\s+|=)([^\s].*?))?(?:\s{2,}|\t)(.+)$")
            .unwrap()
    });

    let re_vals = RE_POSSIBLE_VALS.get_or_init(|| {
        Regex::new(r"(?:\[possible values:\s*([^\]]+)\]|Allowed values:\s*([^.\n]+))").unwrap()
    });

    let re_def = RE_DEFAULT_VAL.get_or_init(|| Regex::new(r"\[default:\s*([^\]]+)\]").unwrap());

    let mut subcmds = Vec::new();
    let mut flags = Vec::new();
    let mut seen = HashSet::new();

    let mut current_sec = HelpSection::None;
    let mut detected_sections = false;

    for line in help_text.lines() {
        if let Some(sec) = classify_section_header(line) {
            current_sec = sec;
            detected_sections = true;
            continue;
        }

        match current_sec {
            HelpSection::Commands => {
                if let Some(caps) = re_sec_sub.captures(line) {
                    let cmd = caps.get(1).map(|m| m.as_str().trim()).unwrap_or("");
                    let alias = caps.get(2).map(|m| m.as_str().trim()).unwrap_or("");
                    let desc = caps.get(3).map(|m| m.as_str().trim()).unwrap_or("");

                    if !cmd.is_empty() && seen.insert(cmd.to_string()) {
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
                    }
                }
            }
            HelpSection::Flags => {
                if let Some(caps) = re_sec_flag.captures(line) {
                    let short = caps.get(1).map(|m| m.as_str().trim()).unwrap_or("");
                    let raw_flag = caps.get(2).map(|m| m.as_str().trim()).unwrap_or("");
                    let flag = raw_flag.replacen("--[no-]", "--", 1);
                    let arg_hint = caps.get(3).map(|m| m.as_str().trim()).unwrap_or("");
                    let desc = caps.get(4).map(|m| m.as_str().trim()).unwrap_or("");

                    if seen.insert(flag.clone()) {
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

                        if let Some(v_caps) = re_vals.captures(desc) {
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
                                entry.children.push(Entry::new(
                                    v,
                                    v,
                                    EntryKind::Enum,
                                    format!("Allowed value for {flag}"),
                                ));
                            }
                        }

                        if let Some(d_caps) = re_def.captures(desc) {
                            let def_val = d_caps.get(1).map(|m| m.as_str().trim()).unwrap_or("");
                            if !def_val.is_empty()
                                && !entry.children.iter().any(|c| c.text == def_val)
                            {
                                entry.children.push(Entry::new(
                                    def_val,
                                    format!("{def_val} [default]"),
                                    EntryKind::Value,
                                    format!("Default value for {flag}"),
                                ));
                            }
                        }

                        flags.push(entry);
                    }
                }
            }
            HelpSection::Ignore | HelpSection::None => {}
        }
    }

    if !detected_sections || (subcmds.is_empty() && flags.is_empty()) {
        let re_long = RE_LONG_FLAG_FALLBACK.get_or_init(|| {
            Regex::new(r"^\s{1,10}(?:(-[a-zA-Z0-9\?]),?\s+(?:\|\s+)?)?(--(?:\[no-\])?[a-zA-Z0-9_-]+)([=\s\[][^\s\]]*\]?)?(?:\s{2,}(.+))?$").unwrap()
        });
        let re_short = RE_SHORT_FLAG_FALLBACK.get_or_init(|| {
            Regex::new(r"^\s{1,10}(-[a-zA-Z0-9\?])(?:\s+(<[^>]+>|\[[^\]]+\]))?\s{2,}(.+)$").unwrap()
        });
        let re_sub = RE_SUBCMD_FALLBACK.get_or_init(|| {
            Regex::new(r"^\s{2,8}([a-z][a-z0-9_-]{1,24})(?:,\s*[a-z0-9_-]+)?\s{2,}([A-Za-z].+)$")
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

        for line in help_text.lines() {
            if let Some(caps) = re_long.captures(line) {
                let short = caps.get(1).map(|m| m.as_str().trim()).unwrap_or("");
                let raw_long = caps.get(2).map(|m| m.as_str().trim()).unwrap_or("");
                let long = raw_long.replacen("--[no-]", "--", 1);
                let arg_hint = caps.get(3).map(|m| m.as_str().trim()).unwrap_or("");
                let desc = caps
                    .get(4)
                    .map(|m| m.as_str().trim().to_string())
                    .unwrap_or_else(|| format!("CLI Flag {long}"));

                if seen.insert(long.clone()) {
                    let kind = if arg_hint.is_empty() {
                        EntryKind::Flag
                    } else {
                        EntryKind::Param
                    };
                    let label = if short.is_empty() {
                        long.clone()
                    } else {
                        format!("{long} ({short})")
                    };
                    let mut entry = Entry::new(long, label, kind, desc);
                    if !short.is_empty() {
                        entry.aliases.push(short.to_string());
                    }
                    flags.push(entry);
                }
            } else if let Some(caps) = re_short.captures(line) {
                let short = caps.get(1).map(|m| m.as_str().trim()).unwrap_or("");
                let arg_hint = caps.get(2).map(|m| m.as_str().trim()).unwrap_or("");
                let desc = caps.get(3).map(|m| m.as_str().trim()).unwrap_or("");
                if seen.insert(short.to_string()) {
                    let kind = if arg_hint.is_empty() {
                        EntryKind::Flag
                    } else {
                        EntryKind::Param
                    };
                    flags.push(Entry::new(short, short, kind, desc));
                }
            } else if let Some(caps) = re_sub.captures(line) {
                let sub = caps.get(1).map(|m| m.as_str().trim()).unwrap_or("");
                let desc = caps.get(2).map(|m| m.as_str().trim()).unwrap_or("");
                if !ignored_words.contains(sub) && seen.insert(sub.to_string()) {
                    subcmds.push(Entry::new(sub, sub, EntryKind::Subcmd, desc));
                }
            }
        }
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
}
