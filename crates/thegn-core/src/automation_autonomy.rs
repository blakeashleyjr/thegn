//! Autonomy ceilings for automation rules (THE-2195).
//!
//! A rule declares how far an unattended run may go: `propose` (raise an
//! approval, start nothing), `review` (start the pipeline; a human lands) or
//! `merge` (the pipeline may land). The declared level is capped by policy
//! before it is stored on the compiled rule, so every consumer reads the
//! effective ceiling, never the request.
//!
//! The policy (filled by b153 lane 4):
//! - `merge` requires a structural-only `if`. The only free-text predicate is
//!   `message_regex`; a config rule using it is capped at `review` with a
//!   warning, and a Lead rule asking for `merge` is refused.
//! - An `origin = lead` rule is never `merge`, and `pipeline.start` from a
//!   Lead rule runs at `propose` only.

use serde::{Deserialize, Serialize};

use crate::automation::AutomationPredicate;

/// How far an automation run may go without a human. Ordered: `Propose` <
/// `Review` < `Merge`, so `min` is "the stricter of".
#[derive(
    Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize,
)]
#[serde(rename_all = "snake_case")]
pub enum Autonomy {
    /// No worktree, no spawn: raise an approval request only.
    Propose,
    /// Start the work; a human reviews and lands it.
    #[default]
    Review,
    /// Start the work and let the pipeline land it.
    Merge,
}

impl Autonomy {
    pub const ALL: [Self; 3] = [Self::Propose, Self::Review, Self::Merge];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Propose => "propose",
            Self::Review => "review",
            Self::Merge => "merge",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|level| level.as_str() == s)
    }
}

/// Who authored a rule. Lead-authored rules are untrusted relative to config.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RuleOrigin {
    Config,
    Lead,
    /// A Lead rule the owner armed (6c): a live arming approval matches the
    /// current body digest. Until b155 lane 4 fills its policy it is treated
    /// exactly like [`RuleOrigin::Lead`] (fail closed).
    LeadApproved,
}

/// Predicate fields that read attacker-influenced free text. Every other
/// [`AutomationPredicate`] field is structural ([`STRUCTURAL_PREDICATES`]);
/// a test walks the struct so a new field must be classified here first.
pub const FREE_TEXT_PREDICATES: &[&str] = &["message_regex"];

/// Predicate fields that match identities, enums, flags or tracker ids the
/// operator controls, never free text.
pub const STRUCTURAL_PREDICATES: &[&str] = &[
    "workspace",
    "repo",
    "worktree",
    "branch",
    "agent_role",
    "notification_kind",
    "min_priority",
    "source_prefix",
    "session_id",
    "pr_checks_passed",
    "pr_review_requested",
    "pr_merged",
    "account",
    "project",
    "label",
    "assignee",
    "status",
];

/// The action whose Lead-authored form may only propose.
const PIPELINE_START: &str = "pipeline.start";

/// Why a config rule asking for `merge` runs at `review`.
pub const FREE_TEXT_CAP_NOTE: &str =
    "autonomy capped at review: the rule matches free text (message_regex)";

/// Why a stored Lead rule asking for `merge` runs at `review`.
pub const LEAD_CAP_NOTE: &str = "autonomy capped at review: Lead-authored rules cannot merge";

/// Why a stored Lead `pipeline.start` rule runs at `propose`.
pub const LEAD_PIPELINE_NOTE: &str =
    "autonomy capped at propose: Lead-authored pipeline.start only proposes";

/// The put-time refusal for a Lead rule asking for `merge`.
pub const LEAD_MERGE_REFUSAL: &str = "Lead-authored rules cannot merge";

/// The put-time refusal for a Lead `pipeline.start` above `propose`.
pub const LEAD_PIPELINE_REFUSAL: &str =
    "Lead-authored pipeline.start must use autonomy = \"propose\"";

/// Whether the predicate reads attacker-influenced free text.
pub fn uses_free_text(p: &AutomationPredicate) -> bool {
    p.message_regex.is_some()
}

/// The ceiling a rule actually runs at, plus a warning when policy lowered
/// the declared level. Never raises: the result is always `<= declared`.
pub fn effective_autonomy(
    declared: Autonomy,
    p: &AutomationPredicate,
    cap: &str,
    origin: RuleOrigin,
) -> (Autonomy, Option<&'static str>) {
    match origin {
        RuleOrigin::Config if declared == Autonomy::Merge && uses_free_text(p) => {
            (Autonomy::Review, Some(FREE_TEXT_CAP_NOTE))
        }
        RuleOrigin::Config => (declared, None),
        RuleOrigin::Lead | RuleOrigin::LeadApproved if cap.trim() == PIPELINE_START => (
            Autonomy::Propose,
            (declared != Autonomy::Propose).then_some(LEAD_PIPELINE_NOTE),
        ),
        RuleOrigin::Lead | RuleOrigin::LeadApproved => (
            declared.min(Autonomy::Review),
            (declared == Autonomy::Merge).then_some(LEAD_CAP_NOTE),
        ),
    }
}

/// Refuse a declaration policy forbids outright (checked at put/validate).
/// Config rules are never refused here: a config error disables the whole
/// runtime, so the free-text cap is a warning from [`effective_autonomy`].
pub fn validate_autonomy(
    declared: Autonomy,
    _p: &AutomationPredicate,
    cap: &str,
    origin: RuleOrigin,
) -> Result<(), String> {
    match origin {
        RuleOrigin::Config => Ok(()),
        RuleOrigin::Lead | RuleOrigin::LeadApproved if declared == Autonomy::Merge => {
            Err(LEAD_MERGE_REFUSAL.into())
        }
        RuleOrigin::Lead | RuleOrigin::LeadApproved
            if cap.trim() == PIPELINE_START && declared != Autonomy::Propose =>
        {
            Err(LEAD_PIPELINE_REFUSAL.into())
        }
        RuleOrigin::Lead | RuleOrigin::LeadApproved => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn autonomy_names_round_trip_and_default_is_review() {
        for level in Autonomy::ALL {
            assert_eq!(Autonomy::parse(level.as_str()), Some(level));
            assert_eq!(
                serde_json::to_string(&level).unwrap(),
                format!("\"{}\"", level.as_str())
            );
        }
        assert_eq!(Autonomy::parse("Merge"), None);
        assert_eq!(Autonomy::parse("yolo"), None);
        assert_eq!(Autonomy::default(), Autonomy::Review);
        assert!(Autonomy::Propose < Autonomy::Review && Autonomy::Review < Autonomy::Merge);
    }

    fn predicate(free_text: bool) -> AutomationPredicate {
        AutomationPredicate {
            branch: Some("tg/*".into()),
            message_regex: free_text.then(|| "deploy".into()),
            ..AutomationPredicate::default()
        }
    }

    /// origin × cap × declared × free text → (effective, note, validate).
    #[test]
    fn the_full_policy_table() {
        use Autonomy::{Merge, Propose, Review};
        use RuleOrigin::{Config, Lead};
        type Row = (
            RuleOrigin,
            &'static str,
            Autonomy,
            bool,
            Autonomy,
            Option<&'static str>,
            Result<(), &'static str>,
        );
        let ok = Ok(());
        let table: &[Row] = &[
            // Config: only merge + free text is lowered, and never refused.
            (Config, "notify.push", Propose, false, Propose, None, ok),
            (Config, "notify.push", Propose, true, Propose, None, ok),
            (Config, "notify.push", Review, false, Review, None, ok),
            (Config, "notify.push", Review, true, Review, None, ok),
            (Config, "notify.push", Merge, false, Merge, None, ok),
            (
                Config,
                "notify.push",
                Merge,
                true,
                Review,
                Some(FREE_TEXT_CAP_NOTE),
                ok,
            ),
            (Config, "pipeline.start", Propose, false, Propose, None, ok),
            (Config, "pipeline.start", Review, true, Review, None, ok),
            (Config, "pipeline.start", Merge, false, Merge, None, ok),
            (
                Config,
                "pipeline.start",
                Merge,
                true,
                Review,
                Some(FREE_TEXT_CAP_NOTE),
                ok,
            ),
            // Lead: never merge.
            (Lead, "notify.push", Propose, false, Propose, None, ok),
            (Lead, "notify.push", Propose, true, Propose, None, ok),
            (Lead, "notify.push", Review, false, Review, None, ok),
            (Lead, "notify.push", Review, true, Review, None, ok),
            (
                Lead,
                "notify.push",
                Merge,
                false,
                Review,
                Some(LEAD_CAP_NOTE),
                Err(LEAD_MERGE_REFUSAL),
            ),
            (
                Lead,
                "lead.wake",
                Merge,
                true,
                Review,
                Some(LEAD_CAP_NOTE),
                Err(LEAD_MERGE_REFUSAL),
            ),
            // Lead pipeline.start: propose, always.
            (Lead, "pipeline.start", Propose, false, Propose, None, ok),
            (Lead, " pipeline.start ", Propose, true, Propose, None, ok),
            (
                Lead,
                "pipeline.start",
                Review,
                false,
                Propose,
                Some(LEAD_PIPELINE_NOTE),
                Err(LEAD_PIPELINE_REFUSAL),
            ),
            (
                Lead,
                "pipeline.start",
                Review,
                true,
                Propose,
                Some(LEAD_PIPELINE_NOTE),
                Err(LEAD_PIPELINE_REFUSAL),
            ),
            (
                Lead,
                "pipeline.start",
                Merge,
                false,
                Propose,
                Some(LEAD_PIPELINE_NOTE),
                Err(LEAD_MERGE_REFUSAL),
            ),
        ];
        for &(origin, cap, declared, free_text, effective, note, valid) in table {
            let p = predicate(free_text);
            let case = format!("{origin:?} {cap} {declared:?} free_text={free_text}");
            assert_eq!(
                effective_autonomy(declared, &p, cap, origin),
                (effective, note),
                "{case}"
            );
            assert_eq!(
                validate_autonomy(declared, &p, cap, origin),
                valid.map_err(str::to_string),
                "{case}"
            );
            assert!(effective <= declared, "policy never raises: {case}");
        }
    }

    #[test]
    fn lead_approved_is_lead_until_lane_four_fills_it() {
        for cap in ["notify.push", "pipeline.start"] {
            for declared in Autonomy::ALL {
                for free in [false, true] {
                    let p = predicate(free);
                    assert_eq!(
                        effective_autonomy(declared, &p, cap, RuleOrigin::LeadApproved),
                        effective_autonomy(declared, &p, cap, RuleOrigin::Lead),
                    );
                    assert_eq!(
                        validate_autonomy(declared, &p, cap, RuleOrigin::LeadApproved),
                        validate_autonomy(declared, &p, cap, RuleOrigin::Lead),
                    );
                }
            }
        }
    }

    #[test]
    fn free_text_never_raises_autonomy() {
        for origin in [RuleOrigin::Config, RuleOrigin::Lead] {
            for cap in ["notify.push", "pipeline.start"] {
                for declared in Autonomy::ALL {
                    let structural = effective_autonomy(declared, &predicate(false), cap, origin).0;
                    let free = effective_autonomy(declared, &predicate(true), cap, origin).0;
                    assert!(free <= structural, "{origin:?} {cap} {declared:?}");
                }
            }
        }
    }

    #[test]
    fn every_predicate_field_is_classified() {
        let fields = serde_json::to_value(AutomationPredicate::default()).unwrap();
        let fields: std::collections::BTreeSet<String> =
            fields.as_object().unwrap().keys().cloned().collect();
        let classified: std::collections::BTreeSet<String> = FREE_TEXT_PREDICATES
            .iter()
            .chain(STRUCTURAL_PREDICATES)
            .map(|field| (*field).to_string())
            .collect();
        assert_eq!(
            FREE_TEXT_PREDICATES.len() + STRUCTURAL_PREDICATES.len(),
            classified.len(),
            "a field is classified twice"
        );
        assert_eq!(
            fields, classified,
            "classify every AutomationPredicate field as free text or structural"
        );
        // Each free-text field, once set, flips `uses_free_text`.
        for field in FREE_TEXT_PREDICATES {
            let mut value = serde_json::to_value(AutomationPredicate::default()).unwrap();
            value[*field] = serde_json::json!("x");
            let p: AutomationPredicate = serde_json::from_value(value).unwrap();
            assert!(uses_free_text(&p), "{field}");
        }
        assert!(!uses_free_text(&AutomationPredicate::default()));
        assert!(!uses_free_text(&predicate(false)));
    }
}
