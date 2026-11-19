#!/usr/bin/env python3
"""Render Homebrew, Scoop, WinGet (and pinned Termux/Arch) manifests from a release SHA256SUMS.

Templates under ``packaging/`` use ``__VERSION__`` and ``__SHA256_<TARGET>__`` placeholders
(``<TARGET>`` is the Rust target triple upper-cased with ``-`` replaced by ``_``). Every
archive hash is looked up in the release ``SHA256SUMS`` so published manifests always match
the exact artifacts uploaded to the GitHub Release.
"""

from __future__ import annotations

import argparse
import re
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent

VERSION_RE = re.compile(r"^[0-9]+\.[0-9]+\.[0-9]+([.~+-][0-9A-Za-z.]+)?$")
SHA256_RE = re.compile(r"^[0-9a-f]{64}$")

# Rust target triple -> release archive extension.
ARCHIVE_TARGETS: dict[str, str] = {
    "aarch64-apple-darwin": "tar.gz",
    "x86_64-apple-darwin": "tar.gz",
    "aarch64-unknown-linux-musl": "tar.gz",
    "x86_64-unknown-linux-musl": "tar.gz",
    "x86_64-pc-windows-msvc": "zip",
}

# (template path relative to ROOT, output path relative to --out)
TEMPLATES: list[tuple[str, str]] = [
    ("packaging/homebrew/zpane.rb", "homebrew/zpane.rb"),
    ("packaging/scoop/zpane.json", "scoop/zpane.json"),
    ("packaging/winget/zenthora.zpane.yaml", "winget/zenthora.zpane.yaml"),
    ("packaging/winget/zenthora.zpane.installer.yaml", "winget/zenthora.zpane.installer.yaml"),
    ("packaging/winget/zenthora.zpane.locale.en-US.yaml", "winget/zenthora.zpane.locale.en-US.yaml"),
]

PLACEHOLDER_RE = re.compile(r"__(?:VERSION|SHA256_[A-Z0-9_]+)__")


def archive_name(version: str, target: str) -> str:
    return f"zpane-v{version}-{target}.{ARCHIVE_TARGETS[target]}"


def placeholder(target: str) -> str:
    return "__SHA256_" + target.upper().replace("-", "_") + "__"


def parse_sums(text: str) -> dict[str, str]:
    """Parse `sha256sum` output (`<hex>  <name>` or `<hex> *<name>`) into {name: hex}."""
    sums: dict[str, str] = {}
    for raw in text.splitlines():
        line = raw.strip()
        if not line:
            continue
        match = re.fullmatch(r"([0-9A-Fa-f]{64}) [ *](.+)", line)
        if not match:
            raise ValueError(f"Malformed SHA256SUMS line: {raw!r}")
        sums[match.group(2)] = match.group(1).lower()
    return sums


def target_hashes(version: str, sums: dict[str, str]) -> dict[str, str]:
    hashes: dict[str, str] = {}
    for target in ARCHIVE_TARGETS:
        name = archive_name(version, target)
        if name not in sums:
            raise KeyError(f"SHA256SUMS has no entry for {name}")
        hashes[target] = sums[name]
    return hashes


def render_template(text: str, version: str, hashes: dict[str, str]) -> str:
    rendered = text.replace("__VERSION__", version)
    for target, digest in hashes.items():
        rendered = rendered.replace(placeholder(target), digest)
    leftover = PLACEHOLDER_RE.findall(rendered)
    if leftover:
        raise ValueError(f"Unresolved placeholders: {sorted(set(leftover))}")
    return rendered


def pin_source_checksums(termux: str, pkgbuild: str, source_sha256: str) -> tuple[str, str]:
    """Pin the GitHub source tarball digest in the Termux build.sh and Arch PKGBUILD."""
    termux_out, n_termux = re.subn(
        r"^TERMUX_PKG_SHA256=.*$", f"TERMUX_PKG_SHA256={source_sha256}", termux, count=1, flags=re.MULTILINE
    )
    pkg_out, n_pkg = re.subn(
        r"^sha256sums=\(.*\)$", f"sha256sums=('{source_sha256}')", pkgbuild, count=1, flags=re.MULTILINE
    )
    if n_termux != 1 or n_pkg != 1:
        raise ValueError("Could not locate TERMUX_PKG_SHA256 / sha256sums to pin")
    return termux_out, pkg_out


def write(path: Path, text: str) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(text, encoding="utf-8", newline="\n")


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--version", required=True, help="Release SemVer (leading 'v' is accepted)")
    parser.add_argument("--sums", required=True, type=Path, help="Path to the release SHA256SUMS")
    parser.add_argument("--out", required=True, type=Path, help="Output directory for rendered manifests")
    parser.add_argument(
        "--source-sha256",
        help="SHA-256 of the GitHub source tarball; also renders pinned termux/build.sh and arch/PKGBUILD",
    )
    args = parser.parse_args(argv)

    version = args.version.lstrip("v")
    if not VERSION_RE.fullmatch(version):
        print(f"Invalid release version: {version}", file=sys.stderr)
        return 1
    if args.source_sha256 is not None and not SHA256_RE.fullmatch(args.source_sha256.lower()):
        print("--source-sha256 must be 64 hex characters", file=sys.stderr)
        return 1

    try:
        hashes = target_hashes(version, parse_sums(args.sums.read_text(encoding="utf-8")))
        for template, output in TEMPLATES:
            text = (ROOT / template).read_text(encoding="utf-8")
            write(args.out / output, render_template(text, version, hashes))
        if args.source_sha256 is not None:
            termux, pkgbuild = pin_source_checksums(
                (ROOT / "packaging/termux/build.sh").read_text(encoding="utf-8"),
                (ROOT / "packaging/arch/PKGBUILD").read_text(encoding="utf-8"),
                args.source_sha256.lower(),
            )
            write(args.out / "termux/build.sh", termux)
            write(args.out / "arch/PKGBUILD", pkgbuild)
    except (KeyError, ValueError, OSError) as err:
        print(f"Error: {err}", file=sys.stderr)
        return 1

    print(f"Rendered package manifests for v{version} into {args.out}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
