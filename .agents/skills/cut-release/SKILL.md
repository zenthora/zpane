---
name: cut-release
description: Cut a SemVer release of zpane using git-cliff, scripts/bump-version.py, and packaging metadata. Use when bumping versions, updating CHANGELOG.md, creating a vX.Y.Z tag, or preparing a GitHub Release build.
---

# Cut Release (`zpane`)

Prepare and tag a `zpane` release cleanly while keeping `git-cliff` commits, packaging manifests, `README.md`, and `CHANGELOG.md` synchronized. Read `docs/agents/semver.md` and `docs/agents/pre-commit.md` before running release steps.

## Workflow

1. **Check git state and tag baseline**
   - Verify clean working tree (`git status`), current branch (`main`), and latest release tag:
     ```bash
     git tag -l --sort=-v:refname
     ```
   - Inspect commits since the latest `v*` tag (`git log <latest-tag>..HEAD --oneline`).
   - Remember that `git-cliff --bumped-version` derives the next bump (`feat` = minor, `fix` = patch, `!`/`BREAKING CHANGE:` = major) relative to the latest `v*` tag. Ensure every user-visible commit uses Conventional Commits (`cliff.toml`).

2. **Preview and verify dependencies**
   - Run `python scripts/bump-version.py --dry-run` to check the computed SemVer version and confirm it matches expectations (pass `--version X.Y.Z` only when intentionally overriding the computed bump).
   - Optionally run the `rust-supply-chain-audit` skill before cutting a public release.

3. **Apply version bump & update non-automated release notes**
   - Run `prek run --stage manual git-cliff-changelog` (or `python scripts/bump-version.py` directly) so `Cargo.toml`, `packaging/arch/PKGBUILD`, `packaging/termux/build.sh`, `packaging/fedora/zpane.spec` (`Version`), `scripts/package-linux.sh`, `README.md`, `Cargo.lock` (`cargo update --workspace --offline`), and `CHANGELOG.md` are updated together.
   - Inspect `packaging/fedora/zpane.spec` `%changelog` and add the new release entry (it is not updated automatically by `scripts/bump-version.py`).
   - Review `README.md` CLI flags, shell integration snippets, and feature notes against the current code before committing.

4. **Run full verification gates**
   - Run `prek run --all-files` to verify formatting, `cargo clippy`, `cargo test --locked`, TOML/YAML/JSON hygiene, and lockfile consistency.
   - Review `git diff` across all modified manifests and `CHANGELOG.md`.

5. **Commit, tag, and push (only with explicit confirmation)**
   - Commit with a Conventional Commit message, e.g. `chore(release): bump version to X.Y.Z and update CHANGELOG.md`.
   - Create an annotated or lightweight `vX.Y.Z` tag and verify `git show vX.Y.Z --stat`.
   - Ask before pushing if the user has not explicitly requested a push: pushing a `v*` tag triggers `.github/workflows/release.yml` (Windows x86_64 MSVC + Linux x86_64/aarch64 musl release artifacts).

## Completion Criteria

- `Cargo.toml`, `CHANGELOG.md`, `README.md`, and `packaging/` manifests agree on version `X.Y.Z`.
- `prek run --all-files` passes with zero failures after the final edit.
- Commit and tag match `vX.Y.Z`, and CI workflow status is reported if pushed.
