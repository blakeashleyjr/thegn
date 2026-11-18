//! `approvals.list` / `approvals.request` / `approvals.resolve` (THE-2172,
//! THE-2182).
//!
//! A request is an unread `agent_attention` notification row whose source ref
//! is [`approval_request::source_ref`]; no schema of its own. Resolving is
//! Admin and HTTP + CLI only: the Lead (an MCP client holding at most Write)
//! can ask, never answer, so it cannot approve its own request. Granting goes
//! through the same `pipeline_approvals` table `thegn supervise approve` writes.

use super::{JsonResult, decode, encode};
use crate::daemon::service::DaemonService;
use serde_json::json;
use thegn_core::approval_request::{
    self, ApprovalRequest, ApprovalResolve, ApprovalView, Decision, PendingApproval,
};
use thegn_core::db::Db;
use thegn_core::notification::{Notification, NotificationKind};
use thegn_core::pipeline_approval::{Approval, state_for};
use thegn_core::push_approval;
use thegn_core::store::NotificationStore;
use thegn_svc::control::ControlError;
use thegn_svc::push::{PushMessage, PushProvider};

/// The notification kind every request row carries.
const KIND: &str = "agent_attention";
/// `approvals.list` default and ceiling for both lists.
const DEFAULT_LIMIT: usize = 50;
const MAX_LIMIT: usize = 500;

#[derive(Debug, Default, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct ListRequest {
    #[serde(default)]
    limit: Option<usize>,
    #[serde(default)]
    issue_id: Option<String>,
}

fn ttl_secs(svc: &DaemonService) -> u64 {
    svc.config.pipeline.supervisor.approval_ttl_secs
}

/// Answer `approvals.list`: recorded approvals (newest first, with their state)
/// and the outstanding requests.
pub(crate) async fn list(svc: &DaemonService, req: serde_json::Value) -> JsonResult {
    let request: ListRequest = if req.is_null() {
        ListRequest::default()
    } else {
        decode(req)?
    };
    let limit = request.limit.unwrap_or(DEFAULT_LIMIT).clamp(1, MAX_LIMIT);
    let issue = request
        .issue_id
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());
    if issue
        .as_ref()
        .is_some_and(|s| s.len() > approval_request::MAX_ISSUE_ID)
    {
        return Err(ControlError::InvalidArgument("issue_id is too long".into()));
    }
    let ttl = ttl_secs(svc);
    let (approvals, pending) = svc
        .with_db(move |db| {
            let now = thegn_core::util::now_ms();
            let fetch = if issue.is_some() { MAX_LIMIT } else { limit };
            let approvals: Vec<ApprovalView> = db
                .list_approvals(fetch)?
                .into_iter()
                .filter(|a| issue.as_deref().is_none_or(|i| a.issue_id == i))
                .take(limit)
                .map(|a| ApprovalView::evaluate(a, now, ttl))
                .collect();
            let mut pending = pending_with_ttl(db, now, ttl, fetch)?;
            pending.retain(|p| issue.as_deref().is_none_or(|i| p.issue_id == i));
            pending.truncate(limit);
            Ok((approvals, pending))
        })
        .await?;
    encode(&json!({ "approvals": approvals, "pending": pending }))
}

/// Answer `approvals.request`: record (once) an `agent_attention` row asking a
/// human to approve. Idempotent while the request is unanswered.
pub(crate) async fn request(svc: &DaemonService, req: serde_json::Value) -> JsonResult {
    let request: ApprovalRequest = decode(req)?;
    approval_request::validate_request(&request).map_err(ControlError::InvalidArgument)?;
    let stored = request.clone();
    let (requested, id) = svc.with_db(move |db| request_in(db, &stored)).await?;
    if requested {
        super::feed::notification(&svc.events, id);
    }
    after_request(svc, &request, requested).await;
    encode(&json!({ "requested": requested, "notification_id": id }))
}

/// Raise an approval from inside the daemon (the stage executor's land ask,
/// a proposal, a rule arming, an outward call; b155). Validates, records the
/// request once, publishes it, then runs [`after_request`]. Returns
/// `(inserted, notification id)`.
#[allow(dead_code, reason = "called by b155 lanes 2, 3 and 4")]
pub(crate) async fn raise(
    svc: &DaemonService,
    req: ApprovalRequest,
) -> anyhow::Result<(bool, i64)> {
    approval_request::validate_request(&req).map_err(anyhow::Error::msg)?;
    let stored = req.clone();
    let (inserted, id) = svc
        .with_db(move |db| request_in(db, &stored))
        .await
        .map_err(|error| anyhow::anyhow!("{error}"))?;
    if inserted {
        super::feed::notification(&svc.events, id);
    }
    after_request(svc, &req, inserted).await;
    Ok((inserted, id))
}

/// How long a pushed nonce can redeem its approval.
const PUSH_NONCE_TTL_MS: i64 = 24 * 60 * 60 * 1_000;
/// The push title every approval push carries.
const PUSH_TITLE: &str = "thegn approval";

/// What happens after a request is recorded (6a): when the row is new and
/// `[notifications.push]` is configured, mint a 128-bit single-use nonce,
/// store its hash bound to the request triple for 24 h, and push the TRUSTED
/// description (never the request's `reason`) plus the reply line. Best-effort
/// throughout: the request itself is already recorded.
pub(crate) async fn after_request(svc: &DaemonService, req: &ApprovalRequest, inserted: bool) {
    if !inserted {
        return;
    }
    let Some(provider) = thegn_svc::push::provider_for(&svc.config.notifications.push) else {
        return;
    };
    let mut bytes = [0u8; push_approval::NONCE_BYTES];
    if let Err(error) = getrandom::fill(&mut bytes) {
        tracing::warn!(target: "thegn::approval", %error, "approval push skipped: no OS randomness");
        return;
    }
    let nonce = push_approval::nonce_from_bytes(&bytes);
    let hash = push_approval::nonce_hash(&nonce);
    let (issue, stage, sha) = (
        req.issue_id.clone(),
        req.stage.clone(),
        req.commit_sha.clone(),
    );
    let stored = svc
        .with_db(move |db| {
            let description = crate::daemon::approval_effects::describe(db, &issue, &stage, &sha);
            // No stored payload backs this triple: never make it phone-approvable.
            if description.starts_with("unverified request") {
                return Ok(None);
            }
            let now = thegn_core::util::now_ms();
            db.put_push_nonce(
                &hash,
                &issue,
                &stage,
                &sha,
                now,
                now.saturating_add(PUSH_NONCE_TTL_MS),
            )?;
            Ok(Some(description))
        })
        .await;
    let description = match stored {
        Ok(Some(d)) => d,
        Ok(None) => {
            tracing::info!(
                target: "thegn::approval",
                issue = %req.issue_id,
                stage = %req.stage,
                "approval push skipped: unverified request"
            );
            return;
        }
        Err(error) => {
            tracing::warn!(target: "thegn::approval", %error, "approval push skipped: nonce not stored");
            return;
        }
    };
    let msg = PushMessage {
        title: PUSH_TITLE.into(),
        body: push_approval::push_body(&description, &nonce),
        priority: thegn_core::notification::Priority::Alert,
        tags: vec!["approval".into()],
    };
    // best-effort: a failed push leaves the request answerable in the panel/CLI.
    if let Err(error) = provider.publish(&msg).await {
        tracing::warn!(
            target: "thegn::approval",
            issue = %req.issue_id,
            stage = %req.stage,
            %error,
            "approval push failed"
        );
    }
}

/// Answer `approvals.resolve` (Admin; HTTP and CLI only, never MCP).
pub(crate) async fn resolve(svc: &DaemonService, req: serde_json::Value) -> JsonResult {
    let request: ApprovalResolve = decode(req)?;
    approval_request::validate_resolve(&request).map_err(ControlError::InvalidArgument)?;
    let stored = request.clone();
    svc.with_db(move |db| resolve_in(db, &stored, thegn_core::util::now_ms()))
        .await?;
    // b155: the decision is recorded; its effect (arm, start, kick) runs off
    // the request path.
    crate::daemon::approval_effects::spawn_on_resolved(request);
    encode(&json!({ "resolved": true }))
}

/// The unread approval-request rows, newest first.
fn unread_requests(db: &Db) -> anyhow::Result<Vec<Notification>> {
    Ok(db
        .get_unread_notifications()?
        .into_iter()
        .filter(|n| {
            n.kind == NotificationKind::AgentAttention
                && n.source_ref.starts_with(approval_request::SOURCE_PREFIX)
        })
        .collect())
}

/// Insert the request row unless an unread one for the same source already
/// exists. Returns `(inserted, row id)`. A request answered earlier (its row
/// read) re-arms with a fresh row.
fn request_in(db: &Db, request: &ApprovalRequest) -> anyhow::Result<(bool, i64)> {
    let source =
        approval_request::source_ref(&request.issue_id, &request.stage, &request.commit_sha);
    if let Some(existing) = unread_requests(db)?
        .into_iter()
        .find(|n| n.source_ref == source)
    {
        return Ok((false, existing.id));
    }
    let message = approval_request::request_message(request);
    let worktree = request.worktree.as_deref().unwrap_or("");
    let id = db.put_notification(KIND, &source, &message, worktree)?;
    Ok((true, id))
}

/// Apply a human decision and retire every unread request row for it.
pub(super) fn resolve_in(db: &Db, request: &ApprovalResolve, now_ms: i64) -> anyhow::Result<()> {
    match request.decision {
        Decision::Approve => db.grant_approval(&Approval {
            issue_id: request.issue_id.clone(),
            stage: request.stage.clone(),
            commit_sha: request.commit_sha.clone(),
            approver: request.approver.clone(),
            note: None,
            granted_at_ms: now_ms,
            expires_at_ms: None,
            revoked_at_ms: None,
        })?,
        Decision::Reject => {
            db.revoke_approvals(&request.issue_id, &request.stage, now_ms)?;
        }
    }
    // A decision made anywhere kills every outstanding phone nonce for it.
    db.void_push_nonces(
        &request.issue_id,
        &request.stage,
        &request.commit_sha,
        now_ms,
    )?;
    let source =
        approval_request::source_ref(&request.issue_id, &request.stage, &request.commit_sha);
    // By id rather than `mark_notification_read_by_source`: that one also
    // keys on the worktree, which the resolver does not know.
    for row in unread_requests(db)?
        .into_iter()
        .filter(|n| n.source_ref == source)
    {
        db.mark_notification_read(row.id)?;
    }
    Ok(())
}

/// The outstanding approval requests, newest first, for the overview, under
/// the DEFAULT approval TTL. Callers holding the config should prefer
/// [`pending_with_ttl`] with `cfg.pipeline.supervisor.approval_ttl_secs`.
#[allow(dead_code, reason = "the overview (b152 lane 3) calls this")]
pub(crate) fn pending(db: &Db, now_ms: i64, limit: usize) -> anyhow::Result<Vec<PendingApproval>> {
    let ttl = thegn_core::config::Pipeline::default()
        .supervisor
        .approval_ttl_secs;
    pending_with_ttl(db, now_ms, ttl, limit)
}

/// The outstanding approval requests, newest first, at most `limit`: unread
/// request rows whose lane/stage has no approval that is Live for the
/// requested commit under `ttl_secs` (a Superseded, Expired or Revoked one
/// leaves the request pending).
pub(crate) fn pending_with_ttl(
    db: &Db,
    now_ms: i64,
    ttl_secs: u64,
    limit: usize,
) -> anyhow::Result<Vec<PendingApproval>> {
    let mut out: Vec<PendingApproval> = Vec::new();
    for row in unread_requests(db)? {
        if out.len() >= limit {
            break;
        }
        let Some((issue_id, stage, commit_sha)) =
            approval_request::parse_source_ref(&row.source_ref)
        else {
            continue;
        };
        // Rows are newest first: a duplicate source is an older copy.
        if out
            .iter()
            .any(|p| p.issue_id == issue_id && p.stage == stage && p.commit_sha == commit_sha)
        {
            continue;
        }
        let latest = db.latest_approval(&issue_id, &stage)?;
        if state_for(latest.as_ref(), &commit_sha, now_ms, ttl_secs).is_live() {
            continue;
        }
        out.push(PendingApproval {
            issue_id,
            stage,
            commit_sha,
            worktree: row.worktree_path,
            // `created_at_ms` holds seconds (a legacy column name).
            requested_at: row.created_at_ms.saturating_mul(1_000),
            notification_id: row.id,
            message: row.message,
        });
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    const SHA: &str = "4babb0901f2c3d4e5f60718293a4b5c6d7e8f901";
    const OTHER: &str = "deadbeefcafe0011223344556677889900aabbcc";

    fn ask(db: &Db, sha: &str) -> (bool, i64) {
        request_in(
            db,
            &ApprovalRequest {
                issue_id: "linear:THE-1".into(),
                stage: "code".into(),
                commit_sha: sha.into(),
                worktree: Some("/wt".into()),
                reason: Some("ready".into()),
            },
        )
        .unwrap()
    }

    fn decide(db: &Db, sha: &str, decision: Decision) {
        resolve_in(
            db,
            &ApprovalResolve {
                issue_id: "linear:THE-1".into(),
                stage: "code".into(),
                commit_sha: sha.into(),
                decision,
                approver: "blake".into(),
            },
            thegn_core::util::now_ms(),
        )
        .unwrap();
    }

    fn pending_now(db: &Db) -> Vec<PendingApproval> {
        pending_with_ttl(db, thegn_core::util::now_ms(), 0, 50).unwrap()
    }

    #[test]
    fn request_is_idempotent_while_unanswered() {
        let db = Db::open_memory().unwrap();
        let (first, id) = ask(&db, SHA);
        let (second, again) = ask(&db, SHA);
        assert!(first);
        assert!(!second);
        assert_eq!(id, again);
        let pending = pending_now(&db);
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].notification_id, id);
        assert_eq!(pending[0].issue_id, "linear:THE-1");
        assert_eq!(pending[0].worktree, "/wt");
    }

    #[test]
    fn pending_excludes_a_live_approval_for_the_requested_sha() {
        let db = Db::open_memory().unwrap();
        ask(&db, SHA);
        db.grant_approval(&Approval {
            issue_id: "linear:THE-1".into(),
            stage: "code".into(),
            commit_sha: SHA.into(),
            approver: "blake".into(),
            note: None,
            granted_at_ms: thegn_core::util::now_ms(),
            expires_at_ms: None,
            revoked_at_ms: None,
        })
        .unwrap();
        assert!(pending_now(&db).is_empty());
    }

    #[test]
    fn pending_includes_a_request_whose_approval_is_superseded() {
        let db = Db::open_memory().unwrap();
        // Approved at OTHER, now asking about SHA: superseded, still pending.
        decide(&db, OTHER, Decision::Approve);
        ask(&db, SHA);
        let pending = pending_now(&db);
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].commit_sha, SHA);
    }

    #[test]
    fn resolve_approve_clears_pending() {
        let db = Db::open_memory().unwrap();
        ask(&db, SHA);
        decide(&db, SHA, Decision::Approve);
        assert!(pending_now(&db).is_empty());
        let latest = db.latest_approval("linear:THE-1", "code").unwrap().unwrap();
        assert_eq!(latest.approver, "blake");
        assert_eq!(latest.revoked_at_ms, None);
    }

    #[test]
    fn resolve_reject_clears_pending_and_revokes() {
        let db = Db::open_memory().unwrap();
        decide(&db, SHA, Decision::Approve);
        ask(&db, SHA); // pending excluded: already live
        assert!(pending_now(&db).is_empty());
        decide(&db, SHA, Decision::Reject);
        let latest = db.latest_approval("linear:THE-1", "code").unwrap().unwrap();
        assert!(latest.revoked_at_ms.is_some());
        // The request row is read, so a revoked approval does not resurrect it.
        assert!(pending_now(&db).is_empty());
        // A fresh request re-arms and is pending again (revoked is not live).
        let (requested, _) = ask(&db, SHA);
        assert!(requested);
        assert_eq!(pending_now(&db).len(), 1);
    }

    #[test]
    fn pending_ignores_foreign_attention_rows_and_respects_the_limit() {
        let db = Db::open_memory().unwrap();
        db.put_notification(KIND, "api", "unrelated", "").unwrap();
        db.put_notification(KIND, "approval:garbage", "forged", "")
            .unwrap();
        ask(&db, SHA);
        ask(&db, OTHER);
        assert_eq!(pending_now(&db).len(), 2);
        assert_eq!(
            pending_with_ttl(&db, thegn_core::util::now_ms(), 0, 1)
                .unwrap()
                .len(),
            1
        );
    }
}
