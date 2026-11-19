mod catalog;
mod config;
mod context;
mod help_parser;
mod history;
mod introspect;
mod powershell;
mod project_tasks;
mod shell_rc;
mod specs;

pub use catalog::{load_catalog, reload_catalog, ShellDialect};
pub use config::{
    config_path, load_user_config, load_user_config_with_diagnostics, parse_user_config_toml,
    parse_user_config_toml_with_diagnostics, BindableAction, BorderStyle, ConfigDiagnostic,
    IconMode, KeyChord, KeybindingsConfig, ThemePreset, UiConfig, UiStyle, UserConfig,
};
pub use context::detect_runtime_context;
pub use introspect::{
    clear_disk_cache, inspect_command_chain, inspect_command_chain_with, warm_cli_cache,
    CliIntrospector, InspectReport, Introspect, PollResult,
};
pub use powershell::sync_powershell_metadata;
