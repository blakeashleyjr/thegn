//! The land rule (5c) for the daemon stage executor: when may a lane land
//! without the owner, and what is a lane's autonomy. "Stop and ask" is the
//! outcome of every doubt. Pure.
//!
//! Spine stubs (b155 lane 1); b155 lane 3 fills the policy. Every stub fails
//! closed: autonomy `review`, unattended land refused, approval kept.

use crate::automation_autonomy::{Autonomy, RuleOrigin};
use crate::config_pipeline::Supervisor;

/// The loaded rule a lane's rows name, as the executor sees it now.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuleStanding {
    pub origin: RuleOrigin,
    pub enabled: bool,
    pub effective: Autonomy,
    pub free_text: bool,
}

/// Everything [`unattended_land`] decides on, gathered by the executor.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnattendedFacts {
    /// Recorded autonomy of every row in the lane's chain (root first).
    pub chain_autonomy: Vec<Option<String>>,
    /// Recorded `automation_rule` of every row in the chain (root first).
    pub chain_rule: Vec<Option<String>>,
    /// The rule the chain names, if it is still loaded.
    pub rule: Option<RuleStanding>,
    pub terminal_is_first_stage: bool,
    pub terminal_done: bool,
    pub report_ok: bool,
    pub validations_green: bool,
    pub head_matches: bool,
    pub gate_on: bool,
    pub gate_command_set: bool,
    pub gate_timeout_secs: u64,
    pub paused: bool,
    pub budget_ok: bool,
}

/// A lane's autonomy: the minimum over every row in its chain, with a NULL
/// or unparseable value counting as `review`.
#[allow(dead_code, reason = "filled by b155 lane 3")]
pub fn lane_autonomy(_chain: &[Option<String>]) -> Autonomy {
    Autonomy::Review
}

/// `Ok` only when every unattended-land check passes; the error names the
/// first failed check (it becomes the land approval's reason).
#[allow(dead_code, reason = "filled by b155 lane 3")]
pub fn unattended_land(_f: &UnattendedFacts) -> Result<(), String> {
    Err("unattended land is not implemented".into())
}

/// The supervisor with `approval` removed from `land_requires` (only after
/// [`unattended_land`] passed).
#[allow(dead_code, reason = "filled by b155 lane 3")]
pub fn without_approval(sup: &Supervisor) -> Supervisor {
    sup.clone()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stubs_fail_closed() {
        assert_eq!(lane_autonomy(&[Some("merge".into())]), Autonomy::Review);
        let facts = UnattendedFacts {
            chain_autonomy: vec![Some("merge".into())],
            chain_rule: vec![Some("r".into())],
            rule: Some(RuleStanding {
                origin: RuleOrigin::Config,
                enabled: true,
                effective: Autonomy::Merge,
                free_text: false,
            }),
            terminal_is_first_stage: false,
            terminal_done: true,
            report_ok: true,
            validations_green: true,
            head_matches: true,
            gate_on: true,
            gate_command_set: true,
            gate_timeout_secs: 600,
            paused: false,
            budget_ok: true,
        };
        assert!(unattended_land(&facts).is_err());
        let sup = Supervisor::default();
        assert_eq!(without_approval(&sup), sup);
    }
}
