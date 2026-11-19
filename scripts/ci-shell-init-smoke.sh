#!/usr/bin/env bash
# Syntax-checks `zpane init <shell>` output without executing it.
# Usage: scripts/ci-shell-init-smoke.sh <zpane-binary> <bash|zsh|fish|nu>...
set -euo pipefail

if [[ $# -lt 2 ]]; then
    echo "Usage: $0 <zpane-binary> <bash|zsh|fish|nu>..." >&2
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
    for variant in default nobind ctrl_g; do
        init_file="${WORK_DIR}/init.${variant}.${shell_name}"
        case "${variant}" in
            default) "${ZPANE_BIN}" init "${shell_name}" > "${init_file}" ;;
            nobind) "${ZPANE_BIN}" init "${shell_name}" --no-bind > "${init_file}" ;;
            ctrl_g) "${ZPANE_BIN}" init "${shell_name}" --bind Ctrl+g > "${init_file}" ;;
        esac
        if [[ ! -s "${init_file}" ]]; then
            echo "Error: 'zpane init ${shell_name}' (${variant}) produced no output" >&2
            exit 1
        fi
        case "${shell_name}" in
            bash | zsh) "${shell_name}" -n "${init_file}" ;;
            fish) fish --no-execute "${init_file}" ;;
            nu) nu --ide-check 10 "${init_file}" >/dev/null ;;
            *)
                echo "Error: unsupported shell '${shell_name}'" >&2
                exit 2
                ;;
        esac
    done
    echo "ok: zpane init ${shell_name} (default, --no-bind, --bind) parses cleanly"
done
