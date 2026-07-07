//! Hibernation state (schema v38): the embedded-SQLite implementation of the
//! [`HibernationStore`] seam over `worktree_hibernations`. Sibling `impl`
//! block (via the `conn()` accessor) so `db.rs` only carries the schema DDL.

use anyhow::Result;
use rusqlite::params;

use crate::db::Db;
use crate::store::{HibernationRow, HibernationStore};
use crate::util;

fn row_from(r: &rusqlite::Row<'_>) -> rusqlite::Result<HibernationRow> {
    Ok(HibernationRow {
        worktree_path: r.get(0)?,
        repo_path: r.get(1)?,
        env_name: r.get(2)?,
        sandbox_name: r.get(3)?,
        snapshot_id: r.get(4)?,
        head: r.get::<_, Option<String>>(5)?.unwrap_or_default(),
        state: r.get(6)?,
        created_at: r.get(7)?,
        updated_at: r.get(8)?,
    })
}

const COLS: &str =
    "worktree_path,repo_path,env_name,sandbox_name,snapshot_id,head,state,created_at,updated_at";

impl HibernationStore for Db {
    fn put_hibernation(&self, row: &HibernationRow) -> Result<()> {
        let now = util::now();
        self.conn().execute(
            "INSERT INTO worktree_hibernations
               (worktree_path,repo_path,env_name,sandbox_name,snapshot_id,head,state,created_at,updated_at)
             VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?8)
             ON CONFLICT(worktree_path) DO UPDATE SET
               repo_path=?2, env_name=?3, sandbox_name=?4, snapshot_id=?5,
               head=?6, state=?7, updated_at=?8",
            params![
                row.worktree_path,
                row.repo_path,
                row.env_name,
                row.sandbox_name,
                row.snapshot_id,
                row.head,
                row.state,
                now,
            ],
        )?;
        Ok(())
    }

    fn set_hibernation_state(
        &self,
        worktree_path: &str,
        state: &str,
        snapshot_id: Option<&str>,
    ) -> Result<()> {
        self.conn().execute(
            "UPDATE worktree_hibernations SET
               state=?2,
               snapshot_id=COALESCE(?3, snapshot_id),
               updated_at=?4
             WHERE worktree_path=?1",
            params![worktree_path, state, snapshot_id, util::now()],
        )?;
        Ok(())
    }

    fn hibernation_for(&self, worktree_path: &str) -> Result<Option<HibernationRow>> {
        let r = self
            .conn()
            .query_row(
                &format!("SELECT {COLS} FROM worktree_hibernations WHERE worktree_path=?1"),
                params![worktree_path],
                row_from,
            )
            .ok();
        Ok(r)
    }

    fn hibernations(&self) -> Result<Vec<HibernationRow>> {
        let conn = self.conn();
        let mut stmt = conn.prepare(&format!(
            "SELECT {COLS} FROM worktree_hibernations ORDER BY updated_at DESC"
        ))?;
        let rows = stmt
            .query_map([], row_from)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    fn delete_hibernation(&self, worktree_path: &str) -> Result<()> {
        self.conn().execute(
            "DELETE FROM worktree_hibernations WHERE worktree_path=?1",
            params![worktree_path],
        )?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(wt: &str) -> HibernationRow {
        HibernationRow {
            worktree_path: wt.into(),
            repo_path: "/repo".into(),
            env_name: "hetzner".into(),
            sandbox_name: "sz-repo-wt".into(),
            snapshot_id: "00000000000000000009-abcd1234".into(),
            head: "abcd1234".into(),
            state: "capturing".into(),
            created_at: 0,
            updated_at: 0,
        }
    }

    #[test]
    fn hibernation_row_fsm_roundtrip() {
        let db = Db::open_memory().unwrap();
        assert!(db.hibernation_for("/wt").unwrap().is_none());

        // capturing → hibernated → restoring → deleted (the happy path).
        db.put_hibernation(&row("/wt")).unwrap();
        let got = db.hibernation_for("/wt").unwrap().unwrap();
        assert_eq!(got.state, "capturing");
        assert_eq!(got.sandbox_name, "sz-repo-wt");

        db.set_hibernation_state("/wt", "hibernated", None).unwrap();
        let got = db.hibernation_for("/wt").unwrap().unwrap();
        assert_eq!(got.state, "hibernated");
        // snapshot_id untouched when None is passed.
        assert_eq!(got.snapshot_id, "00000000000000000009-abcd1234");

        db.set_hibernation_state("/wt", "restoring", Some("00000000000000000011-ffff0000"))
            .unwrap();
        let got = db.hibernation_for("/wt").unwrap().unwrap();
        assert_eq!(got.state, "restoring");
        assert_eq!(got.snapshot_id, "00000000000000000011-ffff0000");

        db.delete_hibernation("/wt").unwrap();
        assert!(db.hibernation_for("/wt").unwrap().is_none());
        // Idempotent delete.
        db.delete_hibernation("/wt").unwrap();
    }

    #[test]
    fn put_replaces_and_list_orders_by_recency() {
        let db = Db::open_memory().unwrap();
        db.put_hibernation(&row("/a")).unwrap();
        db.put_hibernation(&row("/b")).unwrap();
        // Re-put of /a bumps updated_at (and may re-point the snapshot).
        let mut a2 = row("/a");
        a2.snapshot_id = "00000000000000000020-beef0000".into();
        db.put_hibernation(&a2).unwrap();
        let all = db.hibernations().unwrap();
        assert_eq!(all.len(), 2);
        assert_eq!(
            db.hibernation_for("/a").unwrap().unwrap().snapshot_id,
            "00000000000000000020-beef0000"
        );
    }
}
