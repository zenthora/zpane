# Deepen Core Architecture Across Session, Discovery, Rendering, and Shell Dialect Seams

Status: ready-for-agent

## Problem Statement

As `zpane` has grown into a cross-platform interactive Miller-column command builder supporting PowerShell, POSIX, and Fish shells, core domain responsibilities have become fragmented across shallow modules that leak state and invariants across their seams:

1. **Fragmented Command Assembly & Navigation Invariants**: Token pushing, deduplication, scope-root filtering (`$env:` / `$`), parameter-set exclusivity, fuzzy column filtering, and stringly-typed dirty-tracking keys are scattered across event handlers, column resolvers, and preview builders. Structured PowerShell parameter aliases are flattened into display labels during discovery and later reverse-engineered via substring checks, causing short parameter flags (such as `-Id` or `-I`) to falsely match unrelated parameter labels (such as `-InformationAction`) and activate the wrong parameter sets. Accepting a switch or flag in Column 2 with `Tab` also causes the live command preview to drift by immediately appending the newly highlighted index-0 parameter before the user selects it.
2. **Leaked CLI Introspection & Caching Mechanics**: The interactive coordinator manually formats string cache keys (`cmd` vs. `cmd::subcmd`) in multiple places, manages raw background worker channels, tracks cache dirty flags, and duplicates priority CLI tool lists between interactive discovery and batch cache warming.
3. **Wide Rendering Seam & Split Viewport Geometry**: Rendering requires packing a 16-field state structure where the caller pre-computes scroll offsets using hardcoded assumptions about terminal chrome height and Column 3's 3-row inspector header, pre-formats inspector badge strings, and disagrees with the renderer on viewport clamping when terminal height is under 16 rows. Meanwhile, terminal ANSI color escape sequences live inside the core domain data model.
4. **Process-Global Shell Dialect Singletons**: Shell dialect detection and common variable suggestions rely on process-global singletons latched on first access from environment variables, preventing deterministic multi-dialect verification in a single test run and leaking PowerShell serialization structures into the shared domain model.

## Solution

Deepen `zpane`'s architecture around two primary seams—an in-process **Interactive Session & Frame Seam** and a local-substitutable **CLI Introspection & Discovery Seam**—parameterized by an explicit **Shell Dialect** value:

1. **Deep Interactive Session Module**: Consolidate Miller-column navigation, per-column fuzzy filtering, initial command-line buffer seeding, structured parameter-set exclusivity, and command token/preview assembly behind a single deep in-process session module. Preserve structured parameter aliases on domain items so parameter-set activation uses exact case-insensitive flag/alias matching rather than label substring matching, and unify token-commit and preview semantics so `Right`, `Tab`, `Backspace`, and `Enter` share one source of truth.
2. **Deep CLI Introspection Module**: Encapsulate command-chain cache key canonicalization, in-memory and disk cache persistence, background worker queuing/polling, and priority-tool cache warming behind a single discovery module interface. Callers query by command chain slice and poll for completed updates without touching raw worker messages, cache keys, or disk persistence paths.
3. **Deep UI & Viewport Module**: Move terminal ANSI color palettes, inspector title/description formatting, and viewport row/scroll calculations into the rendering module behind a narrow interface that accepts a read-only session view and terminal dimensions. Ensure `Up`, `Down`, `PageUp`, `PageDown`, and frame rendering use one authoritative viewport geometry calculation across all terminal sizes.
4. **Explicit Shell Dialect Value & Internalized Serialization DTOs**: Replace process-global `OnceLock` dialect and variable singletons with an explicit `ShellDialect` value detected once at startup (or passed directly by tests), and internalize PowerShell JSON deserialization structures within the discovery module.

## User Stories

1. As a shell user navigating Miller columns, I want `Right`, `Tab`, `Backspace`, and `Enter` to follow a single consistent token-assembly model, so that the live command preview always matches the command emitted to my shell.
2. As a shell user pressing `Tab` on a switch flag or childless flag in Column 2, I want the flag committed to my command tokens without the newly highlighted index-0 item prematurely appearing as a second flag in the live preview, so that I do not accidentally emit unintended flags.
3. As a PowerShell user typing or selecting a short parameter flag or alias such as `-Id`, I want parameter-set exclusivity to match exact parameter names and structured aliases rather than substrings of display labels, so that unrelated parameters like `-InformationAction` do not falsely lock me into the wrong parameter set.
4. As a PowerShell user viewing a cmdlet with multiple parameter sets, I want the active parameter set badge and filtered Column 2 parameters to update accurately whenever tokens are pushed or popped, so that mutually exclusive parameters from incompatible sets are hidden immediately.
5. As a shell user launching `zpane` with an initial command buffer containing a command and subcommand or flag, I want Column 1 and Column 2 selections to seed deterministically from those initial tokens, so that I resume navigation at the exact point in the command hierarchy I already typed.
6. As a shell user browsing environment variables or shell variables in Column 1, I want the root scope indicator (`$env:` or `$`) to open variable scopes in Column 2 without pushing a dangling scope prefix token into the emitted command, so that selecting a variable inserts only the valid variable reference or literal value.
7. As a shell user selecting a switch parameter in Column 2 and stepping into Column 3, I want the switch indicator placeholder in Column 3 to never emit a blank or spurious argument token, so that switch flags remain valueless unless I explicitly type a filter value.
8. As a shell user typing a custom argument value into the Column 3 filter when no static completion item matches, I want the custom value to appear in the live command preview and be committed on `Tab` or `Enter`, so that I can supply arbitrary paths, strings, or numbers smoothly.
9. As a shell user pressing `Backspace` while a column filter has text, I want `Backspace` to delete the last character of the active column's filter before popping committed tokens, so that I can edit search filters naturally without losing prior command tokens.
10. As a shell user pressing `Backspace` when the active column's filter is empty, I want `Backspace` to pop the most recently committed token or step back to the parent column, so that I can undo command construction step by step.
11. As a shell user pressing `Ctrl+R` during an interactive session, I want the catalog and column views to refresh cleanly without stale cache keys blocking re-evaluation, so that newly installed tools or synced metadata appear immediately.
12. As a shell user exploring a CLI tool whose `--help` output has not been parsed yet, I want the session to display a non-selectable loading indicator while background introspection runs and automatically populate the column as soon as introspection finishes, so that the interface never freezes on slow commands.
13. As a shell user navigating across subcommands in Column 2 (such as `git commit` or `cargo build`), I want Column 3 to automatically query and cache subcommand-specific flags (`<cmd> <subcmd> --help`), so that subcommand flags are available alongside common shell variables.
14. As a shell user running `zpane --warm`, I want batch cache warming to use the exact same canonical priority tool catalog and cache key format as interactive discovery, so that every warmed tool hits the cache during interactive sessions.
15. As a shell user exiting `zpane` after inspecting new CLI tools, I want the CLI introspection module to persist updated help entries to disk only when new entries were added, so that disk writes are avoided on read-only sessions.
16. As a shell user running `zpane` in a compact split pane or small terminal window (under 16 rows high), I want viewport scrolling and frame rendering to agree on the exact number of visible rows in every column, so that the highlighted selection never scrolls off-screen or clips below the border.
17. As a shell user pressing `PageUp` or `PageDown` in any column, I want the selection cursor to jump by the actual visible row capacity of that column (accounting for Column 3's 3-row inspector header), so that paging moves cleanly by one screenful at a time.
18. As a shell user inspecting items in Column 3, I want the 3-row inspector header and bottom info bar to be formatted directly by the UI module from the currently selected item, so that badge icons, titles, and descriptions render consistently without string formatting leaking into session state.
19. As a Fish, POSIX (`bash`/`zsh`), or PowerShell user, I want variable syntax (`$VAR`, `$env:VAR`), path-list splitting (`:` vs `;`), and common built-in variable suggestions (`$PWD`, `$status`, `$LASTEXITCODE`, `$?`) to be governed by an explicit `ShellDialect` value, so that `zpane` always emits syntax native to my active shell.
20. As a developer or automated agent maintaining `zpane`, I want to test interactive navigation, fuzzy filtering, parameter-set exclusivity, command preview construction, and terminal frame rendering entirely in-process across all three shell dialects without spawning subprocesses or mutating global process state, so that tests are fast, deterministic, and isolated.
21. As a developer or automated agent maintaining CLI discovery, I want to test CLI help introspection, cache key canonicalization, and background completion polling using an in-memory command-output adapter, so that discovery logic is verified without executing external binaries on the host machine.
22. As a developer or automated agent reading the domain model, I want the domain model module to contain only core domain types free of ANSI escape codes, background worker channel payloads, and PowerShell JSON serialization structures, so that the domain interface is small and self-contained.

## Implementation Decisions

1. **Deep Interactive Session Module**:
   - Create a dedicated session module that owns the 3-column item catalogs, per-column fuzzy filter strings, cached filtered index views, active column index, per-column selection indices, committed command tokens, pending initial-buffer token state, and active parameter-set metadata.
   - Replace raw key-event branching and scattered free functions with a semantic action interface (`MoveUp`, `MoveDown`, `PageUp`, `PageDown`, `StepRight`, `StepLeft`, `AcceptToken`, `Backspace`, `InsertChar`, `ClearFilter`, `ReloadCatalog`, `Confirm`, `Cancel`).
   - Encapsulate all dirty-tracking inside the session module so callers never manipulate string cache-invalidation keys (`last_c1_key`, `last_c2_key`, `last_active_param_key`) or per-column dirty booleans directly.
   - Unify token commitment (`StepRight`, `AcceptToken`) and live command preview calculation so both consult a single internal rule for root scope items (`Env`, `Var`, `Loading`), switch placeholders, custom Column 3 filter values, and cursor advancement. Specifically, when `AcceptToken` (`Tab`) commits a childless flag or switch in Column 2, track that the current Column 2 token was just accepted or advance state cleanly so the preview does not immediately append the reset index-0 item until the user navigates or types a new filter.

2. **Structured Parameter Aliases on Domain Items**:
   - Extend the domain item representation to retain structured parameter aliases (`Vec<String>`) alongside parameter-set memberships (`Vec<String>`), rather than discarding aliases after formatting them into display labels.
   - Compute active parameter sets by normalizing committed tokens (stripping leading dashes) and comparing them case-insensitively against exact parameter flag names and structured alias entries on the selected Column 1 cmdlet's children.

3. **Deep CLI Introspection & Discovery Module**:
   - Introduce a `CliIntrospector` module inside the discovery subsystem that encapsulates:
     - The in-memory CLI help cache map and its dirty flag.
     - Disk cache loading and conditional saving on shutdown.
     - Canonical cache key construction from command chain slices (`&[&str]`).
     - The background worker thread and internal request/response channels.
     - A single canonical priority CLI tool list shared by Column 1 sorting and `--warm` batch cache warming.
   - Expose a narrow interface on `CliIntrospector`:
     - Querying a command chain (`&[&str]`) returns either the cached slice of items or a loading placeholder item while automatically enqueueing a deduplicated background introspection job.
     - Polling completed background jobs drains internal worker responses into the cache and returns a boolean indicating whether any cache entries were updated so the session can refresh dependent columns.
     - Flushing persists the cache to disk if and only if new entries were recorded.
   - Support an internal command-runner adapter (`Fn(&str, &[&str]) -> Option<String>` or trait equivalent) so tests can supply synthetic `--help` output without spawning OS processes.
   - Remove `BgRequest` and `BgResponse` from the shared domain model module.

4. **Explicit Shell Dialect Value & Internalized Discovery Serialization**:
   - Promote `ShellDialect` (`PowerShell`, `Posix`, `Fish`) to an explicit value passed into discovery functions and session construction, with a single startup detection entry point that reads environment variables (`ZPANE_SHELL`, `FISH_VERSION`, `PSModulePath`, `SHELL`).
   - Remove the process-global `OnceLock<ShellDialect>` and `OnceLock<Vec<MillerItem>>` singletons so any dialect's variable syntax (`env_token`), path-list separator rules, and built-in variable items (`common_var_items`) can be instantiated and tested side-by-side in the same process.
   - Move `PsParamExport` and `PsCommandExport` out of the shared domain model module and make them private implementation details of the discovery module.

5. **Narrow Frame Rendering Seam & Authoritative Viewport Geometry**:
   - Move `ItemKind` ANSI color mapping out of the domain model module into the UI module.
   - Provide a single viewport row-count calculation in the UI module that determines visible list rows for each column (accounting for outer frame chrome and Column 3's 3-row inspector header) and is shared by both scroll clamping and `PageUp`/`PageDown` navigation.
   - Narrow the rendering interface so `render_frame` accepts a compact read-only view of the session state and terminal dimensions, deriving the Column 3 inspector header and bottom info bar strings (`info_title`, `info_desc`) directly inside the UI module from the currently highlighted item.

## Testing Decisions

1. **What Makes a Good Test**:
   - Tests must cross the same public module seams as production callers and assert on observable outcomes (live preview strings, committed output commands, visible column item labels, active parameter-set badges, and rendered frame strings) rather than internal dirty flags or private cache keys.
   - Tests must survive internal refactors of caching or state tracking without modification.

2. **Modules & Seams to Be Tested**:
   - **Seam 1 — Interactive Session & UI Rendering Seam (`in-process`)**:
     - Test multi-step user journeys by constructing a session with a synthetic catalog and an explicit `ShellDialect`, applying sequences of semantic actions (`StepRight`, `AcceptToken`, `Backspace`, `InsertChar`, `MoveDown`, `PageDown`, `Confirm`), and asserting on the resulting preview command, active parameter-set filtering, and rendered terminal frame.
     - Specifically verify:
       - Exact parameter alias matching (confirming `-Id` activates only the parameter set owning `-Id` and does not match `-InformationAction`).
       - `Tab` (`AcceptToken`) on a Column 2 switch or childless flag does not pollute the live preview with the newly selected index-0 item.
       - Initial command-buffer seeding across Column 1 and Column 2 (`zpane git commit`).
       - Environment and variable scope navigation across `PowerShell`, `Posix`, and `Fish` dialects in the same test binary.
       - Small-terminal (`height < 16`) viewport clamping and `PageUp`/`PageDown` step sizes across Column 1, Column 2, and Column 3 (with its 3-row inspector offset).
   - **Seam 2 — CLI Introspection & Discovery Seam (`local-substitutable`)**:
     - Test `CliIntrospector` with an injected in-memory command-help adapter and isolated cache storage, verifying cache misses return a loading item, background completion populates the cache and reports updates on poll, `--help` to `-h` fallback works, and canonical command-chain keys match between interactive queries and batch warming.

3. **Prior Art in the Codebase**:
   - Existing unit tests in `src/main.rs` (`test_parameter_set_filtering_and_preview`, `test_subcommand_and_custom_input_resolution`) and `src/discovery.rs` (`test_parse_clap_and_gnu_help`, `test_parse_go_flag_help`, `test_parse_argparse_and_usage_fallback`, `test_clean_single_line_strips_control_chars`) establish in-memory fixture construction and help-output parsing assertions. The new tests will replace the low-level helper calls in `src/main.rs` with assertions at the deepened session, UI, and introspector interfaces.

## Out of Scope

- Changing the external CLI flags or user-facing keybindings of `zpane`.
- Adding new shell wrapper scripts beyond PowerShell, POSIX (`bash`/`zsh`), and Fish.
- Changing the visual visual theme, box-drawing characters, or color palette of the 3-column Miller TUI.
- Modifying the Linux, Android Termux, or Windows release packaging scripts.

## Further Notes

- This spec synthesizes Candidates 1, 2, 3, and 4 from the architecture review into a cohesive refactoring effort where the four candidates reinforce each other: narrowing the discovery and UI seams directly enables the interactive session module to become a pure, self-contained in-process state machine.
