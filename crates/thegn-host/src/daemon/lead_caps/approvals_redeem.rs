//! `approvals.redeem` (b155 6a): the owner's phone answers an approval with
//! the single-use nonce its push carried. Write scope, HTTP + CLI only, and
//! refused for a Lead-confined caller (`lead_confine` Refuse); it also needs
//! the push inbox's HMAC secret (`[notifications.push.inbox] allow` must list
//! it). The Lead never sees the push, so it never holds a nonce.
//!
//! The nonce is looked up by its sha256 only (the plaintext is never stored),
//! and `consume_push_nonce` claims it in one guarded UPDATE: a replayed
//! envelope that slips past the inbox replay cache (a daemon restart) still
//! finds the nonce used and is refused.

use super::{JsonResult, decode, encode};
use crate::daemon::service::DaemonService;
use serde_json::json;
use thegn_core::approval_request::{ApprovalRedeem, ApprovalResolve};
use thegn_core::db::Db;
use thegn_core::push_approval;
use thegn_svc::control::ControlError;

/// The approver recorded for a phone answer.
const APPROVER: &str = "push";
/// The one refusal for every nonce that cannot redeem (no oracle between
/// unknown, used and expired).
const REFUSED: &str = "unknown, used or expired approval code";

/// Decode and shape-check a redeem body. Runs before the nonce is touched, so
/// a malformed envelope never burns it.
fn parse(req: serde_json::Value) -> Result<ApprovalRedeem, ControlError> {
    let request: ApprovalRedeem = decode(req)?;
    if !push_approval::is_nonce_shape(&request.nonce) {
        return Err(ControlError::InvalidArgument(
            "nonce must be 32 lowercase hex characters".into(),
        ));
    }
    Ok(request)
}

/// Consume the nonce and record the decision for the triple it is bound to.
/// `None` when the nonce is unknown, used or expired.
fn redeem_in(
    db: &Db,
    request: &ApprovalRedeem,
    now_ms: i64,
) -> anyhow::Result<Option<ApprovalResolve>> {
    let hash = push_approval::nonce_hash(&request.nonce);
    let Some((issue_id, stage, commit_sha)) = db.consume_push_nonce(&hash, now_ms)? else {
        return Ok(None);
    };
    let resolve = ApprovalResolve {
        issue_id,
        stage,
        commit_sha,
        decision: request.decision,
        approver: APPROVER.into(),
    };
    super::approvals::resolve_in(db, &resolve, now_ms)?;
    Ok(Some(resolve))
}

/// Answer `approvals.redeem` (`{nonce, decision}`).
pub(crate) async fn redeem(svc: &DaemonService, req: serde_json::Value) -> JsonResult {
    let request = parse(req)?;
    let resolved = svc
        .with_db(move |db| redeem_in(db, &request, thegn_core::util::now_ms()))
        .await?;
    let Some(resolve) = resolved else {
        return Err(ControlError::InvalidArgument(REFUSED.into()));
    };
    tracing::info!(
        target: "thegn::approval",
        issue = %resolve.issue_id,
        stage = %resolve.stage,
        decision = ?resolve.decision,
        "approval redeemed from push"
    );
    let reply = json!({
        "resolved": true,
        "issue_id": resolve.issue_id,
        "stage": resolve.stage,
    });
    // The decision is recorded; its effect (arm, start, kick) runs off the
    // request path, exactly as `approvals.resolve`.
    crate::daemon::approval_effects::spawn_on_resolved(resolve);
    encode(&reply)
}

#[cfg(test)]
mod tests {
    use super::*;
    use thegn_core::approval_request::Decision;

    const SHA: &str = "4babb0901f2c3d4e5f60718293a4b5c6d7e8f901";
    const NONCE: &str = "00112233445566778899aabbccddeeff";

    fn store(db: &Db, nonce: &str, now: i64, expires: i64) {
        db.put_push_nonce(
            &push_approval::nonce_hash(nonce),
            "linear:THE-1",
            "code",
            SHA,
            now,
            expires,
        )
        .unwrap();
    }

    fn body(nonce: &str, decision: Decision) -> ApprovalRedeem {
        ApprovalRedeem {
            nonce: nonce.into(),
            decision,
        }
    }

    #[test]
    fn redeem_approves_once_and_refuses_a_replay() {
        let db = Db::open_memory().unwrap();
        let now = thegn_core::util::now_ms();
        store(&db, NONCE, now, now + 60_000);
        let first = redeem_in(&db, &body(NONCE, Decision::Approve), now)
            .unwrap()
            .unwrap();
        assert_eq!(first.issue_id, "linear:THE-1");
        assert_eq!(first.stage, "code");
        assert_eq!(first.commit_sha, SHA);
        assert_eq!(first.approver, "push");
        let latest = db.latest_approval("linear:THE-1", "code").unwrap().unwrap();
        assert_eq!(latest.approver, "push");
        assert_eq!(latest.commit_sha, SHA);
        assert_eq!(latest.revoked_at_ms, None);
        // Single use: the identical envelope again finds nothing, even with
        // the other decision.
        assert!(
            redeem_in(&db, &body(NONCE, Decision::Approve), now + 1)
                .unwrap()
                .is_none()
        );
        assert!(
            redeem_in(&db, &body(NONCE, Decision::Reject), now + 2)
                .unwrap()
                .is_none()
        );
        let latest = db.latest_approval("linear:THE-1", "code").unwrap().unwrap();
        assert_eq!(latest.revoked_at_ms, None);
    }

    #[test]
    fn redeem_reject_revokes() {
        let db = Db::open_memory().unwrap();
        let now = thegn_core::util::now_ms();
        store(&db, NONCE, now, now + 60_000);
        let resolved = redeem_in(&db, &body(NONCE, Decision::Reject), now)
            .unwrap()
            .unwrap();
        assert_eq!(resolved.decision, Decision::Reject);
    }

    #[test]
    fn redeem_refuses_an_expired_or_unknown_nonce() {
        let db = Db::open_memory().unwrap();
        let now = thegn_core::util::now_ms();
        store(&db, NONCE, now - 10, now);
        assert!(
            redeem_in(&db, &body(NONCE, Decision::Approve), now)
                .unwrap()
                .is_none()
        );
        assert!(
            redeem_in(
                &db,
                &body("ffeeddccbbaa99887766554433221100", Decision::Approve),
                now
            )
            .unwrap()
            .is_none()
        );
        assert!(
            db.latest_approval("linear:THE-1", "code")
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn redeem_matches_by_hash_never_by_plaintext() {
        let db = Db::open_memory().unwrap();
        let now = thegn_core::util::now_ms();
        // A row keyed by the plaintext (never written by the daemon) does
        // not redeem: the lookup hashes what the phone sent.
        db.put_push_nonce(NONCE, "linear:THE-1", "code", SHA, now, now + 60_000)
            .unwrap();
        assert!(
            redeem_in(&db, &body(NONCE, Decision::Approve), now)
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn parse_rejects_bad_decisions_unknown_fields_and_bad_nonces() {
        assert!(parse(json!({ "nonce": NONCE, "decision": "approve" })).is_ok());
        assert!(parse(json!({ "nonce": NONCE, "decision": "reject" })).is_ok());
        for bad in [
            json!({ "nonce": NONCE, "decision": "yes" }),
            json!({ "nonce": NONCE, "decision": "Approve" }),
            json!({ "nonce": NONCE }),
            json!({ "nonce": NONCE, "decision": "approve", "issue_id": "linear:THE-1" }),
            json!({ "nonce": NONCE, "decision": "approve", "approver": "blake" }),
            json!({ "nonce": NONCE.to_uppercase(), "decision": "approve" }),
            json!({ "nonce": &NONCE[..31], "decision": "approve" }),
            json!({ "nonce": format!("{NONCE}00"), "decision": "approve" }),
            json!({ "nonce": "", "decision": "approve" }),
            json!(null),
        ] {
            assert!(
                matches!(parse(bad.clone()), Err(ControlError::InvalidArgument(_))),
                "{bad}"
            );
        }
    }

    #[test]
    fn a_malformed_envelope_does_not_burn_the_nonce() {
        let db = Db::open_memory().unwrap();
        let now = thegn_core::util::now_ms();
        store(&db, NONCE, now, now + 60_000);
        assert!(parse(json!({ "nonce": NONCE, "decision": "maybe" })).is_err());
        assert!(
            redeem_in(&db, &body(NONCE, Decision::Approve), now)
                .unwrap()
                .is_some()
        );
    }
}
