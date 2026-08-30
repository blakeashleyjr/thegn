//! The transport-error retry observer — the daemon's headless half of the
//! run-completion story (THE-86). `pty_drain` stamps *pane* exits and
//! explicitly refuses headless ones; a headless stage worker that dies of a
//! transport failure used to just leave a `running` row nobody would ever
//! touch. This task gives the daemon that half: on a `SessionExit` with a
//! nonzero code, classify the final screen (pure core:
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
//! # The daemon can park a row but never finish one
//!
//! Every outcome stamps `waiting_human` + a `note` on the SAME roster row —
//! never `done`, never `failed`. A retry re-stamps the row's session and
//! artifact (`stamp_dispatch_run`) and moves it back to `running`: one row
//! cycling through attempts, not a chain of rows.
//!
//! # Event-driven, zero timers while idle
//!
//! The task blocks on the event broadcast feed; no polling, no tickers. The
//! only sleep is the backoff between a transport failure and its relaunch.
//! Attempt counters live in this task's memory (keyed by roster row id,
//! surviving session-id changes): a daemon restart kills the sessions it
//! supervised, so there is nothing to retry across a restart — the durable
//! note column records what happened either way.

use anyhow::Context as _;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use thegn_core::control_wire::EventFrame;
use thegn_core::harness::HarnessCaps;
use thegn_core::issue::AgentDispatch;
use thegn_core::issue::AgentDispatchStatus;
use thegn_core::pipeline_exit::{self, ExitSignatures};
use thegn_core::pipeline_run;
use thegn_core::store::{NotificationStore, WorkspaceStore};
use thegn_svc::control::{AgentLaunch, ControlApi, OpenSpec, SessionInfo};

use super::service::DaemonService;

/// The task's db-read budget is one row lookup per nonzero exit — never a
/// scan; `dispatch_by_session` is an indexed-equivalent single-column match on
/// a small roster.
pub(crate) fn spawn(
    svc: Arc<DaemonService>,
    mut rx: tokio::sync::broadcast::Receiver<Arc<EventFrame>>,
) {
    tokio::spawn(async move {
        // Attempt counters, keyed by roster ROW id (a relaunch re-stamps the
        // session id, so the row id is the stable key). Cleared when the row
        // parks or exhausts: a human re-drive starts a fresh budget.
        let mut attempts: HashMap<i64, u32> = HashMap::new();
        loop {
            match rx.recv().await {
                Ok(frame) => {
                    if let EventFrame::SessionExit { session, code } = &*frame
                        && let Some(code) = *code
                        && code != 0
                        && let Err(e) = handle_exit(&svc, session, code, &mut attempts).await
                    {
                        // best-effort: a failed retry cycle must not kill the
                        // observer — the note column records what it could.
                        tracing::warn!(
                            target: "thegn::daemon",
                            session = %session,
                            code,
                            "transport retry: {e:#}"
                        );
                    }
                }
                // A lagging receiver skipped exits; the row keeps its state and
                // the Lead still sees it on the roster. The feed is bounded
                // generously, so this is a burst, not a design assumption.
                Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                    tracing::warn!(target: "thegn::daemon", skipped = n, "transport retry observer lagged");
                }
                Err(tokio::sync::broadcast::error::RecvError::Closed) => return,
            }
        }
    });
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
    attempts: &mut HashMap<i64, u32>,
) -> anyhow::Result<()> {
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
    let attempt = match attempts.get_mut(&row.id) {
        Some(a) => {
            *a += 1;
            *a
        }
        None => {
            attempts.insert(row.id, 1);
            1
        }
    };
    let decision = pipeline_exit::decide(&class, attempt, tr.max_attempts, tr.backoff_ms);

    match decision {
        pipeline_exit::RetryDecision::Park { note } => {
            attempts.remove(&row.id);
            park(svc, row.id, &note).await?;
        }
        pipeline_exit::RetryDecision::Exhausted { note } => {
            attempts.remove(&row.id);
            park(svc, row.id, &note).await?;
        }
        pipeline_exit::RetryDecision::Retry { attempt, delay_ms } => {
            let note = pipeline_exit::retry_note(signature_of(&class), attempt, tr.max_attempts);
            park(svc, row.id, &note).await?;
            tracing::info!(
                target: "thegn::daemon",
                row = row.id,
                attempt,
                delay_ms,
                "transport failure on a headless dispatch; relaunching"
            );
            tokio::time::sleep(Duration::from_millis(delay_ms)).await;
            // The backoff slept past the park: a verdict the Lead wrote on the
            // row meanwhile is NEWER than this retry plan and must win —
            // relaunching would clobber it and spawn a second worker over a
            // closed stage. Re-read and relaunch only a row still parked where
            // this task left it.
            let id_for_check = row.id;
            let still_parked = svc
                .with_db(move |db| {
                    Ok(db
                        .get_dispatch(id_for_check)?
                        .is_some_and(|r| r.status == AgentDispatchStatus::WaitingHuman))
                })
                .await?;
            if !still_parked {
                attempts.remove(&row.id);
                tracing::info!(
                    target: "thegn::daemon",
                    row = row.id,
                    "transport retry: the row was re-driven during backoff; relaunch skipped"
                );
                return Ok(());
            }
            match relaunch(svc, &row).await {
                Ok(info) => {
                    let artifact = row.artifact_path.clone().unwrap_or_default();
                    let id = row.id;
                    let session_id = info.id.clone();
                    svc.with_db(move |db| {
                        db.stamp_dispatch_run(id, &session_id, &artifact)?;
                        db.update_dispatch_status(id, AgentDispatchStatus::Running)
                    })
                    .await?;
                    tracing::info!(target: "thegn::daemon", row = row.id, session = %info.id, "relaunched");
                }
                Err(e) => {
                    // The row stays waiting_human with the attempt note; the
                    // failure is appended underneath it.
                    append_note(svc, row.id, &format!("relaunch failed: {e:#}")).await?;
                }
            }
        }
    }
    Ok(())
}

/// Stamp a row `waiting_human` with the given note — the observer's ONLY
/// status write. The daemon can park a row but never finish one.
async fn park(svc: &DaemonService, id: i64, note: &str) -> anyhow::Result<()> {
    let note = note.to_string();
    svc.with_db(move |db| {
        db.stamp_dispatch_note(id, &note)?;
        db.update_dispatch_status(id, AgentDispatchStatus::WaitingHuman)
    })
    .await?;
    Ok(())
}

/// Append a line under a row's existing note (read-modify-write), used for a
/// failed relaunch attempt.
async fn append_note(svc: &DaemonService, id: i64, line: &str) -> anyhow::Result<()> {
    let existing = svc
        .with_db(move |db| Ok(db.get_dispatch(id)?.and_then(|r| r.note)))
        .await?
        .unwrap_or_default();
    let mut note = existing;
    if !note.is_empty() {
        note.push('\n');
    }
    note.push_str(line);
    let note2 = note;
    svc.with_db(move |db| db.stamp_dispatch_note(id, &note2))
        .await?;
    Ok(())
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

/// Relaunch a failed row: through [`DaemonService::open`], so the relaunch
/// takes the same sandbox/credential/cap/seeder path every launch takes.
///
/// - A harness with a `CONTINUE` cap relaunches with its id-free continue
///   form, seeded with the nudge as the opening message.
/// - Anything else relaunches COLD with the stage prompt re-rendered through
///   the shared helpers — the CLI dispatch path and this path render
///   identically by construction.
async fn relaunch(svc: &DaemonService, row: &AgentDispatch) -> anyhow::Result<SessionInfo> {
    let cfg = svc.config.clone();
    let harness = crate::daemon::agent_open::harness_for_agent(&cfg, &row.agent_name)
        .with_context(|| format!("unknown agent `{}` — cannot relaunch", row.agent_name))?;
    let (prompt, continue_last) = if harness.caps().contains(HarnessCaps::CONTINUE) {
        (pipeline_exit::RETRY_NUDGE.to_string(), true)
    } else {
        (cold_stage_prompt(svc, row).await?, false)
    };
    let spec = OpenSpec {
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
            resume: None,
            continue_last,
            stage: row.stage.clone(),
            fork: false,
            native_session_id: None,
        }),
        adopt: false,
        already_capped: false,
    };
    svc.open(spec)
        .await
        .map_err(|e| anyhow::anyhow!("open: {e}"))
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
            .issues_get(&row.issue_id)
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
    // still relaunches, just without `{branch}` in its prompt.
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
