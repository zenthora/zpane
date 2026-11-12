# 01: Explicit ShellDialect Value, Structured Parameter Aliases, and Domain Model Cleanup

**What to build:** Make `ShellDialect` (`PowerShell`, `Posix`, `Fish`) an explicit value passed into discovery functions instead of process-global `OnceLock` singletons (`shell_dialect()` and `common_var_items()`), internalize `PsParamExport` and `PsCommandExport` inside the discovery module, and preserve structured parameter `aliases: Vec<String>` on `MillerItem` when converting PowerShell metadata.

**Blocked by:** None (can start immediately)

**Status:** ready-for-agent

- [ ] `ShellDialect::detect()` detects the active shell once from environment variables, while discovery functions (`env_token`, `common_var_items`, `build_env_scope_item`, `load_col1_commands`, `introspect_cli_chain`) accept an explicit `ShellDialect` value without process-global `OnceLock` state.
- [ ] `PsParamExport` and `PsCommandExport` are removed from `src/model.rs` and made private to `src/discovery.rs`.
- [ ] `MillerItem` includes `aliases: Vec<String>` (defaulting to empty where unused) and `convert_ps_export_to_item` populates `aliases` from PowerShell parameter metadata while keeping the human-readable display label.
- [ ] Unit tests verify `PowerShell`, `Posix`, and `Fish` variable tokens, path-list splitting, and built-in variable items side-by-side within the same test process.
