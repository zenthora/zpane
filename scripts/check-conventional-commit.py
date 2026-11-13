#!/usr/bin/env python3
"""Validate commit messages against Conventional Commits (used by prek commit-msg)."""

from __future__ import annotations

import re
import sys
from pathlib import Path

CONVENTIONAL_RE = re.compile(
    r"^(feat|fix|perf|refactor|docs|test|chore|ci|build|style|revert)"
    r"(\([a-zA-Z0-9_./-]+\))?(!)?: .+$"
)
BYPASS_PREFIXES = ("Merge ", "Revert ", "fixup! ", "squash! ")


def main() -> int:
    if len(sys.argv) < 2:
        print("Usage: check-conventional-commit.py <commit-msg-file>", file=sys.stderr)
        return 2

    raw = Path(sys.argv[1]).read_text(encoding="utf-8", errors="replace")
    lines = [line for line in raw.splitlines() if not line.startswith("#")]
    subject = next((line.strip() for line in lines if line.strip()), "")

    if not subject:
        print("Commit message is empty.", file=sys.stderr)
        return 1

    if subject.startswith(BYPASS_PREFIXES) or CONVENTIONAL_RE.match(subject):
        return 0

    print(
        f"Invalid commit subject: {subject!r}\n"
        "Expected Conventional Commits format: <type>[optional (<scope>)][!]: <description>\n"
        "Allowed types: feat, fix, perf, refactor, docs, test, chore, ci, build, style, revert\n"
        "Example: feat(session): support custom argument preview",
        file=sys.stderr,
    )
    return 1


if __name__ == "__main__":
    raise SystemExit(main())
