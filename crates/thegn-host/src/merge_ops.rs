//! Reusable merge-queue primitives shared by every surface that enqueues or
//! clears the queue: the `thegn merge` CLI (`cmd/merge.rs`), the agent-facing
//! MCP `HouseMerge` tools (`mcp_merge.rs`), and the control-API daemon
//! (`daemon/service.rs`). Keeping the branch/target resolution and repo-scoped
//! clear in one place means the three surfaces behave identically.
//!
//! Lives in the host crate (not core) because repo-membership needs git
//! resolution (`integrate::main_checkout`), which is host-side.

use anyhow::{Context, Result};
use std::path::{Path, PathBuf};
use thegn_core::config::MergeQueueConfig;
use thegn_core::db::{Db, MergeQueueRow};
use thegn_core::merge_lifecycle::LifecycleEvent;
use thegn_core::remote::GitLoc;
use thegn_core::store::{WorkspaceStore, WorktreeAuxStore};
use thegn_core::util;

use crate::{integrate, merge_driver};

/// The branch a worktree is currently on (`None` when detached).
pub fn branch_of(worktree: &Path) -> Option<String> {
    util::git_out(worktree, &["symbolic-ref", "--quiet", "--short", "HEAD"])
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

/// The repo root (main checkout) a worktree belongs to.
pub fn repo_root_of(worktree: &Path) -> Option<PathBuf> {
    integrate::main_checkout(worktree)
}

/// The [`GitLoc`] of a repo root — the host where the target store (and so the
/// fold/gate/CAS) lives. `Local` for an on-host repo, ssh/provider from the
/// root's own `location`. The merge queue is anchored to this host: the drain
/// must run co-located with it (a remote target can't be folded in-process —
/// see [`is_remote_target`]).
pub fn target_loc(db: &Db, repo_root: &Path) -> GitLoc {
    let root_s = repo_root.to_string_lossy();
    let loc_str = db.location_for(&root_s).ok().flatten();
    GitLoc::from_db(&root_s, loc_str.as_deref())
}

/// A short human label for a target store's host (ssh host / provider prefix),
/// or `None` when it's local. For the "run the drain on that host" guidance.
pub fn target_host_label(loc: &GitLoc) -> Option<String> {
    match loc {
        GitLoc::Local(_) => None,
        GitLoc::Remote { ssh, .. } => Some(ssh.host.clone()),
        GitLoc::Provider { control_prefix, .. } => control_prefix.first().cloned(),
    }
}

/// Guard for the in-process drain/land/integrate paths: when the target repo
/// lives on another host, the fold/gate/CAS can't run here (the object store is
/// remote). Returns a ready-to-print message telling the user to run the drain
/// co-located with the target repo — where Milestone A bundle-fetches any
/// off-host branch tips in. `None` when the target is local (proceed normally).
///
/// (The convenience path — the local UI auto-dispatching to a merge-drain daemon
/// on the target host over ssh/iroh — needs remote-daemon reach that isn't wired
/// yet; see tasks.md J128/129. Running the drain on the target host is the
/// supported workflow until then.)
pub fn remote_target_guard(db: &Db, repo_root: &Path) -> Option<String> {
    let loc = target_loc(db, repo_root);
    let host = target_host_label(&loc)?;
    Some(format!(
        "This repo's target branch lives on another host ({host}). \
         The merge queue folds in the target's object store, so the drain must \
         run there — open a shell on {host} and run `thegn merge drain` (branches \
         queued from other hosts are fetched in automatically)."
    ))
}

/// Queue rows belonging to a repo (membership rule shared with the in-app drain).
pub fn rows_for_repo(db: &Db, root: &Path) -> Vec<MergeQueueRow> {
    merge_driver::rows_for_repo(db, root)
}

/// Enqueue a single worktree's current branch onto the merge queue, applying the
/// sidebar-folder lifecycle. Returns a short human message describing the
/// outcome (queued / skipped). Errors only on a genuinely broken worktree
/// (detached HEAD, not a repo) or a DB write failure.
pub fn enqueue_worktree(mq: &MergeQueueConfig, db: &Db, worktree: &Path) -> Result<String> {
    let root = integrate::main_checkout(worktree)
        .with_context(|| format!("{}: not inside a git repository", worktree.display()))?;
    let target = integrate::resolve_target(mq, &root);
    let branch = branch_of(worktree)
        .with_context(|| format!("{}: not on a branch (detached HEAD?)", worktree.display()))?;
    let wt_s = worktree.to_string_lossy().to_string();
    if branch == target {
        return Ok(format!("skipped {branch} (that's the target branch)"));
    }
    db.enqueue_merge(&wt_s, &branch, &target)?;
    crate::merge_lifecycle::apply(mq, db, &root, &wt_s, &branch, LifecycleEvent::Enqueued);
    Ok(format!("queued {branch}"))
}

/// Drop every queue row for `root`'s repo. Returns the number removed.
pub fn clear_repo(db: &Db, root: &Path) -> Result<usize> {
    let rows = rows_for_repo(db, root);
    let n = rows.len();
    for r in &rows {
        db.remove_merge_entry(&r.worktree)?;
    }
    Ok(n)
}
