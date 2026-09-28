//! `thegn supervise <action>` — the mechanical half of a `[[pipeline.stages]]`
//! chart, and the window onto it.
//!
//! # What these verbs are for
//!
//! A pipeline chart has no engine: every transition, every compile/test/lint
//! run and every land is dispatched by hand, so the pipeline stalls the moment
//! no supervising agent is watching it. Most of what stalls is not judgement —
//! running a build is a rule, classifying its output is a rule, advancing a
//! lane whose declared preconditions are all met is a rule.
//!
//! [`thegn_core::pipeline_supervise::plan`] is those rules as a pure function.
//! These verbs gather the facts it needs, call it, and print the result.
//!
//! # Read-only, by construction
//!
//! Nothing here applies an action. `plan` prints what the supervisor *would*
//! do and the recorded facts that would authorize each one; `status` groups the
//! same information by what a person needs to do about it. That split is the
//! point: an operator must be able to read the supervisor's behaviour against
//! their own chart, in full, before switching it on — which is also why
//! `[pipeline.supervisor] enabled` defaults to false and these verbs work
//! regardless of it.

use anyhow::Result;
use std::collections::BTreeMap;
use std::path::Path;

use thegn_core::config::Config;
use thegn_core::db::Db;
use thegn_core::issue::AgentDispatch;
use thegn_core::outln;
use thegn_core::pipeline_approval::{ApprovalState, state_for};
use thegn_core::pipeline_run::{self, RowLiveness};
use thegn_core::pipeline_supervise::{self, LaneFacts, SuperviseAction};
use thegn_core::store::{NotificationStore as _, WorktreeAuxStore as _};
use thegn_core::util::{git_ok, git_out};

#[derive(clap::Subcommand, Clone)]
pub enum Action {
    /// Print what the supervisor would do about every lane, and the recorded
    /// facts that would authorize each one. Applies nothing.
    ///
    /// This is the audit surface: read it against your own chart before
    /// setting `[pipeline.supervisor] enabled = true`. One line per lane, so a
    /// lane the supervisor decided to leave alone is visible as such rather
    /// than silently absent.
    Plan {
        /// Only lanes for this issue (`"<provider>:<key>"`, e.g.
        /// `linear:THE-407`).
        #[arg(long)]
        issue: Option<String>,
        /// Include lanes whose row has reached a terminal status. Off by
        /// default: a closed row is normally finished business.
        #[arg(long)]
        all: bool,
        /// Emit the plan as JSON.
        #[arg(long)]
        json: bool,
    },
    /// The lane digest, grouped by what it needs from you: decisions first,
    /// then findings, then what is simply in flight.
    Status {
        /// Only lanes for this issue.
        #[arg(long)]
        issue: Option<String>,
        /// Include lanes whose row has reached a terminal status.
        #[arg(long)]
        all: bool,
        /// Emit as JSON.
        #[arg(long)]
        json: bool,
    },
    /// The validation runs recorded for a lane: which task, against which
    /// commit, what it established, and the capped digest of its output.
    Validations {
        /// A roster row id (see `thegn dispatch list`). Omit for every row
        /// carrying a recorded validation.
        #[arg(long)]
        row: Option<i64>,
        /// Print each result's stored output digest, not just its class.
        #[arg(long)]
        digest: bool,
        /// Emit as JSON.
        #[arg(long)]
        json: bool,
    },
}

pub fn run(cfg: &Config, action: Action) -> Result<()> {
    match action {
        Action::Plan { issue, all, json } => plan(cfg, issue.as_deref(), all, json),
        Action::Status { issue, all, json } => status(cfg, issue.as_deref(), all, json),
        Action::Validations { row, digest, json } => validations(row, digest, json),
    }
}

// --- fact gathering ---------------------------------------------------------

/// One lane's facts plus the row they were gathered from, so the printers can
/// name the issue and worktree without a second lookup.
struct Lane {
    row: AgentDispatch,
    facts: LaneFacts,
    approval: ApprovalState,
}

/// Join the roster, git, and the two supervisor ledgers into the facts the
/// planner consumes.
///
/// Every git read is per-row and cheap (`rev-parse`, `merge-base
/// --is-ancestor`); the expensive question — does this build? — is never asked
/// here, because answering it is an *action* the planner has to authorize
/// first.
fn gather(cfg: &Config, db: &Db, issue: Option<&str>, all: bool) -> Result<Vec<Lane>> {
    let now_ms = thegn_core::util::now_ms();
    let rows = db.list_dispatches()?;
    // A row's children give it its `child_stages`; built once rather than
    // re-scanned per row.
    let mut children: BTreeMap<i64, Vec<String>> = BTreeMap::new();
    for r in &rows {
        if let (Some(parent), Some(stage)) = (r.parent_id, r.stage.as_deref()) {
            children.entry(parent).or_default().push(stage.to_string());
        }
    }
    let queued: Vec<String> = db
        .list_merge_queue()
        .unwrap_or_default()
        .into_iter()
        .filter(|q| !q.status.eq_ignore_ascii_case("landed"))
        .map(|q| q.worktree)
        .collect();

    let mut git_cache: BTreeMap<String, WorktreeGit> = BTreeMap::new();
    let mut out = Vec::new();
    for row in rows {
        let Some(stage) = row.stage.clone() else {
            // Not a pipeline dispatch — the supervisor has no chart for it.
            continue;
        };
        if let Some(want) = issue
            && row.issue_id != want
        {
            continue;
        }
        if !all
            && !row.status.is_active()
            && row.status != thegn_core::issue::AgentDispatchStatus::Done
        {
            continue;
        }
        // Several rows of one lane share a worktree (a stage per row), so the
        // two git questions are asked per WORKTREE, not per row: on the live
        // roster that is 678 rows across a few dozen worktrees, and the naive
        // form spawned 1356 processes for a read-only listing.
        let git = match git_cache.get(&row.worktree_path) {
            Some(cached) => cached.clone(),
            None => {
                let facts = worktree_git_facts(cfg, Path::new(&row.worktree_path));
                git_cache.insert(row.worktree_path.clone(), facts.clone());
                facts
            }
        };
        let head_sha = git.head_sha;
        let verify = crate::cmd::dispatch::verify_facts(&row);
        let facts = LaneFacts {
            row: row.id,
            issue_id: row.issue_id.clone(),
            stage: stage.clone(),
            status: row.status,
            // The contract of `worker_live`: unknown maps to `true`. That is
            // exactly what `row_liveness` returns for a row with no exit stamp,
            // so the mapping is one match rather than a hand-rolled guess.
            worker_live: matches!(pipeline_run::row_liveness(&row, now_ms), RowLiveness::Live),
            merged_into_target: git.merged_into_target,
            enqueued: queued.iter().any(|w| w == &row.worktree_path),
            artifact_tracked: verify.tracked,
            report_present: verify.report_present,
            validations: db.validations_for_dispatch(row.id).unwrap_or_default(),
            approval: db.latest_approval(&row.issue_id, &stage).unwrap_or(None),
            child_stages: children.get(&row.id).cloned().unwrap_or_default(),
            head_sha: head_sha.clone(),
        };
        let approval = state_for(
            facts.approval.as_ref(),
            &head_sha,
            now_ms,
            cfg.pipeline.supervisor.approval_ttl_secs,
        );
        out.push(Lane {
            row,
            facts,
            approval,
        });
    }
    Ok(out)
}

/// The two git facts about a worktree, gathered together so they can be cached
/// per worktree rather than re-derived per roster row.
#[derive(Debug, Clone, Default)]
struct WorktreeGit {
    /// The branch tip, or empty when the worktree is gone or has no commits.
    head_sha: String,
    /// The tip is already an ancestor of the repo's target branch.
    merged_into_target: bool,
}

/// Read a worktree's tip and whether it has already landed.
///
/// "Already landed" is answered with `merge-base --is-ancestor` against the
/// repo's configured target, never by diffing: a merged lane shows **no** diff,
/// so a diff-based test reads it as an empty change and files it as something
/// suspicious. That mistake put four already-landed lanes into a review queue.
fn worktree_git_facts(cfg: &Config, worktree: &Path) -> WorktreeGit {
    let Some(head_sha) = git_out(worktree, &["rev-parse", "HEAD"]) else {
        // Gone, or not a repository: the caller holds this lane rather than
        // reasoning about a tree that is not there.
        return WorktreeGit::default();
    };
    let Some(root) = git_out(worktree, &["rev-parse", "--show-toplevel"]) else {
        return WorktreeGit {
            head_sha,
            merged_into_target: false,
        };
    };
    let target = crate::integrate::resolve_target(&cfg.merge_queue, Path::new(&root));
    let merged_into_target = git_ok(
        Path::new(&root),
        &["merge-base", "--is-ancestor", &head_sha, &target],
    );
    WorktreeGit {
        head_sha,
        merged_into_target,
    }
}

// --- plan -------------------------------------------------------------------

fn plan(cfg: &Config, issue: Option<&str>, all: bool, json: bool) -> Result<()> {
    let db = Db::open()?;
    let lanes = gather(cfg, &db, issue, all)?;
    let facts: Vec<LaneFacts> = lanes.iter().map(|l| l.facts.clone()).collect();
    let actions = pipeline_supervise::plan(
        &facts,
        &cfg.pipeline,
        &cfg.pipeline.supervisor,
        thegn_core::util::now_ms(),
    );

    if json {
        let vals: Vec<serde_json::Value> = lanes
            .iter()
            .zip(&actions)
            .map(|(lane, action)| action_json(lane, action))
            .collect();
        return super::emit_json(&serde_json::json!({
            "enabled": cfg.pipeline.supervisor.enabled,
            "actions": vals,
        }));
    }

    if !cfg.pipeline.supervisor.enabled {
        outln!(
            "[pipeline.supervisor] enabled = false — this is what WOULD happen; nothing is applied."
        );
    }
    if lanes.is_empty() {
        outln!("no pipeline lanes on the roster.");
        return Ok(());
    }
    for (lane, action) in lanes.iter().zip(&actions) {
        outln!(
            "row {:<5} {:<16} {:<12} {:<9} {}",
            lane.row.id,
            lane.row.issue_id,
            lane.facts.stage,
            action.token(),
            describe(lane, action)
        );
    }
    Ok(())
}

/// The human-readable tail of a plan line: what the action would do, and what
/// permitted it.
fn describe(lane: &Lane, action: &SuperviseAction) -> String {
    match action {
        SuperviseAction::Validate {
            tasks, commit_sha, ..
        } if !tasks.is_empty() => {
            format!("{} @ {}", tasks.join(" + "), short(commit_sha))
        }
        SuperviseAction::Validate { commit_sha, .. } => format!("@ {}", short(commit_sha)),
        SuperviseAction::Advance {
            next_stage,
            authorized_by,
            ..
        } => format!("→ {next_stage}  ({})", authorized_by.describe()),
        SuperviseAction::Enqueue {
            commit_sha,
            authorized_by,
            ..
        } => format!(
            "→ merge queue @ {}  ({})",
            short(commit_sha),
            authorized_by.describe()
        ),
        SuperviseAction::Escalate { why, .. } => {
            let mut s = why.explain();
            // An approval that exists but does not apply is the most confusing
            // state to be in, so say what IS on record.
            if why.wants_primary() && !matches!(lane.approval, ApprovalState::Absent) {
                s.push_str(&format!(" [{}]", lane.approval.explain()));
            }
            s
        }
        SuperviseAction::Hold { why, .. } => (*why).to_string(),
    }
}

fn action_json(lane: &Lane, action: &SuperviseAction) -> serde_json::Value {
    let mut v = serde_json::json!({
        "row": lane.row.id,
        "issue": lane.row.issue_id,
        "stage": lane.facts.stage,
        "action": action.token(),
        "mutating": action.is_mutating(),
        "head": lane.facts.head_sha,
        "approval": lane.approval.token(),
        "detail": describe(lane, action),
    });
    match action {
        SuperviseAction::Validate { tasks, .. } => v["tasks"] = serde_json::json!(tasks),
        SuperviseAction::Advance {
            next_stage,
            authorized_by,
            ..
        } => {
            v["next_stage"] = serde_json::json!(next_stage);
            v["authorized_by"] = serde_json::json!(authorized_by.tokens());
            v["ungated"] = serde_json::json!(authorized_by.is_ungated());
        }
        SuperviseAction::Enqueue { authorized_by, .. } => {
            v["authorized_by"] = serde_json::json!(authorized_by.tokens());
            v["ungated"] = serde_json::json!(authorized_by.is_ungated());
        }
        SuperviseAction::Escalate { why, .. } => {
            v["reason"] = serde_json::json!(why.token());
            v["wants_primary"] = serde_json::json!(why.wants_primary());
        }
        SuperviseAction::Hold { .. } => {}
    }
    v
}

// --- status -----------------------------------------------------------------

fn status(cfg: &Config, issue: Option<&str>, all: bool, json: bool) -> Result<()> {
    let db = Db::open()?;
    let lanes = gather(cfg, &db, issue, all)?;
    let facts: Vec<LaneFacts> = lanes.iter().map(|l| l.facts.clone()).collect();
    let actions = pipeline_supervise::plan(
        &facts,
        &cfg.pipeline,
        &cfg.pipeline.supervisor,
        thegn_core::util::now_ms(),
    );

    // Grouped by what it asks of the reader, decisions first — the ordering is
    // the whole value of this verb over `plan`.
    let mut decisions = Vec::new();
    let mut findings = Vec::new();
    let mut moving = Vec::new();
    let mut held = Vec::new();
    for (lane, action) in lanes.iter().zip(&actions) {
        match action {
            SuperviseAction::Escalate { why, .. } if why.wants_primary() => {
                decisions.push((lane, action))
            }
            SuperviseAction::Escalate { .. } => findings.push((lane, action)),
            SuperviseAction::Validate { .. }
            | SuperviseAction::Advance { .. }
            | SuperviseAction::Enqueue { .. } => moving.push((lane, action)),
            SuperviseAction::Hold { .. } => held.push((lane, action)),
        }
    }

    if json {
        let group = |rows: &[(&Lane, &SuperviseAction)]| -> Vec<serde_json::Value> {
            rows.iter().map(|(l, a)| action_json(l, a)).collect()
        };
        return super::emit_json(&serde_json::json!({
            "enabled": cfg.pipeline.supervisor.enabled,
            "needs_you": group(&decisions),
            "findings": group(&findings),
            "in_flight": group(&moving),
            "held": group(&held),
        }));
    }

    if decisions.is_empty() && findings.is_empty() && moving.is_empty() {
        outln!("nothing owed: {} lane(s), all held.", held.len());
        return Ok(());
    }
    section("NEEDS YOU", &decisions);
    section("FINDINGS", &findings);
    section("IN FLIGHT", &moving);
    if !held.is_empty() {
        outln!("");
        outln!("held: {} lane(s) (see `thegn supervise plan`)", held.len());
    }
    Ok(())
}

fn section(title: &str, rows: &[(&Lane, &SuperviseAction)]) {
    if rows.is_empty() {
        return;
    }
    outln!("");
    outln!("{title}");
    for (lane, action) in rows {
        outln!(
            "  row {:<5} {:<16} {:<12} {}",
            lane.row.id,
            lane.row.issue_id,
            lane.facts.stage,
            describe(lane, action)
        );
    }
}

// --- validations ------------------------------------------------------------

fn validations(row: Option<i64>, want_digest: bool, json: bool) -> Result<()> {
    let db = Db::open()?;
    let rows: Vec<i64> = match row {
        Some(id) => vec![id],
        None => db
            .list_dispatches()?
            .into_iter()
            .map(|r| r.id)
            .filter(|id| {
                db.validations_for_dispatch(*id)
                    .map(|v| !v.is_empty())
                    .unwrap_or(false)
            })
            .collect(),
    };

    let mut vals = Vec::new();
    for id in rows {
        for v in db.validations_for_dispatch(id)? {
            let digest = if want_digest || json {
                db.validation_digest(id, &v.task, &v.commit_sha)?
                    .unwrap_or_default()
            } else {
                String::new()
            };
            if json {
                vals.push(serde_json::json!({
                    "row": id,
                    "task": v.task,
                    "class": v.class.as_str(),
                    "commit": v.commit_sha,
                    "attempts": v.attempts,
                    "digest": digest,
                }));
            } else {
                outln!(
                    "row {:<5} {:<12} {:<18} @ {}  (attempt {})",
                    id,
                    v.task,
                    v.class.as_str(),
                    short(&v.commit_sha),
                    v.attempts
                );
                if want_digest && !digest.is_empty() {
                    for line in digest.lines() {
                        outln!("      {line}");
                    }
                }
            }
        }
    }
    if json {
        return super::emit_json(&serde_json::json!({ "validations": vals }));
    }
    if vals.is_empty() {
        outln!("no validation runs recorded.");
    }
    Ok(())
}

/// Abbreviate a commit for display. Never used for comparison — matching is
/// [`thegn_core::pipeline_approval`]'s job and is prefix-aware in both
/// directions.
fn short(sha: &str) -> String {
    sha.chars().take(9).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use thegn_core::config_pipeline::Requirement;
    use thegn_core::issue::AgentDispatchStatus;
    use thegn_core::pipeline_approval::Approval;

    const HEAD: &str = "4babb0901f2c3d4e5f60718293a4b5c6d7e8f901";

    fn lane(action_stage: &str) -> Lane {
        let row = AgentDispatch {
            id: 677,
            issue_id: "linear:THE-407".into(),
            worktree_path: "/tmp/wt".into(),
            agent_name: "codex".into(),
            dispatched_at_ms: 0,
            status: AgentDispatchStatus::Done,
            stage: Some(action_stage.into()),
            parent_id: None,
            session_id: None,
            artifact_path: None,
            note: None,
            chunk_path: None,
            report: None,
            exit_code: Some(0),
            exited_at_ms: Some(0),
        };
        let facts = LaneFacts {
            row: 677,
            issue_id: "linear:THE-407".into(),
            stage: action_stage.into(),
            status: AgentDispatchStatus::Done,
            worker_live: false,
            head_sha: HEAD.into(),
            merged_into_target: false,
            enqueued: false,
            artifact_tracked: true,
            report_present: true,
            validations: vec![],
            approval: None,
            child_stages: vec![],
        };
        Lane {
            row,
            facts,
            approval: ApprovalState::Absent,
        }
    }

    #[test]
    fn an_advance_line_names_what_authorized_it() {
        // The audit property: a plan must say WHY, not only WHAT, or reading it
        // establishes nothing about whether the gates are working.
        let l = lane("code");
        let a = SuperviseAction::Advance {
            row: 677,
            next_stage: "review".into(),
            authorized_by: vec![Requirement::ParentArtifact, Requirement::Approval].into(),
        };
        let s = describe(&l, &a);
        assert!(s.contains("review"), "{s}");
        assert!(s.contains("parent_artifact"), "{s}");
        assert!(s.contains("approval"), "{s}");
    }

    #[test]
    fn an_ungated_advance_is_spelled_out_rather_than_left_blank() {
        // Found by running `supervise plan` against the live chart: a stage
        // with no `requires` printed "(authorized by: )". An ungated transition
        // is the most consequential thing a chart can say, so it must read as
        // a statement, not as a rendering bug.
        let l = lane("maintenance-investigate");
        let a = SuperviseAction::Advance {
            row: 665,
            next_stage: "maintenance-code".into(),
            authorized_by: Vec::new().into(),
        };
        let s = describe(&l, &a);
        assert!(s.contains("ungated"), "{s}");
        assert!(!s.contains("()"), "rendered an empty authorization: {s}");

        let v = action_json(&l, &a);
        assert_eq!(v["ungated"], true);
        assert_eq!(v["authorized_by"], serde_json::json!([]));
    }

    #[test]
    fn an_awaiting_approval_line_names_the_commit_to_review() {
        let l = lane("code");
        let a = SuperviseAction::Escalate {
            row: 677,
            why: thegn_core::pipeline_supervise::EscalationReason::AwaitingApproval {
                stage: "code".into(),
                commit_sha: HEAD.into(),
            },
        };
        assert!(
            describe(&l, &a).contains(HEAD),
            "must name the exact commit"
        );
    }

    #[test]
    fn a_superseded_approval_is_spelled_out_rather_than_reported_as_absent() {
        // The most confusing state to be in: you approved something, and the
        // lane still will not move. The line must say what is on record.
        let mut l = lane("code");
        l.approval = ApprovalState::Superseded {
            approved: "1111111".into(),
            head: HEAD.into(),
        };
        let a = SuperviseAction::Escalate {
            row: 677,
            why: thegn_core::pipeline_supervise::EscalationReason::AwaitingApproval {
                stage: "code".into(),
                commit_sha: HEAD.into(),
            },
        };
        let s = describe(&l, &a);
        assert!(s.contains("1111111"), "did not say what was approved: {s}");
        assert!(s.contains("re-review"), "did not name the remedy: {s}");
    }

    #[test]
    fn a_hold_explains_itself() {
        let l = lane("code");
        let a = SuperviseAction::Hold {
            row: 677,
            why: "already merged into the target branch",
        };
        assert_eq!(describe(&l, &a), "already merged into the target branch");
    }

    #[test]
    fn the_json_shape_carries_the_audit_fields() {
        let l = lane("code");
        let a = SuperviseAction::Advance {
            row: 677,
            next_stage: "review".into(),
            authorized_by: vec![Requirement::Approval].into(),
        };
        let v = action_json(&l, &a);
        assert_eq!(v["row"], 677);
        assert_eq!(v["action"], "advance");
        assert_eq!(v["mutating"], true);
        assert_eq!(v["next_stage"], "review");
        assert_eq!(v["authorized_by"][0], "approval");
        assert_eq!(v["head"], HEAD);
    }

    #[test]
    fn a_json_escalation_says_whether_it_wants_a_person() {
        let l = lane("code");
        for (why, wants) in [
            (
                thegn_core::pipeline_supervise::EscalationReason::AwaitingApproval {
                    stage: "code".into(),
                    commit_sha: HEAD.into(),
                },
                true,
            ),
            (
                thegn_core::pipeline_supervise::EscalationReason::HandoffIncomplete {
                    missing: Requirement::ParentReport,
                },
                false,
            ),
        ] {
            let v = action_json(&l, &SuperviseAction::Escalate { row: 677, why });
            assert_eq!(v["wants_primary"], wants);
            assert_eq!(v["mutating"], false);
        }
    }

    #[test]
    fn short_never_splits_a_multibyte_character() {
        assert_eq!(short(HEAD).len(), 9);
        assert_eq!(short("abc"), "abc");
        assert_eq!(short(""), "");
    }

    #[test]
    fn a_live_approval_state_round_trips_into_the_line() {
        let a = Approval {
            issue_id: "linear:THE-407".into(),
            stage: "code".into(),
            commit_sha: HEAD.into(),
            approver: "blake".into(),
            note: None,
            granted_at_ms: 0,
            expires_at_ms: None,
            revoked_at_ms: None,
        };
        assert!(state_for(Some(&a), HEAD, 1, 0).is_live());
    }
}
