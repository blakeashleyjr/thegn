//! Managed external tools — a reusable "acquire and pin a binary" resolver.
//!
//! thegn owns a handful of external tools (today the `pi` coding agent under
//! `~/.thegn/pi`; soon a debug adapter and user-declared MCP servers). Rather
//! than hand-roll the install per tool, this module captures the pattern Zed's
//! extensions use: resolve a tool by **user override → project PATH →
//! download-and-pin**, per platform, with an update policy and graceful
//! fallback. Each tool is one [`ManagedTool`] value.
//!
//! This module is **pure**: it decides *which* tier satisfies a tool, *which*
//! release asset matches the platform, *where* the managed copy lives, and
//! *whether* a (re)install is needed — but it performs no downloads. The
//! side-effecting fetch lives in `thegn-host` (core carries no HTTP client),
//! driven by these decisions. That split keeps this logic unit-testable and
//! under the core coverage gate while the fetch is exercised by `test/smoke.sh`.

use crate::util;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// Operating systems we select release assets for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Os {
    Linux,
    Macos,
    Windows,
}

/// CPU architectures we select release assets for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Arch {
    X64,
    Arm64,
}

impl Os {
    /// The host OS at compile time. The one impure spot in this module; kept a
    /// thin `cfg!` mapper so every decision path takes an explicit [`Os`].
    pub fn current() -> Option<Os> {
        if cfg!(target_os = "linux") {
            Some(Os::Linux)
        } else if cfg!(target_os = "macos") {
            Some(Os::Macos)
        } else if cfg!(target_os = "windows") {
            Some(Os::Windows)
        } else {
            None
        }
    }
}

impl Arch {
    /// The host architecture at compile time (see [`Os::current`]).
    pub fn current() -> Option<Arch> {
        if cfg!(target_arch = "x86_64") {
            Some(Arch::X64)
        } else if cfg!(target_arch = "aarch64") {
            Some(Arch::Arm64)
        } else {
            None
        }
    }
}

/// One GitHub-release asset selector: the asset filename to download on a given
/// platform/architecture.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AssetRule {
    pub os: Os,
    pub arch: Arch,
    pub asset: String,
}

/// Where a tool's binary comes from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Source {
    /// A GitHub release: `owner/repo` plus a per-`(os, arch)` asset selector.
    GithubRelease {
        repo: String,
        assets: Vec<AssetRule>,
    },
    /// An npm package installed with `npm install --prefix <dir> <package>@<version>`.
    Npm { package: String },
    /// A crates.io crate installed with `cargo install <crate> --version
    /// <version> --root <managed_dir>` (binary at `<managed_dir>/bin/<name>`).
    Cargo { crate_name: String },
}

/// How aggressively to re-verify an already-installed tool.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UpdatePolicy {
    /// Re-install/refresh on every ensure (ignores the version marker).
    Always,
    /// Install once at the pinned version; skip while the marker matches.
    Once,
    /// Never auto-manage; only (re)install when the binary is missing.
    Never,
}

/// The on-disk layout of a managed tool's install: the directory holding it, the
/// binary path relative to that dir, and the version-marker filename. Defaults
/// place new tools under `~/.thegn/tools/<name>`; the managed pi keeps its
/// legacy layout for byte-for-byte compatibility with existing installs.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Layout {
    /// Absolute managed dir, or `None` to derive `~/.thegn/tools/<name>`.
    dir: Option<PathBuf>,
    bin_rel: PathBuf,
    marker_name: String,
}

/// A pure, declarative spec for one externally-acquired tool.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ManagedTool {
    /// Stable identifier (also the default managed-dir name).
    pub name: String,
    pub source: Source,
    /// Pinned version string (npm version, or release tag / marker value).
    pub version: String,
    pub policy: UpdatePolicy,
    /// Command names to look up on the project PATH before falling back to the
    /// managed copy (e.g. `["pi"]`).
    pub path_fallbacks: Vec<String>,
    layout: Layout,
}

/// A user-configured override for a managed tool (`[managed_tools.<name>]`): an
/// explicit binary path (highest-priority tier) plus optional extra arguments.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct ToolOverride {
    /// Absolute path to the binary. Empty ⇒ no override (fall through to PATH).
    #[serde(default)]
    pub path: String,
    /// Extra arguments to pass when launching the overridden binary.
    #[serde(default)]
    pub args: Vec<String>,
}

/// The outcome of resolving a tool: which tier satisfied it and where its binary
/// is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Resolution {
    /// Tier 1: a user-configured path won.
    Override { path: String, args: Vec<String> },
    /// Tier 2: found on the project PATH.
    OnPath { path: String },
    /// Tier 3: the managed download-and-pin location (`current` = marker matches
    /// the pinned version and the binary exists).
    Managed { path: String, current: bool },
}

impl Resolution {
    /// The resolved binary path, whichever tier won.
    pub fn path(&self) -> &str {
        match self {
            Resolution::Override { path, .. }
            | Resolution::OnPath { path }
            | Resolution::Managed { path, .. } => path,
        }
    }

    /// A short tier label for `doctor` / diagnostics.
    pub fn tier(&self) -> &'static str {
        match self {
            Resolution::Override { .. } => "override",
            Resolution::OnPath { .. } => "path",
            Resolution::Managed { .. } => "managed",
        }
    }
}

impl ManagedTool {
    /// A tool installed from npm. `bin_name` is the executable npm drops under
    /// `node_modules/.bin/`, which may differ from the package name.
    pub fn npm(name: &str, package: &str, bin_name: &str, version: &str) -> ManagedTool {
        ManagedTool {
            name: name.to_string(),
            source: Source::Npm {
                package: package.to_string(),
            },
            version: version.to_string(),
            policy: UpdatePolicy::Once,
            path_fallbacks: Vec::new(),
            layout: Layout {
                dir: None,
                bin_rel: PathBuf::from("node_modules/.bin").join(bin_name),
                marker_name: ".version".to_string(),
            },
        }
    }

    /// A tool installed from crates.io via `cargo install`. `bin_name` is the
    /// executable cargo drops under `<managed_dir>/bin/` (often ≠ crate name).
    pub fn cargo(name: &str, crate_name: &str, bin_name: &str, version: &str) -> ManagedTool {
        ManagedTool {
            name: name.to_string(),
            source: Source::Cargo {
                crate_name: crate_name.to_string(),
            },
            version: version.to_string(),
            policy: UpdatePolicy::Once,
            path_fallbacks: Vec::new(),
            layout: Layout {
                dir: None,
                bin_rel: PathBuf::from("bin").join(bin_name),
                marker_name: ".version".to_string(),
            },
        }
    }

    /// A tool downloaded from a GitHub release. The binary lands at
    /// `<managed_dir>/bin/<name>`.
    pub fn github(name: &str, repo: &str, version: &str, assets: Vec<AssetRule>) -> ManagedTool {
        ManagedTool {
            name: name.to_string(),
            source: Source::GithubRelease {
                repo: repo.to_string(),
                assets,
            },
            version: version.to_string(),
            policy: UpdatePolicy::Once,
            path_fallbacks: Vec::new(),
            layout: Layout {
                dir: None,
                bin_rel: PathBuf::from("bin").join(name),
                marker_name: ".version".to_string(),
            },
        }
    }

    /// Builder: PATH fallback command names (tier 2).
    pub fn with_path_fallbacks(mut self, names: &[&str]) -> ManagedTool {
        self.path_fallbacks = names.iter().map(|s| s.to_string()).collect();
        self
    }

    /// Builder: update policy (default [`UpdatePolicy::Once`]).
    pub fn with_policy(mut self, policy: UpdatePolicy) -> ManagedTool {
        self.policy = policy;
        self
    }

    /// Builder: pin an absolute managed dir, the binary path relative to it, and
    /// the version-marker filename — for tools with a legacy layout (the managed
    /// pi). New tools should use the defaults.
    pub fn with_layout(mut self, dir: PathBuf, bin_rel: &str, marker_name: &str) -> ManagedTool {
        self.layout = Layout {
            dir: Some(dir),
            bin_rel: PathBuf::from(bin_rel),
            marker_name: marker_name.to_string(),
        };
        self
    }

    /// The GitHub-release asset filename for `(os, arch)`, or `None` when the
    /// platform is unsupported (or the source is npm).
    pub fn asset_for(&self, os: Os, arch: Arch) -> Option<&str> {
        match &self.source {
            Source::GithubRelease { assets, .. } => assets
                .iter()
                .find(|r| r.os == os && r.arch == arch)
                .map(|r| r.asset.as_str()),
            Source::Npm { .. } | Source::Cargo { .. } => None,
        }
    }

    /// The `owner/repo` for a GitHub-release source (`None` otherwise).
    pub fn repo(&self) -> Option<&str> {
        match &self.source {
            Source::GithubRelease { repo, .. } => Some(repo.as_str()),
            Source::Npm { .. } | Source::Cargo { .. } => None,
        }
    }

    /// The directory holding this tool's managed install
    /// (`~/.thegn/tools/<name>` by default, or the pinned legacy dir).
    pub fn managed_dir(&self) -> PathBuf {
        self.layout
            .dir
            .clone()
            .unwrap_or_else(|| util::thegn_dir().join("tools").join(&self.name))
    }

    /// The managed binary path.
    pub fn bin_path(&self) -> PathBuf {
        self.managed_dir().join(&self.layout.bin_rel)
    }

    /// The version-marker file path.
    pub fn version_marker(&self) -> PathBuf {
        self.managed_dir().join(&self.layout.marker_name)
    }

    /// `true` when the managed binary is present and its marker equals the pin.
    pub fn is_current(&self) -> bool {
        self.is_current_at(&self.managed_dir())
    }

    /// [`is_current`](Self::is_current) against an explicit managed dir — the
    /// testable core (no env, no real `~/.thegn`).
    pub fn is_current_at(&self, dir: &Path) -> bool {
        let bin = dir.join(&self.layout.bin_rel);
        let marker = dir.join(&self.layout.marker_name);
        bin.exists()
            && std::fs::read_to_string(marker)
                .map(|s| s.trim() == self.version)
                .unwrap_or(false)
    }

    /// Whether an install/refresh is required, from the update policy, whether
    /// the managed copy is current, whether its binary exists, and a `force`
    /// flag.
    pub fn needs_install(&self, force: bool) -> bool {
        should_install(
            self.policy,
            self.is_current(),
            self.bin_path().exists(),
            force,
        )
    }

    /// Pure three-tier resolution: user override → project PATH → managed.
    /// `which` is injected (production passes [`util::which_path`]) so the
    /// decision is testable without a real PATH.
    pub fn resolve(
        &self,
        over: Option<&ToolOverride>,
        which: impl Fn(&str) -> Option<String>,
    ) -> Resolution {
        if let Some(o) = over
            && !o.path.trim().is_empty()
        {
            return Resolution::Override {
                path: o.path.clone(),
                args: o.args.clone(),
            };
        }
        for name in &self.path_fallbacks {
            if let Some(p) = which(name) {
                return Resolution::OnPath { path: p };
            }
        }
        Resolution::Managed {
            path: self.bin_path().to_string_lossy().into_owned(),
            current: self.is_current(),
        }
    }
}

/// Pure install decision, factored out of [`ManagedTool::needs_install`] so the
/// policy matrix is exhaustively unit-testable without touching the filesystem.
fn should_install(policy: UpdatePolicy, current: bool, bin_present: bool, force: bool) -> bool {
    if force {
        return true;
    }
    match policy {
        UpdatePolicy::Always => true,
        UpdatePolicy::Once => !current,
        UpdatePolicy::Never => !bin_present,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tool() -> ManagedTool {
        ManagedTool::npm("pi", "@earendil-works/pi-coding-agent", "pi", "0.80.2")
            .with_path_fallbacks(&["pi"])
    }

    /// A unique, self-cleaning temp dir (no tempdir dev-dep; avoids env mutation).
    struct Tmp(PathBuf);
    impl Tmp {
        fn new(tag: &str) -> Tmp {
            let dir = std::env::temp_dir().join(format!(
                "sz-mtool-{}-{}-{tag}",
                std::process::id(),
                util::now()
            ));
            std::fs::create_dir_all(&dir).unwrap();
            Tmp(dir)
        }
    }
    impl Drop for Tmp {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn resolve_prefers_override() {
        let t = tool();
        let over = ToolOverride {
            path: "/opt/pi".into(),
            args: vec!["--x".into()],
        };
        // Override wins even when PATH would also match.
        let r = t.resolve(Some(&over), |_| Some("/usr/bin/pi".into()));
        assert_eq!(
            r,
            Resolution::Override {
                path: "/opt/pi".into(),
                args: vec!["--x".into()]
            }
        );
        assert_eq!(r.tier(), "override");
    }

    #[test]
    fn resolve_uses_path_before_managed() {
        let t = tool();
        // Empty override path is ignored; PATH fallback wins over managed.
        let over = ToolOverride::default();
        let r = t.resolve(Some(&over), |name| {
            (name == "pi").then(|| "/usr/local/bin/pi".to_string())
        });
        assert_eq!(
            r,
            Resolution::OnPath {
                path: "/usr/local/bin/pi".into()
            }
        );
    }

    #[test]
    fn resolve_falls_back_to_managed() {
        let t = tool();
        let r = t.resolve(None, |_| None);
        match r {
            Resolution::Managed { path, .. } => {
                assert!(path.ends_with("node_modules/.bin/pi"), "{path}");
            }
            other => panic!("expected managed, got {other:?}"),
        }
    }

    #[test]
    fn current_platform_is_detected() {
        // The build targets we support resolve to a concrete (os, arch); the
        // pair then round-trips through asset selection.
        let (os, arch) = (Os::current(), Arch::current());
        assert!(os.is_some(), "unsupported target_os");
        assert!(arch.is_some(), "unsupported target_arch");
        let t = ManagedTool::github(
            "x",
            "o/r",
            "v1",
            vec![AssetRule {
                os: os.unwrap(),
                arch: arch.unwrap(),
                asset: "here".into(),
            }],
        );
        assert_eq!(t.asset_for(os.unwrap(), arch.unwrap()), Some("here"));
    }

    #[test]
    fn asset_selection_per_platform() {
        let t = ManagedTool::github(
            "bs",
            "godzie44/BugStalker",
            "v0.4.6",
            vec![
                AssetRule {
                    os: Os::Linux,
                    arch: Arch::X64,
                    asset: "bs-linux-x86_64".into(),
                },
                AssetRule {
                    os: Os::Linux,
                    arch: Arch::Arm64,
                    asset: "bs-linux-aarch64".into(),
                },
            ],
        );
        assert_eq!(t.asset_for(Os::Linux, Arch::X64), Some("bs-linux-x86_64"));
        assert_eq!(
            t.asset_for(Os::Linux, Arch::Arm64),
            Some("bs-linux-aarch64")
        );
        // Unsupported platform ⇒ None.
        assert_eq!(t.asset_for(Os::Macos, Arch::Arm64), None);
        assert_eq!(t.repo(), Some("godzie44/BugStalker"));
        // npm sources never have release assets.
        assert_eq!(tool().asset_for(Os::Linux, Arch::X64), None);
        assert_eq!(tool().repo(), None);
    }

    #[test]
    fn cargo_source_resolves_to_managed_bin() {
        let t = ManagedTool::cargo("bugstalker", "bugstalker", "bs", "0.4.6")
            .with_path_fallbacks(&["bs"]);
        // No release asset for any platform; not a github repo.
        assert_eq!(t.asset_for(Os::Linux, Arch::X64), None);
        assert_eq!(t.repo(), None);
        // Default layout: managed bin under tools/<name>/bin/<bin_name>.
        assert!(t.managed_dir().ends_with("tools/bugstalker"));
        assert!(t.bin_path().ends_with("bin/bs"));
        // Falls back to managed when nothing overrides / on PATH.
        match t.resolve(None, |_| None) {
            Resolution::Managed { path, .. } => assert!(path.ends_with("bin/bs"), "{path}"),
            other => panic!("expected managed, got {other:?}"),
        }
    }

    #[test]
    fn should_install_matrix() {
        // force always installs, regardless of policy/state.
        assert!(should_install(UpdatePolicy::Never, true, true, true));
        // Once: install only when not current.
        assert!(should_install(UpdatePolicy::Once, false, false, false));
        assert!(!should_install(UpdatePolicy::Once, true, true, false));
        // Always: install every time.
        assert!(should_install(UpdatePolicy::Always, true, true, false));
        // Never: only when the binary is missing.
        assert!(should_install(UpdatePolicy::Never, false, false, false));
        assert!(!should_install(UpdatePolicy::Never, false, true, false));
    }

    #[test]
    fn is_current_reads_marker_and_binary() {
        let tmp = Tmp::new("cur");
        let dir = &tmp.0;
        let t = tool();
        // Neither present ⇒ not current.
        assert!(!t.is_current_at(dir));
        // Binary present but no marker ⇒ not current.
        let bin = dir.join("node_modules/.bin/pi");
        std::fs::create_dir_all(bin.parent().unwrap()).unwrap();
        std::fs::write(&bin, "#!/bin/sh\n").unwrap();
        assert!(!t.is_current_at(dir));
        // Marker with the wrong version ⇒ not current.
        let marker = dir.join(".version");
        std::fs::write(&marker, "0.79.0\n").unwrap();
        assert!(!t.is_current_at(dir));
        // Marker matching the pin (trimmed) ⇒ current.
        std::fs::write(&marker, "0.80.2\n").unwrap();
        assert!(t.is_current_at(dir));
    }

    #[test]
    fn path_computation_and_layout_override() {
        // Default npm layout lands under tools/<name>.
        let t = tool();
        assert!(
            t.managed_dir().ends_with("tools/pi"),
            "{:?}",
            t.managed_dir()
        );
        assert!(t.bin_path().ends_with("node_modules/.bin/pi"));
        assert!(t.version_marker().ends_with(".version"));

        // A legacy layout override (as the real managed pi uses).
        let legacy = PathBuf::from("/home/u/.thegn/pi");
        let pi = tool().with_layout(legacy.clone(), "node_modules/.bin/pi", ".thegn-pi-version");
        assert_eq!(pi.managed_dir(), legacy);
        assert_eq!(pi.bin_path(), legacy.join("node_modules/.bin/pi"));
        assert_eq!(pi.version_marker(), legacy.join(".thegn-pi-version"));
    }

    #[test]
    fn tool_override_deserializes_from_toml() {
        let o: ToolOverride = toml::from_str(
            r#"path = "/opt/pi"
args = ["--flag"]
"#,
        )
        .unwrap();
        assert_eq!(o.path, "/opt/pi");
        assert_eq!(o.args, vec!["--flag".to_string()]);
        // Absent fields default (empty ⇒ no override).
        let empty: ToolOverride = toml::from_str("").unwrap();
        assert!(empty.path.is_empty() && empty.args.is_empty());
    }
}
