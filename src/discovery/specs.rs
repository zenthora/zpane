use super::catalog::cache_dir;
use crate::model::{Entry, EntryKind};
use serde::Deserialize;
use std::collections::{BTreeMap, HashSet};
use std::path::PathBuf;

/// Returns the local directory searched for declarative JSON/YAML CLI specifications:
/// `%LOCALAPPDATA%\zpane\specs` on Windows, `$XDG_CACHE_HOME/zpane/specs` or `~/.cache/zpane/specs` on Unix.
pub(super) fn specs_dir() -> PathBuf {
    cache_dir().join("specs")
}

/// Returns true if a filename has a supported declarative spec extension (`.json`, `.yaml`, `.yml`).
pub(super) fn is_spec_filename(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    lower.ends_with(".json") || lower.ends_with(".yaml") || lower.ends_with(".yml")
}

/// Parses a declarative specification (Carapace YAML/JSON or simplified Fig JSON) into a root [`Entry`].
pub(super) fn parse_spec_str(file_name: &str, content: &str) -> anyhow::Result<Entry> {
    let trimmed = content.trim_start_matches('\u{feff}').trim();
    if trimmed.is_empty() {
        anyhow::bail!("Empty spec file: {file_name}");
    }

    let value: serde_json::Value =
        if file_name.to_ascii_lowercase().ends_with(".json") || trimmed.starts_with('{') {
            serde_json::from_str(trimmed)?
        } else {
            yaml_to_json(trimmed)?
        };

    spec_value_to_entry(&value)
}

fn spec_value_to_entry(value: &serde_json::Value) -> anyhow::Result<Entry> {
    let Some(obj) = value.as_object() else {
        anyhow::bail!("Spec root must be an object or YAML mapping");
    };

    if obj.contains_key("flags")
        || obj.contains_key("persistentflags")
        || obj.contains_key("commands")
        || obj.contains_key("completion")
    {
        let spec: CarapaceSpec = serde_json::from_value(value.clone())?;
        if spec.name.trim().is_empty() {
            anyhow::bail!("carapace-spec missing required 'name'");
        }
        Ok(carapace_spec_to_entry(&spec, &BTreeMap::new(), true))
    } else if obj.contains_key("subcommands")
        || obj.contains_key("options")
        || obj.contains_key("args")
    {
        let spec: FigSpec = serde_json::from_value(value.clone())?;
        if spec.name.trim().is_empty() {
            anyhow::bail!("Fig spec missing required 'name'");
        }
        Ok(fig_spec_to_entry(&spec, true))
    } else if let Some(name) = obj.get("name").and_then(|v| v.as_str()) {
        let trimmed_name = name.trim();
        if trimmed_name.is_empty() {
            anyhow::bail!("Declarative command spec missing required 'name'");
        }
        let desc = obj
            .get("description")
            .and_then(|v| v.as_str())
            .unwrap_or("Declarative command spec");
        Ok(Entry::new(trimmed_name, trimmed_name, EntryKind::Cli, desc))
    } else {
        anyhow::bail!("Unrecognized declarative spec format");
    }
}

fn deserialize_null_default<'de, D, T>(deserializer: D) -> Result<T, D::Error>
where
    T: Default + Deserialize<'de>,
    D: serde::Deserializer<'de>,
{
    let opt = Option::deserialize(deserializer)?;
    Ok(opt.unwrap_or_default())
}

// ============================================================================
// Carapace-Spec schema & conversion
// ============================================================================

#[derive(Debug, Default, Deserialize)]
struct CarapaceSpec {
    #[serde(default, deserialize_with = "deserialize_null_default")]
    name: String,
    #[serde(default, deserialize_with = "deserialize_null_default")]
    description: String,
    #[serde(default, deserialize_with = "deserialize_null_default")]
    aliases: Vec<String>,
    #[serde(default, deserialize_with = "deserialize_null_default")]
    flags: BTreeMap<String, String>,
    #[serde(default, deserialize_with = "deserialize_null_default")]
    persistentflags: BTreeMap<String, String>,
    #[serde(default, deserialize_with = "deserialize_null_default")]
    commands: Vec<CarapaceSpec>,
    #[serde(default, deserialize_with = "deserialize_null_default")]
    completion: CarapaceCompletion,
}

#[derive(Debug, Default, Deserialize)]
struct CarapaceCompletion {
    #[serde(default, deserialize_with = "deserialize_null_default")]
    flag: BTreeMap<String, Vec<String>>,
    #[serde(default, deserialize_with = "deserialize_null_default")]
    positional: Vec<Vec<String>>,
    #[serde(default, deserialize_with = "deserialize_null_default")]
    positionalany: Vec<String>,
}

fn carapace_spec_to_entry(
    spec: &CarapaceSpec,
    inherited_persistent: &BTreeMap<String, String>,
    is_root: bool,
) -> Entry {
    let kind = if is_root {
        EntryKind::Cli
    } else {
        EntryKind::Subcmd
    };

    let mut effective_persistent = inherited_persistent.clone();
    for (k, v) in &spec.persistentflags {
        effective_persistent.insert(k.clone(), v.clone());
    }

    let spec_name = spec.name.trim();
    let label = if !is_root && !spec.aliases.is_empty() {
        format!("{spec_name} ({})", spec.aliases.join(", "))
    } else {
        spec_name.to_string()
    };

    let mut entry =
        Entry::new(spec_name, label, kind, &spec.description).with_aliases(spec.aliases.clone());

    let mut children = Vec::new();
    let mut seen_sub = HashSet::new();

    // 1. Subcommands first (matches CLI Explorer Pane 2 ergonomics)
    for sub in &spec.commands {
        let sub_name = sub.name.trim();
        if !sub_name.is_empty() && seen_sub.insert(sub_name.to_ascii_lowercase()) {
            children.push(carapace_spec_to_entry(sub, &effective_persistent, false));
        }
    }

    // 2. Positional completion hints (for subcommands or commands with positional args)
    let mut pos_candidates = Vec::new();
    if let Some(first_pos) = spec.completion.positional.first() {
        pos_candidates.extend(first_pos.iter().cloned());
    }
    pos_candidates.extend(spec.completion.positionalany.iter().cloned());
    let mut seen_pos = HashSet::new();
    for raw in pos_candidates {
        if let Some(val_entry) = carapace_completion_item_to_entry(&raw, EntryKind::Arg) {
            let dedup_key = if val_entry.is_placeholder() {
                format!("ph:{}", val_entry.label.to_ascii_lowercase())
            } else {
                format!("tok:{}", val_entry.text.to_ascii_lowercase())
            };
            if seen_pos.insert(dedup_key) {
                children.push(val_entry);
            }
        }
    }

    // 3. Local flags first, then inherited/persistent flags so local overrides win even when
    //    suffix markers (=, ?, *) or short-flag prefixes differ.
    let mut seen_flags = HashSet::new();
    for (flag_spec, desc) in spec.flags.iter().chain(effective_persistent.iter()) {
        if let Some(flag_entry) = parse_carapace_flag(flag_spec, desc, &spec.completion.flag) {
            if seen_flags.insert(flag_entry.text.to_ascii_lowercase()) {
                children.push(flag_entry);
            }
        }
    }

    entry.children = children;
    entry
}

fn parse_carapace_flag(
    raw_spec: &str,
    desc: &str,
    flag_completions: &BTreeMap<String, Vec<String>>,
) -> Option<Entry> {
    let mut trimmed = raw_spec.trim();
    if trimmed.is_empty() {
        return None;
    }

    let mut takes_arg = false;
    while let Some(last) = trimmed.chars().last() {
        if matches!(last, '=' | '?' | '*') {
            if matches!(last, '=' | '?') {
                takes_arg = true;
            }
            trimmed = &trimmed[..trimmed.len() - last.len_utf8()];
        } else {
            break;
        }
    }

    let parts: Vec<&str> = trimmed
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .collect();
    if parts.is_empty() {
        return None;
    }

    // Prefer long option (--foo) as primary token; keep short/other variants as Parameter Aliases
    let primary = parts
        .iter()
        .copied()
        .find(|p| p.starts_with("--"))
        .unwrap_or(parts[0]);
    let aliases: Vec<String> = parts
        .iter()
        .copied()
        .filter(|p| *p != primary)
        .map(str::to_string)
        .collect();

    let mut children = Vec::new();
    let mut seen_vals = HashSet::new();
    for candidate_key in parts.iter().copied() {
        let stripped = candidate_key.trim_start_matches('-');
        if let Some(vals) = flag_completions
            .get(stripped)
            .or_else(|| flag_completions.get(candidate_key))
        {
            for raw in vals {
                if let Some(child) = carapace_completion_item_to_entry(raw, EntryKind::Enum) {
                    let dedup_key = if child.is_placeholder() {
                        format!("ph:{}", child.label.to_ascii_lowercase())
                    } else {
                        format!("tok:{}", child.text.to_ascii_lowercase())
                    };
                    if seen_vals.insert(dedup_key) {
                        children.push(child);
                    }
                }
            }
        }
    }

    let kind = if takes_arg || !children.is_empty() {
        EntryKind::Param
    } else {
        EntryKind::Flag
    };

    let label = if aliases.is_empty() {
        primary.to_string()
    } else {
        format!("{primary} ({})", aliases.join(", "))
    };

    let mut entry = Entry::new(primary, label, kind, desc).with_aliases(aliases);
    entry.children = children;
    Some(entry)
}

fn carapace_completion_item_to_entry(raw: &str, default_kind: EntryKind) -> Option<Entry> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }

    if trimmed == "$files"
        || trimmed.starts_with("$files(")
        || trimmed == "$directories"
        || trimmed.starts_with("$directories(")
    {
        let (macro_label, desc) = if trimmed.starts_with("$directories") {
            ("$directories", "Dynamic macro: current directory folders")
        } else {
            ("$files", "Dynamic macro: current directory files & folders")
        };
        return Some(Entry::placeholder(macro_label, EntryKind::Path, desc));
    }
    if trimmed.starts_with('$') {
        return None;
    }

    let (val, item_desc) = match trimmed.split_once('\t') {
        Some((v, d)) => (v.trim(), d.trim()),
        None => (trimmed, ""),
    };
    if val.is_empty() {
        return None;
    }

    Some(Entry::new(val, val, default_kind, item_desc))
}

// ============================================================================
// Simplified Fig Spec JSON Schema & Conversion
// ============================================================================

#[derive(Debug, Default, Deserialize)]
struct FigSpec {
    #[serde(default, deserialize_with = "deserialize_null_default")]
    name: String,
    #[serde(default, deserialize_with = "deserialize_null_default")]
    description: String,
    #[serde(default, deserialize_with = "deserialize_null_default")]
    subcommands: Vec<FigSpec>,
    #[serde(default, deserialize_with = "deserialize_null_default")]
    options: Vec<FigOption>,
    #[serde(default)]
    args: Option<FigArgsField>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
enum OneOrManyStrings {
    One(String),
    Many(Vec<String>),
}

impl OneOrManyStrings {
    fn as_slice(&self) -> &[String] {
        match self {
            OneOrManyStrings::One(s) => std::slice::from_ref(s),
            OneOrManyStrings::Many(v) => v.as_slice(),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
enum FigArgsField {
    Single(FigArg),
    Multiple(Vec<FigArg>),
}

impl FigArgsField {
    fn as_slice(&self) -> &[FigArg] {
        match self {
            FigArgsField::Single(a) => std::slice::from_ref(a),
            FigArgsField::Multiple(v) => v.as_slice(),
        }
    }
}

#[derive(Debug, Default, Deserialize)]
struct FigOption {
    name: Option<OneOrManyStrings>,
    #[serde(default, deserialize_with = "deserialize_null_default")]
    description: String,
    #[serde(default)]
    args: Option<FigArgsField>,
}

#[derive(Debug, Clone, Default, Deserialize)]
struct FigArg {
    #[allow(dead_code)]
    #[serde(default, deserialize_with = "deserialize_null_default")]
    name: String,
    #[serde(default, deserialize_with = "deserialize_null_default")]
    suggestions: Vec<FigSuggestion>,
    #[serde(default)]
    template: Option<OneOrManyStrings>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
enum FigSuggestion {
    Plain(String),
    Detailed {
        name: String,
        #[serde(default, deserialize_with = "deserialize_null_default")]
        description: String,
    },
}

fn fig_spec_to_entry(spec: &FigSpec, is_root: bool) -> Entry {
    let kind = if is_root {
        EntryKind::Cli
    } else {
        EntryKind::Subcmd
    };
    let spec_name = spec.name.trim();
    let mut entry = Entry::new(spec_name, spec_name, kind, &spec.description);
    let mut children = Vec::new();

    for sub in &spec.subcommands {
        if !sub.name.trim().is_empty() {
            children.push(fig_spec_to_entry(sub, false));
        }
    }

    if let Some(ref args_field) = spec.args {
        children.extend(fig_args_to_entries(args_field.as_slice()));
    }

    for opt in &spec.options {
        let Some(ref names_raw) = opt.name else {
            continue;
        };
        let names: Vec<String> = names_raw
            .as_slice()
            .iter()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect();
        if names.is_empty() {
            continue;
        }
        let primary = names
            .iter()
            .find(|n| n.starts_with("--"))
            .unwrap_or(&names[0])
            .clone();
        let aliases: Vec<String> = names.into_iter().filter(|n| *n != primary).collect();
        let arg_children = opt
            .args
            .as_ref()
            .map(|a| fig_args_to_entries(a.as_slice()))
            .unwrap_or_default();

        let takes_arg = opt.args.is_some() || !arg_children.is_empty();
        let opt_kind = if takes_arg {
            EntryKind::Param
        } else {
            EntryKind::Flag
        };
        let label = if aliases.is_empty() {
            primary.clone()
        } else {
            format!("{primary} ({})", aliases.join(", "))
        };

        let mut opt_entry =
            Entry::new(&primary, label, opt_kind, &opt.description).with_aliases(aliases);
        opt_entry.children = arg_children;
        children.push(opt_entry);
    }

    entry.children = children;
    entry
}

fn fig_args_to_entries(args: &[FigArg]) -> Vec<Entry> {
    let mut out = Vec::new();
    for arg in args {
        if let Some(ref tpl) = arg.template {
            for t in tpl.as_slice() {
                if t == "folders" {
                    out.push(Entry::placeholder(
                        "$directories",
                        EntryKind::Path,
                        "Dynamic macro: current directory folders",
                    ));
                } else if t == "filepaths" {
                    out.push(Entry::placeholder(
                        "$files",
                        EntryKind::Path,
                        "Dynamic macro: current directory files & folders",
                    ));
                }
            }
        }
        for sug in &arg.suggestions {
            match sug {
                FigSuggestion::Plain(val) if !val.trim().is_empty() => {
                    let v = val.trim();
                    out.push(Entry::new(v, v, EntryKind::Enum, ""));
                }
                FigSuggestion::Detailed { name, description } if !name.trim().is_empty() => {
                    let n = name.trim();
                    out.push(Entry::new(n, n, EntryKind::Enum, description));
                }
                _ => {}
            }
        }
    }
    out
}

// ============================================================================
// Lightweight Zero-Dependency YAML-Subset -> JSON Value Parser
// ============================================================================

#[derive(Debug, Clone)]
struct YamlLine {
    indent: usize,
    text: String,
}

fn strip_yaml_comment(line: &str) -> &str {
    let mut in_single = false;
    let mut in_double = false;
    let mut escaped = false;
    let bytes = line.as_bytes();

    for (idx, ch) in line.char_indices() {
        if escaped {
            escaped = false;
            continue;
        }
        match ch {
            '\\' if in_double => escaped = true,
            '"' if !in_single => in_double = !in_double,
            '\'' if !in_double => in_single = !in_single,
            '#' if !in_single
                && !in_double
                && (idx == 0 || bytes[idx - 1].is_ascii_whitespace()) =>
            {
                return &line[..idx];
            }
            _ => {}
        }
    }
    line
}

fn tokenize_yaml(input: &str) -> anyhow::Result<Vec<YamlLine>> {
    let mut out = Vec::new();
    for raw in input.lines() {
        let indent = raw.bytes().take_while(|b| *b == b' ').count();
        let rest = &raw[indent..];
        if rest.starts_with('\t') {
            anyhow::bail!("Tab indentation is not supported in YAML specs");
        }
        let cleaned = strip_yaml_comment(rest).trim();
        if cleaned.is_empty() || cleaned == "---" || cleaned == "..." {
            continue;
        }
        out.push(YamlLine {
            indent,
            text: cleaned.to_string(),
        });
    }
    Ok(out)
}

fn yaml_to_json(input: &str) -> anyhow::Result<serde_json::Value> {
    let lines = tokenize_yaml(input)?;
    if lines.is_empty() {
        return Ok(serde_json::Value::Object(serde_json::Map::new()));
    }
    let (val, next_idx) = parse_yaml_block(&lines, 0, lines[0].indent)?;
    if next_idx < lines.len() {
        anyhow::bail!(
            "Unexpected YAML syntax or indentation near: {}",
            lines[next_idx].text
        );
    }
    Ok(val)
}

fn is_seq_item(text: &str) -> bool {
    text == "-" || text.starts_with("- ")
}

fn is_block_scalar(val_part: &str) -> Option<bool> {
    match val_part {
        "|" | "|-" | "|+" => Some(true),
        ">" | ">-" | ">+" => Some(false),
        _ => None,
    }
}

fn consume_yaml_value(
    lines: &[YamlLine],
    idx: &mut usize,
    parent_indent: usize,
    val_part: &str,
) -> anyhow::Result<serde_json::Value> {
    if let Some(literal) = is_block_scalar(val_part) {
        if *idx < lines.len() && lines[*idx].indent > parent_indent {
            let block_indent = lines[*idx].indent;
            let mut parts = Vec::new();
            while *idx < lines.len() && lines[*idx].indent >= block_indent {
                parts.push(lines[*idx].text.as_str());
                *idx += 1;
            }
            let sep = if literal { "\n" } else { " " };
            return Ok(serde_json::Value::String(parts.join(sep)));
        }
        return Ok(serde_json::Value::String(String::new()));
    }

    if val_part.is_empty() {
        if *idx < lines.len() && lines[*idx].indent > parent_indent {
            let child_indent = lines[*idx].indent;
            let (nested, next_idx) = parse_yaml_block(lines, *idx, child_indent)?;
            *idx = next_idx;
            Ok(nested)
        } else if *idx < lines.len()
            && lines[*idx].indent == parent_indent
            && is_seq_item(&lines[*idx].text)
        {
            let (nested, next_idx) = parse_yaml_seq(lines, *idx, lines[*idx].indent)?;
            *idx = next_idx;
            Ok(nested)
        } else {
            Ok(serde_json::Value::Null)
        }
    } else {
        parse_yaml_inline(val_part)
    }
}

fn find_mapping_colon(text: &str) -> Option<usize> {
    let mut in_single = false;
    let mut in_double = false;
    let mut escaped = false;
    let bytes = text.as_bytes();

    for (idx, ch) in text.char_indices() {
        if escaped {
            escaped = false;
            continue;
        }
        match ch {
            '\\' if in_double => escaped = true,
            '"' if !in_single => in_double = !in_double,
            '\'' if !in_double => in_single = !in_single,
            ':' if !in_single && !in_double => {
                let next = bytes.get(idx + 1).copied();
                if next.is_none() || next.is_some_and(|b| b.is_ascii_whitespace()) {
                    return Some(idx);
                }
            }
            _ => {}
        }
    }
    None
}

fn parse_yaml_block(
    lines: &[YamlLine],
    start: usize,
    base_indent: usize,
) -> anyhow::Result<(serde_json::Value, usize)> {
    if start >= lines.len() {
        return Ok((serde_json::Value::Null, start));
    }
    if is_seq_item(&lines[start].text) {
        parse_yaml_seq(lines, start, base_indent)
    } else {
        parse_yaml_map(lines, start, base_indent)
    }
}

fn parse_yaml_map(
    lines: &[YamlLine],
    mut idx: usize,
    base_indent: usize,
) -> anyhow::Result<(serde_json::Value, usize)> {
    let mut map = serde_json::Map::new();
    while idx < lines.len() {
        let line = &lines[idx];
        if line.indent < base_indent || is_seq_item(&line.text) {
            break;
        }
        if line.indent > base_indent {
            anyhow::bail!("Unexpected YAML indentation at: {}", line.text);
        }
        let Some(colon) = find_mapping_colon(&line.text) else {
            anyhow::bail!(
                "Expected YAML 'key: value' mapping entry, found: {}",
                line.text
            );
        };
        let key = unquote_yaml_scalar(line.text[..colon].trim());
        let val_part = line.text[colon + 1..].trim();
        let line_indent = line.indent;
        idx += 1;

        let val = consume_yaml_value(lines, &mut idx, line_indent, val_part)?;
        map.insert(key, val);
    }
    Ok((serde_json::Value::Object(map), idx))
}

fn parse_yaml_seq(
    lines: &[YamlLine],
    mut idx: usize,
    base_indent: usize,
) -> anyhow::Result<(serde_json::Value, usize)> {
    let mut seq = Vec::new();
    while idx < lines.len() {
        let line = &lines[idx];
        if line.indent != base_indent || !is_seq_item(&line.text) {
            break;
        }
        let after_dash_raw = &line.text[1..];
        let spaces = after_dash_raw.bytes().take_while(|b| *b == b' ').count();
        let after_dash = after_dash_raw[spaces..].trim();
        let item_indent = base_indent + 1 + spaces;
        idx += 1;

        if after_dash.is_empty() {
            if idx < lines.len() && lines[idx].indent > base_indent {
                let child_indent = lines[idx].indent;
                let (nested, next_idx) = parse_yaml_block(lines, idx, child_indent)?;
                idx = next_idx;
                seq.push(nested);
            } else {
                seq.push(serde_json::Value::Null);
            }
        } else if let Some(colon) = find_mapping_colon(after_dash) {
            let first_key = unquote_yaml_scalar(after_dash[..colon].trim());
            let first_val_part = after_dash[colon + 1..].trim();
            let first_val = consume_yaml_value(lines, &mut idx, item_indent, first_val_part)?;

            let mut item_map = serde_json::Map::new();
            item_map.insert(first_key, first_val);
            if idx < lines.len()
                && lines[idx].indent > base_indent
                && !is_seq_item(&lines[idx].text)
            {
                let (rest_val, next_idx) = parse_yaml_map(lines, idx, lines[idx].indent)?;
                idx = next_idx;
                if let serde_json::Value::Object(rest_map) = rest_val {
                    item_map.extend(rest_map);
                }
            }
            seq.push(serde_json::Value::Object(item_map));
        } else {
            seq.push(parse_yaml_inline(after_dash)?);
        }
    }
    Ok((serde_json::Value::Array(seq), idx))
}

fn parse_yaml_inline(raw: &str) -> anyhow::Result<serde_json::Value> {
    let trimmed = raw.trim();
    if trimmed.starts_with('[') && trimmed.ends_with(']') {
        let inner = &trimmed[1..trimmed.len() - 1];
        let mut arr = Vec::new();
        for item in split_flow_items(inner) {
            let t = item.trim();
            if !t.is_empty() {
                arr.push(parse_yaml_inline(t)?);
            }
        }
        return Ok(serde_json::Value::Array(arr));
    }

    if trimmed.starts_with('{') && trimmed.ends_with('}') {
        let inner = &trimmed[1..trimmed.len() - 1];
        let mut map = serde_json::Map::new();
        for pair in split_flow_items(inner) {
            let t = pair.trim();
            if t.is_empty() {
                continue;
            }
            if let Some(colon) = find_mapping_colon(t) {
                let k = unquote_yaml_scalar(t[..colon].trim());
                let v = parse_yaml_inline(t[colon + 1..].trim())?;
                map.insert(k, v);
            }
        }
        return Ok(serde_json::Value::Object(map));
    }

    if !trimmed.starts_with('"') && !trimmed.starts_with('\'') {
        match trimmed {
            "true" => return Ok(serde_json::Value::Bool(true)),
            "false" => return Ok(serde_json::Value::Bool(false)),
            "null" | "~" => return Ok(serde_json::Value::Null),
            _ => {}
        }
    }

    Ok(serde_json::Value::String(unquote_yaml_scalar(trimmed)))
}

fn split_flow_items(inner: &str) -> Vec<String> {
    let mut items = Vec::new();
    let mut cur = String::new();
    let mut in_single = false;
    let mut in_double = false;
    let mut escaped = false;
    let mut depth = 0usize;

    for ch in inner.chars() {
        if escaped {
            cur.push(ch);
            escaped = false;
            continue;
        }
        match ch {
            '\\' if in_double => {
                cur.push(ch);
                escaped = true;
            }
            '"' if !in_single => {
                in_double = !in_double;
                cur.push(ch);
            }
            '\'' if !in_double => {
                in_single = !in_single;
                cur.push(ch);
            }
            '[' | '{' if !in_single && !in_double => {
                depth += 1;
                cur.push(ch);
            }
            ']' | '}' if !in_single && !in_double => {
                depth = depth.saturating_sub(1);
                cur.push(ch);
            }
            ',' if !in_single && !in_double && depth == 0 => {
                items.push(std::mem::take(&mut cur));
            }
            _ => cur.push(ch),
        }
    }
    if !cur.trim().is_empty() {
        items.push(cur);
    }
    items
}

fn unquote_yaml_scalar(raw: &str) -> String {
    if raw.len() >= 2 && raw.starts_with('"') && raw.ends_with('"') {
        let inner = &raw[1..raw.len() - 1];
        let mut out = String::with_capacity(inner.len());
        let mut chars = inner.chars();
        while let Some(ch) = chars.next() {
            if ch == '\\' {
                match chars.next() {
                    Some('t') => out.push('\t'),
                    Some('n') => out.push('\n'),
                    Some('r') => out.push('\r'),
                    Some('"') => out.push('"'),
                    Some('\\') => out.push('\\'),
                    Some(other) => {
                        out.push('\\');
                        out.push(other);
                    }
                    None => out.push('\\'),
                }
            } else {
                out.push(ch);
            }
        }
        out
    } else if raw.len() >= 2 && raw.starts_with('\'') && raw.ends_with('\'') {
        raw[1..raw.len() - 1].replace("''", "'")
    } else {
        raw.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_carapace_yaml_spec_with_flags_macros_and_nested_commands() {
        let yaml = r#"
# yaml-language-server: $schema=https://carapace.sh/schemas/command.json
name: mytool
description: Custom workspace tool
persistentflags:
  -h, --help: Show help message
flags:
  -v, --verbose: Enable verbose output
  -c, --config=: Config file path
  --mode?: Optional output mode
completion:
  flag:
    config: ["$files"]
    mode:
      - "fast\tMinimal output"
      - "full"
commands:
  - name: deploy
    description: Deploy service
    aliases: ["d"]
    flags:
      -e, --env=: Target environment
    completion:
      flag:
        env: ["staging", "prod"]
      positional:
        - ["$directories"]
"#;

        let entry = parse_spec_str("mytool.yaml", yaml).expect("parse carapace YAML");
        assert_eq!(entry.text, "mytool");
        assert_eq!(entry.kind, EntryKind::Cli);
        assert_eq!(entry.desc, "Custom workspace tool");

        // Subcommand `deploy` inherits persistent `--help` and includes env param + $directories macro
        let deploy = entry
            .children
            .iter()
            .find(|c| c.text == "deploy")
            .expect("deploy subcmd");
        assert_eq!(deploy.kind, EntryKind::Subcmd);
        assert_eq!(deploy.label, "deploy (d)");
        assert_eq!(deploy.aliases, vec!["d"]);
        assert!(deploy.children.iter().any(|c| c.text == "--help"));

        let env_param = deploy
            .children
            .iter()
            .find(|c| c.text == "--env")
            .expect("--env param");
        assert_eq!(env_param.kind, EntryKind::Param);
        assert_eq!(env_param.aliases, vec!["-e"]);
        let env_vals: Vec<&str> = env_param.children.iter().map(|c| c.text.as_str()).collect();
        assert_eq!(env_vals, vec!["staging", "prod"]);

        let dir_macro = deploy
            .children
            .iter()
            .find(|c| c.label == "$directories")
            .expect("$directories macro placeholder");
        assert!(dir_macro.is_placeholder());
        assert_eq!(dir_macro.kind, EntryKind::Path);

        // Root flags: --verbose is Flag, --config is Param with $files macro, --mode has tab-parsed desc
        let verbose = entry
            .children
            .iter()
            .find(|c| c.text == "--verbose")
            .expect("--verbose");
        assert_eq!(verbose.kind, EntryKind::Flag);

        let config = entry
            .children
            .iter()
            .find(|c| c.text == "--config")
            .expect("--config");
        assert_eq!(config.kind, EntryKind::Param);
        assert!(config
            .children
            .iter()
            .any(|c| c.label == "$files" && c.is_placeholder()));

        let mode = entry
            .children
            .iter()
            .find(|c| c.text == "--mode")
            .expect("--mode");
        assert_eq!(mode.kind, EntryKind::Param);
        assert_eq!(mode.children[0].text, "fast");
        assert_eq!(mode.children[0].desc, "Minimal output");
        assert_eq!(mode.children[1].text, "full");
    }

    #[test]
    fn test_parse_carapace_json_and_fig_json_specs() {
        let carapace_json = r#"{
  "name": "cjson",
  "description": "JSON carapace tool",
  "flags": {
    "--format=": "Output format"
  },
  "completion": {
    "flag": {
      "format": ["json", "table"]
    }
  }
}"#;
        let c_entry = parse_spec_str("cjson.json", carapace_json).expect("carapace json");
        assert_eq!(c_entry.text, "cjson");
        assert_eq!(c_entry.children[0].text, "--format");
        assert_eq!(c_entry.children[0].kind, EntryKind::Param);
        assert_eq!(c_entry.children[0].children.len(), 2);

        let fig_json = r#"{
  "name": "figtool",
  "description": "Fig-style spec",
  "subcommands": [
    {
      "name": "run",
      "description": "Run target",
      "args": {
        "name": "target",
        "template": "folders",
        "suggestions": [
          { "name": "api", "description": "API crate" }
        ]
      }
    }
  ],
  "options": [
    {
      "name": ["-r", "--release"],
      "description": "Release build"
    },
    {
      "name": ["--target"],
      "description": "Build target",
      "args": {
        "name": "triple",
        "suggestions": ["x86_64-pc-windows-msvc", "x86_64-unknown-linux-gnu"]
      }
    }
  ]
}"#;
        let fig_entry = parse_spec_str("figtool.json", fig_json).expect("fig json");
        assert_eq!(fig_entry.text, "figtool");
        let run = fig_entry
            .children
            .iter()
            .find(|c| c.text == "run")
            .expect("run subcmd");
        assert_eq!(run.kind, EntryKind::Subcmd);
        assert!(run.children.iter().any(|c| c.label == "$directories"));
        assert!(run
            .children
            .iter()
            .any(|c| c.text == "api" && c.desc == "API crate"));

        let release = fig_entry
            .children
            .iter()
            .find(|c| c.text == "--release")
            .expect("--release");
        assert_eq!(release.kind, EntryKind::Flag);
        assert_eq!(release.aliases, vec!["-r"]);

        let target = fig_entry
            .children
            .iter()
            .find(|c| c.text == "--target")
            .expect("--target");
        assert_eq!(target.kind, EntryKind::Param);
        assert_eq!(target.children.len(), 2);
    }

    #[test]
    fn test_carapace_yaml_empty_sections_block_scalars_local_override_and_nested_seq() {
        let yaml = r#"
name: mytool
description: |
  Line one: with colon
  Line two
aliases:
persistentflags:
  --mode: Persistent flag without value
flags:
  -m, --mode=: Local parameter overrides persistent flag
commands:
  - aliases:
    - s
    name: status
    completion:
      positional:
        - ["$files([\"*.rs\"])", "$files", "explicit-arg"]
      positionalany:
        - "explicit-arg"
"#;
        let entry = parse_spec_str("mytool.yaml", yaml).expect("parse extended carapace YAML");
        assert_eq!(entry.desc, "Line one: with colon\nLine two");

        let mode = entry
            .children
            .iter()
            .find(|c| c.text == "--mode")
            .expect("--mode parameter");
        assert_eq!(mode.kind, EntryKind::Param);
        assert_eq!(mode.aliases, vec!["-m"]);
        assert_eq!(mode.desc, "Local parameter overrides persistent flag");

        let status = entry
            .children
            .iter()
            .find(|c| c.text == "status")
            .expect("status subcmd");
        assert_eq!(status.aliases, vec!["s"]);
        assert_eq!(
            status
                .children
                .iter()
                .filter(|c| c.label == "$files")
                .count(),
            1
        );
        assert_eq!(
            status
                .children
                .iter()
                .filter(|c| c.text == "explicit-arg")
                .count(),
            1
        );
    }

    #[test]
    fn test_parse_spec_rejects_malformed_yaml_indentation_and_blank_entries() {
        let bad_yaml = "name: tool\n- unexpected_seq\n";
        assert!(parse_spec_str("bad.yaml", bad_yaml).is_err());

        let bad_indent = "name: tool\n  stray: value\n";
        assert!(parse_spec_str("indent.yaml", bad_indent).is_err());

        let blank_name = r#"{"name": "   ", "description": "blank"}"#;
        assert!(parse_spec_str("blank.json", blank_name).is_err());

        let fig_with_empty_opt = r#"{
  "name": "fig",
  "options": [
    { "name": ["", "  "], "description": "ignored" },
    { "name": "-v", "description": "verbose" }
  ],
  "subcommands": null
}"#;
        let parsed = parse_spec_str("fig.json", fig_with_empty_opt).expect("fig with null/empty");
        let texts: Vec<&str> = parsed.children.iter().map(|c| c.text.as_str()).collect();
        assert_eq!(texts, vec!["-v"]);
    }
}
