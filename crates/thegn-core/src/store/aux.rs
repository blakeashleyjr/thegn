//! The **worktree-aux** seam: assorted per-worktree local state — vim
//! registers, share/forward port bindings, the local merge queue, disk-usage
//! cache, worktree↔issue links, undo marks, and the sandbox audit trail.

use crate::db::{ForwardRow, MergeQueueRow, ShareRow};
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

    /// Drop a worktree's merge-queue row (e.g. after a clean land is recorded
    /// elsewhere, or the worktree is removed).
    fn remove_merge_entry(&self, worktree: &str) -> Result<()>;

    /// The whole queue, oldest-queued first (the fold order + UI feed).
    fn list_merge_queue(&self) -> Result<Vec<MergeQueueRow>>;

    /// `(size_bytes, target_bytes, fetched_at)` for one worktree, or `None`.
    fn get_worktree_disk(&self, worktree: &str) -> Result<Option<(i64, i64, i64)>>;

    fn put_worktree_disk(&self, worktree: &str, size_bytes: i64, target_bytes: i64) -> Result<()>;

    /// All cached disk sizes keyed by worktree path → `(size_bytes, target_bytes)`.
    /// One bulk read for the sidebar/statusbar; never scans.
    fn all_worktree_disk(&self) -> Result<std::collections::HashMap<String, (i64, i64)>>;

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
