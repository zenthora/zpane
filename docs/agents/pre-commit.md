# Pre-commit (`prek`)

`prek.toml` defines all git hooks for formatting, linting, tests, file hygiene, Conventional Commits, and `git-cliff` checks.

## Setup and commands

- Install both `pre-commit` and `commit-msg` hook shims once per clone:
  ```sh
  prek install
  ```
- Run all `pre-commit` hooks across the repository before finishing a task:
  ```sh
  prek run --all-files
  ```
- Validate `prek.toml` after modifying hook definitions:
  ```sh
  prek validate-config prek.toml
  ```
- Trigger the manual release-bump hook when cutting a version:
  ```sh
  prek run --hook-stage manual git-cliff-changelog
  ```

## Expectations for agents

- Let `pre-commit` and `commit-msg` hooks run on every commit; bypass with `--no-verify` only if the user explicitly requests it.
- Keep all Rust changes clean under `cargo fmt --all -- --check`, `cargo clippy --all-targets -- -D warnings`, and `cargo test --locked`.
- Keep config, markdown, and script files clean under the builtin hygiene hooks in `prek.toml` (trailing whitespace, single trailing newline, valid TOML/YAML/JSON, no merge-conflict markers or private keys).
- When `cliff.toml` or `CHANGELOG.md` changes, `git-cliff-semver` runs automatically during `pre-commit` to verify `git-cliff --config cliff.toml --bumped-version` succeeds.
