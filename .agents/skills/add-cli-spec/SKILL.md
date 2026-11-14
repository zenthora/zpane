---
name: add-cli-spec
description: Author, validate, or debug declarative Carapace or Fig CLI completion specs for zpane in src/discovery/specs.rs. Use when adding custom CLI subcommand/flag/value YAML or JSON specs, importing Carapace/Fig bridges, or troubleshooting spec parsing.
---

# Add or Debug CLI Spec (`zpane`)

Create or validate declarative Carapace (`.yaml` / `.yml` / `.json`) or simplified Fig (`.json`) completion specs loaded by `zpane`'s spec discovery engine (`src/discovery/specs.rs`, documented in `README.md`).

## Workflow

1. **Confirm target storage location & schema**
   - Read `specs_dir`, `is_spec_filename`, `parse_spec_str`, `CarapaceSpec`, `CarapaceCompletion`, and `FigSpec` in `src/discovery/specs.rs`.
   - `specs_dir()` resolves to:
     - Windows: `%LOCALAPPDATA%\zpane\specs`
     - Linux/macOS: `$XDG_CACHE_HOME/zpane/specs` (fallback `~/.cache/zpane/specs`)
   - Choose format intentionally:
     - **Carapace YAML/JSON** (`CarapaceSpec`): supports `name`, `description`, `aliases`, `flags`, `persistentflags`, `commands`, and dynamic `completion` (`flag`, `positional`, `positionalany`).
     - **Simplified Fig JSON** (`FigSpec`): supports `name`, `description`, `subcommands`, `options` (`FigOption`), and `args` (`FigArg` with `suggestions` and `template`).

2. **Watch `yaml_to_json` subset constraints**
   - `src/discovery/specs.rs` parses YAML via the built-in `yaml_to_json` helper rather than a full YAML engine.
   - Stick to standard 2-space indentation, `key: value` mappings, inline or `- item` sequences, and quoted strings when descriptions contain `: `, `#`, `[...]`, or `{...}`. Avoid YAML anchors/aliases, merge keys (`<<:`), multi-line block scalars (`|` / `>`), or custom YAML tags; if complex structures are needed, emit `.json` instead.

3. **Author spec from verified CLI ground truth**
   - Verify actual subcommands, flags, aliases, and value expectations against `<cli> --help` (or an upstream Carapace/Fig spec). Do not invent flags or subcommands.
   - For flags that accept values (paths, choices, dynamic commands), check how `flags` / `persistentflags` and `completion` entries map in `src/discovery/specs.rs` so `zpane` treats them as value-taking parameters rather than standalone flags.

4. **Test parsing & interactive behavior**
   - Add or run unit tests exercising `parse_spec_str` and `load_specs` in `src/discovery/specs.rs` (`cargo test --locked specs`).
   - For live verification in `zpane`, place the spec in `specs_dir()` as `<tool>.yaml` or `<tool>.json` and test `zpane --initial "<tool> "` (plus `--warm` / cache refresh if catalog caching applies).

## Completion Criteria

- Spec parses without errors through `parse_spec_str` and preserves subcommands, persistent flags, aliases, and value completions.
- YAML specs comply with `yaml_to_json` rules (or use `.json` cleanly).
- `cargo test --locked` passes after any code or fixture changes in `src/discovery/specs.rs`.
