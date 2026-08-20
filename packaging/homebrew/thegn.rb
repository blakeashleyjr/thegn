# Homebrew formula for thegn (template).
#
# This installs the prebuilt macOS binary from a GitHub Release. On each release,
# bump `version`, the two `url`s, and their `sha256`s (from the release's
# `thegn-<tag>-<target>.sha256` files — note: no `.tar.gz` infix). Once a tap
# exists, users install with `brew install <owner>/tap/thegn`.
#
# Until the first tagged release exists these URLs 404 — see RELEASING.md.
class Thegn < Formula
  desc "Terminal-native git-worktree IDE that is its own terminal multiplexer"
  homepage "https://github.com/blakeashleyjr/thegn"
  version "0.1.0-alpha.2"
  # Matches the workspace's `MIT OR Apache-2.0` (Cargo.toml, LICENSE-MIT,
  # LICENSE-APACHE) — not plain MIT, which is what this said before.
  license any_of: ["MIT", "Apache-2.0"]

  on_macos do
    on_arm do
      url "https://github.com/blakeashleyjr/thegn/releases/download/v#{version}/thegn-v#{version}-aarch64-apple-darwin.tar.gz"
      sha256 "REPLACE_WITH_AARCH64_SHA256"
    end
    on_intel do
      url "https://github.com/blakeashleyjr/thegn/releases/download/v#{version}/thegn-v#{version}-x86_64-apple-darwin.tar.gz"
      sha256 "REPLACE_WITH_X86_64_SHA256"
    end
  end

  def install
    bin.install "thegn"
    # The short `tg` alias the Nix package + install.sh also provide.
    bin.install_symlink "thegn" => "tg"
  end

  test do
    assert_match "thegn", shell_output("#{bin}/thegn --version")
  end
end
