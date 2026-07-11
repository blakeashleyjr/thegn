//! IntentStore state — the embedded-SQLite implementation of the
//! [`IntentStore`] seam. Sibling `impl` block (via the `conn()` accessor) so
//! the pinned `db.rs` only carries the schema DDL, not these bodies.

use crate::db::Db;
use crate::store::{IntentRow, IntentStore};
use crate::util;
use anyhow::Result;
use rusqlite::params;

impl IntentStore for Db {
    fn put_intent(&self, kind: &str, payload: &str) -> Result<i64> {
        self.conn().execute(
            "INSERT INTO intents(kind, payload, created_at) VALUES(?1, ?2, ?3)",
            params![kind, payload, util::now()],
        )?;
        Ok(self.conn().last_insert_rowid())
    }

    fn take_intents(&self, kind: &str) -> Result<Vec<IntentRow>> {
        // Claim-and-delete in one transaction so two consumers can't both
        // apply the same intent.
        let tx = self.conn().unchecked_transaction()?;
        let rows: Vec<IntentRow> = {
            let mut stmt = tx.prepare(
                "SELECT id, kind, payload, created_at FROM intents
                 WHERE kind = ?1 ORDER BY id ASC",
            )?;
            let mapped = stmt.query_map(params![kind], |r| {
                Ok(IntentRow {
                    id: r.get(0)?,
                    kind: r.get(1)?,
                    payload: r.get(2)?,
                    created_at: r.get(3)?,
                })
            })?;
            mapped.filter_map(|r| r.ok()).collect()
        };
        tx.execute("DELETE FROM intents WHERE kind = ?1", params![kind])?;
        tx.commit()?;
        Ok(rows)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn put_take_roundtrip_fifo_and_empties() {
        let db = Db::open_memory().unwrap();
        db.put_intent("focus_workspace", r#"{"repo":"/a"}"#)
            .unwrap();
        db.put_intent("focus_workspace", r#"{"repo":"/b"}"#)
            .unwrap();
        let rows = db.take_intents("focus_workspace").unwrap();
        assert_eq!(rows.len(), 2);
        // FIFO by insertion order.
        assert_eq!(rows[0].payload, r#"{"repo":"/a"}"#);
        assert_eq!(rows[1].payload, r#"{"repo":"/b"}"#);
        assert!(rows[0].id < rows[1].id);
        assert!(rows.iter().all(|r| r.kind == "focus_workspace"));
        assert!(rows.iter().all(|r| r.created_at > 0));
        // Taking consumed everything.
        assert!(db.take_intents("focus_workspace").unwrap().is_empty());
    }

    #[test]
    fn take_is_kind_isolated() {
        let db = Db::open_memory().unwrap();
        db.put_intent("focus_workspace", r#"{"repo":"/a"}"#)
            .unwrap();
        db.put_intent("other_kind", "{}").unwrap();
        assert_eq!(db.take_intents("focus_workspace").unwrap().len(), 1);
        // The other kind is untouched by the take above.
        assert_eq!(db.take_intents("other_kind").unwrap().len(), 1);
    }
}
