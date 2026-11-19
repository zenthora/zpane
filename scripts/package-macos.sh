#!/usr/bin/env bash
# Builds a deterministic macOS release archive: dist/zpane-v<version>-<target>.tar.gz
# containing zpane, README.md, and LICENSE (fixed ownership/mtime/mode, no embedded filename/timestamp).
# Usage: scripts/package-macos.sh <version> <aarch64-apple-darwin|x86_64-apple-darwin> [binary-path]
set -euo pipefail
umask 022

VERSION="${1:-}"
TARGET="${2:-}"
VERSION="${VERSION#v}"

if [[ ! "${VERSION}" =~ ^[0-9]+\.[0-9]+\.[0-9]+([.~+-][0-9A-Za-z.]+)?$ ]]; then
    echo "Error: invalid package version '${VERSION}'" >&2
    exit 1
fi
case "${TARGET}" in
    aarch64-apple-darwin | x86_64-apple-darwin) ;;
    *)
        echo "Error: unsupported macOS target '${TARGET}'" >&2
        exit 1
        ;;
esac

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
DIST_DIR="${ROOT_DIR}/dist"
BIN_PATH="${3:-${ROOT_DIR}/target/${TARGET}/release/zpane}"

if [[ -z "${SOURCE_DATE_EPOCH:-}" ]]; then
    SOURCE_DATE_EPOCH="$(git -C "${ROOT_DIR}" log -1 --format=%ct 2>/dev/null || date +%s)"
fi
if [[ ! "${SOURCE_DATE_EPOCH}" =~ ^[0-9]+$ ]]; then
    echo "Error: invalid SOURCE_DATE_EPOCH '${SOURCE_DATE_EPOCH}'" >&2
    exit 1
fi
export SOURCE_DATE_EPOCH

for req in "${BIN_PATH}" "${ROOT_DIR}/README.md" "${ROOT_DIR}/LICENSE"; do
    if [[ ! -f "${req}" ]]; then
        echo "Error: required input file not found: ${req}" >&2
        exit 1
    fi
done

PYTHON_BIN=""
for candidate in python python3; do
    if command -v "${candidate}" >/dev/null 2>&1 && "${candidate}" -c "import sys" >/dev/null 2>&1; then
        PYTHON_BIN="${candidate}"
        break
    fi
done
if [[ -z "${PYTHON_BIN}" ]]; then
    echo "Error: python3 is required to build deterministic archives" >&2
    exit 1
fi

host_path() {
    if command -v cygpath >/dev/null 2>&1; then
        cygpath -w -- "$1"
    else
        printf '%s\n' "$1"
    fi
}

mkdir -p "${DIST_DIR}"
ARCHIVE_NAME="zpane-v${VERSION}-${TARGET}.tar.gz"

"${PYTHON_BIN}" - "$(host_path "${DIST_DIR}/${ARCHIVE_NAME}")" "$(host_path "${BIN_PATH}")" "$(host_path "${ROOT_DIR}/README.md")" "$(host_path "${ROOT_DIR}/LICENSE")" <<'PY'
import gzip
import io
import os
import sys
import tarfile

out_path, bin_path, readme_path, license_path = sys.argv[1:5]
mtime = int(os.environ["SOURCE_DATE_EPOCH"])
members = [("zpane", bin_path, 0o755), ("README.md", readme_path, 0o644), ("LICENSE", license_path, 0o644)]

with open(out_path, "wb") as raw:
    with gzip.GzipFile(filename="", fileobj=raw, mode="wb", compresslevel=9, mtime=0) as gz:
        with tarfile.open(fileobj=gz, mode="w", format=tarfile.USTAR_FORMAT) as tf:
            for name, src, mode in members:
                with open(src, "rb") as f:
                    data = f.read()
                info = tarfile.TarInfo(name=name)
                info.size = len(data)
                info.mode = mode
                info.mtime = mtime
                info.uid = info.gid = 0
                info.uname = info.gname = "root"
                tf.addfile(info, io.BytesIO(data))
PY
chmod 0644 "${DIST_DIR}/${ARCHIVE_NAME}"

# Per-archive checksum for build-job verification; publish-release regenerates unified SHA256SUMS.
SUMS_FILE="SHA256SUMS.macos-${TARGET}"
(
    cd "${DIST_DIR}"
    if command -v sha256sum >/dev/null 2>&1; then
        sha256sum -- "${ARCHIVE_NAME}" | sed -e 's/^\([0-9a-f]\{64\}\) \*/\1  /' > "${SUMS_FILE}"
        sha256sum -c -- "${SUMS_FILE}"
    else
        shasum -a 256 -- "${ARCHIVE_NAME}" | sed -e 's/^\([0-9a-f]\{64\}\) \*/\1  /' > "${SUMS_FILE}"
        shasum -a 256 -c -- "${SUMS_FILE}"
    fi
    chmod 0644 "${SUMS_FILE}"
)
