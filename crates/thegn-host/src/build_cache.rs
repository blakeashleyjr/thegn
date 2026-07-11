//! Build- and hook-tooling caches for interactive panes.
//!
//! Two concerns, both driven by `[disk]`:
//!  * the env a pane needs for a shared `sccache` compile cache / `CARGO_TARGET_DIR`
//!    ([`build_env_vars`]), and
//!  * making those caches — plus the pre-commit hook FRAMEWORK caches
//!    (`prek`/`pre-commit`) — writable *inside* a sandbox that binds `$HOME`
//!    read-only ([`inject_cache_mounts`]). Without the latter, `git commit` hooks
//!    die with "Read-only file system" and agents fall back to `--no-verify`.
//!
//! Extracted from `agent.rs` (pinned at its god-file ratchet ceiling).

use std::path::Path;
use thegn_core::config::Config;
use thegn_core::sandbox::{Mount, SandboxSpec};

/// Resolve a configured build path: `~`/`~/…` expands to home; a relative path
/// resolves against the repo root (so a shared `target/` is per-repo).
pub(crate) fn resolve_build_path(raw: &str, repo_root: &Path) -> String {
    let expanded = thegn_core::util::expand_tilde(raw);
    let p = Path::new(&expanded);
    if p.is_absolute() {
        expanded
    } else {
        repo_root.join(p).to_string_lossy().into_owned()
    }
}

/// Where the shared `sccache` compile cache lives when `[disk] sccache` is on:
/// the configured `sccache_dir` (tilde/relative-resolved), or sccache's own
/// default `~/.cache/sccache`. `None` when sccache is disabled. Config-gated only
/// (the PATH check for the actual `RUSTC_WRAPPER` lives in [`build_env_vars`]), so
/// it's a pure function of config + `$HOME` — the single source of truth shared by
/// the pane env and the sandbox cache mount so they can never disagree.
pub(crate) fn resolved_sccache_dir(cfg: &Config, repo_root: &Path) -> Option<String> {
    if !cfg.disk.sccache {
        return None;
    }
    if cfg.disk.sccache_dir.is_empty() {
        let home = std::env::var("HOME").ok()?;
        Some(format!("{home}/.cache/sccache"))
    } else {
        Some(resolve_build_path(&cfg.disk.sccache_dir, repo_root))
    }
}

/// Build-tooling env injected into interactive panes from `[disk]`: a shared
/// `sccache` compile cache and/or a shared `CARGO_TARGET_DIR`. Empty when both
/// are off (the common case), so panes are untouched unless opted in.
pub(crate) fn build_env_vars(cfg: &Config, repo_root: &Path) -> Vec<(String, String)> {
    let mut out = Vec::new();
    if cfg.disk.sccache
        && thegn_core::util::have("sccache")
        && let Some(dir) = resolved_sccache_dir(cfg, repo_root)
    {
        out.push(("RUSTC_WRAPPER".to_string(), "sccache".to_string()));
        // Pin SCCACHE_DIR — even the default ~/.cache/sccache — so the pane env
        // and the sandbox's read-write cache mount can't disagree via an
        // in-container XDG_CACHE_HOME, which would put sccache back under the
        // read-only $HOME ("Read-only file system").
        out.push(("SCCACHE_DIR".to_string(), dir));
    }
    if !cfg.disk.shared_target_dir.is_empty() {
        out.push((
            "CARGO_TARGET_DIR".to_string(),
            resolve_build_path(&cfg.disk.shared_target_dir, repo_root),
        ));
    }
    out
}

/// Read-write cache directories the in-sandbox pre-commit toolchain needs under
/// a read-only `$HOME`: the hook FRAMEWORK caches (`prek`, and legacy
/// `pre-commit`) — without which `git commit` hooks can't write their hook
/// environments — plus the `sccache` compile cache and any out-of-worktree
/// `CARGO_TARGET_DIR` that clippy/tests write. Each is a path-preserving,
/// read-write [`Mount`]; the caller creates the source dir and filters with
/// `keep_cfg_mount`. An in-tree `CARGO_TARGET_DIR` is already writable (it lives
/// under the read-write worktree bind), so it's skipped.
pub(crate) fn sandbox_cache_mounts(cfg: &Config, repo_root: &Path) -> Vec<Mount> {
    let mut dirs: Vec<String> = Vec::new();
    if let Ok(home) = std::env::var("HOME") {
        dirs.push(format!("{home}/.cache/prek"));
        dirs.push(format!("{home}/.cache/pre-commit"));
    }
    if let Some(dir) = resolved_sccache_dir(cfg, repo_root) {
        dirs.push(dir);
    }
    if !cfg.disk.shared_target_dir.is_empty() {
        let t = resolve_build_path(&cfg.disk.shared_target_dir, repo_root);
        if !Path::new(&t).starts_with(repo_root) {
            dirs.push(t);
        }
    }
    dirs.into_iter()
        .map(|host| Mount {
            dest: host.clone(),
            host,
            ro: false,
            cache: true,
        })
        .collect()
}

/// Overmount the pre-commit toolchain's caches read-write when a sandbox binds
/// `$HOME` read-only (the hardened/sealed default, or any OCI backend). No-op
/// under an `open`/`all` profile — detected by the presence of a read-only
/// `$HOME` bind in the already-resolved spec, so we never add a redundant mount
/// where `$HOME` is already writable.
pub(crate) fn inject_cache_mounts(spec: &mut SandboxSpec, cfg: &Config, repo_root: &Path) {
    overmount_caches(&mut spec.mounts, cfg, repo_root);
}

/// Core of [`inject_cache_mounts`], on the raw mount list so it's testable
/// without a full `SandboxSpec`. No-op unless the list already binds `$HOME`
/// read-only.
fn overmount_caches(mounts: &mut Vec<Mount>, cfg: &Config, repo_root: &Path) {
    let home = std::env::var("HOME").unwrap_or_default();
    let home_ro = !home.is_empty() && mounts.iter().any(|m| m.host == home && m.ro);
    if !home_ro {
        return;
    }
    for m in sandbox_cache_mounts(cfg, repo_root) {
        // best-effort: bwrap needs the bind source to exist; create a cold cache
        // dir before overmounting it (keep_cfg_mount also requires it to be a
        // real directory to overmount the read-only parent).
        let _ = std::fs::create_dir_all(&m.host);
        if thegn_core::sandbox_mounts::keep_cfg_mount(mounts, &m) {
            mounts.push(m);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_env_vars_off_by_default() {
        let cfg = Config::default();
        assert!(
            build_env_vars(&cfg, Path::new("/repo")).is_empty(),
            "no build env injected unless opted in"
        );
    }

    #[test]
    fn build_env_vars_injects_shared_target() {
        let mut cfg = Config::default();
        cfg.disk.shared_target_dir = "shared-target".into();
        let env = build_env_vars(&cfg, Path::new("/repo"));
        // shared_target_dir present → CARGO_TARGET_DIR resolved against repo root.
        assert!(env.contains(&(
            "CARGO_TARGET_DIR".to_string(),
            "/repo/shared-target".to_string()
        )));
        // sccache off → no RUSTC_WRAPPER regardless of PATH.
        assert!(!env.iter().any(|(k, _)| k == "RUSTC_WRAPPER"));

        // An absolute shared dir is used verbatim.
        cfg.disk.shared_target_dir = "/abs/target".into();
        let env = build_env_vars(&cfg, Path::new("/repo"));
        assert!(env.contains(&("CARGO_TARGET_DIR".to_string(), "/abs/target".to_string())));
    }

    #[test]
    fn resolved_sccache_dir_defaults_to_home_cache_and_honors_custom() {
        let mut cfg = Config::default();
        assert_eq!(resolved_sccache_dir(&cfg, Path::new("/repo")), None);
        cfg.disk.sccache = true;
        if let Ok(home) = std::env::var("HOME") {
            assert_eq!(
                resolved_sccache_dir(&cfg, Path::new("/repo")),
                Some(format!("{home}/.cache/sccache"))
            );
        }
        cfg.disk.sccache_dir = "/custom/sccache".into();
        assert_eq!(
            resolved_sccache_dir(&cfg, Path::new("/repo")),
            Some("/custom/sccache".to_string())
        );
    }

    #[test]
    fn sandbox_cache_mounts_always_covers_the_hook_frameworks() {
        let cfg = Config::default(); // sccache off, no shared target
        let mounts = sandbox_cache_mounts(&cfg, Path::new("/repo"));
        if let Ok(home) = std::env::var("HOME") {
            // The prek / pre-commit hook-framework caches are mounted regardless
            // of sccache, and always read-write & path-preserving.
            for name in ["prek", "pre-commit"] {
                let want = format!("{home}/.cache/{name}");
                let m = mounts.iter().find(|m| m.host == want);
                let m = m.unwrap_or_else(|| panic!("{name} cache mount missing"));
                assert!(!m.ro && m.dest == want);
            }
        }
        // sccache off → no sccache mount.
        assert!(!mounts.iter().any(|m| m.host.ends_with("/sccache")));
    }

    #[test]
    fn sandbox_cache_mounts_adds_sccache_and_out_of_tree_target() {
        let mut cfg = Config::default();
        cfg.disk.sccache = true;
        cfg.disk.sccache_dir = "/cache/sccache".into();
        cfg.disk.shared_target_dir = "/cache/target".into();
        let mounts = sandbox_cache_mounts(&cfg, Path::new("/repo"));
        assert!(mounts.iter().any(|m| m.host == "/cache/sccache" && !m.ro));
        assert!(mounts.iter().any(|m| m.host == "/cache/target" && !m.ro));

        // An IN-tree target dir is already writable via the worktree bind → skip.
        cfg.disk.shared_target_dir = "target".into();
        let mounts = sandbox_cache_mounts(&cfg, Path::new("/repo"));
        assert!(!mounts.iter().any(|m| m.host == "/repo/target"));
    }

    #[test]
    fn overmount_caches_noop_without_a_readonly_home() {
        // No read-only $HOME bind (open/all profile) → nothing overmounted.
        let mut mounts: Vec<Mount> = Vec::new();
        overmount_caches(&mut mounts, &Config::default(), Path::new("/repo"));
        assert!(mounts.is_empty());
    }

    #[test]
    fn overmount_caches_overmounts_under_readonly_home() {
        let Ok(home) = std::env::var("HOME") else {
            return;
        };
        // Hardened substrate: a read-only $HOME bind the caches must overmount.
        let mut mounts = vec![Mount {
            host: home.clone(),
            dest: home.clone(),
            ro: true,
            cache: false,
        }];
        overmount_caches(&mut mounts, &Config::default(), Path::new("/repo"));
        let prek = format!("{home}/.cache/prek");
        assert!(
            mounts.iter().any(|m| m.host == prek && !m.ro),
            "prek cache should be overmounted read-write under a read-only $HOME"
        );
    }
}
