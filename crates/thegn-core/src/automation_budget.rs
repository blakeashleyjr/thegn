//! Automation budgets (6e): how many automation-started lanes may be active
//! at once and how many automation spawns a rolling 24 h window allows.
//!
//! Usage is counted from the DB (`automation_budget_usage`); the policy here
//! is pure. `0` in either limit means automation may spawn nothing.

use crate::config_automations::AutomationsConfig;

/// The configured limits.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Budgets {
    pub max_concurrent_lanes: u32,
    pub max_spawns_per_day: u32,
}

/// What the DB says is in use now.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct BudgetUsage {
    /// Distinct issues with an active dispatch row carrying an automation rule.
    pub active_lanes: u32,
    /// Automation spawns in the last 24 h.
    pub spawns_24h: u32,
}

/// What kind of spawn is being admitted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SpawnKind {
    /// A new lane (`pipeline.start` at review or merge).
    NewLane,
    /// The executor advancing an existing lane to its next stage.
    NextStage,
    /// A session (`sessions.open`, `lead.wake`).
    Session,
}

/// Why a spawn was refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BudgetRefusal {
    ConcurrentLanes { limit: u32, active: u32 },
    SpawnsPerDay { limit: u32, used: u32 },
}

impl std::fmt::Display for BudgetRefusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::ConcurrentLanes { limit, active } => write!(
                f,
                "automation lane budget reached: {active} of {limit} lanes active ([automations] max_concurrent_lanes)"
            ),
            Self::SpawnsPerDay { limit, used } => write!(
                f,
                "automation spawn budget reached: {used} of {limit} in the last 24 h ([automations] max_spawns_per_day)"
            ),
        }
    }
}

impl Budgets {
    /// The limits from `[automations]`.
    pub fn from_config(c: &AutomationsConfig) -> Self {
        Self {
            max_concurrent_lanes: c.max_concurrent_lanes,
            max_spawns_per_day: c.max_spawns_per_day,
        }
    }
}

/// Whether one more spawn of `kind` fits the budgets.
#[allow(dead_code, reason = "filled by b155 lane 7")]
pub fn check(_b: &Budgets, _u: &BudgetUsage, _kind: SpawnKind) -> Result<(), BudgetRefusal> {
    // Stub: fail closed until b155 lane 7 fills the policy.
    Err(BudgetRefusal::SpawnsPerDay { limit: 0, used: 0 })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn budgets_from_config_and_display() {
        let cfg = AutomationsConfig {
            max_concurrent_lanes: 3,
            max_spawns_per_day: 7,
            ..AutomationsConfig::default()
        };
        assert_eq!(
            Budgets::from_config(&cfg),
            Budgets {
                max_concurrent_lanes: 3,
                max_spawns_per_day: 7
            }
        );
        assert_eq!(BudgetUsage::default().active_lanes, 0);
        let lanes = BudgetRefusal::ConcurrentLanes {
            limit: 2,
            active: 2,
        }
        .to_string();
        assert!(lanes.contains("max_concurrent_lanes"), "{lanes}");
        let spawns = BudgetRefusal::SpawnsPerDay {
            limit: 12,
            used: 12,
        }
        .to_string();
        assert!(spawns.contains("max_spawns_per_day"), "{spawns}");
    }

    #[test]
    fn stub_check_fails_closed() {
        let b = Budgets::from_config(&AutomationsConfig::default());
        for kind in [SpawnKind::NewLane, SpawnKind::NextStage, SpawnKind::Session] {
            assert!(check(&b, &BudgetUsage::default(), kind).is_err());
        }
    }
}
