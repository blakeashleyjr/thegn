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
    /// Set while a reconcile runs, so concurrent triggers coalesce.
    reconciling: AtomicBool,
}

type BoxFut = Pin<Box<dyn Future<Output = ()> + Send>>;

/// The receive loop stays constant-time (THE-266): each exit is handed to a
/// bounded per-session task, then the loop returns to `recv` immediately. A
/// 60 s backoff on one row can no longer delay another row's stamp or
/// classification, and a lagged receiver reconciles from durable state instead
/// of only logging.
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
        reconciling: AtomicBool::new(false),
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

/// Constant-time handling of one exit: enqueue bounded per-session work.
/// Association, durable stamping, and classification share that work item.
fn observe_exit(obs: &Arc<Observer>, session: String, code: Option<i32>) {
    let Some(code) = code.filter(|c| *c != 0) else {
        // A clean exit is never retried: drop its assigned native id.
        obs.svc
            .native_ids
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .remove(&session);
        let svc = obs.svc.clone();
        let stamp_session = session.clone();
        tokio::spawn(async move {
            if let Err(e) = stamp_dispatch_exit(&svc, stamp_session.clone(), code).await {
                tracing::warn!(target: "thegn::daemon", session = %stamp_session, "dispatch exit stamp: {e:#}");
            }
        });
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
            // Queue capacity must not decide whether the durable exit stamp
            // is written. Reconciliation can recover classification later.
            let svc = obs.svc.clone();
            let stamp_session = session.clone();
            tokio::spawn(async move {
                if let Err(e) = stamp_dispatch_exit(&svc, stamp_session.clone(), Some(code)).await {
                    tracing::warn!(
                        target: "thegn::daemon",
                        session = %stamp_session,
                        "dispatch exit stamp: {e:#}"
                    );
                }
            });
            return;
        }
        pending.insert(session.clone());
    }
    let obs = obs.clone();
    tokio::spawn(async move {
        // The permit is taken inside `handle_exit`, only around backoff and
        // relaunch: classification, parking and persisting the native id never
        // wait behind other rows' retries.
        if let Err(e) =
            handle_exit(&obs.svc, &session, code, &obs.attempts, Some(&obs.permits)).await
        {
            // best-effort: a failed retry cycle must not kill the observer —
            // the note column records what it could.
            tracing::warn!(
                target: "thegn::daemon",
                session = %session,
                code,
                "transport retry: {e:#}"
            );
        }
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
        if obs.reconciling.swap(true, Ordering::SeqCst) {
            return; // one already running: it reads the same durable state
        }
        reconcile_pass(&obs).await;
        obs.reconciling.store(false, Ordering::SeqCst);
    })
}

async fn reconcile_pass(obs: &Arc<Observer>) {
    {
        let rows = match obs.svc.with_db(|db| db.list_dispatches()).await {
            Ok(rows) => rows,
            Err(e) => {
                tracing::warn!(target: "thegn::daemon", "exit reconcile: {e:#}");
                return;
            }
        };
        for row in rows {
            // Only a row still `running` can hold a missed exit: a parked or
            // spawning row was already handled by a retry cycle.
            if row.status != AgentDispatchStatus::Running || row.exited_at_ms.is_some() {
                continue;
            }
            let Some(session) = row.session_id.filter(|s| !s.is_empty()) else {
                continue;
            };
            if let Some(tomb) = obs.svc.tombstone(&session).await {
                observe_exit(obs, session, tomb.exit_code);
            }
        }
    }
}

/// Associate and persist a daemon session exit on its dispatch row. The
/// returned run reference is the fence used by classification, so the stamp
/// and retry decision cannot resolve different rows or generations. The
/// bounded async lookup closes the server-id publication race without blocking
/// the event receiver or unrelated exits.
pub(crate) async fn stamp_dispatch_exit(
    svc: &DaemonService,
    session: String,
    code: Option<i32>,
) -> anyhow::Result<Option<thegn_core::issue::DispatchRunRef>> {
    const LOOKUP_ATTEMPTS: usize = 20;
    const LOOKUP_RETRY: Duration = Duration::from_millis(25);
    const LOOKUP_DEADLINE: Duration = Duration::from_millis(500);
    let deadline = tokio::time::Instant::now() + LOOKUP_DEADLINE;

    for attempt in 0..LOOKUP_ATTEMPTS {
        let sid = session.clone();
        // Session identity only (THE-238): a miss is Stale, usually the
        // publication race this bounded loop exists to close.
        let attribution = match tokio::time::timeout_at(
            deadline,
            svc.with_db(move |db| db.dispatch_for_exit("", Some(&sid))),
        )
        .await
        {
            Ok(result) => result?,
            Err(_) => break,
        };
        if let thegn_core::issue::ExitAttribution::Exact(run) = attribution {
            let outcome = match tokio::time::timeout_at(
                deadline,
                svc.with_db({
                    let run = run.clone();
                    move |db| db.stamp_dispatch_exit(&run, code.map(i64::from))
                }),
            )
            .await
            {
                Ok(result) => result?,
                Err(_) => break,
            };
            if matches!(
                outcome,
                thegn_core::issue::ExitStamp::Stamped
                    | thegn_core::issue::ExitStamp::AlreadyStamped
            ) {
                return Ok(Some(run));
            }
            tracing::warn!(
                target: "thegn::daemon",
                session = %session,
                ?outcome,
                "session exit did not stamp its run"
            );
            return Ok(None);
        }
        if attempt + 1 < LOOKUP_ATTEMPTS {
            tokio::time::sleep_until((tokio::time::Instant::now() + LOOKUP_RETRY).min(deadline))
                .await;
        }
    }
    tracing::warn!(
        target: "thegn::daemon",
        session = %session,
        attempts = LOOKUP_ATTEMPTS,
        "session exit was not associated before lookup deadline"
    );
    Ok(None)
}

/// Handle one nonzero headless exit. Split from [`spawn`] so a stub test can
/// drive a synthetic exit through the same path with no PTY and no harness.
pub(crate) async fn handle_exit(
    svc: &DaemonService,
    session: &str,
    code: i32,
    attempts: &Attempts,
    permits: Option<&tokio::sync::Semaphore>,
) -> anyhow::Result<()> {
    // Associate and stamp first. Classification below uses this exact row/run
    // identity, never a second session lookup with a different timing window.
    let Some(run) = stamp_dispatch_exit(svc, session.to_string(), Some(code)).await? else {
        svc.native_ids
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .remove(session);
        return Ok(());
    };
    // Consume the launch-assigned native id up front, so no later return leaves
    // an entry behind.
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

    // 2. Read the row by the identity that was stamped. Recheck the current
    //    run reference in the same DB closure so a concurrent redrive cannot
    //    make this old exit act on its replacement.
    let run_id = run.id;
    let exact_run = run.clone();
    let assigned_native = assigned.clone();
    let (row, native) = svc
        .with_db(move |db| {
            let Some(current) = db.dispatch_run_ref(run_id)? else {
                return Ok((None, None));
            };
            if current != exact_run {
                return Ok((None, None));
            }
            let row = db.get_dispatch(run_id)?;
            if row.as_ref().is_some_and(|r| r.status.is_terminal()) {
                return Ok((None, None));
            }
            if let Some(native) = &assigned_native {
                db.set_dispatch_native_session(&current, native)?;
            }
            let native = db.dispatch_native_session(&current)?;
            Ok((row, native))
        })
        .await?;
    let Some(row) = row else {
        return Ok(());
    };

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
    let mut native = native;
    let Some(class) = retry_class(
        pipeline_exit::classify_tail(true, &screen, &sig, pipeline_exit::CLASSIFY_TAIL_LINES),
        &screen,
        &mut native,
    ) else {
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
            park(svc, &run, row.status, &note).await?;
        }
        pipeline_exit::RetryDecision::Exhausted { note } => {
            forget(attempts, row.id);
            park(svc, &run, row.status, &note).await?;
        }
        pipeline_exit::RetryDecision::Retry { attempt, delay_ms } => {
            let note = pipeline_exit::retry_note(signature_of(&class), attempt, tr.max_attempts);
            if !park(svc, &run, row.status, &note).await? {
                // The row moved (verdict or a newer run): leave its budget be.
                forget_if_current(svc, attempts, &run).await;
                return Ok(());
            }
            tracing::info!(
                target: "thegn::daemon",
                row = row.id,
                attempt,
                delay_ms,
                "transport failure on a headless dispatch; relaunching"
            );
            // Bound concurrent backoff + relaunch work (held to function end).
            // `Result::ok(..)`: the permit is used, not ignored.
            let _permit = match permits {
                Some(p) => Result::ok(p.acquire().await),
                None => None,
            };
            tokio::time::sleep(Duration::from_millis(delay_ms)).await;
            // Reserve the row before opening the replacement, fenced on the
            // exact run that was parked: a re-drive during the backoff replaced
            // the session and bumped `run_gen`, so this matches nothing and the
            // newer run is never relaunched over.
            // Register before the DB CAS so the reaper cannot observe the
            // newly-spawning row in the gap before this daemon marks it live.
            let _spawning_guard = svc.track_spawning_relaunch(row.id);
            let reserved = {
                let run = run.clone();
                svc.with_db(move |db| {
                    db.compare_and_set_dispatch_status_run(
                        &run,
                        AgentDispatchStatus::WaitingHuman,
                        AgentDispatchStatus::Spawning,
                        None,
                    )
                })
                .await?
            };
            if !reserved {
                forget_if_current(svc, attempts, &run).await;
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
                    park(svc, &run, AgentDispatchStatus::Spawning, &failed_note).await?;
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
    run: &thegn_core::issue::DispatchRunRef,
    expected: AgentDispatchStatus,
    note: &str,
) -> anyhow::Result<bool> {
    let note = note.to_string();
    let run = run.clone();
    svc.with_db(move |db| db.compare_and_set_dispatch_retry_park_run(&run, expected, &note))
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

/// Output of a resumed claude whose transcript no longer exists.
const NO_CONVERSATION: &str = "No conversation found";

/// The class a retry acts on. A resumed run that died because its conversation
/// is gone (provider pruning) is a recoverable transport-like failure that must
/// be retried COLD, so the stale id is dropped; it never parks the row.
fn retry_class(
    class: Option<pipeline_exit::ExitClass>,
    screen: &str,
    native: &mut Option<String>,
) -> Option<pipeline_exit::ExitClass> {
    if native.is_some() && screen.contains(NO_CONVERSATION) {
        *native = None;
        return class.or(Some(pipeline_exit::ExitClass::Transport {
            signature: NO_CONVERSATION.to_string(),
        }));
    }
    class
}

/// Drop a row's retry budget only when the row still carries the run this
/// task handled, so a stale task can never reset a newer run's budget.
async fn forget_if_current(
    svc: &DaemonService,
    attempts: &Attempts,
    run: &thegn_core::issue::DispatchRunRef,
) {
    let id = run.id;
    let current = svc.with_db(move |db| db.dispatch_run_ref(id)).await;
    if matches!(current, Ok(Some(ref c)) if c == run) {
        forget(attempts, run.id);
    }
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
    use crate::daemon::service::DaemonService;
    use crate::daemon::session::{IdleTransition, SessionMeta};
    use crate::daemon::tombstone::{MAX_TOMBSTONES, TOMBSTONE_TTL_MS, Tombstone};
    use tempfile::TempDir;
    use thegn_core::control_wire::EventFrame;
    use thegn_core::issue::NewDispatch;
    use thegn_core::store::NotificationStore;
    use thegn_core::{db::Db, graveyard::Graveyard, issue::AgentDispatchStatus as St};
    use tokio::sync::{broadcast, mpsc};

    fn test_service(config: thegn_core::config::Config) -> (Arc<DaemonService>, TempDir) {
        let profile = tempfile::Builder::new()
            .prefix("pipeline-retry-")
            .tempdir()
            .expect("private profile tempdir");
        thegn_core::fsperm::ensure_private_dir(profile.path())
            .expect("private profile permissions");
        let (events, _rx) = broadcast::channel(16);
        let (idle_tx, _idle_rx) = mpsc::unbounded_channel::<IdleTransition>();
        let svc = Arc::new(DaemonService {
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
            events,
            db: Arc::new(Mutex::new(Db::open_memory().expect("db"))),
            grace_ms: 0,
            idle_tx,
            shutdown: Arc::new(tokio::sync::Notify::new()),
            config: Arc::new(config),
            profile_root: profile.path().to_path_buf(),
            endpoint: "/tmp/test.sock".into(),
        });
        (svc, profile)
    }

    async fn bury(svc: &DaemonService, session: &str, screen: &[u8], code: i32) {
        let tomb = Tombstone {
            meta: SessionMeta {
                id: session.into(),
                worktree: None,
                program: "aider".into(),
                cwd: None,
                created_at_ms: 0,
                pid: None,
                forked_from: None,
            },
            exit_code: Some(code),
            exited_at_ms: 1,
            final_screen: EventFrame::PaneSnapshot {
                session: session.into(),
                seq: 0,
                cols: 80,
                rows: 24,
                bytes: screen.to_vec(),
            },
            history_tail: Vec::new(),
            last_state: thegn_core::attention::PaneAgentState::Idle,
            attached: 0,
            rows: 24,
            cols: 80,
            recording: None,
        };
        let buried_at_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("system clock is after the Unix epoch")
            .as_millis() as i64;
        svc.tombs
            .lock()
            .await
            .insert(session.into(), tomb, buried_at_ms);
    }

    fn running_row(svc: &DaemonService, issue: &str, agent: &str) -> i64 {
        let db = svc.db.lock().unwrap();
        let id = db
            .put_agent_dispatch(NewDispatch {
                stage: Some("code"),
                ..NewDispatch::new(issue, "/wt/257", agent)
            })
            .expect("row");
        db.update_dispatch_status(id, St::Running)
            .expect("running status");
        id
    }

    async fn publish_late(
        svc: &Arc<DaemonService>,
        row_id: i64,
        session: &'static str,
        attempts: &Arc<Attempts>,
    ) {
        let task_svc = svc.clone();
        let task_attempts = attempts.clone();
        let task =
            tokio::spawn(
                async move { handle_exit(&task_svc, session, 1, &task_attempts, None).await },
            );
        {
            let db = svc.db.lock().unwrap();
            let row = db.get_dispatch(row_id).expect("get row").expect("row");
            assert_eq!(row.session_id, None, "exit starts before publication");
            assert_eq!(row.exit_code, None, "exit has not been stamped yet");
        }
        tokio::time::sleep(Duration::from_millis(60)).await;
        let db = svc.db.lock().unwrap();
        assert!(
            db.bind_dispatch_session(row_id, session)
                .expect("publish session")
        );
        drop(db);
        task.await.expect("exit task").expect("handle exit");
    }

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

    #[test]
    fn a_vanished_conversation_forces_a_cold_retry() {
        let mut native = Some("gone".to_string());
        let class = retry_class(
            None,
            "Error: No conversation found with session ID",
            &mut native,
        );
        assert!(matches!(
            class,
            Some(pipeline_exit::ExitClass::Transport { .. })
        ));
        assert_eq!(native, None, "the stale id is dropped");
        // Without a resumed id the text is not special.
        let mut none = None;
        assert!(retry_class(None, "No conversation found", &mut none).is_none());
        // Unrelated failures leave the id and the classification alone.
        let mut keep = Some("id".to_string());
        assert!(retry_class(None, "segfault", &mut keep).is_none());
        assert_eq!(keep.as_deref(), Some("id"));
    }

    /// THE-257: a retryable exit before roster publication is stamped and
    /// classified, then one failed relaunch parks the row with its retry note.
    #[tokio::test]
    async fn retryable_exit_before_publication_is_stamped_and_relaunches_once() {
        let mut config = thegn_core::config::Config::default();
        config.pipeline.transport_retry.backoff_ms = 0;
        config.pipeline.transport_retry.max_attempts = 3;
        config.pipeline.transport_retry.transport_signatures = vec!["Connection error".into()];
        let (svc, _profile) = test_service(config);
        let row_id = running_row(&svc, "linear:THE-257", "no-such-agent");
        let session = "s-retryable-late";
        bury(
            &svc,
            session,
            b"Connection error. SDK retry budget exhausted",
            1,
        )
        .await;
        let attempts = Arc::new(Mutex::new(HashMap::new()));

        publish_late(&svc, row_id, session, &attempts).await;

        let row = svc
            .db
            .lock()
            .unwrap()
            .get_dispatch(row_id)
            .unwrap()
            .unwrap();
        assert_eq!(row.exit_code, Some(1), "exit is durably stamped");
        assert!(row.exited_at_ms.is_some());
        assert_eq!(row.status, St::WaitingHuman, "failed relaunch is parked");
        let note = row.note.as_deref().unwrap_or_default();
        assert!(
            note.contains("(attempt 1/3)"),
            "retry attempt is recorded: {note}"
        );
        assert!(
            note.contains("relaunch failed"),
            "failed relaunch is recorded: {note}"
        );
        assert!(
            attempts.lock().unwrap().is_empty(),
            "completed attempt is forgotten"
        );
    }

    /// THE-257: transport exhaustion is stamped and parked after late publication.
    #[tokio::test]
    async fn exit_before_roster_publication_is_stamped_and_parked() {
        let mut config = thegn_core::config::Config::default();
        config.pipeline.transport_retry.max_attempts = 0;
        config.pipeline.transport_retry.backoff_ms = 0;
        let (svc, _profile) = test_service(config);
        let row_id = running_row(&svc, "linear:THE-257", "aider");
        let session = "s-published-late";
        bury(
            &svc,
            session,
            b"Connection error. SDK retry budget exhausted",
            1,
        )
        .await;
        let attempts = Arc::new(Mutex::new(HashMap::new()));

        publish_late(&svc, row_id, session, &attempts).await;

        let row = svc
            .db
            .lock()
            .unwrap()
            .get_dispatch(row_id)
            .unwrap()
            .unwrap();
        assert_eq!(row.exit_code, Some(1), "exit is durably stamped");
        assert!(row.exited_at_ms.is_some());
        assert_eq!(row.status, St::WaitingHuman, "transport failure classified");
        assert!(
            row.note
                .as_deref()
                .unwrap_or_default()
                .starts_with("transport")
        );
    }

    /// A usage limit is parked without consuming transport retry budget.
    #[tokio::test]
    async fn limit_exit_before_publication_is_stamped_and_parked() {
        let mut config = thegn_core::config::Config::default();
        config.pipeline.transport_retry.backoff_ms = 0;
        config.pipeline.transport_retry.limit_signatures = vec!["weekly limit".into()];
        let (svc, _profile) = test_service(config);
        let row_id = running_row(&svc, "linear:THE-257", "aider");
        let session = "s-limit-late";
        bury(&svc, session, b"You have hit your weekly limit", 1).await;
        let attempts = Arc::new(Mutex::new(HashMap::new()));

        publish_late(&svc, row_id, session, &attempts).await;

        let row = svc
            .db
            .lock()
            .unwrap()
            .get_dispatch(row_id)
            .unwrap()
            .unwrap();
        assert_eq!(row.status, St::WaitingHuman);
        assert!(
            row.note
                .as_deref()
                .unwrap_or_default()
                .starts_with("limit:")
        );
        assert_eq!(row.exit_code, Some(1));
        assert!(attempts.lock().unwrap().is_empty());
    }

    /// An unmatched exit is durably stamped and left to the supervisor.
    #[tokio::test]
    async fn unmatched_exit_before_publication_is_stamped_without_verdict() {
        let (svc, _profile) = test_service(thegn_core::config::Config::default());
        let row_id = running_row(&svc, "linear:THE-257", "aider");
        let session = "s-unmatched-late";
        bury(&svc, session, b"segfault", 1).await;
        let attempts = Arc::new(Mutex::new(HashMap::new()));

        publish_late(&svc, row_id, session, &attempts).await;

        let row = svc
            .db
            .lock()
            .unwrap()
            .get_dispatch(row_id)
            .unwrap()
            .unwrap();
        assert_eq!(row.exit_code, Some(1));
        assert!(row.exited_at_ms.is_some());
        assert_eq!(row.status, St::Running);
        assert_eq!(row.note, None);
        assert!(attempts.lock().unwrap().is_empty());
    }

    /// Publication and a terminal verdict in one DB lock leave the row
    /// terminal; the exit cannot associate, stamp, or retain its native id.
    #[tokio::test]
    async fn concurrent_terminal_verdict_wins_exit_before_publication() {
        let (svc, _profile) = test_service(thegn_core::config::Config::default());
        let row_id = running_row(&svc, "linear:THE-257", "aider");
        let session = "s-terminal-late";
        bury(&svc, session, b"Connection error", 1).await;
        svc.native_ids
            .lock()
            .unwrap()
            .insert(session.into(), "native-id".into());
        let attempts = Arc::new(Mutex::new(HashMap::new()));
        let task_svc = svc.clone();
        let task_attempts = attempts.clone();
        let started = tokio::time::Instant::now();
        let task =
            tokio::spawn(
                async move { handle_exit(&task_svc, session, 1, &task_attempts, None).await },
            );
        {
            let db = svc.db.lock().unwrap();
            let row = db.get_dispatch(row_id).unwrap().unwrap();
            assert_eq!(row.session_id, None);
            assert_eq!(row.exit_code, None);
        }
        tokio::time::sleep(Duration::from_millis(60)).await;
        assert!(
            !task.is_finished(),
            "exit remains pending through the publication window"
        );
        {
            let db = svc.db.lock().unwrap();
            assert!(db.bind_dispatch_session(row_id, session).unwrap());
            db.update_dispatch_status(row_id, St::Done).unwrap();
        }
        task.await.unwrap().unwrap();
        assert!(started.elapsed() <= Duration::from_millis(750));
        let row = svc
            .db
            .lock()
            .unwrap()
            .get_dispatch(row_id)
            .unwrap()
            .unwrap();
        assert_eq!(row.status, St::Done);
        assert_eq!(row.note, None);
        assert_eq!(row.exit_code, None);
        assert!(!svc.native_ids.lock().unwrap().contains_key(session));
    }

    /// Once generation A is stamped, hold the tombstone read while publishing
    /// generation B through the retry API. A's classifier then sees the fence
    /// change and cannot park or stamp B.
    #[tokio::test]
    async fn delayed_old_generation_exit_does_not_change_replacement() {
        let mut config = thegn_core::config::Config::default();
        config.pipeline.transport_retry.backoff_ms = 0;
        config.pipeline.transport_retry.transport_signatures = vec!["Connection error".into()];
        let (svc, _profile) = test_service(config);
        let row_id = running_row(&svc, "linear:THE-257", "aider");
        let session_a = "s-old-generation";
        bury(&svc, session_a, b"Connection error", 1).await;
        let attempts = Arc::new(Mutex::new(HashMap::new()));
        let tomb_guard = svc.tombs.lock().await;
        let task_svc = svc.clone();
        let task_attempts = attempts.clone();
        let started = tokio::time::Instant::now();
        let task = tokio::spawn(async move {
            handle_exit(&task_svc, session_a, 1, &task_attempts, None).await
        });
        {
            let row = svc
                .db
                .lock()
                .unwrap()
                .get_dispatch(row_id)
                .unwrap()
                .unwrap();
            assert_eq!(row.session_id, None, "old exit begins before publication");
            assert_eq!(row.exit_code, None);
        }
        tokio::time::sleep(Duration::from_millis(60)).await;
        {
            let db = svc.db.lock().unwrap();
            assert!(db.bind_dispatch_session(row_id, session_a).unwrap());
        }

        let stamp_deadline = tokio::time::Instant::now() + Duration::from_millis(300);
        loop {
            let stamped = svc
                .db
                .lock()
                .unwrap()
                .get_dispatch(row_id)
                .unwrap()
                .unwrap()
                .exit_code
                .is_some();
            if stamped {
                break;
            }
            assert!(
                tokio::time::Instant::now() < stamp_deadline,
                "generation A was not stamped"
            );
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        {
            let db = svc.db.lock().unwrap();
            db.update_dispatch_status(row_id, St::Spawning).unwrap();
            assert!(
                db.compare_and_set_dispatch_retry_run(
                    row_id,
                    St::Spawning,
                    "s-replacement",
                    "/wt/257/retry",
                )
                .unwrap()
            );
        }
        drop(tomb_guard);
        task.await.unwrap().unwrap();
        assert!(started.elapsed() <= Duration::from_millis(750));

        let row = svc
            .db
            .lock()
            .unwrap()
            .get_dispatch(row_id)
            .unwrap()
            .unwrap();
        assert_eq!(row.session_id.as_deref(), Some("s-replacement"));
        assert_eq!(row.status, St::Running);
        assert_eq!(row.exit_code, None);
        assert_eq!(row.exited_at_ms, None);
        assert_eq!(row.note, None);
        assert!(attempts.lock().unwrap().is_empty());
    }

    /// THE-257: a row that never publishes is unchanged, and association gives
    /// up within the existing 500 ms lookup deadline.
    #[tokio::test]
    async fn exit_without_a_published_row_gives_up_cleanly() {
        let (svc, _profile) = test_service(thegn_core::config::Config::default());
        let row_id = running_row(&svc, "linear:THE-257", "aider");
        {
            let db = svc.db.lock().unwrap();
            assert!(
                db.bind_dispatch_session(row_id, "s-still-published")
                    .unwrap()
            );
        }
        svc.native_ids
            .lock()
            .unwrap()
            .insert("s-never-published".into(), "native-id".into());
        let attempts = Mutex::new(HashMap::new());
        let started = tokio::time::Instant::now();
        handle_exit(&svc, "s-never-published", 1, &attempts, None)
            .await
            .unwrap();

        assert!(
            started.elapsed() <= Duration::from_millis(750),
            "unassociated exit exceeded its 500 ms lookup deadline"
        );
        let row = svc
            .db
            .lock()
            .unwrap()
            .get_dispatch(row_id)
            .unwrap()
            .unwrap();
        assert_eq!(row.status, St::Running, "unrelated row remains unchanged");
        assert_eq!(row.session_id.as_deref(), Some("s-still-published"));
        assert_eq!(row.exit_code, None);
        assert!(
            !svc.native_ids
                .lock()
                .unwrap()
                .contains_key("s-never-published"),
            "native id is consumed when association gives up"
        );
    }
}
