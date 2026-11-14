TERMUX_PKG_HOMEPAGE=https://github.com/zenthora/zpane
TERMUX_PKG_DESCRIPTION="3-pane Miller column command, subcommand, parameter & variable explorer"
TERMUX_PKG_LICENSE="MIT"
TERMUX_PKG_MAINTAINER="@zenthora"
TERMUX_PKG_VERSION="0.4.1"
TERMUX_PKG_SRCURL="https://github.com/zenthora/zpane/archive/refs/tags/v${TERMUX_PKG_VERSION}.tar.gz"
# Security: replace SKIP_CHECKSUM with the release tarball SHA-256 digest when submitting to termux-packages.
TERMUX_PKG_SHA256=SKIP_CHECKSUM
TERMUX_PKG_BUILD_IN_SRC=true
TERMUX_PKG_AUTO_UPDATE=true

termux_step_make() {
    termux_setup_rust
    cargo build \
        --locked \
        --jobs "${TERMUX_PKG_MAKE_PROCESSES:-${TERMUX_MAKE_PROCESSES:-1}}" \
        --target "${CARGO_TARGET_NAME}" \
        --release
}

termux_step_make_install() {
    umask 022
    install -d -m0700 "${TERMUX_PREFIX}/bin"
    install -d -m0755 "${TERMUX_PREFIX}/share/doc/zpane"
    install -Dm0700 -t "${TERMUX_PREFIX}/bin" "target/${CARGO_TARGET_NAME}/release/zpane"
    install -Dm0644 LICENSE "${TERMUX_PREFIX}/share/doc/zpane/LICENSE"
    install -Dm0644 README.md "${TERMUX_PREFIX}/share/doc/zpane/README.md"
}
