use super::catalog::{
    cache_dir, ends_with_ignore_ascii_case, is_safe_command_token, sanitize_multiline_desc,
    sanitize_single_line, MAX_SPEC_BYTES,
};
use crate::model::{Entry, EntryKind};
use serde::Deserialize;
use std::borrow::Cow;
use std::collections::{BTreeMap, HashSet};
use std::path::PathBuf;

const MAX_YAML_DEPTH: usize = 48;
const MAX_YAML_LINES: usize = 16_384;
const MAX_SPEC_TREE_DEPTH: usize = 24;
const MAX_SPEC_CHILDREN: usize = 1_024;

/// Returns the local directory searched for declarative JSON/YAML CLI specifications:
/// `%LOCALAPPDATA%\zpane\specs` on Windows, `$XDG_CACHE_HOME/zpane/specs` or `~/.cache/zpane/specs` on Unix.
pub(super) fn specs_dir() -> PathBuf {
    cache_dir().join("specs")
}

/// Returns true if a filename is a safe basename with a supported declarative spec extension (`.json`, `.yaml`, `.yml`).
pub(super) fn is_spec_filename(name: &str) -> bool {
    if name.is_empty()
        || name.len() > 255
        || name.starts_with('.')
        || name.contains(['/', '\\', ':', '\0'])
        || name.contains("..")
        || name.chars().any(char::is_control)
    {
        return false;
    }
    ends_with_ignore_ascii_case(name, ".json")
        || ends_with_ignore_ascii_case(name, ".yaml")
        || ends_with_ignore_ascii_case(name, ".yml")
}

fn is_safe_spec_command_name(name: &str) -> bool {
    is_safe_command_token(name) && !name.contains(['/', '\\', ':', '\0']) && !name.contains("..")
}

fn clean_spec_token_cow(raw: &str) -> Option<Cow<'_, str>> {
    let cleaned = sanitize_single_line(raw.trim());
    let valid = |s: &str| {
        !s.is_empty() && s.len() <= 256 && !s.chars().any(|c| c.is_control() || c.is_whitespace())
    };
    match cleaned {
        Cow::Borrowed(s) => {
            let trimmed = s.trim();
            valid(trimmed).then_some(Cow::Borrowed(trimmed))
        }
        Cow::Owned(s) => {
            let trimmed = s.trim();
            valid(trimmed).then(|| Cow::Owned(trimmed.to_string()))
        }
    }
}

fn clean_spec_token(raw: &str) -> Option<String> {
    clean_spec_token_cow(raw).map(Cow::into_owned)
}

#[inline]
fn to_ascii_lower_cow(s: &str) -> Cow<'_, str> {
    if s.bytes().any(|b| b.is_ascii_uppercase()) {
        Cow::Owned(s.to_ascii_lowercase())
    } else {
        Cow::Borrowed(s)
    }
}

#[inline]
fn insert_ascii_lower_unique(set: &mut HashSet<String>, value: &str) -> bool {
    if value.bytes().any(|b| b.is_ascii_uppercase()) {
        let lower = value.to_ascii_lowercase();
        if set.contains(lower.as_str()) {
            return false;
        }
        set.insert(lower)
    } else {
        if set.contains(value) {
            return false;
        }
        set.insert(value.to_owned())
    }
}

fn build_option_entry_from_tokens(
    mut tokens: Vec<Cow<'_, str>>,
    primary_idx: usize,
    kind: EntryKind,
    desc: &str,
) -> Entry {
    let primary_str = tokens.get(primary_idx).map(Cow::as_ref).unwrap_or_default();
    let aliases: Vec<String> = tokens
        .iter()
        .filter(|t| t.as_ref() != primary_str)
        .map(|t| t.as_ref().to_string())
        .collect();
    let primary = if primary_idx < tokens.len() {
        tokens.swap_remove(primary_idx).into_owned()
    } else {
        String::new()
    };
    let label = if aliases.is_empty() {
        primary.clone()
    } else {
        format!("{primary} ({})", aliases.join(", "))
    };
    let clean_desc = sanitize_multiline_desc(desc.trim());
    Entry::new(primary, label, kind, clean_desc.as_ref()).with_aliases(aliases)
}

fn clean_spec_aliases(raw_aliases: &[String]) -> Vec<String> {
    raw_aliases
        .iter()
        .take(32)
        .filter_map(|a| clean_spec_token(a))
        .filter(|a| is_safe_spec_command_name(a))
        .collect()
}

/// Parses a declarative specification (Carapace YAML/JSON or simplified Fig JSON) into a root [`Entry`].
pub(super) fn parse_spec_str(file_name: &str, content: &str) -> anyhow::Result<Entry> {
    if !is_spec_filename(file_name) {
        anyhow::bail!("Unsafe or unsupported spec file name: {file_name}");
    }
    if content.len() > MAX_SPEC_BYTES {
        anyhow::bail!(
            "Spec file exceeds maximum allowed size ({MAX_SPEC_BYTES} bytes): {file_name}"
        );
    }

    let trimmed = content.trim_start_matches('\u{feff}').trim();
    if trimmed.is_empty() {
        anyhow::bail!("Empty spec file: {file_name}");
    }

    let value: serde_json::Value =
        if ends_with_ignore_ascii_case(file_name, ".json") || trimmed.starts_with('{') {
            serde_json::from_str(trimmed)?
        } else {
            yaml_to_json(trimmed)?
        };

    spec_value_to_entry(value)
}

fn spec_value_to_entry(value: serde_json::Value) -> anyhow::Result<Entry> {
    let Some(obj) = value.as_object() else {
        anyhow::bail!("Spec root must be an object or YAML mapping");
    };

    if obj.contains_key("flags")
        || obj.contains_key("persistentflags")
        || obj.contains_key("commands")
        || obj.contains_key("completion")
    {
        let spec: CarapaceSpec = serde_json::from_value(value)?;
        if !is_safe_spec_command_name(spec.name.trim()) {
            anyhow::bail!("carapace-spec missing or unsafe 'name'");
        }
        Ok(carapace_spec_to_entry(&spec, &BTreeMap::new(), true, 0))
    } else if obj.contains_key("subcommands")
        || obj.contains_key("options")
        || obj.contains_key("args")
    {
        let spec: FigSpec = serde_json::from_value(value)?;
        if !is_safe_spec_command_name(spec.name.trim()) {
            anyhow::bail!("Fig spec missing or unsafe 'name'");
        }
        Ok(fig_spec_to_entry(&spec, true, 0))
    } else if let Some(name) = obj.get("name").and_then(|v| v.as_str()) {
        let trimmed_name = name.trim();
        if !is_safe_spec_command_name(trimmed_name) {
            anyhow::bail!("Declarative command spec missing or unsafe 'name'");
        }
        let raw_desc = obj
            .get("description")
            .and_then(|v| v.as_str())
            .unwrap_or("Declarative command spec");
        let desc = sanitize_multiline_desc(raw_desc.trim());
        Ok(Entry::new(
            trimmed_name,
            trimmed_name,
            EntryKind::Cli,
            desc.as_ref(),
        ))
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
    depth: usize,
) -> Entry {
    let kind = if is_root {
        EntryKind::Cli
    } else {
        EntryKind::Subcmd
    };

    let effective_persistent: Cow<'_, BTreeMap<String, String>> = if spec.persistentflags.is_empty()
    {
        Cow::Borrowed(inherited_persistent)
    } else {
        let mut merged = inherited_persistent.clone();
        for (k, v) in &spec.persistentflags {
            merged.insert(k.clone(), v.clone());
        }
        Cow::Owned(merged)
    };

    let spec_name = spec.name.trim();
    let aliases = clean_spec_aliases(&spec.aliases);
    let label = if !is_root && !aliases.is_empty() {
        format!("{spec_name} ({})", aliases.join(", "))
    } else {
        spec_name.to_string()
    };

    let clean_desc = sanitize_multiline_desc(spec.description.trim());
    let mut entry = Entry::new(spec_name, label, kind, clean_desc.as_ref()).with_aliases(aliases);

    let sub_cap = spec.commands.len().min(MAX_SPEC_CHILDREN);
    let total_flags = spec.flags.len().saturating_add(effective_persistent.len());
    let mut children = Vec::with_capacity(
        sub_cap
            .saturating_add(total_flags)
            .min(MAX_SPEC_CHILDREN * 2),
    );

    // 1. Subcommands first (matches CLI Explorer Pane 2 ergonomics)
    if depth < MAX_SPEC_TREE_DEPTH && !spec.commands.is_empty() {
        let mut seen_sub: HashSet<Cow<'_, str>> = HashSet::with_capacity(sub_cap);
        for sub in spec.commands.iter().take(MAX_SPEC_CHILDREN) {
            let sub_name = sub.name.trim();
            if is_safe_spec_command_name(sub_name) && seen_sub.insert(to_ascii_lower_cow(sub_name))
            {
                children.push(carapace_spec_to_entry(
                    sub,
                    effective_persistent.as_ref(),
                    false,
                    depth + 1,
                ));
            }
        }
    }

    // 2. Positional completion hints (for subcommands or commands with positional args)
    let first_pos = spec
        .completion
        .positional
        .first()
        .map(Vec::as_slice)
        .unwrap_or(&[]);
    if !first_pos.is_empty() || !spec.completion.positionalany.is_empty() {
        let mut seen_pos_placeholders = HashSet::new();
        let mut seen_pos_values = HashSet::new();
        for raw in first_pos
            .iter()
            .chain(spec.completion.positionalany.iter())
            .take(MAX_SPEC_CHILDREN)
        {
            if let Some(val_entry) = carapace_completion_item_to_entry(raw, EntryKind::Arg) {
                let is_new = if val_entry.is_placeholder() {
                    insert_ascii_lower_unique(&mut seen_pos_placeholders, &val_entry.label)
                } else {
                    insert_ascii_lower_unique(&mut seen_pos_values, &val_entry.text)
                };
                if is_new {
                    children.push(val_entry);
                }
            }
        }
    }

    // 3. Local flags first, then inherited/persistent flags so local overrides win even when
    //    suffix markers (=, ?, *) or short-flag prefixes differ.
    if total_flags > 0 {
        let mut seen_flags: HashSet<Cow<'_, str>> =
            HashSet::with_capacity(total_flags.min(MAX_SPEC_CHILDREN));
        for (flag_spec, desc) in spec
            .flags
            .iter()
            .chain(effective_persistent.iter())
            .take(MAX_SPEC_CHILDREN)
        {
            if let Some(flag_entry) =
                parse_carapace_flag_if_new(flag_spec, desc, &spec.completion.flag, &mut seen_flags)
            {
                children.push(flag_entry);
            }
        }
    }

    entry.children = children;
    entry
}

fn parse_carapace_flag_if_new<'a>(
    raw_spec: &'a str,
    desc: &str,
    flag_completions: &BTreeMap<String, Vec<String>>,
    seen_flags: &mut HashSet<Cow<'a, str>>,
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

    let parts: Vec<Cow<'a, str>> = trimmed
        .split(',')
        .filter_map(clean_spec_token_cow)
        .collect();
    if parts.is_empty() {
        return None;
    }

    // Prefer long option (--foo) as primary token; keep short/other variants as Parameter Aliases
    let primary_idx = parts.iter().position(|p| p.starts_with("--")).unwrap_or(0);
    let primary_ref = parts[primary_idx].as_ref();
    let dedup_key: Cow<'a, str> = if primary_ref.bytes().any(|b| b.is_ascii_uppercase()) {
        let lower = primary_ref.to_ascii_lowercase();
        if seen_flags.contains(lower.as_str()) {
            return None;
        }
        Cow::Owned(lower)
    } else {
        if seen_flags.contains(primary_ref) {
            return None;
        }
        match &parts[primary_idx] {
            Cow::Borrowed(s) => Cow::Borrowed(*s),
            Cow::Owned(s) => Cow::Owned(s.clone()),
        }
    };
    seen_flags.insert(dedup_key);

    let mut children = Vec::new();
    if !flag_completions.is_empty() {
        let mut seen_placeholders = HashSet::new();
        let mut seen_vals = HashSet::new();
        for candidate_key in &parts {
            let stripped = candidate_key.trim_start_matches('-');
            if let Some(vals) = flag_completions
                .get(stripped)
                .or_else(|| flag_completions.get(candidate_key.as_ref()))
            {
                for raw in vals.iter().take(MAX_SPEC_CHILDREN) {
                    if let Some(child) = carapace_completion_item_to_entry(raw, EntryKind::Enum) {
                        let is_new = if child.is_placeholder() {
                            insert_ascii_lower_unique(&mut seen_placeholders, &child.label)
                        } else {
                            insert_ascii_lower_unique(&mut seen_vals, &child.text)
                        };
                        if is_new {
                            children.push(child);
                        }
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

    let mut entry = build_option_entry_from_tokens(parts, primary_idx, kind, desc);
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

    let (raw_val, raw_item_desc) = match trimmed.split_once('\t') {
        Some((v, d)) => (v.trim(), d.trim()),
        None => (trimmed, ""),
    };
    let val_clean = sanitize_single_line(raw_val);
    let val = val_clean.trim();
    if val.is_empty() || val.len() > 256 {
        return None;
    }
    let desc_clean = sanitize_multiline_desc(raw_item_desc);

    Some(Entry::new(val, val, default_kind, desc_clean.as_ref()))
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

fn fig_spec_to_entry(spec: &FigSpec, is_root: bool, depth: usize) -> Entry {
    let kind = if is_root {
        EntryKind::Cli
    } else {
        EntryKind::Subcmd
    };
    let spec_name = spec.name.trim();
    let clean_desc = sanitize_multiline_desc(spec.description.trim());
    let mut entry = Entry::new(spec_name, spec_name, kind, clean_desc.as_ref());
    let mut children = Vec::with_capacity(
        spec.subcommands
            .len()
            .saturating_add(spec.options.len())
            .min(MAX_SPEC_CHILDREN),
    );

    if depth < MAX_SPEC_TREE_DEPTH {
        for sub in spec.subcommands.iter().take(MAX_SPEC_CHILDREN) {
            if is_safe_spec_command_name(sub.name.trim()) {
                children.push(fig_spec_to_entry(sub, false, depth + 1));
            }
        }
    }

    if let Some(ref args_field) = spec.args {
        children.extend(fig_args_to_entries(args_field.as_slice()));
    }

    for opt in spec.options.iter().take(MAX_SPEC_CHILDREN) {
        let Some(ref names_raw) = opt.name else {
            continue;
        };
        let names: Vec<Cow<'_, str>> = names_raw
            .as_slice()
            .iter()
            .take(64)
            .filter_map(|s| clean_spec_token_cow(s))
            .collect();
        if names.is_empty() {
            continue;
        }
        let primary_idx = names.iter().position(|n| n.starts_with("--")).unwrap_or(0);
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

        let mut opt_entry =
            build_option_entry_from_tokens(names, primary_idx, opt_kind, &opt.description);
        opt_entry.children = arg_children;
        children.push(opt_entry);
    }

    entry.children = children;
    entry
}

fn fig_args_to_entries(args: &[FigArg]) -> Vec<Entry> {
    let mut out = Vec::new();
    for arg in args.iter().take(MAX_SPEC_CHILDREN) {
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
        for sug in arg.suggestions.iter().take(MAX_SPEC_CHILDREN) {
            match sug {
                FigSuggestion::Plain(val) if !val.trim().is_empty() => {
                    let clean_v = sanitize_single_line(val.trim());
                    let v = clean_v.trim();
                    if !v.is_empty() && v.len() <= 256 {
                        out.push(Entry::new(v, v, EntryKind::Enum, ""));
                    }
                }
                FigSuggestion::Detailed { name, description } if !name.trim().is_empty() => {
                    let clean_n = sanitize_single_line(name.trim());
                    let n = clean_n.trim();
                    if !n.is_empty() && n.len() <= 256 {
                        let clean_d = sanitize_multiline_desc(description.trim());
                        out.push(Entry::new(n, n, EntryKind::Enum, clean_d.as_ref()));
                    }
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

#[derive(Debug, Clone, Copy)]
struct YamlLine<'a> {
    indent: usize,
    text: &'a str,
}

fn strip_yaml_comment(line: &str) -> &str {
    if !line.contains('#') {
        return line;
    }
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

fn tokenize_yaml(input: &str) -> anyhow::Result<Vec<YamlLine<'_>>> {
    let est = (input.len() / 32).clamp(8, 512);
    let mut out = Vec::with_capacity(est);
    for raw in input.lines() {
        if out.len() >= MAX_YAML_LINES {
            anyhow::bail!("YAML spec exceeds maximum allowed lines ({MAX_YAML_LINES})");
        }
        let indent = raw.bytes().take_while(|b| *b == b' ').count();
        if indent > 256 {
            anyhow::bail!("YAML indentation exceeds maximum allowed spaces (256)");
        }
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
            text: cleaned,
        });
    }
    Ok(out)
}

fn yaml_to_json(input: &str) -> anyhow::Result<serde_json::Value> {
    let lines = tokenize_yaml(input)?;
    if lines.is_empty() {
        return Ok(serde_json::Value::Object(serde_json::Map::new()));
    }
    let (val, next_idx) = parse_yaml_block(&lines, 0, lines[0].indent, 0)?;
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
    lines: &[YamlLine<'_>],
    idx: &mut usize,
    parent_indent: usize,
    val_part: &str,
    depth: usize,
) -> anyhow::Result<serde_json::Value> {
    if depth >= MAX_YAML_DEPTH {
        anyhow::bail!("YAML nesting depth exceeds maximum allowed ({MAX_YAML_DEPTH})");
    }
    if let Some(literal) = is_block_scalar(val_part) {
        if *idx < lines.len() && lines[*idx].indent > parent_indent {
            let block_indent = lines[*idx].indent;
            let mut parts = Vec::new();
            while *idx < lines.len() && lines[*idx].indent >= block_indent {
                parts.push(lines[*idx].text);
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
            let (nested, next_idx) = parse_yaml_block(lines, *idx, child_indent, depth)?;
            *idx = next_idx;
            Ok(nested)
        } else if *idx < lines.len()
            && lines[*idx].indent == parent_indent
            && is_seq_item(lines[*idx].text)
        {
            let (nested, next_idx) = parse_yaml_seq(lines, *idx, lines[*idx].indent, depth)?;
            *idx = next_idx;
            Ok(nested)
        } else {
            Ok(serde_json::Value::Null)
        }
    } else {
        parse_yaml_inline(val_part, depth)
    }
}

fn find_mapping_colon(text: &str) -> Option<usize> {
    if !text.contains(':') {
        return None;
    }
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
    lines: &[YamlLine<'_>],
    start: usize,
    base_indent: usize,
    depth: usize,
) -> anyhow::Result<(serde_json::Value, usize)> {
    if depth >= MAX_YAML_DEPTH {
        anyhow::bail!("YAML nesting depth exceeds maximum allowed ({MAX_YAML_DEPTH})");
    }
    if start >= lines.len() {
        return Ok((serde_json::Value::Null, start));
    }
    if is_seq_item(lines[start].text) {
        parse_yaml_seq(lines, start, base_indent, depth)
    } else {
        parse_yaml_map(lines, start, base_indent, depth)
    }
}

fn parse_yaml_map(
    lines: &[YamlLine<'_>],
    mut idx: usize,
    base_indent: usize,
    depth: usize,
) -> anyhow::Result<(serde_json::Value, usize)> {
    if depth >= MAX_YAML_DEPTH {
        anyhow::bail!("YAML nesting depth exceeds maximum allowed ({MAX_YAML_DEPTH})");
    }
    let mut map = serde_json::Map::new();
    while idx < lines.len() {
        let line = &lines[idx];
        if line.indent < base_indent || is_seq_item(line.text) {
            break;
        }
        if line.indent > base_indent {
            anyhow::bail!("Unexpected YAML indentation at: {}", line.text);
        }
        let Some(colon) = find_mapping_colon(line.text) else {
            anyhow::bail!(
                "Expected YAML 'key: value' mapping entry, found: {}",
                line.text
            );
        };
        let key = unquote_yaml_scalar(line.text[..colon].trim());
        let val_part = line.text[colon + 1..].trim();
        let line_indent = line.indent;
        idx += 1;

        let val = consume_yaml_value(lines, &mut idx, line_indent, val_part, depth + 1)?;
        map.insert(key, val);
    }
    Ok((serde_json::Value::Object(map), idx))
}

fn parse_yaml_seq(
    lines: &[YamlLine<'_>],
    mut idx: usize,
    base_indent: usize,
    depth: usize,
) -> anyhow::Result<(serde_json::Value, usize)> {
    if depth >= MAX_YAML_DEPTH {
        anyhow::bail!("YAML nesting depth exceeds maximum allowed ({MAX_YAML_DEPTH})");
    }
    let mut seq = Vec::new();
    while idx < lines.len() {
        let line = &lines[idx];
        if line.indent != base_indent || !is_seq_item(line.text) {
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
                let (nested, next_idx) = parse_yaml_block(lines, idx, child_indent, depth + 1)?;
                idx = next_idx;
                seq.push(nested);
            } else {
                seq.push(serde_json::Value::Null);
            }
        } else if let Some(colon) = find_mapping_colon(after_dash) {
            let first_key = unquote_yaml_scalar(after_dash[..colon].trim());
            let first_val_part = after_dash[colon + 1..].trim();
            let first_val =
                consume_yaml_value(lines, &mut idx, item_indent, first_val_part, depth + 1)?;

            let mut item_map = serde_json::Map::new();
            item_map.insert(first_key, first_val);
            if idx < lines.len() && lines[idx].indent > base_indent && !is_seq_item(lines[idx].text)
            {
                let (rest_val, next_idx) =
                    parse_yaml_map(lines, idx, lines[idx].indent, depth + 1)?;
                idx = next_idx;
                if let serde_json::Value::Object(rest_map) = rest_val {
                    item_map.extend(rest_map);
                }
            }
            seq.push(serde_json::Value::Object(item_map));
        } else {
            seq.push(parse_yaml_inline(after_dash, depth + 1)?);
        }
    }
    Ok((serde_json::Value::Array(seq), idx))
}

fn parse_yaml_inline(raw: &str, depth: usize) -> anyhow::Result<serde_json::Value> {
    if depth >= MAX_YAML_DEPTH {
        anyhow::bail!("YAML inline nesting depth exceeds maximum allowed ({MAX_YAML_DEPTH})");
    }
    let trimmed = raw.trim();
    if trimmed.starts_with('[') && trimmed.ends_with(']') {
        let inner = &trimmed[1..trimmed.len() - 1];
        let items = split_flow_items(inner);
        let mut arr = Vec::with_capacity(items.len());
        for item in items {
            let t = item.trim();
            if !t.is_empty() {
                arr.push(parse_yaml_inline(t, depth + 1)?);
            }
        }
        return Ok(serde_json::Value::Array(arr));
    }

    if trimmed.starts_with('{') && trimmed.ends_with('}') {
        let inner = &trimmed[1..trimmed.len() - 1];
        let pairs = split_flow_items(inner);
        let mut map = serde_json::Map::new();
        for pair in pairs {
            let t = pair.trim();
            if t.is_empty() {
                continue;
            }
            if let Some(colon) = find_mapping_colon(t) {
                let k = unquote_yaml_scalar(t[..colon].trim());
                let v = parse_yaml_inline(t[colon + 1..].trim(), depth + 1)?;
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

fn split_flow_items(inner: &str) -> Vec<&str> {
    let mut items = Vec::new();
    let mut in_single = false;
    let mut in_double = false;
    let mut escaped = false;
    let mut depth = 0usize;
    let mut start = 0usize;

    for (idx, ch) in inner.char_indices() {
        if escaped {
            escaped = false;
            continue;
        }
        match ch {
            '\\' if in_double => escaped = true,
            '"' if !in_single => in_double = !in_double,
            '\'' if !in_double => in_single = !in_single,
            '[' | '{' if !in_single && !in_double => depth += 1,
            ']' | '}' if !in_single && !in_double => depth = depth.saturating_sub(1),
            ',' if !in_single && !in_double && depth == 0 => {
                items.push(&inner[start..idx]);
                start = idx + 1;
            }
            _ => {}
        }
    }
    let tail = &inner[start..];
    if !tail.trim().is_empty() {
        items.push(tail);
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

    #[test]
    fn test_parse_spec_blocks_path_traversal_unsafe_names_and_oversized_files() {
        for bad_name in [
            "../evil.yaml",
            "..\\evil.yaml",
            "sub/cmd.json",
            "sub\\cmd.yml",
            "C:cmd.json",
            ".hidden.yaml",
            ".json",
            "bad\0.json",
            "tool.txt",
            "",
        ] {
            assert!(!is_spec_filename(bad_name), "should reject {bad_name:?}");
            assert!(parse_spec_str(bad_name, r#"{"name":"ok"}"#).is_err());
        }
        assert!(is_spec_filename("git.json"));
        assert!(is_spec_filename("MYTOOL.YAML"));
        assert!(is_spec_filename("docker.yml"));

        for hostile_root in ["-rf", "../evil", "sub/cmd", "bad name", "cmd\nid", "C:evil"] {
            let yaml = format!("name: \"{hostile_root}\"\nflags:\n  -v: verbose\n");
            assert!(
                parse_spec_str("hostile.yaml", &yaml).is_err(),
                "should reject hostile root {hostile_root:?}"
            );
            let fig = format!(r#"{{"name":"{hostile_root}","options":[{{"name":"-v"}}]}}"#);
            assert!(
                parse_spec_str("hostile.json", &fig).is_err(),
                "should reject hostile Fig root {hostile_root:?}"
            );
        }

        let oversized = format!(
            "{{\"name\":\"ok\",\"description\":\"{}\"}}",
            "x".repeat(MAX_SPEC_BYTES)
        );
        assert!(parse_spec_str("big.json", &oversized).is_err());

        // Valid root should filter unsafe subcommands and strip Bidi/CR while keeping \n in desc
        let mixed_yaml = "name: safetool\ndescription: \"Line1\\r\\nLine2\u{202e}\"\ncommands:\n  - name: -rf\n  - name: ../escape\n  - name: build\n    description: Valid sub\n";
        let entry = parse_spec_str("safetool.yaml", mixed_yaml).expect("valid root survives");
        let sub_names: Vec<&str> = entry.children.iter().map(|c| c.text.as_str()).collect();
        assert_eq!(sub_names, vec!["build"]);
        assert_eq!(entry.desc, "Line1\nLine2");
    }

    #[test]
    fn test_parse_spec_bounds_yaml_depth_flow_depth_and_command_tree_recursion() {
        let mut deep_yaml = String::from("name: deep\n");
        for lvl in 0..60 {
            let indent = "  ".repeat(lvl + 1);
            deep_yaml.push_str(&format!("{indent}k{lvl}:\n"));
        }
        deep_yaml.push_str(&format!("{}leaf: 1\n", "  ".repeat(61)));
        assert!(parse_spec_str("deep.yaml", &deep_yaml).is_err());

        let deep_flow = format!(
            "name: flow\nflags:\n  -f: {}1{}\n",
            "[".repeat(60),
            "]".repeat(60)
        );
        assert!(parse_spec_str("flow.yaml", &deep_flow).is_err());

        // Deep Carapace command tree (> MAX_SPEC_TREE_DEPTH) should parse safely with capped depth
        let mut deep_json = String::from(r#"{"name":"root","flags":{"-h":"help"},"commands":["#);
        for lvl in 0..30 {
            deep_json.push_str(&format!(r#"{{"name":"sub{lvl}","commands":["#));
        }
        deep_json.push_str(r#"{"name":"leaf","flags":{}}"#);
        for _ in 0..30 {
            deep_json.push_str("]}");
        }
        deep_json.push_str("]}");

        let parsed = parse_spec_str("tree.json", &deep_json).expect("deep JSON bounded safely");
        let mut depth_seen = 0usize;
        let mut cur = &parsed;
        while let Some(next) = cur.children.iter().find(|c| c.kind == EntryKind::Subcmd) {
            depth_seen += 1;
            cur = next;
        }
        assert!(depth_seen <= MAX_SPEC_TREE_DEPTH + 1);
        assert!(depth_seen >= MAX_SPEC_TREE_DEPTH);
    }

    #[test]
    fn test_pentest_specs_adversarial_yaml_json_escapes_and_bounds() {
        // 1. Malformed YAML structures: tabs, >256 indent, > MAX_YAML_LINES, BOM-only.
        assert!(parse_spec_str("tabs.yaml", "name: tool\n\tflags:\n").is_err());
        let deep_indent = format!("name: tool\n{}k: v\n", " ".repeat(260));
        assert!(parse_spec_str("indent260.yaml", &deep_indent).is_err());

        let mut too_many_lines = String::from("name: lines\nflags:\n");
        for i in 0..(MAX_YAML_LINES + 10) {
            too_many_lines.push_str(&format!("  --f{i}: desc\n"));
        }
        assert!(parse_spec_str("many.yaml", &too_many_lines).is_err());
        assert!(parse_spec_str("bom.yaml", "\u{feff}   \n  \n").is_err());

        // 2. Deeply nested JSON beyond serde_json recursion limit must fail cleanly without panic.
        let deep_json = format!(
            "{{\"name\":\"deep\",\"options\":[{{\"name\":\"-x\",\"args\":{{\"suggestions\":{}1{}}}}}]}}",
            "[".repeat(200),
            "]".repeat(200)
        );
        assert!(parse_spec_str("deepjson.json", &deep_json).is_err());

        // 3. Injection / Bidi / control chars in Carapace & Fig specs.
        for bad_root in [
            "tool\x1b[31m",
            "tool\u{202e}evil",
            "tool\u{061c}alm",
            "tool\u{2028}sep",
        ] {
            let json = serde_json::json!({
                "name": bad_root,
                "flags": {"-v": "verbose"}
            })
            .to_string();
            assert!(
                parse_spec_str("badroot.json", &json).is_err(),
                "must reject hostile root {bad_root:?}"
            );
        }

        // 4. Case-insensitive dedup in seen_sub / seen_flags + local override + cap bounds.
        let yaml_dedup = r#"
name: deduptool
persistentflags:
  --Mode: Persistent flag
  --Keep: Persistent keep
flags:
  -m, --mode=: Local override
  --KEEP: Duplicate differing only by case
commands:
  - name: Deploy
    description: First deploy
  - name: deploy
    description: Duplicate lowercase deploy
  - name: "bad\u{202e}sub"
"#;
        let parsed = parse_spec_str("dedup.yaml", yaml_dedup).expect("dedup yaml");
        let sub_names: Vec<&str> = parsed
            .children
            .iter()
            .filter(|c| c.kind == EntryKind::Subcmd)
            .map(|c| c.text.as_str())
            .collect();
        assert_eq!(sub_names, vec!["Deploy"]);

        let mode = parsed
            .children
            .iter()
            .find(|c| c.text == "--mode")
            .expect("--mode local override");
        assert_eq!(mode.kind, EntryKind::Param);
        assert_eq!(mode.desc, "Local override");
        assert_eq!(
            parsed
                .children
                .iter()
                .filter(|c| c.text.eq_ignore_ascii_case("--mode"))
                .count(),
            1
        );
        assert_eq!(
            parsed
                .children
                .iter()
                .filter(|c| c.text.eq_ignore_ascii_case("--keep"))
                .count(),
            1
        );

        // 5. Fig JSON suggestion length bounds & control/Bidi stripping + alias cap.
        let long_sug = "a".repeat(300);
        let many_aliases: Vec<String> = (0..100).map(|i| format!("--alias-{i}")).collect();
        let fig_hostile = serde_json::json!({
            "name": "figsec",
            "description": "Safe\r\nDesc\u{202e}\u{061c}",
            "subcommands": [
                {"name": "-rf"},
                {"name": "oksub", "description": "Valid"}
            ],
            "options": [
                {
                    "name": many_aliases,
                    "description": "Option\x1b[31m",
                    "args": {
                        "suggestions": [
                            long_sug,
                            {"name": "good\u{202e}", "description": "Clean\u{0007}\nLine2"}
                        ]
                    }
                }
            ]
        })
        .to_string();
        let fig_entry = parse_spec_str("figsec.json", &fig_hostile).expect("figsec");
        assert_eq!(fig_entry.desc, "Safe\nDesc");
        assert!(fig_entry.children.iter().any(|c| c.text == "oksub"));
        assert!(!fig_entry.children.iter().any(|c| c.text == "-rf"));
        let opt = fig_entry
            .children
            .iter()
            .find(|c| c.text == "--alias-0")
            .expect("--alias-0");
        assert!(opt.aliases.len() < 64);
        assert_eq!(opt.children.len(), 1);
        assert_eq!(opt.children[0].text, "good");
        assert_eq!(opt.children[0].desc, "Clean\nLine2");
    }
}
