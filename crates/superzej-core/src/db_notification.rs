//! NotificationStore state — the embedded-SQLite implementation of the [`NotificationStore`] seam.
//! Sibling `impl` block (via the `conn()` accessor) so the pinned `db.rs`
//! only carries the schema DDL, not these bodies. The DB is a cache; git /
//! the live source is truth. A server backend implements this trait against
//! Postgres for shared, multi-user state.

use crate::db::Db;
use crate::store::NotificationStore;
use crate::util;
use anyhow::Result;
use rusqlite::{OptionalExtension, params};

impl NotificationStore for Db {
    /// Append a notification.  Returns the new row id.
    fn put_notification(
        &self,
        kind: &str,
        issue_id: &str,
        message: &str,
        worktree_path: &str,
    ) -> Result<i64> {
        self.conn().execute(
            r#"INSERT INTO notifications(kind,issue_id,message,created_at_ms,read,worktree_path)
               VALUES(?1,?2,?3,?4,0,?5)"#,
            params![kind, issue_id, message, util::now(), worktree_path],
        )?;
        Ok(self.conn().last_insert_rowid())
    }

    /// All unread notifications, newest first.
    fn get_unread_notifications(&self) -> Result<Vec<crate::notification::Notification>> {
        self.notifications_query(
            "SELECT id,kind,issue_id,message,created_at_ms,read,worktree_path \
             FROM notifications WHERE read=0 ORDER BY created_at_ms DESC",
            rusqlite::params![],
            usize::MAX,
        )
    }

    /// All notifications (read and unread), newest first, capped at `limit`.
    fn get_all_notifications(
        &self,
        limit: usize,
    ) -> Result<Vec<crate::notification::Notification>> {
        self.notifications_query(
            "SELECT id,kind,issue_id,message,created_at_ms,read,worktree_path \
             FROM notifications ORDER BY created_at_ms DESC",
            rusqlite::params![],
            limit,
        )
    }

    /// Mark a single notification as read.
    fn mark_notification_read(&self, id: i64) -> Result<()> {
        self.conn()
            .execute("UPDATE notifications SET read=1 WHERE id=?1", params![id])?;
        Ok(())
    }

    /// Mark all notifications as read.
    fn mark_all_notifications_read(&self) -> Result<()> {
        self.conn().execute("UPDATE notifications SET read=1", [])?;
        Ok(())
    }

    /// Get unread notification counts grouped by worktree_path.
    /// Returns a map from worktree_path to count of unread notifications.
    /// Unread notification counts grouped by worktree, restricted to `counted_kinds`
    /// (the config-derived non-`info` kinds). Informational kinds are excluded by
    /// passing only the counted set, so lifecycle events never inflate the badge.
    /// An empty slice yields an empty map.
    fn get_unread_counts_by_worktree(
        &self,
        counted_kinds: &[&str],
    ) -> Result<std::collections::BTreeMap<String, usize>> {
        self.unread_counts_for_kinds(counted_kinds)
    }

    /// Alert counts grouped by worktree, restricted to `alert_kinds` (the
    /// config-derived `alert`-priority kinds). Drives the red ⚑ flag badge. An
    /// empty slice yields an empty map (no flag).
    fn get_alert_counts_by_worktree(
        &self,
        alert_kinds: &[&str],
    ) -> Result<std::collections::BTreeMap<String, usize>> {
        self.unread_counts_for_kinds(alert_kinds)
    }

    /// Delete a single notification row (dismiss).
    fn delete_notification(&self, id: i64) -> Result<()> {
        self.conn()
            .execute("DELETE FROM notifications WHERE id=?1", params![id])?;
        Ok(())
    }

    /// Record a new agent dispatch.  Returns the new row id.
    fn put_agent_dispatch(
        &self,
        issue_id: &str,
        worktree_path: &str,
        agent_name: &str,
    ) -> Result<i64> {
        self.conn().execute(
            r#"INSERT INTO agent_dispatches(issue_id,worktree_path,agent_name,dispatched_at_ms,status)
               VALUES(?1,?2,?3,?4,'queued')"#,
            params![issue_id, worktree_path, agent_name, util::now()],
        )?;
        Ok(self.conn().last_insert_rowid())
    }

    /// Update the status of a dispatch.
    fn update_dispatch_status(&self, id: i64, status: &str) -> Result<()> {
        self.conn().execute(
            "UPDATE agent_dispatches SET status=?1 WHERE id=?2",
            params![status, id],
        )?;
        Ok(())
    }

    /// Find the dispatch id for a worktree path (most recent, if any).
    fn dispatch_for_worktree(&self, worktree_path: &str) -> Result<Option<i64>> {
        Ok(self.conn()
            .query_row(
                "SELECT id FROM agent_dispatches WHERE worktree_path=?1 ORDER BY dispatched_at_ms DESC, id DESC LIMIT 1",
                params![worktree_path],
                |r| r.get::<_, i64>(0),
            )
            .optional()?)
    }

    /// The dispatch timestamp (`dispatched_at_ms`) of a worktree's most recent
    /// agent dispatch, if any. Read at resurrection to age a persisted
    /// running/active agent signal through [`crate::activity::coerce_stale`], so a
    /// phantom forever-running dot from a session killed mid-run is downgraded.
    fn dispatch_dispatched_at_ms(&self, worktree_path: &str) -> Result<Option<i64>> {
        Ok(self
            .conn()
            .query_row(
                "SELECT dispatched_at_ms FROM agent_dispatches WHERE worktree_path=?1 \
                 ORDER BY dispatched_at_ms DESC, id DESC LIMIT 1",
                params![worktree_path],
                |r| r.get::<_, i64>(0),
            )
            .optional()?)
    }

    /// Find the dispatch id and originating issue id for a worktree path.
    fn dispatch_info_for_worktree(&self, worktree_path: &str) -> Result<Option<(i64, String)>> {
        Ok(self
            .conn()
            .query_row(
                "SELECT id, issue_id FROM agent_dispatches WHERE worktree_path=?1 \
                 ORDER BY dispatched_at_ms DESC, id DESC LIMIT 1",
                params![worktree_path],
                |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?)),
            )
            .optional()?)
    }
}
