//! Warm-pool state (schema v26): the embedded-SQLite implementation of the
//! [`PoolStore`] seam — spare sandboxes (`pool_spares`), per-`(repo, env)` base
//! snapshots (`env_base_snapshots`), and the runtime pool-target override
//! (`pool_targets`). Sibling `impl` block (via the `conn()` accessor) so the
//! pinned `db.rs` only carries the schema DDL, not these bodies.

use anyhow::Result;
use rusqlite::params;

use crate::db::{Db, PoolSpare};
use crate::store::PoolStore;
use crate::util;

impl PoolStore for Db {
    fn set_base_snapshot(
        &self,
        repo_path: &str,
        env_name: &str,
        snapshot_id: &str,
        lock_hash: &str,
    ) -> Result<()> {
        self.conn().execute(
            r#"INSERT INTO env_base_snapshots(repo_path,env_name,snapshot_id,lock_hash,updated_at)
               VALUES(?1,?2,?3,?4,?5)
               ON CONFLICT(repo_path,env_name) DO UPDATE SET
                 snapshot_id=?3, lock_hash=?4, updated_at=?5"#,
            params![repo_path, env_name, snapshot_id, lock_hash, util::now()],
        )?;
        Ok(())
    }

    fn base_snapshot(&self, repo_path: &str, env_name: &str) -> Result<Option<(String, String)>> {
        let r = self
            .conn()
            .query_row(
                "SELECT snapshot_id, lock_hash FROM env_base_snapshots WHERE repo_path=?1 AND env_name=?2",
                params![repo_path, env_name],
                |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)),
            )
            .ok();
        Ok(r)
    }

    fn insert_pool_spare(&self, name: &str, repo: &str, env: &str) -> Result<()> {
        let now = util::now();
        self.conn().execute(
            "INSERT OR REPLACE INTO pool_spares
               (sandbox_name,repo_path,env_name,state,checkpoint_id,lock_hash,created_at,updated_at)
             VALUES(?1,?2,?3,'provisioning',NULL,NULL,?4,?4)",
            params![name, repo, env, now],
        )?;
        Ok(())
    }

    fn set_pool_spare_ready(
        &self,
        name: &str,
        checkpoint_id: Option<&str>,
        lock_hash: &str,
    ) -> Result<()> {
        self.conn().execute(
            "UPDATE pool_spares SET state='ready', checkpoint_id=?2, lock_hash=?3, updated_at=?4
             WHERE sandbox_name=?1",
            params![name, checkpoint_id, lock_hash, util::now()],
        )?;
        Ok(())
    }

    fn delete_pool_spare(&self, name: &str) -> Result<()> {
        self.conn().execute(
            "DELETE FROM pool_spares WHERE sandbox_name=?1",
            params![name],
        )?;
        Ok(())
    }

    fn pool_spares_for(&self, repo: &str, env: &str) -> Result<Vec<PoolSpare>> {
        let mut stmt = self.conn().prepare(
            "SELECT sandbox_name,repo_path,env_name,state,checkpoint_id,lock_hash,created_at,updated_at
             FROM pool_spares WHERE repo_path=?1 AND env_name=?2 ORDER BY created_at DESC",
        )?;
        let rows = stmt
            .query_map(params![repo, env], |r| {
                Ok(PoolSpare {
                    sandbox_name: r.get(0)?,
                    repo_path: r.get(1)?,
                    env_name: r.get(2)?,
                    state: r.get(3)?,
                    checkpoint_id: r.get(4)?,
                    lock_hash: r.get(5)?,
                    created_at: r.get(6)?,
                    updated_at: r.get(7)?,
                })
            })?
            .filter_map(|r| r.ok())
            .collect();
        Ok(rows)
    }

    fn claim_pool_spare(
        &self,
        repo: &str,
        env: &str,
        worktree: &str,
    ) -> Result<Option<(String, Option<String>)>> {
        let tx = self.conn().unchecked_transaction()?;
        let picked: Option<(String, Option<String>)> = tx
            .query_row(
                "SELECT sandbox_name, checkpoint_id FROM pool_spares
                 WHERE repo_path=?1 AND env_name=?2 AND state='ready'
                 ORDER BY created_at ASC LIMIT 1",
                params![repo, env],
                |r| Ok((r.get::<_, String>(0)?, r.get::<_, Option<String>>(1)?)),
            )
            .ok();
        if let Some((ref name, _)) = picked {
            tx.execute(
                "UPDATE pool_spares SET state='claimed', updated_at=?2 WHERE sandbox_name=?1",
                params![name, util::now()],
            )?;
            tx.execute(
                "UPDATE worktrees SET provider_sandbox_id=?2 WHERE worktree=?1",
                params![worktree, name],
            )?;
        }
        tx.commit()?;
        Ok(picked)
    }

    fn pool_spare_by_name(&self, name: &str) -> Result<Option<PoolSpare>> {
        let r = self
            .conn()
            .query_row(
                "SELECT sandbox_name,repo_path,env_name,state,checkpoint_id,lock_hash,\
                        created_at,updated_at
                 FROM pool_spares WHERE sandbox_name=?1",
                params![name],
                |r| {
                    Ok(PoolSpare {
                        sandbox_name: r.get(0)?,
                        repo_path: r.get(1)?,
                        env_name: r.get(2)?,
                        state: r.get(3)?,
                        checkpoint_id: r.get(4)?,
                        lock_hash: r.get(5)?,
                        created_at: r.get(6)?,
                        updated_at: r.get(7)?,
                    })
                },
            )
            .ok();
        Ok(r)
    }

    fn worktree_provider_sandbox(&self, worktree: &str) -> Result<Option<String>> {
        let r = self
            .conn()
            .query_row(
                "SELECT provider_sandbox_id FROM worktrees WHERE worktree=?1",
                params![worktree],
                |r| r.get::<_, Option<String>>(0),
            )
            .ok()
            .flatten()
            .filter(|s: &String| !s.is_empty());
        Ok(r)
    }

    fn pool_target(&self, repo: &str, env: &str) -> Result<Option<i64>> {
        let r = self
            .conn()
            .query_row(
                "SELECT target FROM pool_targets WHERE repo_path=?1 AND env_name=?2",
                params![repo, env],
                |r| r.get::<_, i64>(0),
            )
            .ok();
        Ok(r)
    }

    fn set_pool_target(&self, repo: &str, env: &str, target: i64) -> Result<()> {
        self.conn().execute(
            "INSERT INTO pool_targets(repo_path,env_name,target) VALUES(?1,?2,?3)
             ON CONFLICT(repo_path,env_name) DO UPDATE SET target=?3",
            params![repo, env, target.max(0)],
        )?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use crate::db::Db;
    use crate::store::PoolStore;

    #[test]
    fn pool_spare_by_name_round_trips_states() {
        let db = Db::open_memory().unwrap();
        assert!(db.pool_spare_by_name("nope").unwrap().is_none());

        db.insert_pool_spare("repo-pool-1", "/repo", "sprites")
            .unwrap();
        let row = db.pool_spare_by_name("repo-pool-1").unwrap().unwrap();
        assert_eq!(row.state, "provisioning");
        assert_eq!(row.repo_path, "/repo");
        assert_eq!(row.env_name, "sprites");
        assert!(row.checkpoint_id.is_none());

        db.set_pool_spare_ready("repo-pool-1", Some("cp-1"), "lock-a")
            .unwrap();
        let row = db.pool_spare_by_name("repo-pool-1").unwrap().unwrap();
        assert_eq!(row.state, "ready");
        assert_eq!(row.checkpoint_id.as_deref(), Some("cp-1"));
        assert_eq!(row.lock_hash.as_deref(), Some("lock-a"));

        db.claim_pool_spare("/repo", "sprites", "/wt/x").unwrap();
        let row = db.pool_spare_by_name("repo-pool-1").unwrap().unwrap();
        assert_eq!(row.state, "claimed");
        assert_eq!(row.checkpoint_id.as_deref(), Some("cp-1"), "id kept");

        db.delete_pool_spare("repo-pool-1").unwrap();
        assert!(db.pool_spare_by_name("repo-pool-1").unwrap().is_none());
    }

    #[test]
    fn base_snapshot_round_trips_and_replaces() {
        let db = Db::open_memory().unwrap();
        assert!(db.base_snapshot("/repo", "sprites").unwrap().is_none());

        db.set_base_snapshot("/repo", "sprites", "snap-1", "lock-a")
            .unwrap();
        assert_eq!(
            db.base_snapshot("/repo", "sprites").unwrap(),
            Some(("snap-1".to_string(), "lock-a".to_string()))
        );

        // Upsert replaces the pair's prior base (new lock ⇒ new snapshot).
        db.set_base_snapshot("/repo", "sprites", "snap-2", "lock-b")
            .unwrap();
        assert_eq!(
            db.base_snapshot("/repo", "sprites").unwrap(),
            Some(("snap-2".to_string(), "lock-b".to_string()))
        );
        // Keyed per (repo, env): another env is independent.
        assert!(db.base_snapshot("/repo", "other").unwrap().is_none());
    }
}
