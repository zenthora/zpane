TERMUX_PKG_HOMEPAGE=https://github.com/zenthora/zpane
TERMUX_PKG_DESCRIPTION="3-pane Miller column command, subcommand, parameter & variable explorer"
TERMUX_PKG_LICENSE="MIT"
TERMUX_PKG_MAINTAINER="@zenthora"
TERMUX_PKG_VERSION="0.3.0"
TERMUX_PKG_SRCURL=https://github.com/zenthora/zpane/archive/refs/tags/v${TERMUX_PKG_VERSION}.tar.gz
TERMUX_PKG_SHA256=SKIP_CHECKSUM
TERMUX_PKG_BUILD_IN_SRC=true
TERMUX_PKG_AUTO_UPDATE=true

termux_step_make() {
    termux_setup_rust
    cargo build --jobs "${TERMUX_MAKE_PROCESSES}" --target "${CARGO_TARGET_NAME}" --release
}

termux_step_make_install() {
    install -Dm700 -t "${TERMUX_PREFIX}/bin" "target/${CARGO_TARGET_NAME}/release/zpane"
    install -Dm644 LICENSE "${TERMUX_PREFIX}/share/doc/zpane/LICENSE"
    install -Dm644 README.md "${TERMUX_PREFIX}/share/doc/zpane/README.md"
}
