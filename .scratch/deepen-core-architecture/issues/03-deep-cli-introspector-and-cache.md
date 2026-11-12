# 03: Deep CLI Introspection Module and Canonical Priority Tool Catalog

**What to build:** Encapsulate CLI `--help`/`-h` introspection, command-chain cache key canonicalization, in-memory and disk cache persistence (`cli_cache.json`), background worker queuing/polling, and priority tool cache warming behind a single deep `CliIntrospector` module in `src/discovery.rs` with an injectable command-output adapter for deterministic tests.

**Blocked by:** 01: Explicit ShellDialect Value, Structured Parameter Aliases, and Domain Model Cleanup

**Status:** ready-for-agent

- [ ] `CliIntrospector` in `src/discovery.rs` owns the in-memory CLI cache map, dirty tracking, disk persistence, canonical cache key formatting from command chains (`&[&str]`), and background worker thread/channels.
- [ ] `BgRequest` and `BgResponse` are removed from `src/model.rs`.
- [ ] A single canonical priority CLI tool list in `src/discovery.rs` is shared between Column 1 sorting and `zpane --warm` batch cache warming (including `gh` and all priority tools).
- [ ] `CliIntrospector` supports an injectable command-runner adapter in tests to verify cache misses returning a loading item, background completion populating the cache on poll, `--help` to `-h` fallback, and canonical key matching without spawning OS processes.
