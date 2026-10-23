//! Warm-pool state (schema v26): the embedded-SQLite implementation of the
//! [`PoolStore`] seam — spare sandboxes (`pool_spares`), per-`(repo, env)` base
//! snapshots (`env_base_snapshots`), and the runtime pool-target override
//! (`pool_targets`). Sibling `impl` block (via the `conn()` accessor) so the
//! pinned `db.rs` only carries the schema DDL, not these bodies.

use anyhow::{Context, Result};
use rusqlite::params;

use crate::db::{Db, PoolSpare};
use crate::store::PoolStore;
use crate::util;

fn pick_ready_spare(
    tx: &rusqlite::Transaction<'_>,
    repo: &str,
    env: &str,
) -> Option<(String, Option<String>)> {
    let res = tx.query_row(
        "SELECT sandbox_name, checkpoint_id FROM pool_spares
         WHERE repo_path=?1 AND env_name=?2 AND state='ready'
         ORDER BY created_at ASC LIMIT 1",
        params![repo, env],
        |r| Ok((r.get::<_, String>(0)?, r.get::<_, Option<String>>(1)?)),
    );
    match res {
        Ok(row) => Some(row),
        Err(rusqlite::Error::QueryReturnedNoRows) => None,
        Err(error) => {
            tracing::warn!(target: "thegn::db", error = %error, "pick_spare read failed; treating as a miss");
            None
        }
    }
}

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
        let res = self
            .conn()
            .query_row(
                "SELECT snapshot_id, lock_hash FROM env_base_snapshots WHERE repo_path=?1 AND env_name=?2",
                params![repo_path, env_name],
                |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)),
            );
        // best-effort read: no rows is the None case; a real DB error is
        // surfaced but still degrades to a miss (the snapshot re-runs).
        let r = match res {
            Ok(v) => Some(v),
            Err(rusqlite::Error::QueryReturnedNoRows) => None,
            Err(e) => {
                tracing::warn!(target: "thegn::db", error = %e, "base_snapshot read failed; treating as a miss");
                None
            }
        };
        Ok(r)
    }

    fn insert_pool_spare(&self, name: &str, repo: &str, env: &str) -> Result<()> {
        let now = util::now();
        self.conn()
            .execute(
                "INSERT INTO pool_spares
                   (sandbox_name,repo_path,env_name,state,checkpoint_id,lock_hash,created_at,updated_at)
                 VALUES(?1,?2,?3,'provisioning',NULL,NULL,?4,?4)",
                params![name, repo, env, now],
            )
            .with_context(|| format!("warm pool: spare {name} already has a ledger row"))?;
        Ok(())
    }

    fn set_pool_spare_ready(
        &self,
        name: &str,
        checkpoint_id: Option<&str>,
        lock_hash: &str,
    ) -> Result<()> {
        let changed = self.conn().execute(
            "UPDATE pool_spares SET state='ready', checkpoint_id=?2, lock_hash=?3, updated_at=?4
             WHERE sandbox_name=?1",
            params![name, checkpoint_id, lock_hash, util::now()],
        )?;
        if changed != 1 {
            return Err(anyhow::anyhow!(
                "warm pool: spare {name} has no ledger row to mark ready"
            ));
        }
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
            .collect::<rusqlite::Result<Vec<_>>>()
            .with_context(|| {
                format!("warm pool: undecodable spare ledger row for {repo} ({env})")
            })?;
        Ok(rows)
    }

    fn claim_pool_spare(
        &self,
        repo: &str,
        env: &str,
        worktree: &str,
    ) -> Result<Option<(String, Option<String>)>> {
        let tx = self.conn().unchecked_transaction()?;
        let picked = pick_ready_spare(&tx, repo, env);
        if let Some((ref name, _)) = picked {
            let begun = tx.execute(
                "UPDATE pool_spares SET state='claiming', updated_at=?2
                 WHERE sandbox_name=?1 AND state='ready'",
                params![name, util::now()],
            )?;
            if begun != 1 {
                return Err(anyhow::anyhow!(
                    "warm pool: spare {name} was not exactly one ready ledger row to begin claiming"
                ));
            }
            let published = tx.execute(
                "UPDATE pool_spares SET state='claimed', updated_at=?2
                 WHERE sandbox_name=?1 AND state='claiming'",
                params![name, util::now()],
            )?;
            if published != 1 {
                return Err(anyhow::anyhow!(
                    "warm pool: spare {name} was not exactly one claiming ledger row to publish"
                ));
            }
            tx.execute(
                "UPDATE worktrees SET provider_sandbox_id=?2 WHERE worktree=?1",
                params![worktree, name],
            )?;
        }
        tx.commit()?;
        Ok(picked)
    }

    fn begin_pool_spare_claim(
        &self,
        repo: &str,
        env: &str,
    ) -> Result<Option<(String, Option<String>)>> {
        let tx = self.conn().unchecked_transaction()?;
        let picked = pick_ready_spare(&tx, repo, env);
        if let Some((name, _)) = &picked {
            let changed = tx.execute(
                "UPDATE pool_spares SET state='claiming', updated_at=?2
                 WHERE sandbox_name=?1 AND state='ready'",
                params![name, util::now()],
            )?;
            if changed != 1 {
                return Err(anyhow::anyhow!(
                    "warm pool: spare {name} was not exactly one ready ledger row to begin claiming"
                ));
            }
        }
        tx.commit()?;
        Ok(picked)
    }

    fn publish_pool_spare_claim(&self, name: &str, worktree: &str) -> Result<()> {
        let tx = self.conn().unchecked_transaction()?;
        let changed = tx.execute(
            "UPDATE pool_spares SET state='claimed', updated_at=?2
             WHERE sandbox_name=?1 AND state='claiming'",
            params![name, util::now()],
        )?;
        if changed != 1 {
            return Err(anyhow::anyhow!(
                "warm pool: spare {name} was not exactly one claiming ledger row to publish"
            ));
        }
        let bound = tx.execute(
            "UPDATE worktrees SET provider_sandbox_id=?2 WHERE worktree=?1",
            params![worktree, name],
        )?;
        if bound != 1 {
            return Err(anyhow::anyhow!(
                "warm pool: worktree {worktree} was not exactly one row to bind spare {name}"
            ));
        }
        tx.commit()?;
        Ok(())
    }

    fn quarantine_pool_spare(&self, name: &str, expected_state: &str) -> Result<()> {
        let changed = self.conn().execute(
            "UPDATE pool_spares SET state='quarantined', updated_at=?3
             WHERE sandbox_name=?1 AND state=?2",
            params![name, expected_state, util::now()],
        )?;
        if changed != 1 {
            return Err(anyhow::anyhow!(
                "warm pool: spare {name} was not exactly one {expected_state} ledger row to quarantine"
            ));
        }
        Ok(())
    }

    fn pool_spare_by_name(&self, name: &str) -> Result<Option<PoolSpare>> {
        let res = self.conn().query_row(
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
        );
        let r = match res {
            Ok(v) => Some(v),
            Err(rusqlite::Error::QueryReturnedNoRows) => return Ok(None),
            Err(error) => {
                return Err(anyhow::Error::new(error)
                    .context(format!("warm pool: cannot read spare ledger row {name}")));
            }
        };
        Ok(r)
    }

    fn worktree_provider_sandbox(&self, worktree: &str) -> Result<Option<String>> {
        let result = self.conn().query_row(
            "SELECT provider_sandbox_id FROM worktrees WHERE worktree=?1",
            params![worktree],
            |r| r.get::<_, Option<String>>(0),
        );
        match result {
            Ok(Some(sandbox)) if !sandbox.is_empty() => Ok(Some(sandbox)),
            Ok(_) | Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(error) => Err(anyhow::Error::new(error).context(format!(
                "warm pool: cannot read the provider sandbox binding for {worktree}"
            ))),
        }
    }

    fn unattended_pool_spares(&self) -> Result<Vec<PoolSpare>> {
        let mut stmt = self.conn().prepare(
            "SELECT s.sandbox_name,s.repo_path,s.env_name,s.state,s.checkpoint_id,s.lock_hash,s.created_at,s.updated_at
             FROM pool_spares s
             WHERE s.state='quarantined' OR (s.state='claimed' AND NOT EXISTS (
                 SELECT 1 FROM worktrees w WHERE w.provider_sandbox_id=s.sandbox_name
             ))
             ORDER BY s.updated_at, s.sandbox_name",
        )?;
        let rows = stmt
            .query_map([], |r| {
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
            .collect::<rusqlite::Result<Vec<_>>>()
            .context("warm pool: undecodable spare ledger row")?;
        Ok(rows)
    }

    fn pool_target(&self, repo: &str, env: &str) -> Result<Option<i64>> {
        let res = self.conn().query_row(
            "SELECT target FROM pool_targets WHERE repo_path=?1 AND env_name=?2",
            params![repo, env],
            |r| r.get::<_, i64>(0),
        );
        match res {
            Ok(v) => Ok(Some(v)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(error) => Err(anyhow::Error::new(error).context(format!(
                "warm pool: cannot read the pool-target override for {repo} ({env})"
            ))),
        }
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
    use crate::store::{PoolStore, WorkspaceStore};

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
    fn ledger_read_undecodable_spare_row_is_an_error() {
        let db = Db::open_memory().unwrap();
        db.insert_pool_spare("good", "/repo", "sprites").unwrap();
        db.insert_pool_spare("bad", "/repo", "sprites").unwrap();
        db.conn()
            .execute(
                "UPDATE pool_spares SET created_at='not-a-number' WHERE sandbox_name='bad'",
                [],
            )
            .unwrap();

        assert!(db.pool_spares_for("/repo", "sprites").is_err());
    }

    #[test]
    fn unattended_spares_list_a_claimed_row_whose_worktree_row_is_gone() {
        let db = Db::open_memory().unwrap();
        db.put_worktree("tab", "/repo", "/wt/x", "tg/x", None, None)
            .unwrap();
        db.insert_pool_spare("s1", "/repo", "sprites").unwrap();
        db.set_pool_spare_ready("s1", Some("cp"), "lock").unwrap();

        assert!(
            db.claim_pool_spare("/repo", "sprites", "/wt/x")
                .unwrap()
                .is_some()
        );
        assert!(db.unattended_pool_spares().unwrap().is_empty());

        db.del_worktree("/wt/x").unwrap();
        let rows = db.unattended_pool_spares().unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].sandbox_name, "s1");
        assert_eq!(rows[0].state, "claimed");
    }

    #[test]
    fn unattended_spares_list_quarantined_rows() {
        let db = Db::open_memory().unwrap();
        db.insert_pool_spare("q1", "/repo", "sprites").unwrap();
        db.set_pool_spare_ready("q1", Some("cp"), "lock").unwrap();
        assert!(
            db.begin_pool_spare_claim("/repo", "sprites")
                .unwrap()
                .is_some()
        );
        db.quarantine_pool_spare("q1", "claiming").unwrap();

        let rows = db.unattended_pool_spares().unwrap();
        assert_eq!(
            rows.iter()
                .map(|row| (row.sandbox_name.as_str(), row.state.as_str()))
                .collect::<Vec<_>>(),
            [("q1", "quarantined")]
        );
    }

    #[test]
    fn unattended_spares_skip_ready_provisioning_claiming_and_bound_rows() {
        let db = Db::open_memory().unwrap();
        db.insert_pool_spare("provisioning", "/repo", "sprites")
            .unwrap();
        db.insert_pool_spare("ready", "/r1", "sprites").unwrap();
        db.set_pool_spare_ready("ready", None, "lock").unwrap();
        db.insert_pool_spare("claiming", "/r2", "sprites").unwrap();
        db.set_pool_spare_ready("claiming", None, "lock").unwrap();
        assert!(
            db.begin_pool_spare_claim("/r2", "sprites")
                .unwrap()
                .is_some()
        );
        db.put_worktree("tab", "/repo", "/wt/x", "tg/x", None, None)
            .unwrap();
        db.insert_pool_spare("bound", "/repo", "sprites").unwrap();
        db.set_pool_spare_ready("bound", None, "lock").unwrap();
        assert!(
            db.claim_pool_spare("/repo", "sprites", "/wt/x")
                .unwrap()
                .is_some()
        );

        assert!(db.unattended_pool_spares().unwrap().is_empty());
    }

    #[test]
    fn unattended_spares_order_oldest_first() {
        let db = Db::open_memory().unwrap();
        for name in ["a", "b"] {
            db.insert_pool_spare(name, "/repo", "sprites").unwrap();
            db.quarantine_pool_spare(name, "provisioning").unwrap();
        }
        db.conn()
            .execute(
                "UPDATE pool_spares SET updated_at=?1 WHERE sandbox_name=?2",
                rusqlite::params![200, "a"],
            )
            .unwrap();
        db.conn()
            .execute(
                "UPDATE pool_spares SET updated_at=?1 WHERE sandbox_name=?2",
                rusqlite::params![100, "b"],
            )
            .unwrap();

        assert_eq!(
            db.unattended_pool_spares()
                .unwrap()
                .iter()
                .map(|row| row.sandbox_name.as_str())
                .collect::<Vec<_>>(),
            ["b", "a"]
        );
    }

    #[test]
    fn unattended_spares_undecodable_row_is_an_error() {
        let db = Db::open_memory().unwrap();
        db.insert_pool_spare("bad", "/repo", "sprites").unwrap();
        db.quarantine_pool_spare("bad", "provisioning").unwrap();
        db.conn()
            .execute(
                "UPDATE pool_spares SET created_at='not-a-number' WHERE sandbox_name='bad'",
                [],
            )
            .unwrap();

        assert!(db.unattended_pool_spares().is_err());
    }

    #[test]
    fn ledger_read_spare_by_name_db_error_is_an_error() {
        let db = Db::open_memory().unwrap();
        db.conn().execute("DROP TABLE pool_spares", []).unwrap();

        assert!(db.pool_spare_by_name("x").is_err());
    }

    #[test]
    fn ledger_read_binding_db_error_is_an_error_and_missing_is_none() {
        let db = Db::open_memory().unwrap();
        assert!(db.worktree_provider_sandbox("/wt/none").unwrap().is_none());

        db.conn().execute("DROP TABLE worktrees", []).unwrap();
        assert!(db.worktree_provider_sandbox("/wt/none").is_err());
    }

    #[test]
    fn pool_target_db_error_is_an_error_and_missing_is_none() {
        let db = Db::open_memory().unwrap();
        assert_eq!(db.pool_target("/repo", "sprites").unwrap(), None);

        db.conn().execute("DROP TABLE pool_targets", []).unwrap();
        assert!(db.pool_target("/repo", "sprites").is_err());
    }

    #[test]
    fn ready_spare_pick_query_errors_are_misses() {
        let db = Db::open_memory().unwrap();
        db.conn().execute("DROP TABLE pool_spares", []).unwrap();

        assert!(
            db.begin_pool_spare_claim("/repo", "sprites")
                .unwrap()
                .is_none()
        );
        assert!(
            db.claim_pool_spare("/repo", "sprites", "/wt/x")
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn spare_ledger_insert_refuses_existing_name() {
        let db = Db::open_memory().unwrap();
        db.insert_pool_spare("x", "/repo", "sprites").unwrap();
        db.set_pool_spare_ready("x", Some("checkpoint-x"), "lock-x")
            .unwrap();

        assert!(db.insert_pool_spare("x", "/other", "other-env").is_err());

        let row = db.pool_spare_by_name("x").unwrap().unwrap();
        assert_eq!(row.state, "ready");
        assert_eq!(row.repo_path, "/repo");
        assert_eq!(row.env_name, "sprites");
        assert_eq!(row.checkpoint_id.as_deref(), Some("checkpoint-x"));
        assert_eq!(row.lock_hash.as_deref(), Some("lock-x"));
    }

    #[test]
    fn spare_ledger_ready_requires_one_row() {
        let db = Db::open_memory().unwrap();
        assert!(
            db.set_pool_spare_ready("missing", Some("cp"), "lock")
                .is_err()
        );
    }

    #[test]
    fn spare_ledger_claim_updates_exactly_one_ready_row() {
        let db = Db::open_memory().unwrap();
        db.insert_pool_spare("first", "/repo", "sprites").unwrap();
        db.insert_pool_spare("second", "/repo", "sprites").unwrap();
        db.set_pool_spare_ready("first", Some("cp-first"), "lock")
            .unwrap();
        db.set_pool_spare_ready("second", Some("cp-second"), "lock")
            .unwrap();

        let claimed = db
            .claim_pool_spare("/repo", "sprites", "/wt/x")
            .unwrap()
            .unwrap();

        let first = db.pool_spare_by_name("first").unwrap().unwrap();
        let second = db.pool_spare_by_name("second").unwrap().unwrap();
        let claimed_count = [&first.state, &second.state]
            .into_iter()
            .filter(|state| state.as_str() == "claimed")
            .count();
        assert_eq!(claimed_count, 1);
        let other = if claimed.0 == "first" { second } else { first };
        assert_eq!(other.state, "ready");
    }

    #[test]
    fn spare_ledger_claim_refuses_when_update_changes_no_rows() {
        let db = Db::open_memory().unwrap();
        db.insert_pool_spare("held", "/repo", "sprites").unwrap();
        db.set_pool_spare_ready("held", Some("cp"), "lock").unwrap();
        db.conn()
            .execute_batch(
                "CREATE TRIGGER keep_spare_ready BEFORE UPDATE ON pool_spares
                 WHEN OLD.sandbox_name = 'held'
                 BEGIN SELECT RAISE(IGNORE); END;",
            )
            .unwrap();

        assert!(db.claim_pool_spare("/repo", "sprites", "/wt/x").is_err());
        assert_eq!(
            db.pool_spare_by_name("held").unwrap().unwrap().state,
            "ready"
        );
    }

    #[test]
    fn begin_claim_moves_ready_to_claiming_without_binding() {
        let db = Db::open_memory().unwrap();
        db.insert_pool_spare("claiming", "/repo", "sprites")
            .unwrap();
        db.set_pool_spare_ready("claiming", Some("cp"), "lock")
            .unwrap();
        db.put_worktree("tab", "/repo", "/wt/x", "tg/x", None, None)
            .unwrap();

        let picked = db
            .begin_pool_spare_claim("/repo", "sprites")
            .unwrap()
            .unwrap();
        assert_eq!(picked, ("claiming".to_string(), Some("cp".to_string())));
        assert_eq!(
            db.pool_spare_by_name("claiming").unwrap().unwrap().state,
            "claiming"
        );
        assert_eq!(db.worktree_provider_sandbox("/wt/x").unwrap(), None);

        db.publish_pool_spare_claim(&picked.0, "/wt/x").unwrap();
        assert_eq!(
            db.worktree_provider_sandbox("/wt/x").unwrap().as_deref(),
            Some("claiming")
        );
    }

    #[test]
    fn publish_requires_a_claiming_row_and_binds_once() {
        let db = Db::open_memory().unwrap();
        db.insert_pool_spare("claiming", "/repo", "sprites")
            .unwrap();
        db.set_pool_spare_ready("claiming", Some("cp"), "lock")
            .unwrap();
        assert!(db.publish_pool_spare_claim("claiming", "/wt/x").is_err());

        db.begin_pool_spare_claim("/repo", "sprites").unwrap();
        assert!(
            db.publish_pool_spare_claim("claiming", "/wt/missing")
                .is_err()
        );
        assert_eq!(
            db.pool_spare_by_name("claiming").unwrap().unwrap().state,
            "claiming"
        );
        db.put_worktree("tab", "/repo", "/wt/x", "tg/x", None, None)
            .unwrap();
        db.publish_pool_spare_claim("claiming", "/wt/x").unwrap();
        assert_eq!(
            db.pool_spare_by_name("claiming").unwrap().unwrap().state,
            "claimed"
        );
        assert!(db.publish_pool_spare_claim("claiming", "/wt/x").is_err());
    }

    #[test]
    fn quarantine_requires_the_expected_state() {
        let db = Db::open_memory().unwrap();
        db.insert_pool_spare("claiming", "/repo", "sprites")
            .unwrap();
        db.set_pool_spare_ready("claiming", Some("cp"), "lock")
            .unwrap();
        db.begin_pool_spare_claim("/repo", "sprites").unwrap();

        assert!(db.quarantine_pool_spare("claiming", "ready").is_err());
        assert_eq!(
            db.pool_spare_by_name("claiming").unwrap().unwrap().state,
            "claiming"
        );
        db.quarantine_pool_spare("claiming", "claiming").unwrap();
        assert_eq!(
            db.pool_spare_by_name("claiming").unwrap().unwrap().state,
            "quarantined"
        );
        assert!(
            db.begin_pool_spare_claim("/repo", "sprites")
                .unwrap()
                .is_none()
        );
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
