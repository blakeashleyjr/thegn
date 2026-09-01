//! Pure policy and durable-value types for issue autopilot.
//!
//! The host owns all side effects.  This module only decides whether an issue
//! is eligible, whether a claim fits the configured budgets, and whether a
//! run's lifecycle transition is legal.

use crate::issue::{Issue, IssueStatus};
use serde::{Deserialize, Serialize};

/// Lifecycle of one claimed issue.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AutopilotState {
    Claimed,
    Working,
    PrOpened,
    Shepherding,
    NeedsHuman,
    Done,
    Stopped,
}

impl AutopilotState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Claimed => "claimed",
            Self::Working => "working",
            Self::PrOpened => "pr_opened",
            Self::Shepherding => "shepherding",
            Self::NeedsHuman => "needs_human",
            Self::Done => "done",
            Self::Stopped => "stopped",
        }
    }

    pub fn parse(value: &str) -> Self {
        match value {
            "claimed" => Self::Claimed,
            "working" => Self::Working,
            "pr_opened" => Self::PrOpened,
            "shepherding" => Self::Shepherding,
            "done" => Self::Done,
            "stopped" => Self::Stopped,
            _ => Self::NeedsHuman,
        }
    }

    pub fn is_terminal(self) -> bool {
        matches!(self, Self::NeedsHuman | Self::Done | Self::Stopped)
    }
}

/// A provider/account-qualified issue identity.  The stable issue id is kept
/// opaque; normalizing vendor-specific identifiers here would make claims
/// collide or diverge between providers.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct AutopilotIssueKey {
    pub provider: String,
    pub account: String,
    pub issue_id: String,
}

impl AutopilotIssueKey {
    pub fn new(
        provider: impl Into<String>,
        account: impl Into<String>,
        issue_id: impl Into<String>,
    ) -> Self {
        Self {
            provider: provider.into(),
            account: account.into(),
            issue_id: issue_id.into(),
        }
    }

    /// Stable storage key, intentionally length-delimited so component values
    /// containing `:` cannot be ambiguous.
    pub fn storage_key(&self) -> String {
        format!(
            "{}\u{001f}{}\u{001f}{}",
            self.provider, self.account, self.issue_id
        )
    }
}

/// A bounded, read-safe projection of a run.  It contains no issue body,
/// command expansion, credentials, or worker output.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AutopilotSummary {
    pub id: i64,
    pub key: AutopilotIssueKey,
    pub repo_root: String,
    pub worktree: Option<String>,
    pub branch: Option<String>,
    pub base_branch: Option<String>,
    pub state: AutopilotState,
    pub attempt: u32,
    pub dispatch_id: Option<i64>,
    pub pr_number: Option<u64>,
    pub pr_head: Option<String>,
    pub pr_url: Option<String>,
    pub created_at: i64,
    pub updated_at: i64,
    pub claimed_at: Option<i64>,
    pub finished_at: Option<i64>,
    pub reason: Option<String>,
}

/// The result of a pure lifecycle transition.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Transition {
    pub state: AutopilotState,
    pub attempt: u32,
    pub pr_number: Option<u64>,
    pub reason: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TransitionError(pub String);

impl std::fmt::Display for TransitionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for TransitionError {}

/// Maximum persisted diagnostic length.  This is a character bound and keeps
/// UTF-8 intact while ensuring status output remains small.
pub const MAX_REASON_CHARS: usize = 512;

pub fn bounded_reason(reason: Option<&str>) -> Option<String> {
    reason.map(|value| value.chars().take(MAX_REASON_CHARS).collect())
}

/// Exact eligibility matcher.  The caller supplies the provider provenance;
/// issue body text and assignee display names are never inspected.
pub fn matches_issue(
    issue: &Issue,
    trigger_label: &str,
    pickup_status: IssueStatus,
    from_assignee_me: bool,
) -> bool {
    from_assignee_me
        && issue.status == pickup_status
        && issue.labels.iter().any(|label| label == trigger_label)
}

/// Whether a new attempt fits both the active-run and per-run budgets.
pub fn can_claim(active_runs: usize, max_concurrent: u32, attempt: u32, max_attempts: u32) -> bool {
    active_runs < max_concurrent as usize && attempt < max_attempts
}

/// Validate and describe a lifecycle transition.  Terminal states cannot move
/// again, and transitions never go backward.
pub fn transition(
    current: AutopilotState,
    next: AutopilotState,
    attempt: u32,
    pr_number: Option<u64>,
    reason: Option<&str>,
) -> Result<Transition, TransitionError> {
    if current.is_terminal() {
        return Err(TransitionError(format!(
            "terminal state {} cannot transition",
            current.as_str()
        )));
    }
    let legal = matches!(
        (current, next),
        (
            AutopilotState::Claimed,
            AutopilotState::Working | AutopilotState::NeedsHuman | AutopilotState::Stopped
        ) | (
            AutopilotState::Working,
            AutopilotState::PrOpened | AutopilotState::NeedsHuman | AutopilotState::Stopped
        ) | (
            AutopilotState::PrOpened,
            AutopilotState::Shepherding
                | AutopilotState::Done
                | AutopilotState::NeedsHuman
                | AutopilotState::Stopped
        ) | (
            AutopilotState::Shepherding,
            AutopilotState::Done | AutopilotState::NeedsHuman | AutopilotState::Stopped
        )
    );
    if !legal {
        return Err(TransitionError(format!(
            "illegal transition {} -> {}",
            current.as_str(),
            next.as_str()
        )));
    }
    Ok(Transition {
        state: next,
        attempt,
        pr_number,
        reason: bounded_reason(reason),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn issue() -> Issue {
        Issue {
            status: IssueStatus::Todo,
            labels: vec!["agent-ready".into()],
            ..Issue::default()
        }
    }

    #[test]
    fn matching_is_exact_and_requires_provider_consent() {
        let i = issue();
        assert!(matches_issue(&i, "agent-ready", IssueStatus::Todo, true));
        assert!(!matches_issue(&i, "Agent-ready", IssueStatus::Todo, true));
        assert!(!matches_issue(
            &i,
            "agent-ready",
            IssueStatus::Backlog,
            true
        ));
        assert!(!matches_issue(&i, "agent-ready", IssueStatus::Todo, false));
    }

    #[test]
    fn claim_budget_is_strict() {
        assert!(can_claim(0, 1, 0, 1));
        assert!(!can_claim(1, 1, 0, 1));
        assert!(!can_claim(0, 1, 1, 1));
    }

    #[test]
    fn transitions_reject_backward_and_terminal_moves() {
        assert!(
            transition(
                AutopilotState::Claimed,
                AutopilotState::Working,
                1,
                None,
                None
            )
            .is_ok()
        );
        assert!(
            transition(
                AutopilotState::Working,
                AutopilotState::Claimed,
                1,
                None,
                None
            )
            .is_err()
        );
        assert!(transition(AutopilotState::Done, AutopilotState::Working, 1, None, None).is_err());
    }

    #[test]
    fn reasons_are_bounded_without_splitting_utf8() {
        let out = bounded_reason(Some(&"é".repeat(MAX_REASON_CHARS + 4))).unwrap();
        assert_eq!(out.chars().count(), MAX_REASON_CHARS);
        assert!(out.len() > MAX_REASON_CHARS);
    }
}
