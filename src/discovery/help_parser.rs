use super::catalog::{ends_with_ignore_ascii_case, starts_with_ignore_ascii_case};
use crate::model::{Entry, EntryKind};
use crate::sanitize::{contains_ignore_ascii_case, strip_help_text_escapes};
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
    Positional,
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

fn leading_ws_count(line: &str) -> usize {
    let bytes = line.as_bytes();
    let mut idx = 0usize;
    while idx < bytes.len() {
        let b = bytes[idx];
        if b == b' ' || b == b'\t' {
            idx += 1;
        } else if b < 0x80 && b != b'\x0b' && b != b'\x0c' && b != b'\r' && b != b'\n' {
            return idx;
        } else {
            return idx
                + line[idx..]
                    .chars()
                    .take_while(|c| c.is_whitespace())
                    .count();
        }
    }
    idx
}

fn extract_box_header_title(trimmed: &str) -> Option<&str> {
    let inner = trimmed
        .strip_prefix('╭')
        .and_then(|s| s.strip_suffix('╮'))
        .or_else(|| trimmed.strip_prefix('┌').and_then(|s| s.strip_suffix('┐')))
        .or_else(|| trimmed.strip_prefix('+').and_then(|s| s.strip_suffix('+')))?;
    let title = inner.trim_matches(|c: char| c == '─' || c == '-' || c.is_whitespace());
    (!title.is_empty() && title.len() <= 64).then_some(title)
}

fn is_box_bottom_border(trimmed: &str) -> bool {
    (trimmed.starts_with('╰') && trimmed.ends_with('╯'))
        || (trimmed.starts_with('└') && trimmed.ends_with('┘'))
        || (trimmed.starts_with('+')
            && trimmed.ends_with('+')
            && trimmed.len() >= 4
            && trimmed[1..trimmed.len() - 1].bytes().all(|b| b == b'-'))
}

fn unwrap_box_row(line: &str) -> Option<String> {
    let trimmed = line.trim();
    let inner = trimmed.strip_prefix('│')?.strip_suffix('│')?;
    Some(format!(" {}", inner.trim_end()))
}

fn classify_section_title(title: &str) -> Option<HelpSection> {
    if starts_with_ignore_ascii_case(title, "positional argument") {
        return Some(HelpSection::Positional);
    }
    if contains_ignore_ascii_case(title, "command")
        || contains_ignore_ascii_case(title, "subcommand")
        || contains_ignore_ascii_case(title, "groups")
    {
        return Some(HelpSection::Commands);
    }
    if contains_ignore_ascii_case(title, "flag")
        || contains_ignore_ascii_case(title, "option")
        || contains_ignore_ascii_case(title, "argument")
        || contains_ignore_ascii_case(title, "selection")
        || contains_ignore_ascii_case(title, "switch")
        || contains_ignore_ascii_case(title, "parameter")
    {
        return Some(HelpSection::Flags);
    }
    None
}

fn classify_section_header(line: &str) -> Option<HelpSection> {
    let trimmed = line.trim();
    if trimmed.is_empty() || trimmed.len() > 256 || trimmed.starts_with('-') {
        return None;
    }

    if let Some(box_title) = extract_box_header_title(trimmed) {
        if starts_with_ignore_ascii_case(box_title, "usage")
            || starts_with_ignore_ascii_case(box_title, "examples")
        {
            return Some(HelpSection::Ignore);
        }
        return classify_section_title(box_title);
    }

    if trimmed.len() > 128 {
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

    if has_column_gap {
        return None;
    }

    let ends_with_colon = trimmed.ends_with(':');
    let is_all_caps = || {
        trimmed.len() >= 3
            && if trimmed.is_ascii() {
                trimmed
                    .bytes()
                    .all(|b| !b.is_ascii_alphabetic() || b.is_ascii_uppercase())
            } else {
                trimmed
                    .chars()
                    .all(|c| !c.is_alphabetic() || c.is_uppercase())
            }
    };
    let header_eligible = leading_spaces <= 1 || ends_with_colon || is_all_caps();

    if header_eligible
        && (starts_with_ignore_ascii_case(trimmed, "examples")
            || starts_with_ignore_ascii_case(trimmed, "learn more")
            || starts_with_ignore_ascii_case(trimmed, "see '")
            || starts_with_ignore_ascii_case(trimmed, "help topics")
            || starts_with_ignore_ascii_case(trimmed, "environment:")
            || starts_with_ignore_ascii_case(trimmed, "topics"))
    {
        return Some(HelpSection::Ignore);
    }

    match trimmed.as_bytes().first().map(u8::to_ascii_lowercase) {
        Some(b's') if starts_with_ignore_ascii_case(trimmed, "start a working area") => {
            return Some(HelpSection::Commands);
        }
        Some(b'w') if starts_with_ignore_ascii_case(trimmed, "work on the current change") => {
            return Some(HelpSection::Commands);
        }
        Some(b'e') if starts_with_ignore_ascii_case(trimmed, "examine the history") => {
            return Some(HelpSection::Commands);
        }
        Some(b'g') if starts_with_ignore_ascii_case(trimmed, "grow, mark and tweak") => {
            return Some(HelpSection::Commands);
        }
        Some(b'c') if starts_with_ignore_ascii_case(trimmed, "collaborate") => {
            return Some(HelpSection::Commands);
        }
        Some(b'm') if starts_with_ignore_ascii_case(trimmed, "main porcelain commands") => {
            return Some(HelpSection::Commands);
        }
        Some(b'a') if starts_with_ignore_ascii_case(trimmed, "ancillary commands") => {
            return Some(HelpSection::Commands);
        }
        Some(b't') => {
            if starts_with_ignore_ascii_case(trimmed, "the following commands are available") {
                return Some(HelpSection::Commands);
            }
            if starts_with_ignore_ascii_case(trimmed, "the following options are available") {
                return Some(HelpSection::Flags);
            }
        }
        _ => {}
    }

    if ends_with_colon || is_all_caps() {
        if let Some(sec) = classify_section_title(trimmed) {
            return Some(sec);
        }
    }

    // Azure CLI (`az` / `knack`) uses column-0 section headings without trailing colons,
    // such as `Arguments`, `Global Arguments`, or `Subgroups`.
    if leading_spaces == 0 && trimmed.len() <= 48 && trimmed.split_whitespace().count() <= 4 {
        if ends_with_ignore_ascii_case(trimmed, "arguments")
            || ends_with_ignore_ascii_case(trimmed, "options")
            || ends_with_ignore_ascii_case(trimmed, "flags")
        {
            return Some(HelpSection::Flags);
        }
        if ends_with_ignore_ascii_case(trimmed, "commands")
            || ends_with_ignore_ascii_case(trimmed, "subgroups")
            || ends_with_ignore_ascii_case(trimmed, "groups")
        {
            return Some(HelpSection::Commands);
        }
    }

    None
}

fn cached_regex<'a>(slot: &'a OnceLock<Option<Regex>>, pattern: &str) -> Option<&'a Regex> {
    slot.get_or_init(|| Regex::new(pattern).ok()).as_ref()
}

#[inline]
fn has_two_adjacent_ws(s: &str) -> bool {
    if !s.is_ascii() {
        return true;
    }
    s.as_bytes()
        .windows(2)
        .any(|w| w[0].is_ascii_whitespace() && w[1].is_ascii_whitespace())
}

fn normalize_flag_token(raw_flag: &str) -> Cow<'_, str> {
    if raw_flag.contains("--[no-]") {
        Cow::Owned(raw_flag.replacen("--[no-]", "--", 1))
    } else {
        Cow::Borrowed(raw_flag)
    }
}

fn is_short_flag_token(tok: &str) -> bool {
    let bytes = tok.as_bytes();
    bytes.len() == 2 && bytes[0] == b'-' && (bytes[1].is_ascii_alphanumeric() || bytes[1] == b'?')
}

fn is_separated_metavar_token(tok: &str) -> bool {
    if tok.is_empty() || tok.len() > 64 {
        return false;
    }
    if (tok.starts_with('<') && tok.ends_with('>') && tok.len() >= 3)
        || (tok.starts_with('{') && tok.ends_with('}') && tok.contains(','))
        || (tok.starts_with('[') && tok.ends_with(']') && tok.contains('|'))
    {
        return true;
    }
    tok.len() >= 2
        && tok.len() <= 24
        && tok != "DEPRECATED"
        && tok.as_bytes()[0].is_ascii_uppercase()
        && tok
            .bytes()
            .all(|b| b.is_ascii_uppercase() || b.is_ascii_digit() || b == b'_' || b == b'-')
}

fn split_at_column_gap(s: &str) -> Option<(&str, &str)> {
    let bytes = s.as_bytes();
    for i in 0..bytes.len().saturating_sub(1) {
        if bytes[i] == b'\t' || (bytes[i] == b' ' && bytes[i + 1] == b' ') {
            let head = s[..i].trim_end();
            let tail = s[i..].trim_start();
            if !head.is_empty() && !tail.is_empty() {
                return Some((head, tail));
            }
        }
    }
    None
}

fn refine_flag_parts<'a>(
    short: &'a str,
    raw_flag: &str,
    mut arg_hint: &'a str,
    mut desc: &'a str,
) -> (Vec<&'a str>, &'a str, &'a str, bool) {
    let mut aliases: Vec<&'a str> = Vec::new();
    let mut force_param = false;

    if !short.is_empty() && short != raw_flag {
        aliases.push(short);
    }

    // 1. Click/Typer boolean toggle pair: `--verbose / --no-verbose`
    if let Some(after_slash) = arg_hint.strip_prefix('/') {
        let toggle = after_slash.trim();
        if toggle.starts_with('-') {
            let toggle_tok = toggle.split_whitespace().next().unwrap_or("");
            if !toggle_tok.is_empty() && toggle_tok != raw_flag && !aliases.contains(&toggle_tok) {
                aliases.push(toggle_tok);
            }
            arg_hint = "";
        }
    }

    // 2. Long-first short alias (Azure CLI `az`/knack: `--output -o`, `--resource-group -g`)
    if aliases.is_empty() && raw_flag.starts_with("--") && !arg_hint.is_empty() {
        let mut parts = arg_hint.split_whitespace();
        if let Some(first) = parts.next() {
            if is_short_flag_token(first) {
                aliases.push(first);
                let rest = arg_hint[first.len()..].trim();
                if rest.eq_ignore_ascii_case("[required]") {
                    force_param = true;
                    arg_hint = "";
                } else {
                    arg_hint = rest;
                }
            }
        }
    }

    // 3. Typer Rich separated short flag / metavar column inside `desc`:
    //    `--verbose   -v   Enable verbose` or `--config   -c   TEXT   Config path`
    if arg_hint.is_empty() && raw_flag.starts_with("--") {
        if aliases.is_empty() {
            if let Some((head, tail)) = split_at_column_gap(desc) {
                if is_short_flag_token(head) {
                    aliases.push(head);
                    desc = tail;
                }
            } else if is_short_flag_token(desc) {
                aliases.push(desc);
                desc = "";
            }
        }
        if let Some((head, tail)) = split_at_column_gap(desc) {
            if is_separated_metavar_token(head) {
                arg_hint = head;
                desc = tail;
            }
        }
    }

    // 4. Azure CLI `[Required] : <desc>` and `: <desc>` separators
    if let Some(rest) = desc
        .strip_prefix("[Required]")
        .or_else(|| desc.strip_prefix("[required]"))
    {
        let after = rest.trim_start();
        if let Some(after_colon) = after.strip_prefix(':') {
            force_param = true;
            desc = after_colon.trim_start();
        }
    } else if let Some(after_colon) = desc.strip_prefix(':') {
        if after_colon.is_empty() || after_colon.starts_with(char::is_whitespace) {
            desc = after_colon.trim_start();
        }
    }

    (aliases, arg_hint, desc, force_param)
}

fn populate_arg_hint_enums(entry: &mut Entry, arg_hint: &str) {
    let trimmed = arg_hint.trim();
    let (inner, sep) = if let Some(s) = trimmed.strip_prefix('{').and_then(|s| s.strip_suffix('}'))
    {
        if !s.contains(',') {
            return;
        }
        (s, ',')
    } else if let Some(s) = trimmed.strip_prefix('[').and_then(|s| s.strip_suffix(']')) {
        if !s.contains('|') {
            return;
        }
        (s, '|')
    } else {
        return;
    };

    for raw in inner
        .split(sep)
        .map(str::trim)
        .filter(|v| !v.is_empty() && v.len() <= 128 && !v.chars().any(char::is_whitespace))
        .take(MAX_ENUM_VALUES)
    {
        if !entry.children.iter().any(|c| c.text == raw) {
            entry.children.push(Entry::new(
                raw,
                raw,
                EntryKind::Enum,
                format!("Allowed value for {}", entry.text),
            ));
        }
    }
    if entry.children.iter().any(|c| c.kind == EntryKind::Enum) {
        entry.kind = EntryKind::Param;
    }
}

fn build_flag_entry(
    aliases: &[&str],
    flag: Cow<'_, str>,
    arg_hint: &str,
    desc: &str,
    force_param: bool,
) -> Entry {
    let kind = if !arg_hint.is_empty() || force_param {
        EntryKind::Param
    } else {
        EntryKind::Flag
    };

    let primary_alias = aliases.first().copied().unwrap_or("");
    let has_distinct_alias = !primary_alias.is_empty() && primary_alias != flag.as_ref();
    let label = if has_distinct_alias {
        if !arg_hint.is_empty() {
            format!("{flag} {arg_hint} ({primary_alias})")
        } else {
            format!("{flag} ({primary_alias})")
        }
    } else if !arg_hint.is_empty() {
        format!("{flag} {arg_hint}")
    } else {
        flag.to_string()
    };

    let mut entry = Entry::new(flag.into_owned(), label, kind, desc);
    for &alias in aliases {
        if !alias.is_empty() && alias != entry.text && !entry.aliases.iter().any(|a| a == alias) {
            entry.aliases.push(alias.to_string());
        }
    }
    populate_arg_hint_enums(&mut entry, arg_hint);
    entry
}

fn try_push_flag_entry(
    flags: &mut Vec<Entry>,
    seen: &mut HashSet<String>,
    short: &str,
    raw_flag: &str,
    arg_hint: &str,
    desc: &str,
) -> Option<usize> {
    let flag = normalize_flag_token(raw_flag);
    if seen.contains(flag.as_ref()) {
        return None;
    }
    let (aliases, clean_hint, clean_desc, force_param) =
        refine_flag_parts(short, flag.as_ref(), arg_hint, desc);
    let entry = build_flag_entry(
        &aliases,
        flag,
        clean_hint,
        truncate_utf8_prefix(clean_desc, MAX_DESC_BYTES),
        force_param,
    );
    seen.insert(entry.text.clone());
    flags.push(entry);
    Some(flags.len() - 1)
}

fn try_push_subcmd_entry(
    subcmds: &mut Vec<Entry>,
    seen: &mut HashSet<String>,
    cmd: &str,
    alias: &str,
    desc: &str,
) -> Option<usize> {
    if cmd.is_empty() || seen.contains(cmd) {
        return None;
    }
    seen.insert(cmd.to_string());
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
    Some(subcmds.len() - 1)
}

fn extract_possible_values_slice(desc: &str) -> Option<&str> {
    const POSSIBLE_PREFIX: &str = "[possible values:";
    const ALLOWED_PREFIX: &str = "Allowed values:";

    let mut offset = 0usize;
    let mut check_possible = true;
    let mut check_allowed = true;

    while offset < desc.len() && (check_possible || check_allowed) {
        let tail = &desc[offset..];
        let possible_idx = if check_possible {
            let found = tail.find(POSSIBLE_PREFIX);
            if found.is_none() {
                check_possible = false;
            }
            found
        } else {
            None
        };
        let allowed_idx = if check_allowed {
            let found = tail.find(ALLOWED_PREFIX);
            if found.is_none() {
                check_allowed = false;
            }
            found
        } else {
            None
        };

        match (possible_idx, allowed_idx) {
            (Some(p), Some(a)) if p <= a => {
                let after = &tail[p + POSSIBLE_PREFIX.len()..];
                let Some(close) = after.find(']') else {
                    check_possible = false;
                    continue;
                };
                if close > 0 {
                    return Some(after[..close].trim());
                }
                offset += p + POSSIBLE_PREFIX.len() + 1;
            }
            (_, Some(a)) => {
                let after_start = a + ALLOWED_PREFIX.len();
                let after = &tail[after_start..];
                let trimmed = after.trim_start();
                let end = trimmed.find(['.', '\n']).unwrap_or(trimmed.len());
                if end > 0 {
                    return Some(trimmed[..end].trim());
                }
                let raw_end = after.find(['.', '\n']).unwrap_or(after.len());
                if raw_end > 0 {
                    return Some("");
                }
                offset += after_start;
            }
            (Some(p), None) => {
                let after = &tail[p + POSSIBLE_PREFIX.len()..];
                let close = after.find(']')?;
                if close > 0 {
                    return Some(after[..close].trim());
                }
                offset += p + POSSIBLE_PREFIX.len() + 1;
            }
            (None, None) => break,
        }
    }
    None
}

fn extract_default_value_slice(desc: &str) -> Option<&str> {
    const DEFAULT_PREFIX: &str = "[default:";

    let mut rest = desc;
    while let Some(pos) = rest.find(DEFAULT_PREFIX) {
        let after = &rest[pos + DEFAULT_PREFIX.len()..];
        let close = after.find(']')?;
        if close > 0 {
            return Some(after[..close].trim());
        }
        rest = &after[1..];
    }
    None
}

fn populate_flag_metadata(entry: &mut Entry) {
    let has_bracket = entry.desc.contains('[');
    if !has_bracket && !entry.desc.contains("Allowed values:") {
        return;
    }

    if let Some(raw_vals) = extract_possible_values_slice(&entry.desc) {
        for v in raw_vals
            .split(',')
            .map(str::trim)
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

    if has_bracket {
        if let Some(def_val) = extract_default_value_slice(&entry.desc) {
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

fn strip_leading_colon_desc(desc: &str) -> &str {
    if let Some(rest) = desc.strip_prefix(':') {
        if rest.is_empty() || rest.starts_with(char::is_whitespace) {
            return rest.trim_start();
        }
    }
    desc
}

fn parse_brace_subcommands(trimmed: &str) -> Option<Vec<String>> {
    let rest = trimmed.strip_prefix('{')?;
    let close = rest.find('}')?;
    let inner = &rest[..close];
    if inner.is_empty() {
        return None;
    }
    let mut cmds = Vec::new();
    for raw in inner.split(',').take(MAX_HELP_ENTRIES) {
        let name = raw.trim();
        if !name.is_empty()
            && name.len() <= 64
            && name
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
            && name.as_bytes()[0].is_ascii_alphanumeric()
        {
            cmds.push(name.to_string());
        }
    }
    (!cmds.is_empty()).then_some(cmds)
}

fn flush_positional_subcommands(
    pending: &mut Vec<String>,
    pending_set: &mut HashSet<String>,
    subcmds: &mut Vec<Entry>,
    seen: &mut HashSet<String>,
) {
    for cmd in pending.drain(..) {
        if subcmds.len() >= MAX_HELP_ENTRIES {
            break;
        }
        try_push_subcmd_entry(subcmds, seen, &cmd, "", "");
    }
    pending_set.clear();
}

/// Parses `--help` / `-h` output from Cobra, Clap, winget, git, Azure CLI (`az`/`knack`),
/// `gcloud`, Python (`argparse`/`click`/`typer`), and generic CLIs into Pane 2/3 [`Entry`]
/// values (subcommands, Parameters, Flags, enums, and defaults).
pub(super) fn parse_cli_help(help_text: &str) -> Vec<Entry> {
    static RE_SECTION_SUBCMD: OnceLock<Option<Regex>> = OnceLock::new();
    static RE_SECTION_SUBCMD_HEADER: OnceLock<Option<Regex>> = OnceLock::new();
    static RE_SECTION_FLAG: OnceLock<Option<Regex>> = OnceLock::new();
    static RE_SECTION_FLAG_HEADER: OnceLock<Option<Regex>> = OnceLock::new();
    static RE_LONG_FLAG_FALLBACK: OnceLock<Option<Regex>> = OnceLock::new();
    static RE_SHORT_FLAG_FALLBACK: OnceLock<Option<Regex>> = OnceLock::new();
    static RE_SUBCMD_FALLBACK: OnceLock<Option<Regex>> = OnceLock::new();

    let re_sec_sub = cached_regex(
        &RE_SECTION_SUBCMD,
        r"^\s{2,10}([a-zA-Z0-9][a-zA-Z0-9_-]*)(?::|,\s*([a-zA-Z0-9][a-zA-Z0-9_-]*))?(?:\s{2,}|\t|\s*:\s+)(.+)$",
    );
    let re_sec_sub_header = cached_regex(
        &RE_SECTION_SUBCMD_HEADER,
        r"^\s{2,6}([a-zA-Z0-9][a-zA-Z0-9_-]{1,24})(?:,\s*([a-zA-Z0-9][a-zA-Z0-9_-]*))?:?\s*$",
    );

    let re_sec_flag = cached_regex(
        &RE_SECTION_FLAG,
        r"^\s{1,10}(?:(-[a-zA-Z0-9\?])(?:\s+(?:<[^>]+>|\[[^\]]+\]|\{[^}]+\}|[A-Za-z0-9_-]+)\s*(?:,|\|)|,?\s*(?:\||,)?)\s*)?(--(?:\[no-\])?[a-zA-Z0-9][a-zA-Z0-9_\.-]*|-[a-zA-Z0-9\?])(?:[ =]([^\s].*?)|(\[[^\]]+\]))?(?:\s{2,}|\t)(.+)$",
    );
    let re_sec_flag_header = cached_regex(
        &RE_SECTION_FLAG_HEADER,
        r"^\s{1,10}(?:(-[a-zA-Z0-9\?])(?:\s+(?:<[^>]+>|\[[^\]]+\]|\{[^}]+\}|[A-Za-z0-9_-]+)\s*(?:,|\|)|,?\s*(?:\||,)?)\s*)?(--(?:\[no-\])?[a-zA-Z0-9][a-zA-Z0-9_\.-]*|-[a-zA-Z0-9\?])(?: (<[^>]+>|\[[^\]]+\]|\{[^}]+\}|[A-Z][A-Z0-9_.-]*)|=([^\s]+)|(\[[^\]]+\]))?\s*$",
    );

    let bounded_help = truncate_utf8_prefix(help_text, MAX_HELP_BYTES);
    let clean_help = strip_help_text_escapes(bounded_help);

    let est_entries = (clean_help.len() / 96).clamp(8, 256);
    let mut subcmds: Vec<Entry> = Vec::with_capacity(est_entries / 2);
    let mut flags: Vec<Entry> = Vec::with_capacity(est_entries);
    let mut seen = HashSet::with_capacity(est_entries);

    let mut current_sec = HelpSection::None;
    let mut detected_sections = false;
    let mut active_item: Option<(HelpSection, usize, usize, usize)> = None;
    let mut section_cmd_indent: Option<usize> = None;
    let mut positional_subcmds: Vec<String> = Vec::new();
    let mut positional_subcmd_set: HashSet<String> = HashSet::new();

    for raw_line in clean_help.lines().take(MAX_HELP_LINES) {
        let bounded_line = truncate_utf8_prefix(raw_line, MAX_HELP_LINE_BYTES);
        let raw_trimmed = bounded_line.trim();
        if raw_trimmed.is_empty() {
            if matches!(current_sec, HelpSection::Ignore | HelpSection::None) {
                active_item = None;
            }
            continue;
        }
        if is_box_bottom_border(raw_trimmed) {
            active_item = None;
            continue;
        }
        let unwrapped_box = unwrap_box_row(bounded_line);
        let line = unwrapped_box.as_deref().unwrap_or(bounded_line);
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        if unwrapped_box.is_none() && trimmed.len() <= 256 && !trimmed.starts_with('-') {
            if let Some(sec) = classify_section_header(line) {
                if current_sec == HelpSection::Positional {
                    flush_positional_subcommands(
                        &mut positional_subcmds,
                        &mut positional_subcmd_set,
                        &mut subcmds,
                        &mut seen,
                    );
                }
                current_sec = sec;
                detected_sections = true;
                active_item = None;
                section_cmd_indent = None;
                continue;
            }
        }

        if matches!(current_sec, HelpSection::Ignore | HelpSection::None) {
            active_item = None;
            continue;
        }

        let line_indent = leading_ws_count(line);
        let at_entry_cap = subcmds.len() + flags.len() >= MAX_HELP_ENTRIES;

        match current_sec {
            HelpSection::Commands => {
                let within_cmd_col =
                    !at_entry_cap && section_cmd_indent.is_none_or(|ci| line_indent <= ci + 2);
                let first_is_alnum = trimmed
                    .as_bytes()
                    .first()
                    .is_some_and(u8::is_ascii_alphanumeric);
                if within_cmd_col && first_is_alnum {
                    let matched_sec_sub = if (2..=8).contains(&line_indent)
                        && (trimmed.contains(':')
                            || trimmed.contains('\t')
                            || has_two_adjacent_ws(trimmed))
                    {
                        re_sec_sub.and_then(|re| re.captures(line))
                    } else {
                        None
                    };
                    if let Some(caps) = matched_sec_sub {
                        let cmd = caps.get(1).map(|m| m.as_str().trim()).unwrap_or("");
                        let alias = caps.get(2).map(|m| m.as_str().trim()).unwrap_or("");
                        let desc = strip_leading_colon_desc(
                            caps.get(3).map(|m| m.as_str().trim()).unwrap_or(""),
                        );
                        if let Some(idx) =
                            try_push_subcmd_entry(&mut subcmds, &mut seen, cmd, alias, desc)
                        {
                            section_cmd_indent.get_or_insert(line_indent);
                            active_item = Some((HelpSection::Commands, idx, line_indent, 0));
                            continue;
                        }
                    } else if active_item
                        .as_ref()
                        .is_none_or(|(_, _, ai, _)| line_indent <= *ai)
                        && (2..=6).contains(&line_indent)
                        && trimmed.len() >= 2
                        && (!trimmed.contains(' ') || trimmed.contains(','))
                    {
                        if let Some(caps) = re_sec_sub_header.and_then(|re| re.captures(line)) {
                            let cmd = caps.get(1).map(|m| m.as_str().trim()).unwrap_or("");
                            let alias = caps.get(2).map(|m| m.as_str().trim()).unwrap_or("");
                            if let Some(idx) =
                                try_push_subcmd_entry(&mut subcmds, &mut seen, cmd, alias, "")
                            {
                                section_cmd_indent.get_or_insert(line_indent);
                                active_item = Some((HelpSection::Commands, idx, line_indent, 0));
                                continue;
                            }
                        }
                    }
                }

                if let Some((HelpSection::Commands, idx, item_indent, ref mut cont)) = active_item {
                    if line_indent > item_indent && *cont < MAX_CONTINUATION_LINES {
                        append_desc_line(&mut subcmds[idx], trimmed);
                        *cont += 1;
                    } else {
                        active_item = None;
                    }
                }
            }
            HelpSection::Positional => {
                if !at_entry_cap && trimmed.starts_with('{') {
                    if let Some(parsed_cmds) = parse_brace_subcommands(trimmed) {
                        for cmd in parsed_cmds {
                            if positional_subcmd_set.insert(cmd.clone()) {
                                positional_subcmds.push(cmd);
                            }
                        }
                        active_item = None;
                        continue;
                    }
                }
                if !at_entry_cap
                    && !positional_subcmd_set.is_empty()
                    && (2..=10).contains(&line_indent)
                    && (trimmed.contains('\t') || has_two_adjacent_ws(trimmed))
                {
                    if let Some(caps) = re_sec_sub.and_then(|re| re.captures(line)) {
                        let cmd = caps.get(1).map(|m| m.as_str().trim()).unwrap_or("");
                        if positional_subcmd_set.contains(cmd) {
                            let alias = caps.get(2).map(|m| m.as_str().trim()).unwrap_or("");
                            let desc = strip_leading_colon_desc(
                                caps.get(3).map(|m| m.as_str().trim()).unwrap_or(""),
                            );
                            if let Some(idx) =
                                try_push_subcmd_entry(&mut subcmds, &mut seen, cmd, alias, desc)
                            {
                                active_item = Some((HelpSection::Positional, idx, line_indent, 0));
                                continue;
                            }
                        }
                    }
                }
                if let Some((HelpSection::Positional, idx, item_indent, ref mut cont)) = active_item
                {
                    if line_indent > item_indent && *cont < MAX_CONTINUATION_LINES {
                        append_desc_line(&mut subcmds[idx], trimmed);
                        *cont += 1;
                    } else {
                        active_item = None;
                    }
                }
            }
            HelpSection::Flags => {
                let looks_like_flag_line = !at_entry_cap
                    && (1..=10).contains(&line_indent)
                    && trimmed.len() >= 2
                    && trimmed.as_bytes().first() == Some(&b'-')
                    && trimmed
                        .as_bytes()
                        .get(1)
                        .is_some_and(|&b| b == b'-' || b == b'?' || b.is_ascii_alphanumeric());

                if looks_like_flag_line {
                    let matched_sec_flag = if trimmed.contains('\t') || has_two_adjacent_ws(trimmed)
                    {
                        re_sec_flag.and_then(|re| re.captures(line))
                    } else {
                        None
                    };
                    if let Some(caps) = matched_sec_flag {
                        let short = caps.get(1).map(|m| m.as_str().trim()).unwrap_or("");
                        let raw_flag = caps.get(2).map(|m| m.as_str().trim()).unwrap_or("");
                        let arg_hint = caps
                            .get(3)
                            .or_else(|| caps.get(4))
                            .map(|m| m.as_str().trim())
                            .unwrap_or("");
                        let desc = caps.get(5).map(|m| m.as_str().trim()).unwrap_or("");
                        active_item = try_push_flag_entry(
                            &mut flags, &mut seen, short, raw_flag, arg_hint, desc,
                        )
                        .map(|idx| (HelpSection::Flags, idx, line_indent, 0));
                        continue;
                    } else if let Some(caps) = re_sec_flag_header.and_then(|re| re.captures(line)) {
                        let short = caps.get(1).map(|m| m.as_str().trim()).unwrap_or("");
                        let raw_flag = caps.get(2).map(|m| m.as_str().trim()).unwrap_or("");
                        let arg_hint = caps
                            .get(3)
                            .or_else(|| caps.get(4))
                            .or_else(|| caps.get(5))
                            .map(|m| m.as_str().trim())
                            .unwrap_or("");
                        active_item = try_push_flag_entry(
                            &mut flags, &mut seen, short, raw_flag, arg_hint, "",
                        )
                        .map(|idx| (HelpSection::Flags, idx, line_indent, 0));
                        continue;
                    }
                }
                if let Some((HelpSection::Flags, idx, item_indent, ref mut cont)) = active_item {
                    if line_indent > item_indent && *cont < MAX_CONTINUATION_LINES {
                        append_desc_line(&mut flags[idx], trimmed);
                        *cont += 1;
                    } else {
                        active_item = None;
                    }
                }
            }
            HelpSection::Ignore | HelpSection::None => {
                active_item = None;
            }
        }
    }

    if !positional_subcmds.is_empty() {
        flush_positional_subcommands(
            &mut positional_subcmds,
            &mut positional_subcmd_set,
            &mut subcmds,
            &mut seen,
        );
    }

    if !detected_sections || (subcmds.is_empty() && flags.is_empty()) {
        let re_long = cached_regex(
            &RE_LONG_FLAG_FALLBACK,
            r"^\s{1,10}(?:(-[a-zA-Z0-9\?])(?:\s+(?:<[^>]+>|\[[^\]]+\]|[A-Za-z0-9_-]+)\s*(?:,|\|)|,?\s*(?:\||,)?)\s*)?(--(?:\[no-\])?[a-zA-Z0-9][a-zA-Z0-9_-]*)(?:[ =](<[^>]+>|\[[^\]]+\]|[A-Z][A-Z0-9_.-]*|[^\s\]]+\]?)|(\[[^\]]+\]))?(?:\s{2,}(.+))?$",
        );
        let re_short = cached_regex(
            &RE_SHORT_FLAG_FALLBACK,
            r"^\s{1,10}(-[a-zA-Z0-9\?])(?:\s+(<[^>]+>|\[[^\]]+\]))?\s{2,}(.+)$",
        );
        let re_sub = cached_regex(
            &RE_SUBCMD_FALLBACK,
            r"^\s{2,8}([a-z][a-z0-9_-]{1,24})(?:,\s*([a-z0-9_-]+))?\s{2,}([A-Za-z].+)$",
        );

        let mut active_fallback_flag: Option<(usize, usize, usize)> = None;

        for raw_line in clean_help.lines().take(MAX_HELP_LINES) {
            let line = truncate_utf8_prefix(raw_line, MAX_HELP_LINE_BYTES);
            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }
            let line_indent = leading_ws_count(line);
            let at_entry_cap = subcmds.len() + flags.len() >= MAX_HELP_ENTRIES;

            if !at_entry_cap {
                let first_byte = trimmed.as_bytes().first().copied();
                let maybe_flag = (1..=10).contains(&line_indent) && first_byte == Some(b'-');
                let matched_long = if maybe_flag && trimmed.contains("--") {
                    re_long.and_then(|re| re.captures(line))
                } else {
                    None
                };

                if let Some(caps) = matched_long {
                    let short = caps.get(1).map(|m| m.as_str().trim()).unwrap_or("");
                    let raw_long = caps.get(2).map(|m| m.as_str().trim()).unwrap_or("");
                    let arg_hint = caps
                        .get(3)
                        .or_else(|| caps.get(4))
                        .map(|m| m.as_str().trim())
                        .unwrap_or("");
                    let desc = caps.get(5).map(|m| m.as_str().trim()).unwrap_or("");
                    active_fallback_flag =
                        try_push_flag_entry(&mut flags, &mut seen, short, raw_long, arg_hint, desc)
                            .map(|idx| (idx, line_indent, 0));
                    continue;
                }

                let matched_short = if maybe_flag
                    && trimmed.len() >= 5
                    && trimmed
                        .as_bytes()
                        .get(1)
                        .is_some_and(|&b| b != b'-' && (b.is_ascii_alphanumeric() || b == b'?'))
                    && has_two_adjacent_ws(trimmed)
                {
                    re_short.and_then(|re| re.captures(line))
                } else {
                    None
                };

                if let Some(caps) = matched_short {
                    let short = caps.get(1).map(|m| m.as_str().trim()).unwrap_or("");
                    let arg_hint = caps.get(2).map(|m| m.as_str().trim()).unwrap_or("");
                    let desc = caps.get(3).map(|m| m.as_str().trim()).unwrap_or("");
                    active_fallback_flag =
                        try_push_flag_entry(&mut flags, &mut seen, "", short, arg_hint, desc)
                            .map(|idx| (idx, line_indent, 0));
                    continue;
                }

                let matched_sub = if (2..=8).contains(&line_indent)
                    && first_byte.is_some_and(|b| b.is_ascii_lowercase())
                    && has_two_adjacent_ws(trimmed)
                {
                    re_sub.and_then(|re| re.captures(line))
                } else {
                    None
                };

                if let Some(caps) = matched_sub {
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
                    if !is_ignored {
                        try_push_subcmd_entry(&mut subcmds, &mut seen, sub, alias, desc);
                    }
                    continue;
                }
            }
            if let Some((idx, item_indent, ref mut cont)) = active_fallback_flag {
                if line_indent > item_indent && *cont < MAX_CONTINUATION_LINES {
                    append_desc_line(&mut flags[idx], trimmed);
                    *cont += 1;
                } else {
                    active_fallback_flag = None;
                }
            }
        }
    }

    for entry in &mut flags {
        if entry.desc.is_empty() {
            entry.desc = format!("CLI Flag {}", entry.text);
        } else {
            populate_flag_metadata(entry);
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

    #[test]
    fn test_perf_parse_cli_help_throughput() {
        use std::fmt::Write as _;
        use std::hint::black_box;
        use std::time::{Duration, Instant};

        let mut help = String::with_capacity(48 * 1024);
        help.push_str("Usage: benchtool [OPTIONS] <COMMAND>\n\nCommands:\n");
        for i in 0..120 {
            let _ = writeln!(
                help,
                "  subcmd-{i:03}, s{i:03}   Execute operation {i} across workspace targets\n                         with extended wrapped continuation details for subcommand {i}"
            );
        }
        help.push_str("\nGlobal Options:\n");
        for i in 0..180 {
            let _ = writeln!(
                help,
                "  -{}, --option-{i:03} <VAL>   Configure option {i}\n                               Multi-line option description line 1 for {i}\n                               Multi-line option description line 2 [default: auto] [possible values: auto, fast, strict]",
                char::from(b'a' + (i % 26) as u8),
            );
        }
        help.push_str("\nExamples:\n  benchtool subcmd-001 --option-001 fast\n");

        let iterations = 150usize;
        // Warm up regex OnceLock initialization before measuring steady-state throughput.
        let warmup = parse_cli_help(black_box(&help));
        assert_eq!(warmup.len(), 300);

        let start = Instant::now();
        let mut total_entries = 0usize;
        for _ in 0..iterations {
            let parsed = parse_cli_help(black_box(&help));
            total_entries += parsed.len();
        }
        let elapsed = start.elapsed();
        let total_bytes = help.len() * iterations;
        let mb_per_sec = (total_bytes as f64 / (1024.0 * 1024.0)) / elapsed.as_secs_f64();
        let us_per_call = elapsed.as_secs_f64() * 1_000_000.0 / iterations as f64;

        println!(
            "parse_cli_help throughput: {iterations} iters x {} bytes = {:.2} MiB in {:?} ({us_per_call:.1} us/call, {mb_per_sec:.2} MiB/s)",
            help.len(),
            total_bytes as f64 / (1024.0 * 1024.0),
            elapsed
        );

        assert_eq!(total_entries, 300 * iterations);
        let max_budget = if cfg!(debug_assertions) {
            Duration::from_secs(10)
        } else {
            Duration::from_secs(2)
        };
        assert!(
            elapsed < max_budget,
            "parse_cli_help took {elapsed:?}, expected < {max_budget:?}"
        );
    }

    #[test]
    fn test_pentest_help_parser_adversarial_inputs_and_metadata_scanners() {
        // 1. Oversized help and oversized single lines with multibyte UTF-8 straddling byte caps.
        let mut huge = String::from("Usage: pentest\n\nCommands:\n  run   Run task\n\nOptions:\n");
        let crab_line = format!("  --utf8   {}\n", "🦀".repeat(700)); // > MAX_HELP_LINE_BYTES
        huge.push_str(&crab_line);
        while huge.len() < MAX_HELP_BYTES + 4096 {
            huge.push_str(&"🦀".repeat(128));
            huge.push('\n');
        }
        let parsed_huge = parse_cli_help(&huge);
        assert!(parsed_huge.iter().any(|e| e.text == "run"));
        let utf8_opt = parsed_huge
            .iter()
            .find(|e| e.text == "--utf8")
            .expect("--utf8 survived oversized line");
        assert!(utf8_opt.desc.len() <= MAX_DESC_BYTES);

        // 2. Hostile CSI/OSC/C1/Bidi/line-separator/CR/NUL payloads + malformed metadata hints.
        let oversized_enum = "z".repeat(200);
        let hostile = format!(
            concat!(
                "\x1b[2J\x1b]8;;https://evil.example\x1b\\Usage:\x1b]8;;\x07\n",
                "Commands:\n",
                "  \x1b[31mdeploy, d   Deploy\u{061c}\u{2028}\u{2029}\u{2066} app\rspoof\0\x07\n",
                "  deploy   Duplicate ignored\n",
                "Options:\n",
                "  --[no-]color   Toggle color\n",
                "  --color        Duplicate after normalization ignored\n",
                "  --mode <M>     Broken [possible values: ignored_unclosed and Allowed values: fast, {}, safe.\n",
                "  --tag <T>      Empty [possible values:] [possible values:   ] [default:] [default: alpha] [default: beta]\n",
                "  --unclosed <U>   Unterminated [default: never_closed\n",
            ),
            oversized_enum
        );
        let entries = parse_cli_help(&hostile);

        let deploy = entries.iter().find(|e| e.text == "deploy").expect("deploy");
        assert_eq!(deploy.desc, "Deploy appspoof");
        assert!(!deploy.desc.contains('\u{061c}'));
        assert!(!deploy.desc.contains('\u{2028}'));
        assert!(!deploy.desc.contains('\u{2029}'));
        assert!(!deploy.desc.contains('\u{2066}'));
        assert!(!deploy.desc.contains('\r'));
        assert!(!deploy.desc.contains('\0'));

        let color_count = entries.iter().filter(|e| e.text == "--color").count();
        assert_eq!(color_count, 1);

        let mode = entries.iter().find(|e| e.text == "--mode").expect("--mode");
        let mode_enums: Vec<&str> = mode.children.iter().map(|c| c.text.as_str()).collect();
        assert_eq!(mode_enums, vec!["fast", "safe"]);

        let tag = entries.iter().find(|e| e.text == "--tag").expect("--tag");
        assert_eq!(tag.children.len(), 1);
        assert_eq!(tag.children[0].text, "alpha");
        assert_eq!(tag.children[0].kind, EntryKind::Value);

        let unclosed = entries
            .iter()
            .find(|e| e.text == "--unclosed")
            .expect("--unclosed");
        assert!(unclosed.children.is_empty());

        // 3. Fallback path with adversarial unterminated escapes and duplicates.
        let fallback_hostile =
            "  -x, --exec <CMD>   Run\u{202e} \x1b[999999cmd\n  --exec   Duplicate fallback\n  build, b   Build\n  build   Duplicate sub\n";
        let fb = parse_cli_help(fallback_hostile);
        assert_eq!(fb.iter().filter(|e| e.text == "--exec").count(), 1);
        assert_eq!(fb.iter().filter(|e| e.text == "build").count(), 1);
        let exec = fb.iter().find(|e| e.text == "--exec").expect("--exec");
        assert!(!exec.desc.contains('\u{202e}'));
        assert!(!exec.desc.contains('\x1b'));
    }

    #[test]
    fn test_parse_az_knack_and_gcloud_subgroups_groups_help() {
        let az_help = r#"
Group
    az vm : Manage Linux or Windows virtual machines.

Subgroups:
    application          : Manage applications for VM.
    availability-set     : Group resources into availability sets.
    disk                 : Manage the managed data disks attached to a VM.

Commands:
    create               : Create an Azure Virtual Machine.
    deallocate           : Deallocate a VM so that computing resources are no longer allocated.
    list                 : List details of Virtual Machines.

Arguments
    --resource-group -g  [Required] : Name of resource group.
    --output -o                     : Output format.  Allowed values: json, jsonc, none, table, tsv, yaml.  Default: json.
    --verbose                       : Increase logging verbosity.
    --help -h                       : Show this help message and exit.
"#;
        let items = parse_cli_help(az_help);
        let subcmds: Vec<&str> = items
            .iter()
            .filter(|i| i.kind == EntryKind::Subcmd)
            .map(|i| i.text.as_str())
            .collect();
        assert_eq!(
            subcmds,
            vec![
                "application",
                "availability-set",
                "disk",
                "create",
                "deallocate",
                "list"
            ]
        );
        let app = items.iter().find(|i| i.text == "application").unwrap();
        assert_eq!(app.desc, "Manage applications for VM.");

        let rg = items
            .iter()
            .find(|i| i.text == "--resource-group")
            .expect("--resource-group");
        assert_eq!(rg.kind, EntryKind::Param);
        assert_eq!(rg.aliases, vec!["-g"]);

        let output = items
            .iter()
            .find(|i| i.text == "--output")
            .expect("--output");
        assert_eq!(output.kind, EntryKind::Param);
        assert_eq!(output.aliases, vec!["-o"]);
        let out_enums: Vec<&str> = output.children.iter().map(|c| c.text.as_str()).collect();
        assert!(out_enums.contains(&"json"));
        assert!(out_enums.contains(&"table"));
        assert!(out_enums.contains(&"yaml"));

        let verbose = items
            .iter()
            .find(|i| i.text == "--verbose")
            .expect("--verbose");
        assert_eq!(verbose.kind, EntryKind::Flag);
        assert_eq!(verbose.desc, "Increase logging verbosity.");

        let help_flag = items.iter().find(|i| i.text == "--help").expect("--help");
        assert_eq!(help_flag.kind, EntryKind::Flag);
        assert_eq!(help_flag.aliases, vec!["-h"]);

        let gcloud_help = r#"
GROUPS
    GROUP is one of the following:

     compute
        Create and manipulate Compute Engine resources.

     storage
        Create and manage Cloud Storage buckets and objects.

COMMANDS
    COMMAND is one of the following:

     auth
        Manage oauth2 credentials for the Google Cloud CLI.
"#;
        let gcloud_items = parse_cli_help(gcloud_help);
        let gcloud_subs: Vec<&str> = gcloud_items
            .iter()
            .filter(|i| i.kind == EntryKind::Subcmd)
            .map(|i| i.text.as_str())
            .collect();
        assert_eq!(gcloud_subs, vec!["compute", "storage", "auth"]);
        let compute = gcloud_items.iter().find(|i| i.text == "compute").unwrap();
        assert!(compute.desc.contains("Compute Engine"));
    }

    #[test]
    fn test_parse_python_argparse_help_with_subparsers_and_wrapped_metavars() {
        let help = r#"
usage: pytool [-h] [-v] [-f FILE] [--format {json,yaml,text}] {init,build,deploy} ...

A sample Python argparse CLI tool.

positional arguments:
  {init,build,deploy}   Available subcommands
    init                Initialize a new project
    build               Build project artifacts
    deploy              Deploy to target environment

optional arguments:
  -h, --help            show this help message and exit
  -v, --verbose         enable verbose logging
  -f FILE, --file FILE  path to configuration file
  -o, --output OUTPUT   destination output path
  --format {json,yaml,text}
                        output serialization format
  --dry-run             preview actions without executing
"#;
        let items = parse_cli_help(help);
        let subcmds: Vec<&str> = items
            .iter()
            .filter(|i| i.kind == EntryKind::Subcmd)
            .map(|i| i.text.as_str())
            .collect();
        assert_eq!(subcmds, vec!["init", "build", "deploy"]);
        assert_eq!(
            items.iter().find(|i| i.text == "build").unwrap().desc,
            "Build project artifacts"
        );

        let file_param = items.iter().find(|i| i.text == "--file").expect("--file");
        assert_eq!(file_param.kind, EntryKind::Param);
        assert_eq!(file_param.aliases, vec!["-f"]);

        let output_param = items
            .iter()
            .find(|i| i.text == "--output")
            .expect("--output");
        assert_eq!(output_param.kind, EntryKind::Param);
        assert_eq!(output_param.aliases, vec!["-o"]);

        let format_param = items
            .iter()
            .find(|i| i.text == "--format")
            .expect("--format");
        assert_eq!(format_param.kind, EntryKind::Param);
        assert!(format_param.desc.contains("output serialization format"));
        let fmt_vals: Vec<&str> = format_param
            .children
            .iter()
            .map(|c| c.text.as_str())
            .collect();
        assert_eq!(fmt_vals, vec!["json", "yaml", "text"]);

        let dry_run = items
            .iter()
            .find(|i| i.text == "--dry-run")
            .expect("--dry-run");
        assert_eq!(dry_run.kind, EntryKind::Flag);

        // Python 3.10+ `options:` heading and non-subparser `positional arguments:` must not create fake subcommands
        let py312_help = r#"
usage: copytool [-h] [--force] src dst

positional arguments:
  src        Source path
  dst        Destination path

options:
  -h, --help   show this help message and exit
  --force      overwrite existing destination
"#;
        let py312_items = parse_cli_help(py312_help);
        assert!(
            py312_items.iter().all(|i| i.kind != EntryKind::Subcmd),
            "plain positional arguments without {{...}} subparsers must not become Subcmd entries"
        );
        assert!(py312_items
            .iter()
            .any(|i| i.text == "--force" && i.kind == EntryKind::Flag));
    }

    #[test]
    fn test_parse_python_click_and_typer_help_fixtures() {
        let click_help = r#"
Usage: clickcli [OPTIONS] COMMAND [ARGS]...

  Sample Click CLI application.

Options:
  -c, --config PATH               Path to config file.
  -f, --format [json|yaml|table]  Output format.  [default: table]
  -v, --verbose / --no-verbose    Enable verbose output.
  --help                          Show this message and exit.

Commands:
  build   Build the project.
  deploy  Deploy the application.
"#;
        let click_items = parse_cli_help(click_help);
        let click_subs: Vec<&str> = click_items
            .iter()
            .filter(|i| i.kind == EntryKind::Subcmd)
            .map(|i| i.text.as_str())
            .collect();
        assert_eq!(click_subs, vec!["build", "deploy"]);

        let config = click_items
            .iter()
            .find(|i| i.text == "--config")
            .expect("--config");
        assert_eq!(config.kind, EntryKind::Param);
        assert_eq!(config.aliases, vec!["-c"]);

        let fmt = click_items
            .iter()
            .find(|i| i.text == "--format")
            .expect("--format");
        assert_eq!(fmt.kind, EntryKind::Param);
        assert_eq!(fmt.aliases, vec!["-f"]);
        let fmt_vals: Vec<&str> = fmt.children.iter().map(|c| c.text.as_str()).collect();
        assert!(fmt_vals.contains(&"json"));
        assert!(fmt_vals.contains(&"yaml"));
        assert!(fmt_vals.contains(&"table"));

        let verbose = click_items
            .iter()
            .find(|i| i.text == "--verbose")
            .expect("--verbose");
        assert_eq!(verbose.kind, EntryKind::Flag);
        assert!(verbose.aliases.contains(&"-v".to_string()));
        assert!(verbose.aliases.contains(&"--no-verbose".to_string()));

        let typer_rich_help = r#"
 Usage: typercli [OPTIONS] COMMAND [ARGS]...

 Manage workspace services.

╭─ Options ────────────────────────────────────────────────────────────────────╮
│ --install-completion        [bash|zsh|fish|pwsh]  Install completion         │
│ --verbose             -v                          Enable verbose output.     │
│ --config              -c    TEXT                  Config path                │
│                                                   [default: config.toml]     │
│ --env                 -e    [dev|staging|prod]    Target environment         │
│ --help                -h                          Show this message and      │
│                                                   exit.                      │
╰──────────────────────────────────────────────────────────────────────────────╯
╭─ Commands ───────────────────────────────────────────────────────────────────╮
│ deploy   Deploy services to target environment.                              │
│ status   Check current service health.                                       │
╰──────────────────────────────────────────────────────────────────────────────╯
"#;
        let typer_items = parse_cli_help(typer_rich_help);
        let typer_subs: Vec<&str> = typer_items
            .iter()
            .filter(|i| i.kind == EntryKind::Subcmd)
            .map(|i| i.text.as_str())
            .collect();
        assert_eq!(typer_subs, vec!["deploy", "status"]);

        let t_verbose = typer_items
            .iter()
            .find(|i| i.text == "--verbose")
            .expect("typer --verbose");
        assert_eq!(t_verbose.kind, EntryKind::Flag);
        assert_eq!(t_verbose.aliases, vec!["-v"]);
        assert_eq!(t_verbose.desc, "Enable verbose output.");

        let t_config = typer_items
            .iter()
            .find(|i| i.text == "--config")
            .expect("typer --config");
        assert_eq!(t_config.kind, EntryKind::Param);
        assert_eq!(t_config.aliases, vec!["-c"]);
        assert!(t_config.desc.contains("Config path"));
        assert!(t_config
            .children
            .iter()
            .any(|c| c.text == "config.toml" && c.kind == EntryKind::Value));

        let t_env = typer_items
            .iter()
            .find(|i| i.text == "--env")
            .expect("typer --env");
        assert_eq!(t_env.kind, EntryKind::Param);
        assert_eq!(t_env.aliases, vec!["-e"]);
        let env_vals: Vec<&str> = t_env.children.iter().map(|c| c.text.as_str()).collect();
        assert_eq!(env_vals, vec!["dev", "staging", "prod"]);

        let t_comp = typer_items
            .iter()
            .find(|i| i.text == "--install-completion")
            .expect("typer --install-completion");
        assert_eq!(t_comp.kind, EntryKind::Param);
        assert!(t_comp.children.iter().any(|c| c.text == "fish"));
    }
}
