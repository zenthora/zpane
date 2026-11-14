---
name: add-help-parser
description: Extend or debug zpane CLI --help and -h parsing in src/discovery/help_parser.rs. Use when a CLI tool (Cobra, Clap, winget, git, or custom help layout) misses subcommands, flags, aliases, or value-taking parameters, or when adding regression fixtures.
---

# Add or Fix Help Parser (`zpane`)

Extend `zpane`'s `--help` / `-h` text discovery parser safely using real CLI output fixtures and domain-accurate `Entry` classification (`CONTEXT.md`). Read `docs/agents/testing.md` before adding tests.

## Workflow

1. **Capture real `--help` output**
   - Run the actual target CLI command (`<tool> --help`, `<tool> -h`, or subcommand help) and save or quote the exact text. Never fabricate or paraphrase CLI help output.
   - Identify what is missing or misclassified:
     - Section headers (`HelpSection` via `classify_section_header` in `src/discovery/help_parser.rs`)
     - Subcommands (`EntryKind::Subcmd`)
     - Boolean switches (`EntryKind::Flag`) vs. value-taking parameters (`EntryKind::Param`)
     - Short/long aliases (`aliases` vector, e.g. `-f, --force`)
     - Default/enum hints or positional placeholders (`EntryRole::Placeholder`)

2. **Locate the matching parser path**
   - Inspect `parse_cli_help` and `classify_section_header` in `src/discovery/help_parser.rs`.
   - Check whether the tool already matches an existing format handler (Cobra, Clap, winget, git, or fallback regexes) before introducing a new rule. Keep common logic shared rather than duplicating per-CLI special cases.
   - Ensure boolean flags stay `EntryKind::Flag` and option arguments expecting values (e.g. `--file <PATH>`, `--format string`) map to `EntryKind::Param`.

3. **Add a regression fixture test first (red-green)**
   - Inspect existing unit tests in `src/discovery/help_parser.rs` (such as `test_parse_git_help`, `test_parse_winget_help`, `test_parse_cobra_gh_help`, `test_parse_clap_prek_help`, `test_fallback_parser_preserves_aliases_and_param_kinds`).
   - Add a deterministic unit test using the captured real help snippet as an inline fixture, asserting exact parsed command/flag/param names, kinds, and aliases (and no spurious false positives from wrapped description lines).

4. **Verify**
   - Run `cargo test --locked help_parser` and then `cargo test --locked` (and `prek run --all-files` before wrapping up).
   - If the parser change affects UI display or parameter completion behavior, inspect affected call sites across `src/discovery/` and `src/session.rs`.

## Completion Criteria

- Real CLI `--help` fixture lives in `src/discovery/help_parser.rs` tests and asserts both positive extractions and negative/false-positive boundaries.
- `EntryKind::Flag` vs. `EntryKind::Param` and aliases match `CONTEXT.md` semantics.
- `cargo test --locked` and `cargo clippy --all-targets -- -D warnings` pass after the final edit.
