//! Dispatch-report + per-row progress queue — sibling `impl Db` block so the
//! pinned `db.rs` only carries the schema DDL, not these bodies. The DB is a
//! cache; git / the live source is truth.

use crate::db::Db;
use crate::issue::DispatchNote;
use crate::store::NotificationStore;
use crate::util;
use anyhow::Result;

impl Db {
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
        use rusqlite::OptionalExtension;
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
    /// `allow_duplicate` is the auditable override: it skips the policy and
    /// records the operator's reason as the row's first note, so a deliberate
    /// duplicate is always distinguishable from a runaway one.
    pub fn claim_dispatch(
        &self,
        new: crate::issue::NewDispatch<'_>,
        limit: u32,
        allow_duplicate: Option<&str>,
    ) -> Result<std::result::Result<i64, crate::pipeline_claim::ClaimDecision>> {
        use crate::pipeline_claim::{ClaimRequest, decide};
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
                if allow_duplicate.is_none() {
                    let rows = self.list_dispatches()?;
                    let d = decide(&rows, &req, limit);
                    if !d.granted() {
                        return Ok(Err(d));
                    }
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

    /// Stamp a row's worker exit (v63): the exit code, if it was reaped, and
    /// when. Idempotent-by-overwrite (last exit wins, which is what a relaunched
    /// worker should record).
    ///
    /// This is the write that makes `running` mean something again: without it
    /// a supervisor cannot tell a live worker from one that exited into a row
    /// nobody closed, and counts the latter as free capacity.
    pub fn stamp_dispatch_exit(&self, id: i64, exit_code: Option<i64>) -> Result<()> {
        if self.get_dispatch(id)?.is_none() {
            anyhow::bail!("roster row {id} does not exist");
        }
        self.conn().execute(
            "UPDATE agent_dispatches SET exit_code=?1, exited_at_ms=?2 WHERE id=?3",
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
