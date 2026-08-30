//! Pure automation rule matching and planning.
//!
//! This module deliberately knows nothing about SQLite, executors, providers,
//! filesystems, or the host event loop. Callers supply the event, persisted
//! ledger state, and current time; the result contains either an auditable skip
//! or a rendered catalog action plus the exact ledger transition to persist.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

use crate::agent_task::{TaskVars, render_prompt};
use crate::notification::Priority;

/// Catalog capabilities automation rules may target in v1.
pub const SUPPORTED_ACTION_CAPS: &[&str] =
    &["sessions.open", "merge.add", "notify.push", "tools.run"];

/// Event variables accepted by action parameter templates.
pub const EVENT_TEMPLATE_VARS: &[&str] = &[
    "event_id",
    "event_key",
    "event_kind",
    "event_time",
    "workspace",
    "repo",
    "worktree",
    "branch",
    "agent_role",
    "priority",
    "source_ref",
    "message",
    "session_id",
    "pr_checks_passed",
    "pr_review_requested",
    "pr_merged",
];

/// A normalized fact automation rules can consume.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AutomationEventKind {
    Notification,
    AgentNeedsYou,
    AgentFinished,
    AgentFailed,
    PrChecks,
    PrReviewRequested,
    MergeLanded,
    WorktreeIdle,
    DiskLow,
}

impl AutomationEventKind {
    pub const ALL: [Self; 9] = [
        Self::Notification,
        Self::AgentNeedsYou,
        Self::AgentFinished,
        Self::AgentFailed,
        Self::PrChecks,
        Self::PrReviewRequested,
        Self::MergeLanded,
        Self::WorktreeIdle,
        Self::DiskLow,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Notification => "notification",
            Self::AgentNeedsYou => "agent_needs_you",
            Self::AgentFinished => "agent_finished",
            Self::AgentFailed => "agent_failed",
            Self::PrChecks => "pr_checks",
            Self::PrReviewRequested => "pr_review_requested",
            Self::MergeLanded => "merge_landed",
            Self::WorktreeIdle => "worktree_idle",
            Self::DiskLow => "disk_low",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|kind| kind.as_str() == value)
    }
}

/// Stable coalescing/once key supplied by the event producer.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct EventKey(pub String);

/// Metadata carried by anything an automation action causes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AutomationOrigin {
    pub root_event_id: String,
    pub rule_id: String,
    pub run_id: String,
}

/// Stable, optional fields projected from existing host/daemon facts.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AutomationEvent {
    pub id: String,
    /// Unix seconds. Matching never reads a clock directly.
    pub occurred_at: i64,
    pub key: EventKey,
    pub kind: AutomationEventKind,
    pub workspace: Option<String>,
    pub repo: Option<String>,
    pub worktree: Option<String>,
    pub branch: Option<String>,
    pub agent_role: Option<String>,
    pub priority: Option<Priority>,
    pub source_ref: Option<String>,
    pub message: Option<String>,
    pub session_id: Option<String>,
    pub pr_checks_passed: Option<bool>,
    pub pr_review_requested: Option<bool>,
    pub pr_merged: Option<bool>,
    pub origin: Option<AutomationOrigin>,
}

impl AutomationEvent {
    fn template_vars(&self) -> TaskVars {
        TaskVars::new()
            .set("event_id", &self.id)
            .set("event_key", &self.key.0)
            .set("event_kind", self.kind.as_str())
            .set("event_time", self.occurred_at.to_string())
            .set("workspace", self.workspace.as_deref().unwrap_or_default())
            .set("repo", self.repo.as_deref().unwrap_or_default())
            .set("worktree", self.worktree.as_deref().unwrap_or_default())
            .set("branch", self.branch.as_deref().unwrap_or_default())
            .set("agent_role", self.agent_role.as_deref().unwrap_or_default())
            .set(
                "priority",
                self.priority.map(Priority::as_str).unwrap_or_default(),
            )
            .set("source_ref", self.source_ref.as_deref().unwrap_or_default())
            .set("message", self.message.as_deref().unwrap_or_default())
            .set("session_id", self.session_id.as_deref().unwrap_or_default())
            .set("pr_checks_passed", optional_bool(self.pr_checks_passed))
            .set(
                "pr_review_requested",
                optional_bool(self.pr_review_requested),
            )
            .set("pr_merged", optional_bool(self.pr_merged))
    }
}

fn optional_bool(value: Option<bool>) -> &'static str {
    match value {
        Some(true) => "true",
        Some(false) => "false",
        None => "",
    }
}

/// Every configured selector is ANDed. A selector never matches a missing
/// event field.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AutomationPredicate {
    pub workspace: Option<String>,
    pub repo: Option<String>,
    pub worktree: Option<String>,
    pub branch: Option<String>,
    pub agent_role: Option<String>,
    pub min_priority: Option<Priority>,
    pub source_prefix: Option<String>,
    pub message_regex: Option<String>,
    pub session_id: Option<String>,
    pub pr_checks_passed: Option<bool>,
    pub pr_review_requested: Option<bool>,
    pub pr_merged: Option<bool>,
}

impl AutomationPredicate {
    pub fn matches(&self, event: &AutomationEvent) -> bool {
        glob_field(&self.workspace, &event.workspace)
            && glob_field(&self.repo, &event.repo)
            && glob_field(&self.worktree, &event.worktree)
            && glob_field(&self.branch, &event.branch)
            && exact_field(&self.agent_role, &event.agent_role)
            && self
                .min_priority
                .is_none_or(|minimum| event.priority.is_some_and(|p| p >= minimum))
            && self.source_prefix.as_ref().is_none_or(|prefix| {
                event
                    .source_ref
                    .as_ref()
                    .is_some_and(|source| source.starts_with(prefix))
            })
            && self.message_regex.as_ref().is_none_or(|pattern| {
                event.message.as_ref().is_some_and(|message| {
                    regex::Regex::new(pattern).is_ok_and(|re| re.is_match(message))
                })
            })
            && exact_field(&self.session_id, &event.session_id)
            && bool_field(self.pr_checks_passed, event.pr_checks_passed)
            && bool_field(self.pr_review_requested, event.pr_review_requested)
            && bool_field(self.pr_merged, event.pr_merged)
    }
}

fn glob_field(pattern: &Option<String>, value: &Option<String>) -> bool {
    pattern.as_ref().is_none_or(|pattern| {
        value
            .as_ref()
            .is_some_and(|value| crate::notification_route::glob_match(pattern, value))
    })
}

fn exact_field(want: &Option<String>, actual: &Option<String>) -> bool {
    want.as_ref()
        .is_none_or(|want| actual.as_ref().is_some_and(|actual| actual == want))
}

fn bool_field(want: Option<bool>, actual: Option<bool>) -> bool {
    want.is_none_or(|want| actual == Some(want))
}

/// A catalog capability plus bounded string templates for its named params.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ActionTemplate {
    pub cap: String,
    pub params: BTreeMap<String, String>,
}

/// One trusted rule in effective config order.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AutomationRule {
    pub id: String,
    pub enabled: bool,
    pub event: AutomationEventKind,
    pub predicate: AutomationPredicate,
    pub action: ActionTemplate,
    pub debounce_secs: u64,
    pub once_per_key: bool,
    pub max_per_hour: u16,
    pub max_action_per_hour: u16,
}

/// Persisted/in-memory throttle state for one rule.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RuleState {
    pub enabled_override: Option<bool>,
    pub last_fired_at: Option<i64>,
    pub recent_fires: Vec<i64>,
    pub action_fires: BTreeMap<String, Vec<i64>>,
    pub once_keys: BTreeSet<EventKey>,
}

pub type EvaluationState = BTreeMap<String, RuleState>;

/// The state to persist atomically when a plan is accepted for execution.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StateTransition {
    pub rule_id: String,
    pub state: RuleState,
}

/// A fully rendered action. No event value is interpreted as argv or a cap id.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlannedAction {
    pub rule_id: String,
    pub event_id: String,
    pub event_key: EventKey,
    pub cap: String,
    pub params: BTreeMap<String, String>,
    pub transition: StateTransition,
}

/// Why a matching rule did not produce executable work.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SkipReason {
    Disabled,
    Debounced,
    OncePerKey,
    RuleRateLimited,
    ActionRateLimited,
    UnsupportedAction,
    LoopSuppressed,
    InvalidTemplate,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum EvaluationDecision {
    Planned(PlannedAction),
    Skipped {
        rule_id: String,
        event_key: EventKey,
        reason: SkipReason,
    },
}

/// Evaluate matching rules in config order using only injected state and time.
/// Non-matching rules produce no decision; every policy drop after a match is
/// explicit so the caller can audit it without re-running policy.
pub fn evaluate(
    rules: &[AutomationRule],
    event: &AutomationEvent,
    state: &EvaluationState,
    now_secs: i64,
) -> Vec<EvaluationDecision> {
    rules
        .iter()
        .filter(|rule| rule.event == event.kind && rule.predicate.matches(event))
        .map(|rule| evaluate_one(rule, event, state.get(&rule.id), now_secs))
        .collect()
}

fn evaluate_one(
    rule: &AutomationRule,
    event: &AutomationEvent,
    state: Option<&RuleState>,
    now_secs: i64,
) -> EvaluationDecision {
    let current = state.cloned().unwrap_or_default();
    let skip = |reason| EvaluationDecision::Skipped {
        rule_id: rule.id.clone(),
        event_key: event.key.clone(),
        reason,
    };

    if !current.enabled_override.unwrap_or(rule.enabled) {
        return skip(SkipReason::Disabled);
    }
    if event.origin.is_some() {
        return skip(SkipReason::LoopSuppressed);
    }
    if !SUPPORTED_ACTION_CAPS.contains(&rule.action.cap.as_str()) {
        return skip(SkipReason::UnsupportedAction);
    }
    if rule.once_per_key && current.once_keys.contains(&event.key) {
        return skip(SkipReason::OncePerKey);
    }
    if current.last_fired_at.is_some_and(|last| {
        now_secs.saturating_sub(last) < i64::try_from(rule.debounce_secs).unwrap_or(i64::MAX)
    }) {
        return skip(SkipReason::Debounced);
    }

    let window_start = now_secs.saturating_sub(3_600);
    let recent_fires: Vec<i64> = current
        .recent_fires
        .iter()
        .copied()
        .filter(|ts| *ts > window_start && *ts <= now_secs)
        .collect();
    if recent_fires.len() >= usize::from(rule.max_per_hour) {
        return skip(SkipReason::RuleRateLimited);
    }
    let action_recent: Vec<i64> = current
        .action_fires
        .get(&rule.action.cap)
        .into_iter()
        .flatten()
        .copied()
        .filter(|ts| *ts > window_start && *ts <= now_secs)
        .collect();
    if action_recent.len() >= usize::from(rule.max_action_per_hour) {
        return skip(SkipReason::ActionRateLimited);
    }

    let vars = event.template_vars();
    let mut params = BTreeMap::new();
    for (name, template) in &rule.action.params {
        let Ok(value) = render_prompt(template, &vars) else {
            return skip(SkipReason::InvalidTemplate);
        };
        params.insert(name.clone(), value);
    }

    let mut next = current;
    next.last_fired_at = Some(now_secs);
    next.recent_fires = recent_fires;
    next.recent_fires.push(now_secs);
    next.action_fires
        .insert(rule.action.cap.clone(), action_recent);
    next.action_fires
        .entry(rule.action.cap.clone())
        .or_default()
        .push(now_secs);
    if rule.once_per_key {
        next.once_keys.insert(event.key.clone());
    }

    EvaluationDecision::Planned(PlannedAction {
        rule_id: rule.id.clone(),
        event_id: event.id.clone(),
        event_key: event.key.clone(),
        cap: rule.action.cap.clone(),
        params,
        transition: StateTransition {
            rule_id: rule.id.clone(),
            state: next,
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn event() -> AutomationEvent {
        AutomationEvent {
            id: "evt-1".into(),
            occurred_at: 1_000,
            key: EventKey("session:s1:blocked".into()),
            kind: AutomationEventKind::AgentNeedsYou,
            workspace: Some("product".into()),
            repo: Some("thegn".into()),
            worktree: Some("/code/product/thegn".into()),
            branch: Some("tg/the-21".into()),
            agent_role: Some("coder".into()),
            priority: Some(Priority::Alert),
            source_ref: Some("session:s1".into()),
            message: Some("needs review".into()),
            session_id: Some("s1".into()),
            pr_checks_passed: Some(true),
            pr_review_requested: Some(false),
            pr_merged: Some(false),
            origin: None,
        }
    }

    fn rule(id: &str) -> AutomationRule {
        AutomationRule {
            id: id.into(),
            enabled: true,
            event: AutomationEventKind::AgentNeedsYou,
            predicate: AutomationPredicate::default(),
            action: ActionTemplate {
                cap: "notify.push".into(),
                params: BTreeMap::from([
                    ("title".into(), "Agent {agent_role}".into()),
                    ("body".into(), "{message} on {branch}".into()),
                ]),
            },
            debounce_secs: 0,
            once_per_key: false,
            max_per_hour: 30,
            max_action_per_hour: 30,
        }
    }

    fn planned(decisions: &[EvaluationDecision]) -> &PlannedAction {
        match &decisions[0] {
            EvaluationDecision::Planned(plan) => plan,
            other => panic!("expected plan, got {other:?}"),
        }
    }

    fn reason(decisions: &[EvaluationDecision]) -> &SkipReason {
        match &decisions[0] {
            EvaluationDecision::Skipped { reason, .. } => reason,
            other => panic!("expected skip, got {other:?}"),
        }
    }

    #[test]
    fn matching_renders_params_and_transition() {
        let decisions = evaluate(&[rule("a")], &event(), &EvaluationState::new(), 2_000);
        let plan = planned(&decisions);
        assert_eq!(plan.params["title"], "Agent coder");
        assert_eq!(plan.params["body"], "needs review on tg/the-21");
        assert_eq!(plan.transition.state.last_fired_at, Some(2_000));
        assert_eq!(plan.transition.state.recent_fires, vec![2_000]);
    }

    #[test]
    fn every_predicate_matches_and_missing_fields_do_not() {
        let predicate = AutomationPredicate {
            workspace: Some("prod*".into()),
            repo: Some("the?n".into()),
            worktree: Some("*/thegn".into()),
            branch: Some("tg/*".into()),
            agent_role: Some("coder".into()),
            min_priority: Some(Priority::Notice),
            source_prefix: Some("session:".into()),
            message_regex: Some("review$".into()),
            session_id: Some("s1".into()),
            pr_checks_passed: Some(true),
            pr_review_requested: Some(false),
            pr_merged: Some(false),
        };
        assert!(predicate.matches(&event()));
        let mut missing = event();
        missing.branch = None;
        assert!(!predicate.matches(&missing));
        let mut wrong = event();
        wrong.pr_checks_passed = Some(false);
        assert!(!predicate.matches(&wrong));
    }

    #[test]
    fn kind_and_predicate_filter_without_audit_noise() {
        let mut other_kind = event();
        other_kind.kind = AutomationEventKind::DiskLow;
        assert!(evaluate(&[rule("a")], &other_kind, &EvaluationState::new(), 1).is_empty());

        let mut r = rule("a");
        r.predicate.branch = Some("main".into());
        assert!(evaluate(&[r], &event(), &EvaluationState::new(), 1).is_empty());
    }

    #[test]
    fn disabled_and_override_are_explicit() {
        let mut r = rule("a");
        r.enabled = false;
        assert_eq!(
            reason(&evaluate(
                &[r.clone()],
                &event(),
                &EvaluationState::new(),
                1
            )),
            &SkipReason::Disabled
        );
        let state = BTreeMap::from([(
            "a".into(),
            RuleState {
                enabled_override: Some(true),
                ..RuleState::default()
            },
        )]);
        assert!(matches!(
            evaluate(&[r], &event(), &state, 1)[0],
            EvaluationDecision::Planned(_)
        ));
    }

    #[test]
    fn debounce_uses_injected_time() {
        let mut r = rule("a");
        r.debounce_secs = 60;
        let state = BTreeMap::from([(
            "a".into(),
            RuleState {
                last_fired_at: Some(1_000),
                ..RuleState::default()
            },
        )]);
        assert_eq!(
            reason(&evaluate(&[r.clone()], &event(), &state, 1_059)),
            &SkipReason::Debounced
        );
        assert!(matches!(
            evaluate(&[r], &event(), &state, 1_060)[0],
            EvaluationDecision::Planned(_)
        ));
    }

    #[test]
    fn once_per_key_is_stable() {
        let mut r = rule("a");
        r.once_per_key = true;
        let state = BTreeMap::from([(
            "a".into(),
            RuleState {
                once_keys: BTreeSet::from([event().key]),
                ..RuleState::default()
            },
        )]);
        assert_eq!(
            reason(&evaluate(&[r], &event(), &state, 2_000)),
            &SkipReason::OncePerKey
        );
    }

    #[test]
    fn rule_and_action_sliding_windows_are_bounded() {
        let mut r = rule("a");
        r.max_per_hour = 2;
        let state = BTreeMap::from([(
            "a".into(),
            RuleState {
                recent_fires: vec![1, 6_500, 6_999],
                ..RuleState::default()
            },
        )]);
        assert_eq!(
            reason(&evaluate(&[r.clone()], &event(), &state, 7_000)),
            &SkipReason::RuleRateLimited
        );

        r.max_per_hour = 10;
        r.max_action_per_hour = 1;
        let action_state = BTreeMap::from([(
            "a".into(),
            RuleState {
                action_fires: BTreeMap::from([("notify.push".into(), vec![6_999])]),
                ..RuleState::default()
            },
        )]);
        assert_eq!(
            reason(&evaluate(&[r], &event(), &action_state, 7_000)),
            &SkipReason::ActionRateLimited
        );
    }

    #[test]
    fn origin_suppresses_every_matching_rule() {
        let mut e = event();
        e.origin = Some(AutomationOrigin {
            root_event_id: "root".into(),
            rule_id: "previous".into(),
            run_id: "run".into(),
        });
        let decisions = evaluate(&[rule("a"), rule("b")], &e, &EvaluationState::new(), 2_000);
        assert_eq!(decisions.len(), 2);
        assert!(decisions.iter().all(|d| matches!(
            d,
            EvaluationDecision::Skipped {
                reason: SkipReason::LoopSuppressed,
                ..
            }
        )));
    }

    #[test]
    fn unsupported_and_bad_templates_skip_explicitly() {
        let mut unsupported = rule("a");
        unsupported.action.cap = "shell.exec".into();
        assert_eq!(
            reason(&evaluate(
                &[unsupported],
                &event(),
                &EvaluationState::new(),
                1
            )),
            &SkipReason::UnsupportedAction
        );

        let mut invalid = rule("b");
        invalid
            .action
            .params
            .insert("body".into(), "{unknown}".into());
        assert_eq!(
            reason(&evaluate(&[invalid], &event(), &EvaluationState::new(), 1)),
            &SkipReason::InvalidTemplate
        );
    }

    #[test]
    fn decisions_preserve_config_order() {
        let decisions = evaluate(
            &[rule("third"), rule("first"), rule("second")],
            &event(),
            &EvaluationState::new(),
            2_000,
        );
        let ids: Vec<&str> = decisions
            .iter()
            .map(|d| match d {
                EvaluationDecision::Planned(p) => p.rule_id.as_str(),
                EvaluationDecision::Skipped { rule_id, .. } => rule_id.as_str(),
            })
            .collect();
        assert_eq!(ids, ["third", "first", "second"]);
    }
}
