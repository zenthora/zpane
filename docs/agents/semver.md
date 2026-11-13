# SemVer & Releases (`git-cliff`)

`cliff.toml` and `scripts/bump-version.py` govern semantic versioning and changelog generation for this repository.

## Commit format and SemVer impact

Every commit subject must match Conventional Commits (`<type>[optional (<scope>)][!]: <description>`), enforced on `commit-msg` by `scripts/check-conventional-commit.py`.

`git-cliff` computes the next SemVer directly from commits since the latest `v*` tag (configured in `cliff.toml`):

- `feat` bumps **minor** (`features_always_bump_minor = true`)
- `!` or a `BREAKING CHANGE:` footer bumps **major** (`breaking_always_bump_major = true`)
- `fix`, `perf`, `refactor`, `docs`, `test`, `chore`, `ci`, `build`, `style`, `revert` bump **patch**

Choose the commit type and `!` marker carefully—they determine the next release version and how `CHANGELOG.md` groups the change.

## Version bumps and changelogs

Use `scripts/bump-version.py` as the single entry point for version bumps so every manifest stays in sync:

- Preview the next computed version without editing files:
  ```sh
  python scripts/bump-version.py --dry-run
  ```
  (Equivalent to `git-cliff --config cliff.toml --bumped-version`.)
- Apply the next SemVer across `Cargo.toml`, `Cargo.lock` (`cargo update --workspace --offline`), `packaging/`, `scripts/package-linux.sh`, `README.md`, and `CHANGELOG.md`:
  ```sh
  python scripts/bump-version.py
  ```
  Or run the manual `prek` hook:
  ```sh
  prek run --hook-stage manual git-cliff-changelog
  ```
- Override the computed version when needed with `--version X.Y.Z`.

Update versioned files and `CHANGELOG.md` through `scripts/bump-version.py` rather than editing version strings or changelog entries by hand.

## Release tagging

`.github/workflows/release.yml` builds and publishes release packages on `v*` tags (`v0.2.0`, etc.). Commit the `scripts/bump-version.py` output first, verify `prek run --all-files` passes, and tag the release commit with `v<semver>`.
