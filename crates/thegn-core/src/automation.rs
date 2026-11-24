//! Pure automation rule matching and planning.
//!
//! This module deliberately knows nothing about SQLite, executors, providers,
//! filesystems, or the host event loop. Callers supply the event, persisted
//! ledger state, and current time; the result contains either an auditable skip
//! or a rendered catalog action plus the exact ledger transition to persist.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

use crate::agent_task::{TaskVars, render_prompt};
use crate::automation_autonomy::Autonomy;
use crate::notification::{NotificationKind, Priority};

const MAX_ONCE_KEYS: usize = 2_048;

/// Actions automation rules may target: catalog capabilities plus the
/// [`RULE_ONLY_ACTIONS`].
pub const SUPPORTED_ACTION_CAPS: &[&str] = &[
    "sessions.open",
    "merge.add",
    "notify.push",
    "tools.run",
    "merge.drain",
    "pipeline.start",
    "lead.wake",
    "autopilot.run",
];

/// Actions that are not catalog capabilities; reachable only by a rule.
/// They have no MCP tool, HTTP route, CLI verb or Lead tool of their own, so
/// a Lead reaches them only by authoring a rule, which the autonomy policy
/// (`automation_autonomy`) constrains.
pub const RULE_ONLY_ACTIONS: &[&str] = &["pipeline.start", "lead.wake", "autopilot.run"];

/// Longest issue title an [`IssueFacts`] carries (characters).
pub const MAX_ISSUE_TITLE_CHARS: usize = 300;

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
    "notification_kind",
    "priority",
    "source_ref",
    "message",
    "session_id",
    "pr_checks_passed",
    "pr_review_requested",
    "pr_merged",
    "issue_id",
    "issue_number",
    "issue_url",
    "issue_status",
    "account",
    "exit_code",
    "inbox_account",
    "inbox_kind",
];

/// Variables a `lead.wake` prompt template may use: [`EVENT_TEMPLATE_VARS`]
/// minus `message`. Untrusted free text (notification bodies, issue titles)
/// reaches the Lead only inside the bounded, delimited event block, never
/// spliced into the prompt that instructs it.
pub const LEAD_WAKE_TEMPLATE_VARS: &[&str] = &[
    "event_id",
    "event_key",
    "event_kind",
    "event_time",
    "workspace",
    "repo",
    "worktree",
    "branch",
    "agent_role",
    "notification_kind",
    "priority",
    "source_ref",
    "session_id",
    "pr_checks_passed",
    "pr_review_requested",
    "pr_merged",
    "issue_id",
    "issue_number",
    "issue_url",
    "issue_status",
    "account",
    "exit_code",
    "inbox_account",
    "inbox_kind",
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
    /// A rule's own cron/interval schedule came due (daemon producer).
    ScheduleDue,
    /// A tracker refresh surfaced an issue matching an `issue_matched` rule.
    IssueMatched,
    /// Any daemon session exited, whatever its role or status.
    SessionExited,
    /// An item landed in the native inbox: produced by the daemon inbox
    /// producer (b163), for new ids after an account's baseline poll.
    InboxItem,
}

impl AutomationEventKind {
    pub const ALL: [Self; 13] = [
        Self::Notification,
        Self::AgentNeedsYou,
        Self::AgentFinished,
        Self::AgentFailed,
        Self::PrChecks,
        Self::PrReviewRequested,
        Self::MergeLanded,
        Self::WorktreeIdle,
        Self::DiskLow,
        Self::ScheduleDue,
        Self::IssueMatched,
        Self::SessionExited,
        Self::InboxItem,
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
            Self::ScheduleDue => "schedule_due",
            Self::IssueMatched => "issue_matched",
            Self::SessionExited => "session_exited",
            Self::InboxItem => "inbox_item",
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

/// Tracker facts carried by an `issue_matched` event. Every field except
/// `title` is structural (ids, enum-like status, labels, provenance).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct IssueFacts {
    pub provider: String,
    pub account: String,
    pub issue_id: String,
    pub number: String,
    pub url: String,
    pub status: String,
    pub labels: Vec<String>,
    pub assignees: Vec<String>,
    pub project_ids: Vec<String>,
    /// Provenance: the tracker reported the issue as assigned to the
    /// configured account's own user. Never derived from display names.
    pub assigned_to_me: bool,
    /// UNTRUSTED, <= 300 chars. Reaches rules only as `message`; never a template var of its own.
    pub title: String,
}

/// Inbox facts carried by an `inbox_item` event (THE-2214). Every field except
/// `title` is structural; all of it passed `inbox::sanitize_item`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct InboxFacts {
    /// The account's kind (`command`, `todoist`, `imap`).
    pub kind: String,
    /// The `[[inbox.accounts]]` name.
    pub account: String,
    pub item_id: String,
    pub url: String,
    pub labels: Vec<String>,
    /// UNTRUSTED, <= 300 chars; reaches rules only as `message`.
    pub title: String,
}

/// Stable, optional fields projected from existing host/daemon facts.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AutomationEvent {
    pub id: String,
    /// Unix seconds. Matching never reads a clock directly.
    pub occurred_at: i64,
    pub key: EventKey,
    pub kind: AutomationEventKind,
    /// Runtime-only targeting for independently-timed idle rules. Ordinary
    /// events leave this absent and are evaluated against every matching rule.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target_rule: Option<String>,
    pub workspace: Option<String>,
    pub repo: Option<String>,
    pub worktree: Option<String>,
    pub branch: Option<String>,
    pub agent_role: Option<String>,
    pub notification_kind: Option<NotificationKind>,
    pub priority: Option<Priority>,
    pub source_ref: Option<String>,
    pub message: Option<String>,
    pub session_id: Option<String>,
    pub pr_checks_passed: Option<bool>,
    pub pr_review_requested: Option<bool>,
    pub pr_merged: Option<bool>,
    pub origin: Option<AutomationOrigin>,
    /// Tracker facts; present only on `issue_matched` events.
    #[serde(default)]
    pub issue: Option<IssueFacts>,
    /// Process exit status; present only on `session_exited` events whose
    /// session reported one.
    #[serde(default)]
    pub exit_code: Option<i32>,
    /// Inbox facts; present only on `inbox_item` events.
    #[serde(default)]
    pub inbox: Option<InboxFacts>,
}

impl AutomationEvent {
    /// An event with only its identity set: every optional fact absent. The
    /// pure producers below start here and fill what they know.
    pub fn bare(kind: AutomationEventKind, key: impl Into<String>, occurred_at: i64) -> Self {
        let key = key.into();
        Self {
            id: event_id(kind, &key, occurred_at),
            occurred_at,
            key: EventKey(key),
            kind,
            target_rule: None,
            workspace: None,
            repo: None,
            worktree: None,
            branch: None,
            agent_role: None,
            notification_kind: None,
            priority: None,
            source_ref: None,
            message: None,
            session_id: None,
            pr_checks_passed: None,
            pr_review_requested: None,
            pr_merged: None,
            origin: None,
            issue: None,
            exit_code: None,
            inbox: None,
        }
    }

    fn template_vars(&self) -> TaskVars {
        let issue = self.issue.as_ref();
        let issue_field = |field: fn(&IssueFacts) -> &str| issue.map(field).unwrap_or_default();
        let inbox = self.inbox.as_ref();
        let inbox_field = |field: fn(&InboxFacts) -> &str| inbox.map(field).unwrap_or_default();
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
                "notification_kind",
                self.notification_kind
                    .map(NotificationKind::as_str)
                    .unwrap_or_default(),
            )
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
            .set("issue_id", issue_field(|facts| facts.issue_id.as_str()))
            .set("issue_number", issue_field(|facts| facts.number.as_str()))
            .set("issue_url", issue_field(|facts| facts.url.as_str()))
            .set("issue_status", issue_field(|facts| facts.status.as_str()))
            .set("account", issue_field(|facts| facts.account.as_str()))
            .set(
                "exit_code",
                self.exit_code
                    .map(|code| code.to_string())
                    .unwrap_or_default(),
            )
            .set("inbox_account", inbox_field(|facts| facts.account.as_str()))
            .set("inbox_kind", inbox_field(|facts| facts.kind.as_str()))
    }

    /// Typed PR fixtures must carry the fact their event kind promises. This
    /// keeps the dry-run surface from advertising matches no live producer can
    /// construct.
    pub fn validate_required_facts(&self) -> Result<(), &'static str> {
        match self.kind {
            AutomationEventKind::PrChecks if self.pr_checks_passed.is_none() => {
                Err("pr_checks events require pr_checks_passed")
            }
            AutomationEventKind::PrReviewRequested if self.pr_review_requested.is_none() => {
                Err("pr_review_requested events require pr_review_requested")
            }
            _ => Ok(()),
        }
    }
}

/// Deterministic event id from the producer's identity: the same kind, key
/// and instant always name the same event (FNV-1a, stable across builds).
pub fn event_id(kind: AutomationEventKind, key: &str, occurred_at: i64) -> String {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in kind
        .as_str()
        .bytes()
        .chain([0])
        .chain(key.bytes())
        .chain([0])
        .chain(occurred_at.to_le_bytes())
    {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x0100_0000_01b3);
    }
    format!("ae-{occurred_at}-{hash:016x}")
}

/// A daemon session exited. Always produced on a session exit, alongside the
/// role-specific `agent_finished`/`agent_failed` facts.
pub fn session_exited_event(
    session: &str,
    worktree: Option<String>,
    exit_code: Option<i32>,
    origin: Option<AutomationOrigin>,
    now: i64,
) -> AutomationEvent {
    let mut event = AutomationEvent::bare(
        AutomationEventKind::SessionExited,
        format!("session:{session}:exited"),
        now,
    );
    event.worktree = worktree;
    event.session_id = Some(session.to_string());
    event.source_ref = Some(format!("session:{session}"));
    event.exit_code = exit_code;
    event.origin = origin;
    event
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
    pub notification_kind: Option<NotificationKind>,
    pub min_priority: Option<Priority>,
    pub source_prefix: Option<String>,
    pub message_regex: Option<String>,
    pub session_id: Option<String>,
    pub pr_checks_passed: Option<bool>,
    pub pr_review_requested: Option<bool>,
    pub pr_merged: Option<bool>,
    /// Issue selectors. Each is false on an event without [`IssueFacts`].
    /// Tracker account, exact.
    pub account: Option<String>,
    /// One of the issue's project/sprint/milestone ids, exact.
    pub project: Option<String>,
    /// One of the issue's labels, ASCII case-insensitive.
    pub label: Option<String>,
    /// `"me"` means the tracker's own assigned-to-me provenance; any other
    /// value must equal one of the issue's assignees exactly.
    pub assignee: Option<String>,
    /// The issue status, ASCII case-insensitive.
    pub status: Option<String>,
    /// Inbox selectors. Each is false on an event without [`InboxFacts`].
    /// Inbox account name, exact.
    #[serde(default)]
    pub inbox_account: Option<String>,
    /// Inbox account kind, exact.
    #[serde(default)]
    pub inbox_kind: Option<String>,
    /// One of the item's labels, ASCII case-insensitive.
    #[serde(default)]
    pub inbox_label: Option<String>,
}

impl AutomationPredicate {
    pub fn matches(&self, event: &AutomationEvent) -> bool {
        glob_field(self.workspace.as_ref(), event.workspace.as_ref())
            && glob_field(self.repo.as_ref(), event.repo.as_ref())
            && glob_field(self.worktree.as_ref(), event.worktree.as_ref())
            && glob_field(self.branch.as_ref(), event.branch.as_ref())
            && exact_field(self.agent_role.as_ref(), event.agent_role.as_ref())
            && self
                .notification_kind
                .is_none_or(|kind| event.notification_kind == Some(kind))
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
            && exact_field(self.session_id.as_ref(), event.session_id.as_ref())
            && bool_field(self.pr_checks_passed, event.pr_checks_passed)
            && bool_field(self.pr_review_requested, event.pr_review_requested)
            && bool_field(self.pr_merged, event.pr_merged)
            && self.matches_issue(event.issue.as_ref())
            && self.matches_inbox(event.inbox.as_ref())
    }

    fn matches_inbox(&self, inbox: Option<&InboxFacts>) -> bool {
        let has = |selector: Option<&String>, test: &dyn Fn(&InboxFacts, &str) -> bool| {
            selector.is_none_or(|want| inbox.is_some_and(|facts| test(facts, want)))
        };
        has(self.inbox_account.as_ref(), &|facts, want| {
            facts.account == want
        }) && has(self.inbox_kind.as_ref(), &|facts, want| facts.kind == want)
            && has(self.inbox_label.as_ref(), &|facts, want| {
                facts
                    .labels
                    .iter()
                    .any(|label| label.eq_ignore_ascii_case(want))
            })
    }

    fn matches_issue(&self, issue: Option<&IssueFacts>) -> bool {
        let has = |selector: Option<&String>, test: &dyn Fn(&IssueFacts, &str) -> bool| {
            selector.is_none_or(|want| issue.is_some_and(|facts| test(facts, want)))
        };
        has(self.account.as_ref(), &|facts, want| facts.account == want)
            && has(self.project.as_ref(), &|facts, want| {
                facts.project_ids.iter().any(|id| id == want)
            })
            && has(self.label.as_ref(), &|facts, want| {
                facts
                    .labels
                    .iter()
                    .any(|label| label.eq_ignore_ascii_case(want))
            })
            && has(self.assignee.as_ref(), &|facts, want| {
                if want == "me" {
                    facts.assigned_to_me
                } else {
                    facts.assignees.iter().any(|who| who == want)
                }
            })
            && has(self.status.as_ref(), &|facts, want| {
                facts.status.eq_ignore_ascii_case(want)
            })
    }
}

fn glob_field(pattern: Option<&String>, value: Option<&String>) -> bool {
    pattern.is_none_or(|pattern| {
        value.is_some_and(|value| crate::notification_route::glob_match(pattern, value))
    })
}

fn exact_field(want: Option<&String>, actual: Option<&String>) -> bool {
    want.is_none_or(|want| actual.is_some_and(|actual| actual == want))
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
    /// Delay after the authoritative activity edge for `worktree_idle` only.
    pub idle_secs: Option<u64>,
    pub once_per_key: bool,
    pub max_per_hour: u16,
    pub max_action_per_hour: u16,
    /// The rule's own schedule; present only for `schedule_due` rules.
    #[serde(default)]
    pub schedule: Option<crate::automation_schedule::Schedule>,
    /// Effective autonomy ceiling, after the policy caps of
    /// [`crate::automation_autonomy::effective_autonomy`].
    #[serde(default)]
    pub autonomy: Autonomy,
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
    /// The rule's effective autonomy, recorded on the audit row.
    #[serde(default)]
    pub autonomy: Autonomy,
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
    /// The runtime kill switch (`automations.pause`) was on.
    Paused,
    /// The rule's previous run is still live (a session it opened, or an
    /// unfinished audit row inside the crash window).
    PreviousRunLive,
    /// An automation budget (`[automations] max_concurrent_lanes` /
    /// `max_spawns_per_day`) refused the spawn (6e).
    Budget,
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
    // Apply accepted transitions to a scratch ledger in config order. This is
    // what makes max_action_per_hour action-wide when two rules target the same
    // capability in one event, and mirrors the transactional runtime admission.
    let mut scratch = state.clone();
    let mut decisions = Vec::new();
    for rule in rules.iter().filter(|rule| {
        event.target_rule.as_ref().is_none_or(|id| id == &rule.id)
            && rule.event == event.kind
            && rule.predicate.matches(event)
    }) {
        let decision = evaluate_one(rule, event, &scratch, now_secs);
        if let EvaluationDecision::Planned(action) = &decision {
            scratch.insert(
                action.transition.rule_id.clone(),
                action.transition.state.clone(),
            );
        }
        decisions.push(decision);
    }
    decisions
}

fn evaluate_one(
    rule: &AutomationRule,
    event: &AutomationEvent,
    state: &EvaluationState,
    now_secs: i64,
) -> EvaluationDecision {
    let current = state.get(&rule.id).cloned().unwrap_or_default();
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
    let action_recent: Vec<i64> = state
        .values()
        .filter_map(|ledger| ledger.action_fires.get(&rule.action.cap))
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
    // Each rule owns only the fires it admitted. The limit is action-wide by
    // summing those disjoint ledgers above; copying the global window into the
    // firing rule would double-count old entries on every transition.
    next.action_fires.retain(|_, timestamps| {
        timestamps.retain(|ts| *ts > window_start && *ts <= now_secs);
        !timestamps.is_empty()
    });
    next.action_fires
        .entry(rule.action.cap.clone())
        .or_default()
        .push(now_secs);
    if rule.once_per_key {
        next.once_keys.insert(event.key.clone());
        while next.once_keys.len() > MAX_ONCE_KEYS {
            let Some(oldest) = next.once_keys.first().cloned() else {
                break;
            };
            next.once_keys.remove(&oldest);
        }
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
        autonomy: rule.autonomy,
    })
}

/// The paused runtime's view of a decision list: every would-be plan becomes
/// an explicit `Paused` skip (its ledger transition is discarded); policy
/// skips keep their own reason. Pure, so the kill switch is testable without
/// a database.
pub fn paused_decisions(decisions: Vec<EvaluationDecision>) -> Vec<EvaluationDecision> {
    skipped_decisions(decisions, SkipReason::Paused)
}

/// Every would-be plan becomes an explicit skip with `reason` (its ledger
/// transition is discarded); policy skips keep their own reason. The general
/// form of [`paused_decisions`], also used for pre-admission budget refusals.
pub fn skipped_decisions(
    decisions: Vec<EvaluationDecision>,
    reason: SkipReason,
) -> Vec<EvaluationDecision> {
    decisions
        .into_iter()
        .map(|decision| match decision {
            EvaluationDecision::Planned(action) => EvaluationDecision::Skipped {
                rule_id: action.rule_id,
                event_key: action.event_key,
                reason: reason.clone(),
            },
            skipped @ EvaluationDecision::Skipped { .. } => skipped,
        })
        .collect()
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
            target_rule: None,
            workspace: Some("product".into()),
            repo: Some("thegn".into()),
            worktree: Some("/code/product/thegn".into()),
            branch: Some("tg/the-21".into()),
            agent_role: Some("coder".into()),
            notification_kind: None,
            priority: Some(Priority::Alert),
            source_ref: Some("session:s1".into()),
            message: Some("needs review".into()),
            session_id: Some("s1".into()),
            pr_checks_passed: Some(true),
            pr_review_requested: Some(false),
            pr_merged: Some(false),
            origin: None,
            issue: None,
            exit_code: None,
            inbox: None,
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
            idle_secs: None,
            once_per_key: false,
            max_per_hour: 30,
            max_action_per_hour: 30,
            schedule: None,
            autonomy: Autonomy::Review,
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
            notification_kind: None,
            min_priority: Some(Priority::Notice),
            source_prefix: Some("session:".into()),
            message_regex: Some("review$".into()),
            session_id: Some("s1".into()),
            pr_checks_passed: Some(true),
            pr_review_requested: Some(false),
            pr_merged: Some(false),
            ..AutomationPredicate::default()
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
    fn notification_kind_is_typed_and_pr_fixtures_require_their_fact() {
        let mut notification = event();
        notification.kind = AutomationEventKind::Notification;
        notification.notification_kind = Some(NotificationKind::TestFailed);
        let mut r = rule("notification");
        r.event = AutomationEventKind::Notification;
        r.predicate.notification_kind = Some(NotificationKind::TestFailed);
        assert!(matches!(
            evaluate(&[r], &notification, &EvaluationState::new(), 2_000)[0],
            EvaluationDecision::Planned(_)
        ));

        let mut checks = event();
        checks.kind = AutomationEventKind::PrChecks;
        checks.pr_checks_passed = None;
        assert_eq!(
            checks.validate_required_facts(),
            Err("pr_checks events require pr_checks_passed")
        );
    }

    #[test]
    fn action_rate_is_shared_across_rules() {
        let mut first = rule("first");
        let mut second = rule("second");
        first.max_action_per_hour = 1;
        second.max_action_per_hour = 1;
        let decisions = evaluate(&[first, second], &event(), &EvaluationState::new(), 2_000);
        assert!(matches!(decisions[0], EvaluationDecision::Planned(_)));
        assert!(matches!(
            decisions[1],
            EvaluationDecision::Skipped {
                reason: SkipReason::ActionRateLimited,
                ..
            }
        ));

        // Each rule persists only its own contribution to the shared window.
        // Two initial admissions therefore consume exactly two slots, not a
        // copied/compounded three slots in the persisted ledgers.
        let mut first = rule("first");
        let mut second = rule("second");
        first.max_action_per_hour = 3;
        second.max_action_per_hour = 3;
        let rules = vec![first, second];
        let initial = evaluate(&rules, &event(), &EvaluationState::new(), 2_000);
        assert!(
            initial
                .iter()
                .all(|decision| matches!(decision, EvaluationDecision::Planned(_)))
        );
        let state: EvaluationState = initial
            .into_iter()
            .filter_map(|decision| match decision {
                EvaluationDecision::Planned(action) => {
                    Some((action.transition.rule_id, action.transition.state))
                }
                EvaluationDecision::Skipped { .. } => None,
            })
            .collect();
        let next = evaluate(&rules, &event(), &state, 2_001);
        assert!(matches!(next[0], EvaluationDecision::Planned(_)));
        assert!(matches!(
            next[1],
            EvaluationDecision::Skipped {
                reason: SkipReason::ActionRateLimited,
                ..
            }
        ));
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

    #[test]
    fn every_kind_round_trips_and_all_covers_every_name() {
        let names: BTreeSet<&str> = AutomationEventKind::ALL
            .into_iter()
            .map(AutomationEventKind::as_str)
            .collect();
        assert_eq!(names.len(), AutomationEventKind::ALL.len());
        for kind in AutomationEventKind::ALL {
            assert_eq!(AutomationEventKind::parse(kind.as_str()), Some(kind));
            let json = serde_json::to_string(&kind).unwrap();
            assert_eq!(json, format!("\"{}\"", kind.as_str()));
            assert_eq!(
                serde_json::from_str::<AutomationEventKind>(&json).unwrap(),
                kind
            );
        }
        for name in [
            "schedule_due",
            "issue_matched",
            "session_exited",
            "inbox_item",
        ] {
            assert!(names.contains(name), "{name}");
        }
        assert_eq!(AutomationEventKind::parse("nope"), None);
    }

    fn issue_facts() -> IssueFacts {
        IssueFacts {
            provider: "linear".into(),
            account: "work".into(),
            issue_id: "linear:THE-1".into(),
            number: "THE-1".into(),
            url: "https://example.invalid/THE-1".into(),
            status: "In Progress".into(),
            labels: vec!["Bug".into(), "auto".into()],
            assignees: vec!["Ada".into()],
            project_ids: vec!["proj-1".into()],
            assigned_to_me: false,
            title: "fix the thing".into(),
        }
    }

    #[test]
    fn issue_selectors_match_their_fields_and_never_a_missing_issue() {
        let mut with_issue = event();
        with_issue.issue = Some(issue_facts());
        let cases: [(&str, AutomationPredicate, bool); 10] = [
            (
                "account",
                AutomationPredicate {
                    account: Some("work".into()),
                    ..Default::default()
                },
                true,
            ),
            (
                "account case",
                AutomationPredicate {
                    account: Some("Work".into()),
                    ..Default::default()
                },
                false,
            ),
            (
                "project",
                AutomationPredicate {
                    project: Some("proj-1".into()),
                    ..Default::default()
                },
                true,
            ),
            (
                "project miss",
                AutomationPredicate {
                    project: Some("proj-2".into()),
                    ..Default::default()
                },
                false,
            ),
            (
                "label case",
                AutomationPredicate {
                    label: Some("BUG".into()),
                    ..Default::default()
                },
                true,
            ),
            (
                "label miss",
                AutomationPredicate {
                    label: Some("feature".into()),
                    ..Default::default()
                },
                false,
            ),
            (
                "status case",
                AutomationPredicate {
                    status: Some("in progress".into()),
                    ..Default::default()
                },
                true,
            ),
            (
                "status miss",
                AutomationPredicate {
                    status: Some("done".into()),
                    ..Default::default()
                },
                false,
            ),
            (
                "assignee exact",
                AutomationPredicate {
                    assignee: Some("Ada".into()),
                    ..Default::default()
                },
                true,
            ),
            (
                "assignee case",
                AutomationPredicate {
                    assignee: Some("ada".into()),
                    ..Default::default()
                },
                false,
            ),
        ];
        for (name, predicate, expected) in cases {
            assert_eq!(predicate.matches(&with_issue), expected, "{name}");
            // Every issue selector is false when the event carries no issue.
            assert!(!predicate.matches(&event()), "{name} without an issue");
        }
    }

    #[test]
    fn assignee_me_is_provenance_not_a_display_name() {
        let me = AutomationPredicate {
            assignee: Some("me".into()),
            ..AutomationPredicate::default()
        };
        let mut facts = issue_facts();
        facts.assignees = vec!["me".into()];
        facts.assigned_to_me = false;
        let mut e = event();
        e.issue = Some(facts.clone());
        assert!(
            !me.matches(&e),
            "a user literally named `me` is not provenance"
        );
        facts.assignees.clear();
        facts.assigned_to_me = true;
        e.issue = Some(facts);
        assert!(me.matches(&e));
    }

    #[test]
    fn issue_and_exit_template_vars_render_and_default_empty() {
        let mut r = rule("a");
        r.action.params = BTreeMap::from([(
            "body".into(),
            "{issue_id}|{issue_number}|{issue_url}|{issue_status}|{account}|{exit_code}".into(),
        )]);
        let mut e = event();
        e.issue = Some(issue_facts());
        e.exit_code = Some(3);
        let decisions = evaluate(std::slice::from_ref(&r), &e, &EvaluationState::new(), 2_000);
        assert_eq!(
            planned(&decisions).params["body"],
            "linear:THE-1|THE-1|https://example.invalid/THE-1|In Progress|work|3"
        );
        let decisions = evaluate(&[r], &event(), &EvaluationState::new(), 2_000);
        assert_eq!(planned(&decisions).params["body"], "|||||");
        assert!(!EVENT_TEMPLATE_VARS.contains(&"title"));
        assert!(!LEAD_WAKE_TEMPLATE_VARS.contains(&"message"));
        assert_eq!(LEAD_WAKE_TEMPLATE_VARS.len() + 1, EVENT_TEMPLATE_VARS.len());
        assert!(
            LEAD_WAKE_TEMPLATE_VARS
                .iter()
                .all(|var| EVENT_TEMPLATE_VARS.contains(var))
        );
    }

    fn inbox_facts() -> InboxFacts {
        InboxFacts {
            kind: "imap".into(),
            account: "work".into(),
            item_id: "m1".into(),
            url: "https://example.invalid/m1".into(),
            labels: vec!["Urgent".into()],
            title: "hello".into(),
        }
    }

    #[test]
    fn inbox_selectors_match_inbox_facts_and_never_an_event_without_them() {
        let mut with_inbox = event();
        with_inbox.inbox = Some(inbox_facts());
        let p = |f: fn(&mut AutomationPredicate)| {
            let mut predicate = AutomationPredicate::default();
            f(&mut predicate);
            predicate
        };
        let cases = [
            (
                "account",
                p(|x| x.inbox_account = Some("work".into())),
                true,
            ),
            (
                "account other",
                p(|x| x.inbox_account = Some("Work".into())),
                false,
            ),
            ("kind", p(|x| x.inbox_kind = Some("imap".into())), true),
            (
                "kind other",
                p(|x| x.inbox_kind = Some("todoist".into())),
                false,
            ),
            (
                "label any case",
                p(|x| x.inbox_label = Some("URGENT".into())),
                true,
            ),
            (
                "label other",
                p(|x| x.inbox_label = Some("later".into())),
                false,
            ),
            (
                "all",
                p(|x| {
                    x.inbox_account = Some("work".into());
                    x.inbox_kind = Some("imap".into());
                    x.inbox_label = Some("urgent".into());
                }),
                true,
            ),
        ];
        for (name, predicate, expected) in cases {
            assert_eq!(predicate.matches(&with_inbox), expected, "{name}");
            assert!(!predicate.matches(&event()), "{name} without inbox facts");
        }
        // An issue selector never matches inbox facts.
        let issue_only = p(|x| x.account = Some("work".into()));
        assert!(!issue_only.matches(&with_inbox));
    }

    #[test]
    fn inbox_template_vars_render_and_default_empty() {
        let mut r = rule("a");
        r.action.params = BTreeMap::from([(
            "body".into(),
            "{inbox_kind}|{inbox_account}|{message}".into(),
        )]);
        let mut e = event();
        e.inbox = Some(inbox_facts());
        e.message = Some("[work] hello".into());
        let decisions = evaluate(std::slice::from_ref(&r), &e, &EvaluationState::new(), 2_000);
        assert_eq!(planned(&decisions).params["body"], "imap|work|[work] hello");
        let mut bare = event();
        bare.message = None;
        let decisions = evaluate(&[r], &bare, &EvaluationState::new(), 2_000);
        assert_eq!(planned(&decisions).params["body"], "||");
        for var in ["inbox_account", "inbox_kind"] {
            assert!(EVENT_TEMPLATE_VARS.contains(&var), "{var}");
            assert!(LEAD_WAKE_TEMPLATE_VARS.contains(&var), "{var}");
        }
        assert!(!EVENT_TEMPLATE_VARS.contains(&"inbox_title"));
    }

    #[test]
    fn autopilot_run_is_a_rule_only_action() {
        assert!(SUPPORTED_ACTION_CAPS.contains(&"autopilot.run"));
        assert!(RULE_ONLY_ACTIONS.contains(&"autopilot.run"));
    }

    #[test]
    fn session_exited_event_is_keyed_by_session() {
        let e = session_exited_event("s9", Some("/wt".into()), Some(1), None, 77);
        assert_eq!(e.kind, AutomationEventKind::SessionExited);
        assert_eq!(e.key.0, "session:s9:exited");
        assert_eq!(e.session_id.as_deref(), Some("s9"));
        assert_eq!(e.worktree.as_deref(), Some("/wt"));
        assert_eq!(e.exit_code, Some(1));
        assert_eq!(e.occurred_at, 77);
        assert_eq!(e.id, session_exited_event("s9", None, None, None, 77).id);
        assert_ne!(e.id, session_exited_event("s9", None, None, None, 78).id);
    }

    #[test]
    fn events_without_new_fields_still_deserialize() {
        let mut json = serde_json::to_value(event()).unwrap();
        let object = json.as_object_mut().unwrap();
        object.remove("issue");
        object.remove("exit_code");
        object.remove("inbox");
        let decoded: AutomationEvent = serde_json::from_value(json).unwrap();
        assert_eq!(decoded, event());
    }

    #[test]
    fn planned_actions_carry_the_rule_autonomy() {
        let mut r = rule("a");
        r.autonomy = Autonomy::Propose;
        let decisions = evaluate(&[r], &event(), &EvaluationState::new(), 2_000);
        assert_eq!(planned(&decisions).autonomy, Autonomy::Propose);
    }

    #[test]
    fn paused_decisions_turn_every_plan_into_a_paused_skip() {
        let mut disabled = rule("off");
        disabled.enabled = false;
        let decisions = evaluate(
            &[rule("a"), disabled],
            &event(),
            &EvaluationState::new(),
            2_000,
        );
        let paused = paused_decisions(decisions);
        assert_eq!(paused.len(), 2);
        assert!(matches!(
            &paused[0],
            EvaluationDecision::Skipped { rule_id, reason: SkipReason::Paused, .. } if rule_id == "a"
        ));
        assert!(matches!(
            &paused[1],
            EvaluationDecision::Skipped {
                reason: SkipReason::Disabled,
                ..
            }
        ));
    }

    #[test]
    fn rule_only_actions_are_supported_rule_targets() {
        for cap in RULE_ONLY_ACTIONS {
            assert!(SUPPORTED_ACTION_CAPS.contains(cap), "{cap}");
            assert!(
                crate::capability::lookup(cap).is_none(),
                "{cap} is not a catalog cap"
            );
        }
        assert!(SUPPORTED_ACTION_CAPS.contains(&"merge.drain"));
    }
}
