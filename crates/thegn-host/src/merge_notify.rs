//! Merge-queue drain notifications shared by the TUI and CLI.
//!
//! CLI drains have no desktop/event bus, so they record routed notices in the
//! inbox only. Hold dedupe checks unread inbox rows and therefore survives a
//! process restart; marking or clearing a notice allows the next hold to show.

use thegn_core::attention::MqStatus;
use thegn_core::config::Config;
use thegn_core::db::Db;
use thegn_core::notification::NotificationKind;
use thegn_core::notification_route::{self, RouteCtx};
use thegn_core::store::NotificationStore;

use crate::merge_driver::{DriveOutcome, DriveStep};

pub(crate) fn step_notification(
    status: MqStatus,
    branch: &str,
    detail: &str,
) -> Option<(NotificationKind, String)> {
    match status {
        MqStatus::Landed => Some((
            NotificationKind::QueueLanded,
            format!("merge queue: {branch} landed"),
        )),
        MqStatus::Ready => Some((
            NotificationKind::QueueReady,
            format!("merge queue: {branch} ready to land"),
        )),
        MqStatus::NeedsHuman => Some((
            NotificationKind::QueueNeedsHuman,
            format!("merge queue: {branch} needs a human — {detail}"),
        )),
        MqStatus::GateError | MqStatus::AgentBlocked => {
            let headline = bounded_headline(detail);
            Some((
                NotificationKind::QueueNeedsHuman,
                format!(
                    "merge queue: {branch} held (environment/policy, not a code verdict): {headline}"
                ),
            ))
        }
        MqStatus::Deferred => Some((
            NotificationKind::QueueHeld,
            format!(
                "merge queue: {branch} deferred (not landed): {}",
                bounded_headline(detail)
            ),
        )),
        MqStatus::GateFailed => Some((
            NotificationKind::QueueHeld,
            format!(
                "merge queue: {branch} gate failed (red build): {}",
                bounded_headline(detail)
            ),
        )),
        MqStatus::Queued | MqStatus::Folding | MqStatus::Verifying | MqStatus::AgentRunning => None,
    }
}

pub(crate) fn hold_signature(status: MqStatus, branch: &str) -> Option<String> {
    match status {
        MqStatus::Deferred => Some(format!("merge queue: {branch} deferred")),
        MqStatus::GateFailed => Some(format!("merge queue: {branch} gate failed")),
        _ => None,
    }
}

pub(crate) fn bounded_headline(detail: &str) -> String {
    let headline = detail.lines().next().unwrap_or_default();
    thegn_core::notification_render::truncate_chars(headline, 200, "…")
}

pub(crate) fn done_notifications(out: &DriveOutcome) -> Vec<(NotificationKind, String, String)> {
    let mut notifications = Vec::new();
    for failure in &out.bookkeeping_failed {
        let error = bounded_headline(&failure.error);
        notifications.push((
            NotificationKind::QueueNeedsHuman,
            format!("bookkeeping:{}:{}", failure.target, failure.new_oid),
            format!(
                "merge queue: {} landed on {} ({}→{}) but bookkeeping failed: {}; run `thegn merge drain` to reconcile",
                failure.branch, failure.target, failure.old_oid, failure.new_oid, error
            ),
        ));
    }
    for line in &out.persistence_errors {
        notifications.push((
            NotificationKind::QueueNeedsHuman,
            format!("persistence:{line}"),
            format!("merge queue state write failed: {}", bounded_headline(line)),
        ));
    }
    notifications
}

#[derive(Debug)]
pub(crate) struct CliDrainStep {
    worktree: String,
    branch: String,
    status: MqStatus,
    detail: String,
}

pub(crate) fn collect_step(
    steps: &mut Vec<CliDrainStep>,
    step: &DriveStep<'_>,
    json: bool,
) -> bool {
    if let Some(status) = MqStatus::parse(step.status) {
        steps.push(CliDrainStep {
            worktree: step.worktree.to_string(),
            branch: step.branch.to_string(),
            status,
            detail: step.detail.to_string(),
        });
    }
    !json
}

pub(crate) fn record_cli_drain(cfg: &Config, db: &Db, steps: &[CliDrainStep], out: &DriveOutcome) {
    let effective = cfg.effective_notifications(None);
    let ctx = RouteCtx {
        now_local: Some(chrono::Local::now().naive_local()),
        active_mode: effective.active_mode.clone(),
        active_profile: cfg.profile.clone(),
        ..RouteCtx::default()
    };

    for step in steps {
        let Some((kind, message)) = step_notification(step.status, &step.branch, &step.detail)
        else {
            continue;
        };
        let signature = hold_signature(step.status, &step.branch);
        let decision = notification_route::decide(
            kind,
            &step.worktree,
            &message,
            &step.worktree,
            &effective,
            &ctx,
        );
        if !decision.record {
            continue;
        }
        if let Some(signature) = signature.as_deref() {
            match hold_already_unread(db, kind, &step.worktree, signature) {
                Ok(true) => continue,
                Ok(false) => {}
                Err(error) => tracing::warn!(
                    target: "thegn::merge",
                    %error,
                    worktree = %step.worktree,
                    "could not check merge queue hold dedupe; recording notice"
                ),
            }
        }
        let insert_result = if signature.is_some() {
            db.put_notification(kind.as_str(), &step.worktree, &message, &step.worktree)
                .map(|_| ())
        } else {
            db.put_notification_once(kind.as_str(), &step.worktree, &message, &step.worktree)
                .map(|_| ())
        };
        if let Err(error) = insert_result {
            tracing::warn!(target: "thegn::merge", %error, "could not record CLI merge queue notification");
        }
    }

    for (kind, key, message) in done_notifications(out) {
        let decision = notification_route::decide(kind, &key, &message, "", &effective, &ctx);
        if decision.record
            && let Err(error) = db.put_notification_once(kind.as_str(), &key, &message, "")
        {
            tracing::warn!(target: "thegn::merge", %error, "could not record CLI merge queue bookkeeping notification");
        }
    }
}

pub(crate) fn hold_already_unread(
    db: &Db,
    kind: NotificationKind,
    worktree: &str,
    signature: &str,
) -> anyhow::Result<bool> {
    Ok(db.get_unread_notifications()?.iter().any(|notification| {
        notification.kind == kind
            && notification.worktree_path == worktree
            && notification.message.starts_with(signature)
    }))
}

#[cfg(test)]
mod tests {
    use super::{
        CliDrainStep, collect_step, done_notifications, hold_signature, record_cli_drain,
        step_notification,
    };
    use crate::merge_driver::{DriveOutcome, DriveStep, RefAdvancedBookkeepingFailed};
    use thegn_core::attention::MqStatus;
    use thegn_core::config::Config;
    use thegn_core::db::Db;
    use thegn_core::notification::NotificationKind;
    use thegn_core::store::NotificationStore;

    fn step(worktree: &str, branch: &str, status: MqStatus, detail: &str) -> CliDrainStep {
        CliDrainStep {
            worktree: worktree.to_string(),
            branch: branch.to_string(),
            status,
            detail: detail.to_string(),
        }
    }

    #[test]
    fn cli_drain_records_hold_and_landed_notifications() {
        let db = Db::open_memory().unwrap();
        let steps = vec![
            step("/work/feat-a", "feat-a", MqStatus::Deferred, "conflict"),
            step("/work/feat-b", "feat-b", MqStatus::Landed, ""),
        ];
        record_cli_drain(&Config::default(), &db, &steps, &DriveOutcome::default());
        let unread = db.get_unread_notifications().unwrap();
        assert_eq!(unread.len(), 2);
        let hold_signature = hold_signature(MqStatus::Deferred, "feat-a").unwrap();
        assert!(unread.iter().any(|notification| {
            notification.kind == NotificationKind::QueueHeld
                && notification.worktree_path == "/work/feat-a"
                && notification.message.starts_with(&hold_signature)
        }));
        assert!(unread.iter().any(|notification| {
            notification.kind == NotificationKind::QueueLanded
                && notification.worktree_path == "/work/feat-b"
        }));
    }

    #[test]
    fn hold_dedupe_survives_a_fresh_process_state() {
        let db = Db::open_memory().unwrap();
        let config = Config::default();
        let steps = vec![step(
            "/work/feat-a",
            "feat-a",
            MqStatus::Deferred,
            "conflict",
        )];
        record_cli_drain(&config, &db, &steps, &DriveOutcome::default());
        record_cli_drain(&config, &db, &steps, &DriveOutcome::default());
        let unread = db.get_unread_notifications().unwrap();
        assert_eq!(unread.len(), 1);
        db.mark_notification_read(unread[0].id).unwrap();
        record_cli_drain(&config, &db, &steps, &DriveOutcome::default());
        assert_eq!(db.get_unread_notifications().unwrap().len(), 1);
        assert_eq!(db.get_all_notifications(10).unwrap().len(), 2);
    }

    #[test]
    fn different_hold_state_is_not_deduped() {
        let db = Db::open_memory().unwrap();
        let config = Config::default();
        record_cli_drain(
            &config,
            &db,
            &[step(
                "/work/feat-a",
                "feat-a",
                MqStatus::Deferred,
                "conflict",
            )],
            &DriveOutcome::default(),
        );
        record_cli_drain(
            &config,
            &db,
            &[step(
                "/work/feat-a",
                "feat-a",
                MqStatus::GateFailed,
                "red build",
            )],
            &DriveOutcome::default(),
        );
        assert_eq!(db.get_unread_notifications().unwrap().len(), 2);
    }

    #[test]
    fn bookkeeping_notice_is_recorded_once() {
        let db = Db::open_memory().unwrap();
        let config = Config::default();
        let out = DriveOutcome {
            bookkeeping_failed: vec![RefAdvancedBookkeepingFailed {
                branch: "feature".into(),
                target: "main".into(),
                old_oid: "old".into(),
                new_oid: "new".into(),
                error: "database locked".into(),
            }],
            ..DriveOutcome::default()
        };
        record_cli_drain(&config, &db, &[], &out);
        record_cli_drain(&config, &db, &[], &out);
        assert_eq!(db.get_unread_notifications().unwrap().len(), 1);
        assert_eq!(done_notifications(&out).len(), 1);
    }

    #[test]
    fn hold_messages_start_with_their_signature() {
        for status in [MqStatus::Deferred, MqStatus::GateFailed] {
            let signature = hold_signature(status, "feature").unwrap();
            let (_, message) = step_notification(status, "feature", "reason").unwrap();
            assert!(message.starts_with(&signature), "{status:?}: {message}");
        }
        assert_eq!(hold_signature(MqStatus::Landed, "feature"), None);
    }

    #[test]
    fn json_drain_still_records() {
        let mut steps = Vec::new();
        let worktree = "/work/feature";
        let branch = "feature";
        let status = "landed";
        let detail = "";
        let fields = thegn_core::store::MergeStatusFields::default();
        let drive_step = DriveStep {
            worktree,
            branch,
            status,
            detail,
            fields: &fields,
        };
        assert!(!collect_step(&mut steps, &drive_step, true));
        assert_eq!(steps.len(), 1);
        assert_eq!(steps[0].status, MqStatus::Landed);
    }
}
