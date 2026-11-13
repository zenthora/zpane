use serde::{Deserialize, Serialize};
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

    /// Whether accepting this Pane 2 Entry commits it immediately without requiring a Pane 3 value.
    pub fn commits_without_value(&self) -> bool {
        self.role == EntryRole::Token
            && self.kind == EntryKind::Flag
            && (self.children.is_empty() || self.children.iter().any(Entry::is_placeholder))
    }

    /// Whether this parameter or flag Entry matches a raw command token (`-Id`, `--id`, `-Id:value`)
    /// by exact token name or structured Parameter Alias, case-insensitively.
    pub fn matches_parameter_token(&self, token: &str) -> bool {
        if !token.starts_with('-') {
            return false;
        }
        let clean = token
            .trim_start_matches('-')
            .split(':')
            .next()
            .unwrap_or("");
        if clean.is_empty() {
            return false;
        }

        self.text
            .trim_start_matches('-')
            .eq_ignore_ascii_case(clean)
            || self
                .aliases
                .iter()
                .any(|alias| alias.trim_start_matches('-').eq_ignore_ascii_case(clean))
    }

    /// Computes the active PowerShell Parameter Sets implied by `tokens`, using exact
    /// parameter flags and structured Parameter Aliases. Only cmdlets, functions, and
    /// command aliases carry Parameter Set metadata.
    pub fn active_parameter_sets(&self, tokens: &[String]) -> Option<HashSet<String>> {
        if !matches!(
            self.kind,
            EntryKind::Cmdlet | EntryKind::Func | EntryKind::Alias
        ) {
            return None;
        }
        let mut active_sets: Option<HashSet<String>> = None;

        for tok in tokens.iter().skip(1) {
            if !tok.starts_with('-') {
                continue;
            }

            for param in &self.children {
                if param.matches_parameter_token(tok) {
                    let non_all: HashSet<String> = param
                        .parameter_sets
                        .iter()
                        .filter(|s| s.as_str() != ALL_PARAMETER_SETS)
                        .cloned()
                        .collect();

                    if !non_all.is_empty() {
                        active_sets = match active_sets {
                            None => Some(non_all),
                            Some(cur) => {
                                let inter: HashSet<String> =
                                    cur.intersection(&non_all).cloned().collect();
                                if inter.is_empty() {
                                    Some(cur)
                                } else {
                                    Some(inter)
                                }
                            }
                        };
                    }
                    break;
                }
            }
        }

        active_sets
    }

    /// Returns this Entry's Pane 2 children narrowed to the active Parameter Sets implied
    /// by `tokens`, along with a sorted display label for the active Parameter Set(s).
    pub fn children_for_tokens(&self, tokens: &[String]) -> (Vec<Entry>, Option<String>) {
        let active_sets = self.active_parameter_sets(tokens);
        let label = active_sets.as_ref().map(|sets| {
            let mut sorted: Vec<String> = sets.iter().cloned().collect();
            sorted.sort();
            sorted.join(", ")
        });

        if let Some(sets) = active_sets {
            let filtered: Vec<Entry> = self
                .children
                .iter()
                .filter(|p| {
                    p.parameter_sets.is_empty()
                        || p.parameter_sets.iter().any(|s| s == ALL_PARAMETER_SETS)
                        || p.parameter_sets.iter().any(|s| sets.contains(s))
                })
                .cloned()
                .collect();
            if !filtered.is_empty() {
                return (filtered, label);
            }
        }

        (self.children.clone(), label)
    }
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
}
