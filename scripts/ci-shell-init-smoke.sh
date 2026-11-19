#!/usr/bin/env bash
# Syntax-checks `zpane init <shell>` output without executing it.
# Usage: scripts/ci-shell-init-smoke.sh <zpane-binary> <bash|zsh|fish>...
set -euo pipefail

if [[ $# -lt 2 ]]; then
    echo "Usage: $0 <zpane-binary> <bash|zsh|fish>..." >&2
    exit 2
fi

ZPANE_BIN="$1"
shift

WORK_DIR="$(mktemp -d)"
trap 'rm -rf "${WORK_DIR}"' EXIT

for shell_name in "$@"; do
    if ! command -v "${shell_name}" >/dev/null 2>&1; then
        echo "Error: ${shell_name} is required but not installed" >&2
        exit 1
    fi
    init_file="${WORK_DIR}/init.${shell_name}"
    "${ZPANE_BIN}" init "${shell_name}" > "${init_file}"
    if [[ ! -s "${init_file}" ]]; then
        echo "Error: 'zpane init ${shell_name}' produced no output" >&2
        exit 1
    fi
    case "${shell_name}" in
        bash | zsh) "${shell_name}" -n "${init_file}" ;;
        fish) fish --no-execute "${init_file}" ;;
        *)
            echo "Error: unsupported shell '${shell_name}'" >&2
            exit 2
            ;;
    esac
    echo "ok: zpane init ${shell_name} parses cleanly"
done
