//! The executor's terminal step (b155 5c): land a lane through the merge
//! queue when `thegn_core::stage_land::unattended_land` passes, otherwise
//! raise the land approval with the failed check as the reason. "Stop and
//! ask" is the outcome of every doubt.
//!
//! - `Enqueue` (the plan's own `land_requires` were met, e.g. the owner
//!   approved): a `propose` lane holds; otherwise the head is re-read, and
//!   only when it is still the planned commit, automations are not paused
//!   and the budget admits it is the lane enqueued (the `merge.add` code)
//!   and the drain started with a scrubbed environment.
//! - Terminal `Escalate{AwaitingApproval}`: only a lane whose whole chain is
//!   `merge` under one trusted, still-loaded rule is re-planned with
//!   `approval` removed from `land_requires` (every other requirement still
//!   applies). Any failed check, any other autonomy, and any re-plan that is
//!   not `Enqueue` raises the owner's land approval instead. No path writes an
//!   approval on anyone's behalf.
//! - The land rule's seventh check (`gate_isolated`, backed by
//!   [`merge_gate_isolated`]) passes only when `[merge_queue] gate_sandbox =
//!   "sealed"` and the sealed spec resolves for the lane's repo right now
//!   (THE-2200). With the host gate (`off`, the default) `merge` behaves like
//!   `review`: every land stops for the owner's approval with "the merge gate
//!   is not sandboxed"; a sealed gate that cannot start asks with its cause.
//!
//! Other escalations ([`on_escalate`]) ring the owner (and the Lead) once
//! per (row, head, reason), only for `on_blocked = "escalate"` stages or an
//! approval-gated advance.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use thegn_core::approval_request::{ApprovalRequest, MAX_REASON};
use thegn_core::automation_autonomy::{Autonomy, RuleOrigin};
use thegn_core::automation_budget::SpawnKind;
use thegn_core::config::Config;
use thegn_core::config_pipeline::OnBlocked;
use thegn_core::db::Db;
use thegn_core::pipeline_supervise::{EscalationReason, LaneFacts, SuperviseAction};
use thegn_core::stage_land::{RuleStanding, UnattendedFacts};
use thegn_svc::control::ControlApi;

use super::lead_caps::merge_drain::{self, DrainEnv};
use super::service::DaemonService;

/// The notification kind an escalation rings with.
const ATTENTION_KIND: &str = "agent_attention";

/// One lane at its terminal decision, as the executor gathered it.
#[allow(dead_code, reason = "built by the b155 lane 2 executor")]
pub(crate) struct LaneCtx {
    pub issue_id: String,
    pub row: i64,
    pub stage: String,
    pub worktree: String,
    pub head_sha: String,
    pub facts: LaneFacts,
    /// Dispatch row ids from the lane's root to `row`.
    pub chain: Vec<i64>,
}

/// What the terminal step did.
#[allow(dead_code, reason = "read by the b155 lane 2 executor")]
pub(crate) enum TerminalOutcome {
    Landed { drain_started: bool },
    AskedApproval { reason: String },
    Held { reason: String },
}

/// The pure terminal decision for an `AwaitingApproval` terminal.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Decision {
    /// Enqueue this commit (the unattended re-plan said `Enqueue`).
    Enqueue { commit_sha: String },
    /// Raise the owner's land approval with this reason.
    Ask(String),
}

/// `autonomy` is the lane's ([`thegn_core::stage_land::lane_autonomy`]);
/// `standing` is [`thegn_core::stage_land::unattended_land`] (only consulted
/// for a `merge` lane); `replanned` is the plan with `approval` removed, which
/// the caller computes only when `standing` passed.
pub(crate) fn terminal_decision(
    autonomy: Autonomy,
    standing: Result<(), String>,
    replanned: Option<&SuperviseAction>,
) -> Decision {
    if autonomy != Autonomy::Merge {
        return Decision::Ask(format!(
            "lane autonomy is {}: the owner approves the land",
            autonomy.as_str()
        ));
    }
    if let Err(reason) = standing {
        return Decision::Ask(reason);
    }
    match replanned {
        Some(SuperviseAction::Enqueue { commit_sha, .. }) => Decision::Enqueue {
            commit_sha: commit_sha.clone(),
        },
        Some(SuperviseAction::Escalate { why, .. }) => Decision::Ask(why.explain()),
        Some(SuperviseAction::Hold { why, .. }) => Decision::Ask((*why).to_string()),
        Some(other) => Decision::Ask(format!(
            "the unattended re-plan wants {}, not a land",
            other.token()
        )),
        None => Decision::Ask("the unattended re-plan produced nothing".into()),
    }
}

/// Why an `Enqueue` may not proceed now, or `None` when it may. Pure.
pub(crate) fn enqueue_hold(
    autonomy: Autonomy,
    head_matches: bool,
    paused: bool,
    budget: &Result<(), String>,
) -> Option<String> {
    if autonomy == Autonomy::Propose {
        return Some("lane autonomy is propose: nothing lands".into());
    }
    if !head_matches {
        return Some("the lane's head moved since it was planned; re-planning".into());
    }
    if paused {
        return Some("automations are paused".into());
    }
    budget.as_ref().err().cloned()
}

/// Handle a terminal `Enqueue` or `Escalate{AwaitingApproval}`.
#[allow(dead_code, reason = "called by the b155 lane 2 executor")]
pub(crate) async fn on_terminal(
    svc: &Arc<DaemonService>,
    cfg: &Arc<Config>,
    ctx: &LaneCtx,
    action: &SuperviseAction,
) -> anyhow::Result<TerminalOutcome> {
    let chain = read_chain(svc, &ctx.chain).await?;
    let autonomies: Vec<Option<String>> = chain.iter().map(|(a, _)| a.clone()).collect();
    let autonomy = thegn_core::stage_land::lane_autonomy(&autonomies);
    match action {
        SuperviseAction::Enqueue { commit_sha, .. } => {
            enqueue(svc, cfg, ctx, autonomy, commit_sha).await
        }
        SuperviseAction::Escalate {
            why: EscalationReason::AwaitingApproval { stage, commit_sha },
            ..
        } => {
            let decision = if autonomy == Autonomy::Merge {
                let facts = gather_unattended(svc, cfg, ctx, &chain, commit_sha).await?;
                let standing = thegn_core::stage_land::unattended_land(&facts);
                let replanned = standing.is_ok().then(|| {
                    let sup = thegn_core::stage_land::without_approval(&cfg.pipeline.supervisor);
                    thegn_core::pipeline_supervise::plan(
                        std::slice::from_ref(&ctx.facts),
                        &cfg.pipeline,
                        &sup,
                        thegn_core::util::now_ms(),
                    )
                    .into_iter()
                    .next()
                });
                terminal_decision(autonomy, standing, replanned.flatten().as_ref())
            } else {
                terminal_decision(autonomy, Ok(()), None)
            };
            match decision {
                Decision::Enqueue { commit_sha } => {
                    enqueue(svc, cfg, ctx, autonomy, &commit_sha).await
                }
                Decision::Ask(reason) => ask(svc, ctx, stage, commit_sha, &reason).await,
            }
        }
        other => Ok(TerminalOutcome::Held {
            reason: format!("{} is not a terminal land action", other.token()),
        }),
    }
}

/// Enqueue through the `merge.add` code and start the scrubbed drain.
async fn enqueue(
    svc: &Arc<DaemonService>,
    cfg: &Arc<Config>,
    ctx: &LaneCtx,
    autonomy: Autonomy,
    commit_sha: &str,
) -> anyhow::Result<TerminalOutcome> {
    if autonomy == Autonomy::Propose {
        // Checked before any I/O: a propose lane never lands.
        return held(enqueue_hold(autonomy, true, false, &Ok(())));
    }
    let head = read_head(&ctx.worktree).await;
    let head_matches = head.as_deref().is_some_and(|h| same_commit(h, commit_sha));
    if !head_matches {
        if head.is_none() {
            return held(Some("the lane's head could not be read".into()));
        }
        super::stage_executor::kick_row(ctx.row);
        return held(enqueue_hold(autonomy, false, false, &Ok(())));
    }
    let budget_cfg = Arc::clone(cfg);
    let (paused, budget) = svc
        .with_db(move |db| {
            Ok((
                thegn_core::automation_control::paused_fail_closed(db),
                crate::automation_budget::admit(db, &budget_cfg, SpawnKind::NextStage)
                    .map_err(|refusal| refusal.to_string()),
            ))
        })
        .await
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    if let Some(reason) = enqueue_hold(autonomy, true, paused, &budget) {
        return held(Some(reason));
    }

    if let Err(error) = svc.merge_add(&ctx.worktree).await {
        return Ok(TerminalOutcome::Held {
            reason: format!("enqueue failed: {error}"),
        });
    }
    // The queue is keyed by worktree/branch, not by the reviewed commit (it
    // has no column for it, and this batch adds no schema): re-verify the
    // head immediately before the drain starts, and pull the row back out if
    // it moved. The fold itself can still race a commit landing after this
    // check (a short window, reported in THE-2171).
    let recheck = read_head(&ctx.worktree).await;
    if !recheck
        .as_deref()
        .is_some_and(|h| same_commit(h, commit_sha))
    {
        let dq_cfg = Arc::clone(cfg);
        let dq_worktree = PathBuf::from(&ctx.worktree);
        let dequeued = tokio::task::spawn_blocking(move || -> anyhow::Result<()> {
            let db = Db::open()?;
            crate::merge_ops::dequeue_worktree(&dq_cfg, &db, &dq_worktree)
        })
        .await
        .map_err(|e| anyhow::anyhow!("dequeue task: {e}"))
        .and_then(|r| r);
        let reason = moved_after_enqueue_reason(recheck.as_deref(), dequeued.as_ref().err());
        if dequeued.is_err() {
            // The row could not be pulled back: the owner must look before
            // any drain folds it.
            return ask(svc, ctx, &ctx.stage, commit_sha, &reason).await;
        }
        super::stage_executor::kick_row(ctx.row);
        return held(Some(reason));
    }
    let worktree = PathBuf::from(&ctx.worktree);
    let repo = tokio::task::spawn_blocking(move || crate::merge_ops::repo_root_of(&worktree))
        .await
        .ok()
        .flatten();
    let drain_started = match repo {
        Some(repo) => {
            let drain_svc = Arc::clone(svc);
            let started = tokio::task::spawn_blocking(move || {
                merge_drain::start_for_repo(&drain_svc, &repo, DrainEnv::Scrubbed)
            })
            .await;
            match started {
                Ok(Ok(_pid)) => true,
                Ok(Err(error)) => {
                    // A drain already running (single-flight) folds the queue anyway.
                    tracing::info!(target: "thegn::pipeline", row = ctx.row, %error, "land enqueued; drain not started");
                    false
                }
                Err(error) => {
                    tracing::warn!(target: "thegn::pipeline", row = ctx.row, %error, "land drain task failed");
                    false
                }
            }
        }
        None => false,
    };
    Ok(TerminalOutcome::Landed { drain_started })
}

/// Why a land was pulled back after enqueue: the head moved (or became
/// unreadable) between review and the drain.
fn moved_after_enqueue_reason(head: Option<&str>, dequeue_error: Option<&anyhow::Error>) -> String {
    let what = match head {
        Some(_) => "the lane's head moved after it was enqueued",
        None => "the lane's head could not be re-read after it was enqueued",
    };
    match dequeue_error {
        None => format!("{what}; dequeued, not drained"),
        Some(error) => format!("{what}, and dequeue failed ({error:#}): the row is still queued"),
    }
}

fn held(reason: Option<String>) -> anyhow::Result<TerminalOutcome> {
    Ok(TerminalOutcome::Held {
        reason: reason.unwrap_or_else(|| "held".into()),
    })
}

/// Raise the owner's land approval for `(issue, stage, commit)`.
async fn ask(
    svc: &Arc<DaemonService>,
    ctx: &LaneCtx,
    stage: &str,
    commit_sha: &str,
    reason: &str,
) -> anyhow::Result<TerminalOutcome> {
    let reason = bound_reason(reason);
    super::lead_caps::approvals::raise(
        svc,
        ApprovalRequest {
            issue_id: ctx.issue_id.clone(),
            stage: stage.to_string(),
            commit_sha: commit_sha.to_string(),
            worktree: Some(ctx.worktree.clone()),
            reason: Some(reason.clone()),
        },
    )
    .await?;
    Ok(TerminalOutcome::AskedApproval { reason })
}

fn bound_reason(reason: &str) -> String {
    reason
        .chars()
        .filter(|c| !c.is_control())
        .take(MAX_REASON)
        .collect()
}

/// `(autonomy, automation_rule)` of every row in the chain, root first.
async fn read_chain(
    svc: &DaemonService,
    chain: &[i64],
) -> anyhow::Result<Vec<(Option<String>, Option<String>)>> {
    let ids = chain.to_vec();
    svc.with_db(move |db| {
        use thegn_core::store::NotificationStore;
        ids.iter().map(|id| db.dispatch_automation(*id)).collect()
    })
    .await
    .map_err(|e| anyhow::anyhow!("{e}"))
}

/// The lane's current head, re-read from git (off the loop). `None` when it
/// cannot be read.
async fn read_head(worktree: &str) -> Option<String> {
    let wt = PathBuf::from(worktree);
    tokio::task::spawn_blocking(move || {
        thegn_core::remote::GitLoc::Local(wt)
            .git_text(&["rev-parse", "HEAD"])
            .ok()
    })
    .await
    .ok()
    .flatten()
    .map(|h| h.trim().to_string())
    .filter(|h| !h.is_empty())
}

/// Exact commit match (case-insensitive), never on a too-short string.
fn same_commit(a: &str, b: &str) -> bool {
    let (a, b) = (a.trim(), b.trim());
    a.len() >= thegn_core::pipeline_approval::MIN_SHA_PREFIX
        && b.len() >= thegn_core::pipeline_approval::MIN_SHA_PREFIX
        && a.eq_ignore_ascii_case(b)
}

/// Everything [`thegn_core::stage_land::unattended_land`] decides on.
async fn gather_unattended(
    svc: &Arc<DaemonService>,
    cfg: &Arc<Config>,
    ctx: &LaneCtx,
    chain: &[(Option<String>, Option<String>)],
    commit_sha: &str,
) -> anyhow::Result<UnattendedFacts> {
    let head = read_head(&ctx.worktree).await;
    let head_matches = head.as_deref().is_some_and(|h| same_commit(h, commit_sha));
    let rule_id = chain.first().and_then(|(_, rule)| rule.clone());
    let ids = ctx.chain.clone();
    let head_sha = head.unwrap_or_default();
    let db_cfg = Arc::clone(cfg);
    let leaf = ctx.row;
    let (rule, validations_green, paused, budget_ok, passed) = svc
        .with_db(move |db| {
            let rule = rule_id
                .as_deref()
                .and_then(|id| rule_standing(&db_cfg, db, id));
            let green = chain_validations_green(&db_cfg, db, &ids, &head_sha);
            let paused = thegn_core::automation_control::paused_fail_closed(db);
            let budget_ok =
                crate::automation_budget::admit(db, &db_cfg, SpawnKind::NextStage).is_ok();
            let passed = {
                use thegn_core::store::NotificationStore;
                db.get_dispatch(leaf)
                    .ok()
                    .flatten()
                    .is_some_and(|r| review_passed(r.report.as_deref()))
            };
            Ok((rule, green, paused, budget_ok, passed))
        })
        .await
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    let entry = cfg
        .pipeline
        .entry()
        .and_then(|s| s.stage_name())
        .map(str::to_string);
    let mq = &cfg.merge_queue;
    let worktree = PathBuf::from(&ctx.worktree);
    let repo = tokio::task::spawn_blocking(move || crate::merge_ops::repo_root_of(&worktree))
        .await
        .ok()
        .flatten();
    let isolation = match repo {
        Some(repo) => merge_gate_isolated(cfg, &repo).await,
        None => Err(thegn_core::stage_land::gate_cannot_start(
            "the lane's repository could not be resolved",
        )),
    };
    Ok(UnattendedFacts {
        chain_autonomy: chain.iter().map(|(a, _)| a.clone()).collect(),
        chain_rule: chain.iter().map(|(_, r)| r.clone()).collect(),
        rule,
        terminal_is_first_stage: first_stage_terminal(entry.as_deref(), &ctx.stage, chain.len()),
        terminal_done: ctx.facts.status == thegn_core::issue::AgentDispatchStatus::Done,
        report_ok: ctx.facts.report_present && passed,
        validations_green,
        head_matches,
        gate_on: mq.gate_on,
        gate_command_set: !mq.gate_command.trim().is_empty(),
        gate_timeout_secs: mq.gate_timeout_secs,
        paused,
        budget_ok,
        gate_isolated: isolation.is_ok(),
        gate_isolation_refusal: isolation.err(),
    })
}

/// Whether the merge queue's gate for `repo_root` runs inside a sandbox: `Ok`
/// only for `[merge_queue] gate_sandbox = "sealed"` whose sealed spec resolves
/// for the repo now (the same resolve the drain's gate makes). Otherwise the
/// gate would run worker-written code (justfile, build.rs, tests) on the host
/// with no human involved, so the land asks the owner with this reason.
async fn merge_gate_isolated(cfg: &Arc<Config>, repo_root: &Path) -> Result<(), String> {
    // Off refuses without a sandbox ensure; the overlay read and a sealed
    // resolve are blocking, so both run here.
    let full = Arc::clone(cfg);
    let root = repo_root.to_path_buf();
    let resolved = tokio::task::spawn_blocking(move || crate::gate_sandbox::resolve(&full, &root))
        .await
        .unwrap_or_else(|error| Err(format!("the sandbox resolve failed ({error})")));
    isolation_from(resolved)
}

/// The seventh check from a gate resolve. Pure.
fn isolation_from(resolved: Result<crate::gate_sandbox::GateExec, String>) -> Result<(), String> {
    match resolved {
        Ok(crate::gate_sandbox::GateExec::Sealed(_)) => Ok(()),
        Ok(crate::gate_sandbox::GateExec::Host) => {
            Err(thegn_core::stage_land::GATE_NOT_ISOLATED.to_owned())
        }
        Err(why) => Err(thegn_core::stage_land::gate_cannot_start(&why)),
    }
}

/// The terminal stage's report states PASS and carries gate evidence. A
/// FAIL/REVISE/BLOCKED report (which `report_gate_valid` lets through) never
/// satisfies the unattended report check.
pub(crate) fn review_passed(report: Option<&str>) -> bool {
    report.is_some_and(|r| {
        thegn_core::pipeline_report::verdict_of(Some(r))
            == thegn_core::pipeline_report::Verdict::Pass
            && thegn_core::pipeline_report::gate_evidence(r).is_some()
    })
}

/// "No reviewed stage passed": the terminal is the chart's entry stage, or
/// the lane has no stage before its terminal (a single-row chain).
fn first_stage_terminal(entry: Option<&str>, stage: &str, chain_len: usize) -> bool {
    chain_len < 2 || entry.is_none_or(|e| e == stage.trim())
}

/// The standing of `rule_id` among the currently compiled rules (config via
/// `effective_automations`, Lead via `with_lead_rules`; read only). `None`
/// when it is not loaded (or the config does not compile: fail closed).
fn rule_standing(cfg: &Config, db: &Db, rule_id: &str) -> Option<RuleStanding> {
    use thegn_core::store::AutomationStore;
    let effective = cfg.effective_automations();
    if !effective.enabled {
        return None;
    }
    let config_rules = effective.compiled_rules().ok()?;
    let from_config = config_rules.iter().any(|r| r.id == rule_id);
    let rules = crate::automation_runtime::with_lead_rules(db, config_rules);
    let rule = rules.into_iter().find(|r| r.id == rule_id)?;
    let enabled = db
        .automation_state(rule_id)
        .ok()
        .flatten()
        .and_then(|s| s.enabled_override)
        .unwrap_or(rule.enabled);
    let origin = if from_config
        && !rule_id.starts_with(thegn_core::automation_authoring::LEAD_RULE_PREFIX)
    {
        RuleOrigin::Config
    } else {
        use thegn_core::store::AutomationRuleStore;
        let owner_armed = db
            .lead_rules()
            .ok()
            .and_then(|rows| rows.into_iter().find(|r| r.id == rule_id))
            .is_some_and(|row| {
                let (_, _, sha) = thegn_core::approval_digest::rule_arm_ids(
                    rule_id,
                    &row.body_json,
                    Autonomy::Merge,
                );
                crate::automation_arming::armed_set(db, thegn_core::util::now_ms())
                    .contains(&(rule_id.to_string(), sha))
            });
        if owner_armed {
            RuleOrigin::LeadApproved
        } else {
            RuleOrigin::Lead
        }
    };
    Some(RuleStanding {
        origin,
        enabled,
        effective: rule.autonomy,
        free_text: thegn_core::automation_autonomy::uses_free_text(&rule.predicate),
    })
}

/// Every stage in the chain has each of its `validate` tasks recorded green
/// at `head` (on any row of the chain). An unreadable row or a stage no
/// longer in the chart is not green.
fn chain_validations_green(cfg: &Config, db: &Db, chain: &[i64], head: &str) -> bool {
    use thegn_core::store::NotificationStore;
    if head.trim().is_empty() || chain.is_empty() {
        return false;
    }
    let mut stages = Vec::with_capacity(chain.len());
    let mut records = Vec::new();
    for id in chain {
        let Ok(Some(row)) = db.get_dispatch(*id) else {
            return false;
        };
        let Some(stage) = row.stage else {
            return false;
        };
        stages.push(stage);
        let Ok(mut recs) = db.validations_for_dispatch(*id) else {
            return false;
        };
        records.append(&mut recs);
    }
    stages.iter().all(|name| {
        let Some(stage) = cfg.pipeline.stage(name) else {
            return false;
        };
        stage.validate_tasks().iter().all(|task| {
            records
                .iter()
                .any(|r| r.task == *task && r.class.is_green() && same_commit(&r.commit_sha, head))
        })
    })
}

/// Whether an escalation acts: the stage escalates on block, or it is an
/// approval-gated advance (`AwaitingApproval` on a non-terminal stage).
/// `Park`/`Abandon` stages are advisory and do nothing here.
pub(crate) fn escalation_acts(
    on_blocked: OnBlocked,
    terminal: bool,
    why: &EscalationReason,
) -> bool {
    on_blocked == OnBlocked::Escalate
        || (!terminal && matches!(why, EscalationReason::AwaitingApproval { .. }))
}

/// The once-per-(row, head, reason) dedupe key of an escalation.
pub(crate) fn escalation_key(row: i64, head_sha: &str, token: &str) -> String {
    let head7: String = head_sha.trim().chars().take(7).collect();
    format!("escalation:{row}@{head7}:{token}")
}

/// Handle any other `Escalate`.
#[allow(dead_code, reason = "called by the b155 lane 2 executor")]
pub(crate) async fn on_escalate(
    svc: &Arc<DaemonService>,
    cfg: &Arc<Config>,
    ctx: &LaneCtx,
    why: &EscalationReason,
) -> anyhow::Result<()> {
    let stage_cfg = cfg.pipeline.stage(&ctx.stage);
    let on_blocked = stage_cfg.map(|s| s.on_blocked).unwrap_or_default();
    let terminal = stage_cfg.is_none_or(|s| s.next_name().is_none());
    if !escalation_acts(on_blocked, terminal, why) {
        return Ok(());
    }
    let token = why.token();
    let key = escalation_key(ctx.row, &ctx.head_sha, token);
    let message = bound_reason(&format!(
        "Pipeline lane needs you: {} stage {} ({token})",
        ctx.issue_id, ctx.stage
    ));
    let worktree = ctx.worktree.clone();
    let fresh = svc
        .with_db(move |db| {
            use thegn_core::store::NotificationStore;
            db.put_notification_once_id(ATTENTION_KIND, &key, &message, &worktree)
        })
        .await
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    let Some(id) = fresh else {
        // Already rung for this row, head and reason.
        return Ok(());
    };
    super::lead_caps::feed::notification(&svc.events, id);

    let (stage, commit_sha) = match why {
        EscalationReason::AwaitingApproval { stage, commit_sha } => {
            (stage.clone(), commit_sha.clone())
        }
        _ => (ctx.stage.clone(), ctx.head_sha.clone()),
    };
    let raised = super::lead_caps::approvals::raise(
        svc,
        ApprovalRequest {
            issue_id: ctx.issue_id.clone(),
            stage: stage.clone(),
            commit_sha,
            worktree: Some(ctx.worktree.clone()),
            reason: Some(bound_reason(&why.explain())),
        },
    )
    .await;
    if let Err(error) = raised {
        tracing::warn!(target: "thegn::pipeline", row = ctx.row, %error, "escalation approval not raised");
    }

    if cfg.lead.is_some() {
        let (cfg, row, issue) = (Arc::clone(cfg), ctx.row, ctx.issue_id.clone());
        tokio::spawn(async move {
            if let Err(error) =
                crate::lead::wake::wake_escalation(cfg, row, &issue, &stage, token).await
            {
                tracing::info!(target: "thegn::pipeline", row, %error, "the Lead was not woken for an escalation");
            }
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const SHA: &str = "4babb0901f2c3d4e5f60718293a4b5c6d7e8f901";

    fn enqueue_action() -> SuperviseAction {
        SuperviseAction::Enqueue {
            row: 1,
            commit_sha: SHA.into(),
            authorized_by: Vec::<thegn_core::config_pipeline::Requirement>::new().into(),
        }
    }

    #[test]
    fn only_a_merge_lane_with_standing_and_an_enqueue_replan_lands() {
        let enq = enqueue_action();
        assert_eq!(
            terminal_decision(Autonomy::Merge, Ok(()), Some(&enq)),
            Decision::Enqueue {
                commit_sha: SHA.into()
            }
        );
        // Review and propose lanes always ask, whatever else holds.
        for autonomy in [Autonomy::Review, Autonomy::Propose] {
            assert!(matches!(
                terminal_decision(autonomy, Ok(()), Some(&enq)),
                Decision::Ask(r) if r.contains(autonomy.as_str())
            ));
        }
        // A failed check asks with that check's words.
        assert_eq!(
            terminal_decision(Autonomy::Merge, Err("gate off".into()), Some(&enq)),
            Decision::Ask("gate off".into())
        );
        // A re-plan that is not Enqueue asks with its explanation.
        let esc = SuperviseAction::Escalate {
            row: 1,
            why: EscalationReason::StageMissing { name: "x".into() },
        };
        assert!(matches!(
            terminal_decision(Autonomy::Merge, Ok(()), Some(&esc)),
            Decision::Ask(r) if r.contains("not a configured stage")
        ));
        let hold = SuperviseAction::Hold {
            row: 1,
            why: "busy",
        };
        assert_eq!(
            terminal_decision(Autonomy::Merge, Ok(()), Some(&hold)),
            Decision::Ask("busy".into())
        );
        let validate = SuperviseAction::Validate {
            row: 1,
            tasks: vec![],
            commit_sha: SHA.into(),
        };
        assert!(matches!(
            terminal_decision(Autonomy::Merge, Ok(()), Some(&validate)),
            Decision::Ask(r) if r.contains("validate")
        ));
        assert!(matches!(
            terminal_decision(Autonomy::Merge, Ok(()), None),
            Decision::Ask(_)
        ));
    }

    #[test]
    fn enqueue_holds_on_propose_head_move_pause_and_budget() {
        let ok = Ok(());
        assert_eq!(enqueue_hold(Autonomy::Review, true, false, &ok), None);
        assert_eq!(enqueue_hold(Autonomy::Merge, true, false, &ok), None);
        assert!(
            enqueue_hold(Autonomy::Propose, true, false, &ok)
                .unwrap()
                .contains("propose")
        );
        assert!(
            enqueue_hold(Autonomy::Review, false, false, &ok)
                .unwrap()
                .contains("head moved")
        );
        assert!(
            enqueue_hold(Autonomy::Merge, true, true, &ok)
                .unwrap()
                .contains("paused")
        );
        assert_eq!(
            enqueue_hold(Autonomy::Merge, true, false, &Err("budget".into())),
            Some("budget".into())
        );
    }

    #[test]
    fn escalation_acts_only_for_escalate_stages_or_gated_advances() {
        let awaiting = EscalationReason::AwaitingApproval {
            stage: "plan".into(),
            commit_sha: SHA.into(),
        };
        let failed = EscalationReason::StageMissing { name: "x".into() };
        assert!(escalation_acts(OnBlocked::Escalate, true, &failed));
        assert!(escalation_acts(OnBlocked::Escalate, false, &failed));
        assert!(escalation_acts(OnBlocked::Park, false, &awaiting));
        assert!(!escalation_acts(OnBlocked::Park, true, &awaiting));
        assert!(!escalation_acts(OnBlocked::Park, false, &failed));
        assert!(!escalation_acts(OnBlocked::Abandon, false, &failed));
    }

    #[test]
    fn escalation_key_is_row_head7_and_token() {
        assert_eq!(
            escalation_key(42, SHA, "validation-failed"),
            "escalation:42@4babb09:validation-failed"
        );
        assert_ne!(
            escalation_key(42, SHA, "awaiting-approval"),
            escalation_key(42, "ffffffff", "awaiting-approval"),
            "a new head re-arms"
        );
    }

    #[test]
    fn escalation_dedupes_on_its_key() {
        use thegn_core::store::NotificationStore;
        let db = Db::open_memory().unwrap();
        let key = escalation_key(7, SHA, "awaiting-approval");
        let msg = "Pipeline lane needs you: THE-1 stage plan (awaiting-approval)";
        assert!(
            db.put_notification_once_id(ATTENTION_KIND, &key, msg, "/wt")
                .unwrap()
                .is_some()
        );
        assert!(
            db.put_notification_once_id(ATTENTION_KIND, &key, msg, "/wt")
                .unwrap()
                .is_none()
        );
        // Never mistaken for an approval request row.
        assert!(thegn_core::approval_request::parse_source_ref(&key).is_none());
    }

    #[test]
    fn a_single_row_or_entry_stage_terminal_is_unreviewed() {
        assert!(first_stage_terminal(Some("plan"), "plan", 3));
        assert!(first_stage_terminal(Some("plan"), "review", 1));
        assert!(first_stage_terminal(None, "review", 2));
        assert!(!first_stage_terminal(Some("plan"), "review", 2));
    }

    #[test]
    fn moved_after_enqueue_reason_names_the_dequeue_outcome() {
        let moved = moved_after_enqueue_reason(Some("abc"), None);
        assert!(
            moved.contains("moved") && moved.contains("dequeued"),
            "{moved}"
        );
        let unread = moved_after_enqueue_reason(None, None);
        assert!(unread.contains("could not be re-read"), "{unread}");
        let error = anyhow::anyhow!("locked");
        let stuck = moved_after_enqueue_reason(Some("abc"), Some(&error));
        assert!(
            stuck.contains("still queued") && stuck.contains("locked"),
            "{stuck}"
        );
    }

    #[test]
    fn a_merge_lane_asks_while_the_merge_gate_is_not_sandboxed() {
        use crate::gate_sandbox::GateExec;
        let reason = isolation_from(Ok(GateExec::Host)).unwrap_err();
        assert_eq!(reason, thegn_core::stage_land::GATE_NOT_ISOLATED);
        assert!(reason.contains("gate_sandbox = \"sealed\""));
        assert_eq!(
            terminal_decision(Autonomy::Merge, Err(reason.clone()), None),
            Decision::Ask(reason)
        );
        // A sealed gate that cannot start asks with its cause.
        let cannot = isolation_from(Err("no sandbox runtime".into())).unwrap_err();
        assert_eq!(
            cannot,
            "the sealed merge gate cannot start: no sandbox runtime"
        );
        assert_eq!(
            terminal_decision(Autonomy::Merge, Err(cannot.clone()), None),
            Decision::Ask(cannot)
        );
    }

    #[test]
    fn a_merge_lane_with_a_sealed_gate_lands_unattended() {
        use crate::gate_sandbox::GateExec;
        let sealed = isolation_from(Ok(GateExec::Sealed(Box::default())));
        assert_eq!(sealed, Ok(()));
        let enq = enqueue_action();
        assert_eq!(
            terminal_decision(Autonomy::Merge, sealed, Some(&enq)),
            Decision::Enqueue {
                commit_sha: SHA.into()
            }
        );
    }

    #[tokio::test]
    async fn an_off_gate_is_not_isolated_without_resolving() {
        let cfg = Arc::new(Config::default());
        assert_eq!(
            merge_gate_isolated(&cfg, Path::new("/nonexistent")).await,
            Err(thegn_core::stage_land::GATE_NOT_ISOLATED.to_owned())
        );
    }

    #[test]
    fn same_commit_is_exact_and_never_short() {
        assert!(same_commit(SHA, &SHA.to_uppercase()));
        assert!(!same_commit(SHA, &SHA[..7]));
        assert!(!same_commit("", ""));
        assert!(!same_commit("abc", "abc"));
    }

    #[test]
    fn chain_validations_need_every_stage_green_at_head() {
        use thegn_core::store::NotificationStore;
        let db = Db::open_memory().unwrap();
        let mut cfg = Config::default();
        cfg.pipeline.stages = vec![
            thegn_core::config_pipeline::PipelineStage {
                name: "code".into(),
                agent: "claude".into(),
                next: Some("review".into()),
                validate: vec!["test".into()],
                ..Default::default()
            },
            thegn_core::config_pipeline::PipelineStage {
                name: "review".into(),
                agent: "claude".into(),
                ..Default::default()
            },
        ];
        let put = |stage: &str, parent: Option<i64>| {
            db.put_agent_dispatch(thegn_core::issue::NewDispatch {
                issue_id: "linear:THE-1",
                worktree_path: "/wt",
                agent_name: "claude",
                stage: Some(stage),
                parent_id: parent,
                session_id: None,
                artifact_path: None,
                chunk_path: None,
            })
            .unwrap()
        };
        let root = put("code", None);
        let term = put("review", Some(root));
        // `code` declares `test` and nothing recorded it: not green.
        assert!(!chain_validations_green(&cfg, &db, &[root, term], SHA));
        assert!(!chain_validations_green(&cfg, &db, &[], SHA));
        assert!(!chain_validations_green(&cfg, &db, &[root, term], ""));
        // A chain whose stages declare nothing is vacuously green.
        cfg.pipeline.stages[0].validate.clear();
        assert!(chain_validations_green(&cfg, &db, &[root, term], SHA));
        // A stage no longer in the chart is not green.
        cfg.pipeline.stages.remove(1);
        assert!(!chain_validations_green(&cfg, &db, &[root, term], SHA));
    }

    #[test]
    fn review_passed_requires_pass_with_gate_evidence() {
        assert!(!review_passed(None));
        assert!(!review_passed(Some(
            "FAIL\ngate: cargo nextest run --workspace -- 1 passed, 0 skipped"
        )));
        assert!(!review_passed(Some("PASS")));
        assert!(review_passed(Some(
            "PASS\ngate: cargo nextest run --workspace -- 1 passed, 0 skipped"
        )));
    }

    #[test]
    fn rule_standing_is_none_when_automations_are_off_or_the_rule_is_gone() {
        let db = Db::open_memory().unwrap();
        let cfg = Config::default();
        assert_eq!(rule_standing(&cfg, &db, "anything"), None);
    }
}
