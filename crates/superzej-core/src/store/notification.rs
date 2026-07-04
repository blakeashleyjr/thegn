//! The **notification** seam: the notification feed (unread/alert rollups
//! per worktree) and the agent-dispatch registry (which worktree an agent task
//! was dispatched to, and its status).

use anyhow::Result;

/// Object-safe (`&self` + concrete args), so `&dyn NotificationStore` works for
/// backend-agnostic consumers. [`crate::db::Db`] is the embedded-SQLite impl.
pub trait NotificationStore {
    /// Append a notification.  Returns the new row id.
    fn put_notification(
        &self,
        kind: &str,
        issue_id: &str,
        message: &str,
        worktree_path: &str,
    ) -> Result<i64>;

    /// All unread notifications, newest first.
    fn get_unread_notifications(&self) -> Result<Vec<crate::notification::Notification>>;

    /// All notifications (read and unread), newest first, capped at `limit`.
    fn get_all_notifications(&self, limit: usize)
    -> Result<Vec<crate::notification::Notification>>;

    /// Mark a single notification as read.
    fn mark_notification_read(&self, id: i64) -> Result<()>;

    /// Mark all notifications as read.
    fn mark_all_notifications_read(&self) -> Result<()>;

    /// Get unread notification counts grouped by worktree_path.
    /// Returns a map from worktree_path to count of unread notifications.
    /// Unread notification counts grouped by worktree, restricted to `counted_kinds`
    /// (the config-derived non-`info` kinds). Informational kinds are excluded by
    /// passing only the counted set, so lifecycle events never inflate the badge.
    /// An empty slice yields an empty map.
    fn get_unread_counts_by_worktree(
        &self,
        counted_kinds: &[&str],
    ) -> Result<std::collections::BTreeMap<String, usize>>;

    /// Alert counts grouped by worktree, restricted to `alert_kinds` (the
    /// config-derived `alert`-priority kinds). Drives the red ⚑ flag badge. An
    /// empty slice yields an empty map (no flag).
    fn get_alert_counts_by_worktree(
        &self,
        alert_kinds: &[&str],
    ) -> Result<std::collections::BTreeMap<String, usize>>;

    /// Delete a single notification row (dismiss).
    fn delete_notification(&self, id: i64) -> Result<()>;

    /// Record a new agent dispatch.  Returns the new row id.
    fn put_agent_dispatch(
        &self,
        issue_id: &str,
        worktree_path: &str,
        agent_name: &str,
    ) -> Result<i64>;

    /// Update the status of a dispatch.
    fn update_dispatch_status(&self, id: i64, status: &str) -> Result<()>;

    /// Find the dispatch id for a worktree path (most recent, if any).
    fn dispatch_for_worktree(&self, worktree_path: &str) -> Result<Option<i64>>;

    /// The dispatch timestamp (`dispatched_at_ms`) of a worktree's most recent
    /// agent dispatch, if any. Read at resurrection to age a persisted
    /// running/active agent signal through [`crate::activity::coerce_stale`], so a
    /// phantom forever-running dot from a session killed mid-run is downgraded.
    fn dispatch_dispatched_at_ms(&self, worktree_path: &str) -> Result<Option<i64>>;

    /// Find the dispatch id and originating issue id for a worktree path.
    fn dispatch_info_for_worktree(&self, worktree_path: &str) -> Result<Option<(i64, String)>>;
}
