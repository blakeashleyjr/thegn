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

    /// Append a notification only if an identical `(kind, issue_id, message)`
    /// row doesn't already exist — the emit-once primitive for producers that
    /// re-derive the same fact every refresh (overdue, mentions) rather than
    /// diffing old-vs-new state. A changed message (e.g. a moved due date)
    /// re-arms. Returns whether a row was inserted.
    fn put_notification_once(
        &self,
        kind: &str,
        issue_id: &str,
        message: &str,
        worktree_path: &str,
    ) -> Result<bool> {
        let n = self.conn().execute(
            r#"INSERT INTO notifications(kind,issue_id,message,created_at_ms,read,worktree_path)
               SELECT ?1,?2,?3,?4,0,?5
               WHERE NOT EXISTS (
                   SELECT 1 FROM notifications
                   WHERE kind=?1 AND issue_id=?2 AND message=?3
               )"#,
            params![kind, issue_id, message, util::now(), worktree_path],
        )?;
        Ok(n > 0)
    }

    fn has_notification(&self, kind: &str, issue_id: &str) -> Result<bool> {
        let n: i64 = self.conn().query_row(
            "SELECT EXISTS(SELECT 1 FROM notifications WHERE kind=?1 AND issue_id=?2)",
            params![kind, issue_id],
            |r| r.get(0),
        )?;
        Ok(n != 0)
    }

    /// All unread notifications, newest first.
    fn get_unread_notifications(&self) -> Result<Vec<crate::notification::Notification>> {
        self.notifications_query(
            "SELECT id,kind,issue_id,message,created_at_ms,read,worktree_path \
             FROM notifications WHERE read=0 ORDER BY created_at_ms DESC",
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

    /// Repo-scoped clear: rows tagged with one of `worktree_paths` + untagged
    /// (host-global) rows — exactly the set the scoped inbox displays.
    fn mark_notifications_read_scoped(&self, worktree_paths: &[String]) -> Result<()> {
        let conn = self.conn();
        conn.execute("UPDATE notifications SET read=1 WHERE worktree_path=''", [])?;
        for p in worktree_paths {
            conn.execute(
                "UPDATE notifications SET read=1 WHERE worktree_path=?1",
                params![p],
            )?;
        }
        Ok(())
    }

    fn mark_notifications_read_for_worktree(&self, worktree_path: &str) -> Result<()> {
        self.conn().execute(
            "UPDATE notifications SET read=1 WHERE worktree_path=?1",
            params![worktree_path],
        )?;
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

    fn put_attention_ack(
        &self,
        worktree_path: &str,
        reason: &str,
        since: Option<i64>,
        episode: crate::attention::Episode,
    ) -> Result<()> {
        self.conn().execute(
            r#"INSERT INTO attention_acks(worktree_path,reason,since,episode,acked_at)
               VALUES(?1,?2,?3,?4,?5)
               ON CONFLICT(worktree_path,reason) DO UPDATE SET
                 since=excluded.since, episode=excluded.episode, acked_at=excluded.acked_at"#,
            params![worktree_path, reason, since, episode as i64, util::now()],
        )?;
        Ok(())
    }

    fn list_attention_acks(&self) -> Result<Vec<crate::attention::AttentionAckRow>> {
        let conn = self.conn();
        let mut stmt = conn.prepare(
            "SELECT worktree_path, reason, since, episode, acked_at FROM attention_acks",
        )?;
        let rows = stmt
            .query_map([], |r| {
                Ok(crate::attention::AttentionAckRow {
                    worktree_path: r.get::<_, String>(0)?,
                    reason: r.get::<_, String>(1)?,
                    since: r.get::<_, Option<i64>>(2)?,
                    episode: r.get::<_, i64>(3)? as u64,
                    acked_at: r.get::<_, i64>(4)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    fn delete_attention_ack(&self, worktree_path: &str, reason: Option<&str>) -> Result<()> {
        match reason {
            Some(r) => self.conn().execute(
                "DELETE FROM attention_acks WHERE worktree_path=?1 AND reason=?2",
                params![worktree_path, r],
            )?,
            None => self.conn().execute(
                "DELETE FROM attention_acks WHERE worktree_path=?1",
                params![worktree_path],
            )?,
        };
        Ok(())
    }

    fn prune_attention_acks(&self, max_age_secs: i64) -> Result<usize> {
        let cutoff = util::now().saturating_sub(max_age_secs);
        // `acked_at > 0` spares pre-v50 rows migrated with an unknown stamp:
        // they are aged out by `attention::ack_expired`, not deleted blind.
        Ok(self.conn().execute(
            "DELETE FROM attention_acks WHERE acked_at > 0 AND acked_at < ?1",
            params![cutoff],
        )?)
    }

    /// Record a new agent dispatch.  Returns the new row id.
    fn put_agent_dispatch(&self, new: crate::issue::NewDispatch<'_>) -> Result<i64> {
        self.conn().execute(
            r#"INSERT INTO agent_dispatches
                 (issue_id,worktree_path,agent_name,dispatched_at_ms,status,
                  stage,parent_id,session_id,artifact_path)
               VALUES(?1,?2,?3,?4,'queued',?5,?6,?7,?8)"#,
            params![
                new.issue_id,
                new.worktree_path,
                new.agent_name,
                // MILLISECONDS: the column is `dispatched_at_ms` and every
                // reader treats it as such (the board's age, the sidebar's
                // blocked-since). `util::now()` here stored seconds, which made
                // a fresh row read as dispatched in 1970.
                util::now_ms(),
                new.stage,
                new.parent_id,
                new.session_id,
                new.artifact_path,
            ],
        )?;
        Ok(self.conn().last_insert_rowid())
    }

    /// Update the status of a dispatch. Stores the typed status's canonical
    /// `as_str()` form, so the column can never drift to a value
    /// [`AgentDispatchStatus::parse`](crate::issue::AgentDispatchStatus::parse)
    /// does not round-trip.
    fn update_dispatch_status(
        &self,
        id: i64,
        status: crate::issue::AgentDispatchStatus,
    ) -> Result<()> {
        self.conn().execute(
            "UPDATE agent_dispatches SET status=?1 WHERE id=?2",
            params![status.as_str(), id],
        )?;
        Ok(())
    }

    /// The whole roster, newest first, with stored status strings coerced
    /// through [`AgentDispatchStatus::parse`](crate::issue::AgentDispatchStatus::parse)
    /// (unknown → `Unknown`, never an error).
    fn list_dispatches(&self) -> Result<Vec<crate::issue::AgentDispatch>> {
        let conn = self.conn();
        let mut stmt = conn.prepare(&format!(
            "SELECT {DISPATCH_COLS} FROM agent_dispatches ORDER BY dispatched_at_ms DESC, id DESC"
        ))?;
        let rows = stmt
            .query_map([], map_dispatch)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    /// One dispatch row by id (typed), coerced like [`Self::list_dispatches`].
    fn get_dispatch(&self, id: i64) -> Result<Option<crate::issue::AgentDispatch>> {
        Ok(self
            .conn()
            .query_row(
                &format!("SELECT {DISPATCH_COLS} FROM agent_dispatches WHERE id=?1"),
                params![id],
                map_dispatch,
            )
            .optional()?)
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

    fn dispatch_for_exit(
        &self,
        worktree_path: &str,
        session_id: Option<&str>,
    ) -> Result<Option<(i64, String)>> {
        // Rule 1 — identity. The session id is matched on its own, NOT scoped to
        // the worktree: a daemon session id is unique, and the pane's path can
        // legitimately differ from the recorded one (symlinked / non-canonical
        // checkout), so scoping it would only add a way to miss the right row.
        if let Some(sid) = session_id.filter(|s| !s.is_empty()) {
            let hit = self
                .conn()
                .query_row(
                    "SELECT id, issue_id FROM agent_dispatches WHERE session_id=?1 \
                     ORDER BY dispatched_at_ms DESC, id DESC LIMIT 1",
                    params![sid],
                    |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?)),
                )
                .optional()?;
            if hit.is_some() {
                return Ok(hit);
            }
        }
        // Rule 2 — most recent ACTIVE row for the worktree. The active test runs
        // through the typed status (never a SQL string list), so the closed set
        // has exactly one definition; `Unknown` is neither active nor terminal,
        // so a corrupt row can't steal the stamp either.
        let conn = self.conn();
        let mut stmt = conn.prepare(
            "SELECT id, issue_id, status FROM agent_dispatches WHERE worktree_path=?1 \
             ORDER BY dispatched_at_ms DESC, id DESC",
        )?;
        let mut rows = stmt.query(params![worktree_path])?;
        while let Some(r) = rows.next()? {
            let status = crate::issue::AgentDispatchStatus::parse(&r.get::<_, String>(2)?);
            if status.is_active() {
                return Ok(Some((r.get::<_, i64>(0)?, r.get::<_, String>(1)?)));
            }
        }
        Ok(None)
    }
}

/// The explicit column list every `AgentDispatch` read selects, paired with
/// [`map_dispatch`]. One definition so the list and the row mapper cannot drift
/// apart when the roster gains a column (v56 added four at once).
const DISPATCH_COLS: &str = "id, issue_id, worktree_path, agent_name, dispatched_at_ms, status, \
     stage, parent_id, session_id, artifact_path";

/// Map one [`DISPATCH_COLS`] row. The stored status string is coerced through
/// [`AgentDispatchStatus::parse`](crate::issue::AgentDispatchStatus::parse), so
/// a legacy or corrupt value reads as `Unknown` instead of failing the row.
fn map_dispatch(r: &rusqlite::Row<'_>) -> rusqlite::Result<crate::issue::AgentDispatch> {
    Ok(crate::issue::AgentDispatch {
        id: r.get(0)?,
        issue_id: r.get(1)?,
        worktree_path: r.get(2)?,
        agent_name: r.get(3)?,
        dispatched_at_ms: r.get(4)?,
        status: crate::issue::AgentDispatchStatus::parse(&r.get::<_, String>(5)?),
        stage: r.get(6)?,
        parent_id: r.get(7)?,
        session_id: r.get(8)?,
        artifact_path: r.get(9)?,
    })
}
