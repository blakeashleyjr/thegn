//! What an answered approval does (b155, THE-2200).
//!
//! Every approval is one `(issue_id, stage, commit_sha)` triple; its shape
//! names the effect ([`thegn_core::approval_digest::effect_kind`]). After
//! `approvals.resolve` (or a phone's `approvals.redeem`) has recorded the
//! decision, [`on_resolved`] dispatches it:
//!
//! - approve: a rule arming arms the rule (lane 4), a proposal starts its
//!   pipeline at `review` (lane 3), a lane approval kicks the stage executor;
//!   an outward approval does nothing here (the Lead retries the identical
//!   call, which the outward gate then admits once);
//! - reject: a proposal is marked rejected; nothing else needs undoing.
//!
//! Each effect recomputes its digest from the TRUSTED stored payload, so an
//! approval the Lead requested with a forged triple does nothing. While
//! automations are paused the decision is still recorded (the resolve ran)
//! but no effect fires.
//!
//! [`describe`] is the only text an owner sees for an approval (panel and
//! push): built from trusted state, never from the request's `reason`.

use std::sync::{Arc, OnceLock, Weak};

use thegn_core::approval_digest::{EffectKind, RULE_PREFIX, effect_kind};
use thegn_core::approval_request::{ApprovalResolve, Decision};
use thegn_core::db::Db;

use super::service::DaemonService;

/// The running daemon's service, for effects spawned from a `&DaemonService`
/// call path. Weak, so it never keeps a shutting-down service alive; unset
/// (tests, transport-only processes) means no effect fires.
static HANDLE: OnceLock<Weak<DaemonService>> = OnceLock::new();

/// Record the daemon's service handle (once, at daemon start).
pub(crate) fn install(svc: &Arc<DaemonService>) {
    if HANDLE.set(Arc::downgrade(svc)).is_err() {
        tracing::debug!(target: "thegn::approval", "approval effect handle already installed");
    }
}

/// The daemon's service handle, if one is installed and alive.
pub(crate) fn handle() -> Option<Arc<DaemonService>> {
    HANDLE.get().and_then(Weak::upgrade)
}

/// Spawn [`on_resolved`] for an answered approval when a daemon handle is
/// installed. Never blocks the caller.
pub(crate) fn spawn_on_resolved(req: ApprovalResolve) {
    let Some(svc) = handle() else {
        return;
    };
    tokio::spawn(on_resolved(svc, req));
}

/// The effect an answered approval has, before any I/O.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Effect {
    ArmRule { rule_id: String },
    StartProposal,
    RejectProposal,
    KickLane,
    Nothing,
}

/// Pure dispatch table: `(kind, decision)` to the effect.
pub(crate) fn effect_for(issue_id: &str, stage: &str, decision: &Decision) -> Effect {
    match (effect_kind(issue_id, stage), decision) {
        (EffectKind::RuleArm, Decision::Approve) => Effect::ArmRule {
            rule_id: issue_id
                .strip_prefix(RULE_PREFIX)
                .unwrap_or(issue_id)
                .to_string(),
        },
        (EffectKind::Proposal, Decision::Approve) => Effect::StartProposal,
        (EffectKind::Proposal, Decision::Reject) => Effect::RejectProposal,
        (EffectKind::Lane, Decision::Approve) => Effect::KickLane,
        (EffectKind::Outward, _) | (EffectKind::RuleArm | EffectKind::Lane, Decision::Reject) => {
            Effect::Nothing
        }
    }
}

/// Fire the effect of an answered approval. Errors are logged, never
/// surfaced: the decision itself is already recorded.
pub(crate) async fn on_resolved(svc: Arc<DaemonService>, req: ApprovalResolve) {
    let effect = effect_for(&req.issue_id, &req.stage, &req.decision);
    if effect == Effect::Nothing {
        return;
    }
    let paused = svc
        .with_db(|db| Ok(thegn_core::automation_control::paused_strict(db).unwrap_or(true)))
        .await
        .unwrap_or(true);
    if paused {
        tracing::info!(
            target: "thegn::approval",
            issue = %req.issue_id,
            stage = %req.stage,
            outcome = "paused",
            "approval recorded; automations are paused, so no effect fires"
        );
        return;
    }
    let (issue, stage, sha) = (&req.issue_id, &req.stage, &req.commit_sha);
    let result = match effect {
        Effect::ArmRule { rule_id } => {
            crate::automation_arming::on_approved(&svc, &rule_id, sha).await
        }
        Effect::StartProposal => {
            crate::automation_proposal::on_approved(&svc, issue, stage, sha).await
        }
        Effect::RejectProposal => {
            crate::automation_proposal::on_rejected(&svc, issue, stage, sha).await
        }
        Effect::KickLane => {
            super::stage_executor::kick_issue(issue);
            Ok(())
        }
        Effect::Nothing => Ok(()),
    };
    match result {
        Ok(()) => tracing::info!(
            target: "thegn::approval",
            issue = %issue,
            stage = %stage,
            outcome = "applied",
            "approval effect applied"
        ),
        Err(error) => tracing::warn!(
            target: "thegn::approval",
            issue = %issue,
            stage = %stage,
            %error,
            outcome = "failed",
            "approval effect failed"
        ),
    }
}

/// The trusted description of an approval triple, for the panel and the
/// push. Falls back to "unverified request" when no trusted payload names it.
#[allow(
    dead_code,
    reason = "the panel (b155 lane 5) and the push (lane 6) call this"
)]
pub(crate) fn describe(db: &Db, issue: &str, stage: &str, sha: &str) -> String {
    let sha7: String = sha.chars().take(7).collect();
    let trusted = match effect_kind(issue, stage) {
        EffectKind::RuleArm => issue
            .strip_prefix(RULE_PREFIX)
            .and_then(|rule| crate::automation_arming::describe(db, rule, sha)),
        EffectKind::Proposal => crate::automation_proposal::describe(db, issue, stage, sha),
        EffectKind::Lane => lane_description(db, issue, stage, &sha7),
        // Built by the outward gate from the call itself (lane 4).
        EffectKind::Outward => super::lead_caps::outward::describe(issue, stage, sha),
    };
    trusted.unwrap_or_else(|| format!("unverified request: {issue} {stage} @ {sha7}"))
}

/// A lane approval is trusted when the roster has a dispatch row for that
/// issue at that stage (the executor raised it); its text names the land.
fn lane_description(db: &Db, issue: &str, stage: &str, sha7: &str) -> Option<String> {
    use thegn_core::store::NotificationStore;
    let rows = db.list_dispatches().ok()?;
    rows.iter()
        .any(|row| row.issue_id == issue && row.stage.as_deref() == Some(stage))
        .then(|| format!("land {issue} after stage {stage} @ {sha7}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn effect_table() {
        use Decision::{Approve, Reject};
        let cases = [
            (
                "rule:lead:x",
                "arm",
                Approve,
                Effect::ArmRule {
                    rule_id: "lead:x".into(),
                },
            ),
            ("rule:lead:x", "arm", Reject, Effect::Nothing),
            (
                "linear:THE-1",
                "start.coder",
                Approve,
                Effect::StartProposal,
            ),
            (
                "linear:THE-1",
                "start.coder",
                Reject,
                Effect::RejectProposal,
            ),
            ("linear:THE-1", "coder", Approve, Effect::KickLane),
            ("linear:THE-1", "coder", Reject, Effect::Nothing),
            (
                "outward:linear:THE-1",
                "issues.comment",
                Approve,
                Effect::Nothing,
            ),
            (
                "outward:linear:THE-1",
                "issues.comment",
                Reject,
                Effect::Nothing,
            ),
        ];
        for (issue, stage, decision, effect) in cases {
            assert_eq!(
                effect_for(issue, stage, &decision),
                effect,
                "{issue} {stage}"
            );
        }
    }

    #[test]
    fn describe_falls_back_to_unverified() {
        let db = Db::open_memory().unwrap();
        let sha = "4babb0901f2c3d4e5f60718293a4b5c6d7e8f901";
        for (issue, stage) in [
            ("linear:THE-1", "coder"),
            ("linear:THE-1", "start.coder"),
            ("rule:lead:x", "arm"),
            ("outward:linear:THE-1", "issues.comment"),
        ] {
            assert_eq!(
                describe(&db, issue, stage, sha),
                format!("unverified request: {issue} {stage} @ 4babb09"),
                "{issue} {stage}"
            );
        }
    }

    #[test]
    fn describe_trusts_a_lane_the_roster_knows() {
        use thegn_core::store::NotificationStore;
        let db = Db::open_memory().unwrap();
        db.put_agent_dispatch(thegn_core::issue::NewDispatch {
            issue_id: "linear:THE-1",
            worktree_path: "/wt/lane",
            agent_name: "codex",
            stage: Some("coder"),
            parent_id: None,
            session_id: None,
            artifact_path: None,
            chunk_path: None,
        })
        .unwrap();
        assert_eq!(
            describe(&db, "linear:THE-1", "coder", "abcdef0123"),
            "land linear:THE-1 after stage coder @ abcdef0"
        );
    }
}
