# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [0.6.0] - 2026-11-21

### Features

- *(config)* Respect NO_COLOR, add themes and ASCII UI modes, and expand config.toml schema
- *(discovery)* Add XDG spec precedence, knack/argparse/man fallbacks, and project tasks
- *(ui)* Add subcommand breadcrumbs, committed markers, pipeline chaining, and scrollable inspector
- *(pwsh)* Add rich CLR parameter metadata, MAML help, profile sync, and PS 5.1 fallback
- *(shell)* Add cursor-aware editing, init --bind/--no-bind, live history, and Nushell support
- *(ui)* Add inline partial-height rendering mode (--height)
- *(cli)* Add zpane doctor diagnostics and sync docs

### Performance

- *(discovery)* Lazy cache fingerprinting, config-aware warm, and configurable timeout

### Bug Fixes

- *(ci)* Normalize Windows spec-dir dedup and satisfy Rust 1.100 clippy
- *(packaging)* Fix Unix unused_mut lint and aarch64 RPM cross-build

### Refactoring

- *(core)* Unify sanitization, Entry::walk_chain, and add CLI integration tests
- *(core)* Split discovery catalog submodules and extract shell module

### Miscellaneous Tasks

- *(release)* Add PR CI matrix, macOS and Linux arm64 packages, and package manager support
- *(release)* Bump version to 0.6.0 and update CHANGELOG.md
- *(release)* Refresh CHANGELOG.md for v0.6.0

## [0.5.2] - 2026-11-17

### Bug Fixes

- *(session)* Preserve pane 2 selection across catalog refresh

### Miscellaneous Tasks

- *(git)* Expand .gitignore and untrack AGENTS.md and docs/agents
- *(release)* Bump version to 0.5.2 and update CHANGELOG.md

## [0.5.1] - 2026-11-15

### Documentation

- *(readme)* Add animated SVG hero demo, badges, and crates.io package metadata
- *(readme)* Add crates.io badge and cargo install instructions

### Miscellaneous Tasks

- Remove skills-lock.json and use GitHub noreply email
- *(release)* Bump version to 0.5.1 and update CHANGELOG.md

## [0.5.0] - 2026-11-15

### Features

- *(cli)* Add inspect command and --json machine output

### Performance

- *(ui)* Optimize fuzzy matching and frame rendering without unwraps
- *(session)* Avoid subtree cloning on navigation and catalog key allocs
- *(discovery)* Optimize help and spec parsing without unwraps

### Bug Fixes

- *(security)* Sanitize CLI error output and document Win32 safety invariants

### Miscellaneous Tasks

- *(release)* Bump version to 0.5.0 and update CHANGELOG.md

## [0.4.1] - 2026-11-14

### Performance

- *(session)* Harden shell quoting and optimize hot-path pane syncing
- *(packaging)* Optimize release profile, deterministic packaging, and CI security

### Bug Fixes

- *(discovery)* Harden spec, help, introspection cache, and shell catalog parsing
- *(session)* Harden tokenization, subcommand sliding, parameter matching, and seeding
- *(ui)* Harden Unicode rendering, CLI parsing, fuzzy scrolling, and init wrappers
- *(scripts)* Add Python packaging fallback to package-linux.sh
- *(discovery)* Harden parsing/spawning and optimize cross-distro discovery
- *(session)* Block BiDi marks and path double-quote/flag bypasses
- *(ui)* Harden terminal escapes, file/shell paths, and optimize fuzzy rendering

### Miscellaneous Tasks

- *(git)* Remove migrated .kilo skill
- *(release)* Bump version to 0.4.1 and update CHANGELOG.md
- *(release)* Update CHANGELOG.md for 0.4.1
- *(release)* Update CHANGELOG.md for 0.4.1

## [0.4.0] - 2026-11-14

### Features

- Add multi-level subcommand sliding, shell init, cache freshness, and inspector overlay

### Miscellaneous Tasks

- *(skills)* Add project agent skills
- *(release)* Bump version to 0.4.0 and update CHANGELOG.md
- *(git)* Ignore and untrack .agents
- *(release)* Update CHANGELOG.md for 0.4.0

## [0.3.0] - 2026-11-14

### Features

- Add declarative specs, carapace bridge, and dynamic runtime context

### Refactoring

- *(core)* Split discovery module, align domain terms, and add agent guidelines

### Miscellaneous Tasks

- *(cargo)* Restore .cargo/config.toml tracking
- *(release)* Bump version to 0.3.0 and update CHANGELOG.md

## [0.2.0] - 2026-11-13

### Features

- *(ui)* Narrow rendering seam and unify viewport geometry
- *(discovery)* Make ShellDialect explicit, preserve parameter aliases, and internalize PS DTOs
- *(discovery)* Encapsulate CLI introspection and caching in CliIntrospector
- *(session)* Extract MillerSession with semantic UserAction interface

### Bug Fixes

- *(session)* Keep preview, Column 3 cursor stable across async updates
- *(scripts)* Use cargo update --workspace --offline in bump-version

### Refactoring

- *(core)* Deepen session, discovery, and viewport seams
- *(core)* Simplify session, UI rendering, and discovery helpers

### Documentation

- *(spec)* Add deepen-core-architecture spec and tickets

### Miscellaneous Tasks

- *(git)* Ignore and untrack .scratch and .cargo
- *(hooks)* Configure prek stages, conventional commits, and git-cliff SemVer
- *(hooks)* Mark manual git-cliff-changelog hook as always_run
- *(release)* Bump version to 0.2.0 and update CHANGELOG.md

## [0.1.0] - 2026-11-12

### Features

- Framework help parsers, interactive value inputs, and parameter set exclusivity
- *(release)* Rename project to zpane, add Fish shell dialect, and package for Arch, Debian, Fedora & Termux

### Performance

- *(tui)* Cache filtered column views, coalesce burst events, and prep v0.1.0 release

<!-- generated by git-cliff -->
