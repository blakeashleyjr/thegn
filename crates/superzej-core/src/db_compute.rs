//! Persisted compute-spend state (schema **v36**): `compute_budgets` (the
//! placement ledger's caps + monthly windows + kill-switch — the proxy
//! budget row minus tokens) and `compute_meters` (watermark accrual rows).
//! Sibling `impl Db` block; `db.rs` carries only the version bump + call.

use anyhow::Result;
use rusqlite::{Connection, OptionalExtension, params};

use crate::db::Db;
use crate::store::{ComputeBudgetRow, ComputeLedgerStore, ComputeMeterRow};

/// A monthly window (the proxy ledger's period semantics).
const MONTH_MS: i64 = 30 * 24 * 60 * 60 * 1000;

/// v36: the compute spend ledger. Purely additive.
pub(crate) fn migrate_v36(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        r#"
        BEGIN;
        CREATE TABLE IF NOT EXISTS compute_budgets (
          scope      TEXT PRIMARY KEY,
          period     TEXT NOT NULL DEFAULT 'monthly',
          spent_cost REAL NOT NULL DEFAULT 0,
          limit_cost REAL,
          reset_ms   INTEGER NOT NULL DEFAULT 0,
          killed     INTEGER NOT NULL DEFAULT 0
        );
        CREATE TABLE IF NOT EXISTS compute_meters (
          resource        TEXT PRIMARY KEY,
          provider        TEXT NOT NULL DEFAULT '',
          category        TEXT NOT NULL DEFAULT 'fixed',
          rate_hourly     REAL NOT NULL DEFAULT 0,
          scope           TEXT NOT NULL DEFAULT '',
          zone            TEXT NOT NULL DEFAULT '',
          started_at_ms   INTEGER NOT NULL,
          last_accrued_ms INTEGER NOT NULL,
          stopped_at_ms   INTEGER
        );
        COMMIT;
        "#,
    )?;
    Ok(())
}

fn budget_from(r: &rusqlite::Row<'_>) -> rusqlite::Result<ComputeBudgetRow> {
    Ok(ComputeBudgetRow {
        scope: r.get(0)?,
        period: r.get(1)?,
        spent_cost: r.get(2)?,
        limit_cost: r.get(3)?,
        reset_ms: r.get(4)?,
        killed: r.get::<_, i64>(5)? != 0,
    })
}

fn meter_from(r: &rusqlite::Row<'_>) -> rusqlite::Result<ComputeMeterRow> {
    Ok(ComputeMeterRow {
        resource: r.get(0)?,
        provider: r.get(1)?,
        category: r.get(2)?,
        rate_hourly: r.get(3)?,
        scope: r.get(4)?,
        zone: r.get(5)?,
        started_at_ms: r.get(6)?,
        last_accrued_ms: r.get(7)?,
        stopped_at_ms: r.get(8)?,
    })
}

const METER_COLS: &str = "resource, provider, category, rate_hourly, scope, zone, \
                          started_at_ms, last_accrued_ms, stopped_at_ms";

/// The compute budget verdict for a PAID lane (autoscale create, spillover).
/// Packing onto already-paid hosts is never gated — a cap breach stops new
/// spend, not service. The Downgrade analog is deferral: compute cannot swap
/// a cheaper machine mid-spawn.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ComputeVerdict {
    Allow,
    /// Kill-switch or cap breach with `on_exhaustion` ∈ {reject, error}.
    Refuse(String),
    /// Cap breach with `on_exhaustion = queue`.
    Queue,
}

/// Walk `global` (+ the zone scope when any): kill-switch always refuses; a
/// spent ≥ limit breach defers or refuses per `queue_on_breach`.
pub fn check_compute_budget<S: ComputeLedgerStore>(
    db: &S,
    zone: Option<&str>,
    queue_on_breach: bool,
) -> ComputeVerdict {
    let mut scopes = vec!["global".to_string()];
    if let Some(z) = zone.filter(|z| !z.is_empty()) {
        scopes.push(format!("zone:{z}"));
    }
    for scope in scopes {
        let Some(b) = db.compute_budget(&scope).ok().flatten() else {
            continue;
        };
        if b.killed {
            return ComputeVerdict::Refuse(format!("compute kill-switch on ({scope})"));
        }
        if let Some(limit) = b.limit_cost
            && limit > 0.0
            && b.spent_cost >= limit
        {
            let why = format!(
                "compute cap reached ({scope}: {:.2}/{:.2} USD)",
                b.spent_cost, limit
            );
            return if queue_on_breach {
                ComputeVerdict::Queue
            } else {
                ComputeVerdict::Refuse(why)
            };
        }
    }
    ComputeVerdict::Allow
}

/// Pure watermark math (also the unit-test surface): cost of the slice from
/// `last_ms` to `now_ms` at `rate_hourly`, clamped ≥ 0 (clock skew reads 0).
pub fn accrue_cost(rate_hourly: f64, last_ms: i64, now_ms: i64) -> f64 {
    if now_ms <= last_ms || !rate_hourly.is_finite() || rate_hourly <= 0.0 {
        return 0.0;
    }
    rate_hourly * ((now_ms - last_ms) as f64 / 3_600_000.0)
}

impl Db {
    /// Accrue-or-stop shared body; `stop` also stamps `stopped_at_ms`.
    fn meter_advance(&self, resource: &str, now_ms: i64, stop: bool) -> Result<f64> {
        let m = self
            .conn()
            .query_row(
                &format!("SELECT {METER_COLS} FROM compute_meters WHERE resource=?1"),
                params![resource],
                meter_from,
            )
            .optional()?;
        let Some(m) = m else { return Ok(0.0) };
        if m.stopped_at_ms.is_some() {
            return Ok(0.0); // already final — idempotent
        }
        let cost = accrue_cost(m.rate_hourly, m.last_accrued_ms, now_ms);
        let wm = now_ms.max(m.last_accrued_ms);
        if stop {
            self.conn().execute(
                "UPDATE compute_meters SET last_accrued_ms=?2, stopped_at_ms=?2
                  WHERE resource=?1",
                params![resource, wm],
            )?;
        } else {
            self.conn().execute(
                "UPDATE compute_meters SET last_accrued_ms=?2 WHERE resource=?1",
                params![resource, wm],
            )?;
        }
        if cost > 0.0 {
            // Triple attribution: scope, zone (when any), global.
            let _ = self.add_compute_spend(&m.scope, cost, now_ms);
            if !m.zone.is_empty() {
                let _ = self.add_compute_spend(&format!("zone:{}", m.zone), cost, now_ms);
            }
            if m.scope != "global" {
                let _ = self.add_compute_spend("global", cost, now_ms);
            }
        }
        Ok(cost)
    }
}

impl ComputeLedgerStore for Db {
    fn compute_budget(&self, scope: &str) -> Result<Option<ComputeBudgetRow>> {
        Ok(self
            .conn()
            .query_row(
                "SELECT scope, period, spent_cost, limit_cost, reset_ms, killed
                   FROM compute_budgets WHERE scope=?1",
                params![scope],
                budget_from,
            )
            .optional()?)
    }

    fn set_compute_budget_limits(
        &self,
        scope: &str,
        period: &str,
        limit_cost: Option<f64>,
        reset_ms: i64,
    ) -> Result<()> {
        self.conn().execute(
            "INSERT INTO compute_budgets (scope, period, limit_cost, reset_ms)
             VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT(scope) DO UPDATE SET
               period = excluded.period,
               limit_cost = excluded.limit_cost",
            params![scope, period, limit_cost, reset_ms],
        )?;
        Ok(())
    }

    fn set_compute_kill_switch(&self, scope: &str, killed: bool) -> Result<()> {
        self.conn().execute(
            "INSERT INTO compute_budgets (scope, killed) VALUES (?1, ?2)
             ON CONFLICT(scope) DO UPDATE SET killed = excluded.killed",
            params![scope, killed as i64],
        )?;
        Ok(())
    }

    fn add_compute_spend(&self, scope: &str, cost: f64, now_ms: i64) -> Result<(f64, bool)> {
        // Window roll: elapsed reset ⇒ spend restarts at this charge.
        self.conn().execute(
            "INSERT INTO compute_budgets (scope, spent_cost, reset_ms)
             VALUES (?1, ?2, ?3 + ?4)
             ON CONFLICT(scope) DO UPDATE SET
               spent_cost = CASE WHEN compute_budgets.reset_ms > 0
                                  AND ?3 >= compute_budgets.reset_ms
                             THEN excluded.spent_cost
                             ELSE compute_budgets.spent_cost + excluded.spent_cost END,
               reset_ms = CASE WHEN compute_budgets.reset_ms > 0
                                AND ?3 >= compute_budgets.reset_ms
                           THEN ?3 + ?4
                           ELSE compute_budgets.reset_ms END",
            params![scope, cost, now_ms, MONTH_MS],
        )?;
        let row = self.compute_budget(scope)?.unwrap_or(ComputeBudgetRow {
            scope: scope.into(),
            period: "monthly".into(),
            spent_cost: cost,
            limit_cost: None,
            reset_ms: 0,
            killed: false,
        });
        Ok((row.spent_cost, row.killed))
    }

    fn start_compute_meter(&self, m: &ComputeMeterRow) -> Result<()> {
        self.conn().execute(
            "INSERT OR REPLACE INTO compute_meters
               (resource, provider, category, rate_hourly, scope, zone,
                started_at_ms, last_accrued_ms, stopped_at_ms)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, NULL)",
            params![
                m.resource,
                m.provider,
                m.category,
                m.rate_hourly,
                m.scope,
                m.zone,
                m.started_at_ms,
                m.last_accrued_ms,
            ],
        )?;
        Ok(())
    }

    fn live_compute_meters(&self) -> Result<Vec<ComputeMeterRow>> {
        let mut stmt = self.conn().prepare(&format!(
            "SELECT {METER_COLS} FROM compute_meters WHERE stopped_at_ms IS NULL"
        ))?;
        let rows = stmt.query_map([], meter_from)?;
        Ok(rows.filter_map(|r| r.ok()).collect())
    }

    fn accrue_compute_meter(&self, resource: &str, now_ms: i64) -> Result<f64> {
        self.meter_advance(resource, now_ms, false)
    }

    fn stop_compute_meter(&self, resource: &str, now_ms: i64) -> Result<f64> {
        self.meter_advance(resource, now_ms, true)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn db() -> Db {
        Db::open_memory().unwrap()
    }

    #[test]
    fn accrue_cost_math() {
        assert_eq!(accrue_cost(1.0, 0, 3_600_000), 1.0);
        assert_eq!(accrue_cost(2.0, 0, 1_800_000), 1.0);
        assert_eq!(accrue_cost(1.0, 5, 5), 0.0, "zero elapsed");
        assert_eq!(accrue_cost(1.0, 10, 5), 0.0, "clock skew reads 0");
        assert_eq!(accrue_cost(0.0, 0, 999), 0.0);
        assert_eq!(accrue_cost(f64::NAN, 0, 999), 0.0);
    }

    #[test]
    fn limits_never_clobber_spend_and_kill_switch_round_trips() {
        let db = db();
        db.add_compute_spend("global", 5.0, 1000).unwrap();
        db.set_compute_budget_limits("global", "monthly", Some(50.0), 0)
            .unwrap();
        let b = db.compute_budget("global").unwrap().unwrap();
        assert_eq!(b.spent_cost, 5.0, "spend preserved");
        assert_eq!(b.limit_cost, Some(50.0));
        db.set_compute_kill_switch("global", true).unwrap();
        assert!(db.compute_budget("global").unwrap().unwrap().killed);
        db.set_compute_kill_switch("global", false).unwrap();
        assert!(!db.compute_budget("global").unwrap().unwrap().killed);
    }

    #[test]
    fn window_rolls_after_reset() {
        let db = db();
        let (s, _) = db.add_compute_spend("global", 3.0, 1_000).unwrap();
        assert_eq!(s, 3.0);
        let (s, _) = db.add_compute_spend("global", 2.0, 2_000).unwrap();
        assert_eq!(s, 5.0);
        // Jump past the window: spend restarts at the new charge.
        let reset = db.compute_budget("global").unwrap().unwrap().reset_ms;
        let (s, _) = db.add_compute_spend("global", 1.5, reset + 5).unwrap();
        assert_eq!(s, 1.5, "window rolled");
    }

    fn meter(resource: &str, rate: f64, zone: &str) -> ComputeMeterRow {
        ComputeMeterRow {
            resource: resource.into(),
            provider: "hetzner".into(),
            category: "fixed".into(),
            rate_hourly: rate,
            scope: "worktree:/wt/x".into(),
            zone: zone.into(),
            started_at_ms: 0,
            last_accrued_ms: 0,
            stopped_at_ms: None,
        }
    }

    #[test]
    fn meter_watermark_is_idempotent_and_catches_up() {
        let db = db();
        db.start_compute_meter(&meter("host-a", 1.0, "")).unwrap();
        // One hour → $1; a second tick at the same clock adds nothing.
        assert_eq!(db.accrue_compute_meter("host-a", 3_600_000).unwrap(), 1.0);
        assert_eq!(db.accrue_compute_meter("host-a", 3_600_000).unwrap(), 0.0);
        // A week-long gap accrues exactly once.
        let week = 7 * 24 * 3_600_000i64;
        let c = db.accrue_compute_meter("host-a", 3_600_000 + week).unwrap();
        assert!((c - 168.0).abs() < 1e-9, "{c}");
        // Attributed to scope + global (no zone).
        let g = db.compute_budget("global").unwrap().unwrap().spent_cost;
        assert!((g - 169.0).abs() < 1e-9, "{g}");
        let w = db
            .compute_budget("worktree:/wt/x")
            .unwrap()
            .unwrap()
            .spent_cost;
        assert!((w - 169.0).abs() < 1e-9, "{w}");
    }

    #[test]
    fn zone_attribution_and_stop_finality() {
        let db = db();
        db.start_compute_meter(&meter("host-z", 2.0, "clientA"))
            .unwrap();
        assert_eq!(db.stop_compute_meter("host-z", 1_800_000).unwrap(), 1.0);
        let z = db
            .compute_budget("zone:clientA")
            .unwrap()
            .unwrap()
            .spent_cost;
        assert!((z - 1.0).abs() < 1e-9);
        // Stopped ⇒ further accruals are 0 and it leaves the live set.
        assert_eq!(db.accrue_compute_meter("host-z", 9_999_999).unwrap(), 0.0);
        assert_eq!(db.stop_compute_meter("host-z", 9_999_999).unwrap(), 0.0);
        assert!(db.live_compute_meters().unwrap().is_empty());
    }

    #[test]
    fn verdict_ladder_walks_global_then_zone() {
        let db = db();
        assert_eq!(check_compute_budget(&db, None, true), ComputeVerdict::Allow);
        db.set_compute_budget_limits("zone:clientA", "monthly", Some(10.0), 0)
            .unwrap();
        db.add_compute_spend("zone:clientA", 10.0, 1).unwrap();
        // Zone cap hits members only.
        assert_eq!(check_compute_budget(&db, None, true), ComputeVerdict::Allow);
        assert_eq!(
            check_compute_budget(&db, Some("clientA"), true),
            ComputeVerdict::Queue
        );
        assert!(matches!(
            check_compute_budget(&db, Some("clientA"), false),
            ComputeVerdict::Refuse(_)
        ));
        // Kill-switch always refuses, queue or not.
        db.set_compute_kill_switch("global", true).unwrap();
        assert!(matches!(
            check_compute_budget(&db, None, true),
            ComputeVerdict::Refuse(_)
        ));
    }

    #[test]
    fn migration_is_idempotent() {
        let db = db();
        migrate_v36(db.conn()).unwrap();
        migrate_v36(db.conn()).unwrap();
        assert!(db.live_compute_meters().unwrap().is_empty());
    }
}
