//! Reusable merge-queue primitives shared by every surface that enqueues or
//! clears the queue: the `szhost merge` CLI (`cmd/merge.rs`), the agent-facing
//! MCP `HouseMerge` tools (`mcp_merge.rs`), and the control-API daemon
//! (`daemon/service.rs`). Keeping the branch/target resolution and repo-scoped
//! clear in one place means the three surfaces behave identically.
//!
//! Lives in the host crate (not core) because repo-membership needs git
//! resolution (`integrate::main_checkout`), which is host-side.

use anyhow::{Context, Result};
use std::path::{Path, PathBuf};
use superzej_core::config::MergeQueueConfig;
use superzej_core::db::{Db, MergeQueueRow};
use superzej_core::merge_lifecycle::LifecycleEvent;
use superzej_core::store::WorktreeAuxStore;
use superzej_core::util;

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
