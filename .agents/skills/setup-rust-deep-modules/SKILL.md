---
name: setup-rust-deep-modules
description: Enforce deep module boundaries in a Rust crate using lib.rs entry points, private submodules, integration tests in tests/, and clippy/rustc checks. User-invoked.
disable-model-invocation: true
---

# Setup Rust Deep Modules

Make each module in this crate a **deep module**: substantial behaviour behind a small public interface, with internals hidden in private submodules. Rust's compiler (`rustc`) enforces module privacy at build time—no external dependency linter required.

For the vocabulary (deep module, interface, seam, depth, adapter), call the Skill tool with "codebase-design" and use its language throughout.

## The shape this enforces

```text
src/
├── lib.rs              # narrow crate surface: re-exports only public seams
├── main.rs             # thin binary shell (CLI args, TTY guard, event loop)
└── <module>/           # or <module>.rs when single-file
    ├── mod.rs          # public module interface + re-exports
    └── inner.rs        # private submodule (no `pub` leakage)
tests/
└── <seam>_test.rs      # integration tests: compiled externally, only see `lib.rs`
```

Four rules enforced by `rustc` and `clippy`:

1. **Narrow crate surface**: `src/lib.rs` exports only the types, traits, and functions callers and integration tests cross. Internal helpers stay private (`mod inner;`) or `pub(crate)` only when sibling modules genuinely share them.
2. **Intra-module freedom**: submodules inside `src/<module>/` share `pub(super)` helpers freely without exposing them outside the module.
3. **Tests through the public seam**: `tests/*.rs` compile as separate external crates against `lib.rs`, so `rustc` rejects any deep import of private internals (`E0603`). Use `#[cfg(test)]` inside a module only for private internal seams.
4. **No dead public surface**: enable `#![warn(unreachable_pub)]` (or `clippy` lints) so accidental `pub` items inside private modules are flagged.

## Steps

### 1. Inspect the crate layout

Read `Cargo.toml`, `src/main.rs`, and `src/lib.rs` (if present). Identify:

- Current modules (`mod ...;` in `src/main.rs` or `src/lib.rs`).
- Whether `src/lib.rs` exists; if binary-only, check whether moving core modules behind `src/lib.rs` makes `tests/*.rs` possible without duplicating `mod` declarations.
- Existing check wiring (`prek.toml`, `.github/workflows/*.yml`, `Cargo.toml` lints).

**Done when:** module list, library/binary split, and check runner are known.

### 2. Establish `src/lib.rs` and module boundaries

- If `src/lib.rs` does not exist, create it to own the crate's modules and `pub use` only the confirmed seam interfaces. Have `src/main.rs` import from `<crate_name>::...` instead of declaring duplicate `mod` trees.
- Audit `pub` items in each module: narrow internal structs, fields, DTOs, and helpers from `pub` to private or `pub(super)` / `pub(crate)`.
- Add `#![warn(unreachable_pub)]` to `src/lib.rs` (or configure `[lints.rust]` / `[lints.clippy]` in `Cargo.toml`) so leaked visibility triggers warnings under `-D warnings`.

**Done when:** `src/lib.rs` exposes only the intended seam surface and `cargo check --all-targets` passes.

### 3. Wire checks into `prek.toml` and CI

Ensure the repo's check pipeline runs:

- `cargo clippy --all-targets -- -D warnings`
- `cargo test --locked`
- `cargo test --doc` (if public items include doc examples)

**Done when:** `prek run --all-files` (or `cargo clippy --all-targets -- -D warnings && cargo test --locked`) enforces visibility and seam tests automatically.

### 4. Scaffold or verify an integration test at the seam

Ensure at least one test in `tests/` exercises a real seam exported from `src/lib.rs` without touching internal state.

**Done when:** `cargo test --test <name>` passes using only the crate's public interface.

### 5. Prove the compiler enforces the boundary

This is the completion criterion for the skill:

1. Run `cargo test --locked`. It must **pass**.
2. Temporarily add an import to a private internal item or submodule from `tests/<name>.rs`. Run `cargo test --test <name>`; it must **fail to compile** (`E0603: module/item is private`).
3. Revert the temporary import. Run `cargo clippy --all-targets -- -D warnings && cargo test --locked`, and it must **pass**.

**Done when:** you have observed pass → compile fail on private import → pass again.

### 6. Document the convention

Add a brief pointer in `AGENTS.md` (or `CONTEXT.md` / `GLOSSARY.md` if present) only if the crate layout is non-obvious, noting that `src/lib.rs` is the public seam and integration tests live in `tests/`. Keep it to one line.

**Done when:** documentation matches the actual crate layout without duplicating `Cargo.toml`.
