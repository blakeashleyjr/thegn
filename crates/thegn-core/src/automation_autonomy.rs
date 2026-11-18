//! Autonomy ceilings for automation rules (THE-2195).
//!
//! A rule declares how far an unattended run may go: `propose` (raise an
//! approval, start nothing), `review` (start the pipeline; a human lands) or
//! `merge` (the pipeline may land). The declared level is capped by policy
//! before it is stored on the compiled rule, so every consumer reads the
//! effective ceiling, never the request.
//!
//! The policy (b153 lane 4, b155 lane 4):
//! - `merge` requires a structural-only `if`. The only free-text predicate is
//!   `message_regex`; a config rule using it is capped at `review` with a
//!   warning.
//! - An `origin = lead` rule is never `merge` unless it is a `pipeline.start`
//!   the owner armed ([`RuleOrigin::LeadApproved`]); a Lead rule asking for
//!   `merge` on any other action is refused.
//! - An unarmed Lead `pipeline.start` runs at `propose` only. Asking above
//!   that is not refused: the rule is stored disarmed and raises an arming
//!   approval (b155 6c, `automation_authoring::needs_arming`).
//! - A [`RuleOrigin::LeadApproved`] `pipeline.start` runs at its declared
//!   level, capped at `review` when it reads free text; anything else armed
//!   is still capped at `review`.
//! - (b163) Any `inbox_item` rule is capped at `review`: inbox items are
//!   third-party content. `autopilot.run` is capped at `review` for every
//!   origin and is config-only: a Lead rule naming it is refused.

use serde::{Deserialize, Serialize};

use crate::automation::{AutomationEventKind, AutomationPredicate};

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
    /// current body digest. Only a `pipeline.start` gains anything from it.
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
    "inbox_account",
    "inbox_kind",
    "inbox_label",
];

/// The action whose Lead-authored form only proposes until the owner arms it.
pub const PIPELINE_START: &str = "pipeline.start";

/// The config-only rule action that runs issue autopilot (THE-2196).
pub const AUTOPILOT_RUN: &str = "autopilot.run";

/// Why an `inbox_item` rule asking for `merge` runs at `review`.
pub const INBOX_CAP_REASON: &str = "autonomy capped at review: inbox items are third-party content";

/// Why an `autopilot.run` rule asking for `merge` runs at `review`.
pub const AUTOPILOT_CAP_NOTE: &str = "autonomy capped at review: autopilot.run never merges";

/// The put-time refusal for a Lead rule naming `autopilot.run`.
pub const AUTOPILOT_RUN_REFUSAL: &str = "autopilot.run is config-only";

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
/// `event` is the rule's `when`: every `inbox_item` rule is capped at
/// `review` ([`INBOX_CAP_REASON`]), as is every `autopilot.run`.
pub fn effective_autonomy(
    declared: Autonomy,
    event: AutomationEventKind,
    p: &AutomationPredicate,
    cap: &str,
    origin: RuleOrigin,
) -> (Autonomy, Option<&'static str>) {
    let (level, note) = origin_autonomy(declared, p, cap, origin);
    if level > Autonomy::Review && reads_inbox_items(event, p) {
        return (Autonomy::Review, Some(INBOX_CAP_REASON));
    }
    if level > Autonomy::Review && cap.trim() == AUTOPILOT_RUN {
        return (Autonomy::Review, Some(AUTOPILOT_CAP_NOTE));
    }
    (level, note)
}

/// Whether a rule fires on inbox items: an `inbox_item` rule, or a
/// `notification` rule pinned to `notification_kind = inbox_item`.
pub fn reads_inbox_items(event: AutomationEventKind, p: &AutomationPredicate) -> bool {
    event == AutomationEventKind::InboxItem
        || (event == AutomationEventKind::Notification
            && p.notification_kind == Some(crate::notification::NotificationKind::InboxItem))
}

/// The origin/free-text half of [`effective_autonomy`].
fn origin_autonomy(
    declared: Autonomy,
    p: &AutomationPredicate,
    cap: &str,
    origin: RuleOrigin,
) -> (Autonomy, Option<&'static str>) {
    let pipeline = cap.trim() == PIPELINE_START;
    let free_text_merge = declared == Autonomy::Merge && uses_free_text(p);
    match origin {
        RuleOrigin::Config if free_text_merge => (Autonomy::Review, Some(FREE_TEXT_CAP_NOTE)),
        RuleOrigin::Config => (declared, None),
        RuleOrigin::LeadApproved if pipeline && free_text_merge => {
            (Autonomy::Review, Some(FREE_TEXT_CAP_NOTE))
        }
        RuleOrigin::LeadApproved if pipeline => (declared, None),
        RuleOrigin::Lead if pipeline => (
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
/// A Lead `pipeline.start` above `propose` is not refused either: it is
/// stored disarmed and waits for the owner's arming approval.
pub fn validate_autonomy(
    declared: Autonomy,
    _p: &AutomationPredicate,
    cap: &str,
    origin: RuleOrigin,
) -> Result<(), String> {
    match origin {
        RuleOrigin::Config => Ok(()),
        RuleOrigin::Lead | RuleOrigin::LeadApproved if cap.trim() == AUTOPILOT_RUN => {
            Err(AUTOPILOT_RUN_REFUSAL.into())
        }
        RuleOrigin::Lead | RuleOrigin::LeadApproved
            if declared == Autonomy::Merge && cap.trim() != PIPELINE_START =>
        {
            Err(LEAD_MERGE_REFUSAL.into())
        }
        RuleOrigin::Lead | RuleOrigin::LeadApproved => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOTE: AutomationEventKind = AutomationEventKind::Notification;

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
        use RuleOrigin::{Config, Lead, LeadApproved};
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
            // Unarmed Lead pipeline.start: propose; asking higher is not
            // refused (the rule is stored disarmed and asks to be armed).
            (Lead, "pipeline.start", Propose, false, Propose, None, ok),
            (Lead, " pipeline.start ", Propose, true, Propose, None, ok),
            (
                Lead,
                "pipeline.start",
                Review,
                false,
                Propose,
                Some(LEAD_PIPELINE_NOTE),
                ok,
            ),
            (
                Lead,
                "pipeline.start",
                Review,
                true,
                Propose,
                Some(LEAD_PIPELINE_NOTE),
                ok,
            ),
            (
                Lead,
                "pipeline.start",
                Merge,
                false,
                Propose,
                Some(LEAD_PIPELINE_NOTE),
                ok,
            ),
            (
                Lead,
                "pipeline.start",
                Merge,
                true,
                Propose,
                Some(LEAD_PIPELINE_NOTE),
                ok,
            ),
            // Armed Lead pipeline.start: declared, free text capped at review.
            (
                LeadApproved,
                "pipeline.start",
                Propose,
                false,
                Propose,
                None,
                ok,
            ),
            (
                LeadApproved,
                "pipeline.start",
                Review,
                false,
                Review,
                None,
                ok,
            ),
            (
                LeadApproved,
                "pipeline.start",
                Review,
                true,
                Review,
                None,
                ok,
            ),
            (
                LeadApproved,
                " pipeline.start ",
                Merge,
                false,
                Merge,
                None,
                ok,
            ),
            (
                LeadApproved,
                "pipeline.start",
                Merge,
                true,
                Review,
                Some(FREE_TEXT_CAP_NOTE),
                ok,
            ),
            // Armed anything else: still never merge.
            (LeadApproved, "notify.push", Review, false, Review, None, ok),
            (
                LeadApproved,
                "notify.push",
                Propose,
                true,
                Propose,
                None,
                ok,
            ),
            (
                LeadApproved,
                "lead.wake",
                Merge,
                false,
                Review,
                Some(LEAD_CAP_NOTE),
                Err(LEAD_MERGE_REFUSAL),
            ),
            (
                LeadApproved,
                "sessions.open",
                Merge,
                true,
                Review,
                Some(LEAD_CAP_NOTE),
                Err(LEAD_MERGE_REFUSAL),
            ),
        ];
        for &(origin, cap, declared, free_text, effective, note, valid) in table {
            let p = predicate(free_text);
            let case = format!("{origin:?} {cap} {declared:?} free_text={free_text}");
            assert_eq!(
                effective_autonomy(declared, NOTE, &p, cap, origin),
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
    fn arming_only_ever_raises_pipeline_start() {
        for cap in [
            "notify.push",
            "sessions.open",
            "lead.wake",
            "pipeline.start",
        ] {
            for declared in Autonomy::ALL {
                for free in [false, true] {
                    let p = predicate(free);
                    let lead = effective_autonomy(declared, NOTE, &p, cap, RuleOrigin::Lead).0;
                    let armed =
                        effective_autonomy(declared, NOTE, &p, cap, RuleOrigin::LeadApproved).0;
                    let config = effective_autonomy(declared, NOTE, &p, cap, RuleOrigin::Config).0;
                    assert!(
                        lead <= armed && armed <= config,
                        "{cap} {declared:?} {free}"
                    );
                    if cap != PIPELINE_START {
                        assert_eq!(lead, armed, "{cap} {declared:?} {free}");
                        assert_eq!(
                            validate_autonomy(declared, &p, cap, RuleOrigin::LeadApproved),
                            validate_autonomy(declared, &p, cap, RuleOrigin::Lead),
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn free_text_never_raises_autonomy() {
        for origin in [
            RuleOrigin::Config,
            RuleOrigin::Lead,
            RuleOrigin::LeadApproved,
        ] {
            for cap in ["notify.push", "pipeline.start"] {
                for declared in Autonomy::ALL {
                    let structural =
                        effective_autonomy(declared, NOTE, &predicate(false), cap, origin).0;
                    let free = effective_autonomy(declared, NOTE, &predicate(true), cap, origin).0;
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

    #[test]
    fn inbox_item_rules_are_capped_at_review_for_every_origin() {
        let p = predicate(false);
        for origin in [
            RuleOrigin::Config,
            RuleOrigin::Lead,
            RuleOrigin::LeadApproved,
        ] {
            for cap in [
                "notify.push",
                "pipeline.start",
                "merge.add",
                "sessions.open",
            ] {
                for declared in Autonomy::ALL {
                    let (inbox, note) = effective_autonomy(
                        declared,
                        AutomationEventKind::InboxItem,
                        &p,
                        cap,
                        origin,
                    );
                    let other = effective_autonomy(declared, NOTE, &p, cap, origin).0;
                    assert!(inbox <= Autonomy::Review, "{origin:?} {cap} {declared:?}");
                    assert!(inbox <= other, "never above the same rule elsewhere");
                    if other == Autonomy::Merge {
                        assert_eq!(note, Some(INBOX_CAP_REASON));
                    }
                }
            }
        }
        assert_eq!(
            effective_autonomy(
                Autonomy::Merge,
                AutomationEventKind::InboxItem,
                &p,
                "merge.add",
                RuleOrigin::Config
            ),
            (Autonomy::Review, Some(INBOX_CAP_REASON))
        );
        assert_eq!(
            effective_autonomy(
                Autonomy::Propose,
                AutomationEventKind::InboxItem,
                &p,
                "merge.add",
                RuleOrigin::Config
            ),
            (Autonomy::Propose, None),
            "never raises"
        );
    }

    #[test]
    fn inbox_item_notification_rules_are_capped_too() {
        let pinned = AutomationPredicate {
            notification_kind: Some(crate::notification::NotificationKind::InboxItem),
            ..AutomationPredicate::default()
        };
        assert!(reads_inbox_items(NOTE, &pinned));
        assert!(!reads_inbox_items(NOTE, &AutomationPredicate::default()));
        assert!(!reads_inbox_items(
            AutomationEventKind::AgentFinished,
            &pinned
        ));
        assert_eq!(
            effective_autonomy(
                Autonomy::Merge,
                NOTE,
                &pinned,
                "merge.add",
                RuleOrigin::Config
            ),
            (Autonomy::Review, Some(INBOX_CAP_REASON))
        );
    }

    #[test]
    fn autopilot_run_is_review_capped_and_config_only() {
        let p = predicate(false);
        let issue = AutomationEventKind::IssueMatched;
        assert_eq!(
            effective_autonomy(
                Autonomy::Merge,
                issue,
                &p,
                AUTOPILOT_RUN,
                RuleOrigin::Config
            ),
            (Autonomy::Review, Some(AUTOPILOT_CAP_NOTE))
        );
        assert_eq!(
            effective_autonomy(
                Autonomy::Review,
                issue,
                &p,
                AUTOPILOT_RUN,
                RuleOrigin::Config
            ),
            (Autonomy::Review, None)
        );
        for origin in [RuleOrigin::Lead, RuleOrigin::LeadApproved] {
            for declared in Autonomy::ALL {
                assert_eq!(
                    validate_autonomy(declared, &p, AUTOPILOT_RUN, origin),
                    Err(AUTOPILOT_RUN_REFUSAL.to_string()),
                    "{origin:?} {declared:?}"
                );
                assert!(
                    effective_autonomy(declared, issue, &p, AUTOPILOT_RUN, origin).0
                        <= Autonomy::Review
                );
            }
        }
        assert_eq!(
            validate_autonomy(Autonomy::Merge, &p, AUTOPILOT_RUN, RuleOrigin::Config),
            Ok(())
        );
    }
}
