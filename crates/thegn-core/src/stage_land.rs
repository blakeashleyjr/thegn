//! The land rule (5c) for the daemon stage executor: when may a lane land
//! without the owner, and what is a lane's autonomy. "Stop and ask" is the
//! outcome of every doubt. Pure.
//!

use crate::automation_autonomy::{Autonomy, RuleOrigin};
use crate::config_pipeline::{Requirement, Supervisor};

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
    /// The merge gate runs inside a sandbox. While it does not, the gate runs
    /// worker-written code on the host, so no land may skip the owner.
    pub gate_isolated: bool,
}

/// A lane's autonomy: the minimum over every row in its chain, with a NULL
/// or unparseable value counting as `review`. An empty chain is `review`.
pub fn lane_autonomy(chain: &[Option<String>]) -> Autonomy {
    chain
        .iter()
        .map(|v| {
            v.as_deref()
                .and_then(|s| Autonomy::parse(s.trim()))
                .unwrap_or(Autonomy::Review)
        })
        .min()
        .unwrap_or(Autonomy::Review)
}

/// Check 1's refusal.
pub const CHAIN_NOT_MERGE: &str =
    "not every stage of this lane was started at autonomy merge by one automation rule";
/// Check 2's refusals.
pub const RULE_NOT_LOADED: &str = "the automation rule that started this lane is no longer loaded";
pub const RULE_DISABLED: &str = "the automation rule that started this lane is disabled";
pub const RULE_NOT_MERGE: &str =
    "the automation rule that started this lane is no longer at autonomy merge";
pub const RULE_FREE_TEXT: &str = "the automation rule that started this lane matches free text";
pub const RULE_ORIGIN: &str =
    "the automation rule that started this lane is Lead-authored and not armed by the owner";
/// Check 3's refusals.
pub const NOT_REVIEWED: &str =
    "no reviewed stage passed: the terminal stage is the chart's first stage";
pub const TERMINAL_NOT_DONE: &str = "the terminal stage's row is not done";
pub const REPORT_INVALID: &str = "the terminal stage's report is missing, invalid or not PASS";
pub const VALIDATIONS_NOT_GREEN: &str = "not every stage's validation is green at the current head";
/// Check 4's refusal.
pub const HEAD_MOVED: &str = "the lane's head moved since it was planned";
/// Check 5's refusals.
pub const GATE_OFF: &str = "[merge_queue] gate_on is false, so nothing would gate the land";
pub const GATE_COMMAND_EMPTY: &str = "[merge_queue] gate_command is empty";
pub const GATE_UNBOUNDED: &str = "[merge_queue] gate_timeout_secs is 0, so the gate is unbounded";
/// Check 6's refusals.
pub const PAUSED: &str = "automations are paused";
pub const BUDGET_SPENT: &str = "the automation budget does not allow another land";
/// Check 7's refusal.
pub const GATE_NOT_ISOLATED: &str = "the merge gate is not sandboxed yet";

/// `Ok` only when every unattended-land check passes; the error names the
/// first failed check (it becomes the land approval's reason). Checks run in
/// the documented order 1..=7.
pub fn unattended_land(f: &UnattendedFacts) -> Result<(), String> {
    check_chain(f)?;
    check_rule(f)?;
    check_reviewed(f)?;
    if !f.head_matches {
        return Err(HEAD_MOVED.into());
    }
    if !f.gate_on {
        return Err(GATE_OFF.into());
    }
    if !f.gate_command_set {
        return Err(GATE_COMMAND_EMPTY.into());
    }
    if f.gate_timeout_secs == 0 {
        return Err(GATE_UNBOUNDED.into());
    }
    if f.paused {
        return Err(PAUSED.into());
    }
    if !f.budget_ok {
        return Err(BUDGET_SPENT.into());
    }
    if !f.gate_isolated {
        return Err(GATE_NOT_ISOLATED.into());
    }
    Ok(())
}

/// 1. Every row in the chain records `merge` and the same `automation_rule`.
fn check_chain(f: &UnattendedFacts) -> Result<(), String> {
    let all_merge = !f.chain_autonomy.is_empty()
        && f.chain_autonomy
            .iter()
            .all(|a| a.as_deref().map(str::trim) == Some(Autonomy::Merge.as_str()));
    let first = f.chain_rule.first().and_then(|r| r.as_deref());
    let one_rule = f.chain_rule.len() == f.chain_autonomy.len()
        && first.is_some_and(|r| !r.trim().is_empty())
        && f.chain_rule.iter().all(|r| r.as_deref() == first);
    if all_merge && one_rule {
        Ok(())
    } else {
        Err(CHAIN_NOT_MERGE.into())
    }
}

/// 2. That rule is loaded, enabled, effective `merge`, structural-only, and
///    of trusted origin (config, or a Lead rule the owner armed).
fn check_rule(f: &UnattendedFacts) -> Result<(), String> {
    let Some(rule) = &f.rule else {
        return Err(RULE_NOT_LOADED.into());
    };
    if !rule.enabled {
        return Err(RULE_DISABLED.into());
    }
    if rule.effective != Autonomy::Merge {
        return Err(RULE_NOT_MERGE.into());
    }
    if rule.free_text {
        return Err(RULE_FREE_TEXT.into());
    }
    match rule.origin {
        RuleOrigin::Config | RuleOrigin::LeadApproved => Ok(()),
        RuleOrigin::Lead => Err(RULE_ORIGIN.into()),
    }
}

/// 3. A reviewed stage passed.
fn check_reviewed(f: &UnattendedFacts) -> Result<(), String> {
    if f.terminal_is_first_stage {
        return Err(NOT_REVIEWED.into());
    }
    if !f.terminal_done {
        return Err(TERMINAL_NOT_DONE.into());
    }
    if !f.report_ok {
        return Err(REPORT_INVALID.into());
    }
    if !f.validations_green {
        return Err(VALIDATIONS_NOT_GREEN.into());
    }
    Ok(())
}

/// The supervisor with `approval` removed from `land_requires` (only after
/// [`unattended_land`] passed). Every other requirement still applies.
pub fn without_approval(sup: &Supervisor) -> Supervisor {
    let mut out = sup.clone();
    out.land_requires
        .retain(|r| Requirement::parse(r) != Some(Requirement::Approval));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn passing() -> UnattendedFacts {
        UnattendedFacts {
            chain_autonomy: vec![Some("merge".into()), Some("merge".into())],
            chain_rule: vec![Some("r".into()), Some("r".into())],
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
            gate_isolated: true,
        }
    }

    #[test]
    fn all_checks_pass_is_ok() {
        assert_eq!(unattended_land(&passing()), Ok(()));
        let mut lead = passing();
        lead.rule.as_mut().unwrap().origin = RuleOrigin::LeadApproved;
        assert_eq!(unattended_land(&lead), Ok(()));
    }

    /// Each check alone flips the verdict to `Err` with its own message.
    #[test]
    fn each_check_alone_refuses_with_its_message() {
        type Flip = fn(&mut UnattendedFacts);
        let table: &[(&str, Flip, &str)] = &[
            (
                "one row review",
                |f| f.chain_autonomy[1] = Some("review".into()),
                CHAIN_NOT_MERGE,
            ),
            (
                "one row null autonomy",
                |f| f.chain_autonomy[0] = None,
                CHAIN_NOT_MERGE,
            ),
            (
                "bad autonomy",
                |f| f.chain_autonomy[0] = Some("MERGE".into()),
                CHAIN_NOT_MERGE,
            ),
            (
                "empty chain",
                |f| {
                    f.chain_autonomy.clear();
                    f.chain_rule.clear();
                },
                CHAIN_NOT_MERGE,
            ),
            (
                "mixed rules",
                |f| f.chain_rule[1] = Some("other".into()),
                CHAIN_NOT_MERGE,
            ),
            ("manual row", |f| f.chain_rule[1] = None, CHAIN_NOT_MERGE),
            (
                "blank rule",
                |f| f.chain_rule = vec![Some(" ".into()), Some(" ".into())],
                CHAIN_NOT_MERGE,
            ),
            (
                "rule list short",
                |f| {
                    f.chain_rule.pop();
                },
                CHAIN_NOT_MERGE,
            ),
            ("rule gone", |f| f.rule = None, RULE_NOT_LOADED),
            (
                "rule disabled",
                |f| f.rule.as_mut().unwrap().enabled = false,
                RULE_DISABLED,
            ),
            (
                "rule review",
                |f| f.rule.as_mut().unwrap().effective = Autonomy::Review,
                RULE_NOT_MERGE,
            ),
            (
                "rule propose",
                |f| f.rule.as_mut().unwrap().effective = Autonomy::Propose,
                RULE_NOT_MERGE,
            ),
            (
                "free text",
                |f| f.rule.as_mut().unwrap().free_text = true,
                RULE_FREE_TEXT,
            ),
            (
                "unarmed lead",
                |f| f.rule.as_mut().unwrap().origin = RuleOrigin::Lead,
                RULE_ORIGIN,
            ),
            (
                "first stage",
                |f| f.terminal_is_first_stage = true,
                NOT_REVIEWED,
            ),
            ("not done", |f| f.terminal_done = false, TERMINAL_NOT_DONE),
            ("report", |f| f.report_ok = false, REPORT_INVALID),
            (
                "validations",
                |f| f.validations_green = false,
                VALIDATIONS_NOT_GREEN,
            ),
            ("head moved", |f| f.head_matches = false, HEAD_MOVED),
            ("gate off", |f| f.gate_on = false, GATE_OFF),
            (
                "gate command",
                |f| f.gate_command_set = false,
                GATE_COMMAND_EMPTY,
            ),
            ("gate timeout", |f| f.gate_timeout_secs = 0, GATE_UNBOUNDED),
            ("paused", |f| f.paused = true, PAUSED),
            ("budget", |f| f.budget_ok = false, BUDGET_SPENT),
            (
                "gate not sandboxed",
                |f| f.gate_isolated = false,
                GATE_NOT_ISOLATED,
            ),
        ];
        for (name, flip, want) in table {
            let mut f = passing();
            flip(&mut f);
            assert_eq!(unattended_land(&f), Err((*want).to_string()), "{name}");
        }
    }

    #[test]
    fn the_first_failed_check_is_reported() {
        let mut f = passing();
        f.paused = true;
        f.head_matches = false;
        f.rule = None;
        assert_eq!(unattended_land(&f), Err(RULE_NOT_LOADED.to_string()));
    }

    #[test]
    fn lane_autonomy_is_the_chain_minimum() {
        let m = || Some("merge".to_string());
        assert_eq!(lane_autonomy(&[]), Autonomy::Review);
        assert_eq!(lane_autonomy(&[None]), Autonomy::Review);
        assert_eq!(lane_autonomy(&[m(), None]), Autonomy::Review);
        assert_eq!(lane_autonomy(&[m(), Some("yolo".into())]), Autonomy::Review);
        assert_eq!(
            lane_autonomy(&[m(), Some("review".into())]),
            Autonomy::Review
        );
        assert_eq!(
            lane_autonomy(&[m(), Some("propose".into())]),
            Autonomy::Propose
        );
        assert_eq!(
            lane_autonomy(&[None, Some("propose".into())]),
            Autonomy::Propose
        );
        assert_eq!(lane_autonomy(&[m()]), Autonomy::Merge);
        assert_eq!(lane_autonomy(&[m(), m(), m()]), Autonomy::Merge);
    }

    #[test]
    fn without_approval_keeps_the_other_requirements() {
        let sup = Supervisor {
            land_requires: vec![
                "parent_report".into(),
                "approval".into(),
                "validation:green".into(),
            ],
            ..Supervisor::default()
        };
        let out = without_approval(&sup);
        assert_eq!(
            out.land_requires,
            vec!["parent_report".to_string(), "validation:green".to_string()]
        );
        assert_eq!(
            Supervisor {
                land_requires: sup.land_requires.clone(),
                ..out.clone()
            },
            sup,
            "nothing but land_requires changes"
        );
        assert!(
            without_approval(&Supervisor::default())
                .land_requires
                .is_empty()
        );
    }
}
