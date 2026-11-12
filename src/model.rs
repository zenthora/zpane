use serde::{Deserialize, Serialize};

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

    #[allow(dead_code)]
    pub fn with_parameter_sets(mut self, sets: Vec<String>) -> Self {
        self.parameter_sets = sets;
        self
    }

    #[allow(dead_code)]
    pub fn with_aliases(mut self, aliases: Vec<String>) -> Self {
        self.aliases = aliases;
        self
    }
}
