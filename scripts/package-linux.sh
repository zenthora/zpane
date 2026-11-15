#!/usr/bin/env bash
set -euo pipefail
umask 022

VERSION="${1:-0.5.1}"
VERSION="${VERSION#v}"
REL="1"

if [[ ! "${VERSION}" =~ ^[0-9]+\.[0-9]+\.[0-9]+([.~+-][0-9A-Za-z.]+)?$ ]]; then
    echo "Error: invalid package version '${VERSION}'" >&2
    exit 1
fi
if [[ ! "${REL}" =~ ^[0-9]+$ ]]; then
    echo "Error: invalid package release '${REL}'" >&2
    exit 1
fi

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
DIST_DIR="${ROOT_DIR}/dist"
WORK_DIR="$(mktemp -d)"
chmod 0700 "${WORK_DIR}"
trap '[[ -n "${WORK_DIR:-}" ]] && rm -rf "${WORK_DIR}"' EXIT

mkdir -p "${DIST_DIR}"
chmod 0755 "${DIST_DIR}"

if [[ -z "${SOURCE_DATE_EPOCH:-}" ]]; then
    if command -v git >/dev/null 2>&1 && git -C "${ROOT_DIR}" rev-parse --git-dir >/dev/null 2>&1; then
        SOURCE_DATE_EPOCH="$(git -C "${ROOT_DIR}" log -1 --format=%ct 2>/dev/null || date +%s)"
    else
        SOURCE_DATE_EPOCH="$(date +%s)"
    fi
fi
if [[ ! "${SOURCE_DATE_EPOCH}" =~ ^[0-9]+$ ]]; then
    echo "Error: invalid SOURCE_DATE_EPOCH '${SOURCE_DATE_EPOCH}'" >&2
    exit 1
fi
export SOURCE_DATE_EPOCH

X86_BIN="${ROOT_DIR}/target/x86_64-unknown-linux-musl/release/zpane"
ARM_BIN="${ROOT_DIR}/target/aarch64-unknown-linux-musl/release/zpane"
README_FILE="${ROOT_DIR}/README.md"
LICENSE_FILE="${ROOT_DIR}/LICENSE"

for req in "${X86_BIN}" "${ARM_BIN}" "${README_FILE}" "${LICENSE_FILE}"; do
    if [[ ! -f "${req}" ]]; then
        echo "Error: required input file not found: ${req}" >&2
        exit 1
    fi
done

PYTHON_BIN=""
if command -v python >/dev/null 2>&1 && python --version >/dev/null 2>&1; then
    PYTHON_BIN="python"
elif command -v python3 >/dev/null 2>&1 && python3 --version >/dev/null 2>&1; then
    PYTHON_BIN="python3"
fi

host_path() {
    if command -v cygpath >/dev/null 2>&1; then
        cygpath -w -- "$1"
    else
        printf '%s\n' "$1"
    fi
}

is_gnu_tar() {
    tar --version 2>/dev/null | grep -qi 'gnu tar'
}

touch_tree_fallback() {
    local path="$1"
    if [[ -L "${path}" ]]; then
        return 0
    fi
    if [[ -d "${path}" ]]; then
        local child
        local children=()
        while IFS= read -r -d '' child; do
            children+=("${child}")
        done < <(
            (
                shopt -s dotglob nullglob
                for c in "${path}"/*; do
                    printf '%s\0' "${c}"
                done
            )
        )
        for child in "${children[@]}"; do
            touch_tree_fallback "${child}"
        done
    fi
    touch -d "@${SOURCE_DATE_EPOCH}" "${path}"
}

stamp_mtime() {
    local target="$1"
    if [[ -n "${PYTHON_BIN}" ]]; then
        "${PYTHON_BIN}" - "$(host_path "${target}")" "${SOURCE_DATE_EPOCH}" <<'PY'
import os
import sys

root_path = sys.argv[1]
epoch = int(sys.argv[2])
if os.path.isdir(root_path):
    for current_root, dirs, files in os.walk(root_path, topdown=False, followlinks=False):
        for name in files:
            p = os.path.join(current_root, name)
            if os.path.islink(p):
                raise RuntimeError(f"Refusing symlink in staging tree: {p}")
            os.utime(p, (epoch, epoch))
        for name in dirs:
            p = os.path.join(current_root, name)
            if os.path.islink(p):
                raise RuntimeError(f"Refusing symlink in staging tree: {p}")
            os.utime(p, (epoch, epoch))
    os.utime(root_path, (epoch, epoch))
elif os.path.exists(root_path):
    if os.path.islink(root_path):
        raise RuntimeError(f"Refusing symlink in staging tree: {root_path}")
    os.utime(root_path, (epoch, epoch))
PY
    elif touch -d "@${SOURCE_DATE_EPOCH}" "${WORK_DIR}/.stamp-probe" 2>/dev/null; then
        rm -f "${WORK_DIR}/.stamp-probe"
        touch_tree_fallback "${target}"
    fi
}

build_deb_fallback() {
    local stage_dir="$1"
    local out_deb="$2"
    local exe_mode="$3"
    local bin_dir_mode="$4"
    if [[ -z "${PYTHON_BIN}" ]]; then
        echo "Error: dpkg-deb not found and Python fallback unavailable" >&2
        exit 1
    fi
    "${PYTHON_BIN}" - "$(host_path "${stage_dir}")" "$(host_path "${out_deb}")" "${exe_mode}" "${bin_dir_mode}" <<'PY'
import gzip
import io
import os
import sys
import tarfile
import time

stage_dir, out_deb, exe_mode_str, bin_dir_mode_str = sys.argv[1:5]
exe_mode = int(exe_mode_str, 8)
bin_dir_mode = int(bin_dir_mode_str, 8)
if (exe_mode & 0o022) != 0 or (bin_dir_mode & 0o022) != 0:
    raise RuntimeError("Refusing group/world-writable mode in package")
now = int(os.environ.get("SOURCE_DATE_EPOCH") or time.time())


def add_bytes_to_tar(tf: tarfile.TarFile, arcname: str, data: bytes, mode: int, is_dir: bool = False) -> None:
    if (mode & 0o022) != 0:
        raise RuntimeError(f"Unsafe writable mode {oct(mode)} for {arcname}")
    ti = tarfile.TarInfo(name=arcname)
    ti.uid = 0
    ti.gid = 0
    ti.uname = "root"
    ti.gname = "root"
    ti.mtime = now
    ti.mode = mode
    if is_dir:
        ti.type = tarfile.DIRTYPE
        ti.size = 0
        tf.addfile(ti)
    else:
        ti.size = len(data)
        tf.addfile(ti, io.BytesIO(data))


ctrl_buf = io.BytesIO()
with gzip.GzipFile(filename="", fileobj=ctrl_buf, mode="wb", compresslevel=9, mtime=0) as gz:
    with tarfile.open(fileobj=gz, mode="w", format=tarfile.USTAR_FORMAT) as tf:
        add_bytes_to_tar(tf, "./", b"", 0o755, is_dir=True)
        ctrl_path = os.path.join(stage_dir, "DEBIAN", "control")
        with open(ctrl_path, "rb") as f:
            add_bytes_to_tar(tf, "./control", f.read(), 0o644)

data_buf = io.BytesIO()
with gzip.GzipFile(filename="", fileobj=data_buf, mode="wb", compresslevel=9, mtime=0) as gz:
    with tarfile.open(fileobj=gz, mode="w", format=tarfile.USTAR_FORMAT) as tf:
        add_bytes_to_tar(tf, "./", b"", 0o755, is_dir=True)
        for root, dirs, files in os.walk(stage_dir, followlinks=False):
            dirs.sort()
            files.sort()
            rel_root = os.path.relpath(root, stage_dir).replace(os.sep, "/")
            if rel_root == "DEBIAN" or rel_root.startswith("DEBIAN/"):
                continue
            dirs[:] = [d for d in dirs if not (rel_root == "." and d == "DEBIAN")]
            if rel_root != ".":
                dmode = bin_dir_mode if (rel_root == "bin" or rel_root.endswith("/bin")) else 0o755
                add_bytes_to_tar(tf, "./" + rel_root + "/", b"", dmode, is_dir=True)
            for fn in files:
                full = os.path.join(root, fn)
                if os.path.islink(full):
                    raise RuntimeError(f"Refusing symlink in deb staging: {full}")
                arc = ("./" + fn) if rel_root == "." else ("./" + rel_root + "/" + fn)
                mode = exe_mode if fn == "zpane" else 0o644
                with open(full, "rb") as f:
                    add_bytes_to_tar(tf, arc, f.read(), mode)


def ar_member(name: str, payload: bytes) -> bytes:
    header = f"{name:<16}{now:<12}{0:<6}{0:<6}{100644:<8}{len(payload):<10}`\n".encode("ascii")
    out = header + payload
    if len(out) % 2 == 1:
        out += b"\n"
    return out


with open(out_deb, "wb") as f:
    f.write(b"!<arch>\n")
    f.write(ar_member("debian-binary", b"2.0\n"))
    f.write(ar_member("control.tar.gz", ctrl_buf.getvalue()))
    f.write(ar_member("data.tar.gz", data_buf.getvalue()))
PY
}

build_tar_py() {
    local stage_dir="$1"
    local out_path="$2"
    local exe_mode="$3"
    local mode_flag="$4"
    shift 4
    if [[ -z "${PYTHON_BIN}" ]]; then
        echo "Error: Python fallback unavailable for tar creation" >&2
        exit 1
    fi
    "${PYTHON_BIN}" - "$(host_path "${stage_dir}")" "$(host_path "${out_path}")" "${exe_mode}" "${mode_flag}" "$@" <<'PY'
import gzip
import io
import os
import sys
import tarfile
import time

stage_dir, out_path, exe_mode_str, tar_mode, *items = sys.argv[1:]
exe_mode = int(exe_mode_str, 8)
if (exe_mode & 0o022) != 0:
    raise RuntimeError("Refusing group/world-writable mode in tar")
now = int(os.environ.get("SOURCE_DATE_EPOCH") or time.time())


def add_path(tf: tarfile.TarFile, base: str, rel: str) -> None:
    full = os.path.join(base, rel)
    if os.path.islink(full):
        raise RuntimeError(f"Refusing symlink in tar staging: {full}")
    ti = tarfile.TarInfo(name=rel.replace(os.sep, "/"))
    ti.uid = 0
    ti.gid = 0
    ti.uname = "root"
    ti.gname = "root"
    ti.mtime = now
    if os.path.isdir(full):
        ti.type = tarfile.DIRTYPE
        ti.mode = 0o755
        tf.addfile(ti)
        for name in sorted(os.listdir(full)):
            add_path(tf, base, os.path.join(rel, name))
    else:
        ti.mode = exe_mode if os.path.basename(rel) == "zpane" else 0o644
        if (ti.mode & 0o022) != 0:
            raise RuntimeError(f"Unsafe mode {oct(ti.mode)} for {rel}")
        with open(full, "rb") as f:
            data = f.read()
        ti.size = len(data)
        tf.addfile(ti, io.BytesIO(data))


if tar_mode == "w:gz":
    with open(out_path, "wb") as raw_out:
        with gzip.GzipFile(filename="", fileobj=raw_out, mode="wb", compresslevel=9, mtime=0) as gz:
            with tarfile.open(fileobj=gz, mode="w", format=tarfile.USTAR_FORMAT) as tf:
                for item in items:
                    add_path(tf, stage_dir, item)
else:
    with tarfile.open(out_path, mode=tar_mode, format=tarfile.USTAR_FORMAT) as tf:
        for item in items:
            add_path(tf, stage_dir, item)
PY
}

build_rpm_fallback() {
    local version="$1"
    local rel="$2"
    local x86_bin="$3"
    local readme="$4"
    local license_file="$5"
    local out_rpm="$6"
    if [[ -z "${PYTHON_BIN}" ]]; then
        echo "Error: rpmbuild not found and Python fallback unavailable" >&2
        exit 1
    fi
    "${PYTHON_BIN}" - "${version}" "${rel}" "$(host_path "${x86_bin}")" "$(host_path "${readme}")" "$(host_path "${license_file}")" "$(host_path "${out_rpm}")" <<'PY'
import gzip
import hashlib
import os
import struct
import sys
import time

version, rel, x86_bin, readme, license_file, out_rpm = sys.argv[1:7]
mtime = int(os.environ.get("SOURCE_DATE_EPOCH") or time.time())

files_spec = [
    ("usr/bin/zpane", "/usr/bin/", "zpane", x86_bin, 0o100755, 0),
    ("usr/share/doc/zpane/README.md", "/usr/share/doc/zpane/", "README.md", readme, 0o100644, 2),
    ("usr/share/licenses/zpane/LICENSE", "/usr/share/licenses/zpane/", "LICENSE", license_file, 0o100644, 128),
]
blobs = []
for _, _, _, src, mode, _ in files_spec:
    if (mode & 0o022) != 0:
        raise RuntimeError(f"Unsafe mode {oct(mode)} in RPM spec")
    with open(src, "rb") as f:
        blobs.append(f.read())

cpio = bytearray()
for idx, (cpio_name, _, _, _, mode, _) in enumerate(files_spec):
    data = blobs[idx]
    name_bytes = cpio_name.encode("utf-8") + b"\x00"
    hdr = (
        f"070701{idx + 1:08x}{mode:08x}{0:08x}{0:08x}{1:08x}{mtime:08x}"
        f"{len(data):08x}{0:08x}{0:08x}{0:08x}{0:08x}{len(name_bytes):08x}{0:08x}"
    ).encode("ascii")
    cpio.extend(hdr + name_bytes)
    while len(cpio) % 4:
        cpio.append(0)
    cpio.extend(data)
    while len(cpio) % 4:
        cpio.append(0)

trailer_name = b"TRAILER!!!\x00"
trailer_hdr = f"070701{0:08x}{0:08x}{0:08x}{0:08x}{1:08x}{0:08x}{0:08x}{0:08x}{0:08x}{0:08x}{0:08x}{len(trailer_name):08x}{0:08x}".encode("ascii")
cpio.extend(trailer_hdr + trailer_name)
while len(cpio) % 512:
    cpio.append(0)
cpio_bytes = bytes(cpio)
payload_gz = gzip.compress(cpio_bytes, compresslevel=9, mtime=0)


def pack_rpm_header(entries: list[tuple[int, int, int, bytes]]) -> bytes:
    entries = sorted(entries, key=lambda e: e[0])
    align_map = {3: 2, 4: 4, 5: 8}
    store = bytearray()
    index = bytearray()
    for tag, typ, count, raw in entries:
        align = align_map.get(typ, 1)
        while len(store) % align:
            store.append(0)
        offset = len(store)
        store.extend(raw)
        index.extend(struct.pack(">IIII", tag, typ, offset, count))
    return b"\x8e\xad\xe8\x01\x00\x00\x00\x00" + struct.pack(">II", len(entries), len(store)) + index + store


def str_arr(items: list[str]) -> bytes:
    return b"".join(s.encode("utf-8") + b"\x00" for s in items)


main_entries = [
    (100, 8, 1, str_arr(["C"])),
    (1000, 6, 1, b"zpane\x00"),
    (1001, 6, 1, version.encode("utf-8") + b"\x00"),
    (1002, 6, 1, rel.encode("utf-8") + b"\x00"),
    (1004, 9, 1, b"3-pane Miller column command, subcommand, parameter & variable explorer\x00"),
    (1005, 9, 1, b"zpane brings 3-column Miller file manager navigation to shell command construction.\x00"),
    (1006, 4, 1, struct.pack(">I", mtime)),
    (1009, 4, 1, struct.pack(">I", sum(len(b) for b in blobs))),
    (1014, 6, 1, b"MIT\x00"),
    (1016, 9, 1, b"Applications/System\x00"),
    (1020, 6, 1, b"https://github.com/zenthora/zpane\x00"),
    (1021, 6, 1, b"linux\x00"),
    (1022, 6, 1, b"x86_64\x00"),
    (1028, 4, 3, struct.pack(">III", *(len(b) for b in blobs))),
    (1030, 3, 3, struct.pack(">HHH", *(f[4] for f in files_spec))),
    (1034, 4, 3, struct.pack(">III", mtime, mtime, mtime)),
    (1035, 8, 3, str_arr([hashlib.md5(b).hexdigest() for b in blobs])),
    (1036, 8, 3, str_arr(["", "", ""])),
    (1037, 4, 3, struct.pack(">III", *(f[5] for f in files_spec))),
    (1039, 8, 3, str_arr(["root", "root", "root"])),
    (1040, 8, 3, str_arr(["root", "root", "root"])),
    (1116, 4, 3, struct.pack(">III", 0, 1, 2)),
    (1117, 8, 3, str_arr([f[2] for f in files_spec])),
    (1118, 8, 3, str_arr([f[1] for f in files_spec])),
    (1124, 6, 1, b"cpio\x00"),
    (1125, 6, 1, b"gzip\x00"),
    (1126, 6, 1, b"9\x00"),
]
main_hdr = pack_rpm_header(main_entries)
sha1_hex = hashlib.sha1(main_hdr).hexdigest().encode("ascii") + b"\x00"
md5_digest = hashlib.md5(main_hdr + payload_gz).digest()
sig_entries = [
    (269, 6, 1, sha1_hex),
    (1000, 4, 1, struct.pack(">I", len(main_hdr) + len(payload_gz))),
    (1004, 7, len(md5_digest), md5_digest),
    (1007, 4, 1, struct.pack(">I", len(cpio_bytes))),
]
sig_hdr = pack_rpm_header(sig_entries)
sig_pad = b"\x00" * ((8 - len(sig_hdr) % 8) % 8)

nvr = f"zpane-{version}-{rel}".encode("utf-8")[:65]
lead = struct.pack(">4sBBHH66sHH16x", b"\xed\xab\xee\xdb", 3, 0, 0, 1, nvr, 1, 5)
with open(out_rpm, "wb") as f:
    f.write(lead)
    f.write(sig_hdr)
    f.write(sig_pad)
    f.write(main_hdr)
    f.write(payload_gz)
PY
}

generate_sha256sums() {
    local target_dir="$1"
    shift
    local artifacts=("$@")
    (
        cd "${target_dir}"
        rm -f SHA256SUMS SHA256SUMS.tmp
        local sorted=()
        while IFS= read -r item; do
            sorted+=("${item}")
        done < <(printf '%s\n' "${artifacts[@]}" | LC_ALL=C sort)

        if [[ ${#sorted[@]} -eq 0 ]]; then
            echo "Error: no package artifacts to checksum in ${target_dir}" >&2
            exit 1
        fi

        if command -v sha256sum >/dev/null 2>&1; then
            sha256sum -- "${sorted[@]}" | sed -e 's/^\([0-9a-f]\{64\}\) \*/\1  /' > SHA256SUMS.tmp
            mv SHA256SUMS.tmp SHA256SUMS
            sha256sum -c -- SHA256SUMS
        elif command -v shasum >/dev/null 2>&1; then
            shasum -a 256 -- "${sorted[@]}" | sed -e 's/^\([0-9a-f]\{64\}\) \*/\1  /' > SHA256SUMS.tmp
            mv SHA256SUMS.tmp SHA256SUMS
            shasum -a 256 -c -- SHA256SUMS
        elif [[ -n "${PYTHON_BIN}" ]]; then
            "${PYTHON_BIN}" - "${sorted[@]}" <<'PY'
import hashlib
import os
import sys

lines = []
for name in sys.argv[1:]:
    h = hashlib.sha256()
    with open(name, "rb") as f:
        for chunk in iter(lambda: f.read(65536), b""):
            h.update(chunk)
    lines.append(f"{h.hexdigest()}  {name}\n")

with open("SHA256SUMS.tmp", "w", encoding="utf-8", newline="\n") as out:
    out.writelines(lines)
os.replace("SHA256SUMS.tmp", "SHA256SUMS")
PY
        else
            echo "Error: no SHA-256 tool available (sha256sum, shasum, or python)" >&2
            exit 1
        fi
        chmod 0644 SHA256SUMS
    )
}

BUILT_ARTIFACTS=()

# 1. Debian / Ubuntu package (zpane_<ver>-<rel>_amd64.deb)
DEB_STAGE="${WORK_DIR}/deb-amd64"
mkdir -p "${DEB_STAGE}/DEBIAN" \
         "${DEB_STAGE}/usr/bin" \
         "${DEB_STAGE}/usr/share/doc/zpane"

install -m 0755 "${X86_BIN}" "${DEB_STAGE}/usr/bin/zpane"
install -m 0644 "${README_FILE}" "${DEB_STAGE}/usr/share/doc/zpane/README.md"
install -m 0644 "${LICENSE_FILE}" "${DEB_STAGE}/usr/share/doc/zpane/copyright"

INSTALLED_KB="$(du -sk "${DEB_STAGE}/usr" | cut -f1)"
cat > "${DEB_STAGE}/DEBIAN/control" <<EOF
Package: zpane
Version: ${VERSION}-${REL}
Section: utils
Priority: optional
Architecture: amd64
Installed-Size: ${INSTALLED_KB}
Maintainer: zenthora <105382125+zenthora@users.noreply.github.com>
Homepage: https://github.com/zenthora/zpane
Description: 3-pane Miller column command, subcommand, parameter & variable explorer
 zpane brings the spatial orientation of 3-column Miller file managers
 to interactive shell command construction across PowerShell and POSIX shells.
EOF

chmod 0755 \
    "${DEB_STAGE}" \
    "${DEB_STAGE}/DEBIAN" \
    "${DEB_STAGE}/usr" \
    "${DEB_STAGE}/usr/bin" \
    "${DEB_STAGE}/usr/share" \
    "${DEB_STAGE}/usr/share/doc" \
    "${DEB_STAGE}/usr/share/doc/zpane"
chmod 0755 "${DEB_STAGE}/usr/bin/zpane"
chmod 0644 \
    "${DEB_STAGE}/DEBIAN/control" \
    "${DEB_STAGE}/usr/share/doc/zpane/README.md" \
    "${DEB_STAGE}/usr/share/doc/zpane/copyright"
stamp_mtime "${DEB_STAGE}"

DEB_NAME="zpane_${VERSION}-${REL}_amd64.deb"
if command -v dpkg-deb >/dev/null 2>&1; then
    dpkg-deb --root-owner-group -Zgzip -z9 --build "${DEB_STAGE}" "${DIST_DIR}/${DEB_NAME}"
else
    build_deb_fallback "${DEB_STAGE}" "${DIST_DIR}/${DEB_NAME}" "0755" "0755"
fi
chmod 0644 "${DIST_DIR}/${DEB_NAME}"
BUILT_ARTIFACTS+=("${DEB_NAME}")

# 2. Termux Android aarch64 package (zpane_<ver>_aarch64.deb)
TERMUX_PREFIX="/data/data/com.termux/files/usr"
TERMUX_STAGE="${WORK_DIR}/termux-aarch64"
mkdir -p "${TERMUX_STAGE}/DEBIAN" \
         "${TERMUX_STAGE}${TERMUX_PREFIX}/bin" \
         "${TERMUX_STAGE}${TERMUX_PREFIX}/share/doc/zpane"

install -m 0700 "${ARM_BIN}" "${TERMUX_STAGE}${TERMUX_PREFIX}/bin/zpane"
install -m 0644 "${README_FILE}" "${TERMUX_STAGE}${TERMUX_PREFIX}/share/doc/zpane/README.md"
install -m 0644 "${LICENSE_FILE}" "${TERMUX_STAGE}${TERMUX_PREFIX}/share/doc/zpane/LICENSE"

TERMUX_KB="$(du -sk "${TERMUX_STAGE}/data" | cut -f1)"
cat > "${TERMUX_STAGE}/DEBIAN/control" <<EOF
Package: zpane
Version: ${VERSION}
Section: utils
Priority: optional
Architecture: aarch64
Installed-Size: ${TERMUX_KB}
Maintainer: @zenthora
Homepage: https://github.com/zenthora/zpane
Description: 3-pane Miller column command, subcommand, parameter & variable explorer
 Static aarch64 build of zpane packaged for Termux on Android.
EOF

chmod 0755 \
    "${TERMUX_STAGE}" \
    "${TERMUX_STAGE}/DEBIAN" \
    "${TERMUX_STAGE}/data" \
    "${TERMUX_STAGE}/data/data" \
    "${TERMUX_STAGE}/data/data/com.termux" \
    "${TERMUX_STAGE}/data/data/com.termux/files" \
    "${TERMUX_STAGE}${TERMUX_PREFIX}" \
    "${TERMUX_STAGE}${TERMUX_PREFIX}/share" \
    "${TERMUX_STAGE}${TERMUX_PREFIX}/share/doc" \
    "${TERMUX_STAGE}${TERMUX_PREFIX}/share/doc/zpane"
chmod 0700 \
    "${TERMUX_STAGE}${TERMUX_PREFIX}/bin" \
    "${TERMUX_STAGE}${TERMUX_PREFIX}/bin/zpane"
chmod 0644 \
    "${TERMUX_STAGE}/DEBIAN/control" \
    "${TERMUX_STAGE}${TERMUX_PREFIX}/share/doc/zpane/README.md" \
    "${TERMUX_STAGE}${TERMUX_PREFIX}/share/doc/zpane/LICENSE"
stamp_mtime "${TERMUX_STAGE}"

TERMUX_DEB_NAME="zpane_${VERSION}_aarch64.deb"
if command -v dpkg-deb >/dev/null 2>&1; then
    dpkg-deb --root-owner-group -Zgzip -z9 --build "${TERMUX_STAGE}" "${DIST_DIR}/${TERMUX_DEB_NAME}"
else
    build_deb_fallback "${TERMUX_STAGE}" "${DIST_DIR}/${TERMUX_DEB_NAME}" "0700" "0700"
fi
chmod 0644 "${DIST_DIR}/${TERMUX_DEB_NAME}"
BUILT_ARTIFACTS+=("${TERMUX_DEB_NAME}")

# Standalone Termux / ARM64 tarball
TERMUX_TAR_NAME="zpane-v${VERSION}-aarch64-linux-musl.tar.gz"
if [[ -n "${PYTHON_BIN}" ]] && { command -v cygpath >/dev/null 2>&1 || ! is_gnu_tar; }; then
    build_tar_py "${TERMUX_STAGE}${TERMUX_PREFIX}/bin" "${DIST_DIR}/${TERMUX_TAR_NAME}" "0700" "w:gz" zpane
else
    tar --sort=name --mtime="@${SOURCE_DATE_EPOCH}" --owner=0 --group=0 --numeric-owner \
        -C "${TERMUX_STAGE}${TERMUX_PREFIX}/bin" -cf - zpane | gzip -9n > "${DIST_DIR}/${TERMUX_TAR_NAME}"
fi
chmod 0644 "${DIST_DIR}/${TERMUX_TAR_NAME}"
BUILT_ARTIFACTS+=("${TERMUX_TAR_NAME}")

# 3. Arch Linux pacman package (zpane-<ver>-<rel>-x86_64.pkg.tar.zst / .gz)
ARCH_STAGE="${WORK_DIR}/arch-x86_64"
mkdir -p "${ARCH_STAGE}/usr/bin" \
         "${ARCH_STAGE}/usr/share/doc/zpane" \
         "${ARCH_STAGE}/usr/share/licenses/zpane"

install -m 0755 "${X86_BIN}" "${ARCH_STAGE}/usr/bin/zpane"
install -m 0644 "${README_FILE}" "${ARCH_STAGE}/usr/share/doc/zpane/README.md"
install -m 0644 "${LICENSE_FILE}" "${ARCH_STAGE}/usr/share/licenses/zpane/LICENSE"

ARCH_BYTES="$(du -sb "${ARCH_STAGE}/usr" | cut -f1)"
BUILD_DATE="${SOURCE_DATE_EPOCH}"
cat > "${ARCH_STAGE}/.PKGINFO" <<EOF
pkgname = zpane
pkgbase = zpane
pkgver = ${VERSION}-${REL}
pkgdesc = Blazing-fast 3-pane Miller Column Command, Subcommand, Parameter & Variable Explorer
url = https://github.com/zenthora/zpane
builddate = ${BUILD_DATE}
packager = zenthora <105382125+zenthora@users.noreply.github.com>
size = ${ARCH_BYTES}
arch = x86_64
license = MIT
optdepend = powershell: PowerShell cmdlet and parameter-set reflection
EOF

chmod 0755 \
    "${ARCH_STAGE}" \
    "${ARCH_STAGE}/usr" \
    "${ARCH_STAGE}/usr/bin" \
    "${ARCH_STAGE}/usr/share" \
    "${ARCH_STAGE}/usr/share/doc" \
    "${ARCH_STAGE}/usr/share/doc/zpane" \
    "${ARCH_STAGE}/usr/share/licenses" \
    "${ARCH_STAGE}/usr/share/licenses/zpane"
chmod 0755 "${ARCH_STAGE}/usr/bin/zpane"
chmod 0644 \
    "${ARCH_STAGE}/.PKGINFO" \
    "${ARCH_STAGE}/usr/share/doc/zpane/README.md" \
    "${ARCH_STAGE}/usr/share/licenses/zpane/LICENSE"
stamp_mtime "${ARCH_STAGE}"

ARCH_ZST_NAME="zpane-${VERSION}-${REL}-x86_64.pkg.tar.zst"
ARCH_GZ_NAME="zpane-${VERSION}-${REL}-x86_64.pkg.tar.gz"
if [[ -n "${PYTHON_BIN}" ]] && { command -v cygpath >/dev/null 2>&1 || ! is_gnu_tar; }; then
    ARCH_TAR="${WORK_DIR}/arch-x86_64.tar"
    build_tar_py "${ARCH_STAGE}" "${ARCH_TAR}" "0755" "w" .PKGINFO usr
    if command -v zstd >/dev/null 2>&1; then
        zstd -f -q -19 "${ARCH_TAR}" -o "${DIST_DIR}/${ARCH_ZST_NAME}"
        chmod 0644 "${DIST_DIR}/${ARCH_ZST_NAME}"
        BUILT_ARTIFACTS+=("${ARCH_ZST_NAME}")
    elif command -v zstd.exe >/dev/null 2>&1; then
        zstd.exe -f -q -19 "$(host_path "${ARCH_TAR}")" -o "$(host_path "${DIST_DIR}/${ARCH_ZST_NAME}")"
        chmod 0644 "${DIST_DIR}/${ARCH_ZST_NAME}"
        BUILT_ARTIFACTS+=("${ARCH_ZST_NAME}")
    else
        gzip -9n -c "${ARCH_TAR}" > "${DIST_DIR}/${ARCH_GZ_NAME}"
        chmod 0644 "${DIST_DIR}/${ARCH_GZ_NAME}"
        BUILT_ARTIFACTS+=("${ARCH_GZ_NAME}")
    fi
elif command -v zstd >/dev/null 2>&1; then
    tar --sort=name --mtime="@${SOURCE_DATE_EPOCH}" --owner=0 --group=0 --numeric-owner \
        -C "${ARCH_STAGE}" -cf - .PKGINFO usr | zstd -q -19 > "${DIST_DIR}/${ARCH_ZST_NAME}"
    chmod 0644 "${DIST_DIR}/${ARCH_ZST_NAME}"
    BUILT_ARTIFACTS+=("${ARCH_ZST_NAME}")
elif command -v zstd.exe >/dev/null 2>&1; then
    tar --sort=name --mtime="@${SOURCE_DATE_EPOCH}" --owner=0 --group=0 --numeric-owner \
        -C "${ARCH_STAGE}" -cf - .PKGINFO usr | zstd.exe -q -19 > "${DIST_DIR}/${ARCH_ZST_NAME}"
    chmod 0644 "${DIST_DIR}/${ARCH_ZST_NAME}"
    BUILT_ARTIFACTS+=("${ARCH_ZST_NAME}")
else
    tar --sort=name --mtime="@${SOURCE_DATE_EPOCH}" --owner=0 --group=0 --numeric-owner \
        -C "${ARCH_STAGE}" -cf - .PKGINFO usr | gzip -9n > "${DIST_DIR}/${ARCH_GZ_NAME}"
    chmod 0644 "${DIST_DIR}/${ARCH_GZ_NAME}"
    BUILT_ARTIFACTS+=("${ARCH_GZ_NAME}")
fi

# 4. Fedora / RHEL / openSUSE RPM package (zpane-<ver>-<rel>.x86_64.rpm)
RPM_VERSION="${VERSION//-/\~}"
RPM_NAME="zpane-${RPM_VERSION}-${REL}.x86_64.rpm"
if command -v rpmbuild >/dev/null 2>&1; then
    RPM_TOP="${WORK_DIR}/rpmbuild"
    mkdir -p "${RPM_TOP}"/{BUILD,RPMS,SOURCES,SPECS,SRPMS}
    chmod 0755 "${RPM_TOP}" "${RPM_TOP}/BUILD" "${RPM_TOP}/RPMS" "${RPM_TOP}/SOURCES" "${RPM_TOP}/SPECS" "${RPM_TOP}/SRPMS"
    install -m 0755 "${X86_BIN}" "${RPM_TOP}/SOURCES/zpane"
    install -m 0644 "${README_FILE}" "${RPM_TOP}/SOURCES/README.md"
    install -m 0644 "${LICENSE_FILE}" "${RPM_TOP}/SOURCES/LICENSE"
    sed -e "s/^Version:.*/Version:        ${RPM_VERSION}/" \
        -e "s/^Release:.*/Release:        ${REL}/" \
        "${ROOT_DIR}/packaging/fedora/zpane.spec" > "${RPM_TOP}/SPECS/zpane.spec"
    chmod 0644 "${RPM_TOP}/SPECS/zpane.spec"
    stamp_mtime "${RPM_TOP}/SOURCES"

    rpmbuild --define "_topdir ${RPM_TOP}" \
             --define "_build_id_links none" \
             --define "use_source_date_epoch_as_buildtime 1" \
             --define "clamp_mtime_to_source_date_epoch 1" \
             --define "_buildhost reproducible" \
             --target x86_64-linux \
             -bb "${RPM_TOP}/SPECS/zpane.spec"

    shopt -s nullglob
    rpm_files=("${RPM_TOP}/RPMS/x86_64/"*.rpm)
    shopt -u nullglob
    if [[ ${#rpm_files[@]} -eq 0 ]]; then
        echo "Error: rpmbuild produced no RPM artifacts" >&2
        exit 1
    fi
    for rpm_path in "${rpm_files[@]}"; do
        rpm_base="$(basename "${rpm_path}")"
        cp -f "${rpm_path}" "${DIST_DIR}/${rpm_base}"
        chmod 0644 "${DIST_DIR}/${rpm_base}"
        BUILT_ARTIFACTS+=("${rpm_base}")
    done
else
    build_rpm_fallback "${RPM_VERSION}" "${REL}" "${X86_BIN}" "${README_FILE}" "${LICENSE_FILE}" "${DIST_DIR}/${RPM_NAME}"
    chmod 0644 "${DIST_DIR}/${RPM_NAME}"
    BUILT_ARTIFACTS+=("${RPM_NAME}")
fi

generate_sha256sums "${DIST_DIR}" "${BUILT_ARTIFACTS[@]}"

echo "Built packages in ${DIST_DIR}:"
ls -lh "${DIST_DIR}"
