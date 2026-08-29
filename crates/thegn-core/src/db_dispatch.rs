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
