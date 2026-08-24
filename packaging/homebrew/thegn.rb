# Homebrew formula for thegn.
#
# Installs the prebuilt macOS binary from a GitHub Release. On each release, bump
# `version` and the `sha256` (from the release's `thegn-<tag>-aarch64-apple-darwin.sha256`
# asset — note: no `.tar.gz` infix). Once a tap exists, users install with
# `brew install <owner>/tap/thegn`.
#
# WHY THIS PATH RATHER THAN A DOWNLOAD: Homebrew does not attach
# `com.apple.quarantine` to formula downloads, so an unsigned binary installed
# this way opens without Gatekeeper prompts. The same tarball fetched through a
# browser DOES get quarantined. See RELEASING.md.
#
# Apple silicon only, matching the release matrix: the pinned nixpkgs dropped
# x86_64-darwin and no Intel Mac is available to prove that target.
class Thegn < Formula
  desc "Terminal-native git-worktree IDE that is its own terminal multiplexer"
  homepage "https://github.com/blakeashleyjr/thegn"
  version "0.1.0-alpha.2"
  # Matches the workspace's `MIT OR Apache-2.0` (Cargo.toml, LICENSE-MIT,
  # LICENSE-APACHE) — not plain MIT, which is what this said before.
  license any_of: ["MIT", "Apache-2.0"]

  depends_on :macos
  depends_on arch: :arm64

  on_macos do
    on_arm do
      url "https://github.com/blakeashleyjr/thegn/releases/download/v#{version}/thegn-v#{version}-aarch64-apple-darwin.tar.gz"
      sha256 "REPLACE_WITH_AARCH64_SHA256"
    end
  end

  # Optional at runtime; thegn degrades without them, but the PR panel, diff
  # highlighting and the file drawer are much better with them present.
  depends_on "gh" => :optional
  depends_on "git-delta" => :optional
  depends_on "lazygit" => :optional

  def install
    bin.install "thegn"
    # The short `tg` alias the Nix package + install.sh also provide.
    bin.install_symlink "thegn" => "tg"
  end

  def caveats
    <<~EOS
      thegn is a TUI, so its launcher entry is a `thegn.app` bundle that opens a
      terminal running it. Generate one (it is not shipped — a locally generated
      bundle carries no com.apple.quarantine, so it needs no code signing):

        git clone https://github.com/blakeashleyjr/thegn
        cd thegn && just macos-app "#{bin}/thegn"

      macOS composes characters with the Option key by default, so thegn's
      Alt-based chords (Alt-w, Alt-o, Alt-s, Alt-.) will not reach it until your
      terminal is told to send Alt:

        Ghostty:    macos-option-as-alt = true
        Alacritty:  [window] option_as_alt = "Both"
        kitty:      macos_option_as_alt yes
    EOS
  end

  test do
    assert_match "thegn", shell_output("#{bin}/thegn --version")
  end
end
