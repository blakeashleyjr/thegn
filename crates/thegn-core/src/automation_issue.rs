//! `issue_matched` producer inputs (THE-2194).
//!
//! Pure: the tracker-refresh job (thegn-host `automation_issue`) diffs the
//! previous and fresh issue lists here and submits one event per candidate.
//! Phase 3 moves that call into the daemon unchanged.
//!
//! The diff and the fact projection are filled by b153 lane 3.

use crate::automation::{AutomationEvent, AutomationEventKind, IssueFacts, MAX_ISSUE_TITLE_CHARS};
use crate::issue::Issue;

/// Upper bound on events one refresh may submit; the rest wait for the next.
pub const MAX_ISSUE_EVENTS_PER_REFRESH: usize = 50;

/// Issues in `new` worth an `issue_matched` event. `had_cache` is false on a
/// cold first refresh.
#[allow(dead_code, unused_variables, reason = "filled by b153 lane 3")]
pub fn matched_candidates<'a>(old: &[Issue], new: &'a [Issue], had_cache: bool) -> Vec<&'a Issue> {
    vec![]
}

/// Project a tracker issue onto the structural facts rules can match.
#[allow(dead_code, unused_variables, reason = "filled by b153 lane 3")]
pub fn facts_from_issue(issue: &Issue, account: &str, assigned_to_me: bool) -> IssueFacts {
    IssueFacts {
        provider: issue.provider.clone(),
        account: account.to_string(),
        issue_id: issue.id.clone(),
        ..IssueFacts::default()
    }
}

/// The event one matched issue produces. The (untrusted, bounded) title rides
/// only as `message`; the key is the issue's identity per account, so a
/// `once_per_key` rule fires once per issue.
pub fn issue_event(mut facts: IssueFacts, repo: &str, now: i64) -> AutomationEvent {
    facts.title = facts.title.chars().take(MAX_ISSUE_TITLE_CHARS).collect();
    let mut event = AutomationEvent::bare(
        AutomationEventKind::IssueMatched,
        format!(
            "issue:{}:{}:{}",
            facts.provider, facts.account, facts.issue_id
        ),
        now,
    );
    event.repo = (!repo.is_empty()).then(|| repo.to_string());
    event.message = Some(facts.title.clone());
    event.source_ref = Some(facts.issue_id.clone());
    event.issue = Some(facts);
    event
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn issue_event_keys_by_provider_account_and_id() {
        let facts = IssueFacts {
            provider: "linear".into(),
            account: "work".into(),
            issue_id: "linear:THE-7".into(),
            title: "t".repeat(MAX_ISSUE_TITLE_CHARS + 50),
            ..IssueFacts::default()
        };
        let event = issue_event(facts.clone(), "thegn", 42);
        assert_eq!(event.kind, AutomationEventKind::IssueMatched);
        assert_eq!(event.key.0, "issue:linear:work:linear:THE-7");
        assert_eq!(event.repo.as_deref(), Some("thegn"));
        assert_eq!(event.source_ref.as_deref(), Some("linear:THE-7"));
        assert_eq!(
            event.message.as_deref().map(|m| m.chars().count()),
            Some(MAX_ISSUE_TITLE_CHARS)
        );
        assert_eq!(
            event.issue.as_ref().map(|i| i.title.chars().count()),
            Some(MAX_ISSUE_TITLE_CHARS)
        );
        assert_eq!(
            event.issue.as_ref().map(|i| &i.issue_id),
            Some(&facts.issue_id)
        );
        assert_eq!(event.target_rule, None);
        assert_eq!(issue_event(facts, "", 42).repo, None);
    }
}
