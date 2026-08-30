//! Durable review-task CRUD on the shared `agent_dispatches` roster.
//!
//! Review columns are nullable so ordinary issue/pipeline dispatches keep
//! their existing shape and behavior. The partial unique index installed by
//! schema v64 makes the upsert atomic on `(task_kind, source_key)`.

use crate::db::Db;
use crate::issue::{AgentDispatchStatus, ReviewTaskRecord};
use crate::pr_review_tasks::{REVIEW_TASK_KIND, ReviewTaskResolution, ReviewTaskUpsert};
use crate::util;
use anyhow::Result;
use rusqlite::{OptionalExtension, params};

const REVIEW_TASK_COLS: &str = "id, issue_id, worktree_path, agent_name, dispatched_at_ms, status, \
     task_kind, source_key, source_revision, prompt, expected_head_oid, \
     forge_action_attempts, next_forge_action_at_ms";

impl Db {
    /// Insert or revise exactly one durable task. The partial unique index is
    /// the concurrency guard; a second reconciler updates the same row rather
    /// than creating a concurrent duplicate.
    pub fn upsert_review_task(&self, task: &ReviewTaskUpsert) -> Result<i64> {
        let id = self.conn().query_row(
            r#"INSERT INTO agent_dispatches
                 (issue_id, worktree_path, agent_name, dispatched_at_ms, status,
                  task_kind, source_key, source_revision, prompt,
                  expected_head_oid, forge_action_attempts,
                  next_forge_action_at_ms)
               VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,0,NULL)
               ON CONFLICT(task_kind, source_key)
                 WHERE task_kind IS NOT NULL AND source_key IS NOT NULL
               DO UPDATE SET
                 issue_id=excluded.issue_id,
                 worktree_path=excluded.worktree_path,
                 agent_name=excluded.agent_name,
                 status=excluded.status,
                 source_revision=excluded.source_revision,
                 prompt=excluded.prompt,
                 expected_head_oid=excluded.expected_head_oid,
                 forge_action_attempts=0,
                 next_forge_action_at_ms=NULL
               RETURNING id"#,
            params![
                task.issue_id,
                task.worktree_path,
                task.role,
                util::now_ms(),
                task.status.as_str(),
                REVIEW_TASK_KIND,
                task.source_key,
                task.source_revision,
                task.prompt,
                task.expected_head_oid,
            ],
            |row| row.get(0),
        )?;
        Ok(id)
    }

    pub fn list_review_tasks(&self) -> Result<Vec<ReviewTaskRecord>> {
        let mut statement = self.conn().prepare(&format!(
            "SELECT {REVIEW_TASK_COLS} FROM agent_dispatches \
             WHERE task_kind=?1 AND source_key IS NOT NULL \
             ORDER BY dispatched_at_ms DESC, id DESC"
        ))?;
        Ok(statement
            .query_map(params![REVIEW_TASK_KIND], map_review_task)?
            .collect::<rusqlite::Result<_>>()?)
    }

    pub fn get_review_task(&self, id: i64) -> Result<Option<ReviewTaskRecord>> {
        Ok(self
            .conn()
            .query_row(
                &format!(
                    "SELECT {REVIEW_TASK_COLS} FROM agent_dispatches \
                     WHERE id=?1 AND task_kind=?2 AND source_key IS NOT NULL"
                ),
                params![id, REVIEW_TASK_KIND],
                map_review_task,
            )
            .optional()?)
    }

    pub fn review_task_by_source(&self, source_key: &str) -> Result<Option<ReviewTaskRecord>> {
        Ok(self
            .conn()
            .query_row(
                &format!(
                    "SELECT {REVIEW_TASK_COLS} FROM agent_dispatches \
                     WHERE task_kind=?1 AND source_key=?2"
                ),
                params![REVIEW_TASK_KIND, source_key],
                map_review_task,
            )
            .optional()?)
    }

    /// Apply a pure resolved transition, scoped by both id and canonical
    /// source key so a stale plan cannot close a reused/corrupt row.
    pub fn resolve_review_task(&self, transition: &ReviewTaskResolution) -> Result<bool> {
        let changed = self.conn().execute(
            "UPDATE agent_dispatches SET status=?1, forge_action_attempts=0, \
             next_forge_action_at_ms=NULL \
             WHERE id=?2 AND task_kind=?3 AND source_key=?4 AND status<>?1",
            params![
                AgentDispatchStatus::Done.as_str(),
                transition.dispatch_id,
                REVIEW_TASK_KIND,
                transition.source_key,
            ],
        )?;
        Ok(changed > 0)
    }

    pub fn update_review_task_status(&self, id: i64, status: AgentDispatchStatus) -> Result<bool> {
        let changed = self.conn().execute(
            "UPDATE agent_dispatches SET status=?1 \
             WHERE id=?2 AND task_kind=?3 AND source_key IS NOT NULL",
            params![status.as_str(), id, REVIEW_TASK_KIND],
        )?;
        Ok(changed > 0)
    }

    /// Record one failed/limited provider action and its durable retry time.
    /// The caller selects the safe lifecycle state (normally `WaitingHuman`).
    pub fn record_review_forge_attempt(
        &self,
        id: i64,
        next_action_at_ms: Option<i64>,
        status: AgentDispatchStatus,
    ) -> Result<bool> {
        let changed = self.conn().execute(
            "UPDATE agent_dispatches \
             SET forge_action_attempts=forge_action_attempts+1, \
                 next_forge_action_at_ms=?1, status=?2 \
             WHERE id=?3 AND task_kind=?4 AND source_key IS NOT NULL",
            params![next_action_at_ms, status.as_str(), id, REVIEW_TASK_KIND],
        )?;
        Ok(changed > 0)
    }
}

fn map_review_task(row: &rusqlite::Row<'_>) -> rusqlite::Result<ReviewTaskRecord> {
    let attempts = row.get::<_, i64>(11)?;
    Ok(ReviewTaskRecord {
        id: row.get(0)?,
        issue_id: row.get(1)?,
        worktree_path: row.get(2)?,
        role: row.get(3)?,
        dispatched_at_ms: crate::issue::normalize_dispatch_ms(row.get(4)?),
        status: AgentDispatchStatus::parse(&row.get::<_, String>(5)?),
        task_kind: row.get(6)?,
        source_key: row.get(7)?,
        source_revision: row.get(8)?,
        prompt: row.get(9)?,
        expected_head_oid: row.get(10)?,
        forge_action_attempts: u32::try_from(attempts).unwrap_or(u32::MAX),
        next_forge_action_at_ms: row.get(12)?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pr_review_tasks::{ReviewTaskEvent, ReviewTaskUpsert};
    use crate::store::NotificationStore;

    fn task(revision: &str, prompt: &str) -> ReviewTaskUpsert {
        ReviewTaskUpsert {
            existing_id: None,
            issue_id: "pr:github:acme/widget#22".into(),
            worktree_path: "/wt/review".into(),
            role: "coder".into(),
            status: AgentDispatchStatus::Queued,
            source_key: "review_thread:sha256:abc".into(),
            source_revision: revision.into(),
            prompt: prompt.into(),
            expected_head_oid: "head".into(),
            event: ReviewTaskEvent {
                event: crate::pr_review_tasks::REVIEW_THREAD_EVENT,
                source_key: "review_thread:sha256:abc".into(),
                source_revision: revision.into(),
                forge: "github".into(),
                repository: "acme/widget".into(),
                pr_number: 22,
                pr_url: String::new(),
                pr_title: String::new(),
                branch: "feature".into(),
                base: "main".into(),
                head_oid: "head".into(),
                thread_id: "thread".into(),
                path: "src/lib.rs".into(),
                line: Some(1),
                role: "coder".into(),
                prompt: prompt.into(),
                worktree_path: "/wt/review".into(),
            },
        }
    }

    #[test]
    fn pr_review_tasks_db_upsert_revises_one_row_and_maps_fields() {
        let db = Db::open_memory().unwrap();
        let id = db.upsert_review_task(&task("r1", "first")).unwrap();
        let revised = db.upsert_review_task(&task("r2", "second")).unwrap();
        assert_eq!(id, revised);
        let rows = db.list_review_tasks().unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].source_revision, "r2");
        assert_eq!(rows[0].prompt, "second");
        assert_eq!(rows[0].forge_action_attempts, 0);
        assert_eq!(
            db.review_task_by_source(&rows[0].source_key).unwrap(),
            Some(rows[0].clone())
        );
        assert_eq!(db.get_review_task(id).unwrap(), Some(rows[0].clone()));
    }

    #[test]
    fn pr_review_tasks_db_keeps_pipeline_rows_null_and_tracks_attempts() {
        let db = Db::open_memory().unwrap();
        db.put_agent_dispatch(crate::issue::NewDispatch::new("THE-1", "/wt/p", "coder"))
            .unwrap();
        assert!(db.list_review_tasks().unwrap().is_empty());

        let id = db.upsert_review_task(&task("r1", "prompt")).unwrap();
        assert!(
            db.record_review_forge_attempt(id, Some(1234), AgentDispatchStatus::WaitingHuman)
                .unwrap()
        );
        let row = db.get_review_task(id).unwrap().unwrap();
        assert_eq!(row.forge_action_attempts, 1);
        assert_eq!(row.next_forge_action_at_ms, Some(1234));
        assert_eq!(row.status, AgentDispatchStatus::WaitingHuman);
        assert!(
            db.update_review_task_status(id, AgentDispatchStatus::Running)
                .unwrap()
        );

        let transition = ReviewTaskResolution {
            dispatch_id: id,
            source_key: row.source_key,
            source_revision: row.source_revision,
            forge: "github".into(),
            repository: "acme/widget".into(),
            pr_number: 22,
            thread_id: "thread".into(),
            path: "src/lib.rs".into(),
            line: Some(1),
            head_oid: "head".into(),
            worktree_path: "/wt/review".into(),
        };
        assert!(db.resolve_review_task(&transition).unwrap());
        assert_eq!(
            db.get_review_task(id).unwrap().unwrap().status,
            AgentDispatchStatus::Done
        );
    }

    #[test]
    fn pr_review_tasks_notifications_are_bounded_and_once_keyed() {
        let db = Db::open_memory().unwrap();
        let mut event = task("r1", "prompt").event;
        event.path = format!("{}\u{1b}[2J", "x".repeat(2048));
        assert!(
            db.put_review_task_queued_notification(&event, false)
                .unwrap()
        );
        assert!(
            !db.put_review_task_queued_notification(&event, false)
                .unwrap()
        );
        event.source_revision = "r2".into();
        assert!(
            db.put_review_task_queued_notification(&event, true)
                .unwrap()
        );

        let rows = db.get_all_notifications(10).unwrap();
        assert_eq!(rows.len(), 2);
        assert!(rows.iter().all(|row| {
            row.message.chars().count() <= crate::notification::MAX_REVIEW_NOTIFICATION_CHARS
                && !row.message.chars().any(char::is_control)
        }));
    }
}
