//! The outward-action gate (b155 6d, THE-2205): a Lead-confined
//! `issues.comment` / `issues.update` needs a live owner approval bound to
//! the call's digest (`thegn_core::outward_gate::approval_ids`). Without one
//! the daemon raises it and refuses with "needs owner approval ... retry the
//! identical call"; with one it consumes (revokes) the approval and lets the
//! call through once. `[lead] outward_allow` opts a cap out.
//!
//! Single use: the liveness check and the revoke run in one `with_db`
//! closure, under the daemon's DB lock, and the call executes only after the
//! revoke reported a withdrawn row. Two identical calls racing on one
//! approval therefore admit exactly one. A call that then fails at the
//! tracker has still spent its approval (fail closed).
//!
//! The owner sees [`describe`], the text this gate built from the call
//! itself, never the Lead's own request reason. It is kept in a bounded
//! in-memory map keyed by the full triple: after a daemon restart the panel
//! shows "unverified request" until the Lead retries, which records it again.

use std::collections::VecDeque;
use std::sync::Mutex;

use crate::daemon::service::DaemonService;
use thegn_core::approval_request::ApprovalRequest;
use thegn_core::control::Verb;
use thegn_core::outward_gate::{allowed_without_approval, approval_ids};
use thegn_core::pipeline_approval::{MIN_SHA_PREFIX, state_for};
use thegn_svc::control::{ControlError, ControlResult, LeadCaller};

/// How many outward descriptions the daemon remembers.
const DESCRIPTIONS_MAX: usize = 256;

type Triple = (String, String, String);

/// Trusted descriptions of the outward calls this daemon raised, oldest first.
static DESCRIPTIONS: Mutex<VecDeque<(Triple, String)>> = Mutex::new(VecDeque::new());

fn remember(triple: Triple, text: String) {
    let mut seen = DESCRIPTIONS.lock().expect("outward descriptions lock");
    remember_in(&mut seen, DESCRIPTIONS_MAX, triple, text);
}

/// Record `text` for `triple` in `seen`, keeping at most `max` entries.
fn remember_in(seen: &mut VecDeque<(Triple, String)>, max: usize, triple: Triple, text: String) {
    seen.retain(|(known, _)| *known != triple);
    while seen.len() >= max.max(1) {
        seen.pop_front();
    }
    seen.push_back((triple, text));
}

fn lookup(
    seen: &VecDeque<(Triple, String)>,
    issue: &str,
    stage: &str,
    sha: &str,
) -> Option<String> {
    seen.iter()
        .find(|((i, s, h), _)| i == issue && s == stage && h == sha)
        .map(|(_, text)| text.clone())
}

/// The trusted description of an outward approval this daemon raised;
/// `None` for a triple it never built (a forged request, or one raised
/// before a restart).
pub(crate) fn describe(issue: &str, stage: &str, sha: &str) -> Option<String> {
    lookup(
        &DESCRIPTIONS.lock().expect("outward descriptions lock"),
        issue,
        stage,
        sha,
    )
}

fn verb_for(cap: &str) -> Verb {
    if cap == "issues.update" {
        Verb::IssuesUpdate
    } else {
        Verb::IssuesComment
    }
}

/// Admit (or refuse) one outward call by a Lead-confined caller.
pub(crate) async fn gate(
    svc: &DaemonService,
    cap: &str,
    target: &str,
    body: serde_json::Value,
    caller: &LeadCaller,
) -> ControlResult<()> {
    let verb = verb_for(cap);
    let allow: &[String] = svc
        .config
        .lead
        .as_ref()
        .map_or(&[], |lead| lead.outward_allow.as_slice());
    if allowed_without_approval(cap, allow) {
        thegn_svc::control::http::audit_lead_allowed(
            &caller.pairing_id,
            &caller.label,
            verb,
            target,
        );
        return Ok(());
    }
    let (issue, stage, sha) = approval_ids(cap, target, &body);
    let ttl = svc.config.pipeline.supervisor.approval_ttl_secs;
    let (check_issue, check_stage, check_sha) = (issue.clone(), stage.clone(), sha.clone());
    let consumed = svc
        .with_db(move |db| {
            let now = thegn_core::util::now_ms();
            let latest = db.latest_approval(&check_issue, &check_stage)?;
            let exact = latest
                .as_ref()
                .is_some_and(|a| a.commit_sha.trim().eq_ignore_ascii_case(&check_sha));
            if !exact || !state_for(latest.as_ref(), &check_sha, now, ttl).is_live() {
                return Ok(false);
            }
            Ok(db.revoke_approvals(&check_issue, &check_stage, now)? > 0)
        })
        .await?;
    if consumed {
        thegn_svc::control::http::audit_lead_allowed(
            &caller.pairing_id,
            &caller.label,
            verb,
            target,
        );
        return Ok(());
    }
    let text = thegn_core::outward_gate::describe(cap, target, &body);
    remember((issue.clone(), stage.clone(), sha.clone()), text.clone());
    let sha7: String = sha.chars().take(MIN_SHA_PREFIX).collect();
    super::approvals::raise(
        svc,
        ApprovalRequest {
            issue_id: issue,
            stage,
            commit_sha: sha,
            worktree: None,
            reason: Some(text),
        },
    )
    .await
    .map_err(ControlError::Internal)?;
    thegn_svc::control::http::audit_lead_refusal(&caller.pairing_id, &caller.label, verb, target);
    Err(ControlError::FailedPrecondition(format!(
        "needs owner approval: requested {sha7}; retry the identical call after approval"
    )))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::daemon::session::IdleTransition;
    use crate::daemon::tombstone::{MAX_TOMBSTONES, TOMBSTONE_TTL_MS};
    use serde_json::json;
    use std::collections::{HashMap, HashSet};
    use std::sync::atomic::AtomicBool;
    use std::sync::{Arc, Mutex};
    use thegn_core::pipeline_approval::Approval;
    use thegn_core::{db::Db, graveyard::Graveyard};
    use tokio::sync::{broadcast, mpsc};

    fn service(outward_allow: &[&str]) -> (DaemonService, tempfile::TempDir) {
        let profile = tempfile::Builder::new()
            .prefix("lead-outward-")
            .tempdir()
            .expect("private profile tempdir");
        let config = thegn_core::config::Config {
            lead: Some(thegn_core::config_lead::LeadConfig {
                outward_allow: outward_allow.iter().map(|cap| cap.to_string()).collect(),
                ..Default::default()
            }),
            ..Default::default()
        };
        let (events, _rx) = broadcast::channel(16);
        let (idle_tx, _idle_rx) = mpsc::unbounded_channel::<IdleTransition>();
        let svc = DaemonService {
            daemon_id: "test".into(),
            sessions: Arc::new(tokio::sync::Mutex::new(HashMap::new())),
            actor_tasks: Arc::new(tokio::sync::Mutex::new(Vec::new())),
            accepting: AtomicBool::new(true),
            tombs: Arc::new(tokio::sync::Mutex::new(Graveyard::new(
                MAX_TOMBSTONES,
                TOMBSTONE_TTL_MS,
            ))),
            native_ids: Mutex::new(HashMap::new()),
            spawning_relaunches: Arc::new(Mutex::new(HashSet::new())),
            launches: Mutex::new(crate::daemon::stage_launch::LaunchRegistry::default()),
            events,
            db: Arc::new(Mutex::new(Db::open_memory().expect("db"))),
            grace_ms: 0,
            idle_tx,
            shutdown: tokio_util::sync::CancellationToken::new(),
            config: Arc::new(config),
            profile_root: profile.path().to_path_buf(),
            endpoint: "/tmp/test.sock".into(),
        };
        (svc, profile)
    }

    fn caller() -> LeadCaller {
        LeadCaller {
            pairing_id: "p1".into(),
            label: "lead".into(),
        }
    }

    fn comment(text: &str) -> serde_json::Value {
        json!({"body": text, "repo": null, "account": null})
    }

    fn approve(svc: &DaemonService, (issue, stage, sha): &Triple) {
        svc.db
            .lock()
            .unwrap()
            .grant_approval(&Approval {
                issue_id: issue.clone(),
                stage: stage.clone(),
                commit_sha: sha.clone(),
                approver: "owner".into(),
                note: None,
                granted_at_ms: thegn_core::util::now_ms(),
                expires_at_ms: None,
                revoked_at_ms: None,
            })
            .unwrap();
    }

    async fn pending(svc: &DaemonService) -> Vec<Triple> {
        svc.with_db(|db| {
            super::super::approvals::pending_with_ttl(db, thegn_core::util::now_ms(), 0, 50)
        })
        .await
        .unwrap()
        .into_iter()
        .map(|p| (p.issue_id, p.stage, p.commit_sha))
        .collect()
    }

    fn refused_with_request(result: ControlResult<()>, sha: &str) {
        match result {
            Err(ControlError::FailedPrecondition(message)) => assert_eq!(
                message,
                format!(
                    "needs owner approval: requested {}; retry the identical call after approval",
                    &sha[..7]
                )
            ),
            other => panic!("{other:?}"),
        }
    }

    #[tokio::test]
    async fn no_approval_raises_and_refuses_then_one_approval_admits_once() {
        let (svc, _profile) = service(&[]);
        let body = comment("ship it");
        let ids = approval_ids("issues.comment", "linear:THE-1", &body);

        let first = gate(
            &svc,
            "issues.comment",
            "linear:THE-1",
            body.clone(),
            &caller(),
        )
        .await;
        refused_with_request(first, &ids.2);
        assert_eq!(pending(&svc).await, [ids.clone()]);
        assert_eq!(
            describe(&ids.0, &ids.1, &ids.2).as_deref(),
            Some("issues.comment on linear:THE-1: ship it")
        );
        // Retrying before approval refuses again and asks only once.
        refused_with_request(
            gate(
                &svc,
                "issues.comment",
                "linear:THE-1",
                body.clone(),
                &caller(),
            )
            .await,
            &ids.2,
        );
        assert_eq!(pending(&svc).await.len(), 1);

        // An approval for a different body does not admit this one.
        approve(
            &svc,
            &approval_ids("issues.comment", "linear:THE-1", &comment("other")),
        );
        refused_with_request(
            gate(
                &svc,
                "issues.comment",
                "linear:THE-1",
                body.clone(),
                &caller(),
            )
            .await,
            &ids.2,
        );

        // Approved: the identical call passes exactly once.
        approve(&svc, &ids);
        assert!(pending(&svc).await.is_empty());
        gate(
            &svc,
            "issues.comment",
            "linear:THE-1",
            body.clone(),
            &caller(),
        )
        .await
        .unwrap();
        // The approval was consumed: the next identical call raises again.
        refused_with_request(
            gate(&svc, "issues.comment", "linear:THE-1", body, &caller()).await,
            &ids.2,
        );
        assert_eq!(pending(&svc).await, [ids]);
    }

    #[tokio::test]
    async fn concurrent_identical_calls_spend_one_approval_once() {
        let (svc, _profile) = service(&[]);
        let body = json!({"patch": {"title": "x"}, "repo": null, "account": null});
        let ids = approval_ids("issues.update", "gh:o/r#1", &body);
        approve(&svc, &ids);
        let (a, b) = tokio::join!(
            gate(&svc, "issues.update", "gh:o/r#1", body.clone(), &caller()),
            gate(&svc, "issues.update", "gh:o/r#1", body.clone(), &caller()),
        );
        assert_eq!(
            [a.is_ok(), b.is_ok()].iter().filter(|ok| **ok).count(),
            1,
            "{a:?} {b:?}"
        );
    }

    #[tokio::test]
    async fn a_sha_prefix_approval_does_not_admit_the_body() {
        let (svc, _profile) = service(&[]);
        let body = comment("ship it");
        let ids = approval_ids("issues.comment", "linear:THE-1", &body);
        approve(
            &svc,
            &(ids.0.clone(), ids.1.clone(), ids.2[..7].to_string()),
        );
        refused_with_request(
            gate(&svc, "issues.comment", "linear:THE-1", body, &caller()).await,
            &ids.2,
        );
    }

    #[tokio::test]
    async fn outward_allow_bypasses_only_the_listed_cap() {
        let (svc, _profile) = service(&["issues.comment"]);
        for _ in 0..2 {
            gate(
                &svc,
                "issues.comment",
                "linear:THE-1",
                comment("hi"),
                &caller(),
            )
            .await
            .unwrap();
        }
        assert!(pending(&svc).await.is_empty());
        let update = json!({"patch": {}, "repo": null, "account": null});
        let ids = approval_ids("issues.update", "linear:THE-1", &update);
        refused_with_request(
            gate(&svc, "issues.update", "linear:THE-1", update, &caller()).await,
            &ids.2,
        );
        assert_eq!(pending(&svc).await, [ids]);
    }

    #[test]
    fn descriptions_are_bounded_and_keyed_by_the_full_triple() {
        let key = |n: usize| {
            (
                "outward:bound".to_string(),
                "issues.comment".to_string(),
                format!("{n:040x}"),
            )
        };
        let mut seen = VecDeque::new();
        for n in 0..=4 {
            remember_in(&mut seen, 4, key(n), format!("text {n}"));
        }
        assert_eq!(seen.len(), 4);
        assert_eq!(
            lookup(&seen, &key(0).0, &key(0).1, &key(0).2),
            None,
            "evicted"
        );
        let last = key(4);
        assert_eq!(
            lookup(&seen, &last.0, &last.1, &last.2).as_deref(),
            Some("text 4")
        );
        assert_eq!(lookup(&seen, &last.0, "issues.update", &last.2), None);
        // Re-recording a triple replaces it rather than duplicating it.
        remember_in(&mut seen, 4, key(2), "again".into());
        assert_eq!(seen.len(), 4);
        assert_eq!(
            lookup(&seen, &key(2).0, &key(2).1, &key(2).2).as_deref(),
            Some("again")
        );
        assert_eq!(verb_for("issues.update"), Verb::IssuesUpdate);
        assert_eq!(verb_for("issues.comment"), Verb::IssuesComment);
    }
}
