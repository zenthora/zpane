# Deepening

How to deepen a cluster of shallow modules safely, given its dependencies. Assumes the vocabulary in [SKILL.md](SKILL.md): **module**, **interface**, **seam**, **adapter**.

## Dependency categories

When assessing a candidate for deepening, classify its dependencies. The category determines how the deepened module is tested across its seam.

### 1. In-process

Pure computation, in-memory state, no OS/TTY/filesystem I/O (e.g., `MillerSession` state transitions, `fuzzy::filter_items`, `ui::render_frame`, help-text parsers). Always deepenable: merge the modules and test through the new interface directly. No adapter needed.

### 2. Local-substitutable

Dependencies with deterministic local test stand-ins (temp directories for disk caches, in-memory closures/adapters like `CommandRunner` instead of spawning host binaries, explicit `ShellDialect` values instead of reading process env vars). Deepenable if the stand-in exists. The deepened module is tested with the stand-in in `cargo test`. The seam is internal or a minimal injected adapter.

### 3. Remote or OS-owned (Ports & Adapters)

External processes, controlling terminals (`CONOUT$` / `/dev/tty`), shell integrations, or networked services across an OS/IPC boundary. Define a **port** (trait or callback) at the seam. The deep module owns the logic; I/O is injected as an **adapter**. Tests use an in-memory adapter; production uses the OS/TTY/subprocess adapter.

Recommendation shape: *"Define a port at the seam, implement an OS/subprocess adapter for production and an in-memory adapter for testing, so the logic sits in one deep module even though it crosses an OS boundary."*

### 4. True external (Mock / Fake)

Third-party services or host tools (`pwsh`, `git`, package managers) you don't control. The deepened module takes the external dependency as an injected port; tests provide a deterministic fake adapter returning canned output.

## Seam discipline

- **One adapter means a hypothetical seam. Two adapters means a real one.** Don't introduce a trait or callback port unless at least two adapters are justified (typically production + test). A single-adapter seam is just indirection.
- **Internal seams vs external seams.** A deep module can have internal seams (private to its `mod`, tested via `#[cfg(test)]`) as well as the external seam at its `pub` interface. Don't make internal seams `pub` just because tests use them.

## Testing strategy: replace, don't layer

- Old unit tests on shallow helpers become waste once tests at the deepened module's interface exist; delete them.
- Write new tests at the deepened module's interface. The **interface is the test surface**.
- Tests assert on observable outcomes through the interface (preview strings, `SessionOutcome`, rendered frames, cached items), not internal dirty flags or private cache keys.
- Tests should survive internal refactors, since they describe behaviour, not implementation. If a test has to change when the implementation changes, it's testing past the interface.
