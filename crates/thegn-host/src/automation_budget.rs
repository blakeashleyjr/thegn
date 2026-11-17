//! Automation budget admission (b155 6e): usage counted from the DB
//! (`AutomationStore::automation_budget_usage`) against `[automations]
//! max_concurrent_lanes` / `max_spawns_per_day`.
//!
//! A usage read error is a refusal ("budget unreadable"): the budget is a
//! bound on unattended spawns, so a broken read must fail closed.

use thegn_core::automation_budget::{BudgetRefusal, BudgetUsage, Budgets, SpawnKind};
use thegn_core::config::Config;
use thegn_core::db::Db;
use thegn_core::store::AutomationStore;

/// Whether one more spawn of `kind` fits the budgets now.
pub(crate) fn admit(db: &Db, cfg: &Config, kind: SpawnKind) -> Result<(), BudgetRefusal> {
    let budgets = Budgets::from_config(&cfg.effective_automations());
    admit_at(db, &budgets, thegn_core::util::now_ms(), kind)
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

/// The pure decision over a usage read that may have failed.
fn decide(
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
