//! Canonical notification/event emission seam.
//!
//! Every host notification producer comes through this module. The two store
//! functions deliberately preserve the old append vs emit-once behavior and
//! submit an automation event only when a row was actually inserted.

use thegn_core::automation::{AutomationEvent, AutomationEventKind, AutomationOrigin, EventKey};
use thegn_core::notification::{NotificationKind, Priority};
use thegn_core::notification_route::{RouteCtx, RouteDecision};
use thegn_core::store::NotificationStore;

static ROUTE_CONFIG: std::sync::OnceLock<std::sync::Mutex<thegn_core::config::Config>> =
    std::sync::OnceLock::new();

/// Publish the effective non-TUI routing configuration. The live TUI state
/// remains authoritative when present because it also carries focus and DND
/// state; daemon and one-shot producers use this snapshot instead of silently
/// bypassing notification rules.
pub fn install_route_config(cfg: &thegn_core::config::Config) {
    let slot = ROUTE_CONFIG.get_or_init(|| std::sync::Mutex::new(cfg.clone()));
    *slot.lock().expect("notification route config lock") = cfg.clone();
}

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
    if let Some(state) = crate::notify::global() {
        let (_, id) = crate::notify::record_with_facts(
            db, &state, kind, source_ref, message, worktree, facts,
        );
        return id.ok_or_else(|| anyhow::anyhow!("notification dropped by routing"));
    }
    let decision = fallback_decision(kind, source_ref, message, worktree);
    insert_routed(
        db, kind, source_ref, message, worktree, facts, &decision, false,
    )?
    .ok_or_else(|| anyhow::anyhow!("notification dropped by routing"))
}

pub fn emit_once(
    db: &thegn_core::db::Db,
    kind: &str,
    source_ref: &str,
    message: &str,
    worktree: &str,
) -> anyhow::Result<bool> {
    let facts = EventFacts::default();
    if let Some(state) = crate::notify::global() {
        let (_, inserted) = crate::notify::record_once_with_facts(
            db, &state, kind, source_ref, message, worktree, facts,
        );
        return Ok(inserted);
    }
    Ok(insert_routed(
        db,
        kind,
        source_ref,
        message,
        worktree,
        facts,
        &fallback_decision(kind, source_ref, message, worktree),
        true,
    )?
    .is_some())
}

/// Persist after the caller's single route decision, then submit exactly one
/// normalized event using the inserted row's unique identity.
#[expect(
    clippy::too_many_arguments,
    reason = "the canonical notification record preserves the routed/store field vocabulary"
)]
pub(crate) fn insert_routed(
    db: &thegn_core::db::Db,
    kind: &str,
    source_ref: &str,
    message: &str,
    worktree: &str,
    facts: EventFacts,
    decision: &RouteDecision,
    once: bool,
) -> anyhow::Result<Option<i64>> {
    if !decision.record {
        return Ok(None);
    }
    let id = if once {
        db.put_notification_once_id(kind, source_ref, message, worktree)?
    } else {
        Some(db.put_notification(kind, source_ref, message, worktree)?)
    };
    if let Some(id) = id {
        submit_notification(
            id,
            kind,
            source_ref,
            message,
            worktree,
            decision.effective_priority,
            facts,
        );
    }
    Ok(id)
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
        target_rule: None,
        workspace: facts.workspace,
        repo: facts.repo,
        worktree,
        branch: facts.branch,
        agent_role: facts.agent_role,
        notification_kind: None,
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

/// Consume ancestry persisted by an automation `merge.add` action. The merge
/// may land in a later process; this durable handoff prevents its descendant
/// event from starting a second rule chain.
pub fn take_merge_origin(db: &thegn_core::db::Db, worktree: &str) -> Option<AutomationOrigin> {
    use thegn_core::store::WorkspaceStore;
    let raw = db
        .get_ui_state("automation_merge_origin", worktree)
        .ok()
        .flatten()?;
    let origin = serde_json::from_str(&raw).ok()?;
    let _ = db.del_ui_state("automation_merge_origin", worktree);
    Some(origin)
}

fn submit_notification(
    row_id: i64,
    kind: &str,
    source_ref: &str,
    message: &str,
    worktree: &str,
    priority: Priority,
    facts: EventFacts,
) {
    crate::automation_runtime::submit(notification_event(
        row_id, kind, source_ref, message, worktree, priority, facts,
    ));
}

fn notification_event(
    row_id: i64,
    kind: &str,
    source_ref: &str,
    message: &str,
    worktree: &str,
    priority: Priority,
    facts: EventFacts,
) -> AutomationEvent {
    let occurred_at = thegn_core::util::now();
    let notification_kind = NotificationKind::ALL
        .into_iter()
        .find(|candidate| candidate.as_str() == kind);
    let key = format!("notification:{row_id}");
    AutomationEvent {
        id: format!("notification:{row_id}"),
        occurred_at,
        key: EventKey(key),
        kind: AutomationEventKind::Notification,
        target_rule: None,
        workspace: facts.workspace,
        repo: facts.repo,
        worktree: (!worktree.is_empty()).then(|| worktree.to_string()),
        branch: facts.branch,
        agent_role: facts.agent_role,
        notification_kind,
        priority: Some(priority),
        source_ref: Some(source_ref.to_string()),
        message: Some(message.to_string()),
        session_id: facts.session_id,
        pr_checks_passed: facts.pr_checks_passed,
        pr_review_requested: facts.pr_review_requested,
        pr_merged: facts.pr_merged,
        origin: facts.origin,
    }
}

fn fallback_decision(kind: &str, source_ref: &str, message: &str, worktree: &str) -> RouteDecision {
    let parsed = NotificationKind::ALL
        .into_iter()
        .find(|candidate| candidate.as_str() == kind);
    if let (Some(kind), Some(slot)) = (parsed, ROUTE_CONFIG.get()) {
        let cfg = slot.lock().expect("notification route config lock");
        let repo_root = (!worktree.is_empty()).then(|| std::path::Path::new(worktree));
        let effective = cfg.effective_notifications(repo_root);
        return thegn_core::notification_route::decide(
            kind,
            source_ref,
            message,
            worktree,
            &effective,
            &RouteCtx {
                now_local: Some(chrono::Local::now().naive_local()),
                active_mode: effective.active_mode.clone(),
                active_profile: cfg.profile.clone(),
                ..RouteCtx::default()
            },
        );
    }
    let effective_priority = parsed
        .map(NotificationKind::default_priority)
        .unwrap_or(Priority::Notice);
    RouteDecision {
        record: true,
        effective_priority,
        desktop: false,
        toast: false,
        push_sinks: Vec::new(),
        sound: None,
    }
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

    #[test]
    fn routed_notification_carries_kind_priority_and_unique_row_identity() {
        let first = notification_event(
            41,
            "test_failed",
            "tests",
            "failed",
            "/wt",
            Priority::Notice,
            EventFacts::default(),
        );
        let second = notification_event(
            42,
            "test_failed",
            "tests",
            "failed",
            "/wt",
            Priority::Alert,
            EventFacts::default(),
        );
        assert_eq!(first.notification_kind, Some(NotificationKind::TestFailed));
        assert_eq!(first.priority, Some(Priority::Notice));
        assert_eq!(second.priority, Some(Priority::Alert));
        assert_ne!(first.id, second.id);
        assert_ne!(first.key, second.key);
    }

    #[test]
    fn merge_origin_is_a_durable_single_use_handoff() {
        use thegn_core::store::WorkspaceStore;

        let db = thegn_core::db::Db::open_memory().unwrap();
        let origin = AutomationOrigin {
            root_event_id: "root".into(),
            rule_id: "merge-rule".into(),
            run_id: "9".into(),
        };
        db.set_ui_state(
            "automation_merge_origin",
            "/wt",
            &serde_json::to_string(&origin).unwrap(),
        )
        .unwrap();

        assert_eq!(take_merge_origin(&db, "/wt"), Some(origin));
        assert_eq!(take_merge_origin(&db, "/wt"), None);
    }
}
