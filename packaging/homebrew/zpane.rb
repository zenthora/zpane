# Homebrew formula template for the zenthora/homebrew-tap repository.
# Placeholders (__VERSION__, __SHA256_<TARGET>__) are rendered from the release SHA256SUMS by
# scripts/render-package-manifests.py; do not edit them by hand.
class Zpane < Formula
  desc "3-pane Miller column command, subcommand, parameter & variable explorer"
  homepage "https://github.com/zenthora/zpane"
  version "__VERSION__"
  license "MIT"

  on_macos do
    on_arm do
      url "https://github.com/zenthora/zpane/releases/download/v__VERSION__/zpane-v__VERSION__-aarch64-apple-darwin.tar.gz"
      sha256 "__SHA256_AARCH64_APPLE_DARWIN__"
    end
    on_intel do
      url "https://github.com/zenthora/zpane/releases/download/v__VERSION__/zpane-v__VERSION__-x86_64-apple-darwin.tar.gz"
      sha256 "__SHA256_X86_64_APPLE_DARWIN__"
    end
  end

  on_linux do
    on_arm do
      url "https://github.com/zenthora/zpane/releases/download/v__VERSION__/zpane-v__VERSION__-aarch64-unknown-linux-musl.tar.gz"
      sha256 "__SHA256_AARCH64_UNKNOWN_LINUX_MUSL__"
    end
    on_intel do
      url "https://github.com/zenthora/zpane/releases/download/v__VERSION__/zpane-v__VERSION__-x86_64-unknown-linux-musl.tar.gz"
      sha256 "__SHA256_X86_64_UNKNOWN_LINUX_MUSL__"
    end
  end

  def install
    bin.install "zpane"
  end

  test do
    assert_match version.to_s, shell_output("#{bin}/zpane --version")
  end
end
