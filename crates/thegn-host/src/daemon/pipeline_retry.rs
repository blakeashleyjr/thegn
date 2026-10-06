//! The daemon's headless exit observer and transport-error retry policy
//! (THE-86/THE-121). `pty_drain` sees adopted panes only; every daemon session
//! emits `SessionExit`, so this task stamps the matching roster row for both
//! successful and failed headless workers. On a nonzero exit it additionally
//! classifies the final screen (pure core:
//! [`thegn_core::pipeline_exit`]) and either relaunch the row or park it.
//!
//! # Scope rules (who the observer may act on)
//!
//! - **Nonzero exits only.** Exit 0 is the artifact gate's verdict to make;
//!   substring matching must never re-read a success as a failure.
//! - **No attached clients at exit.** An adopted/grafted pane or a human
//!   attach means someone was watching, and the pane path or the human owns
//!   the verdict. The count is read from the tombstone (recorded by the actor
//!   at burial) — one lock-scope read, no polling, no race with the actor's
//!   teardown.
//! - **Pipeline rows in flight only.** A row the pane path or the Lead already
//!   closed is terminal and never touched.
//! - **Still parked at relaunch time.** The backoff sleeps up to a minute
//!   between the park and the relaunch; a verdict the Lead writes on the row
//!   in that window is newer than the retry plan, so the row is re-read after
//!   the sleep and only a row still `waiting_human` is relaunched.
//!
//! # This observer can park a row but never finish one
//!
//! Every outcome stamps `waiting_human` + a `note` on the SAME roster row —
//! never `done`, never `failed`. A retry re-stamps the row's session and
//! artifact and moves it back to `running` in one expected-state update: one
//! row cycling through attempts, not a chain of rows. The rule holds absolutely
//! here, because classifying a *final screen* is inference: this task is
//! guessing why a worker died and must never turn a guess into a verdict.
//!
//! The daemon as a whole now has exactly one narrow exception, in
//! [`super::pipeline_reaper`]: it may close a row on `CloseDone`, which fires
//! only when the artifact is committed, git-tracked, AND a report is filed —
//! the same gate `dispatch set-status done` enforces. That is arithmetic on
//! recorded facts rather than inference, which is precisely what separates it
//! from this module. Everything else there is still parked, never failed.
//!
//! # Retry semantics: exact resume, cold restart, never continue-latest
//!
//! - **Exact resume**: the row's CURRENT-generation `native_session_id`
//!   (v72, recorded by a CAS on row + session + `run_gen`, cleared on every
//!   run publication) names the harness conversation that belonged to this
//!   run. If the harness can resume by id, the retry resumes exactly it.
//!   Today only claude can be told its id at launch (`--session-id`).
//! - **Cold restart**: no recorded id (other harnesses, pre-v72 rows, a
//!   replaced run, a refused id shape): the stage prompt is re-rendered plus a
//!   fixed recovery note.
//! - **Continue-latest is unsafe and never used.** The id-free `continue` form
//!   picks the newest history in the worktree, which may be another run's
//!   (THE-265).
//!
//! # Event-driven, zero timers while idle
//!
//! The task blocks on the event broadcast feed; no polling, no tickers. The
//! only sleep is the backoff between a transport failure and its relaunch.
//! Retry work runs in bounded per-session tasks, never inline in the receive
//! loop (THE-266). Attempt counters live in daemon memory (keyed by roster row id,
//! surviving session-id changes): a daemon restart kills the sessions it
//! supervised, so there is nothing to retry across a restart — the durable
//! note column records what happened either way.

use anyhow::Context as _;
use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use thegn_core::control_wire::EventFrame;
use thegn_core::issue::AgentDispatch;
use thegn_core::issue::AgentDispatchStatus;
use thegn_core::pipeline_exit::{self, ExitSignatures};
use thegn_core::pipeline_run;
use thegn_core::store::{NotificationStore, WorkspaceStore};
use thegn_svc::control::{AgentLaunch, ControlApi, OpenSpec, SessionInfo};

use super::service::DaemonService;

/// Hard cap on retry cycles (backoff sleep + relaunch) in flight at once. A
/// provider outage hitting many rows backs them all off concurrently, but never
/// fans out past this (THE-266).
const MAX_CONCURRENT_RETRIES: usize = 8;
/// Hard cap on exits queued or running in the retry stage. An exit past it is
/// dropped with a warning and the `overflowed` flag, which triggers a durable
/// reconcile once the stage drains — nothing is silently lost.
const MAX_PENDING_EXITS: usize = 64;
/// Attempt-counter entries above which terminal rows are garbage-collected.
const ATTEMPTS_GC_THRESHOLD: usize = 128;

/// Attempt counters, keyed by roster ROW id (a relaunch re-stamps the session
/// id, so the row id is the stable key). Cleared when the row parks or
/// exhausts: a human re-drive starts a fresh budget. Locked only for short
/// map operations, never across an await.
pub(crate) type Attempts = Mutex<HashMap<i64, u32>>;

/// What the observer shares between its constant-time receive path and the
/// per-exit retry tasks.
struct Observer {
    svc: Arc<DaemonService>,
    attempts: Attempts,
    /// Sessions whose exit is queued or being handled: a duplicate frame for
    /// one of them coalesces instead of launching a second retry.
    pending: Mutex<HashSet<String>>,
    permits: tokio::sync::Semaphore,
    overflowed: AtomicBool,
}

type BoxFut = Pin<Box<dyn Future<Output = ()> + Send>>;

/// The receive loop stays constant-time (THE-266): each exit is stamped and
/// handed to a bounded per-session task, then the loop returns to `recv`
/// immediately. A 60 s backoff on one row can no longer delay another row's
/// stamp or classification, and a lagged receiver reconciles from durable
/// state instead of only logging.
pub(crate) fn spawn(
    svc: Arc<DaemonService>,
    mut rx: tokio::sync::broadcast::Receiver<Arc<EventFrame>>,
) {
    let obs = Arc::new(Observer {
        svc,
        attempts: Mutex::new(HashMap::new()),
        pending: Mutex::new(HashSet::new()),
        permits: tokio::sync::Semaphore::new(MAX_CONCURRENT_RETRIES),
        overflowed: AtomicBool::new(false),
    });
    tokio::spawn(async move {
        loop {
            match rx.recv().await {
                Ok(frame) => {
                    if let EventFrame::SessionExit { session, code } = &*frame {
                        observe_exit(&obs, session.clone(), *code);
                    }
                }
                Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                    tracing::warn!(
                        target: "thegn::daemon",
                        skipped = n,
                        "transport retry observer lagged; reconciling from durable state"
                    );
                    tokio::spawn(reconcile_missed_exits(obs.clone()));
                }
                Err(tokio::sync::broadcast::error::RecvError::Closed) => return,
            }
        }
    });
}

/// Constant-time handling of one exit: spawn the durable stamp and, for a
/// nonzero code, enqueue bounded retry work. Never awaits.
fn observe_exit(obs: &Arc<Observer>, session: String, code: Option<i32>) {
    // The CLI learns the server-generated session id only after
    // `sessions.open` returns, so a very short-lived worker can emit this
    // event just before open_stage stamps the row. Retry the association off
    // this task; raw sessions simply age out of the bounded lookup.
    let stamp_svc = obs.svc.clone();
    let stamp_session = session.clone();
    tokio::spawn(async move {
        if let Err(e) = stamp_dispatch_exit(stamp_svc, stamp_session.clone(), code).await {
            tracing::warn!(
                target: "thegn::daemon",
                session = %stamp_session,
                "dispatch exit stamp: {e:#}"
            );
        }
    });

    let Some(code) = code.filter(|c| *c != 0) else {
        // A clean exit is never retried: drop its assigned native id.
        obs.svc
            .native_ids
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .remove(&session);
        return;
    };
    {
        let mut pending = obs.pending.lock().unwrap_or_else(|p| p.into_inner());
        if pending.contains(&session) {
            return; // duplicate frame: already queued or running
        }
        if pending.len() >= MAX_PENDING_EXITS {
            obs.overflowed.store(true, Ordering::SeqCst);
            tracing::warn!(
                target: "thegn::daemon",
                session = %session,
                cap = MAX_PENDING_EXITS,
                "transport retry queue full; exit deferred to durable reconcile"
            );
            return;
        }
        pending.insert(session.clone());
    }
    let obs = obs.clone();
    tokio::spawn(async move {
        // best-effort: the semaphore is never closed.
        // `Result::ok(..)` form: the permit is used, not ignored (the ratchet
        // matches a trailing `.ok();` textually).
        let permit = Result::ok(obs.permits.acquire().await);
        if let Err(e) = handle_exit(&obs.svc, &session, code, &obs.attempts).await {
            // best-effort: a failed retry cycle must not kill the observer —
            // the note column records what it could.
            tracing::warn!(
                target: "thegn::daemon",
                session = %session,
                code,
                "transport retry: {e:#}"
            );
        }
        drop(permit);
        obs.pending
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .remove(&session);
        if obs.overflowed.swap(false, Ordering::SeqCst) {
            tokio::spawn(reconcile_missed_exits(obs.clone()));
        }
    });
}

/// Recover exits the broadcast dropped (receiver lag, or overflow of the
/// retry queue) from durable state: any in-flight roster row whose session is
/// dead (a tombstone exists) but whose exit was never stamped is re-fed through
/// the normal exit path. The stamp and the park/relaunch CAS make a row that
/// was in fact already handled a harmless no-op.
fn reconcile_missed_exits(obs: Arc<Observer>) -> BoxFut {
    Box::pin(async move {
        let rows = match obs.svc.with_db(|db| db.list_dispatches()).await {
            Ok(rows) => rows,
            Err(e) => {
                tracing::warn!(target: "thegn::daemon", "exit reconcile: {e:#}");
                return;
            }
        };
        for row in rows {
            if row.status.is_terminal() || row.exited_at_ms.is_some() {
                continue;
            }
            let Some(session) = row.session_id.filter(|s| !s.is_empty()) else {
                continue;
            };
            if let Some(tomb) = obs.svc.tombstone(&session).await {
                observe_exit(&obs, session, tomb.exit_code);
            }
        }
    })
}

/// Persist a daemon session exit on its dispatch row. The association normally
/// exists on the first read; the bounded retry closes the server-id publication
/// race for workers that start and exit before the CLI can stamp the returned
/// id. The database stamp is one-shot per run, so an adopted pane observing the
/// same exit is harmless.
pub(crate) async fn stamp_dispatch_exit(
    svc: Arc<DaemonService>,
    session: String,
    code: Option<i32>,
) -> anyhow::Result<()> {
    const LOOKUP_ATTEMPTS: usize = 20;
    const LOOKUP_RETRY: Duration = Duration::from_millis(25);

    for attempt in 0..LOOKUP_ATTEMPTS {
        let sid = session.clone();
        // Session identity only (THE-238): the worktree argument is unused on
        // the session path, and a miss is Stale — usually the publication race
        // this loop exists for, so it retries rather than falling back.
        let attribution = svc
            .with_db(move |db| db.dispatch_for_exit("", Some(&sid)))
            .await?;
        if let thegn_core::issue::ExitAttribution::Exact(run) = attribution {
            let outcome = svc
                .with_db(move |db| db.stamp_dispatch_exit(&run, code.map(i64::from)))
                .await?;
            if matches!(
                outcome,
                thegn_core::issue::ExitStamp::Stale | thegn_core::issue::ExitStamp::Missing
            ) {
                tracing::warn!(
                    target: "thegn::daemon",
                    session = %session,
                    ?outcome,
                    "session exit did not stamp its run"
                );
            }
            return Ok(());
        }
        if attempt + 1 < LOOKUP_ATTEMPTS {
            tokio::time::sleep(LOOKUP_RETRY).await;
        }
    }
    Ok(())
}

/// Handle one nonzero headless exit. Split from [`spawn`] so a stub test can
/// drive a synthetic exit through the same path with no PTY and no harness.
/// `_code` is already known nonzero (the caller's gate); the classifier's
/// `failed` argument is true by that same construction — the value itself is
/// deliberately unused here (the underscore, not a discarded binding: the
/// ignored-result ratchet pins `let _ = …` shapes).
pub(crate) async fn handle_exit(
    svc: &DaemonService,
    session: &str,
    _code: i32,
    attempts: &Attempts,
) -> anyhow::Result<()> {
    // Consume the launch-assigned native id up front, so no early return below
    // can leave an entry behind.
    let assigned = svc
        .native_ids
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .remove(session);

    // 1. The corpse: final screen + who was attached at death. One lock-scope
    //    read; the actor buries the tombstone BEFORE the exit reaches the feed,
    //    so an observer woken by the event always finds it.
    let Some(tomb) = svc.tombstone(session).await else {
        return Ok(());
    };
    if tomb.attached > 0 {
        // Someone is watching — an adopted pane or a human attach. The pane
        // path or the human owns the verdict; the two stampers never race.
        return Ok(());
    }

    // 2. The roster row this session was running, newest stamp wins. Skip
    //    anything without one, and anything already closed.
    let sid = session.to_string();
    let Some(row) = svc.with_db(move |db| db.dispatch_by_session(&sid)).await? else {
        return Ok(());
    };
    if row.status.is_terminal() {
        // The pane path or the Lead already wrote the verdict.
        return Ok(());
    }

    // 2b. The exact run this exit belongs to, and its harness-native session
    //     id. The id assigned at launch lives in daemon memory; it is made
    //     durable here by a CAS on (row, session, run_gen), so only the run
    //     that really owned this session can carry it. Whatever the row holds
    //     for its CURRENT generation is what a retry may resume.
    let (run, native) = svc
        .with_db({
            let id = row.id;
            let session = session.to_string();
            move |db| {
                let Some(run) = db.dispatch_run_ref(id)? else {
                    return Ok((None, None));
                };
                if run.session_id != session {
                    // The row already belongs to another run: nothing of this
                    // session's identity may be recorded or resumed.
                    return Ok((Some(run), None));
                }
                if let Some(native) = &assigned {
                    // A refused CAS (already has an id) is fine: the read below
                    // reports what is durable for exactly this run.
                    db.set_dispatch_native_session(&run, native)?;
                }
                let native = db.dispatch_native_session(&run)?;
                Ok((Some(run), native))
            }
        })
        .await?;

    // 3. Classify the flattened final screen. `failed = true` here — the
    //    nonzero-exit gate already ran in the caller.
    let Some(screen) = screen_of(&tomb.final_screen) else {
        return Ok(());
    };
    let tr = &svc.config.pipeline.transport_retry;
    if !tr.enabled {
        return Ok(());
    }
    let sig = ExitSignatures::from(tr);
    let Some(class) = pipeline_exit::classify(true, &screen, &sig) else {
        // A plain nonzero exit that matches nothing: the supervisor's call.
        return Ok(());
    };

    // 4. Decide. Attempts are 1-based per row, incremented per observed
    //    transport failure.
    gc_attempts(svc, attempts).await;
    let attempt = {
        let mut map = attempts.lock().unwrap_or_else(|p| p.into_inner());
        let a = map.entry(row.id).or_insert(0);
        *a += 1;
        *a
    };
    // `run` (read in step 2b) fences the backoff below: a re-drive in that
    // window publishes a newer run and this retry must not touch it.
    let decision = pipeline_exit::decide(&class, attempt, tr.max_attempts, tr.backoff_ms);

    match decision {
        pipeline_exit::RetryDecision::Park { note } => {
            forget(attempts, row.id);
            park(svc, row.id, row.status, &note).await?;
        }
        pipeline_exit::RetryDecision::Exhausted { note } => {
            forget(attempts, row.id);
            park(svc, row.id, row.status, &note).await?;
        }
        pipeline_exit::RetryDecision::Retry { attempt, delay_ms } => {
            let note = pipeline_exit::retry_note(signature_of(&class), attempt, tr.max_attempts);
            if !park(svc, row.id, row.status, &note).await? {
                forget(attempts, row.id);
                return Ok(());
            }
            tracing::info!(
                target: "thegn::daemon",
                row = row.id,
                attempt,
                delay_ms,
                "transport failure on a headless dispatch; relaunching"
            );
            tokio::time::sleep(Duration::from_millis(delay_ms)).await;
            // Reserve the row before opening the replacement. The old
            // read-then-open sequence still let a supervisor close the row
            // after the read and before `open` returned.
            let id_for_check = row.id;
            let still_ours = svc
                .with_db(move |db| db.dispatch_run_ref(id_for_check))
                .await?
                == run;
            let reserved = still_ours
                && svc
                    .with_db(move |db| {
                        db.compare_and_set_dispatch_status(
                            id_for_check,
                            AgentDispatchStatus::WaitingHuman,
                            AgentDispatchStatus::Spawning,
                            None,
                        )
                    })
                    .await?;
            if !reserved {
                forget(attempts, row.id);
                tracing::info!(
                    target: "thegn::daemon",
                    row = row.id,
                    "transport retry: the row was re-driven during backoff; relaunch skipped"
                );
                return Ok(());
            }
            match relaunch(svc, &row, native.as_deref()).await {
                Ok(info) => {
                    let artifact = row.artifact_path.clone().unwrap_or_default();
                    let id = row.id;
                    let session_id = info.id.clone();
                    let published = svc
                        .with_db(move |db| {
                            db.compare_and_set_dispatch_retry_run(
                                id,
                                AgentDispatchStatus::Spawning,
                                &session_id,
                                &artifact,
                            )
                        })
                        .await;
                    if !matches!(published, Ok(true)) {
                        forget(attempts, row.id);
                        let publish_error = published.err();
                        // best-effort: either the supervisor won after `open`
                        // or the publish itself failed. In both cases this
                        // session has no roster ownership and must be killed.
                        if let Err(error) = svc.kill(&info.id).await {
                            tracing::warn!(target: "thegn::daemon", session = %info.id, %error, "could not kill stale retry launch");
                        }
                        if let Some(error) = publish_error {
                            return Err(error.into());
                        }
                        return Ok(());
                    }
                    tracing::info!(target: "thegn::daemon", row = row.id, session = %info.id, "relaunched");
                }
                Err(e) => {
                    forget(attempts, row.id);
                    // Provider errors may include arbitrarily long stderr.
                    // The roster ledger is bounded; the artifact/log is where
                    // full diagnostics belong.
                    let failed_note: String = format!("{note}; relaunch failed: {e:#}")
                        .chars()
                        .take(thegn_core::pipeline_report::NOTE_MAX_CHARS)
                        .collect();
                    park(svc, row.id, AgentDispatchStatus::Spawning, &failed_note).await?;
                }
            }
        }
    }
    Ok(())
}

/// Stamp a row `waiting_human` with the given note — the observer's ONLY
/// status write. The daemon can park a row but never finish one.
async fn park(
    svc: &DaemonService,
    id: i64,
    expected: AgentDispatchStatus,
    note: &str,
) -> anyhow::Result<bool> {
    let note = note.to_string();
    svc.with_db(move |db| db.compare_and_set_dispatch_retry_park(id, expected, &note))
        .await
        .map_err(Into::into)
}

/// Flatten the tombstone's final screen to the plain text the classifier
/// reads. The tombstone carries geometry, so the same renderer a late
/// `snapshot` reader uses applies verbatim.
fn screen_of(frame: &EventFrame) -> Option<String> {
    match frame {
        EventFrame::PaneSnapshot {
            rows, cols, bytes, ..
        } => Some(crate::cmd::session::snapshot_text(*rows, *cols, bytes)),
        _ => None,
    }
}

fn signature_of(class: &pipeline_exit::ExitClass) -> &str {
    match class {
        pipeline_exit::ExitClass::Transport { signature } => signature,
        pipeline_exit::ExitClass::Limit { signature } => signature,
    }
}

/// Recovery context appended to the cold stage prompt of a retried row. Fixed
/// text, so the context a retry carries is bounded by construction.
const RECOVERY_CONTEXT: &str = "\n\nNOTE: an earlier attempt at this task was interrupted by a \
transport error. Its conversation is not available to you. Inspect the worktree (git status, \
git log) and the artifact path before redoing any work, and continue from what is already there.";

/// The launch spec of a transport retry (THE-265): an EXACT resume of
/// `resume` (a native session id proven to belong to this run), else a COLD
/// start. `continue_last` is never set: "latest session in the worktree" can be
/// another run's history.
fn retry_open_spec(row: &AgentDispatch, prompt: String, resume: Option<String>) -> OpenSpec {
    OpenSpec {
        automation_origin: None,
        argv: Vec::new(),
        cwd: None,
        env: Vec::new(),
        rows: 24,
        cols: 80,
        worktree: Some(row.worktree_path.clone()),
        agent: Some(AgentLaunch {
            agent: row.agent_name.clone(),
            prompt,
            // A retry is always headless — same as the dispatch it retries.
            headless: Some(true),
            bind_worktree: false,
            resume,
            continue_last: false,
            stage: row.stage.clone(),
            fork: false,
            native_session_id: None,
        }),
        adopt: false,
        already_capped: false,
    }
}

/// The native session id a retry may resume exactly: present, shape-valid, and
/// resumable by id on this agent's harness. Anything else is `None` (cold).
fn exact_resume_id(
    cfg: &thegn_core::config::Config,
    agent: &str,
    native: Option<&str>,
) -> Option<String> {
    let id = native.filter(|id| thegn_core::harness::session_id_ok(id))?;
    let harness = crate::daemon::agent_open::harness_for_agent(cfg, agent)?;
    harness.resume_command(id)?;
    Some(id.to_string())
}

/// Relaunch a failed row (exact resume or cold), through [`DaemonService::open`], so the
/// relaunch takes the same sandbox/credential/cap/seeder path every launch
/// takes. The stage prompt is re-rendered through the shared helpers — the CLI
/// dispatch path and this path render identically by construction — plus a
/// bounded recovery note.
async fn relaunch(
    svc: &DaemonService,
    row: &AgentDispatch,
    native: Option<&str>,
) -> anyhow::Result<SessionInfo> {
    crate::daemon::agent_open::harness_for_agent(&svc.config, &row.agent_name)
        .with_context(|| format!("unknown agent `{}` — cannot relaunch", row.agent_name))?;
    let spec = match exact_resume_id(&svc.config, &row.agent_name, native) {
        Some(id) => retry_open_spec(row, pipeline_exit::RETRY_NUDGE.to_string(), Some(id)),
        None => {
            let prompt = format!("{}{RECOVERY_CONTEXT}", cold_stage_prompt(svc, row).await?);
            retry_open_spec(row, prompt, None)
        }
    };
    svc.open(spec)
        .await
        .map_err(|e| anyhow::anyhow!("open: {e}"))
}

/// Drop a row's retry budget.
fn forget(attempts: &Attempts, id: i64) {
    attempts
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .remove(&id);
}

/// Garbage-collect counters of rows that reached a terminal state (or no
/// longer exist), so the map is bounded by the live roster, not daemon uptime.
async fn gc_attempts(svc: &DaemonService, attempts: &Attempts) {
    let ids: Vec<i64> = {
        let map = attempts.lock().unwrap_or_else(|p| p.into_inner());
        if map.len() < ATTEMPTS_GC_THRESHOLD {
            return;
        }
        map.keys().copied().collect()
    };
    let Ok(rows) = svc.with_db(|db| db.list_dispatches()).await else {
        return;
    };
    let live: HashSet<i64> = rows
        .iter()
        .filter(|r| !r.status.is_terminal())
        .map(|r| r.id)
        .collect();
    let mut map = attempts.lock().unwrap_or_else(|p| p.into_inner());
    for id in ids {
        if !live.contains(&id) {
            map.remove(&id);
        }
    }
}

/// Re-render the row's stage prompt cold — the no-continue-form relaunch.
/// Issue facts come from the daemon's tracker door, the branch from the DB
/// worktree registry (the same two-tier lookup the CLI dispatch uses, minus
/// the `git rev-parse` fallback: the registry row is already there).
async fn cold_stage_prompt(svc: &DaemonService, row: &AgentDispatch) -> anyhow::Result<String> {
    let stage_name = row
        .stage
        .clone()
        .filter(|s| !s.trim().is_empty())
        .context("row has no stage — cannot re-render the prompt")?;
    let stage = svc
        .config
        .pipeline
        .stage(&stage_name)
        .with_context(|| format!("stage '{stage_name}' is no longer configured"))?
        .clone();

    let facts = if crate::stage_prompt::needs_tracker(&stage.prompt) {
        let detail = svc
            .issues_get(&row.issue_id, Some(row.worktree_path.as_str()))
            .await
            .map_err(|e| anyhow::anyhow!("tracker lookup for {}: {e}", row.issue_id))?;
        crate::stage_prompt::IssueFacts {
            number: pipeline_run::issue_key(&row.issue_id),
            title: detail.issue.title,
            body: detail.issue.body.unwrap_or_default(),
            url: detail.issue.url,
        }
    } else {
        crate::stage_prompt::IssueFacts::number_only(pipeline_run::issue_key(&row.issue_id))
    };

    // Branch: the registered worktree row. A worktree the registry has lost
    // still relaunches, just without `{branch}` in its prompt. (The tracker
    // lookup above is different: it is scoped to the worktree's repo overlay
    // and fails closed for an unregistered worktree — no global fallback.)
    let wt = row.worktree_path.clone();
    let branch = svc
        .with_db(move |db| {
            Ok(db
                .worktrees()
                .ok()
                .and_then(|rows| {
                    rows.into_iter()
                        .find(|r| r.worktree == wt)
                        .map(|r| r.branch)
                })
                .filter(|b| !b.is_empty()))
        })
        .await?
        .unwrap_or_default();

    let artifact = row
        .artifact_path
        .clone()
        .unwrap_or_else(|| pipeline_run::artifact_path(&row.issue_id, &stage_name, row.id));
    let parent_artifact = match row.parent_id {
        Some(pid) => svc
            .with_db(move |db| Ok(db.get_dispatch(pid)?.and_then(|p| p.artifact_path)))
            .await?
            .unwrap_or_default(),
        None => String::new(),
    };

    let vars = crate::stage_prompt::stage_task_vars(
        &facts,
        &branch,
        &row.worktree_path,
        &stage_name,
        &artifact,
        &parent_artifact,
        row.id,
    );
    crate::stage_prompt::render_stage(&stage_name, &stage.prompt, &vars)
}

#[cfg(test)]
mod tests {
    use super::*;
    use thegn_core::issue::NewDispatch;

    fn claude_row() -> AgentDispatch {
        let db = thegn_core::db::Db::open_memory().expect("db");
        let id = db
            .put_agent_dispatch(NewDispatch::new("linear:THE-265", "/wt/265", "claude"))
            .expect("row");
        db.get_dispatch(id).expect("get").expect("row")
    }

    /// THE-265: with no proven native id the retry is a cold start; it never
    /// selects "latest" native history.
    #[test]
    fn retry_without_a_native_id_is_cold_never_continue_latest() {
        let launch = retry_open_spec(&claude_row(), "task".into(), None)
            .agent
            .expect("agent launch");
        assert!(!launch.continue_last, "continue-latest is unsafe");
        assert!(launch.resume.is_none());
        assert!(launch.native_session_id.is_none());
        assert!(!launch.fork);
        assert_eq!(launch.headless, Some(true));
    }

    /// THE-265: a proven native id resumes exactly that session — still never
    /// continue-latest.
    #[test]
    fn retry_with_a_native_id_resumes_exactly_that_session() {
        let launch = retry_open_spec(&claude_row(), "nudge".into(), Some("abc-123".into()))
            .agent
            .expect("agent launch");
        assert_eq!(launch.resume.as_deref(), Some("abc-123"));
        assert!(!launch.continue_last);
        assert!(!launch.fork);
    }

    #[test]
    fn only_a_resumable_valid_native_id_is_used() {
        let cfg = thegn_core::config::Config::default();
        assert_eq!(
            exact_resume_id(&cfg, "claude", Some("0c1f-uuid")).as_deref(),
            Some("0c1f-uuid")
        );
        assert_eq!(exact_resume_id(&cfg, "claude", None), None);
        assert_eq!(exact_resume_id(&cfg, "claude", Some("a b; rm -rf /")), None);
        assert_eq!(exact_resume_id(&cfg, "claude", Some("")), None);
        // aider has no resume-by-id: cold, never continue.
        assert_eq!(exact_resume_id(&cfg, "aider", Some("abc")), None);
        assert_eq!(exact_resume_id(&cfg, "no-such-agent", Some("abc")), None);
    }
}
