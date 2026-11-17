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
    ConcurrentLanes {
        limit: u32,
        active: u32,
    },
    SpawnsPerDay {
        limit: u32,
        used: u32,
    },
    /// The usage could not be read; admission fails closed.
    Unreadable {
        error: String,
    },
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
            Self::Unreadable { error } => {
                write!(f, "automation budget unreadable (refusing): {error}")
            }
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
///
/// Every kind draws on the rolling 24 h spawn budget; only a `NewLane` also
/// needs a free concurrent-lane slot (an advancing lane already holds its
/// slot). A `0` in either limit means automation may spawn nothing, so it
/// refuses every kind.
pub fn check(b: &Budgets, u: &BudgetUsage, kind: SpawnKind) -> Result<(), BudgetRefusal> {
    let lane_slot_needed = kind == SpawnKind::NewLane || b.max_concurrent_lanes == 0;
    if lane_slot_needed && u.active_lanes >= b.max_concurrent_lanes {
        return Err(BudgetRefusal::ConcurrentLanes {
            limit: b.max_concurrent_lanes,
            active: u.active_lanes,
        });
    }
    if u.spawns_24h >= b.max_spawns_per_day {
        return Err(BudgetRefusal::SpawnsPerDay {
            limit: b.max_spawns_per_day,
            used: u.spawns_24h,
        });
    }
    Ok(())
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

    fn budgets(lanes: u32, spawns: u32) -> Budgets {
        Budgets {
            max_concurrent_lanes: lanes,
            max_spawns_per_day: spawns,
        }
    }

    fn usage(active_lanes: u32, spawns_24h: u32) -> BudgetUsage {
        BudgetUsage {
            active_lanes,
            spawns_24h,
        }
    }

    #[test]
    fn check_table_covers_each_limit_zeros_and_kinds() {
        use SpawnKind::{NewLane, NextStage, Session};
        let lanes = |limit, active| Err(BudgetRefusal::ConcurrentLanes { limit, active });
        let spawns = |limit, used| Err(BudgetRefusal::SpawnsPerDay { limit, used });
        #[rustfmt::skip]
        let table: &[(Budgets, BudgetUsage, SpawnKind, Result<(), BudgetRefusal>)] = &[
            // Room on both axes: every kind fits.
            (budgets(2, 12), usage(0, 0), NewLane, Ok(())),
            (budgets(2, 12), usage(1, 11), NewLane, Ok(())),
            (budgets(2, 12), usage(1, 11), NextStage, Ok(())),
            (budgets(2, 12), usage(1, 11), Session, Ok(())),
            // Lane limit reached: only a new lane is refused.
            (budgets(2, 12), usage(2, 3), NewLane, lanes(2, 2)),
            (budgets(2, 12), usage(3, 3), NewLane, lanes(2, 3)),
            (budgets(2, 12), usage(2, 3), NextStage, Ok(())),
            (budgets(2, 12), usage(2, 3), Session, Ok(())),
            // Spawn limit reached: every kind is refused.
            (budgets(2, 12), usage(0, 12), NewLane, spawns(12, 12)),
            (budgets(2, 12), usage(0, 12), NextStage, spawns(12, 12)),
            (budgets(2, 12), usage(0, 13), Session, spawns(12, 13)),
            // Both reached: the lane limit is reported first for a new lane.
            (budgets(2, 12), usage(2, 12), NewLane, lanes(2, 2)),
            (budgets(2, 12), usage(2, 12), NextStage, spawns(12, 12)),
            // A zero in either limit refuses every kind.
            (budgets(0, 12), usage(0, 0), NewLane, lanes(0, 0)),
            (budgets(0, 12), usage(0, 0), NextStage, lanes(0, 0)),
            (budgets(0, 12), usage(0, 0), Session, lanes(0, 0)),
            (budgets(2, 0), usage(0, 0), NewLane, spawns(0, 0)),
            (budgets(2, 0), usage(0, 0), NextStage, spawns(0, 0)),
            (budgets(2, 0), usage(0, 0), Session, spawns(0, 0)),
            (budgets(0, 0), usage(0, 0), NewLane, lanes(0, 0)),
            (budgets(0, 0), usage(0, 0), NextStage, lanes(0, 0)),
            (budgets(0, 0), usage(0, 0), Session, lanes(0, 0)),
        ];
        for (b, u, kind, want) in table {
            assert_eq!(&check(b, u, *kind), want, "{b:?} {u:?} {kind:?}");
        }
    }

    #[test]
    fn default_budgets_admit_a_fresh_day() {
        let b = Budgets::from_config(&AutomationsConfig::default());
        for kind in [SpawnKind::NewLane, SpawnKind::NextStage, SpawnKind::Session] {
            assert_eq!(check(&b, &BudgetUsage::default(), kind), Ok(()));
        }
    }

    #[test]
    fn unreadable_names_the_refusal() {
        let msg = BudgetRefusal::Unreadable {
            error: "db locked".into(),
        }
        .to_string();
        assert!(
            msg.contains("unreadable") && msg.contains("db locked"),
            "{msg}"
        );
    }
}
