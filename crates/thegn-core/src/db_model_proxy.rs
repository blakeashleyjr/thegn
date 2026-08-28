//! SQLite persistence for the model proxy's accounting tables (v54).
//!
//! Two fresh tables — `model_proxy_requests` (metadata-only audit rows) and
//! `model_proxy_budget_state` (per-scope rolling-window accumulators). The
//! orphaned pre-alpha `proxy_*` tables are never referenced or dropped here.

use anyhow::Result;
use rusqlite::{Connection, OptionalExtension, params};

use crate::db::Db;
use crate::store::{ModelProxyBudgetStateRow, ModelProxyRequestRow, ModelProxyStore};

/// Create the model-proxy accounting tables. Idempotent, so re-running (fresh DB
/// or upgrade) is a no-op. No column stores message content.
pub(crate) fn migrate_v54(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        r#"
        CREATE TABLE IF NOT EXISTS model_proxy_requests (
            id                    INTEGER PRIMARY KEY AUTOINCREMENT,
            ts_ms                 INTEGER NOT NULL,
            protocol              TEXT NOT NULL DEFAULT '',
            route                 TEXT NOT NULL DEFAULT '',
            agent                 TEXT,
            worktree              TEXT,
            workspace             TEXT,
            client_model          TEXT NOT NULL DEFAULT '',
            backend               TEXT NOT NULL DEFAULT '',
            backend_model         TEXT NOT NULL DEFAULT '',
            input_tokens          INTEGER NOT NULL DEFAULT 0,
            output_tokens         INTEGER NOT NULL DEFAULT 0,
            cache_read_tokens     INTEGER NOT NULL DEFAULT 0,
            cache_creation_tokens INTEGER NOT NULL DEFAULT 0,
            cost_usd              REAL NOT NULL DEFAULT 0,
            cost_source           TEXT NOT NULL DEFAULT '',
            outcome               TEXT NOT NULL DEFAULT '',
            error_code            TEXT,
            duration_ms           INTEGER NOT NULL DEFAULT 0,
            ttfb_ms               INTEGER
        );
        CREATE INDEX IF NOT EXISTS model_proxy_requests_ts
            ON model_proxy_requests(ts_ms);

        CREATE TABLE IF NOT EXISTS model_proxy_budget_state (
            scope           TEXT PRIMARY KEY,
            window_start_ms INTEGER NOT NULL DEFAULT 0,
            spent_tokens    INTEGER NOT NULL DEFAULT 0,
            spent_cost      REAL NOT NULL DEFAULT 0,
            killed          INTEGER NOT NULL DEFAULT 0
        );
        "#,
    )?;
    Ok(())
}

fn budget_row(r: &rusqlite::Row) -> rusqlite::Result<ModelProxyBudgetStateRow> {
    Ok(ModelProxyBudgetStateRow {
        scope: r.get(0)?,
        window_start_ms: r.get(1)?,
        spent_tokens: r.get(2)?,
        spent_cost: r.get(3)?,
        killed: r.get::<_, i64>(4)? != 0,
    })
}

impl ModelProxyStore for Db {
    fn put_model_proxy_request(&self, row: &ModelProxyRequestRow) -> Result<()> {
        self.conn().execute(
            r#"INSERT INTO model_proxy_requests
               (ts_ms, protocol, route, agent, worktree, workspace, client_model,
                backend, backend_model, input_tokens, output_tokens,
                cache_read_tokens, cache_creation_tokens, cost_usd, cost_source,
                outcome, error_code, duration_ms, ttfb_ms)
               VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17,?18,?19)"#,
            params![
                row.ts_ms,
                row.protocol,
                row.route,
                row.agent,
                row.worktree,
                row.workspace,
                row.client_model,
                row.backend,
                row.backend_model,
                row.input_tokens,
                row.output_tokens,
                row.cache_read_tokens,
                row.cache_creation_tokens,
                row.cost_usd,
                row.cost_source,
                row.outcome,
                row.error_code,
                row.duration_ms,
                row.ttfb_ms,
            ],
        )?;
        Ok(())
    }

    fn model_proxy_requests_since(
        &self,
        since_ms: i64,
        limit: i64,
    ) -> Result<Vec<ModelProxyRequestRow>> {
        let conn = self.conn();
        let mut stmt = conn.prepare(
            r#"SELECT ts_ms, protocol, route, agent, worktree, workspace, client_model,
                      backend, backend_model, input_tokens, output_tokens,
                      cache_read_tokens, cache_creation_tokens, cost_usd, cost_source,
                      outcome, error_code, duration_ms, ttfb_ms
               FROM model_proxy_requests
               WHERE ts_ms >= ?1
               ORDER BY ts_ms DESC
               LIMIT ?2"#,
        )?;
        let rows = stmt
            .query_map(params![since_ms, limit.max(0)], |r| {
                Ok(ModelProxyRequestRow {
                    ts_ms: r.get(0)?,
                    protocol: r.get(1)?,
                    route: r.get(2)?,
                    agent: r.get(3)?,
                    worktree: r.get(4)?,
                    workspace: r.get(5)?,
                    client_model: r.get(6)?,
                    backend: r.get(7)?,
                    backend_model: r.get(8)?,
                    input_tokens: r.get(9)?,
                    output_tokens: r.get(10)?,
                    cache_read_tokens: r.get(11)?,
                    cache_creation_tokens: r.get(12)?,
                    cost_usd: r.get(13)?,
                    cost_source: r.get(14)?,
                    outcome: r.get(15)?,
                    error_code: r.get(16)?,
                    duration_ms: r.get(17)?,
                    ttfb_ms: r.get(18)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    fn model_proxy_budget_state(&self, scope: &str) -> Result<Option<ModelProxyBudgetStateRow>> {
        let row = self
            .conn()
            .query_row(
                "SELECT scope, window_start_ms, spent_tokens, spent_cost, killed
                 FROM model_proxy_budget_state WHERE scope = ?1",
                params![scope],
                budget_row,
            )
            .optional()?;
        Ok(row)
    }

    fn model_proxy_budget_states(&self) -> Result<Vec<ModelProxyBudgetStateRow>> {
        let conn = self.conn();
        let mut stmt = conn.prepare(
            "SELECT scope, window_start_ms, spent_tokens, spent_cost, killed
             FROM model_proxy_budget_state ORDER BY scope",
        )?;
        let rows = stmt
            .query_map([], budget_row)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    fn add_model_proxy_spend(
        &self,
        scope: &str,
        tokens: i64,
        cost: f64,
        now_ms: i64,
        window_len_ms: i64,
    ) -> Result<ModelProxyBudgetStateRow> {
        let conn = self.conn();
        // Read-modify-write the accumulator, advancing the window anchor when the
        // rolling window has lapsed so spend never leaks between windows.
        let existing = conn
            .query_row(
                "SELECT scope, window_start_ms, spent_tokens, spent_cost, killed
                 FROM model_proxy_budget_state WHERE scope = ?1",
                params![scope],
                budget_row,
            )
            .optional()?;
        let (window_start, base_tokens, base_cost, killed) = match existing {
            Some(r) => {
                let lapsed = window_len_ms > 0 && now_ms - r.window_start_ms >= window_len_ms;
                if lapsed {
                    (now_ms, 0, 0.0, r.killed)
                } else {
                    (r.window_start_ms, r.spent_tokens, r.spent_cost, r.killed)
                }
            }
            None => (now_ms, 0, 0.0, false),
        };
        let updated = ModelProxyBudgetStateRow {
            scope: scope.to_string(),
            window_start_ms: window_start,
            spent_tokens: base_tokens + tokens,
            spent_cost: base_cost + cost,
            killed,
        };
        conn.execute(
            r#"INSERT INTO model_proxy_budget_state
               (scope, window_start_ms, spent_tokens, spent_cost, killed)
               VALUES (?1, ?2, ?3, ?4, ?5)
               ON CONFLICT(scope) DO UPDATE SET
                 window_start_ms = excluded.window_start_ms,
                 spent_tokens    = excluded.spent_tokens,
                 spent_cost      = excluded.spent_cost,
                 killed          = excluded.killed"#,
            params![
                updated.scope,
                updated.window_start_ms,
                updated.spent_tokens,
                updated.spent_cost,
                updated.killed as i64,
            ],
        )?;
        Ok(updated)
    }

    fn set_model_proxy_kill_switch(&self, scope: &str, killed: bool) -> Result<()> {
        self.conn().execute(
            r#"INSERT INTO model_proxy_budget_state (scope, killed)
               VALUES (?1, ?2)
               ON CONFLICT(scope) DO UPDATE SET killed = excluded.killed"#,
            params![scope, killed as i64],
        )?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn req(ts: i64, backend: &str, outcome: &str) -> ModelProxyRequestRow {
        ModelProxyRequestRow {
            ts_ms: ts,
            protocol: "openai".into(),
            route: "standard".into(),
            backend: backend.into(),
            backend_model: "m".into(),
            client_model: "model-proxy/standard".into(),
            input_tokens: 10,
            output_tokens: 20,
            cache_read_tokens: 3,
            cache_creation_tokens: 1,
            cost_usd: 0.02,
            cost_source: "estimate".into(),
            outcome: outcome.into(),
            duration_ms: 500,
            ttfb_ms: Some(100),
            ..Default::default()
        }
    }

    #[test]
    fn request_round_trip_and_window() {
        let db = Db::open_memory().unwrap();
        db.put_model_proxy_request(&req(1000, "openrouter", "ok"))
            .unwrap();
        db.put_model_proxy_request(&req(2000, "codex", "ok_stream"))
            .unwrap();
        db.put_model_proxy_request(&req(500, "old", "ok")).unwrap();
        // since=1000 excludes the ts=500 row; newest first.
        let rows = db.model_proxy_requests_since(1000, 10).unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].ts_ms, 2000);
        assert_eq!(rows[0].backend, "codex");
        assert_eq!(rows[0].cache_read_tokens, 3);
        assert_eq!(rows[1].ts_ms, 1000);
        // limit caps.
        assert_eq!(db.model_proxy_requests_since(0, 1).unwrap().len(), 1);
    }

    #[test]
    fn budget_accumulates_and_rolls_over() {
        let db = Db::open_memory().unwrap();
        // No window (window_len 0) → pure accumulation.
        let r = db
            .add_model_proxy_spend("agent:x", 100, 0.5, 1000, 0)
            .unwrap();
        assert_eq!(r.spent_tokens, 100);
        assert_eq!(r.window_start_ms, 1000);
        let r = db
            .add_model_proxy_spend("agent:x", 50, 0.25, 1500, 0)
            .unwrap();
        assert_eq!(r.spent_tokens, 150);
        assert!((r.spent_cost - 0.75).abs() < 1e-9);
        assert_eq!(r.window_start_ms, 1000, "no window → anchor stays");

        // With a 1000ms window: a spend past the window resets the anchor.
        let r = db
            .add_model_proxy_spend("agent:y", 10, 0.0, 0, 1000)
            .unwrap();
        assert_eq!(r.spent_tokens, 10);
        let r = db
            .add_model_proxy_spend("agent:y", 5, 0.0, 500, 1000)
            .unwrap();
        assert_eq!(r.spent_tokens, 15, "still inside window");
        let r = db
            .add_model_proxy_spend("agent:y", 7, 0.0, 1000, 1000)
            .unwrap();
        assert_eq!(r.spent_tokens, 7, "window lapsed → reset");
        assert_eq!(r.window_start_ms, 1000);
    }

    #[test]
    fn budget_state_getters() {
        let db = Db::open_memory().unwrap();
        assert!(db.model_proxy_budget_state("nope").unwrap().is_none());
        db.add_model_proxy_spend("global", 1, 0.1, 0, 0).unwrap();
        db.add_model_proxy_spend("agent:a", 2, 0.2, 0, 0).unwrap();
        assert!(db.model_proxy_budget_state("global").unwrap().is_some());
        assert_eq!(db.model_proxy_budget_states().unwrap().len(), 2);
    }

    #[test]
    fn kill_switch_toggles_without_clobbering_spend() {
        let db = Db::open_memory().unwrap();
        db.add_model_proxy_spend("global", 42, 1.0, 0, 0).unwrap();
        db.set_model_proxy_kill_switch("global", true).unwrap();
        let r = db.model_proxy_budget_state("global").unwrap().unwrap();
        assert!(r.killed);
        assert_eq!(r.spent_tokens, 42, "kill-switch must not reset spend");
        db.set_model_proxy_kill_switch("global", false).unwrap();
        assert!(
            !db.model_proxy_budget_state("global")
                .unwrap()
                .unwrap()
                .killed
        );
        // Kill-switch on a fresh scope creates the row.
        db.set_model_proxy_kill_switch("agent:new", true).unwrap();
        assert!(
            db.model_proxy_budget_state("agent:new")
                .unwrap()
                .unwrap()
                .killed
        );
    }

    #[test]
    fn pre_v54_db_gains_tables_and_preserves_data() {
        use rusqlite::Connection;
        let dir = std::env::temp_dir().join(format!("tg-mp-mig-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir); // best-effort: test cleanup: scratch removal must never fail the test
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("db.sqlite");
        {
            let conn = Connection::open(&path).unwrap();
            conn.execute_batch(
                "PRAGMA user_version = 53;
                 CREATE TABLE repos (path TEXT PRIMARY KEY, name TEXT);
                 INSERT INTO repos(path,name) VALUES ('/keep','keep');
                 -- Orphaned pre-alpha proxy tables that MUST stay untouched.
                 CREATE TABLE proxy_requests (id TEXT PRIMARY KEY);
                 INSERT INTO proxy_requests(id) VALUES ('legacy');
                 CREATE TABLE proxy_budgets (scope TEXT PRIMARY KEY);
                 INSERT INTO proxy_budgets(scope) VALUES ('global');",
            )
            .unwrap();
        }
        let db = Db::open_at(&path).unwrap();
        // Existing user data survives.
        let kept: i64 = db
            .conn()
            .query_row("SELECT COUNT(*) FROM repos WHERE path='/keep'", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(kept, 1);
        // Version advanced.
        let ver: i64 = db
            .conn()
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .unwrap();
        assert_eq!(ver, crate::db::SCHEMA_VERSION);
        // New tables are writable.
        db.put_model_proxy_request(&req(1, "b", "ok")).unwrap();
        assert_eq!(db.model_proxy_requests_since(0, 10).unwrap().len(), 1);
        // Orphaned legacy tables are neither dropped nor touched.
        let legacy: i64 = db
            .conn()
            .query_row(
                "SELECT COUNT(*) FROM proxy_requests WHERE id='legacy'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(legacy, 1, "orphaned proxy_requests must stay intact");
        let budgets: i64 = db
            .conn()
            .query_row("SELECT COUNT(*) FROM proxy_budgets", [], |r| r.get(0))
            .unwrap();
        assert_eq!(budgets, 1);
        let _ = std::fs::remove_dir_all(&dir); // best-effort: test cleanup: scratch removal must never fail the test
    }
}
