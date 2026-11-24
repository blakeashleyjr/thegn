//! The PR queue's in-app surface: off-loop spawners, the channel drain that
//! patches panel rows live, and the section's key table.
//!
//! Deliberately the same shape as [`crate::handlers::merge_queue`] — every
//! mutation runs on `spawn_blocking` and reports back over a channel with a
//! waker pulse, so the compositor loop never touches the network or the DB.

use std::path::PathBuf;
use std::time::Instant;
use tokio::sync::mpsc as tokio_mpsc;

use termwiz::terminal::TerminalWaker;
use thegn_core::config::Config;
use thegn_core::db::{Db, PrQueueRow};
use thegn_core::pr_queue::PrqStatus;
use thegn_core::store::WorktreeAuxStore;

use crate::hydrate::RefreshKind;
use crate::pr_driver::{self, PrItem, PrOutcome};
use crate::toast::Toasts;

pub(crate) type PrqTx = tokio_mpsc::UnboundedSender<PrqMsg>;
pub(crate) type PrqRx = tokio_mpsc::UnboundedReceiver<PrqMsg>;

/// What the off-loop PR-queue work reports back.
pub(crate) enum PrqMsg {
    /// One driver status transition (the DB row is already written when this
    /// fires) — the loop patches the panel row in place for a live repaint.
    Step {
        key: String,
        number: u64,
        branch: String,
        status: String,
        detail: String,
    },
    /// A refresh pass finished; clears the inflight flag and toasts the summary.
    Done(Box<PrOutcome>),
    /// A one-line outcome from a one-shot mutation (add / remove / clear).
    Note(String),
    /// One create/revision event whose roster upsert and inbox audit are already
    /// durable. The loop publishes it without dispatching an agent.
    ReviewTask {
        event: Box<thegn_core::pr_review_tasks::ReviewTaskEvent>,
        revised: bool,
    },
    /// The pass (or a pre-pass step) failed outright.
    Failed(String),
}

/// The loop locals the channel drain mutates, borrowed for one pass.
pub(crate) struct PrqDrainCtx<'a> {
    pub model: &'a mut crate::chrome::FrameModel,
    pub toasts: &'a mut Toasts,
    pub notify_state: &'a crate::notify::NotifyState,
    pub event_bus: &'a thegn_core::event_bus::EventBus,
    pub inflight: &'a mut bool,
    pub want_model_refresh: &'a mut bool,
    pub loop_perf: &'a mut crate::perf::LoopPerf,
}

/// Route a settled transition to the notification bus + inbox, mirroring the
/// merge queue's `notify_queue`. The source ref is the PR key, so repeated
/// transitions on one pull request coalesce rather than piling up.
fn notify_prq(
    ctx: &mut PrqDrainCtx,
    kind: thegn_core::notification::NotificationKind,
    key: &str,
    worktree: &str,
    message: String,
) {
    let dec = crate::notify::route(ctx.notify_state, kind.as_str(), key, &message, worktree);
    if dec.desktop {
        let n = thegn_core::notification::Notification {
            id: 0,
            kind,
            source_ref: key.to_string(),
            message: message.clone(),
            created_at_ms: thegn_core::util::now(),
            read: false,
            worktree_path: worktree.to_string(),
        };
        ctx.event_bus.publish_with_notification(
            &thegn_core::event_bus::Event::NotificationReceived { notification: n },
        );
    }
    let (k, src, wt, msg) = (
        kind.as_str(),
        key.to_string(),
        worktree.to_string(),
        message,
    );
    let routed = dec.clone();
    tokio::task::spawn_blocking(move || {
        let Ok(db) = Db::open() else { return };
        if routed.record {
            // best-effort: the inbox is a cache; the queue row is the record.
            let _ = crate::automation_events::insert_routed(
                &db,
                k,
                &src,
                &msg,
                &wt,
                Default::default(),
                &routed,
                false,
            );
        }
        // This is a typed queue fact, not a consequence of inbox visibility.
        // A notification drop must not hide a real merge edge.
        if k == "pr_queue_merged" {
            let origin = crate::automation_events::take_merge_origin(&db, &wt);
            crate::automation_events::submit_fact(
                thegn_core::automation::AutomationEventKind::MergeLanded,
                format!("{src}:merged"),
                Some(wt),
                Some(msg),
                crate::automation_events::EventFacts {
                    pr_merged: Some(true),
                    origin,
                    ..Default::default()
                },
            );
        }
    });
}

/// The durable half of a settled PR queue transition for a pass with no UI
/// drain (the daemon, THE-2211): the inbox row through the canonical routed
/// emitter, and the typed `merge_landed` fact for a merge. Mirrors
/// [`notify_prq`]'s blocking half; transient statuses record nothing.
pub(crate) fn record_settled_step(
    db: &Db,
    key: &str,
    worktree: &str,
    number: u64,
    branch: &str,
    status: &str,
    detail: &str,
) {
    use thegn_core::notification::NotificationKind as K;
    let (kind, message) = match status {
        "merged" => (K::PrQueueMerged, format!("PR #{number} ({branch}) merged")),
        "ready" => (
            K::PrQueueReady,
            format!("PR #{number} ({branch}) is ready to merge"),
        ),
        "needs_human" => (
            K::PrQueueNeedsHuman,
            format!("PR #{number} needs you — {detail}"),
        ),
        _ => return,
    };
    if let Err(error) = crate::automation_events::emit(db, kind.as_str(), key, &message, worktree) {
        tracing::debug!(target: "thegn::prq", %error, "daemon pr queue notification not recorded");
    }
    if status == "merged" {
        let origin = crate::automation_events::take_merge_origin(db, worktree);
        crate::automation_events::submit_fact(
            thegn_core::automation::AutomationEventKind::MergeLanded,
            format!("{key}:merged"),
            Some(worktree.to_string()),
            Some(message),
            crate::automation_events::EventFacts {
                pr_merged: Some(true),
                origin,
                ..Default::default()
            },
        );
    }
}

// ---------------------------------------------------------------------------
// Off-loop spawners
// ---------------------------------------------------------------------------

/// Kick one refresh pass off the loop: collect the repo's active rows, run
/// [`pr_driver::drive_queue`] (which talks to the forge and may dispatch a
/// headless agent), and stream every transition back.
pub(crate) fn spawn_drive(tx: &PrqTx, waker: &TerminalWaker, cfg: Config, any_path: PathBuf) {
    let tx = tx.clone();
    let waker = waker.clone();
    tokio::task::spawn_blocking(move || {
        drive_blocking(&cfg, &any_path, DriveMode::Ui, |m: PrqMsg| {
            if tx.send(m).is_ok() {
                let _ = waker.wake(); // best-effort: waker pulse: an input nudge must never fail the calling path
            }
        });
    });
}

/// Who runs a PR queue pass (THE-2211).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DriveMode {
    /// The UI's ticker / `D` / palette pass. Unchanged behaviour, except that
    /// while the daemon owns the PR queue producer it only reloads.
    Ui,
    /// The daemon's headless producer pass: an agent handoff happens only when
    /// `[pr_queue] agent_sandbox` is on and the agent clears a shared-kernel
    /// (or stricter) floor with a fail-closed miss; otherwise the blocker is
    /// reported, never fixed (the notify degrade).
    Daemon,
}

/// Whether a UI pass yields to the daemon: it skips every remote call and only
/// reloads the panel from the DB cache the daemon writes.
fn ui_yields_to_daemon(mode: DriveMode, daemon_owns: bool) -> bool {
    mode == DriveMode::Ui && daemon_owns
}

/// The queue policy a daemon pass runs under, from the repo-resolved policy
/// `pq` and the global `[pr_queue]` block. Pure.
///
/// - Agent sandbox off (globally or for the repo): no agent is resolved
///   (`agent` and `agent_command` cleared), so the driver reports the blocker
///   and never dispatches — it degrades to notify. A daemon pass never runs an
///   agent on the host.
/// - Agent sandbox on: the floor is the strictest of the global floor, the
///   repo floor and shared-kernel, and a miss fails closed (the driver's
///   `agent_floor_gate` holds the entry; no agent starts).
fn daemon_queue_policy(
    global: &thegn_core::config::PrQueueConfig,
    pq: &thegn_core::config::PrQueueConfig,
) -> thegn_core::config::PrQueueConfig {
    use thegn_core::config::{IsolationFloor, OnFloorMiss};
    let mut out = pq.clone();
    if !(global.agent_sandbox && pq.agent_sandbox) {
        out.agent.clear();
        out.agent_command.clear();
        out.agent_sandbox = false;
        return out;
    }
    let rank = |floor: IsolationFloor| floor.required_rank().unwrap_or(0);
    let floor = [
        global.agent_isolation_floor,
        pq.agent_isolation_floor,
        IsolationFloor::SharedKernel,
    ]
    .into_iter()
    .max_by_key(|floor| rank(*floor))
    .unwrap_or(IsolationFloor::SharedKernel);
    out.agent_sandbox = true;
    out.agent_isolation_floor = floor;
    out.agent_on_floor_miss = OnFloorMiss::Fail;
    out
}

/// The blocking body of a PR queue pass: [`spawn_drive`] runs it in
/// [`DriveMode::Ui`], the daemon's PR queue producer in [`DriveMode::Daemon`].
/// Separated so the entry gates are testable without a runtime.
pub(crate) fn drive_blocking(
    cfg: &Config,
    any_path: &std::path::Path,
    mode: DriveMode,
    send: impl Fn(PrqMsg),
) {
    {
        let Some(root) = crate::integrate::main_checkout(any_path) else {
            send(PrqMsg::Failed("not inside a git repository".into()));
            return;
        };
        // THE-515: the ticker (and `D`/palette) are armed on the GLOBAL
        // `[pr_queue] enabled`; the repo's resolved policy decides here. A
        // refused trusted overlay is reported; a queue the repo turned off is
        // a quiet no-op. Neither talks to the forge or dispatches an agent.
        if let Some(refusal) = cfg.workspace_overlay_refusal(&root) {
            send(PrqMsg::Failed(format!("{}: {refusal}", root.display())));
            return;
        }
        // Repo-resolved inside the blocking task: the loop must not pay for the
        // git call that derives the workspace slug.
        let pq = cfg.repo_pr_queue(&root);
        if !pq.enabled {
            send(PrqMsg::Done(Box::default()));
            return;
        }
        // THE-2211: while the daemon holds the PR queue lease, a UI pass makes
        // no forge call and dispatches nothing; `Done` reloads the panel from
        // the DB rows the daemon writes.
        if ui_yields_to_daemon(
            mode,
            crate::producer_yield::daemon_owns(thegn_core::producer_lease::ProducerKind::PrQueue),
        ) {
            send(PrqMsg::Done(Box::default()));
            return;
        }
        let pq = match mode {
            DriveMode::Ui => pq,
            DriveMode::Daemon => daemon_queue_policy(&cfg.pr_queue, &pq),
        };
        let db = match Db::open() {
            Ok(d) => d,
            Err(e) => {
                send(PrqMsg::Failed(format!("db: {e}")));
                return;
            }
        };
        let items: Vec<PrItem> = pr_driver::rows_for_repo(&db, &root)
            .iter()
            .filter(|r| !matches!(r.status.as_str(), "merged" | "closed" | "needs_human"))
            .map(PrItem::from)
            .collect();
        if items.is_empty() {
            send(PrqMsg::Done(Box::default()));
            return;
        }
        let forges = crate::forge_handle::get();
        let forge = forges.for_loc(&thegn_core::remote::GitLoc::from_db(
            &root.to_string_lossy(),
            None,
        ));
        let out = pr_driver::drive_queue(&pq, cfg, forge, &root, &db, items, |s| {
            send(PrqMsg::Step {
                key: s.key.to_string(),
                number: s.number,
                branch: s.branch.to_string(),
                status: s.status.to_string(),
                detail: s.detail.to_string(),
            });
            // `drive_queue` invokes this callback on the blocking worker. Only
            // an observed `merged` row closes an autopilot run; the transient
            // "merge requested"/ready states do not.
            if s.status == "merged" {
                crate::autopilot_driver::on_pr_merged(cfg, &root, s.number);
            }
        });
        for (event, revised) in &out.review_events {
            send(PrqMsg::ReviewTask {
                event: Box::new(event.clone()),
                revised: *revised,
            });
        }
        send(PrqMsg::Done(Box::new(out)));
    }
}

/// Queue the current worktree's pull request, off the loop (it asks the forge).
pub(crate) fn spawn_add(tx: &PrqTx, waker: &TerminalWaker, worktree: PathBuf) {
    let tx = tx.clone();
    let waker = waker.clone();
    tokio::task::spawn_blocking(move || {
        let send = |m: PrqMsg| {
            if tx.send(m).is_ok() {
                let _ = waker.wake(); // best-effort: waker pulse: an input nudge must never fail the calling path
            }
        };
        let Some(root) = crate::integrate::main_checkout(&worktree) else {
            send(PrqMsg::Failed("not inside a git repository".into()));
            return;
        };
        let wt = worktree.to_string_lossy().into_owned();
        let loc = thegn_core::remote::GitLoc::from_db(&wt, None);
        let forges = crate::forge_handle::get();
        let forge = forges.for_loc(&loc);
        let pr = match forge.pr_status(&loc, thegn_core::forge::PrRef::Current) {
            Ok(pr) => pr,
            Err(e) => {
                send(PrqMsg::Failed(e.describe()));
                return;
            }
        };
        let db = match Db::open() {
            Ok(d) => d,
            Err(e) => {
                send(PrqMsg::Failed(format!("db: {e}")));
                return;
            }
        };
        match db.enqueue_bound_pr(
            &root.to_string_lossy(),
            pr.number,
            Some(&wt),
            &pr.head_ref_name,
            &pr.base_ref_name,
            forge.id(),
            &pr.url,
        ) {
            Ok(()) => send(PrqMsg::Note(format!("Queued PR #{}", pr.number))),
            Err(e) => send(PrqMsg::Failed(format!("could not queue: {e}"))),
        }
    });
}

fn pr_removal_message(number: u64, outcome: thegn_core::store::PrRemoval) -> String {
    match outcome {
        thegn_core::store::PrRemoval::Removed => {
            format!("Removed PR #{number} from the queue.")
        }
        thegn_core::store::PrRemoval::Missing => format!("PR #{number} is no longer in the queue"),
        thegn_core::store::PrRemoval::AgentRunning => format!(
            "PR #{number} was not removed: a PR agent is running for it; remove it after the agent finishes"
        ),
    }
}

fn pr_clear_message(outcome: &thegn_core::store::PrClearOutcome) -> String {
    let mut message = format!("PR queue cleared ({} removed).", outcome.removed);
    if !outcome.kept_running.is_empty() {
        let kept = outcome
            .kept_running
            .iter()
            .map(|number| format!("#{number}"))
            .collect::<Vec<_>>()
            .join(", ");
        message.push_str(&format!(" kept PR {kept}: a PR agent is running for them"));
    }
    message
}

/// Remove one row, or clear the repo's queue, off the loop.
pub(crate) fn spawn_mutate(tx: &PrqTx, waker: &TerminalWaker, any_path: PathBuf, what: Mutation) {
    let tx = tx.clone();
    let waker = waker.clone();
    tokio::task::spawn_blocking(move || {
        let send = |m: PrqMsg| {
            if tx.send(m).is_ok() {
                let _ = waker.wake(); // best-effort: waker pulse: an input nudge must never fail the calling path
            }
        };
        let Some(root) = crate::integrate::main_checkout(&any_path) else {
            send(PrqMsg::Failed("not inside a git repository".into()));
            return;
        };
        let db = match Db::open() {
            Ok(d) => d,
            Err(e) => {
                send(PrqMsg::Failed(format!("db: {e}")));
                return;
            }
        };
        let root_s = root.to_string_lossy().into_owned();
        let msg = match what {
            Mutation::Remove { key, number } => match db.remove_pr_entry(&key) {
                Ok(
                    outcome @ (thegn_core::store::PrRemoval::AgentRunning
                    | thegn_core::store::PrRemoval::Missing),
                ) => {
                    send(PrqMsg::Failed(pr_removal_message(number, outcome)));
                    return;
                }
                Ok(outcome) => pr_removal_message(number, outcome),
                Err(error) => {
                    send(PrqMsg::Failed(format!("remove failed: {error}")));
                    return;
                }
            },
            Mutation::Clear => match db.clear_pr_queue(&root_s) {
                Ok(outcome) if outcome.kept_running.is_empty() => pr_clear_message(&outcome),
                Ok(outcome) => {
                    send(PrqMsg::Failed(pr_clear_message(&outcome)));
                    return;
                }
                Err(error) => {
                    send(PrqMsg::Failed(format!("clear failed: {error}")));
                    return;
                }
            },
            // Re-queue: the same "watch it again" reset the CLI's `add` performs,
            // which is how a `needs_human` row is re-armed.
            Mutation::Rewatch { key, number } => {
                let rows = match db.list_pr_queue() {
                    Ok(rows) => rows,
                    Err(error) => {
                        send(PrqMsg::Failed(format!("re-watch failed: {error}")));
                        return;
                    }
                };
                let Some(row) = rows.iter().find(|row| row.key == key) else {
                    send(PrqMsg::Note(format!("PR #{number} is no longer queued")));
                    return;
                };
                if row.repo_root != root_s || row.number != number {
                    send(PrqMsg::Note(format!(
                        "re-watch refused: PR #{number} is not queued for this repository"
                    )));
                    return;
                }
                if !matches!(row.status.as_str(), "needs_human" | "merged" | "closed") {
                    send(PrqMsg::Note(format!(
                        "re-watch refused: PR #{number} is no longer settled"
                    )));
                    return;
                }
                let loc = thegn_core::remote::GitLoc::from_db(&root_s, None);
                let forges = crate::forge_handle::get();
                let forge = forges.for_loc(&loc);
                let fetched = forge.pr_status(&loc, thegn_core::forge::PrRef::Number(number));
                rewatch_row(&db, row, forge.id(), fetched).unwrap_or_else(|message| message)
            }
        };
        send(PrqMsg::Note(msg));
    });
}

/// Handle one selected durable review task. The entire lifecycle (including
/// the agent run and provider rechecks) stays on this blocking worker.
pub(crate) fn spawn_handle(
    tx: &PrqTx,
    waker: &TerminalWaker,
    cfg: Config,
    task: crate::panel::ReviewTaskRow,
) {
    let tx = tx.clone();
    let waker = waker.clone();
    tokio::task::spawn_blocking(move || {
        let context = crate::review_task_handoff::HandleContext {
            task_id: task.id,
            pr_number: task.pr_number,
            repository: task.repository,
            thread_id: task.thread_id,
            path: task.path,
            line: task.line,
        };
        let message = crate::review_task_handoff::handle(&cfg, &context);
        if tx.send(PrqMsg::Note(message)).is_ok() {
            let _ = waker.wake(); // best-effort: completed blocking work must nudge the idle loop
        }
    });
}

/// A one-shot queue mutation.
pub(crate) enum Mutation {
    Remove { key: String, number: u64 },
    Clear,
    Rewatch { key: String, number: u64 },
}

/// Re-verify a held row against the forge selected from its repository root,
/// then bind the fetched identity while re-arming it.
fn rewatch_row(
    db: &Db,
    row: &PrQueueRow,
    forge_id: &str,
    fetched: Result<thegn_core::forge::model::PrStatus, thegn_core::forge::ForgeError>,
) -> Result<String, String> {
    let number = row.number;
    let fetched = fetched.map_err(|error| {
        format!(
            "re-watch failed: could not verify PR #{number}: {}",
            error.describe()
        )
    })?;
    if fetched.number != number
        || thegn_core::forge::model::repo_identity_from_pr_url(&fetched.url).is_none()
    {
        return Err(format!(
            "re-watch refused: fetched identity does not verify PR #{number}; use `thegn pr queue rebind --pr {number}`"
        ));
    }
    // Validate fetched URL route and number when an unbound row has no identity to compare against.
    if let Err(reason) = thegn_core::pr_queue::attest_pr_identity(
        Some(&fetched.url),
        number,
        &fetched.url,
        fetched.number,
    ) {
        return Err(format!(
            "re-watch refused: {reason}; use `thegn pr queue rebind --pr {number}`"
        ));
    }
    if row.pr_url.is_some() {
        if row.forge != forge_id {
            return Err(format!(
                "re-watch refused: queued forge `{}` differs from `{forge_id}`; use `thegn pr queue rebind --pr {number}`",
                row.forge
            ));
        }
        if let Err(reason) = thegn_core::pr_queue::attest_pr_identity(
            row.pr_url.as_deref(),
            number,
            &fetched.url,
            fetched.number,
        ) {
            return Err(format!(
                "re-watch refused: {reason}; use `thegn pr queue rebind --pr {number}`"
            ));
        }
    }
    let worktree = (fetched.head_ref_name == row.branch)
        .then_some(row.worktree.as_deref())
        .flatten();
    db.rebind_pr(
        &row.key,
        &thegn_core::db::PrRebind {
            worktree: worktree.map(str::to_owned),
            branch: fetched.head_ref_name,
            base_branch: fetched.base_ref_name,
            forge: forge_id.to_owned(),
            pr_url: fetched.url,
        },
    )
    .map_err(|error| format!("re-watch failed: {error}"))?;
    Ok(format!("Watching PR #{number} again"))
}

// ---------------------------------------------------------------------------
// Channel drain
// ---------------------------------------------------------------------------

/// Drain everything the off-loop work reported, patching panel rows in place so
/// a running pass paints live instead of waiting for the next model tick.
///
/// Budgeted per drain (THE-657): at most [`PR_QUEUE_SOURCE`]'s 64 messages or
/// the iteration deadline, whichever comes first, with at least one message
/// always taken. Nothing is dropped or reordered; what is left stays queued and
/// the returned `more_pending` tells the loop to re-arm one wake.
///
/// [`PR_QUEUE_SOURCE`]: crate::ingress_budget::PR_QUEUE_SOURCE
pub(crate) fn drain_msgs(
    rx: &mut PrqRx,
    ctx: &mut PrqDrainCtx,
    budget: &mut crate::ingress_budget::IterationBudget,
    rotation: &mut usize,
) -> bool {
    let (batch, more_pending) = crate::ingress_budget::take_batch(
        rx,
        crate::ingress_budget::PR_QUEUE_SOURCE,
        budget,
        rotation,
        Instant::now,
    );
    for msg in batch {
        ctx.loop_perf.tick(crate::perf::WakeSource::Fold);
        let now = std::time::Instant::now();
        match msg {
            PrqMsg::Step {
                key,
                number,
                branch,
                status,
                detail,
            } => {
                apply_step(&mut ctx.model.panel, &key, &status, &detail);
                // Only settled transitions are worth a toast; the transient ones
                // are already visible as a live row change.
                let wt = ctx
                    .model
                    .panel
                    .pr_queue
                    .iter()
                    .find(|r| r.key == key)
                    .and_then(|r| r.worktree.clone())
                    .unwrap_or_default();
                use thegn_core::notification::NotificationKind as K;
                match status.as_str() {
                    "merged" => {
                        ctx.toasts
                            .success(format!("PR #{number} ({branch}) merged"), now);
                        notify_prq(
                            ctx,
                            K::PrQueueMerged,
                            &key,
                            &wt,
                            format!("PR #{number} ({branch}) merged"),
                        );
                    }
                    "ready" => notify_prq(
                        ctx,
                        K::PrQueueReady,
                        &key,
                        &wt,
                        format!("PR #{number} ({branch}) is ready to merge"),
                    ),
                    "needs_human" => {
                        ctx.toasts.info_ttl(
                            format!("PR #{number} needs you — {detail}"),
                            now,
                            std::time::Duration::from_secs(8),
                        );
                        notify_prq(
                            ctx,
                            K::PrQueueNeedsHuman,
                            &key,
                            &wt,
                            format!("PR #{number} needs you — {detail}"),
                        );
                    }
                    _ => {}
                }
            }
            PrqMsg::Done(out) => {
                *ctx.inflight = false;
                for w in &out.warnings {
                    ctx.toasts
                        .info_ttl(w.clone(), now, std::time::Duration::from_secs(8));
                }
                let total = out.merged.len()
                    + out.merge_requested.len()
                    + out.auto_merge_armed.len()
                    + out.ready.len()
                    + out.blocked.len()
                    + out.needs_human.len()
                    + out.dropped.len();
                let msg = if total == 0 {
                    "PR queue: nothing to refresh".to_string()
                } else {
                    format!(
                        "PR queue: {} merged, {} merge requested, {} auto-merge armed, {} ready, {} blocked, {} need a human",
                        out.merged.len(),
                        out.merge_requested.len(),
                        out.auto_merge_armed.len(),
                        out.ready.len(),
                        out.blocked.len(),
                        out.needs_human.len()
                    )
                };
                if out.needs_human.is_empty() && out.merge_requested.is_empty() {
                    ctx.toasts.success(msg, now);
                } else {
                    ctx.toasts
                        .info_ttl(msg, now, std::time::Duration::from_secs(6));
                }
                *ctx.want_model_refresh = true;
            }
            PrqMsg::Note(m) => {
                ctx.toasts.success(m, now);
                *ctx.want_model_refresh = true;
            }
            PrqMsg::ReviewTask { event, revised } => {
                // Publish the typed automation event only after the worker has
                // confirmed the roster upsert. The inbox notification below is
                // a separate once-keyed attention/audit event.
                ctx.event_bus
                    .publish(&thegn_core::event_bus::Event::PrThreadUnresolved(Box::new(
                        (*event).clone(),
                    )));
                let kind = thegn_core::notification::NotificationKind::PrReviewTaskQueued;
                let message = thegn_core::notification::review_task_queued_message(&event, revised);
                let notification = thegn_core::notification::Notification {
                    id: 0,
                    kind,
                    source_ref: event.source_key.clone(),
                    message: message.clone(),
                    created_at_ms: thegn_core::util::now_ms(),
                    read: false,
                    worktree_path: event.worktree_path.clone(),
                };
                let decision = crate::notify::route(
                    ctx.notify_state,
                    kind.as_str(),
                    &event.source_key,
                    &message,
                    &event.worktree_path,
                );
                let bus_event = thegn_core::event_bus::Event::NotificationReceived { notification };
                if decision.desktop {
                    ctx.event_bus.publish_with_notification(&bus_event);
                } else {
                    ctx.event_bus.publish(&bus_event);
                }
                ctx.toasts
                    .info_ttl(message, now, std::time::Duration::from_secs(6));
                *ctx.want_model_refresh = true;
            }
            PrqMsg::Failed(m) => {
                *ctx.inflight = false;
                ctx.toasts
                    .info_ttl(m, now, std::time::Duration::from_secs(8));
                *ctx.want_model_refresh = true;
            }
        }
    }
    more_pending
}

/// Patch one panel row in place. Pure over `PanelData` so it is unit-testable
/// without a model tick.
pub(crate) fn apply_step(
    panel: &mut crate::panel::PanelData,
    key: &str,
    status: &str,
    detail: &str,
) {
    if let Some(row) = panel.pr_queue.iter_mut().find(|r| r.key == key) {
        row.status = status.to_string();
        row.detail = (!detail.is_empty()).then(|| detail.to_string());
        // A step into `agent_running` is the one transition that proves the
        // refusal was resolved: the durable write clears it in the admission
        // transaction, and this keeps the live row in step until the next model
        // refresh re-reads it.
        if status == PrqStatus::AgentRunning.as_str() {
            row.admission_refusal = None;
            row.admission_refusal_head = None;
        }
    }
}

// ---------------------------------------------------------------------------
// Section keys
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PrqAction {
    /// Queue the current worktree's PR.
    Add,
    /// Drop the cursor row.
    Remove,
    /// Re-arm a settled row (`needs_human`) so the next pass looks again.
    Rewatch,
    /// Empty the repo's queue.
    Clear,
    /// Run one refresh pass now.
    Refresh,
    /// Open the cursor row's PR in a browser.
    OpenInBrowser,
    /// Run one queued per-thread review task.
    HandleReview,
}

/// The section's key table. `Err` carries the status-line hint for a key that
/// doesn't apply to the cursor row. Pure, so the hint row in
/// `panel::sections::pr_queue` can be checked against it.
pub(crate) fn row_action_for(
    key: char,
    row_status: Option<&str>,
    task_status: Option<thegn_core::issue::AgentDispatchStatus>,
) -> Result<PrqAction, &'static str> {
    match key {
        'a' => Ok(PrqAction::Add),
        'D' => Ok(PrqAction::Refresh),
        'c' => Ok(PrqAction::Clear),
        'x' => row_status
            .map(|_| PrqAction::Remove)
            .ok_or("PR queue: no row selected"),
        'o' => row_status
            .map(|_| PrqAction::OpenInBrowser)
            .ok_or("PR queue: no row selected"),
        'r' => match row_status {
            None => Err("PR queue: no row selected"),
            // Only a settled row needs re-arming; an active one is already
            // being looked at every pass.
            Some("needs_human" | "merged" | "closed") => Ok(PrqAction::Rewatch),
            Some(_) => Err("PR queue: this row is already being watched"),
        },
        'h' => match task_status {
            Some(thegn_core::issue::AgentDispatchStatus::Queued) => Ok(PrqAction::HandleReview),
            Some(thegn_core::issue::AgentDispatchStatus::Running)
            | Some(thegn_core::issue::AgentDispatchStatus::Spawning) => {
                Err("review task is already running")
            }
            Some(_) => Err("review task is waiting for a new revision or human action"),
            None => Err("PR queue: select a review task to handle"),
        },
        _ => Err(""),
    }
}

/// The loop locals the section keys need, borrowed for one keypress.
pub(crate) struct PrqKeyCtx<'a> {
    pub model: &'a mut crate::chrome::FrameModel,
    pub cfg: &'a Config,
    /// The active tab's worktree path (the `a` target and the repo anchor).
    pub active_wt: PathBuf,
    pub refresh_tx: &'a tokio_mpsc::UnboundedSender<RefreshKind>,
    pub waker: &'a TerminalWaker,
    pub tx: &'a PrqTx,
    pub inflight: &'a mut bool,
    pub toasts: &'a mut Toasts,
    pub guard: &'a mut crate::panel::confirm::TwoPress,
    pub now: Instant,
}

fn clear_gate(
    rows: &[PrQueueRow],
    guard: &mut crate::panel::confirm::TwoPress,
    key: &str,
    now: Instant,
) -> Result<(), String> {
    let running = rows
        .iter()
        .filter(|row| row.status == "agent_running")
        .count();
    let what = format!(
        "clear {} PR-queue row(s) ({running} with an agent running)",
        rows.len()
    );
    if guard.confirm(key, now) {
        Ok(())
    } else {
        Err(crate::panel::confirm::arm_prompt('c', &what))
    }
}

/// Handle one of the section's action keys on the row under the cursor. Returns
/// whether the key was consumed.
pub(crate) fn section_key(key: char, cursor: usize, ctx: PrqKeyCtx) -> bool {
    let (row, task) = selection_at_cursor(&ctx.model.panel, cursor);
    let action = match row_action_for(
        key,
        row.as_ref().map(|r| r.status.as_str()),
        task.as_ref().map(|task| task.status),
    ) {
        Ok(a) => a,
        Err("") => return false,
        Err(hint) => {
            ctx.toasts.info_ttl(
                hint.to_string(),
                std::time::Instant::now(),
                std::time::Duration::from_secs(4),
            );
            return true;
        }
    };

    match action {
        PrqAction::Add => spawn_add(ctx.tx, ctx.waker, ctx.active_wt.clone()),
        PrqAction::Refresh => {
            if *ctx.inflight {
                ctx.toasts.info_ttl(
                    "Already refreshing…".to_string(),
                    std::time::Instant::now(),
                    std::time::Duration::from_secs(3),
                );
                return true;
            }
            *ctx.inflight = true;
            spawn_drive(ctx.tx, ctx.waker, ctx.cfg.clone(), ctx.active_wt.clone());
        }
        PrqAction::Clear => {
            let rows = &ctx.model.panel.pr_queue;
            if rows.is_empty() {
                ctx.model.status = "PR queue: nothing to clear".into();
                return true;
            }
            let key = format!("prq-clear:{}", ctx.active_wt.display());
            if let Err(prompt) = clear_gate(rows, ctx.guard, &key, ctx.now) {
                ctx.model.status = prompt;
                return true;
            }
            spawn_mutate(ctx.tx, ctx.waker, ctx.active_wt.clone(), Mutation::Clear)
        }
        PrqAction::Remove => {
            if let Some(r) = row {
                spawn_mutate(
                    ctx.tx,
                    ctx.waker,
                    ctx.active_wt.clone(),
                    Mutation::Remove {
                        key: r.key,
                        number: r.number,
                    },
                );
            }
        }
        PrqAction::Rewatch => {
            if let Some(r) = row {
                spawn_mutate(
                    ctx.tx,
                    ctx.waker,
                    ctx.active_wt.clone(),
                    Mutation::Rewatch {
                        key: r.key,
                        number: r.number,
                    },
                );
            }
        }
        PrqAction::OpenInBrowser => {
            if let Some(r) = row {
                let wt = r
                    .worktree
                    .clone()
                    .unwrap_or_else(|| ctx.active_wt.to_string_lossy().into_owned());
                let branch = r.branch.clone();
                tokio::task::spawn_blocking(move || {
                    let loc = thegn_core::remote::GitLoc::from_db(&wt, None);
                    // best-effort: opening a browser can fail for a dozen
                    // environmental reasons and none should disturb the loop.
                    let _ = crate::forge_handle::get()
                        .for_loc(&loc)
                        .open_in_browser(&loc, Some(&branch));
                });
            }
        }
        PrqAction::HandleReview => {
            if let Some(task) = task {
                spawn_handle(ctx.tx, ctx.waker, ctx.cfg.clone(), task);
            }
        }
    }
    let _ = ctx.refresh_tx;
    true
}

/// Resolve the cursor in the same interleaved order used by the PR-queue
/// renderer: each PR row is followed by its review-task rows.
fn selection_at_cursor(
    panel: &crate::panel::PanelData,
    cursor: usize,
) -> (Option<PrQueueRow>, Option<crate::panel::ReviewTaskRow>) {
    let mut display_index = 0;
    for row in &panel.pr_queue {
        if display_index == cursor {
            return (Some(row.clone()), None);
        }
        display_index += 1;
        for task in panel
            .review_tasks
            .iter()
            .filter(|task| task.pr_number == row.number)
        {
            if display_index == cursor {
                return (None, Some(task.clone()));
            }
            display_index += 1;
        }
    }
    (None, None)
}

/// Resolve a review-task selection using the same interleaved row order as the
/// PR-queue renderer. The command palette uses this when the queue section is
/// already open; subtracting the total PR-row count is incorrect when tasks
/// are distributed across multiple PRs.
pub(crate) fn selected_review_task(
    panel: &crate::panel::PanelData,
    cursor: usize,
) -> Option<crate::panel::ReviewTaskRow> {
    selection_at_cursor(panel, cursor).1
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn record_settled_step_writes_the_inbox_row_and_skips_transient_states() {
        use thegn_core::store::NotificationStore;
        let _state = crate::testenv::IsolatedState::new();
        let db = Db::open().unwrap();
        record_settled_step(&db, "k-human", "/wt/a", 7, "b", "needs_human", "conflict");
        assert!(
            db.has_notification("pr_queue_needs_human", "k-human")
                .unwrap()
        );
        record_settled_step(&db, "k-watch", "/wt/a", 8, "b", "watching", "");
        assert!(
            !db.has_notification("pr_queue_needs_human", "k-watch")
                .unwrap()
        );
        assert!(!db.has_notification("pr_queue_ready", "k-watch").unwrap());
    }

    fn held_db_row(
        forge: &str,
        branch: &str,
        bound_url: Option<&str>,
    ) -> (tempfile::TempDir, Db, PrQueueRow) {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open_at(&dir.path().join("q.db")).unwrap();
        let root = dir.path().to_string_lossy().into_owned();
        match bound_url {
            Some(url) => db
                .enqueue_bound_pr(&root, 7, Some("/worktree"), branch, "main", forge, url)
                .unwrap(),
            None => db
                .enqueue_pr(&root, 7, Some("/worktree"), branch, "main", forge)
                .unwrap(),
        }
        let key = db
            .list_pr_queue()
            .unwrap()
            .into_iter()
            .find(|row| row.number == 7)
            .unwrap()
            .key;
        db.update_pr_status(&key, "needs_human", None, None, None)
            .unwrap();
        let row = db
            .list_pr_queue()
            .unwrap()
            .into_iter()
            .find(|row| row.key == key)
            .unwrap();
        (dir, db, row)
    }

    fn fetched(number: u64, url: &str, branch: &str) -> thegn_core::forge::model::PrStatus {
        thegn_core::forge::model::PrStatus {
            number,
            url: url.into(),
            head_ref_name: branch.into(),
            base_ref_name: "main".into(),
            ..Default::default()
        }
    }

    #[test]
    fn rewatch_binds_unbound_row_with_the_resolved_forge() {
        let (_dir, db, row) = held_db_row("gitlab", "feature", None);
        let result = rewatch_row(
            &db,
            &row,
            "gitlab",
            Ok(fetched(
                7,
                "https://gitlab.com/acme/project/-/merge_requests/7",
                "feature",
            )),
        );
        assert!(result.is_ok());
        let updated = db.list_pr_queue().unwrap().remove(0);
        assert_eq!(updated.status, "watching");
        assert_eq!(
            updated.pr_url.as_deref(),
            Some("https://gitlab.com/acme/project/-/merge_requests/7")
        );
        assert_eq!(updated.forge, "gitlab");
    }

    #[test]
    fn rewatch_refuses_a_bound_row_from_another_project() {
        let (_dir, db, row) = held_db_row(
            "github",
            "feature",
            Some("https://github.com/acme/project/pull/7"),
        );
        let result = rewatch_row(
            &db,
            &row,
            "github",
            Ok(fetched(
                7,
                "https://github.com/other/project/pull/7",
                "feature",
            )),
        );
        assert!(result.unwrap_err().contains("thegn pr queue rebind --pr 7"));
        let unchanged = db.list_pr_queue().unwrap().remove(0);
        assert_eq!(unchanged.status, "needs_human");
        assert_eq!(unchanged.pr_url, row.pr_url);
    }

    #[test]
    fn rewatch_refuses_a_bound_row_when_the_forge_changed() {
        let (_dir, db, row) = held_db_row(
            "github",
            "feature",
            Some("https://github.com/acme/project/pull/7"),
        );
        let result = rewatch_row(
            &db,
            &row,
            "gitlab",
            Ok(fetched(
                7,
                "https://github.com/acme/project/pull/7",
                "feature",
            )),
        );
        assert!(result.unwrap_err().contains("thegn pr queue rebind --pr 7"));
        let unchanged = db.list_pr_queue().unwrap().remove(0);
        assert_eq!(unchanged.status, "needs_human");
        assert_eq!(unchanged.forge, "github");
    }

    #[test]
    fn rewatch_fetch_error_leaves_held_row_unchanged() {
        let (_dir, db, row) = held_db_row(
            "github",
            "feature",
            Some("https://github.com/acme/project/pull/7"),
        );
        let result = rewatch_row(
            &db,
            &row,
            "github",
            Err(thegn_core::forge::ForgeError::NoPr),
        );
        assert!(result.unwrap_err().contains("could not verify PR #7"));
        let unchanged = db.list_pr_queue().unwrap().remove(0);
        assert_eq!(unchanged.status, "needs_human");
        assert_eq!(unchanged.pr_url, row.pr_url);
        assert_eq!(unchanged.agent_attempts, row.agent_attempts);
    }

    #[test]
    fn rewatch_clears_checkout_binding_when_pr_branch_changed() {
        let (_dir, db, row) = held_db_row("github", "feature", None);
        rewatch_row(
            &db,
            &row,
            "github",
            Ok(fetched(
                7,
                "https://github.com/acme/project/pull/7",
                "renamed-feature",
            )),
        )
        .unwrap();
        let updated = db.list_pr_queue().unwrap().remove(0);
        assert_eq!(updated.status, "watching");
        assert_eq!(
            updated.pr_url.as_deref(),
            Some("https://github.com/acme/project/pull/7")
        );
        assert_eq!(updated.worktree, None);
    }

    /// THE-515 fixture: a real git repo named `acme` plus an isolated state
    /// dir, and a config whose trusted overlay for it is ambiguous.
    fn refused_acme() -> (crate::testenv::IsolatedState, std::path::PathBuf) {
        let state = crate::testenv::IsolatedState::new();
        let repo = state.root().join("acme");
        std::fs::create_dir_all(&repo).unwrap();
        #[expect(clippy::disallowed_methods)]
        let ok = thegn_core::util::git_cmd(&repo)
            .args(["init", "-q"])
            .status()
            .unwrap()
            .success();
        assert!(ok);
        (state, repo)
    }

    #[test]
    fn drive_refuses_an_ambiguous_overlay_before_any_forge_work() {
        let (_state, repo) = refused_acme();
        let mut cfg = Config::default();
        cfg.pr_queue.enabled = true;
        cfg.workspace.insert("acme".into(), Default::default());
        cfg.workspace.insert("ACME".into(), Default::default());
        let sent = std::cell::RefCell::new(Vec::new());
        drive_blocking(&cfg, &repo, DriveMode::Ui, |m| sent.borrow_mut().push(m));
        let sent = sent.into_inner();
        assert_eq!(sent.len(), 1);
        assert!(
            matches!(&sent[0], PrqMsg::Failed(m) if m.contains("refused") && m.contains("`ACME`")),
            "refusal must be reported, not a drive pass"
        );
    }

    fn agent_queue(sandbox: bool) -> thegn_core::config::PrQueueConfig {
        thegn_core::config::PrQueueConfig {
            enabled: true,
            agent_command: "fixer {prompt}".into(),
            agent: "claude".into(),
            agent_sandbox: sandbox,
            ..Default::default()
        }
    }

    #[test]
    fn daemon_mode_without_agent_sandbox_degrades_to_notify() {
        // Off globally, or off for the repo: no agent resolves, so the driver
        // reports the blocker and dispatches nothing.
        for (global, repo) in [(false, false), (false, true), (true, false)] {
            let policy = daemon_queue_policy(&agent_queue(global), &agent_queue(repo));
            assert!(!policy.agent_sandbox);
            assert!(policy.agent.is_empty() && policy.agent_command.is_empty());
            assert_eq!(
                thegn_core::agent_task::resolve_agent(
                    &Config::default(),
                    &policy.agent,
                    &policy.agent_command
                ),
                None,
                "global={global} repo={repo}: a daemon pass must not resolve an agent"
            );
        }
    }

    #[test]
    fn daemon_mode_with_agent_sandbox_demands_shared_kernel_and_fails_closed() {
        use thegn_core::config::{IsolationFloor, OnFloorMiss};
        let policy = daemon_queue_policy(&agent_queue(true), &agent_queue(true));
        assert!(policy.agent_sandbox);
        assert_eq!(policy.agent_command, "fixer {prompt}");
        assert_eq!(policy.agent_isolation_floor, IsolationFloor::SharedKernel);
        assert_eq!(policy.agent_on_floor_miss, OnFloorMiss::Fail);

        // A stricter configured floor (global or repo) is kept, never lowered.
        let mut strict = agent_queue(true);
        strict.agent_isolation_floor = IsolationFloor::GuestKernel;
        let policy = daemon_queue_policy(&agent_queue(true), &strict);
        assert_eq!(policy.agent_isolation_floor, IsolationFloor::GuestKernel);
        let policy = daemon_queue_policy(&strict, &agent_queue(true));
        assert_eq!(policy.agent_isolation_floor, IsolationFloor::GuestKernel);
    }

    #[test]
    fn ui_pass_yields_only_while_the_daemon_owns_the_queue() {
        assert!(ui_yields_to_daemon(DriveMode::Ui, true));
        assert!(!ui_yields_to_daemon(DriveMode::Ui, false));
        // The daemon's own pass never yields to itself.
        assert!(!ui_yields_to_daemon(DriveMode::Daemon, true));
        assert!(!ui_yields_to_daemon(DriveMode::Daemon, false));
    }

    #[test]
    fn ui_yield_and_daemon_policy_precede_any_forge_work() {
        let source = include_str!("pr_queue.rs");
        let production = source.split_once("\n#[cfg(test)]\nmod tests {").unwrap().0;
        let start = production.find("pub(crate) fn drive_blocking(").unwrap();
        let end = production[start..].find("\n}\n").unwrap() + start;
        let body = &production[start..end];
        let yield_at = body.find(concat!("ui_yields_", "to_daemon(")).unwrap();
        let policy_at = body.find(concat!("daemon_queue_", "policy(")).unwrap();
        let db_at = body.find(concat!("Db::", "open()")).unwrap();
        let forge_at = body.find(concat!("forge_handle::", "get()")).unwrap();
        let drive_at = body.find(concat!("pr_driver::", "drive_queue(")).unwrap();
        assert!(yield_at < db_at && yield_at < forge_at);
        assert!(policy_at < drive_at);
    }

    #[test]
    fn drive_honours_the_repo_resolved_enabled_flag() {
        // Ticker armed on the GLOBAL flag; the repo's own block turned it off.
        let (state, repo) = refused_acme();
        let mut cfg = Config::default();
        cfg.pr_queue.enabled = true;
        let mut ws = thegn_core::config::WorkspaceConfig::default();
        ws.pr_queue.enabled = Some(false);
        cfg.workspace.insert("acme".into(), ws);
        let sent = std::cell::RefCell::new(Vec::new());
        drive_blocking(&cfg, &repo, DriveMode::Ui, |m| sent.borrow_mut().push(m));
        let sent = sent.into_inner();
        assert_eq!(sent.len(), 1);
        match &sent[0] {
            PrqMsg::Done(out) => assert!(out.merged.is_empty()),
            _ => panic!("a repo-disabled queue must be a no-op pass"),
        }
        // The pass stopped before touching state (the old code opened the DB
        // and walked the queue rows toward the forge).
        assert!(
            !state.state_home().join("thegn/thegn.db").exists(),
            "a repo-disabled PR queue must not start a drive pass"
        );
    }

    fn row(key: &str, number: u64, status: &str) -> PrQueueRow {
        PrQueueRow {
            key: key.into(),
            repo_root: "/repo".into(),
            number,
            worktree: Some("/w".into()),
            branch: "feat".into(),
            base_branch: "main".into(),
            forge: "github".into(),
            pr_url: None,
            forge_host: None,
            project: None,
            checkout_gen: 0,
            status: status.into(),
            blocker: None,
            detail: None,
            agent_attempts: 0,
            ci_rerun_head: None,
            last_head_oid: None,
            admission_refusal: None,
            admission_refusal_head: None,
            queued_at: 0,
            updated_at: 0,
        }
    }

    #[test]
    fn clear_needs_two_presses() {
        let panel = crate::panel::PanelData {
            pr_queue: vec![
                row("/repo#1", 1, "queued"),
                row("/repo#2", 2, "agent_running"),
                row("/repo#3", 3, "needs_human"),
            ],
            ..Default::default()
        };
        let t0 = std::time::Instant::now();
        let mut guard = crate::panel::confirm::TwoPress::default();
        let prompt = clear_gate(&panel.pr_queue, &mut guard, "repo", t0).unwrap_err();
        assert!(prompt.contains("3"));
        assert!(prompt.contains("1 with an agent running"));
        assert!(
            clear_gate(
                &panel.pr_queue,
                &mut guard,
                "repo",
                t0 + std::time::Duration::from_secs(1),
            )
            .is_ok()
        );

        assert!(clear_gate(&panel.pr_queue, &mut guard, "repo", t0).is_err());
        assert!(
            clear_gate(
                &panel.pr_queue,
                &mut guard,
                "other",
                t0 + std::time::Duration::from_secs(1)
            )
            .is_err()
        );
        assert!(
            clear_gate(
                &panel.pr_queue,
                &mut guard,
                "other",
                t0 + std::time::Duration::from_secs(5)
            )
            .is_err()
        );
    }

    #[test]
    fn queue_wide_keys_need_no_row() {
        for (k, want) in [
            ('a', PrqAction::Add),
            ('D', PrqAction::Refresh),
            ('c', PrqAction::Clear),
        ] {
            assert_eq!(row_action_for(k, None, None), Ok(want), "{k}");
        }
    }

    #[test]
    fn row_keys_report_an_empty_selection_rather_than_acting() {
        for k in ['x', 'o', 'r'] {
            match row_action_for(k, None, None) {
                Err(hint) => assert!(hint.contains("no row selected"), "{k}: {hint}"),
                Ok(a) => panic!("{k} acted with no row: {a:?}"),
            }
        }
    }

    #[test]
    fn rewatch_applies_only_to_a_settled_row() {
        for s in ["needs_human", "merged", "closed"] {
            assert_eq!(
                row_action_for('r', Some(s), None),
                Ok(PrqAction::Rewatch),
                "{s}"
            );
        }
        for s in ["watching", "blocked_ci", "agent_running", "ready"] {
            assert!(
                row_action_for('r', Some(s), None).is_err(),
                "{s} is already watched; re-arming it is a no-op"
            );
        }
    }

    #[test]
    fn an_unbound_key_is_not_consumed() {
        // Empty hint = "not ours", so the loop can fall through to other
        // handlers rather than swallowing the key.
        assert_eq!(row_action_for('z', Some("watching"), None), Err(""));
        assert_eq!(row_action_for('q', None, None), Err(""));
    }

    #[test]
    fn every_hinted_key_is_dispatchable() {
        // The section's hint row advertises these; a key shown but not handled
        // would be a lie to the reader.
        for k in ['a', 'x', 'r', 'c', 'D', 'o'] {
            let with_row = row_action_for(k, Some("needs_human"), None);
            assert!(with_row.is_ok(), "{k} is advertised but not dispatchable");
        }
        assert_eq!(
            row_action_for(
                'h',
                None,
                Some(thegn_core::issue::AgentDispatchStatus::Queued)
            ),
            Ok(PrqAction::HandleReview)
        );
    }

    /// Run `f` against a throwaway drain context whose panel holds one
    /// `/repo#1` row (the waker needs a pty, so this is a no-op where the test
    /// support has none).
    fn with_prq_ctx(f: impl FnOnce(&mut PrqDrainCtx<'_>)) {
        if !crate::platform::sound_process::test_support::UNIX {
            return;
        }
        let (waker, _fixture) = crate::platform::sound_process::test_support::test_waker();
        let cfg = Config::default();
        let notify =
            crate::notify::NotifyState::new(cfg.notifications.clone(), String::new(), waker);
        let event_bus = thegn_core::event_bus::EventBus::new();
        let mut model = crate::chrome::FrameModel::default();
        model.panel.pr_queue = vec![row("/repo#1", 1, "watching")];
        let mut toasts = Toasts::default();
        let mut inflight = false;
        let mut want_model_refresh = false;
        let mut loop_perf = crate::perf::LoopPerf::new();
        f(&mut PrqDrainCtx {
            model: &mut model,
            toasts: &mut toasts,
            notify_state: &notify,
            event_bus: &event_bus,
            inflight: &mut inflight,
            want_model_refresh: &mut want_model_refresh,
            loop_perf: &mut loop_perf,
        });
    }

    /// A transient step on the fixture row: only the row's detail changes.
    fn step(i: usize) -> PrqMsg {
        PrqMsg::Step {
            key: "/repo#1".into(),
            number: 1,
            branch: "feat".into(),
            status: "checking".into(),
            detail: format!("step {i}"),
        }
    }

    fn row_detail(ctx: &PrqDrainCtx<'_>) -> Option<String> {
        ctx.model.panel.pr_queue[0].detail.clone()
    }

    #[test]
    fn prq_drain_is_bounded_per_call_and_lossless_in_order() {
        with_prq_ctx(|ctx| {
            let cap = crate::ingress_budget::PR_QUEUE_SOURCE.limits.max_msgs;
            assert_eq!(cap, 64);
            let total = 200usize;
            let (tx, mut rx) = tokio_mpsc::unbounded_channel();
            for i in 0..total {
                assert!(tx.send(step(i)).is_ok());
            }
            let mut rotation = 0usize;
            // The deadline cannot expire mid-test: the cap is the only bound.
            let roomy = || {
                crate::ingress_budget::IterationBudget::new(
                    Instant::now(),
                    std::time::Duration::from_secs(3600),
                    false,
                )
            };

            assert!(drain_msgs(&mut rx, ctx, &mut roomy(), &mut rotation));
            assert_eq!(rx.len(), total - cap);
            assert_eq!(row_detail(ctx), Some(format!("step {}", cap - 1)));
            assert_eq!(ctx.model.panel.pr_queue[0].status, "checking");

            let mut applied = cap;
            loop {
                let before = rx.len();
                let more = drain_msgs(&mut rx, ctx, &mut roomy(), &mut rotation);
                let taken = before - rx.len();
                assert_eq!(taken, cap.min(before));
                applied += taken;
                assert_eq!(row_detail(ctx), Some(format!("step {}", applied - 1)));
                assert_eq!(more, !rx.is_empty());
                if !more {
                    break;
                }
            }
            assert_eq!(applied, total);
            assert!(rx.is_empty());
        });
    }

    #[test]
    fn prq_drain_of_empty_receiver_reports_nothing_and_does_not_tick() {
        with_prq_ctx(|ctx| {
            let was_on = crate::perf::enabled();
            crate::perf::set_enabled(true);
            let (tx, mut rx) = tokio_mpsc::unbounded_channel::<PrqMsg>();
            let mut rotation = 0usize;
            let fresh = || {
                crate::ingress_budget::IterationBudget::new(
                    Instant::now(),
                    crate::ingress_budget::LOOP_DEADLINE,
                    false,
                )
            };
            assert!(!drain_msgs(&mut rx, ctx, &mut fresh(), &mut rotation));
            assert_eq!(ctx.loop_perf.items(crate::perf::WakeSource::Fold), 0);
            assert_eq!(row_detail(ctx), None);

            assert!(tx.send(step(0)).is_ok());
            assert!(!drain_msgs(&mut rx, ctx, &mut fresh(), &mut rotation));
            assert_eq!(ctx.loop_perf.items(crate::perf::WakeSource::Fold), 1);
            crate::perf::set_enabled(was_on);
        });
    }

    #[test]
    fn prq_drain_with_spent_clock_still_takes_one_message() {
        with_prq_ctx(|ctx| {
            use crate::ingress_budget::{IterationBudget, LOOP_DEADLINE};
            let start = Instant::now();
            let mut spent = IterationBudget::new(start - LOOP_DEADLINE * 2, LOOP_DEADLINE, false);
            let (tx, mut rx) = tokio_mpsc::unbounded_channel();
            for i in 0..3 {
                assert!(tx.send(step(i)).is_ok());
            }
            let mut rotation = 0usize;
            assert!(drain_msgs(&mut rx, ctx, &mut spent, &mut rotation));
            assert_eq!(rx.len(), 2);
            assert_eq!(row_detail(ctx), Some("step 0".to_string()));
        });
    }

    #[test]
    fn apply_step_patches_the_matching_row_only() {
        let mut panel = crate::panel::PanelData {
            pr_queue: vec![row("/repo#1", 1, "watching"), row("/repo#2", 2, "watching")],
            ..Default::default()
        };
        apply_step(&mut panel, "/repo#2", "blocked_ci", "failing: clippy");
        assert_eq!(panel.pr_queue[0].status, "watching", "untouched");
        assert_eq!(panel.pr_queue[1].status, "blocked_ci");
        assert_eq!(panel.pr_queue[1].detail.as_deref(), Some("failing: clippy"));
        // An empty detail clears rather than storing "".
        apply_step(&mut panel, "/repo#2", "ready", "");
        assert_eq!(panel.pr_queue[1].detail, None);
        // An unknown key is a no-op, not a panic.
        apply_step(&mut panel, "/repo#99", "merged", "x");
        assert_eq!(panel.pr_queue.len(), 2);
    }

    #[test]
    fn cursor_selection_matches_interleaved_review_rows() {
        let panel = crate::panel::PanelData {
            pr_queue: vec![row("/repo#1", 1, "watching"), row("/repo#2", 2, "watching")],
            review_tasks: vec![crate::panel::ReviewTaskRow {
                id: 9,
                pr_number: 1,
                repository: "acme/widget".into(),
                thread_id: "thread-1".into(),
                path: "src/lib.rs".into(),
                line: Some(3),
                role: "coder".into(),
                status: thegn_core::issue::AgentDispatchStatus::Queued,
                source_revision: "revision".into(),
                worktree_path: "/w".into(),
            }],
            ..Default::default()
        };
        assert!(selection_at_cursor(&panel, 0).0.is_some());
        assert_eq!(selection_at_cursor(&panel, 1).1.unwrap().id, 9);
        assert_eq!(selection_at_cursor(&panel, 2).0.unwrap().number, 2);
    }

    #[test]
    fn selected_review_task_handles_uneven_interleaving() {
        let panel = crate::panel::PanelData {
            pr_queue: vec![row("/repo#1", 1, "watching"), row("/repo#2", 2, "watching")],
            review_tasks: vec![
                crate::panel::ReviewTaskRow {
                    id: 9,
                    pr_number: 1,
                    repository: "acme/widget".into(),
                    thread_id: "thread-1".into(),
                    path: "src/lib.rs".into(),
                    line: Some(3),
                    role: "coder".into(),
                    status: thegn_core::issue::AgentDispatchStatus::Queued,
                    source_revision: "revision-1".into(),
                    worktree_path: "/w".into(),
                },
                crate::panel::ReviewTaskRow {
                    id: 10,
                    pr_number: 2,
                    repository: "acme/widget".into(),
                    thread_id: "thread-2".into(),
                    path: "src/main.rs".into(),
                    line: Some(4),
                    role: "coder".into(),
                    status: thegn_core::issue::AgentDispatchStatus::Queued,
                    source_revision: "revision-2".into(),
                    worktree_path: "/w".into(),
                },
            ],
            ..Default::default()
        };
        // Row order: PR1, task1, PR2, task2.
        assert_eq!(selected_review_task(&panel, 3).unwrap().id, 10);
    }

    #[test]
    fn pr_removal_message_names_a_running_agent() {
        assert_eq!(
            pr_removal_message(7, thegn_core::store::PrRemoval::AgentRunning),
            "PR #7 was not removed: a PR agent is running for it; remove it after the agent finishes"
        );
    }

    #[test]
    fn pr_removal_message_reports_a_missing_row() {
        assert_eq!(
            pr_removal_message(7, thegn_core::store::PrRemoval::Missing),
            "PR #7 is no longer in the queue"
        );
    }

    #[test]
    fn pr_clear_message_lists_kept_prs() {
        assert_eq!(
            pr_clear_message(&thegn_core::store::PrClearOutcome {
                removed: 2,
                kept_running: vec![3, 8],
            }),
            "PR queue cleared (2 removed). kept PR #3, #8: a PR agent is running for them"
        );
    }
}
