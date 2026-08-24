//! The **worktree-aux** seam: assorted per-worktree local state — vim
//! registers, share/forward port bindings, the local merge queue, disk-usage
//! cache, worktree↔issue links, undo marks, and the sandbox audit trail.

use crate::db::{ForwardRow, MergeQueueRow, PrQueueRow, ShareRow};
use crate::models::ContainerEvent;
use anyhow::Result;

/// Object-safe (`&self` + concrete args), so `&dyn WorktreeAuxStore` works for
/// backend-agnostic consumers. [`crate::db::Db`] is the embedded-SQLite impl.
pub trait WorktreeAuxStore {
    /// Persist a register's value (upsert). The single-char `name` is the
    /// register id; the volatile `+` clipboard register is never stored here.
    fn put_register(&self, name: char, value: &str) -> Result<()>;

    /// Load every persisted register as `(name, value)` pairs.
    fn all_registers(&self) -> Result<Vec<(char, String)>>;

    /// Insert or update the share record for `(worktree, local_port)`.
    fn upsert_share(
        &self,
        worktree: &str,
        local_port: u16,
        provider: &str,
        public_url: Option<&str>,
        state: &str,
    ) -> Result<()>;

    /// All persisted shares, newest first (restore + panel listing).
    fn list_shares(&self) -> Result<Vec<ShareRow>>;

    /// Remove the share record for `(worktree, local_port)`.
    fn delete_share(&self, worktree: &str, local_port: u16) -> Result<()>;

    /// Insert or update the forward record for `(worktree, container_port)`.
    fn upsert_forward(
        &self,
        worktree: &str,
        container_port: u16,
        host_port: u16,
        url: &str,
    ) -> Result<()>;

    /// All persisted forwards, newest first (restore + panel listing).
    fn list_forwards(&self) -> Result<Vec<ForwardRow>>;

    /// Remove the forward record for `(worktree, container_port)`.
    fn delete_forward(&self, worktree: &str, container_port: u16) -> Result<()>;

    /// Enqueue (or re-enqueue) a worktree branch for the next fold. Re-enqueueing
    /// resets the row to `queued` and clears any prior result/conflict/error, so
    /// a branch that was deferred and then rebased starts fresh.
    fn enqueue_merge(&self, worktree: &str, branch: &str, target_branch: &str) -> Result<()>;

    /// Update a queued worktree's status and (optionally) its result oid,
    /// conflicted paths (newline-joined), and error detail. Passing `None` leaves
    /// the corresponding column unchanged.
    fn update_merge_status(
        &self,
        worktree: &str,
        status: &str,
        result_oid: Option<&str>,
        conflict_paths: Option<&str>,
        error_detail: Option<&str>,
    ) -> Result<()>;

    /// Re-stamp a queued row's target branch to the one a run is actually
    /// folding into.
    ///
    /// `enqueue_merge` freezes `target_branch` at enqueue time, but the
    /// effective target is resolved per run (`--set
    /// merge_queue.target_branch=…`, or `"auto"` following the repo's default
    /// branch as it moves). Without this the queue keeps reporting the stale
    /// value while the drain folds somewhere else — two different targets shown
    /// for one operation.
    fn set_merge_target(&self, worktree: &str, target_branch: &str) -> Result<()>;

    /// Record how many agent-dispatch cycles have been spent on a row, so the
    /// `agent_max_attempts` budget belongs to the branch rather than to one
    /// invocation of the drain.
    fn set_merge_agent_attempts(&self, worktree: &str, attempts: u32) -> Result<()>;

    /// Re-arm a blocked row for another drain: back to `queued`, attempts reset,
    /// prior failure detail cleared. The "I fixed it, try again" gesture.
    /// Returns whether a row was actually reset.
    fn retry_merge_entry(&self, worktree: &str) -> Result<bool>;

    /// Drop a worktree's merge-queue row (e.g. after a clean land is recorded
    /// elsewhere, or the worktree is removed).
    fn remove_merge_entry(&self, worktree: &str) -> Result<()>;

    /// The whole queue, oldest-queued first (the fold order + UI feed).
    fn list_merge_queue(&self) -> Result<Vec<MergeQueueRow>>;

    // --- PR queue (v50) ---------------------------------------------------
    //
    // Keyed by repo + PR number rather than worktree: a queued pull request need
    // not have a local checkout, so `worktree` is optional throughout.

    /// Queue (or re-queue) a pull request. Re-queueing resets it to `watching`
    /// and clears the prior blocker/detail/attempts, so "I fixed it, watch again"
    /// starts clean — the same gesture as `retry_merge_entry`.
    fn enqueue_pr(
        &self,
        repo_root: &str,
        number: u64,
        worktree: Option<&str>,
        branch: &str,
        base_branch: &str,
        forge: &str,
    ) -> Result<()>;

    /// Update a queued PR's status and, optionally, its blocker word, detail,
    /// and last observed head. Passing `None` leaves that column unchanged, so a
    /// failed refresh can update a note without clobbering a known-good head.
    fn update_pr_status(
        &self,
        key: &str,
        status: &str,
        blocker: Option<&str>,
        detail: Option<&str>,
        last_head_oid: Option<&str>,
    ) -> Result<()>;

    /// Record agent-dispatch cycles spent on a PR, so the budget belongs to the
    /// pull request rather than to one drain.
    fn set_pr_agent_attempts(&self, key: &str, attempts: u32) -> Result<()>;

    /// Drop one queued pull request.
    fn remove_pr_entry(&self, key: &str) -> Result<()>;

    /// Drop every queued pull request for a repo. Returns how many went.
    fn clear_pr_queue(&self, repo_root: &str) -> Result<usize>;

    /// Every queued pull request, oldest-queued first.
    fn list_pr_queue(&self) -> Result<Vec<PrQueueRow>>;

    /// `(size_bytes, target_bytes, fetched_at)` for one worktree, or `None`.
    fn get_worktree_disk(&self, worktree: &str) -> Result<Option<(i64, i64, i64)>>;

    fn put_worktree_disk(&self, worktree: &str, size_bytes: i64, target_bytes: i64) -> Result<()>;

    /// All cached disk sizes keyed by worktree path → `(size_bytes, target_bytes)`.
    /// One bulk read for the sidebar/statusbar; never scans.
    fn all_worktree_disk(&self) -> Result<std::collections::HashMap<String, (i64, i64)>>;

    /// Every size-cache key with its fetch timestamp. Feeds the background
    /// scanner's TTL/priority planning and its orphan sweep in one read, instead
    /// of a `get_worktree_disk` per registry row.
    fn all_worktree_disk_stamps(&self) -> Result<std::collections::HashMap<String, i64>>;

    /// Drop a worktree's cached size (e.g. right after a `clean`) so the badge
    /// clears without waiting for the next scan.
    fn delete_worktree_disk(&self, worktree: &str) -> Result<()>;

    /// Associate `issue_id` (in `"<provider>:<key>"` form) with a worktree path.
    fn link_issue(&self, worktree_path: &str, issue_id: &str) -> Result<()>;

    /// Remove a worktree↔issue association.
    fn unlink_issue(&self, worktree_path: &str, issue_id: &str) -> Result<()>;

    /// All issue ids linked to a worktree, newest first.
    fn linked_issues(&self, worktree_path: &str) -> Result<Vec<String>>;

    /// Record a reset target we are about to create, pruning each worktree's
    /// mark set to the freshest 100 (the undo planner only reads ~100 reflog
    /// entries anyway).
    fn add_undo_mark(&self, worktree: &str, sha: &str) -> Result<()>;

    /// All recorded undo-reset targets for a worktree (newest first).
    fn undo_marks(&self, worktree: &str) -> Result<Vec<String>>;

    /// Record a sandbox event (exec, network, dns, orphan_gc) in the audit log.
    fn insert_container_event(
        &self,
        worktree: &str,
        ts: i64,
        kind: &str,
        detail: Option<&str>,
        exit_code: Option<i64>,
    ) -> Result<()>;

    /// Retrieve the most recent `limit` container events for a worktree,
    /// newest first.
    fn container_events(&self, worktree: &str, limit: usize) -> Result<Vec<ContainerEvent>>;

    /// Delete container events older than `older_than_secs` seconds. Called on
    /// startup to keep the audit table from growing unbounded.
    fn prune_container_events(&self, older_than_secs: i64) -> Result<usize>;
}
