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
use thegn_core::config::Config;
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

/// The `GitLoc` of a repo root — the host where the target store (and so the
/// fold/gate/CAS) lives. `Local` for an on-host repo, ssh/provider from the
/// root's own `location`. The merge queue is anchored to this host: the drain
/// must run co-located with it (a remote target can't be folded in-process —
/// see `is_remote_target`).
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

/// Push the advanced target branch to `origin` — the `push` `remote_mode`'s
/// convergence step after a sprite drains its own clone. Surfaces git's stderr
/// on failure so a rejected push is a visible error, never a false success.
pub fn push_target(repo_root: &Path, target: &str) -> Result<()> {
    #[expect(clippy::disallowed_methods)] // one-shot CLI push, not a loop read
    let out = util::git_cmd(repo_root)
        .args(["push", "origin", target])
        .output()
        .with_context(|| format!("spawning `git push origin {target}`"))?;
    if !out.status.success() {
        anyhow::bail!(
            "`git push origin {target}` failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(())
}

/// Queue rows belonging to a repo (membership rule shared with the in-app drain).
pub fn rows_for_repo(db: &Db, root: &Path) -> Vec<MergeQueueRow> {
    merge_driver::rows_for_repo(db, root)
}

/// Normalize a worktree path before it is used as a queue-row key.
///
/// The queue is keyed by worktree path, and the argument arrives straight from
/// the CLI — so `merge add .` used to store the literal `"."`. That is not just
/// ugly: the key is global across repos, so two repos would collide on it, and
/// the row-to-repo membership test re-resolves the key against the *current*
/// process cwd, which only matches from the directory it was added in. Git's own
/// `worktree list` reports absolute resolved paths, so matching that form keeps
/// the two consistent. Falls back to the input when the path can't be resolved
/// (it may simply not exist — the caller reports that).
pub fn canonical_worktree(worktree: &Path) -> PathBuf {
    std::fs::canonicalize(worktree).unwrap_or_else(|_| worktree.to_path_buf())
}

/// The repo root for a worktree, with an error that names the actual problem.
///
/// `main_checkout` runs `git -C <path> worktree list` and collapses every
/// failure to `None`, so a path that simply no longer exists (the common case
/// after `on_landed = "remove"` deleted it) reported "not inside a git
/// repository" — which sent the reader looking in entirely the wrong place.
fn resolve_repo_root(worktree: &Path) -> Result<PathBuf> {
    if !worktree.exists() {
        anyhow::bail!("no such worktree: {}", worktree.display());
    }
    integrate::main_checkout(worktree)
        .with_context(|| format!("{}: not inside a git repository", worktree.display()))
}

/// Enqueue a single worktree's current branch onto the merge queue, applying the
/// sidebar-folder lifecycle. Returns a short human message describing the
/// outcome (queued / skipped). Errors only on a genuinely broken worktree
/// (detached HEAD, not a repo) or a DB write failure.
pub fn enqueue_worktree(cfg: &Config, db: &Db, worktree: &Path) -> Result<String> {
    let worktree = &canonical_worktree(worktree);
    let root = resolve_repo_root(worktree)?;
    // Takes the whole `Config` rather than a `MergeQueueConfig`: only here is the
    // repo root known, and the per-repo layer can't be applied without it.
    let mq = &cfg.repo_merge_queue(&root);
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

/// Remove one worktree's branch from the queue AND un-file it from its
/// lifecycle folder — the symmetric teardown to [`enqueue_worktree`]. A plain
/// dequeue neither lands nor fails, so without the `Dequeued` lifecycle the
/// worktree would be stranded in the "Merging"/"Needs attention" folder its
/// enqueue filed it into (the sidebar/queue de-sync). The un-file is best-effort
/// and guarded host-side to lifecycle-managed folders; dropping the row is the
/// operation that can fail.
pub fn dequeue_worktree(cfg: &Config, db: &Db, worktree: &Path) -> Result<()> {
    let worktree = &canonical_worktree(worktree);
    let wt_s = worktree.to_string_lossy().to_string();
    db.remove_merge_entry(&wt_s)?;
    // `apply` only needs the worktree + repo root for a dequeue (the branch is
    // unused by the un-file), so an empty branch is fine; skip only if the repo
    // root can't be resolved (worktree dir already gone — nothing to un-file).
    if let Some(root) = integrate::main_checkout(worktree) {
        let mq = &cfg.repo_merge_queue(&root);
        crate::merge_lifecycle::apply(mq, db, &root, &wt_s, "", LifecycleEvent::Dequeued);
    }
    Ok(())
}

/// Drop every queue row for `root`'s repo, un-filing each from its lifecycle
/// folder. Returns the number removed.
pub fn clear_repo(cfg: &Config, db: &Db, root: &Path) -> Result<usize> {
    let rows = rows_for_repo(db, root);
    let n = rows.len();
    for r in &rows {
        dequeue_worktree(cfg, db, Path::new(&r.worktree))?;
    }
    Ok(n)
}
