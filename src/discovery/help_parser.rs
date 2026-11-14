use super::catalog::{contains_ignore_ascii_case, starts_with_ignore_ascii_case};
use crate::model::{Entry, EntryKind};
use regex::Regex;
use std::borrow::Cow;
use std::collections::HashSet;
use std::sync::OnceLock;

const MAX_HELP_BYTES: usize = 512 * 1024;
const MAX_HELP_LINES: usize = 4_096;
const MAX_HELP_LINE_BYTES: usize = 2_048;
const MAX_HELP_ENTRIES: usize = 1_024;
const MAX_CONTINUATION_LINES: usize = 16;
const MAX_DESC_BYTES: usize = 1_024;
const MAX_ENUM_VALUES: usize = 128;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HelpSection {
    None,
    Commands,
    Flags,
    Ignore,
}

fn truncate_utf8_prefix(s: &str, max_bytes: usize) -> &str {
    if s.len() <= max_bytes {
        return s;
    }
    let mut end = max_bytes;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

fn is_unsafe_display_control(c: char) -> bool {
    (c.is_control() && c != '\n' && c != '\t')
        || matches!(
            c,
            '\u{200b}'..='\u{200f}' | '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}'
        )
}

fn strip_ansi_escapes(text: &str) -> Cow<'_, str> {
    let bytes = text.as_bytes();
    let needs_clean = if text.is_ascii() {
        let mut dirty = false;
        for (idx, &b) in bytes.iter().enumerate() {
            if b == 0x1b || b == 0x7f || (b < 0x20 && b != b'\n' && b != b'\t' && b != b'\r') {
                dirty = true;
                break;
            }
            if b == b'\r' && bytes.get(idx + 1) != Some(&b'\n') {
                dirty = true;
                break;
            }
        }
        dirty
    } else {
        let mut dirty = false;
        for (idx, ch) in text.char_indices() {
            if ch == '\x1b'
                || (ch == '\r' && bytes.get(idx + 1) != Some(&b'\n'))
                || (ch != '\r' && is_unsafe_display_control(ch))
            {
                dirty = true;
                break;
            }
        }
        dirty
    };
    if !needs_clean {
        return Cow::Borrowed(text);
    }

    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch == '\x1b' {
            match chars.peek().copied() {
                Some('[') => {
                    chars.next();
                    let mut count = 0usize;
                    while let Some(&next) = chars.peek() {
                        if next == '\n' || next == '\r' || count >= 64 {
                            break;
                        }
                        chars.next();
                        count += 1;
                        if ('@'..='~').contains(&next) {
                            break;
                        }
                    }
                }
                Some(']') => {
                    chars.next();
                    let mut count = 0usize;
                    while let Some(&next) = chars.peek() {
                        if next == '\n' || next == '\r' || count >= 256 {
                            break;
                        }
                        let c = chars.next().unwrap();
                        count += 1;
                        if c == '\x07' {
                            break;
                        }
                        if c == '\x1b' && chars.peek() == Some(&'\\') {
                            chars.next();
                            break;
                        }
                    }
                }
                Some(next) if next != '\n' && next != '\r' => {
                    chars.next();
                }
                _ => {}
            }
        } else if ch == '\r' {
            if chars.peek() == Some(&'\n') {
                out.push('\r');
            }
        } else if !is_unsafe_display_control(ch) {
            out.push(ch);
        }
    }
    Cow::Owned(out)
}

fn leading_ws_count(line: &str) -> usize {
    if let Some(&first) = line.as_bytes().first() {
        if first < 0x80 && !first.is_ascii_whitespace() {
            return 0;
        }
    }
    line.chars().take_while(|c| c.is_whitespace()).count()
}

fn classify_section_header(line: &str) -> Option<HelpSection> {
    let trimmed = line.trim();
    if trimmed.is_empty() || trimmed.len() > 128 || trimmed.starts_with('-') {
        return None;
    }

    let leading_spaces = leading_ws_count(line);
    let has_column_gap = trimmed.contains("  ") || trimmed.contains('\t');

    if starts_with_ignore_ascii_case(trimmed, "usage:")
        || (starts_with_ignore_ascii_case(trimmed, "usage")
            && leading_spaces <= 1
            && !has_column_gap)
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
        && (starts_with_ignore_ascii_case(trimmed, "examples")
            || starts_with_ignore_ascii_case(trimmed, "learn more")
            || starts_with_ignore_ascii_case(trimmed, "see '")
            || starts_with_ignore_ascii_case(trimmed, "help topics")
            || starts_with_ignore_ascii_case(trimmed, "environment:")
            || starts_with_ignore_ascii_case(trimmed, "topics"))
    {
        return Some(HelpSection::Ignore);
    }

    if has_column_gap {
        return None;
    }

    if starts_with_ignore_ascii_case(trimmed, "start a working area")
        || starts_with_ignore_ascii_case(trimmed, "work on the current change")
        || starts_with_ignore_ascii_case(trimmed, "examine the history")
        || starts_with_ignore_ascii_case(trimmed, "grow, mark and tweak")
        || starts_with_ignore_ascii_case(trimmed, "collaborate")
        || starts_with_ignore_ascii_case(trimmed, "main porcelain commands")
        || starts_with_ignore_ascii_case(trimmed, "ancillary commands")
        || starts_with_ignore_ascii_case(trimmed, "the following commands are available")
    {
        return Some(HelpSection::Commands);
    }

    if starts_with_ignore_ascii_case(trimmed, "the following options are available") {
        return Some(HelpSection::Flags);
    }

    if ends_with_colon || is_all_caps {
        if contains_ignore_ascii_case(trimmed, "command")
            || contains_ignore_ascii_case(trimmed, "subcommand")
        {
            return Some(HelpSection::Commands);
        }
        if contains_ignore_ascii_case(trimmed, "flag")
            || contains_ignore_ascii_case(trimmed, "option")
            || contains_ignore_ascii_case(trimmed, "argument")
            || contains_ignore_ascii_case(trimmed, "selection")
            || contains_ignore_ascii_case(trimmed, "switch")
            || contains_ignore_ascii_case(trimmed, "parameter")
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
    let has_possible_hint =
        entry.desc.contains("[possible values:") || entry.desc.contains("Allowed values:");
    let has_default_hint = entry.desc.contains("[default:");
    if !has_possible_hint && !has_default_hint {
        return;
    }

    static RE_POSSIBLE_VALS: OnceLock<Regex> = OnceLock::new();
    static RE_DEFAULT_VAL: OnceLock<Regex> = OnceLock::new();

    if has_possible_hint {
        let re_vals = RE_POSSIBLE_VALS.get_or_init(|| {
            Regex::new(r"(?:\[possible values:\s*([^\]]+)\]|Allowed values:\s*([^.\n]+))").unwrap()
        });
        if let Some(v_caps) = re_vals.captures(&entry.desc) {
            let raw_vals = v_caps
                .get(1)
                .or_else(|| v_caps.get(2))
                .map(|m| m.as_str())
                .unwrap_or("");
            for v in raw_vals
                .split(',')
                .map(|s| s.trim())
                .filter(|s| !s.is_empty() && s.len() <= 128)
                .take(MAX_ENUM_VALUES)
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
    }

    if has_default_hint {
        let re_def = RE_DEFAULT_VAL.get_or_init(|| Regex::new(r"\[default:\s*([^\]]+)\]").unwrap());
        if let Some(d_caps) = re_def.captures(&entry.desc) {
            let def_val = d_caps.get(1).map(|m| m.as_str().trim()).unwrap_or("");
            if !def_val.is_empty()
                && def_val.len() <= 128
                && !entry.children.iter().any(|c| c.text == def_val)
            {
                entry.children.push(Entry::new(
                    def_val,
                    format!("{def_val} [default]"),
                    EntryKind::Value,
                    format!("Default value for {}", entry.text),
                ));
            }
        }
    }
}

fn append_desc_line(entry: &mut Entry, trimmed_line: &str) {
    if trimmed_line.is_empty() || entry.desc.len() >= MAX_DESC_BYTES {
        return;
    }
    if !entry.desc.is_empty() {
        if entry.desc.len() + 1 >= MAX_DESC_BYTES {
            return;
        }
        entry.desc.push(' ');
    }
    let remaining = MAX_DESC_BYTES - entry.desc.len();
    entry
        .desc
        .push_str(truncate_utf8_prefix(trimmed_line, remaining));
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

    let bounded_help = truncate_utf8_prefix(help_text, MAX_HELP_BYTES);
    let clean_help = strip_ansi_escapes(bounded_help);

    let est_entries = (clean_help.len() / 96).clamp(8, 256);
    let mut subcmds: Vec<Entry> = Vec::with_capacity(est_entries / 2);
    let mut flags: Vec<Entry> = Vec::with_capacity(est_entries);
    let mut seen = HashSet::with_capacity(est_entries);

    let mut current_sec = HelpSection::None;
    let mut detected_sections = false;
    let mut active_item: Option<(HelpSection, usize, usize, usize)> = None;
    let mut section_cmd_indent: Option<usize> = None;

    for raw_line in clean_help.lines().take(MAX_HELP_LINES) {
        let line = truncate_utf8_prefix(raw_line, MAX_HELP_LINE_BYTES);
        if let Some(sec) = classify_section_header(line) {
            current_sec = sec;
            detected_sections = true;
            active_item = None;
            section_cmd_indent = None;
            continue;
        }

        let line_indent = leading_ws_count(line);
        let trimmed = line.trim();
        let at_entry_cap = subcmds.len() + flags.len() >= MAX_HELP_ENTRIES;

        match current_sec {
            HelpSection::Commands => {
                let within_cmd_col =
                    !at_entry_cap && section_cmd_indent.is_none_or(|ci| line_indent <= ci + 2);
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
                            let mut entry = Entry::new(
                                cmd,
                                label,
                                EntryKind::Subcmd,
                                truncate_utf8_prefix(desc, MAX_DESC_BYTES),
                            );
                            if !alias.is_empty() {
                                entry.aliases.push(alias.to_string());
                            }
                            subcmds.push(entry);
                            active_item =
                                Some((HelpSection::Commands, subcmds.len() - 1, line_indent, 0));
                            continue;
                        }
                    } else if active_item
                        .as_ref()
                        .is_none_or(|(_, _, ai, _)| line_indent <= *ai)
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
                                active_item = Some((
                                    HelpSection::Commands,
                                    subcmds.len() - 1,
                                    line_indent,
                                    0,
                                ));
                                continue;
                            }
                        }
                    }
                }

                if let Some((HelpSection::Commands, idx, item_indent, ref mut cont)) = active_item {
                    if !trimmed.is_empty() {
                        if line_indent > item_indent && *cont < MAX_CONTINUATION_LINES {
                            append_desc_line(&mut subcmds[idx], trimmed);
                            *cont += 1;
                        } else {
                            active_item = None;
                        }
                    }
                }
            }
            HelpSection::Flags => {
                if !at_entry_cap {
                    if let Some(caps) = re_sec_flag.captures(line) {
                        let short = caps.get(1).map(|m| m.as_str().trim()).unwrap_or("");
                        let raw_flag = caps.get(2).map(|m| m.as_str().trim()).unwrap_or("");
                        let arg_hint = caps
                            .get(3)
                            .or_else(|| caps.get(4))
                            .map(|m| m.as_str().trim())
                            .unwrap_or("");
                        let desc = caps.get(5).map(|m| m.as_str().trim()).unwrap_or("");
                        let entry = build_flag_entry(
                            short,
                            raw_flag,
                            arg_hint,
                            truncate_utf8_prefix(desc, MAX_DESC_BYTES),
                        );
                        if seen.insert(entry.text.clone()) {
                            flags.push(entry);
                            active_item =
                                Some((HelpSection::Flags, flags.len() - 1, line_indent, 0));
                        } else {
                            active_item = None;
                        }
                        continue;
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
                            active_item =
                                Some((HelpSection::Flags, flags.len() - 1, line_indent, 0));
                        } else {
                            active_item = None;
                        }
                        continue;
                    }
                }
                if let Some((HelpSection::Flags, idx, item_indent, ref mut cont)) = active_item {
                    if !trimmed.is_empty() {
                        if line_indent > item_indent && *cont < MAX_CONTINUATION_LINES {
                            append_desc_line(&mut flags[idx], trimmed);
                            *cont += 1;
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

        let mut active_fallback_flag: Option<(usize, usize, usize)> = None;

        for raw_line in clean_help.lines().take(MAX_HELP_LINES) {
            let line = truncate_utf8_prefix(raw_line, MAX_HELP_LINE_BYTES);
            let line_indent = leading_ws_count(line);
            let trimmed = line.trim();
            let at_entry_cap = subcmds.len() + flags.len() >= MAX_HELP_ENTRIES;

            if !at_entry_cap {
                if let Some(caps) = re_long.captures(line) {
                    let short = caps.get(1).map(|m| m.as_str().trim()).unwrap_or("");
                    let raw_long = caps.get(2).map(|m| m.as_str().trim()).unwrap_or("");
                    let arg_hint = caps
                        .get(3)
                        .or_else(|| caps.get(4))
                        .map(|m| m.as_str().trim())
                        .unwrap_or("");
                    let desc = caps.get(5).map(|m| m.as_str().trim()).unwrap_or("");
                    let entry = build_flag_entry(
                        short,
                        raw_long,
                        arg_hint,
                        truncate_utf8_prefix(desc, MAX_DESC_BYTES),
                    );
                    if seen.insert(entry.text.clone()) {
                        flags.push(entry);
                        active_fallback_flag = Some((flags.len() - 1, line_indent, 0));
                    } else {
                        active_fallback_flag = None;
                    }
                    continue;
                } else if let Some(caps) = re_short.captures(line) {
                    let short = caps.get(1).map(|m| m.as_str().trim()).unwrap_or("");
                    let arg_hint = caps.get(2).map(|m| m.as_str().trim()).unwrap_or("");
                    let desc = caps.get(3).map(|m| m.as_str().trim()).unwrap_or("");
                    let entry = build_flag_entry(
                        "",
                        short,
                        arg_hint,
                        truncate_utf8_prefix(desc, MAX_DESC_BYTES),
                    );
                    if seen.insert(entry.text.clone()) {
                        flags.push(entry);
                        active_fallback_flag = Some((flags.len() - 1, line_indent, 0));
                    } else {
                        active_fallback_flag = None;
                    }
                    continue;
                } else if let Some(caps) = re_sub.captures(line) {
                    active_fallback_flag = None;
                    let sub = caps.get(1).map(|m| m.as_str().trim()).unwrap_or("");
                    let alias = caps.get(2).map(|m| m.as_str().trim()).unwrap_or("");
                    let desc = caps.get(3).map(|m| m.as_str().trim()).unwrap_or("");
                    let is_ignored = matches!(
                        sub,
                        "usage"
                            | "options"
                            | "commands"
                            | "arguments"
                            | "flags"
                            | "examples"
                            | "env"
                            | "note"
                            | "default"
                            | "or"
                            | "see"
                            | "start"
                            | "work"
                            | "examine"
                            | "grow"
                            | "collaborate"
                    );
                    if !is_ignored && seen.insert(sub.to_string()) {
                        let label = if alias.is_empty() {
                            sub.to_string()
                        } else {
                            format!("{sub} ({alias})")
                        };
                        let mut entry = Entry::new(
                            sub,
                            label,
                            EntryKind::Subcmd,
                            truncate_utf8_prefix(desc, MAX_DESC_BYTES),
                        );
                        if !alias.is_empty() {
                            entry.aliases.push(alias.to_string());
                        }
                        subcmds.push(entry);
                    }
                    continue;
                }
            }
            if let Some((idx, item_indent, ref mut cont)) = active_fallback_flag {
                if !trimmed.is_empty() {
                    if line_indent > item_indent && *cont < MAX_CONTINUATION_LINES {
                        append_desc_line(&mut flags[idx], trimmed);
                        *cont += 1;
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

    #[test]
    fn test_parse_cli_help_hardens_unterminated_escapes_bidi_and_entry_bounds() {
        let mut adversarial = String::from(
            "\x1b]0;unterminated-osc\n\
             Commands:\n\
               \x1b[31m  build   Build \rspoof\x07\u{009b}31m\u{202e}project\n\
             Options:\n  -f, --format <FMT>   Format [possible values: ",
        );
        for i in 0..160 {
            if i > 0 {
                adversarial.push(',');
            }
            adversarial.push_str(&format!("v{i}"));
        }
        adversarial.push_str("]\n");
        for i in 0..40 {
            adversarial.push_str(&format!(
                "                         extra continuation detail number {i:03} with padding\n"
            ));
        }
        for i in 0..1_200 {
            adversarial.push_str(&format!("  --opt-{i:04}   Option {i}\n"));
        }

        let entries = parse_cli_help(&adversarial);
        assert!(entries.len() <= MAX_HELP_ENTRIES);

        let build = entries
            .iter()
            .find(|e| e.text == "build")
            .expect("valid subcommand after unterminated OSC/CSI must survive");
        assert!(!build.desc.contains('\r'));
        assert!(!build.desc.contains('\x07'));
        assert!(!build.desc.contains('\u{009b}'));
        assert!(!build.desc.contains('\u{202e}'));
        assert!(build.desc.contains("Build spoof"));

        let fmt = entries
            .iter()
            .find(|e| e.text == "--format")
            .expect("--format");
        assert_eq!(fmt.kind, EntryKind::Param);
        assert_eq!(fmt.children.len(), MAX_ENUM_VALUES);
        assert!(fmt.desc.len() <= MAX_DESC_BYTES);
    }

    #[test]
    fn test_classify_section_header_zero_alloc_paths_and_length_cap() {
        assert_eq!(classify_section_header("USAGE:"), Some(HelpSection::Ignore));
        assert_eq!(
            classify_section_header("SUBCOMMANDS:"),
            Some(HelpSection::Commands)
        );
        assert_eq!(
            classify_section_header("GLOBAL OPTIONS:"),
            Some(HelpSection::Flags)
        );
        assert_eq!(classify_section_header("  -h, --help    Help"), None);
        assert_eq!(classify_section_header(&"Options: ".repeat(30)), None);
    }
}
