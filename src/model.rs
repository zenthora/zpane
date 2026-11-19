use crate::discovery::ShellDialect;
use crate::sanitize::has_disallowed_control;
use crate::shell::quote_literal_for_dialect;
use serde::{Deserialize, Serialize};
use std::borrow::Cow;
use std::collections::HashSet;

const ALL_PARAMETER_SETS: &str = "__AllParameterSets";

/// Identity of one of the three cascading Miller panes arranged left-to-right.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Pane {
    First,
    Second,
    Third,
}

impl Pane {
    pub const ALL: [Pane; 3] = [Pane::First, Pane::Second, Pane::Third];

    pub const fn index(self) -> usize {
        match self {
            Pane::First => 0,
            Pane::Second => 1,
            Pane::Third => 2,
        }
    }
}

/// Behavioral role of an [`Entry`] inside a Pane.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum EntryRole {
    /// Emittable command, subcommand, parameter, flag, value, or variable token.
    #[default]
    Token,
    /// Synthetic Pane 1 container (e.g., `$env:` or `$`) that opens Pane 2 without emitting a token.
    ScopeRoot,
    /// Non-emittable UI indicator (loading status, valueless-flag hint, or custom-value prompt).
    Placeholder,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum EntryKind {
    History,
    Path,
    Env,
    Var,
    Cli,
    Func,
    Alias,
    Cmdlet,
    Subcmd,
    #[serde(alias = "Switch")]
    Flag,
    Param,
    Enum,
    Value,
    Arg,
    Loading,
}

impl EntryKind {
    pub fn badge(self) -> &'static str {
        match self {
            EntryKind::History => "[Hist]",
            EntryKind::Path => "[Path]",
            EntryKind::Env => "[Env]",
            EntryKind::Var => "[Var]",
            EntryKind::Cli => "[CLI]",
            EntryKind::Func => "[Func]",
            EntryKind::Alias => "[Alias]",
            EntryKind::Cmdlet => "[Cmdlet]",
            EntryKind::Subcmd => "[Subcmd]",
            EntryKind::Flag => "[Flag]",
            EntryKind::Param => "[Param]",
            EntryKind::Enum => "[Enum]",
            EntryKind::Value => "[Value]",
            EntryKind::Arg => "[Arg]",
            EntryKind::Loading => "[...]",
        }
    }
}

/// An entry (directory or file) inside the active working directory.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CwdEntry {
    pub name: String,
    #[serde(default)]
    pub is_dir: bool,
}

/// A live shell session variable captured from the host shell (e.g. PowerShell session vars).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionVar {
    pub name: String,
    #[serde(default)]
    pub value: String,
}

/// A live shell session function captured from the host shell (e.g. PowerShell `Function:` drive).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionFunc {
    pub name: String,
    #[serde(default)]
    pub desc: String,
}

/// Dynamic runtime context captured from the host shell wrapper and/or working directory.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RuntimeContext {
    /// Active Git branch name (e.g. `"main"`).
    #[serde(default)]
    pub git_branch: Option<String>,
    /// Local Git branches in the current repository (current branch first when known).
    #[serde(default)]
    pub git_branches: Vec<String>,
    /// Entries in the current working directory (folders first, then files).
    #[serde(default)]
    pub cwd_entries: Vec<CwdEntry>,
    /// Recent shell session commands (newest first) surfaced at the top of Pane 1.
    #[serde(default)]
    pub history: Vec<String>,
    /// Live session variables captured from the host shell (surfaced in the `$` Scope Root).
    #[serde(default)]
    pub session_vars: Vec<SessionVar>,
    /// Live session functions captured from the host shell (surfaced in Pane 1).
    #[serde(default)]
    pub session_funcs: Vec<SessionFunc>,
}

#[inline]
fn is_ascii_ident(s: &str) -> bool {
    let mut bytes = s.bytes();
    let Some(first) = bytes.next() else {
        return false;
    };
    (first.is_ascii_alphabetic() || first == b'_')
        && bytes.all(|b| b.is_ascii_alphanumeric() || b == b'_')
}

/// Validates and normalizes a shell variable token (`$env:X`, `$X`, `${env:X}`, special vars)
/// for `dialect`, rejecting injected metacharacters or control/newline/NUL characters.
pub fn sanitize_variable_token(token: &str, dialect: ShellDialect) -> Option<String> {
    if token.is_empty() || has_disallowed_control(token) || token.chars().any(char::is_whitespace) {
        return None;
    }
    if !token.starts_with('$') {
        return None;
    }

    match dialect {
        ShellDialect::PowerShell => {
            if matches!(
                token,
                "$_" | "$?" | "$^" | "$$" | "$true" | "$false" | "$null"
            ) {
                return Some(token.to_string());
            }
            if let Some(inner) = token.strip_prefix("${").and_then(|s| s.strip_suffix('}')) {
                if let Some((prefix, name)) = inner.split_once(':') {
                    if prefix.eq_ignore_ascii_case("env")
                        && !name.is_empty()
                        && name.chars().all(|c| {
                            c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '-' | '(' | ')')
                        })
                    {
                        return Some(token.to_string());
                    }
                    return None;
                }
                return is_ascii_ident(inner).then(|| token.to_string());
            }
            if token
                .get(..5)
                .is_some_and(|prefix| prefix.eq_ignore_ascii_case("$env:"))
            {
                let name = &token[5..];
                if name.is_empty() {
                    return None;
                }
                if name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
                    return Some(token.to_string());
                }
                if name
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '-' | '(' | ')'))
                {
                    return Some(format!("${{env:{name}}}"));
                }
                return None;
            }
            let rest = &token[1..];
            if let Some((scope, ident)) = rest.split_once(':') {
                if matches!(
                    scope.to_ascii_lowercase().as_str(),
                    "global" | "script" | "local" | "private"
                ) && is_ascii_ident(ident)
                {
                    return Some(token.to_string());
                }
                return None;
            }
            is_ascii_ident(rest).then(|| token.to_string())
        }
        ShellDialect::Posix => {
            if matches!(token, "$?" | "$!" | "$$" | "$_" | "$-" | "$#" | "$@" | "$*") {
                return Some(token.to_string());
            }
            let rest = &token[1..];
            if rest.len() == 1 && rest.as_bytes()[0].is_ascii_digit() {
                return Some(token.to_string());
            }
            if let Some(inner) = token.strip_prefix("${").and_then(|s| s.strip_suffix('}')) {
                return is_ascii_ident(inner).then(|| token.to_string());
            }
            is_ascii_ident(rest).then(|| token.to_string())
        }
        ShellDialect::Fish => {
            let rest = &token[1..];
            (!rest.is_empty() && rest.chars().all(|c| c.is_ascii_alphanumeric() || c == '_'))
                .then(|| token.to_string())
        }
        ShellDialect::Nushell => {
            if token == "$in" {
                return Some(token.to_string());
            }
            if let Some(rest) = token
                .strip_prefix("$env.")
                .or_else(|| token.strip_prefix("$nu."))
            {
                let valid_cell_path = !rest.is_empty()
                    && rest.split('.').all(|seg| {
                        !seg.is_empty()
                            && seg
                                .bytes()
                                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-'))
                    });
                return valid_cell_path.then(|| token.to_string());
            }
            let rest = &token[1..];
            let valid_ident = !rest.is_empty()
                && rest
                    .as_bytes()
                    .first()
                    .is_some_and(|b| b.is_ascii_alphabetic() || *b == b'_')
                && rest
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-'));
            valid_ident.then(|| token.to_string())
        }
    }
}

/// Returns `true` when `token` is a safe, syntactically valid variable reference for `dialect`.
pub fn is_safe_variable_token(token: &str, dialect: ShellDialect) -> bool {
    sanitize_variable_token(token, dialect).as_deref() == Some(token)
}

fn format_path_token(
    raw: &str,
    is_dir: bool,
    dialect: Option<ShellDialect>,
) -> Option<(String, String)> {
    if has_disallowed_control(raw) {
        return None;
    }
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }
    let display = if is_dir {
        format!("{}/", trimmed.trim_end_matches(['/', '\\']))
    } else {
        trimmed.to_string()
    };
    let token_src = if display.starts_with('-') {
        Cow::Owned(format!("./{display}"))
    } else {
        Cow::Borrowed(display.as_str())
    };
    let token = quote_literal_for_dialect(&token_src, dialect, true);
    let label = if is_dir {
        format!("📁 {display}")
    } else {
        display
    };
    Some((token, label))
}

fn push_git_branch_entry(
    raw: &str,
    is_current: bool,
    dialect: Option<ShellDialect>,
    seen: &mut HashSet<String>,
    out: &mut Vec<Entry>,
) {
    if has_disallowed_control(raw) {
        return;
    }
    let trimmed = raw.trim();
    if trimmed.is_empty() || trimmed.starts_with('-') || !seen.insert(trimmed.to_string()) {
        return;
    }
    let token = quote_literal_for_dialect(trimmed, dialect, true);
    let (label, desc) = if is_current {
        (
            format!("⎇ {trimmed} (current branch)"),
            "Current Git branch from runtime context",
        )
    } else {
        (format!("⎇ {trimmed}"), "Local Git branch")
    };
    out.push(Entry::new(token, label, EntryKind::Arg, desc));
}

impl RuntimeContext {
    /// Returns Git branch [`Entry`] values derived from `git_branch` and `git_branches` (current branch first).
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn git_branch_entries(&self) -> Vec<Entry> {
        self.git_branch_entries_impl(None)
    }

    /// Returns shell-dialect-quoted Git branch [`Entry`] values derived from `git_branch` and `git_branches`.
    pub fn git_branch_entries_for_dialect(&self, dialect: ShellDialect) -> Vec<Entry> {
        self.git_branch_entries_impl(Some(dialect))
    }

    fn git_branch_entries_impl(&self, dialect: Option<ShellDialect>) -> Vec<Entry> {
        let mut out = Vec::with_capacity(self.git_branches.len() + 1);
        let mut seen = HashSet::with_capacity(self.git_branches.len() + 1);

        if let Some(ref cur) = self.git_branch {
            push_git_branch_entry(cur, true, dialect, &mut seen, &mut out);
        }

        for branch in &self.git_branches {
            push_git_branch_entry(branch, false, dialect, &mut seen, &mut out);
        }

        out
    }

    /// Returns current directory file/folder [`Entry`] values, optionally filtered to directories only.
    pub fn cwd_path_entries(&self, dirs_only: bool) -> Vec<Entry> {
        self.cwd_path_entries_impl(dirs_only, None)
    }

    /// Returns shell-dialect-quoted current directory file/folder [`Entry`] values.
    pub fn cwd_path_entries_for_dialect(
        &self,
        dirs_only: bool,
        dialect: ShellDialect,
    ) -> Vec<Entry> {
        self.cwd_path_entries_impl(dirs_only, Some(dialect))
    }

    fn cwd_path_entries_impl(&self, dirs_only: bool, dialect: Option<ShellDialect>) -> Vec<Entry> {
        self.cwd_entries
            .iter()
            .filter(|e| !dirs_only || e.is_dir)
            .filter_map(|e| {
                let (token, label) = format_path_token(&e.name, e.is_dir, dialect)?;
                let kind_desc = if e.is_dir {
                    "Current directory folder"
                } else {
                    "Current directory file"
                };
                Some(Entry::new(token, label, EntryKind::Path, kind_desc))
            })
            .collect()
    }
}

/// A row displayed inside a [`Pane`], representing a navigable container,
/// an emittable command token, or a non-emittable placeholder.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Entry {
    pub text: String,
    pub label: String,
    pub kind: EntryKind,
    #[serde(default)]
    pub role: EntryRole,
    pub desc: String,
    #[serde(default)]
    pub children: Vec<Entry>,
    #[serde(default)]
    pub parameter_sets: Vec<String>,
    #[serde(default)]
    pub mandatory_sets: Vec<String>,
    #[serde(default)]
    pub position: Option<i32>,
    #[serde(default)]
    pub accepts_pipeline: bool,
    #[serde(default)]
    pub accepts_pipeline_by_property_name: bool,
    #[serde(default)]
    pub validate_range: Option<String>,
    #[serde(default)]
    pub help_file: Option<String>,
    #[serde(default)]
    pub aliases: Vec<String>,
    #[serde(default)]
    pub has_literal: bool,
}

impl Entry {
    pub fn new(
        text: impl Into<String>,
        label: impl Into<String>,
        kind: EntryKind,
        desc: impl Into<String>,
    ) -> Self {
        Self {
            text: text.into(),
            label: label.into(),
            kind,
            role: EntryRole::Token,
            desc: desc.into(),
            children: Vec::new(),
            parameter_sets: Vec::new(),
            mandatory_sets: Vec::new(),
            position: None,
            accepts_pipeline: false,
            accepts_pipeline_by_property_name: false,
            validate_range: None,
            help_file: None,
            aliases: Vec::new(),
            has_literal: false,
        }
    }

    pub fn scope_root(
        text: impl Into<String>,
        label: impl Into<String>,
        kind: EntryKind,
        desc: impl Into<String>,
    ) -> Self {
        Self {
            role: EntryRole::ScopeRoot,
            ..Self::new(text, label, kind, desc)
        }
    }

    pub fn placeholder(label: impl Into<String>, kind: EntryKind, desc: impl Into<String>) -> Self {
        Self {
            role: EntryRole::Placeholder,
            ..Self::new("", label, kind, desc)
        }
    }

    pub fn with_parameter_sets(mut self, sets: Vec<String>) -> Self {
        self.parameter_sets = sets;
        self
    }

    pub fn with_mandatory_sets(mut self, sets: Vec<String>) -> Self {
        self.mandatory_sets = sets;
        self
    }

    pub fn with_position(mut self, position: Option<i32>) -> Self {
        self.position = position;
        self
    }

    pub fn with_pipeline(
        mut self,
        accepts_pipeline: bool,
        accepts_pipeline_by_property_name: bool,
    ) -> Self {
        self.accepts_pipeline = accepts_pipeline;
        self.accepts_pipeline_by_property_name = accepts_pipeline_by_property_name;
        self
    }

    pub fn with_validate_range(mut self, range: Option<String>) -> Self {
        self.validate_range = range;
        self
    }

    pub fn with_help_file(mut self, help_file: Option<String>) -> Self {
        self.help_file = help_file;
        self
    }

    /// Returns `true` when this Entry is mandatory in `active_sets` (or in at least one
    /// Parameter Set when `active_sets` is `None` or empty).
    pub fn is_mandatory_for_sets(&self, active_sets: Option<&HashSet<String>>) -> bool {
        if self.mandatory_sets.is_empty() {
            return false;
        }
        if self.mandatory_sets.iter().any(|s| s == ALL_PARAMETER_SETS) {
            return true;
        }
        match active_sets {
            Some(sets) if !sets.is_empty() => self.mandatory_sets.iter().any(|m| sets.contains(m)),
            _ => true,
        }
    }

    /// Returns `true` when this Entry is mandatory for the comma-separated active Parameter
    /// Set display label used in [`RenderState`].
    pub fn is_mandatory_for_set_label(&self, active_label: Option<&str>) -> bool {
        if self.mandatory_sets.is_empty() {
            return false;
        }
        if self.mandatory_sets.iter().any(|s| s == ALL_PARAMETER_SETS) {
            return true;
        }
        match active_label.map(str::trim).filter(|s| !s.is_empty()) {
            Some(label) => self
                .mandatory_sets
                .iter()
                .any(|m| label.split(", ").any(|s| s == m)),
            None => true,
        }
    }

    pub fn with_aliases(mut self, aliases: Vec<String>) -> Self {
        self.aliases = aliases;
        self
    }

    pub fn with_literal(mut self, has_literal: bool) -> Self {
        self.has_literal = has_literal;
        self
    }

    pub fn is_emittable(&self) -> bool {
        self.role == EntryRole::Token && !self.text.is_empty()
    }

    pub fn is_scope_root(&self) -> bool {
        self.role == EntryRole::ScopeRoot
    }

    pub fn is_placeholder(&self) -> bool {
        self.role == EntryRole::Placeholder
    }

    pub fn is_loading(&self) -> bool {
        self.role == EntryRole::Placeholder && self.kind == EntryKind::Loading
    }

    /// Walks nested `children` along `chain`, matching each segment case-insensitively against a
    /// child's `text` and falling back to the structured aliases of `Subcmd` children.
    ///
    /// Returns `self` for an empty chain and `None` when any segment has no match.
    pub fn walk_chain<S: AsRef<str>>(&self, chain: &[S]) -> Option<&Entry> {
        let mut cur = self;
        for seg in chain {
            let seg = seg.as_ref();
            cur = cur
                .children
                .iter()
                .find(|c| c.text.eq_ignore_ascii_case(seg))
                .or_else(|| {
                    cur.children.iter().find(|c| {
                        c.kind == EntryKind::Subcmd
                            && c.aliases.iter().any(|a| a.eq_ignore_ascii_case(seg))
                    })
                })?;
        }
        Some(cur)
    }

    /// Whether accepting this Pane 2 Entry commits it immediately without requiring a Pane 3 value.
    pub fn commits_without_value(&self) -> bool {
        self.role == EntryRole::Token
            && self.kind == EntryKind::Flag
            && (self.children.is_empty() || self.children.iter().any(Entry::is_placeholder))
    }

    /// Whether this parameter or flag Entry matches a raw command token (`-Id`, `--id`, `-Id:value`, `--extension=rs`)
    /// by exact token name or structured Parameter Alias, case-insensitively.
    pub fn matches_parameter_token(&self, token: &str) -> bool {
        if !self.is_emittable() || !matches!(self.kind, EntryKind::Param | EntryKind::Flag) {
            return false;
        }
        let Some(after_dash) = token.strip_prefix("--").or_else(|| token.strip_prefix('-')) else {
            return false;
        };
        let clean = after_dash.split([':', '=']).next().unwrap_or("");
        if clean.is_empty() {
            return false;
        }

        fn norm(raw: &str) -> &str {
            raw.strip_prefix("--")
                .or_else(|| raw.strip_prefix('-'))
                .unwrap_or(raw)
                .trim_end_matches(['=', ':'])
        }

        norm(&self.text).eq_ignore_ascii_case(clean)
            || self
                .aliases
                .iter()
                .any(|alias| norm(alias).eq_ignore_ascii_case(clean))
    }

    /// Computes the active PowerShell Parameter Sets implied by `tokens`, using exact
    /// parameter flags and structured Parameter Aliases. Only cmdlets, functions, and
    /// command aliases carry Parameter Set metadata.
    pub fn active_parameter_sets(&self, tokens: &[String]) -> Option<HashSet<String>> {
        if tokens.len() <= 1
            || !matches!(
                self.kind,
                EntryKind::Cmdlet | EntryKind::Func | EntryKind::Alias
            )
        {
            return None;
        }
        let mut active_sets: Option<HashSet<String>> = None;

        for tok in tokens.iter().skip(1) {
            if tok == "--" || tok == "--%" {
                break;
            }
            if !tok.starts_with('-') {
                continue;
            }

            for param in &self.children {
                if param.matches_parameter_token(tok) {
                    active_sets = match active_sets {
                        None => {
                            let mut first = HashSet::new();
                            for s in &param.parameter_sets {
                                if s.as_str() != ALL_PARAMETER_SETS {
                                    first.insert(s.clone());
                                }
                            }
                            if first.is_empty() {
                                None
                            } else {
                                Some(first)
                            }
                        }
                        Some(mut cur) => {
                            if cur
                                .iter()
                                .any(|s| param.parameter_sets.iter().any(|p| p == s))
                            {
                                cur.retain(|s| param.parameter_sets.iter().any(|p| p == s));
                            }
                            Some(cur)
                        }
                    };
                    break;
                }
            }
        }

        active_sets
    }

    /// Formats the sorted display label for the active Parameter Sets implied by `tokens`
    /// without cloning child [`Entry`] values.
    pub(crate) fn active_parameter_set_label(&self, tokens: &[String]) -> Option<String> {
        let active_sets = self.active_parameter_sets(tokens)?;
        Some(format_parameter_set_label(&active_sets))
    }

    /// Returns this Entry's Pane 2 children narrowed to the active Parameter Sets implied
    /// by `tokens`, along with a sorted display label for the active Parameter Set(s).
    /// Mandatory Parameters for the active Parameter Set(s) are stably prioritized first.
    pub fn children_for_tokens(&self, tokens: &[String]) -> (Vec<Entry>, Option<String>) {
        let active_sets = self.active_parameter_sets(tokens);
        let label = active_sets.as_ref().map(format_parameter_set_label);

        let mut out = if let Some(ref sets) = active_sets {
            let mut filtered = Vec::with_capacity(self.children.len());
            for p in &self.children {
                if p.parameter_sets.is_empty()
                    || p.parameter_sets
                        .iter()
                        .any(|s| s == ALL_PARAMETER_SETS || sets.contains(s))
                {
                    filtered.push(p.clone());
                }
            }
            if filtered.is_empty() {
                self.children.clone()
            } else {
                filtered
            }
        } else {
            self.children.clone()
        };

        if out.iter().any(|p| !p.mandatory_sets.is_empty()) {
            let sets_ref = active_sets.as_ref();
            out.sort_by_key(|p| !p.is_mandatory_for_sets(sets_ref));
        }

        (out, label)
    }
}

fn format_parameter_set_label(sets: &HashSet<String>) -> String {
    if sets.len() == 1 {
        return sets.iter().next().cloned().unwrap_or_default();
    }
    let mut sorted: Vec<&str> = sets.iter().map(String::as_str).collect();
    sorted.sort_unstable();
    sorted.join(", ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_entry_roles_and_flag_commit_semantics() {
        let root = Entry::scope_root("$env:", "$env:Variables", EntryKind::Env, "Env");
        assert!(root.is_scope_root());
        assert!(!root.is_emittable());
        assert!(!root.is_placeholder());

        let loading = Entry::placeholder("Loading...", EntryKind::Loading, "");
        assert!(loading.is_placeholder());
        assert!(loading.is_loading());
        assert!(!loading.is_emittable());

        let cli_flag = Entry::new("--all", "--all", EntryKind::Flag, "All");
        assert!(cli_flag.is_emittable());
        assert!(cli_flag.commits_without_value());

        let mut ps_switch = Entry::new("-Verbose", "-Verbose", EntryKind::Flag, "Verbose");
        ps_switch.children = vec![
            Entry::placeholder("(Flag - No Value Needed)", EntryKind::Flag, ""),
            Entry::new(":$true", ":$true", EntryKind::Value, "Enable"),
        ];
        assert!(ps_switch.commits_without_value());

        let mut param = Entry::new("-Format", "-Format", EntryKind::Param, "Format");
        param.children = vec![Entry::new("json", "json", EntryKind::Enum, "JSON")];
        assert!(!param.commits_without_value());
    }

    #[test]
    fn test_children_for_tokens_narrows_parameter_sets_and_keeps_universal() {
        let mut cmd = Entry::new("Get-Process", "Get-Process", EntryKind::Cmdlet, "");
        cmd.children = vec![
            Entry::new("-Id", "-Id (-PID)", EntryKind::Param, "")
                .with_aliases(vec!["PID".to_string()])
                .with_parameter_sets(vec!["Id".to_string(), "IdWithUser".to_string()]),
            Entry::new("-Name", "-Name", EntryKind::Param, "")
                .with_parameter_sets(vec!["Name".to_string()]),
            Entry::new("-Verbose", "-Verbose", EntryKind::Flag, "")
                .with_parameter_sets(vec![ALL_PARAMETER_SETS.to_string()]),
        ];

        let (all_children, label_none) = cmd.children_for_tokens(&["Get-Process".to_string()]);
        assert_eq!(all_children.len(), 3);
        assert_eq!(label_none, None);

        let (narrowed, label_id) =
            cmd.children_for_tokens(&["Get-Process".to_string(), "-pid".to_string()]);
        let names: Vec<&str> = narrowed.iter().map(|e| e.text.as_str()).collect();
        assert_eq!(names, vec!["-Id", "-Verbose"]);
        assert_eq!(label_id.as_deref(), Some("Id, IdWithUser"));
    }

    #[test]
    fn test_serde_alias_switch_deserializes_as_flag() {
        let json = r#"{"text":"-Verbose","label":"-Verbose","kind":"Switch","desc":"d"}"#;
        let entry: Entry = serde_json::from_str(json).expect("deserialize legacy Switch kind");
        assert_eq!(entry.kind, EntryKind::Flag);
        assert_eq!(entry.role, EntryRole::Token);
    }

    #[test]
    fn test_runtime_context_builds_branch_and_cwd_entries() {
        let ctx = RuntimeContext {
            git_branch: Some("feat/roadmap".to_string()),
            git_branches: vec!["main".to_string(), "feat/roadmap".to_string()],
            cwd_entries: vec![
                CwdEntry {
                    name: "src".to_string(),
                    is_dir: true,
                },
                CwdEntry {
                    name: "my file.txt".to_string(),
                    is_dir: false,
                },
            ],
            history: vec!["cargo test".to_string()],
            ..RuntimeContext::default()
        };

        let branches = ctx.git_branch_entries();
        let branch_texts: Vec<&str> = branches.iter().map(|e| e.text.as_str()).collect();
        assert_eq!(branch_texts, vec!["feat/roadmap", "main"]);
        assert!(branches[0].label.contains("current branch"));

        let all_paths = ctx.cwd_path_entries(false);
        assert_eq!(all_paths.len(), 2);
        assert_eq!(all_paths[0].text, "src/");
        assert_eq!(all_paths[0].kind, EntryKind::Path);
        assert_eq!(all_paths[1].text, "\"my file.txt\"");

        let only_dirs = ctx.cwd_path_entries(true);
        assert_eq!(only_dirs.len(), 1);
        assert_eq!(only_dirs[0].text, "src/");
    }

    #[test]
    fn test_matches_parameter_token_handles_equals_and_colon_attachments() {
        let carapace_param = Entry::new("--extension=", "-e, --extension=", EntryKind::Param, "")
            .with_aliases(vec!["e".to_string()]);
        assert!(carapace_param.matches_parameter_token("--extension=rs"));
        assert!(carapace_param.matches_parameter_token("--extension="));
        assert!(carapace_param.matches_parameter_token("--extension"));
        assert!(carapace_param.matches_parameter_token("-e"));
        assert!(!carapace_param.matches_parameter_token("rs"));

        let ps_param = Entry::new("-Id", "-Id (-PID)", EntryKind::Param, "")
            .with_aliases(vec!["PID".to_string()]);
        assert!(ps_param.matches_parameter_token("-Id:42"));
        assert!(ps_param.matches_parameter_token("-PID:42"));
    }

    #[test]
    fn test_parameter_matching_excludes_subcmds_and_stops_at_end_of_options() {
        let subcmd = Entry::new("version", "version", EntryKind::Subcmd, "Print version")
            .with_aliases(vec!["v".to_string()]);
        assert!(!subcmd.matches_parameter_token("--version"));
        assert!(!subcmd.matches_parameter_token("-v"));

        let arg = Entry::new("Name", "Name", EntryKind::Arg, "Positional arg");
        assert!(!arg.matches_parameter_token("-Name"));

        let param = Entry::new("-Id", "-Id", EntryKind::Param, "Process ID");
        assert!(!param.matches_parameter_token("---Id"));
        assert!(!param.matches_parameter_token("--"));
        assert!(!param.matches_parameter_token("-"));

        let mut cmd = Entry::new("Get-Process", "Get-Process", EntryKind::Cmdlet, "");
        cmd.children = vec![
            Entry::new("-Id", "-Id", EntryKind::Param, "")
                .with_parameter_sets(vec!["Id".to_string()]),
            Entry::new("-Name", "-Name", EntryKind::Param, "")
                .with_parameter_sets(vec!["Name".to_string()]),
        ];

        assert_eq!(
            cmd.active_parameter_sets(&[
                "Get-Process".to_string(),
                "--".to_string(),
                "-Name".to_string()
            ]),
            None
        );
        assert_eq!(
            cmd.active_parameter_sets(&[
                "Get-Process".to_string(),
                "--%".to_string(),
                "-Id".to_string()
            ]),
            None
        );
    }

    #[test]
    fn test_cwd_path_entries_skips_blank_names_and_escapes_embedded_quotes() {
        let ctx = RuntimeContext {
            cwd_entries: vec![
                CwdEntry {
                    name: "   ".to_string(),
                    is_dir: false,
                },
                CwdEntry {
                    name: r#"dir "1""#.to_string(),
                    is_dir: true,
                },
                CwdEntry {
                    name: r#"my "quote".txt"#.to_string(),
                    is_dir: false,
                },
            ],
            ..RuntimeContext::default()
        };

        let paths = ctx.cwd_path_entries(false);
        let texts: Vec<&str> = paths.iter().map(|e| e.text.as_str()).collect();
        assert_eq!(texts, vec![r#"'dir "1"/'"#, r#"'my "quote".txt'"#]);
    }

    #[test]
    fn test_pentest_cwd_and_git_branch_neutralize_shell_metacharacters_and_controls() {
        let hostile_names = [
            "$(id)",
            "`id`",
            "'; evil; '",
            r#""$(evil)""#,
            "$env:x; evil",
            "; | & > <",
        ];

        let mut cwd_entries: Vec<CwdEntry> = hostile_names
            .iter()
            .map(|name| CwdEntry {
                name: (*name).to_string(),
                is_dir: false,
            })
            .collect();
        // Control/newline/NUL entries must be rejected, and leading `-` prefixed with `./`.
        cwd_entries.extend([
            CwdEntry {
                name: "bad\nid.txt".to_string(),
                is_dir: false,
            },
            CwdEntry {
                name: "bad\rid.txt".to_string(),
                is_dir: false,
            },
            CwdEntry {
                name: "bad\0id.txt".to_string(),
                is_dir: false,
            },
            CwdEntry {
                name: "\x1b]52;c;YQ==\x07".to_string(),
                is_dir: false,
            },
            CwdEntry {
                name: "rtl\u{061C}a\u{200E}b\u{200F}c.txt".to_string(),
                is_dir: false,
            },
            CwdEntry {
                name: "--checkpoint-action=exec=id".to_string(),
                is_dir: false,
            },
            CwdEntry {
                name: "-rf; $(id)".to_string(),
                is_dir: false,
            },
        ]);

        let mut git_branches: Vec<String> =
            hostile_names.iter().map(|s| (*s).to_string()).collect();
        git_branches.extend([
            "feat\nid".to_string(),
            "feat\rid".to_string(),
            "feat\0id".to_string(),
            "--upload-pack=evil".to_string(),
        ]);

        let ctx = RuntimeContext {
            git_branch: Some("$(whoami)".to_string()),
            git_branches,
            cwd_entries,
            history: Vec::new(),
            ..RuntimeContext::default()
        };

        // PowerShell dialect
        let ps_paths: Vec<String> = ctx
            .cwd_path_entries_for_dialect(false, ShellDialect::PowerShell)
            .into_iter()
            .map(|e| e.text)
            .collect();
        assert_eq!(
            ps_paths,
            vec![
                "'$(id)'",
                "'`id`'",
                "'''; evil; '''",
                r#"'"$(evil)"'"#,
                "'$env:x; evil'",
                "'; | & > <'",
                "./--checkpoint-action=exec=id",
                "'./-rf; $(id)'",
            ]
        );

        // POSIX dialect
        let posix_paths: Vec<String> = ctx
            .cwd_path_entries_for_dialect(false, ShellDialect::Posix)
            .into_iter()
            .map(|e| e.text)
            .collect();
        assert_eq!(
            posix_paths,
            vec![
                "'$(id)'",
                "'`id`'",
                r"''\''; evil; '\'''",
                r#"'"$(evil)"'"#,
                "'$env:x; evil'",
                "'; | & > <'",
                "./--checkpoint-action=exec=id",
                "'./-rf; $(id)'",
            ]
        );

        // Fish dialect
        let fish_paths: Vec<String> = ctx
            .cwd_path_entries_for_dialect(false, ShellDialect::Fish)
            .into_iter()
            .map(|e| e.text)
            .collect();
        assert_eq!(
            fish_paths,
            vec![
                "'$(id)'",
                "'`id`'",
                r"'\'; evil; \''",
                r#"'"$(evil)"'"#,
                "'$env:x; evil'",
                "'; | & > <'",
                "./--checkpoint-action=exec=id",
                "'./-rf; $(id)'",
            ]
        );

        // Git branches across all dialects
        let ps_branches: Vec<String> = ctx
            .git_branch_entries_for_dialect(ShellDialect::PowerShell)
            .into_iter()
            .map(|e| e.text)
            .collect();
        assert_eq!(
            ps_branches,
            vec![
                "'$(whoami)'",
                "'$(id)'",
                "'`id`'",
                "'''; evil; '''",
                r#"'"$(evil)"'"#,
                "'$env:x; evil'",
                "'; | & > <'",
            ]
        );
        let posix_branches: Vec<String> = ctx
            .git_branch_entries_for_dialect(ShellDialect::Posix)
            .into_iter()
            .map(|e| e.text)
            .collect();
        assert_eq!(
            posix_branches,
            vec![
                "'$(whoami)'",
                "'$(id)'",
                "'`id`'",
                r"''\''; evil; '\'''",
                r#"'"$(evil)"'"#,
                "'$env:x; evil'",
                "'; | & > <'",
            ]
        );
        let fish_branches: Vec<String> = ctx
            .git_branch_entries_for_dialect(ShellDialect::Fish)
            .into_iter()
            .map(|e| e.text)
            .collect();
        assert_eq!(
            fish_branches,
            vec![
                "'$(whoami)'",
                "'$(id)'",
                "'`id`'",
                r"'\'; evil; \''",
                r#"'"$(evil)"'"#,
                "'$env:x; evil'",
                "'; | & > <'",
            ]
        );
    }

    #[test]
    fn entry_walk_chain_matches_text_and_subcommand_aliases_case_insensitively() {
        let add = Entry::new("add", "add", EntryKind::Subcmd, "Add a remote")
            .with_aliases(vec!["new".to_string()]);
        let mut remote = Entry::new("remote", "remote", EntryKind::Subcmd, "Manage remotes")
            .with_aliases(vec!["rm".to_string()]);
        remote.children = vec![
            add,
            Entry::new("--verbose", "--verbose", EntryKind::Flag, "Be verbose")
                .with_aliases(vec!["-v".to_string()]),
        ];
        let mut git = Entry::new("git", "git", EntryKind::Cli, "Git VCS");
        git.children = vec![remote];

        let no_segments: [&str; 0] = [];
        assert_eq!(
            git.walk_chain(&no_segments).map(|e| e.text.as_str()),
            Some("git")
        );
        assert_eq!(
            git.walk_chain(&["REMOTE"]).map(|e| e.text.as_str()),
            Some("remote")
        );
        assert_eq!(
            git.walk_chain(&["rm"]).map(|e| e.text.as_str()),
            Some("remote")
        );
        assert_eq!(
            git.walk_chain(&["RM".to_string(), "New".to_string()])
                .map(|e| e.text.as_str()),
            Some("add")
        );
        assert!(git.walk_chain(&["remote", "-v"]).is_none());
        assert!(git.walk_chain(&["missing"]).is_none());
    }

    #[test]
    fn test_pentest_variable_token_validation_and_parameter_set_intersection() {
        for dialect in [
            ShellDialect::PowerShell,
            ShellDialect::Posix,
            ShellDialect::Fish,
            ShellDialect::Nushell,
        ] {
            assert!(!is_safe_variable_token("$env:x; evil", dialect));
            assert!(!is_safe_variable_token("$x; evil", dialect));
            assert!(!is_safe_variable_token("$(id)", dialect));
            assert!(!is_safe_variable_token("$`id`", dialect));
            assert!(!is_safe_variable_token("$x\nevil", dialect));
            assert!(!is_safe_variable_token("$x\revil", dialect));
            assert!(!is_safe_variable_token("$x\0evil", dialect));
        }

        assert!(is_safe_variable_token(
            "$env:USER",
            ShellDialect::PowerShell
        ));
        assert!(is_safe_variable_token("$_", ShellDialect::PowerShell));
        assert_eq!(
            sanitize_variable_token("$env:ProgramFiles(x86)", ShellDialect::PowerShell).as_deref(),
            Some("${env:ProgramFiles(x86)}")
        );
        assert!(is_safe_variable_token("$USER", ShellDialect::Posix));
        assert!(is_safe_variable_token("$?", ShellDialect::Posix));
        assert!(!is_safe_variable_token("$?", ShellDialect::Fish));
        assert!(is_safe_variable_token("$status", ShellDialect::Fish));
        assert!(is_safe_variable_token("$env.PATH", ShellDialect::Nushell));
        assert!(is_safe_variable_token(
            "$nu.config-path",
            ShellDialect::Nushell
        ));
        assert!(is_safe_variable_token("$in", ShellDialect::Nushell));
        assert!(is_safe_variable_token("$my_var", ShellDialect::Nushell));
        assert!(!is_safe_variable_token("$env:PATH", ShellDialect::Nushell));
        assert!(!is_safe_variable_token("$?", ShellDialect::Nushell));
        assert!(!is_safe_variable_token("$env.", ShellDialect::Nushell));
        assert!(!is_safe_variable_token(
            "$env.PATH;id",
            ShellDialect::Nushell
        ));

        let mut cmd = Entry::new("Get-Item", "Get-Item", EntryKind::Cmdlet, "");
        cmd.children = vec![
            Entry::new("-Path", "-Path", EntryKind::Param, "")
                .with_parameter_sets(vec!["Path".to_string(), "PathForce".to_string()]),
            Entry::new("-Force", "-Force", EntryKind::Flag, "")
                .with_parameter_sets(vec!["PathForce".to_string(), "LiteralForce".to_string()]),
            Entry::new("-LiteralPath", "-LiteralPath", EntryKind::Param, "")
                .with_parameter_sets(vec!["LiteralForce".to_string()]),
        ];
        let tokens = vec![
            "Get-Item".to_string(),
            "-Path".to_string(),
            "-Force".to_string(),
        ];
        assert_eq!(
            cmd.active_parameter_set_label(&tokens).as_deref(),
            Some("PathForce")
        );
        let (children, label) = cmd.children_for_tokens(&tokens);
        assert_eq!(label.as_deref(), Some("PathForce"));
        let names: Vec<&str> = children.iter().map(|e| e.text.as_str()).collect();
        assert_eq!(names, vec!["-Path", "-Force"]);
    }

    #[test]
    fn test_entry_and_runtime_context_backward_compat_and_dynamic_mandatory_sorting() {
        // 1. Legacy JSON without new CLR metadata / session fields deserializes cleanly
        let legacy_entry_json = r#"{"text":"-Name","label":"-Name","kind":"Param","desc":"d"}"#;
        let legacy_entry: Entry =
            serde_json::from_str(legacy_entry_json).expect("deserialize legacy Entry");
        assert!(legacy_entry.mandatory_sets.is_empty());
        assert_eq!(legacy_entry.position, None);
        assert!(!legacy_entry.accepts_pipeline);
        assert!(!legacy_entry.accepts_pipeline_by_property_name);
        assert_eq!(legacy_entry.validate_range, None);
        assert_eq!(legacy_entry.help_file, None);
        assert!(!legacy_entry.is_mandatory_for_sets(None));

        let legacy_ctx_json = r#"{"git_branch":"main","history":["ls"]}"#;
        let legacy_ctx: RuntimeContext =
            serde_json::from_str(legacy_ctx_json).expect("deserialize legacy RuntimeContext");
        assert!(legacy_ctx.session_vars.is_empty());
        assert!(legacy_ctx.session_funcs.is_empty());

        // 2. Parameter mandatory in SetA but optional in SetB updates dynamically and sorts first
        let mut cmd = Entry::new("Invoke-Thing", "Invoke-Thing", EntryKind::Cmdlet, "");
        cmd.children = vec![
            Entry::new("-OptionalFirst", "-OptionalFirst", EntryKind::Param, "")
                .with_parameter_sets(vec!["__AllParameterSets".to_string()]),
            Entry::new("-Target", "-Target", EntryKind::Param, "")
                .with_parameter_sets(vec!["SetA".to_string(), "SetB".to_string()])
                .with_mandatory_sets(vec!["SetA".to_string()])
                .with_position(Some(0))
                .with_pipeline(true, false)
                .with_validate_range(Some("1..10".to_string())),
            Entry::new("-ModeA", "-ModeA", EntryKind::Flag, "")
                .with_parameter_sets(vec!["SetA".to_string()]),
            Entry::new("-ModeB", "-ModeB", EntryKind::Flag, "")
                .with_parameter_sets(vec!["SetB".to_string()])
                .with_mandatory_sets(vec!["SetB".to_string()]),
        ];

        // When no parameter set is active yet, mandatory params sort ahead of optional ones
        let (initial, _) = cmd.children_for_tokens(&["Invoke-Thing".to_string()]);
        let initial_names: Vec<&str> = initial.iter().map(|e| e.text.as_str()).collect();
        assert_eq!(
            initial_names,
            vec!["-Target", "-ModeB", "-OptionalFirst", "-ModeA"]
        );

        // Selecting -ModeB narrows to SetB: -Target is now optional in SetB, -ModeB stays mandatory
        let (set_b, label_b) =
            cmd.children_for_tokens(&["Invoke-Thing".to_string(), "-ModeB".to_string()]);
        assert_eq!(label_b.as_deref(), Some("SetB"));
        let set_b_names: Vec<&str> = set_b.iter().map(|e| e.text.as_str()).collect();
        assert_eq!(set_b_names, vec!["-ModeB", "-OptionalFirst", "-Target"]);
        let b_sets: HashSet<String> = ["SetB".to_string()].into_iter().collect();
        assert!(!set_b[2].is_mandatory_for_sets(Some(&b_sets)));
        assert!(set_b[0].is_mandatory_for_sets(Some(&b_sets)));

        // Selecting -ModeA narrows to SetA: -Target is mandatory in SetA and sorts first
        let (set_a, label_a) =
            cmd.children_for_tokens(&["Invoke-Thing".to_string(), "-ModeA".to_string()]);
        assert_eq!(label_a.as_deref(), Some("SetA"));
        let set_a_names: Vec<&str> = set_a.iter().map(|e| e.text.as_str()).collect();
        assert_eq!(set_a_names, vec!["-Target", "-OptionalFirst", "-ModeA"]);
        let a_sets: HashSet<String> = ["SetA".to_string()].into_iter().collect();
        assert!(set_a[0].is_mandatory_for_sets(Some(&a_sets)));
    }
}
