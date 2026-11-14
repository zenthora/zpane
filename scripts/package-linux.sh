#!/usr/bin/env bash
set -euo pipefail

VERSION="${1:-0.4.1}"
REL="1"
ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
DIST_DIR="${ROOT_DIR}/dist"
WORK_DIR="$(mktemp -d)"
trap 'rm -rf "${WORK_DIR}"' EXIT

mkdir -p "${DIST_DIR}"

X86_BIN="${ROOT_DIR}/target/x86_64-unknown-linux-musl/release/zpane"
ARM_BIN="${ROOT_DIR}/target/aarch64-unknown-linux-musl/release/zpane"

PYTHON_BIN=""
if command -v python >/dev/null 2>&1 && python --version >/dev/null 2>&1; then
    PYTHON_BIN="python"
elif command -v python3 >/dev/null 2>&1 && python3 --version >/dev/null 2>&1; then
    PYTHON_BIN="python3"
fi

host_path() {
    if command -v cygpath >/dev/null 2>&1; then
        cygpath -w "$1"
    else
        printf '%s\n' "$1"
    fi
}

build_deb_fallback() {
    local stage_dir="$1"
    local out_deb="$2"
    local exe_mode="$3"
    "${PYTHON_BIN}" - "$(host_path "${stage_dir}")" "$(host_path "${out_deb}")" "${exe_mode}" <<'PY'
import io
import os
import sys
import tarfile
import time

stage_dir, out_deb, exe_mode_str = sys.argv[1:4]
exe_mode = int(exe_mode_str, 8)
now = int(time.time())


def add_bytes_to_tar(tf: tarfile.TarFile, arcname: str, data: bytes, mode: int, is_dir: bool = False) -> None:
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
with tarfile.open(fileobj=ctrl_buf, mode="w:gz", format=tarfile.USTAR_FORMAT) as tf:
    add_bytes_to_tar(tf, "./", b"", 0o755, is_dir=True)
    ctrl_path = os.path.join(stage_dir, "DEBIAN", "control")
    with open(ctrl_path, "rb") as f:
        add_bytes_to_tar(tf, "./control", f.read(), 0o644)

data_buf = io.BytesIO()
with tarfile.open(fileobj=data_buf, mode="w:gz", format=tarfile.USTAR_FORMAT) as tf:
    add_bytes_to_tar(tf, "./", b"", 0o755, is_dir=True)
    for root, dirs, files in os.walk(stage_dir):
        dirs.sort()
        files.sort()
        rel_root = os.path.relpath(root, stage_dir).replace(os.sep, "/")
        if rel_root == "DEBIAN" or rel_root.startswith("DEBIAN/"):
            continue
        dirs[:] = [d for d in dirs if not (rel_root == "." and d == "DEBIAN")]
        if rel_root != ".":
            add_bytes_to_tar(tf, "./" + rel_root + "/", b"", 0o755, is_dir=True)
        for fn in files:
            full = os.path.join(root, fn)
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
    "${PYTHON_BIN}" - "$(host_path "${stage_dir}")" "$(host_path "${out_path}")" "${exe_mode}" "${mode_flag}" "$@" <<'PY'
import io
import os
import sys
import tarfile
import time

stage_dir, out_path, exe_mode_str, tar_mode, *items = sys.argv[1:]
exe_mode = int(exe_mode_str, 8)
now = int(time.time())


def add_path(tf: tarfile.TarFile, base: str, rel: str) -> None:
    full = os.path.join(base, rel)
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
        with open(full, "rb") as f:
            data = f.read()
        ti.size = len(data)
        tf.addfile(ti, io.BytesIO(data))


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
    "${PYTHON_BIN}" - "${version}" "${rel}" "$(host_path "${x86_bin}")" "$(host_path "${readme}")" "$(host_path "${license_file}")" "$(host_path "${out_rpm}")" <<'PY'
import gzip
import hashlib
import os
import struct
import sys
import time

version, rel, x86_bin, readme, license_file, out_rpm = sys.argv[1:7]
mtime = int(time.time())

files_spec = [
    ("usr/bin/zpane", "/usr/bin/", "zpane", x86_bin, 0o100755, 0),
    ("usr/share/doc/zpane/README.md", "/usr/share/doc/zpane/", "README.md", readme, 0o100644, 2),
    ("usr/share/licenses/zpane/LICENSE", "/usr/share/licenses/zpane/", "LICENSE", license_file, 0o100644, 128),
]
blobs = []
for _, _, _, src, _, _ in files_spec:
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

# 1. Debian / Ubuntu package (zpane_<ver>-<rel>_amd64.deb)
DEB_STAGE="${WORK_DIR}/deb-amd64"
mkdir -p "${DEB_STAGE}/DEBIAN" \
         "${DEB_STAGE}/usr/bin" \
         "${DEB_STAGE}/usr/share/doc/zpane" \
         "${DEB_STAGE}/usr/share/licenses/zpane"

install -m 0755 "${X86_BIN}" "${DEB_STAGE}/usr/bin/zpane"
install -m 0644 "${ROOT_DIR}/README.md" "${DEB_STAGE}/usr/share/doc/zpane/README.md"
install -m 0644 "${ROOT_DIR}/LICENSE" "${DEB_STAGE}/usr/share/doc/zpane/copyright"

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

if command -v dpkg-deb >/dev/null 2>&1; then
    dpkg-deb --root-owner-group -Zgzip --build "${DEB_STAGE}" "${DIST_DIR}/zpane_${VERSION}-${REL}_amd64.deb"
else
    build_deb_fallback "${DEB_STAGE}" "${DIST_DIR}/zpane_${VERSION}-${REL}_amd64.deb" "0755"
fi

# 2. Termux Android aarch64 package (zpane_<ver>_aarch64.deb)
TERMUX_PREFIX="/data/data/com.termux/files/usr"
TERMUX_STAGE="${WORK_DIR}/termux-aarch64"
mkdir -p "${TERMUX_STAGE}/DEBIAN" \
         "${TERMUX_STAGE}${TERMUX_PREFIX}/bin" \
         "${TERMUX_STAGE}${TERMUX_PREFIX}/share/doc/zpane"

install -m 0700 "${ARM_BIN}" "${TERMUX_STAGE}${TERMUX_PREFIX}/bin/zpane"
install -m 0644 "${ROOT_DIR}/README.md" "${TERMUX_STAGE}${TERMUX_PREFIX}/share/doc/zpane/README.md"
install -m 0644 "${ROOT_DIR}/LICENSE" "${TERMUX_STAGE}${TERMUX_PREFIX}/share/doc/zpane/LICENSE"

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

if command -v dpkg-deb >/dev/null 2>&1; then
    dpkg-deb --root-owner-group -Zgzip --build "${TERMUX_STAGE}" "${DIST_DIR}/zpane_${VERSION}_aarch64.deb"
else
    build_deb_fallback "${TERMUX_STAGE}" "${DIST_DIR}/zpane_${VERSION}_aarch64.deb" "0700"
fi

# Standalone Termux / ARM64 tarball
if [[ -n "${PYTHON_BIN}" ]] && command -v cygpath >/dev/null 2>&1; then
    build_tar_py "${TERMUX_STAGE}${TERMUX_PREFIX}/bin" "${DIST_DIR}/zpane-v${VERSION}-aarch64-linux-musl.tar.gz" "0700" "w:gz" zpane
else
    tar -C "${TERMUX_STAGE}${TERMUX_PREFIX}/bin" -czf "${DIST_DIR}/zpane-v${VERSION}-aarch64-linux-musl.tar.gz" zpane
fi

# 3. Arch Linux pacman package (zpane-<ver>-<rel>-x86_64.pkg.tar.gz / .zst)
ARCH_STAGE="${WORK_DIR}/arch-x86_64"
mkdir -p "${ARCH_STAGE}/usr/bin" \
         "${ARCH_STAGE}/usr/share/doc/zpane" \
         "${ARCH_STAGE}/usr/share/licenses/zpane"

install -m 0755 "${X86_BIN}" "${ARCH_STAGE}/usr/bin/zpane"
install -m 0644 "${ROOT_DIR}/README.md" "${ARCH_STAGE}/usr/share/doc/zpane/README.md"
install -m 0644 "${ROOT_DIR}/LICENSE" "${ARCH_STAGE}/usr/share/licenses/zpane/LICENSE"

ARCH_BYTES="$(du -sb "${ARCH_STAGE}/usr" | cut -f1)"
BUILD_DATE="$(date +%s)"
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

if [[ -n "${PYTHON_BIN}" ]] && command -v cygpath >/dev/null 2>&1; then
    ARCH_TAR="${WORK_DIR}/arch-x86_64.tar"
    build_tar_py "${ARCH_STAGE}" "${ARCH_TAR}" "0755" "w" .PKGINFO usr
    if command -v zstd >/dev/null 2>&1; then
        zstd -f -q -19 "${ARCH_TAR}" -o "${DIST_DIR}/zpane-${VERSION}-${REL}-x86_64.pkg.tar.zst"
    elif command -v zstd.exe >/dev/null 2>&1; then
        zstd.exe -f -q -19 "$(host_path "${ARCH_TAR}")" -o "$(host_path "${DIST_DIR}/zpane-${VERSION}-${REL}-x86_64.pkg.tar.zst")"
    else
        gzip -9 -c "${ARCH_TAR}" > "${DIST_DIR}/zpane-${VERSION}-${REL}-x86_64.pkg.tar.gz"
    fi
elif command -v zstd >/dev/null 2>&1; then
    tar -C "${ARCH_STAGE}" --owner=0 --group=0 --zstd -cf "${DIST_DIR}/zpane-${VERSION}-${REL}-x86_64.pkg.tar.zst" .PKGINFO usr
elif command -v zstd.exe >/dev/null 2>&1; then
    tar -C "${ARCH_STAGE}" --owner=0 --group=0 -cf - .PKGINFO usr | zstd.exe -q -19 > "${DIST_DIR}/zpane-${VERSION}-${REL}-x86_64.pkg.tar.zst"
else
    tar -C "${ARCH_STAGE}" --owner=0 --group=0 -czf "${DIST_DIR}/zpane-${VERSION}-${REL}-x86_64.pkg.tar.gz" .PKGINFO usr
fi

# 4. Fedora / RHEL / openSUSE RPM package (zpane-<ver>-<rel>.x86_64.rpm)
if command -v rpmbuild >/dev/null 2>&1; then
    RPM_TOP="${WORK_DIR}/rpmbuild"
    mkdir -p "${RPM_TOP}"/{BUILD,RPMS,SOURCES,SPECS,SRPMS}
    install -m 0755 "${X86_BIN}" "${RPM_TOP}/SOURCES/zpane"
    install -m 0644 "${ROOT_DIR}/README.md" "${RPM_TOP}/SOURCES/README.md"
    install -m 0644 "${ROOT_DIR}/LICENSE" "${RPM_TOP}/SOURCES/LICENSE"
    sed -e "s/^Version:.*/Version:        ${VERSION}/" \
        -e "s/^Release:.*/Release:        ${REL}/" \
        "${ROOT_DIR}/packaging/fedora/zpane.spec" > "${RPM_TOP}/SPECS/zpane.spec"

    rpmbuild --define "_topdir ${RPM_TOP}" \
             --define "_build_id_links none" \
             --target x86_64-linux \
             -bb "${RPM_TOP}/SPECS/zpane.spec"

    cp "${RPM_TOP}/RPMS/x86_64/"*.rpm "${DIST_DIR}/"
elif [[ -n "${PYTHON_BIN}" ]]; then
    build_rpm_fallback "${VERSION}" "${REL}" "${X86_BIN}" "${ROOT_DIR}/README.md" "${ROOT_DIR}/LICENSE" "${DIST_DIR}/zpane-${VERSION}-${REL}.x86_64.rpm"
fi

echo "Built packages in ${DIST_DIR}:"
ls -lh "${DIST_DIR}"
