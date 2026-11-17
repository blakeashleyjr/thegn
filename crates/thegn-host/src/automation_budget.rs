//! Automation budget admission (b155 6e): usage counted from the DB
//! (`AutomationStore::automation_budget_usage`) against `[automations]
//! max_concurrent_lanes` / `max_spawns_per_day`.
//!
//! Spine stub: b155 lane 7 fills it. It calls the core policy, which fails
//! closed until then.

use thegn_core::automation_budget::{BudgetRefusal, BudgetUsage, Budgets, SpawnKind};
use thegn_core::config::Config;
use thegn_core::db::Db;

/// Whether one more spawn of `kind` fits the budgets now.
#[allow(dead_code, reason = "filled by b155 lane 7")]
pub(crate) fn admit(_db: &Db, cfg: &Config, kind: SpawnKind) -> Result<(), BudgetRefusal> {
    let budgets = Budgets::from_config(&cfg.effective_automations());
    thegn_core::automation_budget::check(&budgets, &BudgetUsage::default(), kind)
}
