//! Dispatch-report + per-row progress queue — sibling `impl Db` block so the
//! pinned `db.rs` only carries the schema DDL, not these bodies. The DB is a
//! cache; git / the live source is truth.

use crate::db::Db;
use crate::issue::{AgentDispatchStatus, DispatchNote, DispatchRunPublishOutcome};
use crate::store::NotificationStore;
use crate::util;
use anyhow::Result;
use rusqlite::OptionalExtension as _;

/// The source row's verdict changed after the resume caller read it but
/// before the atomic claim could reconcile it. This is a contention result,
/// rather than a database failure: the newer verdict must be preserved and a
/// caller may retry from a fresh roster read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResumeDispatchConflict {
    /// The write transaction observed a different status than the caller's
    /// expected source verdict.
    SourceStatusChanged {
        source_id: i64,
        expected: AgentDispatchStatus,
        actual: AgentDispatchStatus,
    },
    /// The guarded source update lost its compare-and-set race. The winner's
    /// exact verdict is deliberately read by the next retry.
    SourceUpdateLost {
        source_id: i64,
        expected: AgentDispatchStatus,
    },
}

impl std::fmt::Display for ResumeDispatchConflict {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::SourceStatusChanged {
                source_id,
                expected,
                actual,
            } => write!(
                f,
                "dispatch {source_id} changed from {} to {} while --resume-work was preparing it; the newer verdict was preserved",
                expected.as_str(),
                actual.as_str()
            ),
            Self::SourceUpdateLost { source_id, .. } => write!(
                f,
                "dispatch {source_id} changed while --resume-work was preparing it; the newer verdict was preserved"
            ),
        }
    }
}

impl std::error::Error for ResumeDispatchConflict {}

impl Db {
    /// Atomically publish the server-generated identity of an opened worker and
    /// move its reserved row to `running`. Only `queued`/`spawning` are
    /// admissible: a concurrent terminal or parked verdict wins and is never
    /// resurrected. Every refusal is returned as a typed outcome so the caller
    /// can tear down the process it already opened.
    pub fn publish_dispatch_run(
        &self,
        id: i64,
        session_id: &str,
        artifact_path: &str,
    ) -> Result<DispatchRunPublishOutcome> {
        let conn = self.conn();
        let tx = conn.unchecked_transaction()?;
        let changed = tx.execute(
            "UPDATE agent_dispatches SET session_id=?1, artifact_path=?2, status=?3 \
             WHERE id=?4 AND status IN (?5, ?6)",
            rusqlite::params![
                session_id,
                artifact_path,
                AgentDispatchStatus::Running.as_str(),
                id,
                AgentDispatchStatus::Queued.as_str(),
                AgentDispatchStatus::Spawning.as_str(),
            ],
        )?;
        let outcome = if changed != 0 {
            DispatchRunPublishOutcome::Published
        } else {
            let stored = tx
                .query_row(
                    "SELECT status FROM agent_dispatches WHERE id=?1",
                    [id],
                    |row| row.get::<_, String>(0),
                )
                .optional()?;
            match stored {
                Some(status) => DispatchRunPublishOutcome::StateChanged {
                    status: AgentDispatchStatus::parse(&status),
                },
                None => DispatchRunPublishOutcome::Missing,
            }
        };
        tx.commit()?;
        Ok(outcome)
    }

    /// Atomically update a row's status and append its human-readable reason.
    /// Neither write is allowed to survive without the other.
    pub fn update_dispatch_status_with_note(
        &self,
        id: i64,
        status: AgentDispatchStatus,
        note: &str,
    ) -> Result<()> {
        let note = crate::pipeline_report::note_text(note).map_err(|e| anyhow::anyhow!("{e}"))?;
        let conn = self.conn();
        let tx = conn.unchecked_transaction()?;
        let changed = tx.execute(
            "UPDATE agent_dispatches SET status=?1 WHERE id=?2",
            rusqlite::params![status.as_str(), id],
        )?;
        if changed == 0 {
            anyhow::bail!("roster row {id} does not exist");
        }
        tx.execute(
            "INSERT INTO agent_dispatch_notes (dispatch_id, created_at_ms, text) VALUES (?1, ?2, ?3)",
            rusqlite::params![id, util::now_ms(), note],
        )?;
        tx.commit()?;
        Ok(())
    }

    /// Apply a planned status transition only if the row still has the status
    /// observed by the planner. An optional note commits in the same
    /// transaction. `false` means a concurrent writer won and is preserved.
    pub fn compare_and_set_dispatch_status(
        &self,
        id: i64,
        expected: AgentDispatchStatus,
        status: AgentDispatchStatus,
        note: Option<&str>,
    ) -> Result<bool> {
        let note = note
            .map(crate::pipeline_report::note_text)
            .transpose()
            .map_err(|e| anyhow::anyhow!("{e}"))?;
        let conn = self.conn();
        let tx = conn.unchecked_transaction()?;
        let changed = tx.execute(
            "UPDATE agent_dispatches SET status=?1 WHERE id=?2 AND status=?3",
            rusqlite::params![status.as_str(), id, expected.as_str()],
        )?;
        if changed == 0 {
            tx.rollback()?;
            return Ok(false);
        }
        if let Some(note) = note {
            tx.execute(
                "INSERT INTO agent_dispatch_notes (dispatch_id, created_at_ms, text) VALUES (?1, ?2, ?3)",
                rusqlite::params![id, util::now_ms(), note],
            )?;
        }
        tx.commit()?;
        Ok(true)
    }

    /// Park a transport retry only if the row is still in the state observed
    /// by the exit handler. Status and the retry ledger move together: a
    /// supervisor verdict that wins the race is never overwritten.
    pub fn compare_and_set_dispatch_retry_park(
        &self,
        id: i64,
        expected: AgentDispatchStatus,
        note: &str,
    ) -> Result<bool> {
        let note = crate::pipeline_report::note_text(note).map_err(|e| anyhow::anyhow!("{e}"))?;
        Ok(self.conn().execute(
            "UPDATE agent_dispatches SET status=?1, note=?2 WHERE id=?3 AND status=?4",
            rusqlite::params![
                AgentDispatchStatus::WaitingHuman.as_str(),
                note,
                id,
                expected.as_str()
            ],
        )? != 0)
    }

    /// Publish a relaunched worker only while the retry reservation is still
    /// ours. A supervisor may close the `spawning` row while `open` awaits;
    /// in that case this returns false and the caller kills the orphan launch.
    pub fn compare_and_set_dispatch_retry_run(
        &self,
        id: i64,
        expected: AgentDispatchStatus,
        session_id: &str,
        artifact_path: &str,
    ) -> Result<bool> {
        Ok(self.conn().execute(
            "UPDATE agent_dispatches SET status=?1, session_id=?2, artifact_path=?3, \
             exit_code=NULL, exited_at_ms=NULL \
             WHERE id=?4 AND status=?5",
            rusqlite::params![
                AgentDispatchStatus::Running.as_str(),
                session_id,
                artifact_path,
                id,
                expected.as_str()
            ],
        )? != 0)
    }

    /// Store the worker's structured handoff report on a roster row — UPDATE the
    /// nullable `report` column. Errors when the row does not exist (checks
    /// `get_dispatch` first, naming the id).
    pub fn set_dispatch_report(&self, id: i64, text: &str) -> Result<()> {
        let text = crate::pipeline_report::report_text(text).map_err(|e| anyhow::anyhow!("{e}"))?;
        // Existence check: a silent UPDATE on a missing row is silently wrong.
        if self.get_dispatch(id)?.is_none() {
            anyhow::bail!("roster row {id} does not exist");
        }
        self.conn().execute(
            "UPDATE agent_dispatches SET report=?1 WHERE id=?2",
            rusqlite::params![text, id],
        )?;
        Ok(())
    }

    /// Take (or renew) the named pipeline lease for `owner`, for `ttl_secs`.
    ///
    /// Returns `Ok(())` when this owner now holds it, or `Err(current_owner)`
    /// when someone else does and their claim has not expired. Renewal by the
    /// same owner always succeeds, so a live monitor keeps its own lease by
    /// heartbeating.
    ///
    /// One `INSERT … ON CONFLICT DO UPDATE … WHERE` statement, so acquisition is
    /// atomic: the `WHERE` decides, inside the write lock, whether the existing
    /// row may be taken over. Two monitors racing therefore cannot both win.
    pub fn acquire_pipeline_lease(
        &self,
        name: &str,
        owner: &str,
        ttl_secs: i64,
    ) -> Result<std::result::Result<(), String>> {
        let now = util::now_ms();
        let expires = now.saturating_add(ttl_secs.saturating_mul(1000));
        let changed = self.conn().execute(
            "INSERT INTO pipeline_leases (name, owner, acquired_at_ms, expires_at_ms)
             VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT(name) DO UPDATE SET
               owner=excluded.owner,
               acquired_at_ms=excluded.acquired_at_ms,
               expires_at_ms=excluded.expires_at_ms
             WHERE pipeline_leases.owner = excluded.owner
                OR pipeline_leases.expires_at_ms <= ?3",
            rusqlite::params![name, owner, now, expires],
        )?;
        if changed > 0 {
            return Ok(Ok(()));
        }
        // The upsert was refused, so someone else holds a live lease.
        let holder: String = self
            .conn()
            .query_row(
                "SELECT owner FROM pipeline_leases WHERE name=?1",
                rusqlite::params![name],
                |r| r.get(0),
            )
            .unwrap_or_else(|_| "<unknown>".into());
        Ok(Err(holder))
    }

    /// Release a lease this owner holds. A no-op when someone else holds it —
    /// releasing another process's lease is never correct.
    pub fn release_pipeline_lease(&self, name: &str, owner: &str) -> Result<bool> {
        let n = self.conn().execute(
            "DELETE FROM pipeline_leases WHERE name=?1 AND owner=?2",
            rusqlite::params![name, owner],
        )?;
        Ok(n > 0)
    }

    /// The current holder of a lease and its remaining life in ms, if it is
    /// live. An expired lease reads as `None` — it is nobody's.
    pub fn pipeline_lease_holder(&self, name: &str) -> Result<Option<(String, i64)>> {
        let now = util::now_ms();
        // `.optional()` (not `.ok()`): "no such lease" is a legitimate answer,
        // but a genuine query failure must propagate rather than read as free.
        let row = self
            .conn()
            .query_row(
                "SELECT owner, expires_at_ms FROM pipeline_leases WHERE name=?1",
                rusqlite::params![name],
                |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)),
            )
            .optional()?;
        Ok(row
            .filter(|(_, exp)| *exp > now)
            .map(|(o, exp)| (o, exp - now)))
    }

    /// Atomically claim a slot and create the row, or refuse with the reason.
    ///
    /// # Why this is one call
    ///
    /// A supervisor doing `dispatch list` → decide → `dispatch put` has a
    /// read-modify-write race: two monitors (or one monitor and its own restart)
    /// both read a free stage and both insert. Running
    /// [`crate::pipeline_claim::decide`] *inside* the write transaction closes
    /// it — SQLite's write lock serializes the check with the insert, so the
    /// second caller re-reads the first caller's row and is refused.
    ///
    /// `allow_duplicate` is the auditable de-duplication override: it records
    /// the operator's non-empty reason as the row's first note. It never
    /// bypasses the stage capacity budget.
    pub fn claim_dispatch(
        &self,
        new: crate::issue::NewDispatch<'_>,
        limit: u32,
        allow_duplicate: Option<&str>,
    ) -> Result<std::result::Result<i64, crate::pipeline_claim::ClaimDecision>> {
        use crate::pipeline_claim::{ClaimRequest, decide_allowing_duplicate};
        let allow_duplicate = allow_duplicate.map(str::trim);
        if allow_duplicate.is_some_and(str::is_empty) {
            anyhow::bail!("--allow-duplicate requires a non-empty reason");
        }
        let req = ClaimRequest {
            issue_id: new.issue_id.to_string(),
            stage: new.stage.unwrap_or_default().to_string(),
            worktree_path: new.worktree_path.to_string(),
            artifact_path: new.artifact_path.map(str::to_string),
            chunk_path: new.chunk_path.map(str::to_string),
        };
        // IMMEDIATE: take the write lock up front so the read below cannot be
        // interleaved with another claimant's insert.
        let conn = self.conn();
        conn.execute_batch("BEGIN IMMEDIATE")?;
        let result =
            (|| -> Result<std::result::Result<i64, crate::pipeline_claim::ClaimDecision>> {
                let rows = self.list_dispatches()?;
                let decision =
                    decide_allowing_duplicate(&rows, &req, limit, allow_duplicate.is_some());
                if !decision.granted() {
                    return Ok(Err(decision));
                }
                let id = self.put_agent_dispatch(new)?;
                // The override's audit trail is written INSIDE the transaction,
                // so an authorized duplicate and the record of who authorized it
                // commit together. A duplicate row with no note would be
                // indistinguishable from a runaway one — exactly the ambiguity
                // this whole change exists to remove — so it must not be
                // best-effort.
                if let Some(why) = allow_duplicate {
                    self.append_dispatch_note(
                        id,
                        &format!("duplicate dispatch explicitly authorized: {why}"),
                    )?;
                }
                Ok(Ok(id))
            })();
        match &result {
            Ok(Ok(_)) => conn.execute_batch("COMMIT")?,
            // Nothing was written on a refusal, but the transaction still has to
            // be released or the next writer blocks on it.
            _ => conn.execute_batch("ROLLBACK")?,
        }
        result
    }

    /// Reconcile a resume source and claim its finisher in one write
    /// transaction. The source status read by the caller is guarded under the
    /// same lock. If admission is refused, the source reconciliation is rolled
    /// back; if another supervisor changed the source, that verdict is
    /// preserved and no finisher is inserted.
    pub fn claim_resume_dispatch(
        &self,
        source_id: i64,
        expected_source_status: AgentDispatchStatus,
        new: crate::issue::NewDispatch<'_>,
        limit: u32,
    ) -> Result<std::result::Result<i64, crate::pipeline_claim::ClaimDecision>> {
        use crate::pipeline_claim::{ClaimRequest, decide};
        let req = ClaimRequest {
            issue_id: new.issue_id.to_string(),
            stage: new.stage.unwrap_or_default().to_string(),
            worktree_path: new.worktree_path.to_string(),
            artifact_path: new.artifact_path.map(str::to_string),
            chunk_path: new.chunk_path.map(str::to_string),
        };
        let conn = self.conn();
        conn.execute_batch("BEGIN IMMEDIATE")?;
        let result =
            (|| -> Result<std::result::Result<i64, crate::pipeline_claim::ClaimDecision>> {
                let stored = conn
                    .query_row(
                        "SELECT status FROM agent_dispatches WHERE id=?1",
                        [source_id],
                        |row| row.get::<_, String>(0),
                    )
                    .optional()?
                    .ok_or_else(|| anyhow::anyhow!("dispatch {source_id} disappeared"))?;
                let current = AgentDispatchStatus::parse(&stored);
                if current != expected_source_status {
                    return Err(anyhow::Error::new(
                        ResumeDispatchConflict::SourceStatusChanged {
                            source_id,
                            expected: expected_source_status,
                            actual: current,
                        },
                    ));
                }
                if current.is_active() {
                    let changed = conn.execute(
                        "UPDATE agent_dispatches SET status=?1 WHERE id=?2 AND status=?3",
                        rusqlite::params![
                            AgentDispatchStatus::Failed.as_str(),
                            source_id,
                            current.as_str()
                        ],
                    )?;
                    if changed == 0 {
                        return Err(anyhow::Error::new(
                            ResumeDispatchConflict::SourceUpdateLost {
                                source_id,
                                expected: expected_source_status,
                            },
                        ));
                    }
                    self.append_dispatch_note(
                        source_id,
                        "superseded by a --resume-work finisher dispatch",
                    )?;
                }

                let rows = self.list_dispatches()?;
                let decision = decide(&rows, &req, limit);
                if !decision.granted() {
                    return Ok(Err(decision));
                }
                Ok(Ok(self.put_agent_dispatch(new)?))
            })();
        match &result {
            Ok(Ok(_)) => conn.execute_batch("COMMIT")?,
            // This rollback is material on policy refusal: it restores an
            // active source rather than consuming it without a finisher.
            _ => conn.execute_batch("ROLLBACK")?,
        }
        result
    }

    /// Stamp a row's worker exit (v63): the exit code, if it was reaped, and
    /// when. First writer wins for one run, so the daemon event observer and the
    /// adopted-pane drain can safely see the same exit without moving its time
    /// or changing its code. A retry clears the pair when it publishes the new
    /// session, allowing that later run to be stamped in turn.
    ///
    /// This is the write that makes `running` mean something again: without it
    /// a supervisor cannot tell a live worker from one that exited into a row
    /// nobody closed, and counts the latter as free capacity.
    pub fn stamp_dispatch_exit(&self, id: i64, exit_code: Option<i64>) -> Result<()> {
        if self.get_dispatch(id)?.is_none() {
            anyhow::bail!("roster row {id} does not exist");
        }
        self.conn().execute(
            "UPDATE agent_dispatches SET exit_code=?1, exited_at_ms=?2 \
             WHERE id=?3 AND exited_at_ms IS NULL",
            rusqlite::params![exit_code, util::now_ms(), id],
        )?;
        Ok(())
    }

    /// Worktrees carrying at least one dispatch row that still occupies a slot.
    ///
    /// The disk reclaimer's unverified-work guard: such a worktree has work no
    /// supervisor has closed, so its `target/` must survive even though nothing
    /// is running in it. Liveness is decided by the typed status
    /// ([`crate::issue::AgentDispatchStatus::is_active`]) rather than a SQL
    /// string list, so the closed set keeps exactly one definition.
    pub fn worktrees_with_active_dispatch(&self) -> Result<Vec<String>> {
        let conn = self.conn();
        let mut stmt =
            conn.prepare("SELECT DISTINCT worktree_path, status FROM agent_dispatches")?;
        let mut rows = stmt.query([])?;
        let mut out: Vec<String> = Vec::new();
        while let Some(r) = rows.next()? {
            let path: String = r.get(0)?;
            let status = crate::issue::AgentDispatchStatus::parse(&r.get::<_, String>(1)?);
            if status.is_active() && !out.contains(&path) {
                out.push(path);
            }
        }
        Ok(out)
    }

    /// Append a progress note to a row's queue — INSERT into
    /// `agent_dispatch_notes`. Returns the new note's id. Errors when the row
    /// does not exist.
    pub fn append_dispatch_note(&self, id: i64, text: &str) -> Result<i64> {
        let text = crate::pipeline_report::note_text(text).map_err(|e| anyhow::anyhow!("{e}"))?;
        if self.get_dispatch(id)?.is_none() {
            anyhow::bail!("roster row {id} does not exist");
        }
        let now = util::now_ms();
        self.conn().execute(
            "INSERT INTO agent_dispatch_notes (dispatch_id, created_at_ms, text) VALUES (?1, ?2, ?3)",
            rusqlite::params![id, now, text],
        )?;
        Ok(self.conn().last_insert_rowid())
    }

    /// Read the progress queue for one dispatch row, newest last.
    /// `since_ms` filters `created_at_ms > since`; `limit` caps (0 = no cap).
    pub fn dispatch_notes(
        &self,
        id: i64,
        since_ms: Option<i64>,
        limit: usize,
    ) -> Result<Vec<DispatchNote>> {
        let cap = if limit == 0 { i64::MAX } else { limit as i64 };
        if let Some(since) = since_ms {
            let mut stmt = self.conn().prepare(
                "SELECT id, dispatch_id, created_at_ms, text \
                 FROM agent_dispatch_notes \
                 WHERE dispatch_id=?1 AND created_at_ms > ?2 \
                 ORDER BY created_at_ms ASC, id ASC LIMIT ?3",
            )?;
            Ok(stmt
                .query_map(rusqlite::params![id, since, cap], map_note)?
                .collect::<rusqlite::Result<Vec<_>>>()?)
        } else {
            let mut stmt = self.conn().prepare(
                "SELECT id, dispatch_id, created_at_ms, text \
                 FROM agent_dispatch_notes \
                 WHERE dispatch_id=?1 \
                 ORDER BY created_at_ms ASC, id ASC LIMIT ?2",
            )?;
            Ok(stmt
                .query_map(rusqlite::params![id, cap], map_note)?
                .collect::<rusqlite::Result<Vec<_>>>()?)
        }
    }
}

fn map_note(r: &rusqlite::Row<'_>) -> rusqlite::Result<DispatchNote> {
    Ok(DispatchNote {
        id: r.get(0)?,
        dispatch_id: r.get(1)?,
        created_at_ms: r.get(2)?,
        text: r.get(3)?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::Db;

    fn temp_db() -> (Db, tempfile::TempDir) {
        let dir = tempfile::TempDir::new().unwrap();
        let db = Db::open_at(&dir.path().join("thegn.db")).unwrap();
        (db, dir)
    }

    fn put_row(db: &Db, issue_id: &str, wt: &str) -> i64 {
        use crate::store::NotificationStore;
        db.put_agent_dispatch(crate::issue::NewDispatch::new(issue_id, wt, "claude"))
            .unwrap();
        db.conn().last_insert_rowid()
    }

    #[test]
    fn set_report_stores_and_reads_back() {
        let (db, _dir) = temp_db();
        let id = put_row(&db, "linear:X-1", "/wt/x");
        db.set_dispatch_report(
            id,
            "verdict: done\ncommits: abc\nunverified: ci\nnext: review",
        )
        .unwrap();
        let row = db.get_dispatch(id).unwrap().unwrap();
        assert_eq!(
            row.report.as_deref(),
            Some("verdict: done\ncommits: abc\nunverified: ci\nnext: review")
        );
    }

    #[test]
    fn publishing_an_opened_run_is_atomic_and_reports_a_missing_reservation() {
        let (db, _dir) = temp_db();
        let id = put_row(&db, "linear:X-0", "/wt/x");
        assert_eq!(
            db.publish_dispatch_run(id, "session-1", "artifact.md")
                .unwrap(),
            DispatchRunPublishOutcome::Published
        );
        let row = db.get_dispatch(id).unwrap().unwrap();
        assert_eq!(row.status, AgentDispatchStatus::Running);
        assert_eq!(row.session_id.as_deref(), Some("session-1"));
        assert_eq!(row.artifact_path.as_deref(), Some("artifact.md"));

        assert_eq!(
            db.publish_dispatch_run(99_999, "orphan-session", "orphan.md")
                .unwrap(),
            DispatchRunPublishOutcome::Missing
        );
    }

    #[test]
    fn publication_never_resurrects_a_concurrent_supervisor_verdict() {
        let (db, _dir) = temp_db();
        for status in [AgentDispatchStatus::Failed, AgentDispatchStatus::Abandoned] {
            let id = put_row(&db, &format!("linear:{status:?}"), "/wt/x");
            db.update_dispatch_status(id, status).unwrap();
            assert_eq!(
                db.publish_dispatch_run(id, "orphan-session", "orphan.md")
                    .unwrap(),
                DispatchRunPublishOutcome::StateChanged { status }
            );
            let row = db.get_dispatch(id).unwrap().unwrap();
            assert_eq!(row.status, status);
            assert_eq!(row.session_id, None);
            assert_eq!(row.artifact_path, None);
        }
    }

    #[test]
    fn a_spawning_reservation_remains_admissible_for_publication() {
        let (db, _dir) = temp_db();
        let id = put_row(&db, "linear:X-SPAWNING", "/wt/x");
        db.update_dispatch_status(id, AgentDispatchStatus::Spawning)
            .unwrap();
        assert_eq!(
            db.publish_dispatch_run(id, "session-2", "artifact-2.md")
                .unwrap(),
            DispatchRunPublishOutcome::Published
        );
        assert_eq!(
            db.get_dispatch(id).unwrap().unwrap().status,
            AgentDispatchStatus::Running
        );
    }

    #[test]
    fn set_report_errors_on_missing_row() {
        let (db, _dir) = temp_db();
        let err = db.set_dispatch_report(99, "x").unwrap_err();
        assert!(err.to_string().contains("99"), "{err}");
        assert!(err.to_string().contains("does not exist"), "{err}");
    }

    #[test]
    fn append_note_returns_id_and_reads_back() {
        let (db, _dir) = temp_db();
        let id = put_row(&db, "linear:X-2", "/wt/x");
        let n1 = db.append_dispatch_note(id, "first").unwrap();
        let n2 = db.append_dispatch_note(id, "second").unwrap();
        assert!(n2 > n1, "note ids must increase");
        let all = db.dispatch_notes(id, None, 0).unwrap();
        assert_eq!(all.len(), 2);
        assert_eq!(all[0].text, "first");
        assert_eq!(all[1].text, "second");
    }

    #[test]
    fn append_note_errors_on_missing_row() {
        let (db, _dir) = temp_db();
        let err = db.append_dispatch_note(99, "x").unwrap_err();
        assert!(err.to_string().contains("99"), "{err}");
    }

    #[test]
    fn status_and_reason_commit_together() {
        let (db, _dir) = temp_db();
        let id = put_row(&db, "linear:X-6", "/wt/x");
        db.update_dispatch_status_with_note(id, AgentDispatchStatus::Failed, "why it failed")
            .unwrap();
        assert_eq!(
            db.get_dispatch(id).unwrap().unwrap().status,
            AgentDispatchStatus::Failed
        );
        assert_eq!(
            db.dispatch_notes(id, None, 0).unwrap()[0].text,
            "why it failed"
        );
    }

    #[test]
    fn stale_status_compare_and_set_preserves_the_newer_decision() {
        let (db, _dir) = temp_db();
        let id = put_row(&db, "linear:X-7", "/wt/x");
        db.update_dispatch_status(id, AgentDispatchStatus::Running)
            .unwrap();
        db.update_dispatch_status(id, AgentDispatchStatus::Abandoned)
            .unwrap();

        assert!(
            !db.compare_and_set_dispatch_status(
                id,
                AgentDispatchStatus::Running,
                AgentDispatchStatus::Done,
                Some("stale reap"),
            )
            .unwrap()
        );
        assert_eq!(
            db.get_dispatch(id).unwrap().unwrap().status,
            AgentDispatchStatus::Abandoned
        );
        assert!(db.dispatch_notes(id, None, 0).unwrap().is_empty());
    }

    #[test]
    fn successful_status_compare_and_set_commits_its_note_and_missing_rows_error() {
        let (db, _dir) = temp_db();
        let id = put_row(&db, "linear:X-CAS", "/wt/x");
        assert!(
            db.compare_and_set_dispatch_status(
                id,
                AgentDispatchStatus::Queued,
                AgentDispatchStatus::Running,
                Some("worker published"),
            )
            .unwrap()
        );
        assert_eq!(
            db.dispatch_notes(id, None, 0).unwrap()[0].text,
            "worker published"
        );
        let error = db
            .update_dispatch_status_with_note(99_999, AgentDispatchStatus::Failed, "cannot exist")
            .unwrap_err();
        assert!(
            error
                .to_string()
                .contains("roster row 99999 does not exist")
        );
        let error = db.stamp_dispatch_exit(99_999, Some(1)).unwrap_err();
        assert!(
            error
                .to_string()
                .contains("roster row 99999 does not exist")
        );
    }

    #[test]
    fn transport_retry_updates_are_expected_state_transitions() {
        let (db, _dir) = temp_db();
        let id = put_row(&db, "linear:X-8", "/wt/x");
        db.update_dispatch_status(id, AgentDispatchStatus::Running)
            .unwrap();

        assert!(
            db.compare_and_set_dispatch_retry_park(
                id,
                AgentDispatchStatus::Running,
                "transport: retry 1",
            )
            .unwrap()
        );
        let parked = db.get_dispatch(id).unwrap().unwrap();
        assert_eq!(parked.status, AgentDispatchStatus::WaitingHuman);
        assert_eq!(parked.note.as_deref(), Some("transport: retry 1"));

        assert!(
            db.compare_and_set_dispatch_status(
                id,
                AgentDispatchStatus::WaitingHuman,
                AgentDispatchStatus::Spawning,
                None,
            )
            .unwrap()
        );
        db.update_dispatch_status(id, AgentDispatchStatus::Done)
            .unwrap();
        assert!(
            !db.compare_and_set_dispatch_retry_run(
                id,
                AgentDispatchStatus::Spawning,
                "replacement",
                "artifact.md",
            )
            .unwrap()
        );
        let closed = db.get_dispatch(id).unwrap().unwrap();
        assert_eq!(closed.status, AgentDispatchStatus::Done);
        assert_ne!(closed.session_id.as_deref(), Some("replacement"));
        assert!(
            !db.compare_and_set_dispatch_retry_park(
                id,
                AgentDispatchStatus::Running,
                "stale transport observer",
            )
            .unwrap()
        );
        let still_closed = db.get_dispatch(id).unwrap().unwrap();
        assert_eq!(still_closed.status, AgentDispatchStatus::Done);
        assert_eq!(still_closed.note.as_deref(), Some("transport: retry 1"));
    }

    fn pipeline_row<'a>(
        issue_id: &'a str,
        worktree: &'a str,
        stage: &'a str,
        artifact: Option<&'a str>,
        parent_id: Option<i64>,
    ) -> crate::issue::NewDispatch<'a> {
        crate::issue::NewDispatch {
            stage: Some(stage),
            artifact_path: artifact,
            parent_id,
            ..crate::issue::NewDispatch::new(issue_id, worktree, "claude")
        }
    }

    #[test]
    fn resume_claim_reconciles_active_source_and_creates_one_finisher_atomically() {
        let (db, _dir) = temp_db();
        let source = db
            .put_agent_dispatch(pipeline_row(
                "linear:X-RESUME",
                "/wt/resume",
                "code",
                Some("old.md"),
                None,
            ))
            .unwrap();
        db.update_dispatch_status(source, AgentDispatchStatus::Running)
            .unwrap();

        let finisher = db
            .claim_resume_dispatch(
                source,
                AgentDispatchStatus::Running,
                pipeline_row("linear:X-RESUME", "/wt/resume", "code", None, Some(source)),
                1,
            )
            .unwrap()
            .expect("the reconciled source frees the only slot");

        let source_row = db.get_dispatch(source).unwrap().unwrap();
        assert_eq!(source_row.status, AgentDispatchStatus::Failed);
        assert_eq!(
            db.dispatch_notes(source, None, 0).unwrap()[0].text,
            "superseded by a --resume-work finisher dispatch"
        );
        let finisher_row = db.get_dispatch(finisher).unwrap().unwrap();
        assert_eq!(finisher_row.status, AgentDispatchStatus::Queued);
        assert_eq!(finisher_row.parent_id, Some(source));
    }

    #[test]
    fn resume_claim_policy_refusal_rolls_back_source_reconciliation_and_note() {
        let (db, _dir) = temp_db();
        let source = db
            .put_agent_dispatch(pipeline_row(
                "linear:X-RESUME",
                "/wt/resume",
                "code",
                Some("old.md"),
                None,
            ))
            .unwrap();
        db.update_dispatch_status(source, AgentDispatchStatus::Running)
            .unwrap();
        let occupant = db
            .put_agent_dispatch(pipeline_row(
                "linear:X-OTHER",
                "/wt/other",
                "code",
                Some("other.md"),
                None,
            ))
            .unwrap();
        db.update_dispatch_status(occupant, AgentDispatchStatus::Running)
            .unwrap();

        let decision = db
            .claim_resume_dispatch(
                source,
                AgentDispatchStatus::Running,
                pipeline_row("linear:X-RESUME", "/wt/resume", "code", None, Some(source)),
                1,
            )
            .unwrap()
            .expect_err("the other row consumes the only stage slot");
        assert_eq!(
            decision,
            crate::pipeline_claim::ClaimDecision::AtCapacity {
                occupied: 1,
                limit: 1,
                stale: 0,
            }
        );
        assert_eq!(
            db.get_dispatch(source).unwrap().unwrap().status,
            AgentDispatchStatus::Running,
            "the refused transaction restores the source"
        );
        assert!(db.dispatch_notes(source, None, 0).unwrap().is_empty());
        assert_eq!(db.list_dispatches().unwrap().len(), 2);
    }

    #[test]
    fn resume_claim_accepts_a_terminal_source_without_rewriting_its_verdict() {
        let (db, _dir) = temp_db();
        let source = db
            .put_agent_dispatch(pipeline_row(
                "linear:X-CLOSED",
                "/wt/closed",
                "code",
                Some("old.md"),
                None,
            ))
            .unwrap();
        db.update_dispatch_status(source, AgentDispatchStatus::Done)
            .unwrap();

        let finisher = db
            .claim_resume_dispatch(
                source,
                AgentDispatchStatus::Done,
                pipeline_row("linear:X-CLOSED", "/wt/closed", "code", None, Some(source)),
                1,
            )
            .unwrap()
            .unwrap();
        assert_eq!(
            db.get_dispatch(source).unwrap().unwrap().status,
            AgentDispatchStatus::Done
        );
        assert!(db.dispatch_notes(source, None, 0).unwrap().is_empty());
        assert_eq!(
            db.get_dispatch(finisher).unwrap().unwrap().parent_id,
            Some(source)
        );
    }

    #[test]
    fn resume_claim_rejects_a_stale_or_missing_source_without_inserting() {
        let (db, _dir) = temp_db();
        let source = db
            .put_agent_dispatch(pipeline_row(
                "linear:X-STALE",
                "/wt/stale",
                "code",
                Some("old.md"),
                None,
            ))
            .unwrap();
        db.update_dispatch_status(source, AgentDispatchStatus::Abandoned)
            .unwrap();
        let new = || pipeline_row("linear:X-STALE", "/wt/stale", "code", None, Some(source));

        let stale = db
            .claim_resume_dispatch(source, AgentDispatchStatus::Running, new(), 1)
            .unwrap_err();
        assert!(matches!(
            stale.downcast_ref::<ResumeDispatchConflict>(),
            Some(ResumeDispatchConflict::SourceStatusChanged {
                source_id: id,
                expected: AgentDispatchStatus::Running,
                actual: AgentDispatchStatus::Abandoned,
            }) if *id == source
        ));
        assert!(
            stale
                .to_string()
                .contains("changed from running to abandoned")
        );
        let missing = db
            .claim_resume_dispatch(99_999, AgentDispatchStatus::Running, new(), 1)
            .unwrap_err();
        assert!(missing.downcast_ref::<ResumeDispatchConflict>().is_none());
        assert!(missing.to_string().contains("dispatch 99999 disappeared"));
        assert_eq!(db.list_dispatches().unwrap().len(), 1);
    }

    #[test]
    fn a_retried_run_clears_only_the_previous_runs_exit_stamp() {
        let (db, _dir) = temp_db();
        let id = put_row(&db, "linear:X-9", "/wt/x");
        db.update_dispatch_status(id, AgentDispatchStatus::Running)
            .unwrap();
        db.stamp_dispatch_exit(id, Some(7)).unwrap();
        assert!(
            db.compare_and_set_dispatch_retry_park(
                id,
                AgentDispatchStatus::Running,
                "transport: retry 1",
            )
            .unwrap()
        );
        assert!(
            db.compare_and_set_dispatch_status(
                id,
                AgentDispatchStatus::WaitingHuman,
                AgentDispatchStatus::Spawning,
                None,
            )
            .unwrap()
        );
        assert!(
            db.compare_and_set_dispatch_retry_run(
                id,
                AgentDispatchStatus::Spawning,
                "replacement",
                "artifact.md",
            )
            .unwrap()
        );
        let relaunched = db.get_dispatch(id).unwrap().unwrap();
        assert_eq!(relaunched.status, AgentDispatchStatus::Running);
        assert_eq!(relaunched.exit_code, None);
        assert_eq!(relaunched.exited_at_ms, None);
    }

    #[test]
    fn dispatch_notes_filters_by_since_and_caps() {
        let (db, _dir) = temp_db();
        let id = put_row(&db, "linear:X-3", "/wt/x");
        let _n1 = db.append_dispatch_note(id, "early").unwrap();
        // Read the timestamp of the first note so we can filter strictly after it.
        let n1_ts = db.dispatch_notes(id, None, 0).unwrap()[0].created_at_ms;
        // Wait for a distinct millisecond so the strict `since` filter is
        // observable without imposing a human-scale delay on the test.
        std::thread::sleep(std::time::Duration::from_millis(2));
        let _n2 = db.append_dispatch_note(id, "late").unwrap();

        // Filter by since: only "late" should return (created_at_ms > n1_ts)
        let filtered = db.dispatch_notes(id, Some(n1_ts), 0).unwrap();
        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0].text, "late");

        // Cap at 1
        let capped = db.dispatch_notes(id, None, 1).unwrap();
        assert_eq!(capped.len(), 1);
        assert_eq!(capped[0].text, "early");
    }

    #[test]
    fn set_report_overwrites() {
        let (db, _dir) = temp_db();
        let id = put_row(&db, "linear:X-4", "/wt/x");
        db.set_dispatch_report(id, "first").unwrap();
        db.set_dispatch_report(id, "second").unwrap();
        let row = db.get_dispatch(id).unwrap().unwrap();
        assert_eq!(row.report.as_deref(), Some("second"));
    }

    #[test]
    fn db_writes_reapply_hostile_text_policy() {
        let (db, _dir) = temp_db();
        let id = put_row(&db, "linear:X-5", "/wt/x");
        db.set_dispatch_report(id, "before\x1b[2J\nafter\r")
            .unwrap();
        assert_eq!(
            db.get_dispatch(id).unwrap().unwrap().report.as_deref(),
            Some("before[2J\nafter")
        );
        db.append_dispatch_note(id, "first\nsecond\x07").unwrap();
        assert_eq!(
            db.dispatch_notes(id, None, 0).unwrap()[0].text,
            "firstsecond"
        );
    }
}
