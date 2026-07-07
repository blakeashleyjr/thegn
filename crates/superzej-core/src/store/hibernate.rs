//! The **hibernation** seam (schema v39): worktrees whose provider compute is
//! snapshot-then-destroyed while idle, keyed by worktree path.
//!
//! State machine (intent-before-action, like the VPS create ledger):
//!
//! ```text
//! (no row) ─put─▶ capturing ─snapshot verified─▶ destroying ─destroy ok─▶ hibernated
//!                     │                              │                        │
//!                     │ capture failed:              │ crash: healing sweep   └─▶ restoring
//!                     │  delete row, VM kept         │  re-drives the destroy      │ ok: delete row
//!                     │ crash: sweep discards        │  (idempotent, 404 = gone)   │ failed: back to
//!                     ▼  the stale intent            ▼                             ▼  hibernated
//! ```
//!
//! A server backend would implement this against
//! Postgres; the local shell implements it over the embedded SQLite `Db`
//! (`db_hibernate.rs`). `HibernationRow` is defined in [`crate::db`].

use anyhow::Result;

/// One `worktree_hibernations` row (v39): a worktree whose provider compute
/// was snapshot-then-destroyed (or is mid-capture/mid-restore).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HibernationRow {
    pub worktree_path: String,
    pub repo_path: String,
    pub env_name: String,
    pub sandbox_name: String,
    /// The snapshot id in the `[lifecycle.snapshot]` store.
    pub snapshot_id: String,
    /// Sandbox HEAD at capture time (empty for unborn).
    pub head: String,
    /// `"capturing"` | `"hibernated"` | `"restoring"`.
    pub state: String,
    pub created_at: i64,
    pub updated_at: i64,
}

/// Persisted hibernation state. Object-safe (`&self` + concrete args), so
/// `&dyn HibernationStore` works for backend-agnostic consumers.
pub trait HibernationStore {
    /// Insert/replace the row for `worktree_path` (state `"capturing"`), the
    /// intent record written BEFORE the capture starts.
    fn put_hibernation(&self, row: &HibernationRow) -> Result<()>;

    /// Advance the row's state (`capturing` → `hibernated` → `restoring` …)
    /// and optionally re-point it at a (re-captured) snapshot id.
    fn set_hibernation_state(
        &self,
        worktree_path: &str,
        state: &str,
        snapshot_id: Option<&str>,
    ) -> Result<()>;

    /// The row for one worktree, or `None`.
    fn hibernation_for(&self, worktree_path: &str) -> Result<Option<HibernationRow>>;

    /// Every hibernation row, any state (UI + reaper sweep).
    fn hibernations(&self) -> Result<Vec<HibernationRow>>;

    /// Remove the row: restore completed, or a failed capture kept the VM.
    fn delete_hibernation(&self, worktree_path: &str) -> Result<()>;
}
