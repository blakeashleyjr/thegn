//! LLM-proxy state (schema **v13**): `proxy_health` (exhaustion markers),
//! `proxy_requests` (per-request audit log; never bodies), `proxy_virtual_keys`
//! (per-agent keys → upstream + scope), and `proxy_budgets` (per-scope spend).
//!
//! This is the embedded-SQLite implementation of the [`ProxyStore`] seam. It is
//! a sibling `impl` block (using the `pub(crate) conn()` accessor) so the pinned
//! `db.rs` only carries the schema DDL + version bump, not these method bodies.
//! Consumers (the `superzej-proxy` daemon) depend on the trait, so a future
//! server backend can supply its own `impl ProxyStore` without touching them.
//!
//! Timestamps are caller-supplied epoch-millis so core stays free of wall-clock
//! coupling — the proxy supplies real values from chrono.

use anyhow::Result;
use rusqlite::{OptionalExtension, params};

use crate::db::{Db, ProxyBudgetRow, ProxyHealthRow, ProxyRequestRow};
use crate::store::ProxyStore;

impl ProxyStore for Db {
    #[allow(clippy::too_many_arguments)]
    fn put_proxy_health(
        &self,
        backend: &str,
        model: &str,
        kind: &str,
        reason: &str,
        since_ms: i64,
        next_probe_ms: i64,
        is_stale: bool,
        consecutive_failures: i64,
        cred_file: Option<&str>,
        cred_mtime_ms: Option<i64>,
    ) -> Result<()> {
        self.conn().execute(
            r#"INSERT INTO proxy_health
                 (backend,model,kind,reason,since_ms,next_probe_ms,is_stale,
                  consecutive_failures,cred_file,cred_mtime_ms)
               VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10)
               ON CONFLICT(backend,model) DO UPDATE SET
                 kind=?3, reason=?4, since_ms=?5, next_probe_ms=?6, is_stale=?7,
                 consecutive_failures=?8, cred_file=?9, cred_mtime_ms=?10"#,
            params![
                backend,
                model,
                kind,
                reason,
                since_ms,
                next_probe_ms,
                is_stale as i64,
                consecutive_failures,
                cred_file,
                cred_mtime_ms
            ],
        )?;
        Ok(())
    }

    fn clear_proxy_health(&self, backend: &str, model: &str) -> Result<()> {
        self.conn().execute(
            "DELETE FROM proxy_health WHERE backend=?1 AND model=?2",
            params![backend, model],
        )?;
        Ok(())
    }

    #[allow(clippy::type_complexity)]
    fn load_proxy_health(&self, now_ms: i64) -> Result<Vec<ProxyHealthRow>> {
        let mut stmt = self.conn().prepare(
            r#"SELECT backend,model,kind,reason,since_ms,next_probe_ms,is_stale,
                      consecutive_failures,cred_file,cred_mtime_ms
               FROM proxy_health WHERE next_probe_ms > ?1"#,
        )?;
        let rows = stmt
            .query_map(params![now_ms], |r| {
                Ok(ProxyHealthRow {
                    backend: r.get(0)?,
                    model: r.get(1)?,
                    kind: r.get(2)?,
                    reason: r.get(3)?,
                    since_ms: r.get(4)?,
                    next_probe_ms: r.get(5)?,
                    is_stale: r.get::<_, i64>(6)? != 0,
                    consecutive_failures: r.get(7)?,
                    cred_file: r.get(8)?,
                    cred_mtime_ms: r.get(9)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    fn put_proxy_request(&self, r: &ProxyRequestRow) -> Result<i64> {
        self.conn().execute(
            r#"INSERT INTO proxy_requests
                 (ts_ms,protocol,route,virtual_key,agent,worktree,workspace,
                  client_model,backend,backend_model,input_tokens,output_tokens,
                  cost_usd,cost_source,outcome,error_code)
               VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16)"#,
            params![
                r.ts_ms,
                r.protocol,
                r.route,
                r.virtual_key,
                r.agent,
                r.worktree,
                r.workspace,
                r.client_model,
                r.backend,
                r.backend_model,
                r.input_tokens,
                r.output_tokens,
                r.cost_usd,
                r.cost_source,
                r.outcome,
                r.error_code,
            ],
        )?;
        Ok(self.conn().last_insert_rowid())
    }

    fn proxy_requests(&self, worktree: &str, limit: usize) -> Result<Vec<ProxyRequestRow>> {
        let mut stmt = self.conn().prepare(
            "SELECT ts_ms,protocol,route,virtual_key,agent,worktree,workspace,
                    client_model,backend,backend_model,input_tokens,output_tokens,
                    cost_usd,cost_source,outcome,error_code
             FROM proxy_requests
             WHERE worktree = ?1
             ORDER BY ts_ms DESC
             LIMIT ?2",
        )?;
        let rows = stmt.query_map(params![worktree, limit as i64], |r| {
            Ok(ProxyRequestRow {
                ts_ms: r.get(0)?,
                protocol: r.get(1)?,
                route: r.get(2)?,
                virtual_key: r.get(3)?,
                agent: r.get(4)?,
                worktree: r.get(5)?,
                workspace: r.get(6)?,
                client_model: r.get(7)?,
                backend: r.get(8)?,
                backend_model: r.get(9)?,
                input_tokens: r.get(10)?,
                output_tokens: r.get(11)?,
                cost_usd: r.get(12)?,
                cost_source: r.get(13)?,
                outcome: r.get(14)?,
                error_code: r.get(15)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    fn proxy_spend_since(&self, worktree: &str, since_ms: i64) -> Result<f64> {
        Ok(self.conn().query_row(
            "SELECT COALESCE(SUM(cost_usd), 0.0) FROM proxy_requests
             WHERE worktree = ?1 AND ts_ms >= ?2",
            params![worktree, since_ms],
            |r| r.get(0),
        )?)
    }

    fn proxy_virtual_key(&self, key_id: &str) -> Result<Option<(String, Option<String>)>> {
        Ok(self
            .conn()
            .query_row(
                "SELECT scope, upstream FROM proxy_virtual_keys \
                 WHERE key_id=?1 AND revoked_at IS NULL",
                params![key_id],
                |r| Ok((r.get::<_, String>(0)?, r.get::<_, Option<String>>(1)?)),
            )
            .optional()?)
    }

    fn put_proxy_virtual_key(
        &self,
        key_id: &str,
        token_hash: &str,
        label: &str,
        scope: &str,
        upstream: Option<&str>,
        now_ms: i64,
    ) -> Result<()> {
        self.conn().execute(
            r#"INSERT INTO proxy_virtual_keys
                 (key_id,token_hash,label,scope,upstream,created_at)
               VALUES(?1,?2,?3,?4,?5,?6)
               ON CONFLICT(key_id) DO UPDATE SET
                 token_hash=?2, label=?3, scope=?4, upstream=?5, revoked_at=NULL"#,
            params![key_id, token_hash, label, scope, upstream, now_ms],
        )?;
        Ok(())
    }

    fn revoke_proxy_virtual_key(&self, key_id: &str, now_ms: i64) -> Result<()> {
        self.conn().execute(
            "UPDATE proxy_virtual_keys SET revoked_at=?2 WHERE key_id=?1",
            params![key_id, now_ms],
        )?;
        Ok(())
    }

    fn add_proxy_spend(
        &self,
        scope: &str,
        tokens: i64,
        cost: f64,
        now_ms: i64,
    ) -> Result<(i64, f64, bool)> {
        self.conn().execute(
            "INSERT INTO proxy_budgets(scope) VALUES(?1) ON CONFLICT(scope) DO NOTHING",
            params![scope],
        )?;
        // Roll the window over if due.
        self.conn().execute(
            "UPDATE proxy_budgets SET spent_tokens=0, spent_cost=0 \
             WHERE scope=?1 AND reset_ms>0 AND reset_ms<=?2",
            params![scope, now_ms],
        )?;
        self.conn().execute(
            "UPDATE proxy_budgets SET spent_tokens=spent_tokens+?2, spent_cost=spent_cost+?3 \
             WHERE scope=?1",
            params![scope, tokens, cost],
        )?;
        Ok(self.conn().query_row(
            "SELECT spent_tokens, spent_cost, killed FROM proxy_budgets WHERE scope=?1",
            params![scope],
            |r| {
                Ok((
                    r.get::<_, i64>(0)?,
                    r.get::<_, f64>(1)?,
                    r.get::<_, i64>(2)? != 0,
                ))
            },
        )?)
    }

    fn proxy_budget(&self, scope: &str) -> Result<Option<ProxyBudgetRow>> {
        Ok(self
            .conn()
            .query_row(
                "SELECT scope,period,spent_tokens,spent_cost,limit_tokens,limit_cost,reset_ms,killed \
                 FROM proxy_budgets WHERE scope=?1",
                params![scope],
                |r| {
                    Ok(ProxyBudgetRow {
                        scope: r.get(0)?,
                        period: r.get(1)?,
                        spent_tokens: r.get(2)?,
                        spent_cost: r.get(3)?,
                        limit_tokens: r.get(4)?,
                        limit_cost: r.get(5)?,
                        reset_ms: r.get(6)?,
                        killed: r.get::<_, i64>(7)? != 0,
                    })
                },
            )
            .optional()?)
    }

    fn set_proxy_budget_limits(
        &self,
        scope: &str,
        period: &str,
        limit_tokens: Option<i64>,
        limit_cost: Option<f64>,
        reset_ms: i64,
    ) -> Result<()> {
        self.conn().execute(
            r#"INSERT INTO proxy_budgets(scope,period,limit_tokens,limit_cost,reset_ms)
               VALUES(?1,?2,?3,?4,?5)
               ON CONFLICT(scope) DO UPDATE SET
                 period=?2, limit_tokens=?3, limit_cost=?4, reset_ms=?5"#,
            params![scope, period, limit_tokens, limit_cost, reset_ms],
        )?;
        Ok(())
    }

    fn set_proxy_kill_switch(&self, scope: &str, killed: bool) -> Result<()> {
        self.conn().execute(
            "INSERT INTO proxy_budgets(scope,killed) VALUES(?1,?2) \
             ON CONFLICT(scope) DO UPDATE SET killed=?2",
            params![scope, killed as i64],
        )?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    //! Exercises the embedded `Db` through the `&dyn ProxyStore` trait object —
    //! proving the seam is object-safe and that consumers written against the
    //! trait work against any backend (a future Postgres impl included).
    use super::*;

    #[test]
    fn proxy_store_via_trait_object() {
        let db = Db::open_memory().unwrap();
        let store: &dyn ProxyStore = &db;

        // Budget: caps + spend accumulation + kill switch, all through the trait.
        store
            .set_proxy_budget_limits("agent:x", "monthly", Some(1000), Some(5.0), 0)
            .unwrap();
        let (tokens, cost, killed) = store.add_proxy_spend("agent:x", 100, 1.5, 1).unwrap();
        assert_eq!(tokens, 100);
        assert!((cost - 1.5).abs() < f64::EPSILON);
        assert!(!killed);
        store.set_proxy_kill_switch("agent:x", true).unwrap();
        assert!(store.proxy_budget("agent:x").unwrap().unwrap().killed);

        // Virtual key: register → resolve → revoke.
        store
            .put_proxy_virtual_key("vk1", "hash", "label", "agent:x", Some("up"), 1)
            .unwrap();
        assert_eq!(
            store.proxy_virtual_key("vk1").unwrap(),
            Some(("agent:x".into(), Some("up".into())))
        );
        store.revoke_proxy_virtual_key("vk1", 2).unwrap();
        assert!(store.proxy_virtual_key("vk1").unwrap().is_none());

        // Health markers: live rows load back; cleared rows do not.
        store
            .put_proxy_health(
                "anthropic",
                "opus",
                "quota",
                "429",
                1,
                10_000,
                false,
                3,
                None,
                None,
            )
            .unwrap();
        assert_eq!(store.load_proxy_health(0).unwrap().len(), 1);
        store.clear_proxy_health("anthropic", "opus").unwrap();
        assert!(store.load_proxy_health(0).unwrap().is_empty());

        // Audit log: append → query → spend rollup.
        let row1 = ProxyRequestRow {
            ts_ms: 100,
            worktree: Some("/wt".into()),
            cost_usd: 2.0,
            ..Default::default()
        };
        assert!(store.put_proxy_request(&row1).unwrap() > 0);
        let row2 = ProxyRequestRow {
            ts_ms: 200,
            worktree: Some("/wt".into()),
            cost_usd: 3.0,
            ..Default::default()
        };
        store.put_proxy_request(&row2).unwrap();
        assert_eq!(store.proxy_requests("/wt", 10).unwrap().len(), 2);
        assert!((store.proxy_spend_since("/wt", 0).unwrap() - 5.0).abs() < f64::EPSILON);
    }
}
