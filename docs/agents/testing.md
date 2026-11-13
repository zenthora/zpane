# Testing Guidelines

## Layout and runner

- `zpane` is a binary crate (`src/main.rs`) with modules `discovery`, `fuzzy`, `model`, `session`, and `ui`.
- Tests live in `#[cfg(test)] mod tests` inside the module they verify.
- Run the suite with `cargo test --locked` (also enforced by `prek`).

## Test at seams, not internals

Verify observable behavior through each module's public seam so tests survive internal refactors:

- **Session & command assembly** (`src/session.rs`): drive `MillerSession` with `UserAction` and assert on `SessionOutcome`, `command_line()`, `view()`, or `render()`.
- **CLI introspection & caching** (`src/discovery/introspect.rs`): inject a deterministic runner via `CliIntrospector::with_runner` and observe behavior through `Introspect`.
- **Catalog & help parsing** (`src/discovery/catalog.rs`, `src/discovery/help_parser.rs`): pass explicit `ShellDialect` values and representative CLI help output.
- **Viewport & rendering** (`src/ui.rs`): test `Viewport` geometry and `render_frame` output against terminal dimensions.
- **Fuzzy list & model** (`src/fuzzy.rs`, `src/model.rs`): test `PaneList` cursor/filtering behavior and `Entry` / `Pane` role semantics.

## Mocking and test doubles

Mock or stub only at **system boundaries**:

- External host processes (`pwsh`, `git`, CLI `--help`) via injected runner closures
- Persistent cache paths using unique per-test files under `std::env::temp_dir()` (cleaned up by the test)
- Terminal I/O and wall-clock/async polling via focused trait fakes (such as `FakeIntrospector` implementing `Introspect`)

Keep in-process structs (`Entry`, `Pane`, `Viewport`, `ShellDialect`) and pure parsers real rather than mocking internal collaborators.

## Test quality

- **Domain vocabulary**: name tests and helpers using `CONTEXT.md` terms (`Pane`, `Entry`, `Scope Root`, `Placeholder`, `Command Alias`, `Parameter Alias`, `Parameter`, `Flag`, `Parameter Set`, `Token`, `Committed Token`, `Command Line`, `Shell Dialect`, `Command Catalog`, `Introspection`).
- **Independent expectations**: assert worked literals or spec outcomes rather than recomputing expected values with the same formula as the code under test.
- **Vertical slices**: for features and bug fixes, write one failing test at the seam, implement the minimal behavior to turn it green, and repeat.
