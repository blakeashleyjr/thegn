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

use crate::db::{Db, ProxyBudgetRow, ProxyHealthRow, ProxyRequestRow, ProxyVirtualKeyRow};
use crate::store::{ProxyStore, budget_period_len_ms};

/// Shared SELECT prefix for `proxy_requests` readers (column order must match
/// [`request_row`]).
const REQUEST_COLS: &str = "SELECT ts_ms,protocol,route,virtual_key,agent,worktree,workspace,
        client_model,backend,backend_model,input_tokens,output_tokens,
        cost_usd,cost_source,outcome,error_code,duration_ms,ttfb_ms
 FROM proxy_requests";

fn request_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<ProxyRequestRow> {
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
        duration_ms: r.get(16)?,
        ttfb_ms: r.get(17)?,
    })
}

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
                  cost_usd,cost_source,outcome,error_code,duration_ms,ttfb_ms)
               VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17,?18)"#,
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
                r.duration_ms,
                r.ttfb_ms,
            ],
        )?;
        Ok(self.conn().last_insert_rowid())
    }

    fn proxy_requests(&self, worktree: &str, limit: usize) -> Result<Vec<ProxyRequestRow>> {
        let mut stmt = self.conn().prepare(&format!(
            "{REQUEST_COLS} WHERE worktree = ?1 ORDER BY ts_ms DESC LIMIT ?2"
        ))?;
        let rows = stmt.query_map(params![worktree, limit as i64], request_row)?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    fn proxy_requests_since(&self, since_ms: i64, limit: usize) -> Result<Vec<ProxyRequestRow>> {
        let mut stmt = self.conn().prepare(&format!(
            "{REQUEST_COLS} WHERE ts_ms >= ?1 ORDER BY ts_ms DESC LIMIT ?2"
        ))?;
        let rows = stmt.query_map(params![since_ms, limit as i64], request_row)?;
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
        // Roll the window over if due: zero the counters AND advance the anchor
        // by whole periods, so the new window accumulates instead of resetting
        // on every subsequent request (a stale anchor stays <= now forever).
        let due: Option<(String, i64)> = self
            .conn()
            .query_row(
                "SELECT period, reset_ms FROM proxy_budgets \
                 WHERE scope=?1 AND reset_ms>0 AND reset_ms<=?2",
                params![scope, now_ms],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        if let Some((period, reset_ms)) = due {
            let len = budget_period_len_ms(&period);
            let next = reset_ms + ((now_ms - reset_ms) / len + 1) * len;
            self.conn().execute(
                "UPDATE proxy_budgets SET spent_tokens=0, spent_cost=0, reset_ms=?2 \
                 WHERE scope=?1",
                params![scope, next],
            )?;
        }
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

    fn proxy_budgets_all(&self) -> Result<Vec<ProxyBudgetRow>> {
        let mut stmt = self.conn().prepare(
            "SELECT scope,period,spent_tokens,spent_cost,limit_tokens,limit_cost,reset_ms,killed \
             FROM proxy_budgets ORDER BY scope",
        )?;
        let rows = stmt
            .query_map([], |r| {
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
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    fn proxy_virtual_keys_all(&self) -> Result<Vec<ProxyVirtualKeyRow>> {
        let mut stmt = self.conn().prepare(
            "SELECT key_id,label,scope,upstream,created_at,revoked_at \
             FROM proxy_virtual_keys ORDER BY created_at DESC",
        )?;
        let rows = stmt
            .query_map([], |r| {
                Ok(ProxyVirtualKeyRow {
                    key_id: r.get(0)?,
                    label: r.get(1)?,
                    scope: r.get(2)?,
                    upstream: r.get(3)?,
                    created_at: r.get(4)?,
                    revoked_at: r.get(5)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
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
            duration_ms: 1200,
            ttfb_ms: Some(300),
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

        // Cross-caller feed (the /stats source): latency round-trips, newest first.
        let since = store.proxy_requests_since(150, 10).unwrap();
        assert_eq!(since.len(), 1);
        assert_eq!(since[0].ts_ms, 200);
        let all = store.proxy_requests_since(0, 10).unwrap();
        assert_eq!(all[0].ts_ms, 200);
        assert_eq!(all[1].duration_ms, 1200);
        assert_eq!(all[1].ttfb_ms, Some(300));
    }

    #[test]
    fn budget_window_rollover_advances_anchor() {
        let db = Db::open_memory().unwrap();
        let store: &dyn ProxyStore = &db;
        // Daily window anchored at t=1000ms.
        store
            .set_proxy_budget_limits("agent:x", "daily", Some(100), None, 1000)
            .unwrap();
        store.add_proxy_spend("agent:x", 40, 0.1, 500).unwrap();
        assert_eq!(
            store.proxy_budget("agent:x").unwrap().unwrap().spent_tokens,
            40
        );

        // Spend after the anchor: counters zero first, anchor advances by whole
        // periods past `now` — so the NEXT spend accumulates instead of the
        // window resetting on every call (the pre-v43 bug).
        let day = crate::store::budget_period_len_ms("daily");
        let now = 1000 + day + day / 2; // 1.5 periods past the anchor
        let (tokens, _, _) = store.add_proxy_spend("agent:x", 10, 0.2, now).unwrap();
        assert_eq!(tokens, 10, "old spend dropped, new window counts fresh");
        let b = store.proxy_budget("agent:x").unwrap().unwrap();
        assert_eq!(
            b.reset_ms,
            1000 + 2 * day,
            "anchor advanced by whole periods"
        );
        let (tokens, _, _) = store.add_proxy_spend("agent:x", 5, 0.0, now + 1).unwrap();
        assert_eq!(tokens, 15, "same window accumulates");
    }

    #[test]
    fn budgets_and_virtual_keys_list_all() {
        let db = Db::open_memory().unwrap();
        let store: &dyn ProxyStore = &db;
        store
            .set_proxy_budget_limits("workspace:/r", "weekly", None, Some(2.5), 0)
            .unwrap();
        store.add_proxy_spend("global", 5, 0.1, 1).unwrap();
        let budgets = store.proxy_budgets_all().unwrap();
        let scopes: Vec<&str> = budgets.iter().map(|b| b.scope.as_str()).collect();
        assert_eq!(scopes, vec!["global", "workspace:/r"]);
        assert_eq!(budgets[1].period, "weekly");
        assert_eq!(budgets[1].limit_cost, Some(2.5));

        store
            .put_proxy_virtual_key("k1", "h1", "one", "worktree:/a", Some("nano-gpt"), 10)
            .unwrap();
        store
            .put_proxy_virtual_key("k2", "h2", "two", "global", None, 20)
            .unwrap();
        store.revoke_proxy_virtual_key("k1", 30).unwrap();
        let keys = store.proxy_virtual_keys_all().unwrap();
        assert_eq!(keys.len(), 2);
        // Newest first; revocation is metadata, not deletion.
        assert_eq!(keys[0].key_id, "k2");
        assert!(keys[0].revoked_at.is_none());
        assert_eq!(keys[1].key_id, "k1");
        assert_eq!(keys[1].upstream.as_deref(), Some("nano-gpt"));
        assert!(keys[1].revoked_at.is_some());
    }
}
