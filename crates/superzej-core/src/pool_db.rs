//! Warm-spare-pool DB helpers that live OUTSIDE the pinned `db.rs` (sibling
//! `impl Db` block, same pattern as `host_db.rs`): lookups the recycle paths
//! need beyond the by-(repo, env) queries `db.rs` already carries. The tests
//! here also cover the `env_base_snapshots` round trip for the same reason
//! (`db.rs` is at its size ceiling and cannot grow).

use anyhow::Result;
use rusqlite::params;

use crate::db::{Db, PoolSpare};

impl Db {
    /// The pool-spare row for one sandbox name (any state), or `None`. The
    /// worktree-delete path resolves the deleted worktree's sandbox NAME first;
    /// this answers whether that sandbox is a claimed pool spare (and carries
    /// its checkpoint + lock hash for the recycle decision) without depending
    /// on the racing `worktrees` rows.
    pub fn pool_spare_by_name(&self, name: &str) -> Result<Option<PoolSpare>> {
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
}

#[cfg(test)]
mod tests {
    use crate::db::Db;

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
