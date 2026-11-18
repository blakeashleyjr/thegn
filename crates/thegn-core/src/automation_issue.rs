//! `issue_matched` producer inputs (THE-2194).
//!
//! Pure: the tracker-refresh job (thegn-host `automation_issue`) diffs the
//! previous and fresh issue lists here and submits one event per candidate.
//! Phase 3 moves that call into the daemon unchanged.
//!
//! Semantics: "newly matches" means an issue that appears, or changes a
//! structural fact (status, labels, assignees, project ids), while a rule
//! exists. Already-matching unchanged issues do not fire when a rule is added.
//! `once_per_key` (forced by the spine) makes each rule fire once per issue;
//! the once-key ledger is bounded at 2048 per rule and the lexicographically
//! smallest key is evicted.

use crate::automation::{AutomationEvent, AutomationEventKind, IssueFacts, MAX_ISSUE_TITLE_CHARS};
use crate::issue::Issue;
use std::collections::{BTreeSet, HashMap, HashSet};

/// Upper bound on events one refresh may submit; the rest wait for the next.
pub const MAX_ISSUE_EVENTS_PER_REFRESH: usize = 50;

/// Max title chars carried in facts.
const MAX_TITLE: usize = MAX_ISSUE_TITLE_CHARS;
/// Max labels / assignees carried per issue.
const MAX_LIST: usize = 32;
/// Max bytes per label / assignee / id.
const MAX_ITEM_BYTES: usize = 128;

/// Structural facts that make a changed issue "newly matching".
fn structure(issue: &Issue) -> (&'static str, BTreeSet<&str>, BTreeSet<&str>, BTreeSet<&str>) {
    (
        issue.status.as_str(),
        issue.labels.iter().map(String::as_str).collect(),
        issue.assignees.iter().map(String::as_str).collect(),
        issue.project_ids.iter().map(String::as_str).collect(),
    )
}

/// Issues in `new` worth an `issue_matched` event: absent from `old`, or with
/// a changed status token / label, assignee or project set. Title and body
/// changes never count. `had_cache` false (a cold baseline) yields nothing.
/// Output follows `new` order, capped at [`MAX_ISSUE_EVENTS_PER_REFRESH`].
pub fn matched_candidates<'a>(old: &[Issue], new: &'a [Issue], had_cache: bool) -> Vec<&'a Issue> {
    if !had_cache {
        return vec![];
    }
    let prior: HashMap<&str, &Issue> = old.iter().map(|i| (i.id.as_str(), i)).collect();
    new.iter()
        .filter(|issue| match prior.get(issue.id.as_str()) {
            None => true,
            Some(before) => structure(before) != structure(issue),
        })
        .take(MAX_ISSUE_EVENTS_PER_REFRESH)
        .collect()
}

/// Count of candidates beyond the per-refresh cap (for logging).
pub fn overflow_count(old: &[Issue], new: &[Issue], had_cache: bool) -> usize {
    if !had_cache {
        return 0;
    }
    let prior: HashSet<&str> = old.iter().map(|i| i.id.as_str()).collect();
    let total = new
        .iter()
        .filter(|i| {
            !prior.contains(i.id.as_str())
                || old
                    .iter()
                    .find(|o| o.id == i.id)
                    .is_some_and(|o| structure(o) != structure(i))
        })
        .count();
    total.saturating_sub(MAX_ISSUE_EVENTS_PER_REFRESH)
}

fn bound_bytes(s: &str) -> String {
    let mut out = String::new();
    for c in s.chars().filter(|c| !c.is_control()) {
        if out.len() + c.len_utf8() > MAX_ITEM_BYTES {
            break;
        }
        out.push(c);
    }
    out
}

fn bound_list(items: &[String]) -> Vec<String> {
    items
        .iter()
        .map(|s| bound_bytes(s))
        .filter(|s| !s.is_empty())
        .take(MAX_LIST)
        .collect()
}

/// Same shape gate the tracker identity check applies, without the svc crate:
/// bounded, `provider:key` with a conservative charset.
fn id_shape_ok(id: &str) -> bool {
    id.len() <= MAX_ITEM_BYTES
        && id
            .split_once(':')
            .is_some_and(|(p, k)| !p.is_empty() && !k.is_empty())
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, ':' | '_' | '#' | '/' | '.' | '-'))
}

/// Project a tracker issue onto the structural facts rules can match.
/// `assigned_to_me` comes only from the argument. An id that fails the shape
/// check is left empty so the event is refused by required-fact validation.
pub fn facts_from_issue(issue: &Issue, account: &str, assigned_to_me: bool) -> IssueFacts {
    IssueFacts {
        provider: issue.provider.clone(),
        account: account.to_string(),
        issue_id: if id_shape_ok(&issue.id) {
            issue.id.clone()
        } else {
            String::new()
        },
        number: bound_bytes(&issue.number),
        url: issue
            .url
            .chars()
            .filter(|c| !c.is_control())
            .take(512)
            .collect(),
        status: issue.status.as_str().to_string(),
        labels: bound_list(&issue.labels),
        assignees: bound_list(&issue.assignees),
        project_ids: bound_list(&issue.project_ids),
        assigned_to_me,
        title: issue
            .title
            .chars()
            .filter(|c| !c.is_control())
            .take(MAX_TITLE)
            .collect(),
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

    fn iss(id: &str, labels: &[&str]) -> Issue {
        let (p, k) = id.split_once(':').unwrap();
        Issue {
            id: id.into(),
            number: k.into(),
            provider: p.into(),
            title: "t".into(),
            body: None,
            status: crate::issue::IssueStatus::Todo,
            priority: Default::default(),
            assignees: vec![],
            labels: labels.iter().map(|s| s.to_string()).collect(),
            url: "u".into(),
            branch_hint: None,
            updated_at_ms: 0,
            project_ids: vec![],
            blocked_by: vec![],
            due_at_ms: None,
            account: None,
        }
    }

    fn ids(v: Vec<&Issue>) -> Vec<String> {
        v.into_iter().map(|i| i.id.clone()).collect()
    }

    #[test]
    fn baseline_returns_nothing() {
        assert!(matched_candidates(&[], &[iss("linear:A-1", &[])], false).is_empty());
    }

    #[test]
    fn new_issue_label_and_status_changes_match() {
        let old = vec![iss("linear:A-1", &[]), iss("linear:A-2", &[])];
        let mut new = vec![
            iss("linear:A-1", &["x"]),
            iss("linear:A-2", &[]),
            iss("linear:A-3", &[]),
        ];
        assert_eq!(
            ids(matched_candidates(&old, &new, true)),
            ["linear:A-1", "linear:A-3"]
        );
        new[1].status = crate::issue::IssueStatus::Done;
        assert_eq!(
            ids(matched_candidates(&old, &new, true)),
            ["linear:A-1", "linear:A-2", "linear:A-3"]
        );
    }

    #[test]
    fn title_only_and_reorder_are_ignored() {
        let old = vec![iss("linear:A-1", &["a", "b"]), iss("linear:A-2", &[])];
        let mut new = vec![iss("linear:A-2", &[]), iss("linear:A-1", &["b", "a"])];
        new[1].title = "renamed".into();
        new[1].body = Some("body".into());
        assert!(matched_candidates(&old, &new, true).is_empty());
    }

    #[test]
    fn candidates_are_capped() {
        let new: Vec<Issue> = (0..80)
            .map(|n| iss(&format!("linear:A-{n}"), &[]))
            .collect();
        assert_eq!(
            matched_candidates(&[], &new, true).len(),
            MAX_ISSUE_EVENTS_PER_REFRESH
        );
        assert_eq!(overflow_count(&[], &new, true), 30);
    }

    #[test]
    fn hostile_issue_is_bounded_and_stripped() {
        let mut i = iss("linear:A-1", &[]);
        i.title = format!("a\u{1b}[31m\n{}", "x".repeat(1000));
        i.labels = (0..100)
            .map(|n| format!("{n}{}", "l".repeat(500)))
            .collect();
        i.assignees = vec!["ok\u{7}".repeat(100)];
        let f = facts_from_issue(&i, "acct", false);
        assert!(f.title.chars().count() <= MAX_ISSUE_TITLE_CHARS);
        assert!(!f.title.chars().any(char::is_control));
        assert_eq!(f.labels.len(), 32);
        assert!(f.labels.iter().all(|l| l.len() <= 128));
        assert!(f.assignees[0].len() <= 128 && !f.assignees[0].chars().any(char::is_control));
        i.id = "linear:A-1; rm -rf".into();
        assert!(facts_from_issue(&i, "acct", false).issue_id.is_empty());
    }

    #[test]
    fn assigned_to_me_comes_only_from_argument() {
        let mut i = iss("linear:A-1", &[]);
        i.assignees = vec!["me".into()];
        assert!(!facts_from_issue(&i, "a", false).assigned_to_me);
        assert!(facts_from_issue(&i, "a", true).assigned_to_me);
    }

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
