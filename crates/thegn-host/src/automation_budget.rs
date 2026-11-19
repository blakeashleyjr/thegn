//! Automation budget admission (b155 6e): usage counted from the DB
//! (`AutomationStore::automation_budget_usage`) against `[automations]
//! max_concurrent_lanes` / `max_spawns_per_day`.
//!
//! A usage read error is a refusal ("budget unreadable"): the budget is a
//! bound on unattended spawns, so a broken read must fail closed.
//!
//! Callers (THE-2196): the automation runtime and executor, every issue
//! autopilot pickup (`autopilot_driver::admit_pickup`, a new lane) and a
//! Lead's manual `sessions.open` (a session; `daemon::service`).

use thegn_core::automation_budget::{BudgetRefusal, BudgetUsage, Budgets, SpawnKind};
use thegn_core::config::Config;
use thegn_core::db::Db;
use thegn_core::store::AutomationStore;

/// Whether one more spawn of `kind` fits the budgets now.
pub(crate) fn admit(db: &Db, cfg: &Config, kind: SpawnKind) -> Result<(), BudgetRefusal> {
    let budgets = Budgets::from_config(&cfg.effective_automations());
    admit_at(db, &budgets, thegn_core::util::now_ms(), kind)
}

/// [`admit`] with `pending` added to the usage read: the spawns rules
/// already kept for the same event will make (the runtime's pre-admission
/// check, `automation_runtime::refuse_over_budget`).
pub(crate) fn admit_with_pending(
    db: &Db,
    cfg: &Config,
    kind: SpawnKind,
    pending: &BudgetUsage,
) -> Result<(), BudgetRefusal> {
    let budgets = Budgets::from_config(&cfg.effective_automations());
    let usage = db
        .automation_budget_usage(thegn_core::util::now_ms())
        .map(|u| with_pending(u, pending));
    decide(&budgets, usage, kind)
}

/// Add an event's not-yet-admitted spawns to a usage read.
fn with_pending(usage: BudgetUsage, pending: &BudgetUsage) -> BudgetUsage {
    BudgetUsage {
        active_lanes: usage.active_lanes.saturating_add(pending.active_lanes),
        spawns_24h: usage.spawns_24h.saturating_add(pending.spawns_24h),
    }
}

/// [`admit`] for a planned automation run whose own `'started'` audit row is
/// already in the usage count: that row is subtracted (one spawn, plus one
/// active lane for a new lane) so the run is not counted against itself.
/// The automation executor re-checks with this immediately before a
/// budgeted action runs (`automation_executor::execute`); the stage
/// executor callers have no started row and use [`admit`].
pub(crate) fn admit_planned(db: &Db, cfg: &Config, kind: SpawnKind) -> Result<(), BudgetRefusal> {
    let budgets = Budgets::from_config(&cfg.effective_automations());
    admit_planned_at(db, &budgets, thegn_core::util::now_ms(), kind)
}

/// [`admit_planned`] with explicit limits and clock (testable).
pub(crate) fn admit_planned_at(
    db: &Db,
    budgets: &Budgets,
    now_ms: i64,
    kind: SpawnKind,
) -> Result<(), BudgetRefusal> {
    let usage = db
        .automation_budget_usage(now_ms)
        .map(|u| discount_own_row(u, kind));
    decide(budgets, usage, kind)
}

/// Remove the planned run's own started row from a usage read.
fn discount_own_row(mut usage: BudgetUsage, kind: SpawnKind) -> BudgetUsage {
    usage.spawns_24h = usage.spawns_24h.saturating_sub(1);
    if kind == SpawnKind::NewLane {
        usage.active_lanes = usage.active_lanes.saturating_sub(1);
    }
    usage
}

/// [`admit`] with explicit limits and clock (testable).
pub(crate) fn admit_at(
    db: &Db,
    budgets: &Budgets,
    now_ms: i64,
    kind: SpawnKind,
) -> Result<(), BudgetRefusal> {
    decide(budgets, db.automation_budget_usage(now_ms), kind)
}

/// The pure decision over a usage read that may have failed (an unreadable
/// usage is a refusal). Also the Lead manual-open check
/// (`daemon::service::lead_open_budget`).
pub(crate) fn decide(
    budgets: &Budgets,
    usage: anyhow::Result<BudgetUsage>,
    kind: SpawnKind,
) -> Result<(), BudgetRefusal> {
    match usage {
        Ok(usage) => thegn_core::automation_budget::check(budgets, &usage, kind),
        Err(error) => {
            tracing::warn!(
                target: "thegn::automation",
                error = %format!("{error:#}"),
                "automation budget unreadable; refusing the spawn"
            );
            Err(BudgetRefusal::Unreadable {
                error: format!("{error:#}"),
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const B: Budgets = Budgets {
        max_concurrent_lanes: 2,
        max_spawns_per_day: 12,
    };

    #[test]
    fn admit_on_a_fresh_fixture_db_allows_every_kind() {
        let db = Db::open_memory().unwrap();
        for kind in [SpawnKind::NewLane, SpawnKind::NextStage, SpawnKind::Session] {
            assert_eq!(admit_at(&db, &B, 1_763_337_600_000, kind), Ok(()));
        }
    }

    #[test]
    fn admit_fails_closed_when_the_usage_read_errors() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("budget.sqlite");
        let db = Db::open_at(&path).unwrap();
        rusqlite::Connection::open(&path)
            .unwrap()
            .execute_batch("DROP TABLE automation_runs")
            .unwrap();
        for kind in [SpawnKind::NewLane, SpawnKind::NextStage, SpawnKind::Session] {
            let refusal = admit_at(&db, &B, 1_763_337_600_000, kind).unwrap_err();
            assert!(
                matches!(refusal, BudgetRefusal::Unreadable { .. }),
                "{refusal:?}"
            );
            assert!(refusal.to_string().contains("unreadable"), "{refusal}");
        }
    }

    #[test]
    fn discount_own_row_removes_one_spawn_and_one_lane_for_a_new_lane() {
        let u = BudgetUsage {
            active_lanes: 2,
            spawns_24h: 12,
        };
        assert_eq!(
            discount_own_row(u, SpawnKind::NewLane),
            BudgetUsage {
                active_lanes: 1,
                spawns_24h: 11
            }
        );
        assert_eq!(
            discount_own_row(u, SpawnKind::Session),
            BudgetUsage {
                active_lanes: 2,
                spawns_24h: 11
            }
        );
        assert_eq!(
            discount_own_row(BudgetUsage::default(), SpawnKind::NewLane),
            BudgetUsage::default()
        );
    }

    #[test]
    fn with_pending_adds_saturating() {
        let u = BudgetUsage {
            active_lanes: 1,
            spawns_24h: u32::MAX,
        };
        let p = BudgetUsage {
            active_lanes: 1,
            spawns_24h: 1,
        };
        assert_eq!(
            with_pending(u, &p),
            BudgetUsage {
                active_lanes: 2,
                spawns_24h: u32::MAX
            }
        );
    }

    #[test]
    fn admit_planned_on_a_fresh_fixture_db_allows_every_kind() {
        let db = Db::open_memory().unwrap();
        for kind in [SpawnKind::NewLane, SpawnKind::NextStage, SpawnKind::Session] {
            assert_eq!(admit_planned_at(&db, &B, 1_763_337_600_000, kind), Ok(()));
        }
    }

    #[test]
    fn decide_applies_the_core_policy_to_a_good_read() {
        let full = BudgetUsage {
            active_lanes: 2,
            spawns_24h: 0,
        };
        assert_eq!(
            decide(&B, Ok(full), SpawnKind::NewLane),
            Err(BudgetRefusal::ConcurrentLanes {
                limit: 2,
                active: 2
            })
        );
        assert_eq!(decide(&B, Ok(full), SpawnKind::NextStage), Ok(()));
        assert!(matches!(
            decide(&B, Err(anyhow::anyhow!("locked")), SpawnKind::Session),
            Err(BudgetRefusal::Unreadable { .. })
        ));
    }
}
