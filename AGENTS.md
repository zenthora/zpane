# Agent Guidelines

## Agent skills

### Issue tracker

Issues and specs are tracked as local markdown files under `.scratch/`. See `docs/agents/issue-tracker.md`.

### Triage labels

Default five-role triage label vocabulary (`needs-triage`, `needs-info`, `ready-for-agent`, `ready-for-human`, `wontfix`). See `docs/agents/triage-labels.md`.

### Domain docs

Single-context layout (`CONTEXT.md` + `docs/adr/` at the repo root). See `docs/agents/domain.md`.

## Engineering workflow

### SemVer & releases (`git-cliff`)

Conventional Commits drive SemVer bumps and `CHANGELOG.md` via `cliff.toml` and `scripts/bump-version.py`. Read before committing or preparing a release. See `docs/agents/semver.md`.

### Pre-commit (`prek`)

`prek.toml` enforces hygiene, `cargo fmt`, `cargo clippy`, `cargo test --locked`, and `commit-msg` Conventional Commits. Read when running checks, modifying hooks, or committing. See `docs/agents/pre-commit.md`.

### Testing

Tests run via `cargo test --locked` at module seams using `CONTEXT.md` vocabulary and system-boundary fakes only. Read before writing or modifying tests. See `docs/agents/testing.md`.
