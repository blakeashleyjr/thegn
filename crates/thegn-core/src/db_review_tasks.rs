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
     task_kind, source_key, source_revision, content_revision, prompt, expected_head_oid, \
     pending_source_revision, pending_content_revision, pending_prompt, \
     pending_expected_head_oid, pending_role, pending_worktree_path, \
     forge_action_attempts, next_forge_action_at_ms";

impl Db {
    /// Insert or revise one durable task, admitting new active rows only below
    /// [`Db::ROSTER_ACTIVE_LIMIT`]; revising an active row is always allowed.
    pub fn upsert_review_task(&self, task: &ReviewTaskUpsert) -> Result<i64> {
        let base = self.conn();
        let tx = if base.is_autocommit() {
            Some(rusqlite::Transaction::new_unchecked(
                base,
                rusqlite::TransactionBehavior::Immediate,
            )?)
        } else {
            None
        };
        let conn: &rusqlite::Connection = tx.as_deref().unwrap_or(base);
        let active_statuses = [
            AgentDispatchStatus::Queued,
            AgentDispatchStatus::Spawning,
            AgentDispatchStatus::Running,
            AgentDispatchStatus::WaitingHuman,
            AgentDispatchStatus::PrOpen,
        ];
        let existing_status: Option<String> = conn
            .query_row(
                "SELECT status FROM agent_dispatches WHERE task_kind=?1 AND source_key=?2",
                params![REVIEW_TASK_KIND, task.source_key],
                |row| row.get(0),
            )
            .optional()?;
        let existing_is_active = existing_status
            .as_ref()
            .is_some_and(|stored| active_statuses.contains(&AgentDispatchStatus::parse(stored)));
        let task_is_active = active_statuses.contains(&task.status);
        if task_is_active && !existing_is_active {
            let active: i64 = conn.query_row(
                "SELECT COUNT(*) FROM agent_dispatches WHERE status IN (?1, ?2, ?3, ?4, ?5)",
                params![
                    AgentDispatchStatus::Queued.as_str(),
                    AgentDispatchStatus::Spawning.as_str(),
                    AgentDispatchStatus::Running.as_str(),
                    AgentDispatchStatus::WaitingHuman.as_str(),
                    AgentDispatchStatus::PrOpen.as_str(),
                ],
                |row| row.get(0),
            )?;
            let active = usize::try_from(active)?;
            if active >= Db::ROSTER_ACTIVE_LIMIT {
                return Err(anyhow::Error::new(crate::db::RosterActiveCeiling {
                    active,
                    limit: Db::ROSTER_ACTIVE_LIMIT,
                }));
            }
        }
        let id = conn.query_row(
            r#"INSERT INTO agent_dispatches
                 (issue_id, worktree_path, agent_name, dispatched_at_ms, status,
                 task_kind, source_key, source_revision, content_revision, prompt,
                  expected_head_oid, pending_source_revision, pending_content_revision,
                  pending_prompt, pending_expected_head_oid, pending_role,
                  pending_worktree_path, forge_action_attempts,
                  next_forge_action_at_ms, dispatch_kind)
               VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,NULL,NULL,NULL,NULL,NULL,NULL,0,NULL,'plain')
               ON CONFLICT(task_kind, source_key)
                 WHERE task_kind IS NOT NULL AND source_key IS NOT NULL
               DO UPDATE SET
                 issue_id=excluded.issue_id,
                 worktree_path=CASE WHEN agent_dispatches.status IN ('spawning','running')
                                    THEN agent_dispatches.worktree_path ELSE excluded.worktree_path END,
                 agent_name=CASE WHEN agent_dispatches.status IN ('spawning','running')
                                 THEN agent_dispatches.agent_name ELSE excluded.agent_name END,
                 status=CASE WHEN agent_dispatches.status IN ('spawning','running')
                             THEN agent_dispatches.status ELSE excluded.status END,
                 source_revision=CASE WHEN agent_dispatches.status IN ('spawning','running')
                                      THEN agent_dispatches.source_revision ELSE excluded.source_revision END,
                 content_revision=CASE WHEN agent_dispatches.status IN ('spawning','running')
                                       THEN agent_dispatches.content_revision ELSE excluded.content_revision END,
                 prompt=CASE WHEN agent_dispatches.status IN ('spawning','running')
                             THEN agent_dispatches.prompt ELSE excluded.prompt END,
                 expected_head_oid=CASE WHEN agent_dispatches.status IN ('spawning','running')
                                        THEN agent_dispatches.expected_head_oid ELSE excluded.expected_head_oid END,
                 pending_source_revision=CASE WHEN agent_dispatches.status IN ('spawning','running')
                                              THEN excluded.source_revision ELSE NULL END,
                 pending_content_revision=CASE WHEN agent_dispatches.status IN ('spawning','running')
                                               THEN excluded.content_revision ELSE NULL END,
                 pending_prompt=CASE WHEN agent_dispatches.status IN ('spawning','running')
                                     THEN excluded.prompt ELSE NULL END,
                 pending_expected_head_oid=CASE WHEN agent_dispatches.status IN ('spawning','running')
                                                THEN excluded.expected_head_oid ELSE NULL END,
                 pending_role=CASE WHEN agent_dispatches.status IN ('spawning','running')
                                   THEN excluded.agent_name ELSE NULL END,
                 pending_worktree_path=CASE WHEN agent_dispatches.status IN ('spawning','running')
                                            THEN excluded.worktree_path ELSE NULL END,
                 forge_action_attempts=CASE WHEN agent_dispatches.status IN ('spawning','running')
                                            THEN agent_dispatches.forge_action_attempts ELSE 0 END,
                 next_forge_action_at_ms=CASE WHEN agent_dispatches.status IN ('spawning','running')
                                              THEN agent_dispatches.next_forge_action_at_ms ELSE NULL END
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
                task.content_revision,
                task.prompt,
                task.expected_head_oid,
            ],
            |row| row.get(0),
        )?;
        if let Some(tx) = tx {
            tx.commit()?;
        }
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

    /// Apply a pure resolved transition, scoped by id, canonical source key,
    /// and the active revision. A refresh may write pending feedback while a
    /// handoff is doing its provider call; reject that transition unless the
    /// pending snapshot is only the same-content/head-only refresh that the
    /// handoff is expected to verify.
    pub fn resolve_review_task(&self, transition: &ReviewTaskResolution) -> Result<bool> {
        let changed = self.conn().execute(
            "UPDATE agent_dispatches SET status=?1, forge_action_attempts=0, \
             next_forge_action_at_ms=NULL, pending_source_revision=NULL, \
             pending_content_revision=NULL, pending_prompt=NULL, \
             pending_expected_head_oid=NULL, pending_role=NULL, \
             pending_worktree_path=NULL \
             WHERE id=?2 AND task_kind=?3 AND source_key=?4 AND status<>?1 \
               AND source_revision=?5 \
               AND (status NOT IN ('spawning','running') OR (\
                    (pending_source_revision IS NULL \
                        OR pending_content_revision=content_revision)))",
            params![
                AgentDispatchStatus::Done.as_str(),
                transition.dispatch_id,
                REVIEW_TASK_KIND,
                transition.source_key,
                transition.source_revision,
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

    /// Claim the exact queued revision selected by the user. A refresh may
    /// revise the row between panel hydration and the handle worker starting;
    /// in that case the stale prompt must not be launched.
    pub fn claim_review_task(&self, id: i64, source_revision: &str) -> Result<bool> {
        let changed = self.conn().execute(
            "UPDATE agent_dispatches SET status=?1 \
             WHERE id=?2 AND task_kind=?3 AND source_key IS NOT NULL \
               AND status=?4 AND source_revision=?5",
            params![
                AgentDispatchStatus::Running.as_str(),
                id,
                REVIEW_TASK_KIND,
                AgentDispatchStatus::Queued.as_str(),
                source_revision,
            ],
        )?;
        Ok(changed > 0)
    }

    /// Promote the newest snapshot retained while an active handoff was
    /// running. The conditional update makes this safe against a concurrent
    /// refresh and preserves the one-row `(task_kind, source_key)` identity.
    pub fn promote_review_task_pending(&self, id: i64) -> Result<bool> {
        let changed = self.conn().execute(
            "UPDATE agent_dispatches SET
                 worktree_path=COALESCE(pending_worktree_path, worktree_path),
                 agent_name=COALESCE(pending_role, agent_name),
                 source_revision=pending_source_revision,
                 content_revision=pending_content_revision,
                 prompt=pending_prompt,
                 expected_head_oid=pending_expected_head_oid,
                 status='queued', forge_action_attempts=0,
                 next_forge_action_at_ms=NULL,
                 pending_source_revision=NULL, pending_content_revision=NULL,
                 pending_prompt=NULL, pending_expected_head_oid=NULL,
                 pending_role=NULL, pending_worktree_path=NULL
             WHERE id=?1 AND task_kind=?2 AND source_key IS NOT NULL
               AND status IN ('spawning','running')
               AND pending_source_revision IS NOT NULL",
            params![id, REVIEW_TASK_KIND],
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
    let attempts = row.get::<_, i64>(18)?;
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
        content_revision: row.get::<_, Option<String>>(9)?.unwrap_or_default(),
        prompt: row.get(10)?,
        expected_head_oid: row.get(11)?,
        pending_source_revision: row.get(12)?,
        pending_content_revision: row.get(13)?,
        pending_prompt: row.get(14)?,
        pending_expected_head_oid: row.get(15)?,
        pending_role: row.get(16)?,
        pending_worktree_path: row.get(17)?,
        forge_action_attempts: u32::try_from(attempts).unwrap_or(u32::MAX),
        next_forge_action_at_ms: row.get(19)?,
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
            content_revision: "content".into(),
            prompt: prompt.into(),
            expected_head_oid: "head".into(),
            event: ReviewTaskEvent {
                event: crate::pr_review_tasks::REVIEW_THREAD_EVENT.into(),
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
    fn active_upsert_freezes_inputs_and_retains_one_pending_revision() {
        let db = Db::open_memory().unwrap();
        let first = task("r1", "first");
        let id = db.upsert_review_task(&first).unwrap();
        db.update_review_task_status(id, AgentDispatchStatus::Running)
            .unwrap();
        db.record_review_forge_attempt(id, Some(99), AgentDispatchStatus::Running)
            .unwrap();

        let mut revised = task("r2", "second");
        revised.content_revision = "content-2".into();
        revised.expected_head_oid = "head-2".into();
        revised.role = "new-role".into();
        db.upsert_review_task(&revised).unwrap();
        let active = db.get_review_task(id).unwrap().unwrap();
        assert_eq!(active.source_revision, "r1");
        assert_eq!(active.prompt, "first");
        assert_eq!(active.expected_head_oid, "head");
        assert_eq!(active.role, "coder");
        assert_eq!(active.pending_source_revision.as_deref(), Some("r2"));
        assert_eq!(active.pending_prompt.as_deref(), Some("second"));
        assert_eq!(active.pending_role.as_deref(), Some("new-role"));
        assert_eq!(active.next_forge_action_at_ms, Some(99));

        assert!(db.promote_review_task_pending(id).unwrap());
        let queued = db.get_review_task(id).unwrap().unwrap();
        assert_eq!(queued.status, AgentDispatchStatus::Queued);
        assert_eq!(queued.source_revision, "r2");
        assert_eq!(queued.prompt, "second");
        assert_eq!(queued.expected_head_oid, "head-2");
        assert_eq!(queued.role, "new-role");
        assert!(queued.pending_source_revision.is_none());
        assert_eq!(queued.next_forge_action_at_ms, None);
    }

    #[test]
    fn claim_review_task_requires_the_selected_queued_revision() {
        let db = Db::open_memory().unwrap();
        let id = db.upsert_review_task(&task("r1", "first")).unwrap();
        assert!(!db.claim_review_task(id, "r2").unwrap());
        assert_eq!(
            db.get_review_task(id).unwrap().unwrap().status,
            AgentDispatchStatus::Queued
        );
        assert!(db.claim_review_task(id, "r1").unwrap());
        assert!(!db.claim_review_task(id, "r1").unwrap());
    }

    #[test]
    fn active_resolution_rejects_new_feedback_but_accepts_head_only_refresh() {
        let db = Db::open_memory().unwrap();
        let first = task("r1", "first");
        let id = db.upsert_review_task(&first).unwrap();
        db.update_review_task_status(id, AgentDispatchStatus::Running)
            .unwrap();

        let mut revised = task("r2", "new feedback");
        revised.content_revision = "content-2".into();
        db.upsert_review_task(&revised).unwrap();
        let row = db.get_review_task(id).unwrap().unwrap();
        let transition = ReviewTaskResolution {
            dispatch_id: id,
            source_key: row.source_key.clone(),
            source_revision: row.source_revision.clone(),
            forge: "github".into(),
            repository: "acme/widget".into(),
            pr_number: 22,
            thread_id: "thread".into(),
            path: "src/lib.rs".into(),
            line: Some(1),
            head_oid: "head".into(),
            worktree_path: "/wt/review".into(),
        };
        assert!(!db.resolve_review_task(&transition).unwrap());
        assert_eq!(
            db.get_review_task(id).unwrap().unwrap().status,
            AgentDispatchStatus::Running
        );

        let db = Db::open_memory().unwrap();
        let first = task("r1", "first");
        let id = db.upsert_review_task(&first).unwrap();
        db.update_review_task_status(id, AgentDispatchStatus::Running)
            .unwrap();
        let mut head_only = task("r2", "first");
        head_only.content_revision = first.content_revision;
        db.upsert_review_task(&head_only).unwrap();
        let row = db.get_review_task(id).unwrap().unwrap();
        let transition = ReviewTaskResolution {
            dispatch_id: id,
            source_key: row.source_key,
            source_revision: "r1".into(),
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

    fn seed_active(db: &Db, count: usize) {
        db.conn()
            .execute(
                "WITH RECURSIVE n(i) AS (SELECT 1 UNION ALL SELECT i + 1 FROM n WHERE i < ?1) \
                 INSERT INTO agent_dispatches \
                 (issue_id, worktree_path, agent_name, dispatched_at_ms, status) \
                 SELECT 'linear:CAP', '/wt/cap', 'a', i, 'queued' FROM n",
                params![count as i64],
            )
            .unwrap();
    }

    #[test]
    fn review_task_roster_ceiling_refuses_a_new_task() {
        let db = Db::open_memory().unwrap();
        seed_active(&db, Db::ROSTER_ACTIVE_LIMIT);

        let error = db.upsert_review_task(&task("r1", "p")).unwrap_err();
        assert_eq!(
            error.downcast_ref::<crate::db::RosterActiveCeiling>(),
            Some(&crate::db::RosterActiveCeiling {
                active: Db::ROSTER_ACTIVE_LIMIT,
                limit: Db::ROSTER_ACTIVE_LIMIT,
            })
        );
        assert!(db.list_review_tasks().unwrap().is_empty());
    }

    #[test]
    fn review_task_roster_ceiling_refuses_reviving_a_terminal_task() {
        let db = Db::open_memory().unwrap();
        let id = db.upsert_review_task(&task("r1", "first")).unwrap();
        db.update_review_task_status(id, AgentDispatchStatus::Done)
            .unwrap();
        seed_active(&db, Db::ROSTER_ACTIVE_LIMIT);

        let error = db.upsert_review_task(&task("r2", "second")).unwrap_err();
        assert_eq!(
            error.downcast_ref::<crate::db::RosterActiveCeiling>(),
            Some(&crate::db::RosterActiveCeiling {
                active: Db::ROSTER_ACTIVE_LIMIT,
                limit: Db::ROSTER_ACTIVE_LIMIT,
            })
        );
        let row = db.get_review_task(id).unwrap().unwrap();
        assert_eq!(row.status, AgentDispatchStatus::Done);
        assert_eq!(row.source_revision, "r1");
    }

    #[test]
    fn review_task_roster_ceiling_still_revises_an_active_task() {
        let db = Db::open_memory().unwrap();
        let id = db.upsert_review_task(&task("r1", "first")).unwrap();
        seed_active(&db, Db::ROSTER_ACTIVE_LIMIT - 1);

        let revised = db.upsert_review_task(&task("r2", "second")).unwrap();
        assert_eq!(revised, id);
        assert_eq!(
            db.get_review_task(id).unwrap().unwrap().source_revision,
            "r2"
        );
    }

    #[test]
    fn review_task_roster_ceiling_admits_below_the_limit() {
        let db = Db::open_memory().unwrap();
        seed_active(&db, Db::ROSTER_ACTIVE_LIMIT - 1);

        assert!(db.upsert_review_task(&task("r1", "p")).is_ok());
    }

    #[test]
    fn review_task_upsert_is_a_plain_dispatch_kind() {
        let db = Db::open_memory().unwrap();
        let id = db.upsert_review_task(&task("r1", "p")).unwrap();
        assert_eq!(db.dispatch_kind(id).unwrap().as_deref(), Some("plain"));
        let again = db.upsert_review_task(&task("r2", "p2")).unwrap();
        assert_eq!(again, id);
        assert_eq!(db.dispatch_kind(id).unwrap().as_deref(), Some("plain"));
    }
}
