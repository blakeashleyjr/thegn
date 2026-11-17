//! `pipeline.start`: an `issue_matched` rule starting the stage pipeline for
//! the matched issue (THE-2193 / THE-2195, b153 lane 5).
//!
//! A rule-only action (`automation::RULE_ONLY_ACTIONS`). The effective
//! autonomy rides on the planned action: `propose` raises an approval and
//! starts nothing; `review`/`merge` start the first stage and record the
//! ceiling on the dispatch row (`set_dispatch_autonomy`), which the stage
//! executor enforces at land.
//!
//! Trust boundary: the event's issue facts are tracker data. Only the issue
//! id is used, and only after re-validation; it never chooses the repo (that
//! must be a registered workspace root), the path (the worktree is a
//! registered one linked to the issue, or a fresh `wt new --from-issue` whose
//! branch comes from the sanitized `issue_branch_seed`), the agent (the
//! stage's configured agent), or the stage (the rule's `stage` param or the
//! first configured stage). The launch is the normal daemon `sessions.open`
//! agent path with no event-derived argv or env, behind the same fail-closed
//! sandbox floor queue agents use, further restricted to a sealed sandbox
//! that hides `$HOME` (autopilot's rule).
//!
//! A `pipeline.start` rule that creates a worktree runs your configured and
//! trusted-repo create hooks (`PreCreate`/`PostCreate`), as a manual
//! `thegn wt new` does.

use std::path::Path;
use std::sync::Arc;

use anyhow::{Context, Result};
use thegn_core::automation::{AutomationEvent, PlannedAction};
use thegn_core::automation_autonomy::Autonomy;
use thegn_core::config::Config;

/// Longest issue id `pipeline.start` accepts (the approval request's own
/// bound, so a proposal can never be refused for length downstream).
const MAX_ISSUE_ID: usize = thegn_core::approval_request::MAX_ISSUE_ID;

/// What `start` does for one event, decided before any I/O with effects.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum StartPlan {
    /// Raise an approval request; no worktree, no spawn.
    Propose,
    /// Create or reuse the issue's worktree and dispatch the stage.
    Start,
    /// Do nothing; the string becomes the run's error.
    Refuse(String),
}

/// The pure decision: an issue is required, an issue autopilot owns wins,
/// and `propose` never starts anything.
pub(crate) fn plan_start(autonomy: Autonomy, has_issue: bool, autopilot_live: bool) -> StartPlan {
    if !has_issue {
        return StartPlan::Refuse(
            "pipeline.start requires an issue_matched event; this event carries no issue".into(),
        );
    }
    if autopilot_live {
        return StartPlan::Refuse(
            "issue is owned by [autopilot]: a non-terminal autopilot run exists for it; \
             pipeline.start will not start a second worker"
                .into(),
        );
    }
    match autonomy {
        Autonomy::Propose => StartPlan::Propose,
        Autonomy::Review | Autonomy::Merge => StartPlan::Start,
    }
}

/// The stage to start: the rule's `stage` param when given (it must name a
/// configured stage), otherwise the first `[[pipeline.stages]]` entry.
pub(crate) fn stage_for(cfg: &Config, param: Option<&str>) -> Result<String> {
    let names = cfg.pipeline.stage_names();
    if names.is_empty() {
        anyhow::bail!("pipeline.start needs a pipeline: no [[pipeline.stages]] are configured");
    }
    match param.map(str::trim).filter(|p| !p.is_empty()) {
        Some(name) => cfg
            .pipeline
            .stage(name)
            .and_then(|s| s.stage_name().map(str::to_string))
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "pipeline.start stage '{}' is not a configured [[pipeline.stages]] entry \
                     (configured: {})",
                    thegn_core::pipeline_report::display_cell(name),
                    names.join(", ")
                )
            }),
        None => Ok(names[0].clone()),
    }
}

/// Re-validate the tracker-supplied issue id before it reaches a branch seed,
/// a roster row or an approval. Bounded, a conservative charset, no path
/// traversal; the error never echoes the (untrusted) value.
pub(crate) fn validate_issue_id(raw: &str) -> Result<String> {
    if raw.is_empty() {
        anyhow::bail!("pipeline.start refused: the event's issue id is empty");
    }
    if raw.len() > MAX_ISSUE_ID {
        anyhow::bail!(
            "pipeline.start refused: the event's issue id is {} bytes (limit {MAX_ISSUE_ID})",
            raw.len()
        );
    }
    let charset_ok = raw
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || matches!(b, b':' | b'-' | b'_' | b'/' | b'#' | b'.'));
    if !charset_ok
        || raw.starts_with(['-', '/', '.', ':'])
        || raw.ends_with(['/', '.'])
        || raw.contains("..")
        || raw.contains("//")
    {
        anyhow::bail!("pipeline.start refused: the event's issue id has invalid characters");
    }
    thegn_core::pipeline_identity::canonical_issue_id(raw)
        .map_err(|_| anyhow::anyhow!("pipeline.start refused: the event's issue id is invalid"))
}

pub(crate) async fn start(
    cfg: Arc<Config>,
    event: &AutomationEvent,
    action: &PlannedAction,
    origin: thegn_svc::control::AutomationOrigin,
) -> Result<()> {
    let facts = event
        .issue
        .as_ref()
        .context("pipeline.start requires an issue_matched event (no event.issue)")?;
    let repo = event
        .repo
        .as_deref()
        .filter(|r| !r.trim().is_empty())
        .context("pipeline.start requires event.repo (the repo the issue matched in)")?
        .to_string();
    let issue_id = validate_issue_id(&facts.issue_id)?;
    let stage = stage_for(&cfg, action.params.get("stage").map(String::as_str))?;
    let key = thegn_core::autopilot::AutopilotIssueKey::new(
        facts.provider.clone(),
        facts.account.clone(),
        issue_id.clone(),
    );

    let lookup_issue = issue_id.clone();
    let (repo_root, autopilot_live, linked) =
        tokio::task::spawn_blocking(move || -> Result<(String, bool, Option<String>)> {
            let db = thegn_core::db::Db::open()?;
            let repo_root = registered_repo_root(&db, &repo)?;
            let live = autopilot_owns(&db, &key)?;
            let linked = linked_worktree(&db, &repo_root, &lookup_issue)?;
            Ok((repo_root, live, linked))
        })
        .await??;

    match plan_start(action.autonomy, true, autopilot_live) {
        StartPlan::Refuse(reason) => anyhow::bail!("{reason}"),
        StartPlan::Propose => propose(cfg, action, &repo_root, &issue_id, &stage).await,
        StartPlan::Start => {
            start_stage(
                cfg,
                action,
                origin,
                StartTarget {
                    repo_root,
                    issue_id,
                    stage,
                    linked,
                },
            )
            .await
        }
    }
}

/// The validated inputs of a `review`/`merge` start.
struct StartTarget {
    repo_root: String,
    issue_id: String,
    stage: String,
    linked: Option<String>,
}

/// The registered workspace root `repo` names. The returned path is the DB's,
/// never the event's string; a non-git (`dir`) workspace is refused.
fn registered_repo_root(db: &thegn_core::db::Db, repo: &str) -> Result<String> {
    use thegn_core::store::WorkspaceStore;
    let canonical = match std::fs::canonicalize(repo) {
        Ok(path) => Some(path),
        Err(_) => None,
    };
    db.workspaces()?
        .into_iter()
        .filter(|w| w.kind == "repo")
        .find(|w| {
            w.repo_path == repo
                || canonical.is_some() && std::fs::canonicalize(&w.repo_path).ok() == canonical
        })
        .map(|w| w.repo_path)
        .ok_or_else(|| {
            anyhow::anyhow!("pipeline.start refused: event.repo is not a registered repo workspace")
        })
}

/// Whether a non-terminal autopilot run holds this issue in any registered
/// repo (the autopilot claim is global: `UNIQUE(provider, account, issue_id)`).
fn autopilot_owns(
    db: &thegn_core::db::Db,
    key: &thegn_core::autopilot::AutopilotIssueKey,
) -> Result<bool> {
    use thegn_core::store::{AutopilotStore, WorkspaceStore};
    for ws in db.workspaces()?.into_iter().filter(|w| w.kind == "repo") {
        if db
            .list_autopilot_runs(&ws.repo_path, 1_000)?
            .iter()
            .any(|run| autopilot_run_matches(run, key))
        {
            return Ok(true);
        }
    }
    Ok(false)
}

fn autopilot_run_matches(
    run: &thegn_core::autopilot::AutopilotSummary,
    key: &thegn_core::autopilot::AutopilotIssueKey,
) -> bool {
    !run.state.is_terminal()
        && run.key.provider.eq_ignore_ascii_case(&key.provider)
        && run.key.account == key.account
        && thegn_core::pipeline_identity::same_issue(&run.key.issue_id, &key.issue_id)
}

/// A registered worktree of `repo_root` already linked to the issue (newest
/// link first) that still exists on disk.
fn linked_worktree(
    db: &thegn_core::db::Db,
    repo_root: &str,
    issue_id: &str,
) -> Result<Option<String>> {
    use thegn_core::store::{WorkspaceStore, WorktreeAuxStore};
    for (linked_issue, worktree) in db.issue_link_worktrees()? {
        if !thegn_core::pipeline_identity::same_issue(&linked_issue, issue_id) {
            continue;
        }
        if db.worktree_record(&worktree)?.is_none() {
            continue;
        }
        if db.repo_root_for(&worktree)?.as_deref() != Some(repo_root) {
            continue;
        }
        if Path::new(&worktree).is_dir() {
            return Ok(Some(worktree));
        }
    }
    Ok(None)
}

/// `propose`: resolve the base branch HEAD and raise an approval request.
/// Creates no worktree and spawns nothing.
async fn propose(
    cfg: Arc<Config>,
    action: &PlannedAction,
    repo_root: &str,
    issue_id: &str,
    stage: &str,
) -> Result<()> {
    let root = repo_root.to_string();
    let commit_sha =
        tokio::task::spawn_blocking(move || base_head_sha(&cfg, Path::new(&root))).await??;
    let body = proposal_body(&action.rule_id, issue_id, stage, &commit_sha)?;
    crate::automation_executor::client()
        .await?
        .call_raw("POST", "/v1/approvals/request", Some(body))
        .await?;
    Ok(())
}

/// The `approvals.request` body for a proposal, validated with the daemon's
/// own rules first so a refusal names this action.
fn proposal_body(
    rule_id: &str,
    issue_id: &str,
    stage: &str,
    commit_sha: &str,
) -> Result<serde_json::Value> {
    use thegn_core::approval_request::{ApprovalRequest, MAX_REASON, validate_request};
    let reason: String = format!("automation {rule_id} proposes starting {stage}")
        .chars()
        .filter(|c| !c.is_control())
        .take(MAX_REASON)
        .collect();
    let request = ApprovalRequest {
        issue_id: issue_id.to_string(),
        stage: stage.to_string(),
        commit_sha: commit_sha.to_string(),
        worktree: None,
        reason: Some(reason.clone()),
    };
    validate_request(&request)
        .map_err(|e| anyhow::anyhow!("pipeline.start proposal is invalid: {e}"))?;
    Ok(serde_json::json!({
        "issue_id": request.issue_id,
        "stage": request.stage,
        "commit_sha": request.commit_sha,
        "reason": reason,
    }))
}

/// The commit the configured base branch points at in `root`.
#[expect(
    clippy::disallowed_methods,
    reason = "called only inside spawn_blocking (propose)"
)]
fn base_head_sha(cfg: &Config, root: &Path) -> Result<String> {
    let base = thegn_core::worktree::resolve_base_checked(root, cfg)
        .map_err(|e| anyhow::anyhow!("pipeline.start: cannot resolve the base branch: {e}"))?;
    let out = thegn_core::util::git_cmd(root)
        .args(["rev-parse", "--verify", "--quiet", "--end-of-options"])
        .arg(format!("{base}^{{commit}}"))
        .output()
        .context("pipeline.start: git rev-parse")?;
    let sha = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if !out.status.success() || !is_hex_sha(&sha) {
        anyhow::bail!(
            "pipeline.start: base branch '{}' does not resolve to a commit",
            thegn_core::pipeline_report::display_cell(&base)
        );
    }
    Ok(sha)
}

fn is_hex_sha(s: &str) -> bool {
    matches!(s.len(), 40 | 64) && s.bytes().all(|b| b.is_ascii_hexdigit())
}

/// `review`/`merge`: the issue's worktree, the sandbox floor, the dispatch,
/// then the recorded autonomy ceiling.
async fn start_stage(
    cfg: Arc<Config>,
    action: &PlannedAction,
    origin: thegn_svc::control::AutomationOrigin,
    target: StartTarget,
) -> Result<()> {
    let StartTarget {
        repo_root,
        issue_id,
        stage,
        linked,
    } = target;
    if let Some(reason) = crate::agent_run::headless_unsupported() {
        anyhow::bail!("pipeline.start held: {reason}");
    }

    let worktree = match linked {
        Some(worktree) => worktree,
        None => {
            let cfg = Arc::clone(&cfg);
            let issue = issue_id.clone();
            let root = repo_root.clone();
            tokio::task::spawn_blocking(move || -> Result<String> {
                let config_path = Config::path();
                let config_dir = config_path.parent().unwrap_or(Path::new(""));
                let (created, filing_error) = crate::cmd::wt::new_core(
                    &cfg,
                    crate::cmd::wt::NewOptions {
                        name: None,
                        repo: Some(root),
                        base: None,
                        env: None,
                        profile: None,
                        from_issue: Some(issue),
                        exact_branch: false,
                        folder: None,
                    },
                    config_dir,
                )?;
                if let Some(error) = filing_error {
                    tracing::warn!(
                        target: "thegn::automation",
                        error = %format!("{error:#}"),
                        "pipeline.start: worktree created but not filed"
                    );
                }
                Ok(created.path)
            })
            .await??
        }
    };

    // Sandbox floor, fail closed: the worker must run inside a sandbox that
    // meets at least shared-kernel isolation (the `[sandbox] isolation_floor`
    // can raise it, never lower it), exactly as unattended queue agents do.
    let gate_cfg = Arc::clone(&cfg);
    let gate_wt = worktree.clone();
    let gate = tokio::task::spawn_blocking(move || {
        crate::agent_run::agent_floor_gate(
            &gate_cfg,
            &gate_wt,
            true,
            thegn_core::config::IsolationFloor::SharedKernel,
            thegn_core::config::OnFloorMiss::Fail,
        )
    })
    .await?;
    if let Some(reason) = floor_refusal(gate) {
        anyhow::bail!("{reason}");
    }

    let client = crate::automation_executor::client().await?;
    let dispatch_cfg = Arc::clone(&cfg);
    let (d_stage, d_issue, d_wt) = (stage, issue_id, worktree);
    let handle = tokio::runtime::Handle::current();
    // `open_stage_dispatch` holds a `Db` borrow across awaits (`!Send`): drive
    // it on a blocking thread so the executor future stays `Send`.
    let opened = tokio::task::spawn_blocking(move || {
        let mut request = crate::cmd::session::StageOpenRequest::fresh(&d_stage, &d_issue, &d_wt);
        request.automation_origin = Some(origin);
        handle.block_on(crate::cmd::session::open_stage_dispatch(
            &dispatch_cfg,
            &client,
            request,
        ))
    })
    .await??;

    let row_id = opened.row_id;
    let autonomy = action.autonomy.as_str();
    let recorded = tokio::task::spawn_blocking(move || -> Result<bool> {
        use thegn_core::store::NotificationStore;
        thegn_core::db::Db::open()?.set_dispatch_autonomy(row_id, autonomy)
    })
    .await;
    // The dispatch row and its session exist; a failed ceiling write is
    // logged, never turned into a failed run.
    match recorded {
        Ok(Ok(true)) => {}
        Ok(Ok(false)) => tracing::warn!(
            target: "thegn::automation",
            row_id,
            "pipeline.start: dispatch row vanished before its autonomy was recorded"
        ),
        Ok(Err(error)) => tracing::warn!(
            target: "thegn::automation",
            row_id,
            error = %format!("{error:#}"),
            "pipeline.start: failed to record dispatch autonomy"
        ),
        Err(error) => tracing::warn!(
            target: "thegn::automation",
            row_id,
            %error,
            "pipeline.start: autonomy write task failed"
        ),
    }
    Ok(())
}

/// Map the floor gate to a refusal. Only a runnable, sandboxed dispatch that
/// met the floor passes; a host run, a degraded run or a hold refuses.
fn floor_refusal(gate: crate::agent_run::AgentDispatch) -> Option<String> {
    use crate::agent_run::AgentDispatch;
    const SETTING: &str = "set a sandbox that meets `[sandbox] isolation_floor` (at least \
                           shared-kernel) for this repo/worktree (`[sandbox] profile`, or the \
                           worktree's isolation pin)";
    match gate {
        AgentDispatch::Run(Some(spec)) => {
            if spec.seal_home.is_none() {
                return Some(
                    "pipeline.start refused: the worker's sandbox does not hide $HOME, and an \
                     issue-driven worker runs untrusted tracker text (autopilot's sealed rule); \
                     set `[sandbox] profile = \"sealed\"` for this repo or pin the worktree sealed"
                        .to_owned(),
                );
            }
            if let Some(miss) = thegn_core::sandbox_floor::home_gate(&spec) {
                return Some(format!("pipeline.start refused: {miss}"));
            }
            if matches!(
                spec.backend,
                thegn_core::sandbox::Backend::Systemd
                    | thegn_core::sandbox::Backend::Wsl
                    | thegn_core::sandbox::Backend::WinAppContainer
                    | thegn_core::sandbox::Backend::WinJobObject
            ) {
                return Some(format!(
                    "pipeline.start refused: sandbox backend '{}' cannot provide sealed \
                     filesystem isolation; {SETTING}",
                    spec.backend.label()
                ));
            }
            None
        }
        AgentDispatch::Run(None) => Some(format!(
            "pipeline.start refused: the worker would run on the host with no sandbox; {SETTING}"
        )),
        AgentDispatch::RunDegraded(_, warning) => Some(format!(
            "pipeline.start refused: the worker's sandbox misses the isolation floor \
             ({warning}); {SETTING}"
        )),
        AgentDispatch::InfraHold(reason) => Some(format!(
            "pipeline.start refused: the worker's sandbox could not be admitted ({reason}); \
             {SETTING}"
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg_with_stages(names: &[&str]) -> Config {
        let mut cfg = Config::default();
        cfg.pipeline.stages = names
            .iter()
            .map(|name| thegn_core::config_pipeline::PipelineStage {
                name: (*name).into(),
                agent: "claude".into(),
                ..Default::default()
            })
            .collect();
        cfg
    }

    #[test]
    fn plan_start_refuses_without_an_issue() {
        for autonomy in Autonomy::ALL {
            assert!(matches!(
                plan_start(autonomy, false, false),
                StartPlan::Refuse(r) if r.contains("issue_matched")
            ));
        }
    }

    #[test]
    fn plan_start_defers_to_a_live_autopilot_run() {
        for autonomy in Autonomy::ALL {
            assert!(matches!(
                plan_start(autonomy, true, true),
                StartPlan::Refuse(r) if r.contains("issue is owned by [autopilot]")
            ));
        }
    }

    #[test]
    fn plan_start_proposes_only_at_propose() {
        assert_eq!(
            plan_start(Autonomy::Propose, true, false),
            StartPlan::Propose
        );
        assert_eq!(plan_start(Autonomy::Review, true, false), StartPlan::Start);
        assert_eq!(plan_start(Autonomy::Merge, true, false), StartPlan::Start);
    }

    #[test]
    fn stage_for_without_a_pipeline_names_the_section() {
        let err = stage_for(&Config::default(), None).unwrap_err().to_string();
        assert!(err.contains("[[pipeline.stages]]"), "{err}");
        let err = stage_for(&Config::default(), Some("plan"))
            .unwrap_err()
            .to_string();
        assert!(err.contains("[[pipeline.stages]]"), "{err}");
    }

    #[test]
    fn stage_for_defaults_to_the_first_stage() {
        let cfg = cfg_with_stages(&["plan", "implement"]);
        assert_eq!(stage_for(&cfg, None).unwrap(), "plan");
        assert_eq!(stage_for(&cfg, Some("  ")).unwrap(), "plan");
    }

    #[test]
    fn stage_for_honours_a_configured_param_and_refuses_an_unknown_one() {
        let cfg = cfg_with_stages(&["plan", "implement"]);
        assert_eq!(stage_for(&cfg, Some("implement")).unwrap(), "implement");
        let err = stage_for(&cfg, Some("deploy")).unwrap_err().to_string();
        assert!(
            err.contains("deploy") && err.contains("plan, implement"),
            "{err}"
        );
    }

    #[test]
    fn issue_id_accepts_tracker_shapes() {
        assert_eq!(validate_issue_id("linear:THE-7").unwrap(), "linear:THE-7");
        assert_eq!(validate_issue_id("Linear:THE-7").unwrap(), "linear:THE-7");
        assert_eq!(validate_issue_id("THE-2193").unwrap(), "THE-2193");
        assert!(validate_issue_id("github:acme/repo#12").is_ok());
    }

    #[test]
    fn issue_id_rejects_hostile_input_without_echoing_it() {
        let huge = "A".repeat(10 * 1024);
        for hostile in [
            "",
            "../etc/passwd",
            "linear:../../x",
            "/abs/path",
            "-rf",
            "THE-1\nrm -rf /",
            "THE-1\r",
            "THE 1",
            "THE-1;id",
            "$(id)",
            "THE-1`id`",
            "a//b",
            "x/",
            ":THE-1",
            "linear:",
            "é-1",
            huge.as_str(),
        ] {
            let err = validate_issue_id(hostile).expect_err(hostile).to_string();
            assert!(err.len() < 200, "error must be bounded: {err}");
            if hostile.len() > 3 {
                assert!(!err.contains(hostile), "error echoed the input: {err}");
            }
        }
    }

    #[test]
    fn proposal_body_is_a_valid_approval_request() {
        let sha = "a".repeat(40);
        let body = proposal_body("triage", "linear:THE-7", "plan", &sha).unwrap();
        assert_eq!(body["issue_id"], "linear:THE-7");
        assert_eq!(body["stage"], "plan");
        assert_eq!(body["commit_sha"], sha.as_str());
        assert_eq!(body["reason"], "automation triage proposes starting plan");
        assert!(body.get("worktree").is_none());
    }

    #[test]
    fn proposal_reason_is_bounded() {
        let rule = "r".repeat(2_000);
        let body = proposal_body(&rule, "THE-7", "plan", &"b".repeat(40)).unwrap();
        let reason = body["reason"].as_str().unwrap();
        assert_eq!(
            reason.chars().count(),
            thegn_core::approval_request::MAX_REASON
        );
    }

    #[test]
    fn hex_sha_shapes() {
        assert!(is_hex_sha(&"0".repeat(40)));
        assert!(is_hex_sha(&"f".repeat(64)));
        assert!(!is_hex_sha("main"));
        assert!(!is_hex_sha(&"g".repeat(40)));
        assert!(!is_hex_sha(""));
    }

    #[test]
    fn floor_refusal_passes_only_a_sandboxed_run() {
        use crate::agent_run::AgentDispatch;
        let host = floor_refusal(AgentDispatch::Run(None)).unwrap();
        assert!(host.contains("isolation_floor"), "{host}");
        let degraded =
            floor_refusal(AgentDispatch::RunDegraded(None, "below floor".into())).unwrap();
        assert!(degraded.contains("below floor") && degraded.contains("isolation_floor"));
        let held = floor_refusal(AgentDispatch::InfraHold("no backend".into())).unwrap();
        assert!(held.contains("no backend") && held.contains("[sandbox]"));
    }

    fn run(
        state: thegn_core::autopilot::AutopilotState,
        provider: &str,
        account: &str,
        issue: &str,
    ) -> thegn_core::autopilot::AutopilotSummary {
        thegn_core::autopilot::AutopilotSummary {
            id: 1,
            key: thegn_core::autopilot::AutopilotIssueKey::new(provider, account, issue),
            repo_root: "/repo".into(),
            worktree: None,
            branch: None,
            base_branch: None,
            state,
            attempt: 1,
            dispatch_id: None,
            pr_number: None,
            pr_head: None,
            pr_url: None,
            created_at: 0,
            updated_at: 0,
            claimed_at: None,
            finished_at: None,
            reason: None,
        }
    }

    #[test]
    fn autopilot_ownership_matches_only_live_runs_for_the_same_key() {
        use thegn_core::autopilot::{AutopilotIssueKey, AutopilotState};
        let key = AutopilotIssueKey::new("linear", "work", "linear:THE-7");
        assert!(autopilot_run_matches(
            &run(AutopilotState::Working, "linear", "work", "linear:THE-7"),
            &key
        ));
        assert!(autopilot_run_matches(
            &run(AutopilotState::Claimed, "Linear", "work", "THE-7"),
            &key
        ));
        assert!(!autopilot_run_matches(
            &run(AutopilotState::Done, "linear", "work", "linear:THE-7"),
            &key
        ));
        assert!(!autopilot_run_matches(
            &run(AutopilotState::Working, "linear", "home", "linear:THE-7"),
            &key
        ));
        assert!(!autopilot_run_matches(
            &run(AutopilotState::Working, "linear", "work", "linear:THE-8"),
            &key
        ));
    }

    #[test]
    fn unregistered_repo_is_refused() {
        let _state = crate::testenv::IsolatedState::new();
        let db = thegn_core::db::Db::open().unwrap();
        let err = registered_repo_root(&db, "/definitely/not/registered")
            .unwrap_err()
            .to_string();
        assert!(err.contains("not a registered repo workspace"), "{err}");
    }

    #[test]
    fn registered_repo_root_returns_the_db_path() {
        use thegn_core::store::WorkspaceStore;
        let _state = crate::testenv::IsolatedState::new();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().to_string_lossy().into_owned();
        let db = thegn_core::db::Db::open().unwrap();
        db.put_workspace(&path, "repo", "repo").unwrap();
        assert_eq!(registered_repo_root(&db, &path).unwrap(), path);
        db.put_workspace("/plain/dir", "dir", "dir").unwrap();
        assert!(registered_repo_root(&db, "/plain/dir").is_err());
    }

    #[test]
    fn linked_worktree_requires_registration_repo_and_disk() {
        use thegn_core::store::{WorkspaceStore, WorktreeAuxStore};
        let _state = crate::testenv::IsolatedState::new();
        let repo = tempfile::tempdir().unwrap();
        let wt = tempfile::tempdir().unwrap();
        let repo_s = repo.path().to_string_lossy().into_owned();
        let wt_s = wt.path().to_string_lossy().into_owned();
        let db = thegn_core::db::Db::open().unwrap();
        // Linked but unregistered: not reused.
        db.link_issue(&wt_s, "linear:THE-7").unwrap();
        assert_eq!(linked_worktree(&db, &repo_s, "linear:THE-7").unwrap(), None);
        db.put_worktree("repo/worker", &repo_s, &wt_s, "feature", None, None)
            .unwrap();
        assert_eq!(
            linked_worktree(&db, &repo_s, "linear:THE-7").unwrap(),
            Some(wt_s.clone())
        );
        // Another repo's root, or another issue: not reused.
        assert_eq!(
            linked_worktree(&db, "/other", "linear:THE-7").unwrap(),
            None
        );
        assert_eq!(linked_worktree(&db, &repo_s, "linear:THE-8").unwrap(), None);
    }
}
