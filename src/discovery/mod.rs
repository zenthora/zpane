mod catalog;
mod config;
mod context;
mod help_parser;
mod history;
mod introspect;
mod powershell;
mod shell_rc;
mod specs;

pub use catalog::{load_catalog, reload_catalog, ShellDialect};
pub use context::detect_runtime_context;
pub use introspect::{
    clear_disk_cache, inspect_command_chain, inspect_command_chain_with, warm_cli_cache,
    CliIntrospector, InspectReport, Introspect, PollResult,
};
pub use powershell::sync_powershell_metadata;
