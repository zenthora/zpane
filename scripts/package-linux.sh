#!/usr/bin/env bash
set -euo pipefail

VERSION="${1:-0.1.0}"
REL="1"
ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
DIST_DIR="${ROOT_DIR}/dist"
WORK_DIR="$(mktemp -d)"
trap 'rm -rf "${WORK_DIR}"' EXIT

mkdir -p "${DIST_DIR}"

X86_BIN="${ROOT_DIR}/target/x86_64-unknown-linux-musl/release/zpane"
ARM_BIN="${ROOT_DIR}/target/aarch64-unknown-linux-musl/release/zpane"

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

dpkg-deb --root-owner-group -Zgzip --build "${DEB_STAGE}" "${DIST_DIR}/zpane_${VERSION}-${REL}_amd64.deb"

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

dpkg-deb --root-owner-group -Zgzip --build "${TERMUX_STAGE}" "${DIST_DIR}/zpane_${VERSION}_aarch64.deb"

# Standalone Termux / ARM64 tarball
tar -C "${TERMUX_STAGE}${TERMUX_PREFIX}/bin" -czf "${DIST_DIR}/zpane-v${VERSION}-aarch64-linux-musl.tar.gz" zpane

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

if command -v zstd >/dev/null 2>&1; then
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
fi

echo "Built packages in ${DIST_DIR}:"
ls -lh "${DIST_DIR}"
