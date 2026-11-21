#!/usr/bin/env python3
"""Compute the next SemVer via git-cliff, sync version files, and update CHANGELOG.md."""

from __future__ import annotations

import argparse
import re
import subprocess
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
SOURCE_SHA256_PLACEHOLDER = "0" * 64


def run(cmd: list[str], *, capture: bool = False) -> str:
    res = subprocess.run(
        cmd,
        cwd=ROOT,
        text=True,
        capture_output=capture,
        check=False,
    )
    if res.returncode != 0:
        if capture and res.stderr:
            print(res.stderr.strip(), file=sys.stderr)
        raise SystemExit(res.returncode)
    return res.stdout.strip() if capture else ""


def next_version(explicit: str | None) -> str:
    if explicit:
        return explicit.lstrip("v")
    raw = run(["git-cliff", "--config", "cliff.toml", "--bumped-version"], capture=True)
    return raw.splitlines()[-1].strip().lstrip("v")


def replace_once(rel_path: str, pattern: str, replacement: str) -> None:
    path = ROOT / rel_path
    text = path.read_text(encoding="utf-8")
    updated, count = re.subn(pattern, replacement, text, count=1, flags=re.MULTILINE)
    if count != 1:
        raise RuntimeError(f"Expected 1 match in {rel_path} for {pattern!r}, found {count}")
    path.write_text(updated, encoding="utf-8", newline="\n")


def replace_all(rel_path: str, pattern: str, replacement: str) -> None:
    path = ROOT / rel_path
    text = path.read_text(encoding="utf-8")
    updated, _ = re.subn(pattern, replacement, text, flags=re.MULTILINE)
    path.write_text(updated, encoding="utf-8", newline="\n")


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--version", help="Explicit SemVer (default: computed via git-cliff --bumped-version)")
    parser.add_argument("--dry-run", action="store_true", help="Print target version without modifying files")
    args = parser.parse_args()

    version = next_version(args.version)
    print(f"Next SemVer: {version}")
    if args.dry_run:
        return 0

    replace_once("Cargo.toml", r'^version = "[^"]+"', f'version = "{version}"')
    replace_once("packaging/arch/PKGBUILD", r"^pkgver=.*$", f"pkgver={version}")
    replace_once("packaging/fedora/zpane.spec", r"^Version:\s+.*$", f"Version:        {version}")
    replace_once("packaging/termux/build.sh", r'^TERMUX_PKG_VERSION="[^"]+"', f'TERMUX_PKG_VERSION="{version}"')
    # A digest for the previous release tarball must never be carried over to the new version.
    # (packaging/{homebrew,scoop,winget}/ keep __VERSION__/__SHA256_*__ placeholders rendered at release time.)
    replace_once("packaging/termux/build.sh", r"^TERMUX_PKG_SHA256=.*$", f"TERMUX_PKG_SHA256={SOURCE_SHA256_PLACEHOLDER}")
    replace_once("scripts/package-linux.sh", r'^VERSION="\$\{1:-[^}]+\}"', f'VERSION="${{1:-{version}}}"')
    replace_all("README.md", r"(zpane[-_]v?)\d+\.\d+\.\d+", rf"\g<1>{version}")

    # Refresh Cargo.lock for the workspace package without touching external dependencies.
    run(["cargo", "update", "--workspace", "--offline"], capture=True)
    run(["git-cliff", "--config", "cliff.toml", "--tag", f"v{version}", "-o", "CHANGELOG.md"])
    print(f"Updated version files and CHANGELOG.md for v{version}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
