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
    HeightSpec, IconMode, KeyChord, KeybindingsConfig, ThemePreset, UiConfig, UiStyle, UserConfig,
    DEFAULT_INTROSPECT_TIMEOUT_MS, DEFAULT_MIN_HEIGHT, MAX_INTROSPECT_TIMEOUT_MS, MAX_MIN_HEIGHT,
    MIN_INTROSPECT_TIMEOUT_MS,
};
pub use context::detect_runtime_context;
pub use introspect::{
    clear_disk_cache, inspect_command_chain, inspect_command_chain_with, warm_cli_cache,
    CliIntrospector, CommandRunOutcome, InspectReport, Introspect, PollResult,
};
pub(crate) use powershell::apply_ps_help_to_entry;
pub use powershell::{sync_powershell_metadata, PsCmdHelp};
