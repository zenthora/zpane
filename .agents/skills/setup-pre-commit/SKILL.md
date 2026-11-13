---
name: setup-pre-commit
description: Set up prek git hooks with cargo fmt, cargo clippy, cargo test, conventional commits, and optional git-cliff SemVer checks in a Rust repo. Use when the user wants pre-commit hooks, prek configuration, commit-msg validation, or commit-time Rust formatting/linting/testing.
---

# Setup Pre-Commit Hooks (`prek`)

## What This Sets Up

- **`prek.toml`** configuring `pre-commit` and `commit-msg` hook stages
- **Builtin hygiene hooks**: trailing whitespace, end-of-file fixer, large-file check, TOML/JSON validation, merge-conflict markers
- **Rust quality hooks**: `cargo fmt --all -- --check`, `cargo clippy --all-targets -- -D warnings`, `cargo test --locked`
- **Conventional Commits & SemVer** (when `cliff.toml` or release automation is used): `commit-msg` validation and `git-cliff` bumped-version checks

## Steps

### 1. Inspect existing setup

Check the repo root for:

- `Cargo.toml` (confirm Rust project layout; note workspace vs single crate)
- `prek.toml` (if present, merge missing hooks rather than overwriting user config)
- `cliff.toml` and `scripts/` (e.g., `check-conventional-commit.py`, `bump-version.py`)
- `prek --version` and `git-cliff --version` availability on `PATH`

### 2. Create or update `prek.toml`

Use `prek.toml` as the single configuration file:

```toml
#:schema https://www.schemastore.org/prek.json

default_install_hook_types = ["pre-commit", "commit-msg"]
default_stages = ["pre-commit"]

[[repos]]
repo = "builtin"
hooks = [
    { id = "trailing-whitespace" },
    { id = "end-of-file-fixer" },
    { id = "check-added-large-files" },
    { id = "check-toml" },
    { id = "check-json" },
    { id = "check-merge-conflict" },
]

[[repos]]
repo = "local"
hooks = [
    { id = "cargo-fmt", name = "cargo fmt", entry = "cargo fmt --all -- --check", language = "system", types = ["rust"], pass_filenames = false },
    { id = "cargo-clippy", name = "cargo clippy", entry = "cargo clippy --all-targets -- -D warnings", language = "system", types = ["rust"], pass_filenames = false },
    { id = "cargo-test", name = "cargo test", entry = "cargo test --locked", language = "system", types = ["rust"], pass_filenames = false },
]
```

If `cliff.toml` is present, also include the `git-cliff` SemVer check and `commit-msg` Conventional Commits hook (matching the repo's scripts).

### 3. Install git hooks

```bash
prek install
```

This installs both `pre-commit` and `commit-msg` hooks per `default_install_hook_types`.

### 4. Verify

- [ ] `prek.toml` parses cleanly (`check-toml` passes)
- [ ] `prek run --all-files` runs `cargo fmt`, `cargo clippy`, and `cargo test` without errors
- [ ] `commit-msg` hook rejects non-conventional messages (when configured)

### 5. Commit

Stage `prek.toml` (and any hook scripts) and commit with a Conventional Commit message, e.g. `chore(hooks): configure prek pre-commit and commit-msg checks`, letting the new hooks validate the commit end-to-end.
