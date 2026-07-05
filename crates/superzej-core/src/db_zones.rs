//! Zone existence + membership (the embedded-SQLite [`ZoneStore`] impl): the
//! `zones` table and the nullable `workspaces.zone_id` (schema v33).
//!
//! Sibling `impl` block (via the `conn()` accessor) so the pinned `db.rs` only
//! carries the schema DDL, not these bodies. Membership is DB-tracked, never
//! path-inferred (a filesystem path may only *suggest* a zone in the UI).

use anyhow::Result;
use rusqlite::{OptionalExtension, params};

use crate::db::Db;
use crate::store::{ZoneDeleteOutcome, ZoneRow, ZoneStore};

/// SQL selecting a zone row + its member count, parameterised by a WHERE clause.
fn zone_select(where_clause: &str) -> String {
    format!(
        "SELECT z.zone_id, z.name, z.created_at,
                (SELECT COUNT(*) FROM workspaces w WHERE w.zone_id = z.zone_id)
           FROM zones z {where_clause}"
    )
}

fn row_to_zone(r: &rusqlite::Row) -> rusqlite::Result<ZoneRow> {
    Ok(ZoneRow {
        zone_id: r.get(0)?,
        name: r.get(1)?,
        created_at: r.get(2)?,
        member_count: r.get(3)?,
    })
}

impl ZoneStore for Db {
    fn create_zone(&self, name: &str, now: i64) -> Result<i64> {
        self.conn().execute(
            "INSERT INTO zones(name, created_at) VALUES(?1, ?2)",
            params![name, now],
        )?;
        Ok(self.conn().last_insert_rowid())
    }

    fn rename_zone(&self, zone_id: i64, new_name: &str) -> Result<()> {
        self.conn().execute(
            "UPDATE zones SET name=?2 WHERE zone_id=?1",
            params![zone_id, new_name],
        )?;
        Ok(())
    }

    fn delete_zone(&self, zone_id: i64, force: bool) -> Result<ZoneDeleteOutcome> {
        let members: i64 = self.conn().query_row(
            "SELECT COUNT(*) FROM workspaces WHERE zone_id=?1",
            params![zone_id],
            |r| r.get(0),
        )?;
        if members > 0 && !force {
            return Ok(ZoneDeleteOutcome::RefusedNonEmpty(members));
        }
        if members > 0 {
            self.conn().execute(
                "UPDATE workspaces SET zone_id=NULL WHERE zone_id=?1",
                params![zone_id],
            )?;
        }
        self.conn()
            .execute("DELETE FROM zones WHERE zone_id=?1", params![zone_id])?;
        Ok(ZoneDeleteOutcome::Deleted)
    }

    fn list_zones(&self) -> Result<Vec<ZoneRow>> {
        let conn = self.conn();
        let mut stmt = conn.prepare(&zone_select("ORDER BY z.name"))?;
        let rows = stmt
            .query_map([], row_to_zone)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    fn assign_workspace_zone(&self, repo_path: &str, zone: Option<i64>) -> Result<()> {
        self.conn().execute(
            "UPDATE workspaces SET zone_id=?2 WHERE repo_path=?1",
            params![repo_path, zone],
        )?;
        Ok(())
    }

    fn zone_of_workspace(&self, repo_path: &str) -> Result<Option<ZoneRow>> {
        let conn = self.conn();
        let sql = zone_select("JOIN workspaces w ON w.zone_id = z.zone_id WHERE w.repo_path=?1");
        let row = conn
            .query_row(&sql, params![repo_path], row_to_zone)
            .optional()?;
        Ok(row)
    }

    fn zone_of_worktree(&self, worktree: &str) -> Result<Option<ZoneRow>> {
        // worktree → its repo_path → the workspace's zone. If the arg isn't a
        // known worktree, treat it as a repo path directly (home-tab panes).
        let repo_path: Option<String> = self
            .conn()
            .query_row(
                "SELECT repo_path FROM worktrees WHERE worktree=?1",
                params![worktree],
                |r| r.get(0),
            )
            .optional()?;
        let key = repo_path.unwrap_or_else(|| worktree.to_string());
        self.zone_of_workspace(&key)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::WorkspaceStore;

    fn db() -> Db {
        Db::open_memory().expect("in-memory db")
    }

    fn add_ws(db: &Db, repo: &str) {
        db.put_workspace(repo, "ws", "repo").unwrap();
    }

    #[test]
    fn create_list_and_member_count() {
        let db = db();
        let a = db.create_zone("client-a", 10).unwrap();
        db.create_zone("client-b", 11).unwrap();
        add_ws(&db, "/repo1");
        db.assign_workspace_zone("/repo1", Some(a)).unwrap();
        let zones = db.list_zones().unwrap();
        assert_eq!(zones.len(), 2);
        // Ordered by name: client-a first.
        assert_eq!(zones[0].name, "client-a");
        assert_eq!(zones[0].member_count, 1);
        assert_eq!(zones[1].member_count, 0);
    }

    #[test]
    fn duplicate_name_rejected() {
        let db = db();
        db.create_zone("dup", 1).unwrap();
        assert!(db.create_zone("dup", 2).is_err());
    }

    #[test]
    fn delete_refuses_nonempty_unless_forced() {
        let db = db();
        let z = db.create_zone("z", 1).unwrap();
        add_ws(&db, "/r");
        db.assign_workspace_zone("/r", Some(z)).unwrap();
        assert_eq!(
            db.delete_zone(z, false).unwrap(),
            ZoneDeleteOutcome::RefusedNonEmpty(1)
        );
        // Force unassigns then deletes.
        assert_eq!(db.delete_zone(z, true).unwrap(), ZoneDeleteOutcome::Deleted);
        assert!(db.zone_of_workspace("/r").unwrap().is_none());
        assert!(db.list_zones().unwrap().is_empty());
    }

    #[test]
    fn membership_lookup_by_workspace_and_worktree() {
        let db = db();
        let z = db.create_zone("z", 1).unwrap();
        add_ws(&db, "/repo");
        db.assign_workspace_zone("/repo", Some(z)).unwrap();
        // Register a worktree under that repo.
        db.put_worktree("t", "/repo", "/repo/wt", "main", None, None)
            .unwrap();
        assert_eq!(db.zone_of_workspace("/repo").unwrap().unwrap().name, "z");
        assert_eq!(db.zone_of_worktree("/repo/wt").unwrap().unwrap().name, "z");
        // Unknown worktree falls back to repo-path interpretation.
        assert_eq!(db.zone_of_worktree("/repo").unwrap().unwrap().name, "z");
        assert!(db.zone_of_worktree("/other").unwrap().is_none());
    }

    #[test]
    fn unassign_clears_zone() {
        let db = db();
        let z = db.create_zone("z", 1).unwrap();
        add_ws(&db, "/r");
        db.assign_workspace_zone("/r", Some(z)).unwrap();
        db.assign_workspace_zone("/r", None).unwrap();
        assert!(db.zone_of_workspace("/r").unwrap().is_none());
        assert_eq!(db.list_zones().unwrap()[0].member_count, 0);
    }

    #[test]
    fn migrates_zones_additive_from_v32() {
        use rusqlite::Connection;
        let dir = std::env::temp_dir().join(format!("sz-db-zone-mig-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("db.sqlite");
        {
            let conn = Connection::open(&path).unwrap();
            conn.execute_batch(
                "PRAGMA user_version = 32;
                 CREATE TABLE workspaces (repo_path TEXT PRIMARY KEY, name TEXT);
                 INSERT INTO workspaces(repo_path,name) VALUES('/keep','k');",
            )
            .unwrap();
        }
        let db = Db::open_at(&path).unwrap();
        let z = db.create_zone("z", 1).unwrap();
        db.assign_workspace_zone("/keep", Some(z)).unwrap();
        assert_eq!(db.zone_of_workspace("/keep").unwrap().unwrap().name, "z");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
