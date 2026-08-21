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

    /// Append a notification only if an identical `(kind, issue_id, message)`
    /// row doesn't already exist (emit-once for re-derived facts like overdue
    /// / mentions). Returns whether a row was inserted.
    fn put_notification_once(
        &self,
        kind: &str,
        issue_id: &str,
        message: &str,
        worktree_path: &str,
    ) -> Result<bool>;

    /// All unread notifications, newest first.
    fn get_unread_notifications(&self) -> Result<Vec<crate::notification::Notification>>;

    /// All notifications (read and unread), newest first, capped at `limit`.
    fn get_all_notifications(&self, limit: usize)
    -> Result<Vec<crate::notification::Notification>>;

    /// Mark a single notification as read.
    fn mark_notification_read(&self, id: i64) -> Result<()>;

    /// Mark all notifications as read.
    fn mark_all_notifications_read(&self) -> Result<()>;
    /// Mark read only what the repo-scoped inbox shows: rows tagged with one
    /// of `worktree_paths`, plus untagged (host-global) rows. The unscoped
    /// clear stays for the all-worktrees (`g`) view — a repo-scoped inbox's
    /// "clear all" must not silently mark OTHER repos' notifications read.
    fn mark_notifications_read_scoped(&self, worktree_paths: &[String]) -> Result<()>;

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

    /// Acknowledge (quiet) a worktree's live "Needs you" signal: UPSERT the
    /// `(reason, since, episode)` currently showing so it's suppressed until that
    /// episode changes. `reason` is the serde-encoded
    /// [`crate::attention::AttentionReason`]; keyed per `(worktree, reason)` so
    /// acking one signal never destroys the ack for another on the same worktree.
    fn put_attention_ack(
        &self,
        worktree_path: &str,
        reason: &str,
        since: Option<i64>,
        episode: crate::attention::Episode,
    ) -> Result<()>;

    /// Every stored attention ack. The host matches these against fresh scores;
    /// a non-match means only "that signal isn't the winner right now", so the
    /// read pass never deletes — see [`crate::attention::ack_expired`].
    fn list_attention_acks(&self) -> Result<Vec<crate::attention::AttentionAckRow>>;

    /// Drop attention acks for a worktree — one `reason`, or all of them when
    /// `reason` is `None` (worktree removed / explicit un-ack).
    fn delete_attention_ack(&self, worktree_path: &str, reason: Option<&str>) -> Result<()>;

    /// Drop acks older than `max_age_secs`. A table-growth bound only: acks are
    /// released by a new episode, not by this sweep. Returns the rows removed.
    fn prune_attention_acks(&self, max_age_secs: i64) -> Result<usize>;

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
