//! The daemon stage executor (b155): event-driven, no timer. It is kicked by
//! a worker exit (after `pipeline_retry` stamps the row), the reaper's
//! `CloseDone`, a successful `dispatches.set_status` to done or
//! `dispatches.report`, an approved lane approval, and one reconcile pass at
//! daemon start. It acts only on what `pipeline_supervise::plan` returns.
//!
//! With `[pipeline.supervisor] enabled = false` (the default) or no
//! `[[pipeline.stages]]`, `install` does nothing and every kick returns at
//! once (a relaxed atomic load): the idle invariant.
//!
//! # Shape
//!
//! A kick resolves the lane's tracker issue and runs [`drive`] for it under a
//! per-issue single flight: a kick arriving while the issue is being driven
//! marks it dirty, and the driver runs exactly one more pass when it finishes.
//! `drive` is bounded ([`MAX_ITERATIONS`]); there is no loop that outlives a
//! kick and no interval anywhere.
//!
//! # What it will not do
//!
//! - act while `automations.pause` is on (checked before every plan, before
//!   every validation task and before every advance);
//! - act on a lane whose worker may still be live (`LaneFacts::worker_live`,
//!   unknown = live). The pure policy already holds such lanes; the executor
//!   re-checks it before every action as a second fence;
//! - record a validation against a tip other than the one the plan asked
//!   for: the head is re-read after the task and a moved head records nothing;
//! - run a validation on the host: validations run worker-written code, so
//!   they run only inside the worktree's sealed sandbox
//!   ([`host_validation_isolated`]); without one the task records an
//!   environment error and nothing runs.

use std::collections::HashMap;
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

use thegn_core::config::Config;
use thegn_core::db::Db;
use thegn_core::issue::AgentDispatchStatus;
use thegn_core::pipeline_supervise::{EscalationReason, SuperviseAction};
use thegn_core::pipeline_validate::ValidationClass;
use thegn_core::store::NotificationStore as _;

use super::service::DaemonService;
use super::stage_land::{LaneCtx, TerminalOutcome};
use crate::cmd::supervise::Lane;

/// The most plan/act passes one kick may run. Each pass that changed
/// something re-plans; a lane that keeps changing past this is left for the
/// next event rather than spun on.
const MAX_ITERATIONS: usize = 8;

/// The most rows the start-up reconcile reads.
const RECONCILE_LIMIT: usize = 512;

/// The longest parent chain followed for a lane (root to leaf).
const MAX_CHAIN: usize = 16;

/// The `validate:` prefix on every executor job slot, so `cancel_all` and the
/// panel can tell executor validations from a person's test runs. `task.rs`
/// keys its credential scrub off it (`scrubs_credentials`); the executor's own
/// sandboxed runs apply the same scrub directly ([`run_in_sandbox`]).
pub(crate) const SLOT_PREFIX: &str = "validate:";

struct Handles {
    svc: Arc<DaemonService>,
    cfg: Arc<Config>,
    rt: tokio::runtime::Handle,
    permits: Arc<tokio::sync::Semaphore>,
}

static HANDLES: OnceLock<Handles> = OnceLock::new();
static ENABLED: AtomicBool = AtomicBool::new(false);

/// Per-issue single flight: present = being driven; the value = a kick
/// arrived meanwhile (re-run once). One mutex for both halves, so a kick can
/// never land between "no more dirty" and "release".
static INFLIGHT: Mutex<Option<HashMap<String, bool>>> = Mutex::new(None);

/// Executor validation job slots currently running (for `cancel_all`).
static SLOTS: Mutex<Option<HashSet<String>>> = Mutex::new(None);
/// The cancel flag of each in-flight sandboxed validation, keyed by slot.
static CANCELS: Mutex<Option<HashMap<String, Arc<AtomicBool>>>> = Mutex::new(None);

/// Tasks spawned by kicks; the disabled-supervisor test asserts it stays put.
#[cfg(test)]
static SPAWNED: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

fn note_spawn() {
    #[cfg(test)]
    SPAWNED.fetch_add(1, Ordering::Relaxed);
}

/// Start the executor when the supervisor is enabled and stages exist.
pub(crate) fn install(svc: Arc<DaemonService>, cfg: Arc<Config>) {
    if !enabled(&cfg) {
        return;
    }
    let Ok(rt) = tokio::runtime::Handle::try_current() else {
        tracing::warn!(
            target: "thegn::supervise",
            "stage executor not installed: no tokio runtime"
        );
        return;
    };
    let permits = Arc::new(tokio::sync::Semaphore::new(validate_permits(&cfg)));
    if HANDLES
        .set(Handles {
            svc,
            cfg,
            rt: rt.clone(),
            permits,
        })
        .is_err()
    {
        return;
    }
    ENABLED.store(true, Ordering::Release);
    tracing::info!(target: "thegn::supervise", "stage executor installed");
    note_spawn();
    rt.spawn(reconcile());
}

/// Whether the executor runs for this config.
pub(crate) fn enabled(cfg: &Config) -> bool {
    cfg.pipeline.supervisor.enabled && !cfg.pipeline.stages.is_empty()
}

/// Concurrent validations allowed: `max_validations`, `0` following
/// `[limits] test_max_parallel`, never zero.
pub(crate) fn validate_permits(cfg: &Config) -> usize {
    cfg.pipeline
        .supervisor
        .effective_max_validations(cfg.limits.test_max_parallel)
        .max(1)
}

/// Whether `stage` is the chart's last (it names no `next`). A stage the
/// chart no longer has is not terminal: it escalates instead of landing.
pub(crate) fn is_terminal(cfg: &Config, stage: &str) -> bool {
    cfg.pipeline
        .stage(stage)
        .is_some_and(|s| s.next_name().is_none())
}

/// The installed handles, or `None` when the executor is off (a relaxed load
/// on the idle path).
fn active() -> Option<&'static Handles> {
    if !ENABLED.load(Ordering::Relaxed) {
        return None;
    }
    HANDLES.get()
}

/// Re-plan the lane owning dispatch row `row_id`.
pub(crate) fn kick_row(row_id: i64) {
    let Some(h) = active() else { return };
    note_spawn();
    h.rt.spawn(async move {
        let found = tokio::task::spawn_blocking(move || -> anyhow::Result<_> { Db::open()?.get_dispatch(row_id) }).await;
        match found {
            Ok(Ok(Some(row))) if row.stage.is_some() => kick_issue(&row.issue_id),
            Ok(Ok(_)) => {}
            Ok(Err(error)) => tracing::warn!(
                target: "thegn::supervise",
                row = row_id,
                error = %format!("{error:#}"),
                "stage executor: could not resolve the row's issue"
            ),
            Err(error) => tracing::warn!(target: "thegn::supervise", row = row_id, %error, "stage executor: row lookup task failed"),
        }
    });
}

/// Re-plan the lane whose worker ran in daemon session `session`.
pub(crate) fn kick_session(session: &str) {
    let Some(h) = active() else { return };
    let session = session.to_owned();
    note_spawn();
    h.rt.spawn(async move {
        let lookup = session.clone();
        let found =
            tokio::task::spawn_blocking(move || -> anyhow::Result<_> { Db::open()?.dispatch_by_session(&lookup) }).await;
        match found {
            Ok(Ok(Some(row))) if row.stage.is_some() => kick_issue(&row.issue_id),
            Ok(Ok(_)) => {}
            Ok(Err(error)) => tracing::warn!(
                target: "thegn::supervise",
                %session,
                error = %format!("{error:#}"),
                "stage executor: could not resolve the session's issue"
            ),
            Err(error) => tracing::warn!(target: "thegn::supervise", %session, %error, "stage executor: session lookup task failed"),
        }
    });
}

/// Re-plan every lane of tracker issue `issue_id`.
pub(crate) fn kick_issue(issue_id: &str) {
    let Some(h) = active() else { return };
    let key = flight_key(issue_id);
    if !claim(&key) {
        return;
    }
    let issue = issue_id.to_owned();
    note_spawn();
    h.rt.spawn(async move { run_claimed(h, key, issue).await });
}

/// Take the single flight for `key` (see [`flight_key`]). `false` = already being driven; the
/// driver was told to run once more.
fn claim(key: &str) -> bool {
    let mut guard = INFLIGHT.lock().unwrap_or_else(|e| e.into_inner());
    let map = guard.get_or_insert_with(HashMap::new);
    match map.get_mut(key) {
        Some(dirty) => {
            *dirty = true;
            false
        }
        None => {
            map.insert(key.to_owned(), false);
            true
        }
    }
}

/// End one pass for a claimed issue: `true` = a kick arrived meanwhile, run
/// again (the claim is kept); `false` = released.
fn finish_pass(key: &str) -> bool {
    let mut guard = INFLIGHT.lock().unwrap_or_else(|e| e.into_inner());
    let map = guard.get_or_insert_with(HashMap::new);
    match map.get_mut(key) {
        Some(dirty) if *dirty => {
            *dirty = false;
            true
        }
        _ => {
            map.remove(key);
            false
        }
    }
}

async fn run_claimed(h: &'static Handles, key: String, issue: String) {
    loop {
        drive(h, &issue).await;
        if !finish_pass(&key) {
            break;
        }
    }
}

/// The one start-up pass: every issue with a staged row that is running,
/// done or parked, oldest first, driven one at a time. Then it exits.
async fn reconcile() {
    let Some(h) = active() else { return };
    let issues = tokio::task::spawn_blocking(|| -> anyhow::Result<Vec<String>> {
        let db = Db::open()?;
        let (rows, truncated) = db.list_dispatches_with_status(
            &[
                AgentDispatchStatus::Running,
                AgentDispatchStatus::Done,
                AgentDispatchStatus::WaitingHuman,
            ],
            RECONCILE_LIMIT,
        )?;
        if truncated {
            tracing::warn!(
                target: "thegn::supervise",
                limit = RECONCILE_LIMIT,
                "stage executor reconcile read only the oldest rows; later lanes wait for their next event"
            );
        }
        Ok(distinct_issues(
            rows.into_iter()
                .filter(|row| row.stage.is_some())
                .map(|row| row.issue_id),
        ))
    })
    .await;
    let issues = match issues {
        Ok(Ok(issues)) => issues,
        Ok(Err(error)) => {
            tracing::warn!(target: "thegn::supervise", error = %format!("{error:#}"), "stage executor reconcile could not read the roster");
            return;
        }
        Err(error) => {
            tracing::warn!(target: "thegn::supervise", %error, "stage executor reconcile task failed");
            return;
        }
    };
    tracing::info!(target: "thegn::supervise", lanes = issues.len(), "stage executor reconcile");
    for issue in issues {
        let key = flight_key(&issue);
        if claim(&key) {
            run_claimed(h, key, issue).await;
        }
    }
}

/// First-seen order, duplicates dropped.
fn distinct_issues(ids: impl Iterator<Item = String>) -> Vec<String> {
    let mut seen = HashSet::new();
    ids.filter(|id| seen.insert(id.clone())).collect()
}

/// What one action did to the lane.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Step {
    /// Something changed: re-plan.
    Changed,
    /// Nothing to re-plan for.
    Unchanged,
    /// Stop driving this issue (paused, refused, cancelled).
    Stop,
}

/// Whether `path` is a local worktree in thegn's registry.
fn registered_local_worktree(path: &str) -> bool {
    use thegn_core::store::WorkspaceStore as _;
    let Ok(db) = Db::open() else { return false };
    let Ok(rows) = db.worktrees() else {
        return false;
    };
    matches_registered(&rows, path, |p| std::fs::canonicalize(p).ok())
}

/// Pure row match for [`registered_local_worktree`]: a local row whose path
/// equals `path`, or canonicalizes to the same place.
fn matches_registered(
    rows: &[thegn_core::models::WorktreeRow],
    path: &str,
    canon: impl Fn(&str) -> Option<PathBuf>,
) -> bool {
    let want = canon(path);
    rows.iter().any(|r| {
        let loc = r.location.trim();
        (loc.is_empty() || loc == "local")
            && (r.worktree == path || (want.is_some() && canon(&r.worktree) == want))
    })
}

/// Single-flight key: one per issue, whatever the provider spelling.
fn flight_key(issue: &str) -> String {
    thegn_core::pipeline_identity::split_issue_id(issue.trim())
        .1
        .to_ascii_lowercase()
}

fn log_action(issue: &str, row: i64, action: &str, outcome: &str) {
    tracing::info!(target: "thegn::supervise", issue, row, action, outcome, "stage executor");
}

/// Plan the issue and act on it, at most [`MAX_ITERATIONS`] passes.
async fn drive(h: &'static Handles, issue: &str) {
    for _ in 0..MAX_ITERATIONS {
        let cfg = Arc::clone(&h.cfg);
        let want = issue.to_owned();
        let planned = tokio::task::spawn_blocking(
            move || -> anyhow::Result<Option<Vec<(Lane, SuperviseAction)>>> {
                let db = Db::open()?;
                if thegn_core::automation_control::paused_fail_closed(&db) {
                    return Ok(None);
                }
                let lanes = crate::cmd::supervise::gather_lane(&cfg, &db, &want)?;
                let facts: Vec<_> = lanes.iter().map(|lane| lane.facts.clone()).collect();
                let actions = thegn_core::pipeline_supervise::plan(
                    &facts,
                    &cfg.pipeline,
                    &cfg.pipeline.supervisor,
                    thegn_core::util::now_ms(),
                );
                Ok(Some(lanes.into_iter().zip(actions).collect()))
            },
        )
        .await;
        let planned = match planned {
            Ok(Ok(Some(planned))) => planned,
            Ok(Ok(None)) => {
                log_action(issue, 0, "plan", "paused");
                return;
            }
            Ok(Err(error)) => {
                tracing::warn!(target: "thegn::supervise", issue, error = %format!("{error:#}"), "stage executor could not plan");
                return;
            }
            Err(error) => {
                tracing::warn!(target: "thegn::supervise", issue, %error, "stage executor plan task failed");
                return;
            }
        };

        let mut changed = false;
        for (lane, action) in &planned {
            match act(h, lane, action).await {
                Step::Changed => changed = true,
                Step::Unchanged => {}
                Step::Stop => return,
            }
        }
        if !changed {
            return;
        }
    }
    log_action(issue, 0, "drive", "iteration_cap");
}

async fn act(h: &'static Handles, lane: &Lane, action: &SuperviseAction) -> Step {
    let row = action.row();
    let token = action.token();
    let issue = lane.facts.issue_id.as_str();
    let acts = action.is_mutating()
        || matches!(
            action,
            SuperviseAction::Validate { .. } | SuperviseAction::Escalate { .. }
        );
    if !acts {
        return Step::Unchanged;
    }
    // Only act in a local worktree thegn registered: a roster row's path is
    // caller-supplied, so an arbitrary git dir must never run tasks.
    let wt = lane.row.worktree_path.clone();
    let registered = tokio::task::spawn_blocking(move || registered_local_worktree(&wt))
        .await
        .unwrap_or(false);
    if !registered {
        log_action(issue, row, token, "unregistered_worktree");
        return Step::Stop;
    }
    // Second fence over the policy's own `worker_live` hold: never touch a
    // lane a worker may still be writing (unknown maps to live).
    if lane.facts.worker_live && !matches!(action, SuperviseAction::Escalate { .. }) {
        log_action(issue, row, token, "worker_live");
        return Step::Unchanged;
    }
    match action {
        SuperviseAction::Validate {
            row,
            tasks,
            commit_sha,
        } => validate(h, lane, *row, tasks, commit_sha).await,
        SuperviseAction::Advance {
            row, next_stage, ..
        } => advance(h, lane, *row, next_stage).await,
        SuperviseAction::Enqueue { .. } => terminal(h, lane, action).await,
        SuperviseAction::Escalate {
            why: EscalationReason::AwaitingApproval { stage, .. },
            ..
        } if is_terminal(&h.cfg, stage) => {
            if lane.facts.worker_live {
                log_action(issue, row, token, "worker_live");
                return Step::Unchanged;
            }
            terminal(h, lane, action).await
        }
        SuperviseAction::Escalate { why, .. } => {
            let ctx = match lane_ctx(lane).await {
                Some(ctx) => ctx,
                None => return Step::Unchanged,
            };
            match super::stage_land::on_escalate(&h.svc, &h.cfg, &ctx, why).await {
                Ok(()) => log_action(issue, row, why.token(), "escalated"),
                Err(error) => {
                    tracing::warn!(target: "thegn::supervise", issue, row, error = %format!("{error:#}"), "stage executor escalation failed");
                }
            }
            Step::Unchanged
        }
        SuperviseAction::Hold { .. } => Step::Unchanged,
    }
}

// --- validate ---------------------------------------------------------------

/// What to do with a finished validation, given the tip it was planned for
/// and the tip re-read after it ran.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ValidationDecision {
    /// The tip is unchanged: record the result against it.
    Record,
    /// The tip moved (or is unreadable): record nothing, re-plan.
    HeadMoved,
}

fn validation_decision(planned: &str, head_now: Option<&str>) -> ValidationDecision {
    match head_now {
        Some(head) if !planned.is_empty() && head == planned => ValidationDecision::Record,
        _ => ValidationDecision::HeadMoved,
    }
}

/// The worktree's current tip, or `None` when it cannot be read.
fn read_head(worktree: &Path) -> Option<String> {
    thegn_core::remote::GitLoc::Local(worktree.to_path_buf())
        .git_text(&["rev-parse", "HEAD"])
        .ok()
        .filter(|sha| !sha.is_empty())
}

/// Register a validation slot; the returned flag is set by [`cancel_all`].
fn slots_insert(slot: &str) -> Arc<AtomicBool> {
    let mut guard = SLOTS.lock().unwrap_or_else(|e| e.into_inner());
    guard
        .get_or_insert_with(HashSet::new)
        .insert(slot.to_owned());
    let cancel = Arc::new(AtomicBool::new(false));
    let mut flags = CANCELS.lock().unwrap_or_else(|e| e.into_inner());
    flags
        .get_or_insert_with(HashMap::new)
        .insert(slot.to_owned(), Arc::clone(&cancel));
    cancel
}

fn slots_remove(slot: &str) {
    let mut guard = SLOTS.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(set) = guard.as_mut() {
        set.remove(slot);
    }
    let mut flags = CANCELS.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(map) = flags.as_mut() {
        map.remove(slot);
    }
}

/// The refusal an executor validation records when the worktree has no
/// sealed sandbox to run it in: nothing runs on the host.
const NEEDS_SEALED: &str = "unattended validation needs a sealed sandbox for this worktree";

/// How one sandboxed validation ended.
#[derive(Debug, PartialEq, Eq)]
enum SandboxedRun {
    /// Nothing ran (or the run could not be supervised): an environment error.
    Refused(String),
    /// Cancelled by pause/halt: record nothing.
    Cancelled,
    /// The command ran to an end (a timeout reads as exit 124).
    Done {
        exit_code: Option<i32>,
        text: String,
    },
}

/// Whether `spec` is a real isolating sandbox an unattended validation may
/// run worker-written code in: the same sealed floor `pipeline.start` and the
/// stage advance apply ([`floor_refusal`]). A `standard` profile that leaves
/// `$HOME` credentials readable does not qualify. Validations run worker code
/// (its justfile, build.rs, tests) and must never run on the host, where they
/// could reach the daemon socket (implicit local admin) and `$HOME` secrets.
pub(crate) fn host_validation_isolated(spec: &thegn_core::sandbox::SandboxSpec) -> bool {
    floor_refusal(crate::agent_run::AgentDispatch::Run(Some(spec.clone()))).is_none()
}

/// Resolve the lane worktree's sandbox through the same floor gate a stage
/// advance uses; `Ok` only for a sealed, isolating sandbox.
async fn validation_sandbox(
    h: &'static Handles,
    worktree: &str,
) -> Result<thegn_core::sandbox::SandboxSpec, String> {
    use crate::agent_run::AgentDispatch;
    let cfg = Arc::clone(&h.cfg);
    let wt = worktree.to_owned();
    let gate = tokio::task::spawn_blocking(move || {
        crate::agent_run::agent_floor_gate(
            &cfg,
            &wt,
            true,
            thegn_core::config::IsolationFloor::SharedKernel,
            thegn_core::config::OnFloorMiss::Fail,
        )
    })
    .await
    .map_err(|error| format!("floor check failed ({error})"))?;
    match gate {
        AgentDispatch::Run(Some(spec)) if host_validation_isolated(&spec) => Ok(spec),
        other => Err(floor_refusal(other).unwrap_or_else(|| NEEDS_SEALED.to_owned())),
    }
}

/// Run `command` inside `spec` (blocking): the guarded sandbox entry (which
/// masks the daemon control socket dir, the state run dir, the runtime dir and
/// the Lead control dir exactly as for worker panes), the `thegn.slice` CPU
/// cap, the drain's credential scrub and a git-env scrub, in the lane's own
/// worktree, with the configured timeout. The child's process group is owned
/// by the platform's bounded runner (`platform::bounded_validation`): waited
/// on, killed on timeout or cancel, fully reaped. Where no bounded runner
/// exists the run is refused, never run on the host.
fn run_in_sandbox(
    spec: &thegn_core::sandbox::SandboxSpec,
    worktree: &Path,
    command: &str,
    timeout_secs: u64,
    cancel: &AtomicBool,
) -> SandboxedRun {
    use crate::platform::bounded_validation;
    if !bounded_validation::SUPPORTED {
        return SandboxedRun::Refused(format!("{NEEDS_SEALED} (unsupported on this platform)"));
    }
    // Keep a secret env-file alive until the bounded runner has reaped the child.
    let (inner, _env_file) = match thegn_core::sandbox::enter_argv_guarded_background(spec, command)
    {
        Ok(guarded) => guarded.into_parts(),
        Err(error) => return SandboxedRun::Refused(format!("{NEEDS_SEALED} ({error})")),
    };
    let argv = thegn_core::sandbox_cpucap::wrap_background_argv_tagged(
        inner,
        &thegn_core::sandbox_cpucap::ScopeTag {
            kind: thegn_core::sandbox_cpucap::ScopeKind::Agent,
            worktree: worktree.display().to_string(),
        },
    );
    let Some((program, args)) = argv.split_first() else {
        return SandboxedRun::Refused(format!("{NEEDS_SEALED} (empty sandbox entry)"));
    };
    let mut cmd = std::process::Command::new(program);
    cmd.args(args)
        .current_dir(worktree)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    for var in crate::daemon::lead_caps::merge_drain::SCRUBBED_ENV {
        cmd.env_remove(var);
    }
    for var in thegn_core::util::GIT_ENV_VARS {
        cmd.env_remove(var);
    }
    let run = bounded_validation::run(&mut cmd, timeout_secs, cancel);
    sandboxed_end(run.end, timeout_secs, run.text)
}

/// Map a bounded run's end to a validation result.
fn sandboxed_end(
    end: crate::platform::bounded_validation::ValidationEnd,
    timeout_secs: u64,
    text: String,
) -> SandboxedRun {
    use crate::platform::bounded_validation::ValidationEnd;
    match end {
        ValidationEnd::Exited(Some(125)) => {
            SandboxedRun::Refused("the validation sandbox refused to start (exit 125)".to_owned())
        }
        ValidationEnd::Exited(exit_code) => SandboxedRun::Done { exit_code, text },
        ValidationEnd::TimedOut => SandboxedRun::Done {
            exit_code: Some(124),
            text: format!(
                "validation exceeded {timeout_secs}s ([limits] test_timeout_secs) and was stopped\n{text}"
            ),
        },
        ValidationEnd::Cancelled => SandboxedRun::Cancelled,
        ValidationEnd::Failed(why) => SandboxedRun::Refused(why),
    }
}

fn paused_now() -> bool {
    match Db::open() {
        Ok(db) => thegn_core::automation_control::paused_fail_closed(&db),
        // An unopenable DB cannot record anything either: treat as a stop.
        Err(_) => true,
    }
}

async fn validate(
    h: &'static Handles,
    lane: &Lane,
    row: i64,
    tasks: &[String],
    commit_sha: &str,
) -> Step {
    let issue = lane.facts.issue_id.as_str();
    let Ok(_permit) = Arc::clone(&h.permits).acquire_owned().await else {
        return Step::Stop;
    };
    let worktree = PathBuf::from(&lane.row.worktree_path);
    for name in tasks {
        if tokio::task::spawn_blocking(paused_now)
            .await
            .unwrap_or(true)
        {
            log_action(issue, row, "validate", "paused");
            return Step::Stop;
        }
        let configured = h.cfg.tasks.iter().find(|t| t.name == *name).cloned();
        let (class, exit_code, digest) = match configured {
            None => (
                ValidationClass::EnvironmentError,
                None,
                "task not configured".to_owned(),
            ),
            Some(_) if h.cfg.limits.test_timeout_secs == 0 => (
                ValidationClass::EnvironmentError,
                None,
                "validation needs [limits] test_timeout_secs > 0".to_owned(),
            ),
            Some(task) => {
                let test = crate::task::test_task_from_config(&task);
                let matcher = test.matcher.clone();
                let slot = crate::task::job_slot(&worktree, &format!("{SLOT_PREFIX}{name}"));
                let run = match validation_sandbox(h, &lane.row.worktree_path).await {
                    Err(why) => {
                        tracing::warn!(target: "thegn::supervise", issue, row, task = %name, %why, "validation refused: no sealed sandbox");
                        SandboxedRun::Refused(NEEDS_SEALED.to_owned())
                    }
                    Ok(spec) => {
                        let cancel = slots_insert(&slot);
                        let wt = worktree.clone();
                        let command = test.command.clone();
                        let secs = h.cfg.limits.test_timeout_secs;
                        let joined = tokio::task::spawn_blocking(move || {
                            run_in_sandbox(&spec, &wt, &command, secs, &cancel)
                        })
                        .await;
                        slots_remove(&slot);
                        match joined {
                            Ok(run) => run,
                            Err(error) => {
                                tracing::warn!(target: "thegn::supervise", issue, row, task = %name, %error, "validation task failed to join");
                                return Step::Stop;
                            }
                        }
                    }
                };
                match run {
                    SandboxedRun::Refused(why) => (ValidationClass::EnvironmentError, None, why),
                    SandboxedRun::Cancelled => {
                        // Cancelled (pause/halt): the run established nothing.
                        log_action(issue, row, "validate", "cancelled");
                        return Step::Stop;
                    }
                    SandboxedRun::Done { exit_code, text } => {
                        let class = thegn_core::pipeline_validate::classify(
                            exit_code,
                            false,
                            Some(&matcher),
                            &text,
                        );
                        let digest =
                            thegn_core::pipeline_validate::digest(&text, class, Some(&matcher));
                        (class, exit_code, digest)
                    }
                }
            }
        };

        let wt = worktree.clone();
        let head = tokio::task::spawn_blocking(move || read_head(&wt))
            .await
            .unwrap_or(None);
        if head.is_none() {
            log_action(issue, row, "validate", "head_unreadable");
            return Step::Stop;
        }
        if validation_decision(commit_sha, head.as_deref()) == ValidationDecision::HeadMoved {
            log_action(issue, row, "validate", "head_moved");
            return Step::Changed;
        }
        let (task_name, sha) = (name.clone(), commit_sha.to_owned());
        let recorded = tokio::task::spawn_blocking(move || -> anyhow::Result<()> {
            let db = Db::open()?;
            db.record_validation(
                &thegn_core::db_pipeline_supervise::ValidationOutcome {
                    dispatch_id: row,
                    task: &task_name,
                    commit_sha: &sha,
                    class,
                    exit_code,
                    digest: &digest,
                },
                thegn_core::util::now_ms(),
            )?;
            Ok(())
        })
        .await;
        match recorded {
            Ok(Ok(())) => log_action(issue, row, "validate", class.as_str()),
            Ok(Err(error)) => {
                tracing::warn!(target: "thegn::supervise", issue, row, task = %name, error = %format!("{error:#}"), "could not record validation");
                return Step::Stop;
            }
            Err(error) => {
                tracing::warn!(target: "thegn::supervise", issue, row, task = %name, %error, "validation record task failed");
                return Step::Stop;
            }
        }
    }
    Step::Changed
}

// --- advance ----------------------------------------------------------------

/// What the child row inherits from its parent's automation columns.
#[derive(Debug, Clone, PartialEq, Eq)]
enum ChildAutomation {
    /// Autonomy and the rule that started the lane.
    Both { autonomy: String, rule: String },
    /// A ceiling with no rule (recorded by hand or before `automation_rule`).
    AutonomyOnly(String),
    /// A manual lane: nothing to copy.
    None,
}

fn child_automation(autonomy: Option<String>, rule: Option<String>) -> ChildAutomation {
    let nonempty = |v: Option<String>| v.filter(|s| !s.trim().is_empty());
    match (nonempty(autonomy), nonempty(rule)) {
        (Some(autonomy), Some(rule)) => ChildAutomation::Both { autonomy, rule },
        (Some(autonomy), None) => ChildAutomation::AutonomyOnly(autonomy),
        // A rule with no autonomy is not copied as anything: the land rule
        // reads a missing autonomy as `review` either way.
        (None, _) => ChildAutomation::None,
    }
}

/// The sandbox floor, as `pipeline.start` applies it: only a sealed sandbox
/// that met the floor passes.
pub(crate) fn floor_refusal(gate: crate::agent_run::AgentDispatch) -> Option<String> {
    use crate::agent_run::AgentDispatch;
    match gate {
        AgentDispatch::Run(Some(spec)) => {
            if spec.seal_home.is_none() {
                return Some(
                    "stage advance refused: the worker's sandbox does not hide $HOME".to_owned(),
                );
            }
            if let Some(miss) = thegn_core::sandbox_floor::home_gate(&spec) {
                return Some(format!("stage advance refused: {miss}"));
            }
            if matches!(
                spec.backend,
                thegn_core::sandbox::Backend::Systemd
                    | thegn_core::sandbox::Backend::Wsl
                    | thegn_core::sandbox::Backend::WinAppContainer
                    | thegn_core::sandbox::Backend::WinJobObject
            ) {
                return Some(format!(
                    "stage advance refused: sandbox backend '{}' cannot provide sealed filesystem isolation",
                    spec.backend.label()
                ));
            }
            None
        }
        AgentDispatch::Run(None) => Some(
            "stage advance refused: the worker would run on the host with no sandbox".to_owned(),
        ),
        AgentDispatch::RunDegraded(_, warning) => Some(format!(
            "stage advance refused: the worker's sandbox misses the isolation floor ({warning})"
        )),
        AgentDispatch::InfraHold(reason) => Some(format!(
            "stage advance refused: the worker's sandbox could not be admitted ({reason})"
        )),
    }
}

/// Pause, then the budget, as one blocking read.
enum Admission {
    Admitted,
    Paused,
    Refused(String),
}

async fn advance(h: &'static Handles, lane: &Lane, row: i64, next: &str) -> Step {
    let issue = lane.facts.issue_id.clone();
    let worktree = lane.row.worktree_path.clone();

    // 1. Pause + budget.
    let cfg = Arc::clone(&h.cfg);
    let (n_next, n_wt) = (next.to_owned(), worktree.clone());
    let admission = tokio::task::spawn_blocking(move || -> anyhow::Result<Admission> {
        let db = Db::open()?;
        if thegn_core::automation_control::paused_fail_closed(&db) {
            return Ok(Admission::Paused);
        }
        match crate::automation_budget::admit(
            &db,
            &cfg,
            thegn_core::automation_budget::SpawnKind::NextStage,
        ) {
            Ok(()) => Ok(Admission::Admitted),
            Err(refusal) => {
                let message = format!("Stage advance of row {row} to `{n_next}` held: {refusal}");
                // best-effort: the log line is the record.
                if let Err(error) = db.put_notification_once(
                    "automation_budget",
                    &format!("budget:{row}:{n_next}"),
                    &message,
                    &n_wt,
                ) {
                    tracing::warn!(target: "thegn::supervise", row, %error, "could not file the budget notification");
                }
                Ok(Admission::Refused(refusal.to_string()))
            }
        }
    })
    .await;
    match admission {
        Ok(Ok(Admission::Admitted)) => {}
        Ok(Ok(Admission::Paused)) => {
            log_action(&issue, row, "advance", "paused");
            return Step::Stop;
        }
        Ok(Ok(Admission::Refused(reason))) => {
            tracing::info!(target: "thegn::supervise", %issue, row, action = "advance", outcome = "budget", %reason, "stage executor");
            return Step::Stop;
        }
        Ok(Err(error)) => {
            tracing::warn!(target: "thegn::supervise", %issue, row, error = %format!("{error:#}"), "stage advance admission failed");
            return Step::Stop;
        }
        Err(error) => {
            tracing::warn!(target: "thegn::supervise", %issue, row, %error, "stage advance admission task failed");
            return Step::Stop;
        }
    }

    // 2. The sandbox floor, fail closed.
    let gate_cfg = Arc::clone(&h.cfg);
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
    .await;
    let refusal = match gate {
        Ok(gate) => floor_refusal(gate),
        Err(error) => Some(format!(
            "stage advance refused: floor check failed ({error})"
        )),
    };
    if let Some(reason) = refusal {
        let (n_next, n_wt) = (next.to_owned(), worktree.clone());
        let filed = tokio::task::spawn_blocking(move || -> anyhow::Result<bool> {
            Db::open()?.put_notification_once(
                "automation_failed",
                &format!("floor:{row}:{n_next}"),
                &reason,
                &n_wt,
            )
        })
        .await;
        if !matches!(filed, Ok(Ok(_))) {
            tracing::warn!(target: "thegn::supervise", %issue, row, "could not file the floor refusal notification");
        }
        log_action(&issue, row, "advance", "floor_refused");
        return Step::Stop;
    }

    // 3. Open the next stage, parented on the row. The parent's automation
    // origin rides along while its session is still registered; once that
    // session has exited (its origin is gone) the row's `automation_rule`
    // stands in, so the child is never an origin-less automation session
    // (`automations.halt` and Lead-rule confinement both key off it). A
    // roster read error stops the advance.
    let live_origin = lane
        .row
        .session_id
        .as_deref()
        .and_then(crate::automation_runtime::session_origin)
        .map(|o| thegn_svc::control::AutomationOrigin {
            root_event_id: o.root_event_id,
            rule_id: o.rule_id,
            run_id: o.run_id,
        });
    let origin = match live_origin {
        Some(origin) => Some(origin),
        None => {
            let rule = tokio::task::spawn_blocking(move || -> anyhow::Result<Option<String>> {
                Ok(Db::open()?.dispatch_automation(row)?.1)
            })
            .await;
            match rule {
                Ok(Ok(rule)) => {
                    crate::daemon::pipeline_retry::roster_origin(row, rule.as_deref(), "advance")
                }
                _ => {
                    tracing::warn!(target: "thegn::supervise", %issue, row, "stage advance could not read the lane's automation rule");
                    log_action(&issue, row, "advance", "origin_unreadable");
                    return Step::Stop;
                }
            }
        }
    };
    let client = match crate::automation_executor::client().await {
        Ok(client) => client,
        Err(error) => {
            tracing::warn!(target: "thegn::supervise", %issue, row, error = %format!("{error:#}"), "stage advance: no control client");
            return Step::Stop;
        }
    };
    let dispatch_cfg = Arc::clone(&h.cfg);
    let (d_next, d_issue, d_wt) = (next.to_owned(), issue.clone(), worktree.clone());
    let handle = h.rt.clone();
    // `open_stage_dispatch` holds a `Db` borrow across awaits (`!Send`): drive
    // it on a blocking thread so this future stays `Send`.
    let opened = tokio::task::spawn_blocking(move || {
        let mut request = crate::cmd::session::StageOpenRequest::fresh(&d_next, &d_issue, &d_wt);
        request.parent = Some(row);
        request.automation_origin = origin;
        handle.block_on(crate::cmd::session::open_stage_dispatch(
            &dispatch_cfg,
            &client,
            request,
        ))
    })
    .await;
    let child = match opened {
        Ok(Ok(opened)) => opened.row_id,
        Ok(Err(error)) => {
            tracing::warn!(target: "thegn::supervise", %issue, row, next, error = %format!("{error:#}"), "stage advance failed to open the next stage");
            log_action(&issue, row, "advance", "open_failed");
            return Step::Stop;
        }
        Err(error) => {
            tracing::warn!(target: "thegn::supervise", %issue, row, %error, "stage advance open task failed");
            return Step::Stop;
        }
    };

    // 4. Carry the lane's autonomy ceiling and rule onto the child.
    let copied = tokio::task::spawn_blocking(move || -> anyhow::Result<()> {
        let db = Db::open()?;
        let (autonomy, rule) = db.dispatch_automation(row)?;
        match child_automation(autonomy, rule) {
            ChildAutomation::Both { autonomy, rule } => {
                db.set_dispatch_automation(child, &autonomy, &rule)?;
            }
            ChildAutomation::AutonomyOnly(autonomy) => {
                db.set_dispatch_autonomy(child, &autonomy)?;
            }
            ChildAutomation::None => {}
        }
        Ok(())
    })
    .await;
    if !matches!(copied, Ok(Ok(()))) {
        // The child exists; a missing ceiling reads as `review` at land, the
        // safe direction.
        tracing::warn!(target: "thegn::supervise", %issue, row, child, "stage advance could not copy the lane's autonomy");
    }
    log_action(&issue, row, "advance", "opened");
    Step::Changed
}

// --- terminal ---------------------------------------------------------------

/// The dispatch chain from the root to `leaf`, following `parent_id`.
/// Cycle-safe and bounded at [`MAX_CHAIN`] rows (the leaf end is kept).
pub(crate) fn chain_of(rows: &[(i64, Option<i64>)], leaf: i64) -> Vec<i64> {
    let parents: HashMap<i64, Option<i64>> = rows.iter().copied().collect();
    let mut chain = vec![leaf];
    let mut seen = HashSet::from([leaf]);
    let mut at = leaf;
    while chain.len() < MAX_CHAIN {
        let Some(Some(parent)) = parents.get(&at) else {
            break;
        };
        if !seen.insert(*parent) {
            break;
        }
        chain.push(*parent);
        at = *parent;
    }
    chain.reverse();
    chain
}

/// Read the lane's parent links (bounded) and build its terminal context.
async fn lane_ctx(lane: &Lane) -> Option<LaneCtx> {
    let leaf = lane.row.id;
    let first_parent = lane.row.parent_id;
    let links = tokio::task::spawn_blocking(move || -> anyhow::Result<Vec<(i64, Option<i64>)>> {
        let db = Db::open()?;
        let mut links = vec![(leaf, first_parent)];
        let mut next = first_parent;
        let mut seen = HashSet::from([leaf]);
        while let Some(id) = next {
            if links.len() >= MAX_CHAIN || !seen.insert(id) {
                break;
            }
            let parent = db.get_dispatch(id)?.and_then(|row| row.parent_id);
            links.push((id, parent));
            next = parent;
        }
        Ok(links)
    })
    .await;
    let links = match links {
        Ok(Ok(links)) => links,
        Ok(Err(error)) => {
            tracing::warn!(target: "thegn::supervise", row = leaf, error = %format!("{error:#}"), "could not read the lane chain");
            return None;
        }
        Err(error) => {
            tracing::warn!(target: "thegn::supervise", row = leaf, %error, "lane chain task failed");
            return None;
        }
    };
    Some(LaneCtx {
        issue_id: lane.facts.issue_id.clone(),
        row: leaf,
        stage: lane.facts.stage.clone(),
        worktree: lane.row.worktree_path.clone(),
        head_sha: lane.facts.head_sha.clone(),
        facts: lane.facts.clone(),
        chain: chain_of(&links, leaf),
    })
}

async fn terminal(h: &'static Handles, lane: &Lane, action: &SuperviseAction) -> Step {
    let issue = lane.facts.issue_id.as_str();
    let row = action.row();
    let Some(ctx) = lane_ctx(lane).await else {
        return Step::Unchanged;
    };
    match super::stage_land::on_terminal(&h.svc, &h.cfg, &ctx, action).await {
        Ok(TerminalOutcome::Landed { drain_started }) => log_action(
            issue,
            row,
            action.token(),
            if drain_started {
                "landed_drain_started"
            } else {
                "landed"
            },
        ),
        Ok(TerminalOutcome::AskedApproval { reason }) => {
            tracing::info!(target: "thegn::supervise", issue, row, action = action.token(), outcome = "asked_approval", %reason, "stage executor");
        }
        Ok(TerminalOutcome::Held { reason }) => {
            tracing::info!(target: "thegn::supervise", issue, row, action = action.token(), outcome = "held", %reason, "stage executor");
        }
        Err(error) => {
            tracing::warn!(target: "thegn::supervise", issue, row, error = %format!("{error:#}"), "stage executor terminal step failed");
        }
    }
    // The land path files its own follow-ups (queue, approval); the next
    // event re-plans the lane.
    Step::Unchanged
}

/// Cancel every in-flight executor validation; returns how many stopped.
pub(crate) fn cancel_all() -> usize {
    let slots: Vec<String> = {
        let mut guard = SLOTS.lock().unwrap_or_else(|e| e.into_inner());
        guard
            .as_mut()
            .map(|set| set.drain().collect())
            .unwrap_or_default()
    };
    let flags: Vec<Arc<AtomicBool>> = {
        let mut guard = CANCELS.lock().unwrap_or_else(|e| e.into_inner());
        guard
            .as_mut()
            .map(|map| map.drain().map(|(_, flag)| flag).collect())
            .unwrap_or_default()
    };
    for flag in &flags {
        flag.store(true, Ordering::Release);
    }
    for slot in &slots {
        crate::task::cancel_slot(slot);
    }
    if !slots.is_empty() {
        tracing::info!(target: "thegn::supervise", stopped = slots.len(), "stage executor validations cancelled");
    }
    slots.len()
}

#[cfg(test)]
mod tests {
    use super::*;
    use thegn_core::config_pipeline::PipelineStage;

    fn stage(name: &str, next: Option<&str>) -> PipelineStage {
        PipelineStage {
            name: name.into(),
            next: next.map(Into::into),
            ..Default::default()
        }
    }

    fn staged(enabled: bool, stages: Vec<PipelineStage>) -> Config {
        let mut cfg = Config::default();
        cfg.pipeline.supervisor.enabled = enabled;
        cfg.pipeline.stages = stages;
        cfg
    }

    #[test]
    fn enabled_needs_the_supervisor_and_a_chart() {
        assert!(!enabled(&Config::default()));
        assert!(!enabled(&staged(true, vec![])));
        assert!(!enabled(&staged(false, vec![stage("code", None)])));
        assert!(enabled(&staged(true, vec![stage("code", None)])));
    }

    #[test]
    fn terminal_is_the_stage_with_no_next() {
        let cfg = staged(
            true,
            vec![stage("code", Some("review")), stage("review", Some("  "))],
        );
        assert!(!is_terminal(&cfg, "code"));
        assert!(is_terminal(&cfg, "review"), "a blank next is no next");
        assert!(
            !is_terminal(&cfg, "gone"),
            "an unconfigured stage escalates"
        );
    }

    #[test]
    fn a_sandboxed_validation_end_maps_timeouts_refusals_and_cancels() {
        use crate::platform::bounded_validation::ValidationEnd;
        assert_eq!(
            sandboxed_end(ValidationEnd::Exited(Some(0)), 60, "ok".into()),
            SandboxedRun::Done {
                exit_code: Some(0),
                text: "ok".into()
            }
        );
        assert!(matches!(
            sandboxed_end(ValidationEnd::Exited(Some(125)), 60, String::new()),
            SandboxedRun::Refused(_)
        ));
        match sandboxed_end(ValidationEnd::TimedOut, 60, "out".into()) {
            SandboxedRun::Done { exit_code, text } => {
                assert_eq!(exit_code, Some(124));
                assert!(text.contains("60s") && text.ends_with("out"));
            }
            other => panic!("a timeout is a failed run, got {other:?}"),
        }
        assert_eq!(
            sandboxed_end(ValidationEnd::Cancelled, 60, String::new()),
            SandboxedRun::Cancelled
        );
        assert!(matches!(
            sandboxed_end(ValidationEnd::Failed("unsettled".into()), 60, String::new()),
            SandboxedRun::Refused(_)
        ));
    }

    #[test]
    fn cancel_all_sets_a_sandboxed_validation_flag() {
        let slot = "/wt/cancel-flag-test:job:validate:nextest";
        let flag = slots_insert(slot);
        assert!(cancel_all() >= 1);
        assert!(
            flag.load(Ordering::Acquire),
            "halt must stop the sandboxed run"
        );
        slots_remove(slot);
    }

    #[test]
    fn validate_permits_follow_the_config_and_never_reach_zero() {
        let mut cfg = Config::default();
        cfg.pipeline.supervisor.max_validations = 3;
        assert_eq!(validate_permits(&cfg), 3);
        cfg.pipeline.supervisor.max_validations = 0;
        cfg.limits.test_max_parallel = 0;
        assert_eq!(validate_permits(&cfg), 1);
        cfg.limits.test_max_parallel = 4;
        assert_eq!(validate_permits(&cfg), 4);
    }

    #[test]
    fn chain_of_walks_to_the_root_and_survives_cycles() {
        let rows = [(1, None), (2, Some(1)), (3, Some(2)), (9, Some(8))];
        assert_eq!(chain_of(&rows, 3), vec![1, 2, 3]);
        assert_eq!(chain_of(&rows, 1), vec![1]);
        // An unknown parent ends the walk at the last known row.
        assert_eq!(chain_of(&rows, 9), vec![8, 9]);
        assert_eq!(chain_of(&rows, 42), vec![42]);
        let cycle = [(1, Some(2)), (2, Some(1))];
        assert_eq!(chain_of(&cycle, 1), vec![2, 1]);
        let long: Vec<(i64, Option<i64>)> =
            (1..=40).map(|id| (id, (id > 1).then(|| id - 1))).collect();
        let chain = chain_of(&long, 40);
        assert_eq!(chain.len(), MAX_CHAIN);
        assert_eq!(chain.last(), Some(&40));
    }

    #[test]
    fn a_disabled_executor_spawns_nothing_on_any_kick() {
        // Nothing in the test binary installs an enabled executor.
        assert!(active().is_none());
        let before = SPAWNED.load(Ordering::Relaxed);
        kick_row(1);
        kick_session("s");
        kick_issue("linear:THE-1");
        install_disabled_is_inert();
        assert_eq!(SPAWNED.load(Ordering::Relaxed), before);
        assert_eq!(cancel_all(), 0);
    }

    fn install_disabled_is_inert() {
        // `install` with the default config returns before touching a runtime
        // or the handles.
        assert!(!enabled(&Config::default()));
        assert!(HANDLES.get().is_none());
    }

    #[test]
    fn a_moved_or_unreadable_head_records_nothing() {
        let sha = "4babb0901f2c3d4e5f60718293a4b5c6d7e8f901";
        assert_eq!(
            validation_decision(sha, Some(sha)),
            ValidationDecision::Record
        );
        assert_eq!(
            validation_decision(sha, Some("0000000000000000000000000000000000000000")),
            ValidationDecision::HeadMoved
        );
        assert_eq!(
            validation_decision(sha, None),
            ValidationDecision::HeadMoved
        );
        assert_eq!(
            validation_decision("", Some("")),
            ValidationDecision::HeadMoved
        );
    }

    #[test]
    fn single_flight_reruns_once_for_kicks_during_a_pass() {
        let issue = "linear:THE-single-flight-test";
        assert!(claim(issue));
        assert!(!claim(issue), "second kick joins the running pass");
        assert!(!claim(issue));
        assert!(finish_pass(issue), "kicks during the pass re-run once");
        assert!(!finish_pass(issue), "then the claim is released");
        assert!(claim(issue), "released claims can be taken again");
        assert!(!finish_pass(issue));
    }

    #[test]
    fn single_flight_key_spans_issue_spellings() {
        let key = flight_key("linear:THE-9-flight-key-test");
        assert!(claim(&key));
        assert!(!claim(&flight_key("THE-9-flight-key-test")));
        assert!(finish_pass(&key));
        assert!(!finish_pass(&key));
    }

    #[test]
    fn unregistered_worktree_is_refused() {
        use thegn_core::store::WorkspaceStore as _;
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open_at(&dir.path().join("registry.db")).unwrap();
        let rows = db.worktrees().unwrap();
        let lead_repo = dir.path().join("notes");
        std::fs::create_dir_all(&lead_repo).unwrap();
        assert!(!matches_registered(
            &rows,
            lead_repo.to_str().unwrap(),
            |p| std::fs::canonicalize(p).ok()
        ));
    }

    #[test]
    fn distinct_issues_keep_first_seen_order() {
        let ids = ["b", "a", "b", "c", "a"].into_iter().map(String::from);
        assert_eq!(distinct_issues(ids), vec!["b", "a", "c"]);
    }

    #[test]
    fn child_automation_copies_autonomy_and_rule() {
        assert_eq!(
            child_automation(Some("merge".into()), Some("rule-a".into())),
            ChildAutomation::Both {
                autonomy: "merge".into(),
                rule: "rule-a".into()
            }
        );
        assert_eq!(
            child_automation(Some("review".into()), None),
            ChildAutomation::AutonomyOnly("review".into())
        );
        assert_eq!(
            child_automation(Some("review".into()), Some("  ".into())),
            ChildAutomation::AutonomyOnly("review".into())
        );
        assert_eq!(
            child_automation(None, Some("rule-a".into())),
            ChildAutomation::None
        );
        assert_eq!(child_automation(None, None), ChildAutomation::None);
    }

    #[test]
    fn advance_copies_the_parents_automation_onto_the_child_row() {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open_at(&dir.path().join("exec.db")).unwrap();
        let new = |parent| thegn_core::issue::NewDispatch {
            stage: Some("code"),
            parent_id: parent,
            ..thegn_core::issue::NewDispatch::new("linear:THE-2198", "/tmp/wt", "codex")
        };
        let parent = db.put_agent_dispatch(new(None)).unwrap();
        assert!(
            db.set_dispatch_automation(parent, "merge", "rule-a")
                .unwrap()
        );
        let child = db.put_agent_dispatch(new(Some(parent))).unwrap();

        let (autonomy, rule) = db.dispatch_automation(parent).unwrap();
        match child_automation(autonomy, rule) {
            ChildAutomation::Both { autonomy, rule } => {
                db.set_dispatch_automation(child, &autonomy, &rule).unwrap();
            }
            other => panic!("expected both columns, got {other:?}"),
        }
        assert_eq!(
            db.dispatch_automation(child).unwrap(),
            (Some("merge".into()), Some("rule-a".into()))
        );

        // A manual parent copies nothing.
        let manual = db.put_agent_dispatch(new(None)).unwrap();
        let (autonomy, rule) = db.dispatch_automation(manual).unwrap();
        assert_eq!(child_automation(autonomy, rule), ChildAutomation::None);
    }
}
