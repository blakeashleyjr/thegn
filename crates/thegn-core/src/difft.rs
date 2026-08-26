//! difftastic (`difft`) — the structural differ thegn renders read-only diff
//! surfaces through when `[git] structural_diff` selects it.
//!
//! Like the BugStalker debugger, `difft` is a [`crate::managed_tool::ManagedTool`]:
//! resolved **override → PATH (`difft`) → managed GitHub-release download**, so a
//! user who already has difftastic installed pays nothing, and everyone else can
//! have thegn acquire a pinned build. This module carries only the *pure* spec
//! (which asset per platform, the pinned tag); the download + the ANSI→cells
//! rendering live in the host / [`crate::ansi_cells`].
//!
//! GitHub-release (not `cargo install`) is deliberate: it dodges the managed-tool
//! `Source::Cargo` spec drift and gives every platform a prebuilt binary.

use crate::managed_tool::{Arch, AssetRule, ManagedTool, Os, UpdatePolicy};

/// Pinned difftastic release tag (Wilfred/difftastic uses bare `x.y.z` tags, no
/// `v` prefix). Bump to adopt a newer difftastic; the version marker under the
/// managed dir triggers a re-download when this changes.
pub const DIFFT_PIN: &str = "0.63.0";

/// GitHub `owner/repo` difftastic releases come from.
pub const DIFFT_REPO: &str = "Wilfred/difftastic";

/// The difftastic managed-tool spec: GitHub-release download of the platform
/// tarball/zip, `difft` on PATH as the tier-2 fallback, pinned at [`DIFFT_PIN`].
pub fn difft_tool() -> ManagedTool {
    ManagedTool::github(
        "difft",
        DIFFT_REPO,
        DIFFT_PIN,
        vec![
            asset(Os::Linux, Arch::X64, "x86_64-unknown-linux-gnu.tar.gz"),
            asset(Os::Linux, Arch::Arm64, "aarch64-unknown-linux-gnu.tar.gz"),
            asset(Os::Macos, Arch::X64, "x86_64-apple-darwin.tar.gz"),
            asset(Os::Macos, Arch::Arm64, "aarch64-apple-darwin.tar.gz"),
            asset(Os::Windows, Arch::X64, "x86_64-pc-windows-msvc.zip"),
        ],
    )
    .with_policy(UpdatePolicy::Once)
    .with_path_fallbacks(&["difft"])
}

/// A difftastic release asset for `(os, arch)`. Names follow
/// `difft-<triple>.<ext>` at every release.
fn asset(os: Os, arch: Arch, triple_ext: &str) -> AssetRule {
    AssetRule {
        os,
        arch,
        asset: format!("difft-{triple_ext}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn difft_is_github_sourced_and_pinned() {
        let t = difft_tool();
        assert_eq!(t.name, "difft");
        assert_eq!(t.version, DIFFT_PIN);
        assert_eq!(t.repo(), Some(DIFFT_REPO));
        // PATH fallback so an existing difftastic wins before any download.
        assert_eq!(t.path_fallbacks, vec!["difft".to_string()]);
    }

    #[test]
    fn asset_selection_per_platform() {
        let t = difft_tool();
        assert_eq!(
            t.asset_for(Os::Linux, Arch::X64),
            Some("difft-x86_64-unknown-linux-gnu.tar.gz")
        );
        assert_eq!(
            t.asset_for(Os::Macos, Arch::Arm64),
            Some("difft-aarch64-apple-darwin.tar.gz")
        );
        assert_eq!(
            t.asset_for(Os::Windows, Arch::X64),
            Some("difft-x86_64-pc-windows-msvc.zip")
        );
        // No Windows-arm64 build is published.
        assert_eq!(t.asset_for(Os::Windows, Arch::Arm64), None);
    }

    #[test]
    fn resolves_path_before_managed() {
        let t = difft_tool();
        let r = t.resolve(None, |name| {
            (name == "difft").then(|| "/usr/bin/difft".to_string())
        });
        assert_eq!(r.tier(), "path");
        assert_eq!(r.path(), "/usr/bin/difft");
    }
}
