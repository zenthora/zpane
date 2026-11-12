use serde::{Deserialize, Serialize};
use std::collections::HashSet;

/// Identity of one Miller column, replacing magic `0..=2` indices.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Column {
    First,
    Second,
    Third,
}

impl Column {
    pub const ALL: [Column; 3] = [Column::First, Column::Second, Column::Third];

    pub const fn index(self) -> usize {
        match self {
            Column::First => 0,
            Column::Second => 1,
            Column::Third => 2,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ItemKind {
    Env,
    Var,
    Cli,
    Func,
    Alias,
    Cmdlet,
    Subcmd,
    Flag,
    Param,
    Switch,
    Enum,
    Value,
    Arg,
    Loading,
}

impl ItemKind {
    pub fn badge(self) -> &'static str {
        match self {
            ItemKind::Env => "[Env]",
            ItemKind::Var => "[Var]",
            ItemKind::Cli => "[CLI]",
            ItemKind::Func => "[Func]",
            ItemKind::Alias => "[Alias]",
            ItemKind::Cmdlet => "[Cmdlet]",
            ItemKind::Subcmd => "[Subcmd]",
            ItemKind::Flag => "[Flag]",
            ItemKind::Param => "[Param]",
            ItemKind::Switch => "[Switch]",
            ItemKind::Enum => "[Enum]",
            ItemKind::Value => "[Value]",
            ItemKind::Arg => "[Arg]",
            ItemKind::Loading => "[...]",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MillerItem {
    pub text: String,
    pub label: String,
    pub kind: ItemKind,
    pub desc: String,
    #[serde(default)]
    pub children: Vec<MillerItem>,
    #[serde(default)]
    pub parameter_sets: Vec<String>,
    #[serde(default)]
    pub aliases: Vec<String>,
}

impl MillerItem {
    pub fn new(
        text: impl Into<String>,
        label: impl Into<String>,
        kind: ItemKind,
        desc: impl Into<String>,
    ) -> Self {
        Self {
            text: text.into(),
            label: label.into(),
            kind,
            desc: desc.into(),
            children: Vec::new(),
            parameter_sets: Vec::new(),
            aliases: Vec::new(),
        }
    }

    #[cfg(test)]
    pub fn with_parameter_sets(mut self, sets: Vec<String>) -> Self {
        self.parameter_sets = sets;
        self
    }

    #[cfg(test)]
    pub fn with_aliases(mut self, aliases: Vec<String>) -> Self {
        self.aliases = aliases;
        self
    }

    /// Whether this parameter item matches a raw command token (`-Id`, `--id`, `-Id:value`)
    /// by exact parameter flag name or structured alias, case-insensitively.
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

    /// Computes the active PowerShell parameter sets implied by `tokens`, using exact
    /// parameter flags and structured aliases. Only cmdlets, functions, and aliases
    /// carry parameter-set metadata.
    pub fn active_parameter_sets(&self, tokens: &[String]) -> Option<HashSet<String>> {
        if !matches!(
            self.kind,
            ItemKind::Cmdlet | ItemKind::Func | ItemKind::Alias
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
                        .filter(|s| s.as_str() != "__AllParameterSets")
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
}
