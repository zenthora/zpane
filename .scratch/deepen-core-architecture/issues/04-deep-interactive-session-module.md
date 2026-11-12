# 04: Deep Interactive Miller Session Module and Unified Token Assembly

**What to build:** Consolidate Miller-column navigation, per-column fuzzy filtering, initial command-buffer token seeding, structured parameter-set alias matching, and live command preview/output assembly behind a single deep in-process session module (`MillerSession`) with a semantic `UserAction` interface.

**Blocked by:** 01: Explicit ShellDialect Value, Structured Parameter Aliases, and Domain Model Cleanup, 02: Narrow UI Frame Seam and Authoritative Viewport Geometry, 03: Deep CLI Introspection Module and Canonical Priority Tool Catalog

**Status:** resolved

- [x] `MillerSession` encapsulates column catalogs, fuzzy filter strings, cached filtered index views, selection/scroll indices, committed tokens, initial buffer seeding, and active parameter-set computation behind a semantic action interface (`UserAction`).
- [x] Parameter-set exclusivity matches committed tokens case-insensitively against exact parameter flag names and structured `MillerItem.aliases` entries rather than substring-matching `MillerItem.label` (fixing `-Id` falsely matching `-InformationAction`).
- [x] Accepting a switch or childless flag in Column 2 via `Tab` (`AcceptToken`) commits the flag to `tokens` without polluting the live command preview with the newly highlighted index-0 item before the user moves or filters.
- [x] Stringly-typed cache-invalidation keys (`last_c1_key`, `last_c2_key`, `last_active_param_key`) are eliminated or fully encapsulated inside `MillerSession`.
- [x] Comprehensive in-process tests exercise multi-step user journeys (`StepRight`, `AcceptToken`, `Backspace`, `InsertChar`, `MoveDown`, `PageDown`, `Confirm`, `ReloadCatalog`) across dialects and verify preview/output invariants.
