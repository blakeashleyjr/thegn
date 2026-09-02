//! Shared host-side worktree creation lifecycle.
//!
//! Core owns branch/path policy; this module owns the host adapter that sends
//! git mutations through GitBackend and performs best-effort submodule setup on
//! the worker that already owns creation.

use std::path::Path;

use thegn_core::config::{Config, SubmoduleMode, WorktreeMode};
use thegn_core::remote::GitLoc;
use thegn_core::store::RepoTrustStore;

/// Create a linked worktree through the service seam. Directory preparation and
/// the in-repo exclude are host policy; the git mutation is never a core
/// subprocess at this call site.
#[allow(dead_code)]
pub(crate) fn add_checked(
    root: &Path,
    branch: &str,
    base: &str,
    path: &Path,
    cfg: &Config,
) -> Result<(), String> {
    if cfg.worktree_mode == WorktreeMode::InRepo {
        let exclude = root.join(".git/info/exclude");
        if let Ok(contents) = std::fs::read_to_string(&exclude)
            && !contents.lines().any(|line| line == ".worktrees/")
        {
            use std::io::Write;
            if let Ok(mut file) = std::fs::OpenOptions::new().append(true).open(&exclude) {
                let _ = writeln!(file, ".worktrees/");
            }
        }
    }
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let loc = GitLoc::for_worktree(root);
    thegn_svc::git::GitBackend::add_worktree(&*crate::git_handle::get(), root, branch, base, path)
        .map_err(|error| {
            format!(
                "git worktree add failed (branch={branch} base={base} at {}): {error}",
                loc.path()
            )
        })
}

#[allow(dead_code)]
pub(crate) fn remove(root: &Path, path: &Path, delete_branch: bool) -> bool {
    let removed = crate::git_handle::get()
        .remove_worktree(root, path, delete_branch)
        .is_ok();
    if removed {
        let _ = std::fs::remove_dir_all(path);
    }
    removed
}

/// Initialize submodules after a checkout exists. Ok(false) means policy
/// disabled or there was no root-level .gitmodules; failures are non-fatal to
/// worktree creation and are reported by the caller.
pub(crate) fn initialize(
    cfg: &Config,
    repo_root: &Path,
    worktree: &Path,
    loc: Option<&GitLoc>,
) -> Result<bool, String> {
    if cfg.repo_git(repo_root).submodules == SubmoduleMode::Off {
        return Ok(false);
    }
    let local_has_metadata = worktree.join(".gitmodules").is_file();
    if !local_has_metadata && loc.is_none_or(|location| !location.is_remote()) {
        return Ok(false);
    }
    let metadata_root = if local_has_metadata {
        worktree
    } else {
        repo_root
    };
    let metadata = std::fs::read_to_string(metadata_root.join(".gitmodules"))
        .map_err(|e| format!("submodule metadata unreadable: {e}"))?;
    let specs = thegn_core::submodule::parse_gitmodules(&metadata)
        .map_err(|e| format!("submodule metadata invalid: {e}"))?;
    if specs.is_empty() {
        return Ok(false);
    }
    let request = thegn_core::config_resolve::GatedRequest {
        key: "git.submodules".into(),
        value: serde_json::json!(
            specs
                .iter()
                .map(|spec| serde_json::json!({"path": spec.path.trim(), "url": spec.url.trim()}))
                .collect::<Vec<_>>()
        ),
        summary: format!("initialize {} recursive submodule(s)", specs.len()),
    };
    let root_s = repo_root.to_string_lossy();
    let approved = thegn_core::db::Db::open()
        .ok()
        .and_then(|db| db.repo_trust_approved(&root_s).ok())
        .is_some_and(|approved| thegn_core::repo_trust::is_approved(&request, &approved));
    if !approved {
        let id = thegn_core::repo_trust::request_id(&request.canonical());
        return Err(format!(
            "submodules not initialized: trust approval required (repo trust request {id})"
        ));
    }
    let location = loc
        .cloned()
        .unwrap_or_else(|| GitLoc::for_worktree(worktree));
    crate::git_handle::get()
        .init_submodules(&location, true)
        .map(|_| true)
        .map_err(|e| format!("submodules not initialized: {e}"))
}

/// Whether a remote clone may run the recursive update command. This is kept
/// host-side because the remote script must not make an independent trust
/// decision or enable a git transport override.
pub(crate) fn recursive_submodules_allowed(cfg: &Config, repo_root: &Path) -> bool {
    if cfg.repo_git(repo_root).submodules == SubmoduleMode::Off {
        return false;
    }
    let Ok(metadata) = std::fs::read_to_string(repo_root.join(".gitmodules")) else {
        return false;
    };
    let Ok(specs) = thegn_core::submodule::parse_gitmodules(&metadata) else {
        return false;
    };
    if specs.is_empty() {
        return false;
    }
    let request = thegn_core::config_resolve::GatedRequest {
        key: "git.submodules".into(),
        value: serde_json::json!(
            specs
                .iter()
                .map(|spec| serde_json::json!({"path": spec.path.trim(), "url": spec.url.trim()}))
                .collect::<Vec<_>>()
        ),
        summary: format!("initialize {} recursive submodule(s)", specs.len()),
    };
    let root_s = repo_root.to_string_lossy();
    thegn_core::db::Db::open()
        .ok()
        .and_then(|db| db.repo_trust_approved(&root_s).ok())
        .is_some_and(|approved| thegn_core::repo_trust::is_approved(&request, &approved))
}
