//! Store seam for credential-free `sessions.fork` lineage rows (schema v62).
//!
//! The implementation stores only the metadata needed for listings and audit.
//! Live argv and environment recipes stay in the daemon's memory and never
//! cross this seam.

use anyhow::Result;
use rusqlite::params;

use crate::db::Db;
use crate::session_fork::{ForkRecord, ForkSourceKind};

/// Persistence for successful session-fork lineage records.
pub trait SessionForkStore {
    fn put_session_fork(&self, row: &ForkRecord) -> Result<()>;
    fn session_fork(&self, child_id: &str) -> Result<Option<ForkRecord>>;
    fn session_forks(&self) -> Result<Vec<ForkRecord>>;
    fn delete_session_fork(&self, child_id: &str) -> Result<()>;
}

fn source_kind_name(kind: ForkSourceKind) -> &'static str {
    match kind {
        ForkSourceKind::Daemon => "daemon",
        ForkSourceKind::Harness => "harness",
    }
}

fn source_kind(value: String) -> rusqlite::Result<ForkSourceKind> {
    match value.as_str() {
        "daemon" => Ok(ForkSourceKind::Daemon),
        "harness" => Ok(ForkSourceKind::Harness),
        other => Err(rusqlite::Error::FromSqlConversionFailure(
            0,
            rusqlite::types::Type::Text,
            format!("unknown fork source kind `{other}`").into(),
        )),
    }
}

fn row_from(r: &rusqlite::Row<'_>) -> rusqlite::Result<ForkRecord> {
    Ok(ForkRecord {
        child_id: r.get(0)?,
        source_kind: source_kind(r.get(1)?)?,
        source_id: r.get(2)?,
        harness: r.get(3)?,
        worktree: r.get(4)?,
        created_at: r.get(5)?,
    })
}

const COLS: &str = "child_id,source_kind,source_id,harness,worktree,created_at";

impl SessionForkStore for Db {
    fn put_session_fork(&self, row: &ForkRecord) -> Result<()> {
        self.conn().execute(
            "INSERT OR REPLACE INTO session_forks
               (child_id, source_kind, source_id, harness, worktree, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                row.child_id,
                source_kind_name(row.source_kind),
                row.source_id,
                row.harness,
                row.worktree,
                row.created_at,
            ],
        )?;
        Ok(())
    }

    fn session_fork(&self, child_id: &str) -> Result<Option<ForkRecord>> {
        match self.conn().query_row(
            &format!("SELECT {COLS} FROM session_forks WHERE child_id = ?1"),
            params![child_id],
            row_from,
        ) {
            Ok(row) => Ok(Some(row)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(error) => Err(error.into()),
        }
    }

    fn session_forks(&self) -> Result<Vec<ForkRecord>> {
        let mut stmt = self.conn().prepare(&format!(
            "SELECT {COLS} FROM session_forks ORDER BY created_at DESC, child_id"
        ))?;
        let rows = stmt.query_map([], row_from)?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    fn delete_session_fork(&self, child_id: &str) -> Result<()> {
        self.conn().execute(
            "DELETE FROM session_forks WHERE child_id = ?1",
            params![child_id],
        )?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::SessionForkStore;

    fn row(child_id: &str) -> ForkRecord {
        ForkRecord {
            child_id: child_id.into(),
            source_kind: ForkSourceKind::Daemon,
            source_id: "parent-1".into(),
            harness: None,
            worktree: Some("/worktree".into()),
            created_at: 42,
        }
    }

    #[test]
    fn lineage_cache_round_trips_without_recipe_fields() {
        let db = Db::open_memory().unwrap();
        let original = row("child-1");
        db.put_session_fork(&original).unwrap();
        assert_eq!(db.session_fork("child-1").unwrap(), Some(original));
        assert_eq!(db.session_fork("missing").unwrap(), None);
        assert_eq!(db.session_forks().unwrap().len(), 1);

        db.delete_session_fork("child-1").unwrap();
        assert!(db.session_forks().unwrap().is_empty());
    }

    #[test]
    fn harness_lineage_round_trips_its_native_identity_only() {
        let db = Db::open_memory().unwrap();
        let record = ForkRecord {
            child_id: "child-2".into(),
            source_kind: ForkSourceKind::Harness,
            source_id: "native-1".into(),
            harness: Some("claude".into()),
            worktree: None,
            created_at: 7,
        };
        db.put_session_fork(&record).unwrap();
        assert_eq!(db.session_fork("child-2").unwrap(), Some(record));
    }
}
