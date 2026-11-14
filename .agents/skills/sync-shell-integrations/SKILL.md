---
name: sync-shell-integrations
description: Keep zpane CLI flags, README.md shell integration snippets (PowerShell, Zsh, Bash, Fish), and ShellDialect runtime behavior synchronized. Use when changing CLI flags, keybindings, --context-file/--history injection, or dialect-specific rendering.
---

# Sync Shell Integrations (`zpane`)

Keep `zpane`'s CLI contract, dialect-aware runtime behavior (`ShellDialect`), and shell glue snippets in `README.md` aligned whenever flags, keybindings, context injection, or variable/path syntax changes.

## Workflow

1. **Audit the source of truth**
   - Inspect CLI argument parsing in `src/main.rs` (`--initial`, `--out-file`, `--width`, `--height`, `--context`, `--context-file`, `--git-branch`, `--history`, `--sync-ps`, `--warm`, and any newly added flags).
   - Inspect dialect detection and catalog loading in `src/discovery/catalog.rs` (`ShellDialect`, `detect_runtime_context`, `load_catalog`, `reload_catalog`, `sync_powershell_metadata`).
   - Check dialect-dependent rendering and variable scoping in `src/ui.rs` and `src/session.rs`.

2. **Check all 4 shell snippets in `README.md`**
   - Review the integration blocks in `README.md` for every supported shell:
     - **PowerShell** (`PSReadLine` handler, `$env:LOCALAPPDATA`, temp `--context-file` JSON payload with history, `--sync-ps`)
     - **Zsh** (`zle` widget, `LBUFFER`, `--initial`, `--out-file`, plus `--context`/`--history` parity where applicable)
     - **Bash** (`bind -x`, `READLINE_LINE`, `READLINE_POINT`, `--initial`, `--out-file`)
     - **Fish** (`commandline`, `bind \t`, `--initial`, `--out-file`)
   - Ensure flag names, exit-code handling (cancel vs. accept without clobbering the prompt line), temp file cleanup, and quoting match `src/main.rs` across all four shells.

3. **Verify dialect tests & docs**
   - Run and update dialect-coverage tests when adding or altering dialect behavior:
     - `cargo test --locked test_variable_scopes_across_all_three_dialects` (`src/session.rs`)
     - `cargo test --locked test_footer_ctrl_r_hint_is_dialect_aware` (`src/ui.rs`)
     - Full suite: `cargo test --locked`
   - Keep `README.md` CLI Options table and Keyboard Shortcuts section synchronized with actual keybindings and flags.
   - Check `prek run --all-files` so Markdown formatting, trailing whitespace, and Rust checks pass together.

## Completion Criteria

- Every CLI flag or keybinding referenced in `README.md` shell snippets exists verbatim in `src/main.rs` / `src/ui.rs`.
- PowerShell, Zsh, Bash, and Fish snippets handle output insertion and clean cancellation consistently.
- Dialect tests and `prek run --all-files` pass after final edits.
