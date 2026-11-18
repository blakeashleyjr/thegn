//! Approved `propose` starts (b155): an `automation_proposals` row waits for
//! the owner; on approval the pipeline starts at `review` (never `merge`)
//! after re-validating the stored proposal; on rejection or expiry the row is
//! marked and nothing starts.
//!
//! Trust: the approval triple only LOCATES a proposal. The digest is
//! recomputed from the stored (daemon-written) row and must equal the
//! approved sha, so an approval for a triple nobody proposed, or for a row
//! whose fields changed, starts nothing. Every start-time fact is re-checked
//! (registered repo, issue id shape, stage still configured, rule still
//! loaded and still `pipeline.start`, no autopilot ownership, not paused,
//! budget) before the one-shot `pending -> started` compare-and-set.

use std::sync::Arc;

use anyhow::{Context, bail};
use thegn_core::automation_autonomy::Autonomy;
use thegn_core::automation_budget::SpawnKind;
use thegn_core::config::Config;
use thegn_core::db::Db;
use thegn_core::db_approval_effects::ProposalRow;

use crate::automation_pipeline::{StartAs, StartTarget};
use crate::daemon::service::DaemonService;

const PENDING: &str = "pending";
const STARTED: &str = "started";
const FAILED: &str = "failed";
const REJECTED: &str = "rejected";
const EXPIRED: &str = "expired";
/// The capability a proposal's rule must still carry.
const PIPELINE_START: &str = "pipeline.start";

/// Why a stored proposal may not start, before any start-time re-check.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ProposalCheck {
    /// Start it at this autonomy (always `review`).
    Start { autonomy: Autonomy },
    /// No proposal waits on this triple.
    Unknown,
    /// The proposal is no longer pending (already started, rejected, ...).
    NotPending(String),
    /// The proposal's 7-day window passed; mark it expired.
    Expired,
    /// The digest of the stored row is not the approved sha.
    DigestMismatch,
}

/// The pure decision for an approved triple: a pending, unexpired proposal
/// whose recomputed digest equals the approved triple starts at `review`.
pub(crate) fn check_proposal(
    row: Option<&ProposalRow>,
    issue: &str,
    stage: &str,
    sha: &str,
    now_ms: i64,
) -> ProposalCheck {
    let Some(row) = row else {
        return ProposalCheck::Unknown;
    };
    if row.state != PENDING {
        return ProposalCheck::NotPending(row.state.clone());
    }
    if now_ms >= row.expires_at_ms {
        return ProposalCheck::Expired;
    }
    let (d_issue, d_stage, d_sha) = thegn_core::approval_digest::proposal_ids(row);
    if d_issue != issue || d_stage != stage || !d_sha.eq_ignore_ascii_case(sha) {
        return ProposalCheck::DigestMismatch;
    }
    // An approved proposal never starts above `review`, whatever the rule says.
    ProposalCheck::Start {
        autonomy: Autonomy::Review,
    }
}

/// The start-time facts re-read just before the start.
struct Revalidated {
    target: StartTarget,
}

/// Start the proposal waiting on approval `(issue, stage, sha)`.
pub(crate) async fn on_approved(
    svc: &Arc<DaemonService>,
    issue: &str,
    stage: &str,
    sha: &str,
) -> anyhow::Result<()> {
    let (q_issue, q_stage, q_sha) = (issue.to_string(), stage.to_string(), sha.to_string());
    let row = svc
        .with_db(move |db| db.proposal_for(&q_issue, &q_stage, &q_sha))
        .await
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    let autonomy = match check_proposal(row.as_ref(), issue, stage, sha, thegn_core::util::now_ms())
    {
        ProposalCheck::Start { autonomy } => autonomy,
        ProposalCheck::Unknown => bail!("no stored proposal matches this approval"),
        ProposalCheck::NotPending(state) => bail!("the proposal is already {state}"),
        ProposalCheck::Expired => {
            if let Some(id) = row.as_ref().map(|r| r.id) {
                svc.with_db(move |db| db.set_proposal_state(id, PENDING, EXPIRED))
                    .await
                    .map_err(|e| anyhow::anyhow!("{e}"))?;
            }
            bail!("the proposal expired before it was approved");
        }
        ProposalCheck::DigestMismatch => {
            bail!("the approval does not match the stored proposal's digest")
        }
    };
    let row = row.context("proposal vanished")?;
    let cfg = Arc::clone(&svc.config);

    let pre_row = row.clone();
    let pre_cfg = Arc::clone(&cfg);
    let revalidated = tokio::task::spawn_blocking(move || revalidate(&pre_cfg, &pre_row))
        .await
        .context("joining the proposal re-validation")??;

    let id = row.id;
    let claimed = svc
        .with_db(move |db| db.set_proposal_state(id, PENDING, STARTED))
        .await
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    if !claimed {
        bail!("the proposal was already settled by another answer");
    }

    let origin = thegn_svc::control::AutomationOrigin {
        root_event_id: format!("proposal:{id}"),
        rule_id: row.rule_id.clone(),
        run_id: format!("proposal:{id}"),
    };
    let started = crate::automation_pipeline::start_stage(
        cfg,
        StartAs {
            autonomy,
            rule_id: row.rule_id.clone(),
            origin,
        },
        revalidated.target,
    )
    .await;
    if let Err(error) = started {
        let message: String = format!(
            "Approved proposal for {} ({}) failed to start: {error:#}",
            row.issue_id, row.stage
        )
        .chars()
        .filter(|c| !c.is_control())
        .take(512)
        .collect();
        let source = format!("automation:{}", row.rule_id);
        let worktree = row.repo_root.clone();
        let recorded = svc
            .with_db(move |db| {
                use thegn_core::store::NotificationStore;
                db.set_proposal_state(id, STARTED, FAILED)?;
                db.put_notification("automation_failed", &source, &message, &worktree)
            })
            .await;
        match recorded {
            Ok(nid) => crate::daemon::lead_caps::feed::notification(&svc.events, nid),
            Err(e) => tracing::warn!(
                target: "thegn::automation",
                error = %e,
                "failed to record a failed proposal start"
            ),
        }
        return Err(error);
    }
    Ok(())
}

/// Re-check every start-time fact against the current config and DB. Runs
/// on a blocking thread (fresh DB handle; the autopilot and repo checks may
/// touch the filesystem).
fn revalidate(cfg: &Config, row: &ProposalRow) -> anyhow::Result<Revalidated> {
    let db = Db::open()?;
    if thegn_core::automation_control::paused(&db) {
        bail!("automations are paused");
    }
    crate::automation_budget::admit(&db, cfg, SpawnKind::NewLane)
        .map_err(|refusal| anyhow::anyhow!("{refusal}"))?;
    let repo_root = crate::automation_pipeline::registered_repo_root(&db, &row.repo_root)?;
    let issue_id = crate::automation_pipeline::validate_issue_id(&row.issue_id)?;
    if issue_id != row.issue_id {
        bail!("the proposal's issue id is not canonical");
    }
    let stage = crate::automation_pipeline::stage_for(cfg, Some(&row.stage))?;
    rule_still_starts(cfg, &db, &row.rule_id)?;
    let key = thegn_core::autopilot::AutopilotIssueKey::new(
        row.provider.clone(),
        row.account.clone(),
        issue_id.clone(),
    );
    if crate::automation_pipeline::autopilot_owns(&db, &key)? {
        bail!("issue is owned by [autopilot]; the proposal will not start a second worker");
    }
    let linked = crate::automation_pipeline::linked_worktree(&db, &repo_root, &issue_id)?;
    Ok(Revalidated {
        target: StartTarget {
            repo_root,
            issue_id,
            stage,
            linked,
        },
    })
}

/// The proposal's rule must still be loaded, enabled and a `pipeline.start`.
fn rule_still_starts(cfg: &Config, db: &Db, rule_id: &str) -> anyhow::Result<()> {
    use thegn_core::store::AutomationStore;
    let effective = cfg.effective_automations();
    if !effective.enabled {
        bail!("automations are disabled");
    }
    let config_rules = effective.compiled_rules().unwrap_or_default();
    let rules = crate::automation_runtime::with_lead_rules(db, config_rules);
    let Some(rule) = rules.iter().find(|r| r.id == rule_id) else {
        bail!("the proposal's automation rule is no longer loaded");
    };
    if rule.action.cap.trim() != PIPELINE_START {
        bail!("the proposal's automation rule no longer starts the pipeline");
    }
    let override_enabled = db
        .automation_state(rule_id)
        .ok()
        .flatten()
        .and_then(|s| s.enabled_override);
    if !override_enabled.unwrap_or(rule.enabled) {
        bail!("the proposal's automation rule is disabled");
    }
    Ok(())
}

/// Mark the proposal waiting on approval `(issue, stage, sha)` rejected.
pub(crate) async fn on_rejected(
    svc: &Arc<DaemonService>,
    issue: &str,
    stage: &str,
    sha: &str,
) -> anyhow::Result<()> {
    let (q_issue, q_stage, q_sha) = (issue.to_string(), stage.to_string(), sha.to_string());
    svc.with_db(move |db| {
        if let Some(row) = db.proposal_for(&q_issue, &q_stage, &q_sha)? {
            db.set_proposal_state(row.id, PENDING, REJECTED)?;
        }
        Ok(())
    })
    .await
    .map_err(|e| anyhow::anyhow!("{e}"))
}

/// The trusted description of a proposal approval, from the stored row (only
/// when its digest still matches the triple).
pub(crate) fn describe(db: &Db, issue: &str, stage: &str, sha: &str) -> Option<String> {
    let row = db.proposal_for(issue, stage, sha).ok()??;
    let (d_issue, d_stage, d_sha) = thegn_core::approval_digest::proposal_ids(&row);
    if d_issue != issue || d_stage != stage || !d_sha.eq_ignore_ascii_case(sha) {
        return None;
    }
    Some(describe_row(&row))
}

fn describe_row(row: &ProposalRow) -> String {
    let repo = std::path::Path::new(&row.repo_root)
        .file_name()
        .map_or_else(
            || row.repo_root.clone(),
            |n| n.to_string_lossy().into_owned(),
        );
    format!(
        "Start {} for {} in {repo} (rule {})",
        row.stage, row.issue_id, row.rule_id
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOW: i64 = 1_000_000;

    fn row() -> ProposalRow {
        let mut row = ProposalRow {
            id: 0,
            approval_issue: String::new(),
            approval_stage: String::new(),
            approval_sha: String::new(),
            rule_id: "triage".into(),
            repo_root: "/code/thegn".into(),
            issue_id: "linear:THE-7".into(),
            provider: "linear".into(),
            account: "work".into(),
            stage: "plan".into(),
            base_sha: "c".repeat(40),
            created_at_ms: NOW - 10,
            expires_at_ms: NOW + 10,
            state: PENDING.into(),
        };
        let (i, s, d) = thegn_core::approval_digest::proposal_ids(&row);
        row.approval_issue = i;
        row.approval_stage = s;
        row.approval_sha = d;
        row
    }

    fn check(row: &ProposalRow, sha: &str, now: i64) -> ProposalCheck {
        check_proposal(
            Some(row),
            &row.approval_issue,
            &row.approval_stage,
            sha,
            now,
        )
    }

    #[test]
    fn an_approved_proposal_starts_at_review_never_merge() {
        let r = row();
        assert_eq!(
            check(&r, &r.approval_sha, NOW),
            ProposalCheck::Start {
                autonomy: Autonomy::Review
            }
        );
    }

    #[test]
    fn a_digest_mismatch_refuses() {
        let r = row();
        // A forged sha for the right issue/stage.
        assert_eq!(
            check(&r, &"f".repeat(40), NOW),
            ProposalCheck::DigestMismatch
        );
        // A stored row whose trusted fields no longer hash to its triple.
        let mut tampered = r.clone();
        tampered.repo_root = "/elsewhere".into();
        assert_eq!(
            check(&tampered, &r.approval_sha, NOW),
            ProposalCheck::DigestMismatch
        );
        // The right sha under another stage.
        assert_eq!(
            check_proposal(
                Some(&r),
                &r.approval_issue,
                "start.other",
                &r.approval_sha,
                NOW
            ),
            ProposalCheck::DigestMismatch
        );
    }

    #[test]
    fn an_expired_or_settled_proposal_refuses() {
        let r = row();
        assert_eq!(check(&r, &r.approval_sha, NOW + 10), ProposalCheck::Expired);
        for state in [STARTED, REJECTED, EXPIRED, FAILED] {
            let mut settled = r.clone();
            settled.state = state.into();
            assert_eq!(
                check(&settled, &r.approval_sha, NOW),
                ProposalCheck::NotPending(state.into())
            );
        }
        assert_eq!(
            check_proposal(None, "linear:THE-7", "start.plan", &r.approval_sha, NOW),
            ProposalCheck::Unknown
        );
    }

    #[test]
    fn describe_uses_the_stored_row_and_rejects_a_forged_triple() {
        let db = Db::open_memory().unwrap();
        let r = row();
        assert!(db.put_proposal(&r).unwrap());
        assert_eq!(
            describe(&db, &r.approval_issue, &r.approval_stage, &r.approval_sha).as_deref(),
            Some("Start plan for linear:THE-7 in thegn (rule triage)")
        );
        assert_eq!(
            describe(&db, &r.approval_issue, &r.approval_stage, &"f".repeat(40)),
            None
        );
    }
}
