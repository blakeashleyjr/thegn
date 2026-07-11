//! Trust-on-first-use approvals (the embedded-SQLite [`RepoTrustStore`] impl):
//! per-repo decisions on the gated sandbox requests a `.thegn.*` overlay
//! makes (extra mounts, init/prepare scripts, image, ports, gpu, nix-daemon).
//!
//! Sibling `impl` block (via the `conn()` accessor) so the pinned `db.rs` only
//! carries the `repo_trust` schema DDL (v32), not these bodies. The canonical
//! `request_json` is the match key; a later edit to the requested set produces
//! a different key and re-prompts.

use anyhow::Result;
use rusqlite::params;

use crate::db::Db;
use crate::store::{RepoTrustRow, RepoTrustStore};

impl RepoTrustStore for Db {
    fn repo_trust_decide(
        &self,
        repo_root: &str,
        request_id: &str,
        request_json: &str,
        decision: &str,
        now: i64,
    ) -> Result<()> {
        self.conn().execute(
            r#"INSERT INTO repo_trust(repo_root,request_id,request_json,decision,decided_at)
               VALUES(?1,?2,?3,?4,?5)
               ON CONFLICT(repo_root,request_json)
               DO UPDATE SET request_id=?2, decision=?4, decided_at=?5"#,
            params![repo_root, request_id, request_json, decision, now],
        )?;
        Ok(())
    }

    fn repo_trust_revoke(&self, repo_root: &str, request_json: &str) -> Result<()> {
        self.conn().execute(
            "DELETE FROM repo_trust WHERE repo_root=?1 AND request_json=?2",
            params![repo_root, request_json],
        )?;
        Ok(())
    }

    fn repo_trust_list(&self, repo_root: &str) -> Result<Vec<RepoTrustRow>> {
        let conn = self.conn();
        let mut stmt = conn.prepare(
            "SELECT request_id, request_json, decision, decided_at
               FROM repo_trust WHERE repo_root=?1 ORDER BY decided_at DESC",
        )?;
        let rows = stmt
            .query_map(params![repo_root], |r| {
                Ok(RepoTrustRow {
                    request_id: r.get(0)?,
                    request_json: r.get(1)?,
                    decision: r.get(2)?,
                    decided_at: r.get(3)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    fn repo_trust_approved(&self, repo_root: &str) -> Result<Vec<String>> {
        let conn = self.conn();
        let mut stmt = conn.prepare(
            "SELECT request_json FROM repo_trust
               WHERE repo_root=?1 AND decision='approved'",
        )?;
        let rows = stmt
            .query_map(params![repo_root], |r| r.get::<_, String>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn db() -> Db {
        Db::open_memory().expect("in-memory db")
    }

    #[test]
    fn approve_then_listed_and_approved() {
        let db = db();
        db.repo_trust_decide(
            "/repo",
            "abc123",
            r#"{"key":"sandbox.mounts","value":"/x:/x"}"#,
            "approved",
            100,
        )
        .unwrap();
        let approved = db.repo_trust_approved("/repo").unwrap();
        assert_eq!(
            approved,
            vec![r#"{"key":"sandbox.mounts","value":"/x:/x"}"#.to_string()]
        );
        let list = db.repo_trust_list("/repo").unwrap();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].decision, "approved");
        assert_eq!(list[0].request_id, "abc123");
    }

    #[test]
    fn denied_is_not_approved() {
        let db = db();
        db.repo_trust_decide(
            "/repo",
            "d1",
            r#"{"key":"sandbox.image","value":"evil"}"#,
            "denied",
            1,
        )
        .unwrap();
        assert!(db.repo_trust_approved("/repo").unwrap().is_empty());
        assert_eq!(db.repo_trust_list("/repo").unwrap().len(), 1);
    }

    #[test]
    fn decide_is_idempotent_upsert() {
        let db = db();
        let json = r#"{"key":"sandbox.mounts","value":"/x:/x"}"#;
        db.repo_trust_decide("/repo", "id", json, "denied", 1)
            .unwrap();
        db.repo_trust_decide("/repo", "id", json, "approved", 2)
            .unwrap();
        let list = db.repo_trust_list("/repo").unwrap();
        assert_eq!(list.len(), 1, "same (repo,json) updates in place");
        assert_eq!(list[0].decision, "approved");
        assert_eq!(list[0].decided_at, 2);
    }

    #[test]
    fn revoke_removes_approval() {
        let db = db();
        let json = r#"{"key":"sandbox.mounts","value":"/x:/x"}"#;
        db.repo_trust_decide("/repo", "id", json, "approved", 1)
            .unwrap();
        db.repo_trust_revoke("/repo", json).unwrap();
        assert!(db.repo_trust_approved("/repo").unwrap().is_empty());
        assert!(db.repo_trust_list("/repo").unwrap().is_empty());
    }

    #[test]
    fn scoped_per_repo() {
        let db = db();
        let json = r#"{"key":"sandbox.mounts","value":"/x:/x"}"#;
        db.repo_trust_decide("/repo-a", "id", json, "approved", 1)
            .unwrap();
        assert!(db.repo_trust_approved("/repo-b").unwrap().is_empty());
        assert_eq!(db.repo_trust_approved("/repo-a").unwrap().len(), 1);
    }

    #[test]
    fn migrates_repo_trust_additive_from_v31() {
        use rusqlite::Connection;
        // A pre-v32 DB (no `repo_trust` table): opening it adds the table
        // additively without disturbing existing data.
        let dir = std::env::temp_dir().join(format!("sz-db-trust-mig-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("db.sqlite");
        {
            let conn = Connection::open(&path).unwrap();
            conn.execute_batch(
                "PRAGMA user_version = 31;
                 CREATE TABLE repos (path TEXT PRIMARY KEY, name TEXT);
                 INSERT INTO repos(path,name) VALUES ('/keep','keep');",
            )
            .unwrap();
        }
        let db = Db::open_at(&path).unwrap();
        let kept: i64 = db
            .conn()
            .query_row("SELECT COUNT(*) FROM repos WHERE path='/keep'", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(kept, 1, "existing data survives the migration");
        db.repo_trust_decide("/keep", "id", "{\"k\":1}", "approved", 7)
            .unwrap();
        assert_eq!(db.repo_trust_approved("/keep").unwrap().len(), 1);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
