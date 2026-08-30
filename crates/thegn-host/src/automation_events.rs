//! Canonical notification/event emission seam.
//!
//! Every host notification producer comes through this module. The two store
//! functions deliberately preserve the old append vs emit-once behavior and
//! submit an automation event only when a row was actually inserted.

use thegn_core::automation::{AutomationEvent, AutomationEventKind, AutomationOrigin, EventKey};
use thegn_core::notification::{NotificationKind, Priority};
use thegn_core::store::NotificationStore;

#[derive(Debug, Clone, Default)]
pub struct EventFacts {
    pub workspace: Option<String>,
    pub repo: Option<String>,
    pub branch: Option<String>,
    pub agent_role: Option<String>,
    pub session_id: Option<String>,
    pub pr_checks_passed: Option<bool>,
    pub pr_review_requested: Option<bool>,
    pub pr_merged: Option<bool>,
    pub origin: Option<AutomationOrigin>,
}

pub fn emit(
    db: &thegn_core::db::Db,
    kind: &str,
    source_ref: &str,
    message: &str,
    worktree: &str,
) -> anyhow::Result<i64> {
    emit_with_facts(
        db,
        kind,
        source_ref,
        message,
        worktree,
        EventFacts::default(),
    )
}

pub fn emit_with_facts(
    db: &thegn_core::db::Db,
    kind: &str,
    source_ref: &str,
    message: &str,
    worktree: &str,
    facts: EventFacts,
) -> anyhow::Result<i64> {
    let id = db.put_notification(kind, source_ref, message, worktree)?;
    submit_notification(kind, source_ref, message, worktree, facts);
    Ok(id)
}

pub fn emit_once(
    db: &thegn_core::db::Db,
    kind: &str,
    source_ref: &str,
    message: &str,
    worktree: &str,
) -> anyhow::Result<bool> {
    let inserted = db.put_notification_once(kind, source_ref, message, worktree)?;
    if inserted {
        submit_notification(kind, source_ref, message, worktree, EventFacts::default());
    }
    Ok(inserted)
}

/// Submit a normalized non-notification fact (session, PR, queue, or disk).
pub fn submit_fact(
    kind: AutomationEventKind,
    key: impl Into<String>,
    worktree: Option<String>,
    message: Option<String>,
    facts: EventFacts,
) {
    let occurred_at = thegn_core::util::now();
    let key = key.into();
    crate::automation_runtime::submit(AutomationEvent {
        id: event_id(kind, &key, occurred_at),
        occurred_at,
        key: EventKey(key),
        kind,
        workspace: facts.workspace,
        repo: facts.repo,
        worktree,
        branch: facts.branch,
        agent_role: facts.agent_role,
        priority: None,
        source_ref: None,
        message,
        session_id: facts.session_id,
        pr_checks_passed: facts.pr_checks_passed,
        pr_review_requested: facts.pr_review_requested,
        pr_merged: facts.pr_merged,
        origin: facts.origin,
    });
}

fn submit_notification(
    kind: &str,
    source_ref: &str,
    message: &str,
    worktree: &str,
    facts: EventFacts,
) {
    let occurred_at = thegn_core::util::now();
    let key = format!("notification:{kind}:{source_ref}:{message}");
    let priority = NotificationKind::ALL
        .into_iter()
        .find(|candidate| candidate.as_str() == kind)
        .map(NotificationKind::default_priority)
        .unwrap_or(Priority::Notice);
    crate::automation_runtime::submit(AutomationEvent {
        id: event_id(AutomationEventKind::Notification, &key, occurred_at),
        occurred_at,
        key: EventKey(key),
        kind: AutomationEventKind::Notification,
        workspace: facts.workspace,
        repo: facts.repo,
        worktree: (!worktree.is_empty()).then(|| worktree.to_string()),
        branch: facts.branch,
        agent_role: facts.agent_role,
        priority: Some(priority),
        source_ref: Some(source_ref.to_string()),
        message: Some(message.to_string()),
        session_id: facts.session_id,
        pr_checks_passed: facts.pr_checks_passed,
        pr_review_requested: facts.pr_review_requested,
        pr_merged: facts.pr_merged,
        origin: facts.origin,
    });
}

fn event_id(kind: AutomationEventKind, key: &str, occurred_at: i64) -> String {
    use std::hash::{Hash, Hasher};
    let mut hash = std::collections::hash_map::DefaultHasher::new();
    kind.hash(&mut hash);
    key.hash(&mut hash);
    occurred_at.hash(&mut hash);
    format!("ae-{occurred_at}-{:016x}", hash.finish())
}

#[cfg(test)]
mod tests {
    use super::*;
    use thegn_core::store::NotificationStore;

    #[test]
    fn once_preserves_store_idempotence() {
        let db = thegn_core::db::Db::open_memory().unwrap();
        assert!(emit_once(&db, "mentioned", "issue:1", "hello", "/wt").unwrap());
        assert!(!emit_once(&db, "mentioned", "issue:1", "hello", "/wt").unwrap());
        assert_eq!(db.get_all_notifications(10).unwrap().len(), 1);
    }
}
