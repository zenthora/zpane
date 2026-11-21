use super::catalog::read_bounded_string;
use crate::model::{Entry, EntryKind};
use crate::sanitize::{
    is_unsafe_display_char, sanitize_single_line, strip_escapes_and_controls_into,
};
use std::collections::HashSet;
use std::path::Path;

const MAX_TASK_FILE_BYTES: u64 = 512 * 1024;
const MAX_TASK_LINES: usize = 8_192;
const MAX_TASK_ENTRIES: usize = 256;
const MAX_TASK_NAME_BYTES: usize = 128;
const MAX_TASK_DESC_BYTES: usize = 256;

fn truncate_utf8(s: &str, max_bytes: usize) -> &str {
    if s.len() <= max_bytes {
        return s;
    }
    let mut end = max_bytes;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

fn is_safe_task_name(name: &str) -> bool {
    if name.is_empty()
        || name.len() > MAX_TASK_NAME_BYTES
        || name.starts_with(['-', '/', '\\'])
        || name == "."
        || name == ".."
        || name.contains("..")
    {
        return false;
    }
    !name.chars().any(|c| {
        is_unsafe_display_char(c)
            || c.is_whitespace()
            || matches!(
                c,
                '&' | '|'
                    | ';'
                    | '<'
                    | '>'
                    | '$'
                    | '`'
                    | '"'
                    | '\''
                    | '('
                    | ')'
                    | '{'
                    | '}'
                    | '*'
                    | '?'
                    | '!'
                    | '%'
                    | '^'
                    | '='
                    | '\\'
            )
    })
}

fn clean_task_desc(raw: &str, fallback: &str) -> String {
    let mut cleaned = String::new();
    strip_escapes_and_controls_into(raw.trim(), &mut cleaned);
    let trimmed = cleaned.trim();
    if trimmed.is_empty() {
        fallback.to_string()
    } else {
        truncate_utf8(trimmed, MAX_TASK_DESC_BYTES).to_string()
    }
}

fn make_task_entry(name: &str, desc: &str, fallback_desc: &str) -> Option<Entry> {
    let clean_name = sanitize_single_line(name.trim());
    let trimmed = clean_name.trim();
    if !is_safe_task_name(trimmed) {
        return None;
    }
    Some(Entry::new(
        trimmed,
        trimmed,
        EntryKind::Subcmd,
        clean_task_desc(desc, fallback_desc),
    ))
}

fn strip_hash_comment_respecting_quotes(line: &str) -> &str {
    let mut in_single = false;
    let mut in_double = false;
    let mut escaped = false;
    for (idx, ch) in line.char_indices() {
        if escaped {
            escaped = false;
            continue;
        }
        match ch {
            '\\' if in_double => escaped = true,
            '"' if !in_single => in_double = !in_double,
            '\'' if !in_double => in_single = !in_single,
            '#' if !in_single && !in_double => return &line[..idx],
            _ => {}
        }
    }
    line
}

fn unquote_simple_string(raw: &str) -> Option<String> {
    let trimmed = raw.trim();
    if trimmed.len() >= 2 && trimmed.starts_with('"') && trimmed.ends_with('"') {
        let inner = &trimmed[1..trimmed.len() - 1];
        let mut out = String::with_capacity(inner.len());
        let mut chars = inner.chars();
        while let Some(ch) = chars.next() {
            if ch == '\\' {
                match chars.next() {
                    Some('n') => out.push(' '),
                    Some('t') => out.push(' '),
                    Some('"') => out.push('"'),
                    Some('\\') => out.push('\\'),
                    Some(other) => out.push(other),
                    None => break,
                }
            } else {
                out.push(ch);
            }
        }
        Some(out)
    } else if trimmed.len() >= 2 && trimmed.starts_with('\'') && trimmed.ends_with('\'') {
        Some(trimmed[1..trimmed.len() - 1].to_string())
    } else {
        None
    }
}

// ============================================================================
// package.json scripts parser
// ============================================================================

fn parse_package_json_scripts(content: &str) -> Vec<Entry> {
    let trimmed = content.trim_start_matches('\u{feff}').trim();
    if trimmed.is_empty() {
        return Vec::new();
    }
    let Ok(val) = serde_json::from_str::<serde_json::Value>(trimmed) else {
        return Vec::new();
    };
    let Some(scripts) = val.get("scripts").and_then(|v| v.as_object()) else {
        return Vec::new();
    };

    let mut out = Vec::with_capacity(scripts.len().min(MAX_TASK_ENTRIES));
    let mut seen = HashSet::with_capacity(scripts.len().min(MAX_TASK_ENTRIES));
    for (name, cmd) in scripts {
        if out.len() >= MAX_TASK_ENTRIES {
            break;
        }
        let desc = cmd.as_str().unwrap_or("");
        if let Some(entry) = make_task_entry(name, desc, "package.json script") {
            if seen.insert(entry.text.to_ascii_lowercase()) {
                out.push(entry);
            }
        }
    }
    out
}

fn load_package_json_scripts(dir: &Path) -> Vec<Entry> {
    let path = dir.join("package.json");
    read_bounded_string(&path, MAX_TASK_FILE_BYTES)
        .map(|raw| parse_package_json_scripts(&raw))
        .unwrap_or_default()
}

// ============================================================================
// justfile / .justfile recipe parser
// ============================================================================

fn extract_just_doc_attr(trimmed: &str) -> Option<String> {
    let inner = trimmed.strip_prefix('[')?.strip_suffix(']')?;
    let doc_start = inner.find("doc(")?;
    let after = &inner[doc_start + 4..];
    let quote = after.chars().next()?;
    if quote != '\'' && quote != '"' {
        return None;
    }
    let mut end_idx = None;
    let mut escaped = false;
    for (i, ch) in after[1..].char_indices() {
        if escaped {
            escaped = false;
            continue;
        }
        if ch == '\\' && quote == '"' {
            escaped = true;
        } else if ch == quote {
            end_idx = Some(1 + i);
            break;
        }
    }
    let end = end_idx?;
    unquote_simple_string(&after[..=end])
}

fn just_line_has_recipe_colon(after_name: &str) -> bool {
    let trimmed = after_name.trim_start();
    if trimmed.starts_with(":=") {
        return false;
    }
    let mut in_single = false;
    let mut in_double = false;
    let mut escaped = false;
    let bytes = after_name.as_bytes();
    for (idx, ch) in after_name.char_indices() {
        if escaped {
            escaped = false;
            continue;
        }
        match ch {
            '\\' if in_double => escaped = true,
            '"' if !in_single => in_double = !in_double,
            '\'' if !in_double => in_single = !in_single,
            '#' if !in_single && !in_double => return false,
            ':' if !in_single && !in_double => {
                if bytes.get(idx + 1) == Some(&b'=') {
                    return false;
                }
                return true;
            }
            _ => {}
        }
    }
    false
}

fn is_divider_comment(s: &str) -> bool {
    s.is_empty()
        || s.chars()
            .all(|c| matches!(c, '#' | '=' | '-' | '*' | '_' | '~' | ' '))
}

fn parse_justfile_recipes(content: &str) -> Vec<Entry> {
    let body = content.strip_prefix('\u{feff}').unwrap_or(content);
    let mut out = Vec::new();
    let mut seen = HashSet::new();
    let mut pending_doc: Option<String> = None;
    let mut is_private = false;

    for raw_line in body.lines().take(MAX_TASK_LINES) {
        if out.len() >= MAX_TASK_ENTRIES {
            break;
        }
        let line = raw_line.trim_end_matches('\r');
        let trimmed = line.trim();
        if trimmed.is_empty() {
            pending_doc = None;
            is_private = false;
            continue;
        }
        // Indented lines are recipe bodies or continuations, never recipe headers.
        if line.starts_with([' ', '\t']) {
            pending_doc = None;
            is_private = false;
            continue;
        }
        if let Some(comment) = trimmed.strip_prefix('#') {
            let doc = comment.trim();
            if !is_divider_comment(doc) {
                pending_doc = Some(doc.to_string());
            }
            continue;
        }
        if trimmed.starts_with('[') && trimmed.ends_with(']') {
            let inner = &trimmed[1..trimmed.len() - 1];
            if inner
                .split(',')
                .any(|part| part.trim().eq_ignore_ascii_case("private"))
            {
                is_private = true;
            }
            if let Some(doc) = extract_just_doc_attr(trimmed) {
                pending_doc = Some(doc);
            }
            continue;
        }

        let header = trimmed.strip_prefix('@').unwrap_or(trimmed);
        let ident_len = header
            .bytes()
            .take_while(|&b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
            .count();
        let name = &header[..ident_len];
        let rest = &header[ident_len..];

        let first_valid = name
            .as_bytes()
            .first()
            .is_some_and(|&b| b.is_ascii_alphabetic() || b == b'_');
        let is_directive = matches!(
            name,
            "set" | "alias" | "export" | "unexport" | "import" | "mod" | "if" | "else"
        ) && (rest.is_empty() || rest.starts_with([' ', '\t', '?']));

        if first_valid
            && !is_directive
            && !is_private
            && !name.starts_with('_')
            && just_line_has_recipe_colon(rest)
        {
            let desc = pending_doc.as_deref().unwrap_or("");
            if let Some(entry) = make_task_entry(name, desc, "justfile recipe") {
                if seen.insert(entry.text.to_ascii_lowercase()) {
                    out.push(entry);
                }
            }
        }

        pending_doc = None;
        is_private = false;
    }

    out
}

fn load_justfile_recipes(dir: &Path) -> Vec<Entry> {
    for candidate in ["justfile", "Justfile", ".justfile", ".Justfile"] {
        let path = dir.join(candidate);
        if let Some(raw) = read_bounded_string(&path, MAX_TASK_FILE_BYTES) {
            return parse_justfile_recipes(&raw);
        }
    }
    Vec::new()
}

// ============================================================================
// Makefile / makefile / GNUmakefile target parser
// ============================================================================

fn parse_makefile_targets(content: &str) -> Vec<Entry> {
    let body = content.strip_prefix('\u{feff}').unwrap_or(content);
    let mut out = Vec::new();
    let mut seen = HashSet::new();
    let mut pending_comment: Option<String> = None;
    let mut in_define = false;

    for raw_line in body.lines().take(MAX_TASK_LINES) {
        if out.len() >= MAX_TASK_ENTRIES {
            break;
        }
        let line = raw_line.trim_end_matches('\r');
        let trimmed = line.trim();
        if trimmed.is_empty() {
            pending_comment = None;
            continue;
        }
        if line.starts_with(['\t', ' ']) {
            pending_comment = None;
            continue;
        }
        if let Some(comment) = trimmed.strip_prefix('#') {
            let doc = comment.trim_start_matches('#').trim();
            if !is_divider_comment(doc) {
                pending_comment = Some(doc.to_string());
            }
            continue;
        }

        let first_word = trimmed.split_whitespace().next().unwrap_or("");
        if first_word == "define"
            || (first_word == "override" && trimmed.split_whitespace().nth(1) == Some("define"))
        {
            in_define = true;
            pending_comment = None;
            continue;
        }
        if first_word == "endef" {
            in_define = false;
            pending_comment = None;
            continue;
        }
        if in_define
            || matches!(
                first_word,
                "include"
                    | "-include"
                    | "sinclude"
                    | "override"
                    | "export"
                    | "unexport"
                    | "ifdef"
                    | "ifndef"
                    | "ifeq"
                    | "ifneq"
                    | "else"
                    | "endif"
                    | "vpath"
            )
        {
            pending_comment = None;
            continue;
        }

        if let Some(colon_idx) = trimmed.find(':') {
            let lhs = trimmed[..colon_idx].trim();
            let rhs_after_colon = &trimmed[colon_idx + 1..];
            // Skip variable assignments (`FOO = bar:baz`, `FOO := bar`, `FOO ::= bar`).
            let is_assignment = lhs.contains('=')
                || rhs_after_colon.starts_with('=')
                || rhs_after_colon.starts_with(":=");
            if !is_assignment && !lhs.is_empty() {
                let inline_doc = rhs_after_colon
                    .split_once("##")
                    .map(|(_, d)| d.trim())
                    .filter(|d| !d.is_empty());
                let desc = inline_doc.or(pending_comment.as_deref()).unwrap_or("");

                for target in lhs.split_whitespace() {
                    if out.len() >= MAX_TASK_ENTRIES {
                        break;
                    }
                    // Exclude special targets (`.PHONY`, `.DEFAULT`, `.SILENT`, etc.) and pattern/var rules.
                    if target.starts_with('.')
                        || target.contains(['%', '$', '(', ')', '=', ';', ','])
                    {
                        continue;
                    }
                    if let Some(entry) = make_task_entry(target, desc, "Makefile target") {
                        if seen.insert(entry.text.to_ascii_lowercase()) {
                            out.push(entry);
                        }
                    }
                }
            }
        }

        pending_comment = None;
    }

    out
}

fn load_makefile_targets(dir: &Path) -> Vec<Entry> {
    for candidate in ["GNUmakefile", "makefile", "Makefile"] {
        let path = dir.join(candidate);
        if let Some(raw) = read_bounded_string(&path, MAX_TASK_FILE_BYTES) {
            return parse_makefile_targets(&raw);
        }
    }
    Vec::new()
}

// ============================================================================
// mise.toml / .mise.toml task parser
// ============================================================================

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MiseSection {
    Other,
    TasksTable,
    TaskSubtable(usize),
}

fn split_toml_key_value(line: &str) -> Option<(&str, &str)> {
    let mut in_single = false;
    let mut in_double = false;
    let mut escaped = false;
    for (idx, ch) in line.char_indices() {
        if escaped {
            escaped = false;
            continue;
        }
        match ch {
            '\\' if in_double => escaped = true,
            '"' if !in_single => in_double = !in_double,
            '\'' if !in_double => in_single = !in_single,
            '=' if !in_single && !in_double => {
                return Some((line[..idx].trim(), line[idx + 1..].trim()));
            }
            _ => {}
        }
    }
    None
}

fn parse_mise_subtable_task_name(rest: &str) -> Option<String> {
    let trimmed = rest.trim();
    if trimmed.is_empty() {
        return None;
    }
    if trimmed.starts_with('"') || trimmed.starts_with('\'') {
        // Must be a single quoted task name without trailing `.subkey` (e.g. skip `[tasks."a".env]`).
        let unquoted = unquote_simple_string(trimmed)?;
        Some(unquoted)
    } else if trimmed.contains('.') {
        // Skip nested subtables like `[tasks.build.env]`.
        None
    } else {
        Some(trimmed.to_string())
    }
}

fn extract_inline_table_field(inline_table: &str, field: &str) -> Option<String> {
    let inner = inline_table.trim().strip_prefix('{')?.strip_suffix('}')?;
    let mut in_single = false;
    let mut in_double = false;
    let mut escaped = false;
    let mut depth = 0usize;
    let mut start = 0usize;

    let mut segments = Vec::new();
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
                segments.push(&inner[start..idx]);
                start = idx + 1;
            }
            _ => {}
        }
    }
    segments.push(&inner[start..]);

    for seg in segments {
        if let Some((k, v)) = split_toml_key_value(seg) {
            let key_clean = unquote_simple_string(k).unwrap_or_else(|| k.to_string());
            if key_clean == field {
                if let Some(val) = unquote_simple_string(v) {
                    return Some(val);
                }
            }
        }
    }
    None
}

fn upsert_mise_task(
    tasks: &mut Vec<(String, String, bool)>,
    index_by_lower: &mut std::collections::HashMap<String, usize>,
    raw_name: &str,
    desc: &str,
    explicit_desc: bool,
) -> Option<usize> {
    let clean_name = sanitize_single_line(raw_name.trim());
    let name = clean_name.trim();
    if !is_safe_task_name(name) {
        return None;
    }
    let lower = name.to_ascii_lowercase();
    if let Some(&idx) = index_by_lower.get(&lower) {
        if !desc.trim().is_empty() && (explicit_desc || !tasks[idx].2) {
            tasks[idx].1 = clean_task_desc(desc, "mise task");
            tasks[idx].2 = explicit_desc;
        }
        return Some(idx);
    }
    if tasks.len() >= MAX_TASK_ENTRIES {
        return None;
    }
    let idx = tasks.len();
    tasks.push((
        name.to_string(),
        clean_task_desc(desc, "mise task"),
        explicit_desc,
    ));
    index_by_lower.insert(lower, idx);
    Some(idx)
}

fn parse_mise_toml_tasks(content: &str) -> Vec<Entry> {
    let body = content.strip_prefix('\u{feff}').unwrap_or(content);
    let mut tasks: Vec<(String, String, bool)> = Vec::new(); // (name, desc, has_explicit_desc)
    let mut index_by_lower: std::collections::HashMap<String, usize> =
        std::collections::HashMap::new();
    let mut section = MiseSection::Other;

    for raw_line in body.lines().take(MAX_TASK_LINES) {
        let line = strip_hash_comment_respecting_quotes(raw_line).trim();
        if line.is_empty() {
            continue;
        }
        if line.starts_with('[') && line.ends_with(']') && !line.starts_with("[[") {
            let hdr = line[1..line.len() - 1].trim();
            if hdr == "tasks" {
                section = MiseSection::TasksTable;
            } else if let Some(rest) = hdr.strip_prefix("tasks.") {
                if let Some(task_name) = parse_mise_subtable_task_name(rest) {
                    section =
                        upsert_mise_task(&mut tasks, &mut index_by_lower, &task_name, "", false)
                            .map(MiseSection::TaskSubtable)
                            .unwrap_or(MiseSection::Other);
                } else {
                    section = MiseSection::Other;
                }
            } else {
                section = MiseSection::Other;
            }
            continue;
        }

        match section {
            MiseSection::TasksTable => {
                if let Some((raw_k, raw_v)) = split_toml_key_value(line) {
                    let key = unquote_simple_string(raw_k).unwrap_or_else(|| raw_k.to_string());
                    let (desc, explicit) = if raw_v.starts_with('{') {
                        if let Some(d) = extract_inline_table_field(raw_v, "description") {
                            (d, true)
                        } else if let Some(r) = extract_inline_table_field(raw_v, "run") {
                            (r, false)
                        } else {
                            (String::new(), false)
                        }
                    } else if let Some(s) = unquote_simple_string(raw_v) {
                        (s, false)
                    } else {
                        (String::new(), false)
                    };
                    let _ =
                        upsert_mise_task(&mut tasks, &mut index_by_lower, &key, &desc, explicit);
                }
            }
            MiseSection::TaskSubtable(idx) => {
                if let Some((raw_k, raw_v)) = split_toml_key_value(line) {
                    let key = raw_k.trim();
                    if key == "description" {
                        if let Some(d) = unquote_simple_string(raw_v) {
                            tasks[idx].1 = clean_task_desc(&d, "mise task");
                            tasks[idx].2 = true;
                        }
                    } else if key == "run" && !tasks[idx].2 {
                        if let Some(r) = unquote_simple_string(raw_v) {
                            tasks[idx].1 = clean_task_desc(&r, "mise task");
                        }
                    }
                }
            }
            MiseSection::Other => {}
        }
    }

    tasks
        .into_iter()
        .map(|(name, desc, _)| Entry::new(name.clone(), name, EntryKind::Subcmd, desc))
        .collect()
}

fn load_mise_toml_tasks(dir: &Path) -> Vec<Entry> {
    for candidate in ["mise.toml", ".mise.toml"] {
        let path = dir.join(candidate);
        if let Some(raw) = read_bounded_string(&path, MAX_TASK_FILE_BYTES) {
            return parse_mise_toml_tasks(&raw);
        }
    }
    Vec::new()
}

// ============================================================================
// Tool spec assembly, chain lookup, and front-merge helpers
// ============================================================================

fn build_run_subcmd(desc: &str, alias: Option<&str>, tasks: Vec<Entry>) -> Entry {
    let label = match alias {
        Some(a) => format!("run ({a})"),
        None => "run".to_string(),
    };
    let mut run = Entry::new("run", label, EntryKind::Subcmd, desc);
    if let Some(a) = alias {
        run.aliases.push(a.to_string());
    }
    run.children = tasks;
    run
}

fn resolve_existing_candidate_name(
    dir_names: &HashSet<String>,
    candidates: &[&str],
) -> Option<String> {
    for &candidate in candidates {
        if dir_names.contains(candidate) {
            return Some(candidate.to_string());
        }
    }
    None
}

/// Read-only detection of supported project task files in `dir` (`package.json`,
/// `justfile`/`.justfile`, `GNUmakefile`/`makefile`/`Makefile`, `mise.toml`/`.mise.toml`)
/// along with the number of parsed tasks discovered in each file.
pub(super) fn detect_project_task_files(dir: &Path) -> Vec<(String, std::path::PathBuf, usize)> {
    const KNOWN_LOWER: &[&str] = &[
        "package.json",
        "justfile",
        ".justfile",
        "gnumakefile",
        "makefile",
        "mise.toml",
        ".mise.toml",
    ];
    let Ok(rd) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut dir_names = HashSet::new();
    for entry in rd.take(10_000).flatten() {
        if let Some(name) = entry.file_name().to_str() {
            if KNOWN_LOWER
                .iter()
                .any(|known| name.eq_ignore_ascii_case(known))
            {
                dir_names.insert(name.to_string());
            }
        }
    }

    let mut found = Vec::new();

    if let Some(name) = resolve_existing_candidate_name(&dir_names, &["package.json"]) {
        let path = dir.join(&name);
        if let Some(raw) = read_bounded_string(&path, MAX_TASK_FILE_BYTES) {
            let count = parse_package_json_scripts(&raw).len();
            found.push((name, path, count));
        }
    }

    if let Some(name) = resolve_existing_candidate_name(
        &dir_names,
        &["justfile", "Justfile", ".justfile", ".Justfile"],
    ) {
        let path = dir.join(&name);
        if let Some(raw) = read_bounded_string(&path, MAX_TASK_FILE_BYTES) {
            let count = parse_justfile_recipes(&raw).len();
            found.push((name, path, count));
        }
    }

    if let Some(name) =
        resolve_existing_candidate_name(&dir_names, &["GNUmakefile", "makefile", "Makefile"])
    {
        let path = dir.join(&name);
        if let Some(raw) = read_bounded_string(&path, MAX_TASK_FILE_BYTES) {
            let count = parse_makefile_targets(&raw).len();
            found.push((name, path, count));
        }
    }

    if let Some(name) = resolve_existing_candidate_name(&dir_names, &["mise.toml", ".mise.toml"]) {
        let path = dir.join(&name);
        if let Some(raw) = read_bounded_string(&path, MAX_TASK_FILE_BYTES) {
            let count = parse_mise_toml_tasks(&raw).len();
            found.push((name, path, count));
        }
    }

    found
}

/// Discovers project-local task targets in `dir` (`justfile`, `Makefile`, `package.json`, `mise.toml`)
/// without spawning any subprocesses and returns root tool [`Entry`] specs for catalog merging.
pub(super) fn discover_project_task_specs(dir: &Path) -> Vec<Entry> {
    let mut specs = Vec::new();

    let just_recipes = load_justfile_recipes(dir);
    if !just_recipes.is_empty() {
        let mut entry = Entry::new("just", "just", EntryKind::Cli, "Project recipes (justfile)");
        entry.children = just_recipes;
        specs.push(entry);
    }

    let make_targets = load_makefile_targets(dir);
    if !make_targets.is_empty() {
        let mut entry = Entry::new("make", "make", EntryKind::Cli, "Project targets (Makefile)");
        entry.children = make_targets;
        specs.push(entry);
    }

    let npm_scripts = load_package_json_scripts(dir);
    if !npm_scripts.is_empty() {
        let mut npm = Entry::new(
            "npm",
            "npm",
            EntryKind::Cli,
            "Project scripts (package.json)",
        );
        npm.children = vec![build_run_subcmd(
            "Run a package.json script",
            Some("run-script"),
            npm_scripts.clone(),
        )];
        specs.push(npm);

        let mut pnpm = Entry::new(
            "pnpm",
            "pnpm",
            EntryKind::Cli,
            "Project scripts (package.json)",
        );
        let mut pnpm_children = Vec::with_capacity(1 + npm_scripts.len());
        pnpm_children.push(build_run_subcmd(
            "Run a package.json script",
            None,
            npm_scripts.clone(),
        ));
        pnpm_children.extend(npm_scripts.clone());
        pnpm.children = pnpm_children;
        specs.push(pnpm);

        let mut bun = Entry::new(
            "bun",
            "bun",
            EntryKind::Cli,
            "Project scripts (package.json)",
        );
        let mut bun_children = Vec::with_capacity(1 + npm_scripts.len());
        bun_children.push(build_run_subcmd(
            "Run a package.json script",
            None,
            npm_scripts.clone(),
        ));
        bun_children.extend(npm_scripts);
        bun.children = bun_children;
        specs.push(bun);
    }

    let mise_tasks = load_mise_toml_tasks(dir);
    if !mise_tasks.is_empty() {
        let mut mise = Entry::new("mise", "mise", EntryKind::Cli, "Project tasks (mise.toml)");
        mise.children = vec![build_run_subcmd(
            "Run a mise.toml task",
            Some("r"),
            mise_tasks,
        )];
        specs.push(mise);
    }

    specs
}

fn exe_stem_lower(exe: &str) -> Option<String> {
    Path::new(exe)
        .file_stem()
        .and_then(|s| s.to_str())
        .map(str::to_ascii_lowercase)
}

/// Returns project-local task entries in `dir` matching `chain` (e.g. `["just"]`, `["make"]`,
/// `["npm", "run"]`, `["pnpm"]`, `["bun"]`, `["mise", "run"]`), performing zero filesystem I/O
/// for unrelated commands.
pub(super) fn project_task_entries_for_chain(dir: &Path, chain: &[&str]) -> Vec<Entry> {
    let Some(root) = chain.first().and_then(|exe| exe_stem_lower(exe)) else {
        return Vec::new();
    };
    match (root.as_str(), chain.len()) {
        ("just", 1) => load_justfile_recipes(dir),
        ("make", 1) => load_makefile_targets(dir),
        ("npm", 1) => {
            let scripts = load_package_json_scripts(dir);
            if scripts.is_empty() {
                Vec::new()
            } else {
                vec![build_run_subcmd(
                    "Run a package.json script",
                    Some("run-script"),
                    scripts,
                )]
            }
        }
        ("npm", 2)
            if chain[1].eq_ignore_ascii_case("run")
                || chain[1].eq_ignore_ascii_case("run-script") =>
        {
            load_package_json_scripts(dir)
        }
        ("pnpm" | "bun", 1) => {
            let scripts = load_package_json_scripts(dir);
            if scripts.is_empty() {
                Vec::new()
            } else {
                let mut out = Vec::with_capacity(1 + scripts.len());
                out.push(build_run_subcmd(
                    "Run a package.json script",
                    None,
                    scripts.clone(),
                ));
                out.extend(scripts);
                out
            }
        }
        ("pnpm" | "bun", 2) if chain[1].eq_ignore_ascii_case("run") => {
            load_package_json_scripts(dir)
        }
        ("mise", 1) => {
            let tasks = load_mise_toml_tasks(dir);
            if tasks.is_empty() {
                Vec::new()
            } else {
                vec![build_run_subcmd("Run a mise.toml task", Some("r"), tasks)]
            }
        }
        ("mise", 2)
            if chain[1].eq_ignore_ascii_case("run") || chain[1].eq_ignore_ascii_case("r") =>
        {
            load_mise_toml_tasks(dir)
        }
        _ => Vec::new(),
    }
}

/// Prepends `tasks` to `target` without duplicating entries; when a matching `Subcmd` (such as
/// `run`) already exists in `target`, merges `task.children` to the front of its children.
pub(super) fn merge_task_entries_front(target: &mut Vec<Entry>, tasks: Vec<Entry>) {
    if tasks.is_empty() {
        return;
    }
    let mut prefix = Vec::with_capacity(tasks.len());
    let mut seen_prefix = HashSet::with_capacity(tasks.len());

    for task in tasks {
        if let Some(existing) = target
            .iter_mut()
            .find(|e| !e.is_placeholder() && e.text.eq_ignore_ascii_case(&task.text))
        {
            if !task.children.is_empty() {
                merge_task_entries_front(&mut existing.children, task.children);
            }
            for alias in task.aliases {
                if !existing
                    .aliases
                    .iter()
                    .any(|a| a.eq_ignore_ascii_case(&alias))
                {
                    existing.aliases.push(alias);
                }
            }
            continue;
        }
        let key = task.text.to_ascii_lowercase();
        if seen_prefix.insert(key) {
            prefix.push(task);
        }
    }

    if !prefix.is_empty() {
        prefix.append(target);
        *target = prefix;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::env;
    use std::fs;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn make_temp_project_dir(label: &str) -> std::path::PathBuf {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = env::temp_dir().join(format!(
            "zpane_proj_tasks_{label}_{}_{nonce}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn test_discover_project_tasks_from_package_just_make_and_mise_in_temp_dir() {
        let dir = make_temp_project_dir("all");

        fs::write(
            dir.join("package.json"),
            r#"{
  "name": "demo-app",
  "scripts": {
    "dev": "vite",
    "build:prod": "vite build --mode production",
    "test": "vitest run"
  }
}"#,
        )
        .unwrap();

        fs::write(
            dir.join("justfile"),
            r#"
default := "debug"
export RUST_LOG := "info"
alias b := build

# Build release workspace
build:
    cargo build --release

[doc('Run test suite')]
test filter="":
    cargo test {{filter}}

@lint *args: build
    cargo clippy {{args}}

[private]
secret:
    echo hidden

_helper:
    echo private
"#,
        )
        .unwrap();

        fs::write(
            dir.join("Makefile"),
            concat!(
                ".PHONY: build test clean\n",
                "CC := gcc\n",
                "CFLAGS = -O2\n\n",
                "build: ## Build project binary\n",
                "\tcargo build\n\n",
                "# Clean build outputs\n",
                "clean:\n",
                "\trm -rf target\n\n",
                "%.o: %.c\n",
                "\t$(CC) -c $<\n\n",
                "$(OUT)/bin:\n",
                "\tmkdir -p $(OUT)\n",
            ),
        )
        .unwrap();

        fs::write(
            dir.join(".mise.toml"),
            r#"
[tools]
rust = "latest"

[tasks]
lint = "cargo clippy"
fmt = { run = "cargo fmt --check", description = "Check Rust formatting" }

[tasks.release]
description = "Build release artifacts"
run = "cargo build --release"

[tasks."db:migrate"]
run = "sqlx migrate run"

[tasks.release.env]
RUST_BACKTRACE = "1"
"#,
        )
        .unwrap();

        let specs = discover_project_task_specs(&dir);

        // 1. justfile recipes (excluding private/_helper and variable assignments)
        let just = specs.iter().find(|e| e.text == "just").expect("just spec");
        let just_names: Vec<&str> = just.children.iter().map(|c| c.text.as_str()).collect();
        assert_eq!(just_names, vec!["build", "test", "lint"]);
        assert_eq!(just.children[0].desc, "Build release workspace");
        assert_eq!(just.children[1].desc, "Run test suite");

        // 2. Makefile targets (excluding .PHONY, assignments, pattern rules, and $(VAR) targets)
        let make = specs.iter().find(|e| e.text == "make").expect("make spec");
        let make_names: Vec<&str> = make.children.iter().map(|c| c.text.as_str()).collect();
        assert_eq!(make_names, vec!["build", "clean"]);
        assert_eq!(make.children[0].desc, "Build project binary");
        assert_eq!(make.children[1].desc, "Clean build outputs");

        // 3. package.json scripts under npm (run), pnpm, and bun
        let npm = specs.iter().find(|e| e.text == "npm").expect("npm spec");
        let npm_run = npm.walk_chain(&["run"]).expect("npm run");
        let npm_script_names: Vec<&str> =
            npm_run.children.iter().map(|c| c.text.as_str()).collect();
        assert_eq!(npm_script_names, vec!["build:prod", "dev", "test"]);
        assert!(npm.walk_chain(&["run-script"]).is_some());

        for pm in ["pnpm", "bun"] {
            let entry = specs.iter().find(|e| e.text == pm).expect("pm spec");
            assert!(entry.children.iter().any(|c| c.text == "dev"));
            assert!(entry.children.iter().any(|c| c.text == "build:prod"));
            assert!(entry
                .walk_chain(&["run"])
                .is_some_and(|r| r.children.iter().any(|c| c.text == "test")));
        }

        // 4. .mise.toml tasks under mise (run / r), skipping [tools] and [tasks.release.env]
        let mise = specs.iter().find(|e| e.text == "mise").expect("mise spec");
        let mise_run = mise.walk_chain(&["r"]).expect("mise run via alias r");
        let mise_names: Vec<&str> = mise_run.children.iter().map(|c| c.text.as_str()).collect();
        assert_eq!(mise_names, vec!["lint", "fmt", "release", "db:migrate"]);
        assert_eq!(
            mise_run
                .children
                .iter()
                .find(|c| c.text == "fmt")
                .unwrap()
                .desc,
            "Check Rust formatting"
        );
        assert_eq!(
            mise_run
                .children
                .iter()
                .find(|c| c.text == "release")
                .unwrap()
                .desc,
            "Build release artifacts"
        );

        // 5. Direct chain lookup returns expected Pane 2/3 targets and ignores unrelated tools
        assert_eq!(
            project_task_entries_for_chain(&dir, &["npm", "run"])
                .iter()
                .map(|e| e.text.as_str())
                .collect::<Vec<_>>(),
            vec!["build:prod", "dev", "test"]
        );
        assert_eq!(
            project_task_entries_for_chain(&dir, &["mise", "run"])
                .iter()
                .map(|e| e.text.as_str())
                .collect::<Vec<_>>(),
            vec!["lint", "fmt", "release", "db:migrate"]
        );
        assert!(project_task_entries_for_chain(&dir, &["git"]).is_empty());

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_project_tasks_sanitizes_hostile_names_descriptions_and_enforces_bounds() {
        let dir = make_temp_project_dir("pentest");

        // Hostile package.json with option injection, shell metachars, traversal, BiDi/ANSI controls
        fs::write(
            dir.join("package.json"),
            serde_json::json!({
                "scripts": {
                    "-rf": "rm -rf /",
                    "../escape": "bad",
                    "cmd;id": "inject",
                    "$(whoami)": "subshell",
                    "good": "echo ok\x1b[31m\u{202e}\r\nevil"
                }
            })
            .to_string(),
        )
        .unwrap();

        // Oversized Makefile (> MAX_TASK_FILE_BYTES) must be rejected cleanly
        let huge_makefile = "a:\n\techo hi\n".repeat((MAX_TASK_FILE_BYTES as usize / 8) + 100);
        fs::write(dir.join("Makefile"), huge_makefile).unwrap();

        // justfile with more than MAX_TASK_ENTRIES recipes must cap cleanly
        let mut many_just = String::new();
        for i in 0..(MAX_TASK_ENTRIES + 50) {
            many_just.push_str(&format!("task_{i:04}:\n    echo {i}\n"));
        }
        fs::write(dir.join(".justfile"), many_just).unwrap();

        let npm_scripts = project_task_entries_for_chain(&dir, &["npm", "run"]);
        assert_eq!(npm_scripts.len(), 1);
        assert_eq!(npm_scripts[0].text, "good");
        assert!(!npm_scripts[0].desc.contains('\x1b'));
        assert!(!npm_scripts[0].desc.contains("[31m"));
        assert!(!npm_scripts[0].desc.contains('\u{202e}'));
        assert!(!npm_scripts[0].desc.contains('\n'));
        assert!(!npm_scripts[0].desc.contains('\r'));

        assert!(
            project_task_entries_for_chain(&dir, &["make"]).is_empty(),
            "oversized Makefile must be skipped safely"
        );

        let just_recipes = project_task_entries_for_chain(&dir, &["just"]);
        assert_eq!(just_recipes.len(), MAX_TASK_ENTRIES);

        let _ = fs::remove_dir_all(&dir);
    }
}
