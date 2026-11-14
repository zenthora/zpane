mod catalog;
mod help_parser;
mod introspect;
mod specs;

pub use catalog::{
    detect_runtime_context, load_catalog, reload_catalog, sync_powershell_metadata, ShellDialect,
};
#[cfg(test)]
pub(crate) use introspect::inspect_command_chain_with;
#[cfg(test)]
pub use introspect::PollResult;
pub use introspect::{
    clear_disk_cache, inspect_command_chain, warm_cli_cache, CliIntrospector, InspectReport,
    Introspect,
};
