//! Periodic roster self-heal — the daemon half of `dispatch reap`.
//!
//! The failure this exists to end: a stage worker does its work, commits its
//! artifact, and dies before calling `dispatch report`. `pipeline_reap`
//! classifies that correctly, but until now its only caller was the CLI verb
//! (`cmd::dispatch::reap`), so a row stayed `running` — indistinguishable from
//! a live worker — until a human happened to type the command. Across the
//! 469-row pipeline run that produced 40 rows recording no reason at all, and
//! it is the mechanism behind the 121-unclosable-row incident.
//!
//! # What this task may and may not write
//!
//! `pipeline_retry`'s header states the rule the daemon has always followed:
//! *the daemon can park a row but never finish one*. This task **extends that
//! rule in exactly one direction**, and it is worth being precise about why.
//!
//! - **`CloseDone` → `done`.** Allowed. This verdict fires only when the
//!   artifact exists, is *tracked by git*, and a report is filed — bit for bit
//!   the gate `dispatch set-status done` already enforces on the supervisor's
//!   behalf. Applying it is arithmetic on recorded facts, not a judgement about
//!   whether the work was any good, so there is nothing here for a human to
//!   decide differently. Refusing to apply it would not protect anything; it
//!   would only mean the row keeps lying about being live.
//! - **`MarkFailed` → `waiting_human` + a note.** Parked, never failed. The
//!   CLI writes `failed` here because a supervisor is present and owns the
//!   verdict; the daemon has no such standing, and "the worker vanished leaving
//!   nothing" is exactly the case a human should look at.
//! - **`NeedsDecision` → untouched.** Explicitly a human's call
//!   (`thegn_core::pipeline_reap`'s module doc).
//! - **`Live` / `Closed` → untouched.**
//!
//! # Cadence
//!
//! A slow timer, deliberately unlike [`super::HEARTBEAT_SECS`]: each pass reads
//! every active row's worktree and shells out to `git` twice per row, so it
//! runs on its own long interval and does that work inside `spawn_blocking` —
//! never on the runtime, and never on the compositor's loop (this is the
//! daemon process; the 0%-idle contract still applies to the host).

use std::sync::Arc;
use std::time::Duration;

use thegn_core::issue::AgentDispatchStatus;
use thegn_core::pipeline_reap::ReapVerdict;
use thegn_core::store::NotificationStore;

use super::service::DaemonService;

/// How often to reconcile. Long on purpose: the work is per-row git I/O, and
/// nothing here is latency-sensitive — a stale row costs a supervisor's
/// attention, not a user's frame.
const REAP_INTERVAL_SECS: u64 = 300;

/// Wait this long after daemon start before the first pass, so a restart that
/// is about to re-adopt live sessions does not read them as absent and park
/// rows that are seconds away from checking back in.
const REAP_FIRST_DELAY_SECS: u64 = 60;

pub(crate) fn spawn(svc: Arc<DaemonService>) {
    tokio::spawn(reap_loop(svc));
}

async fn reap_loop(svc: Arc<DaemonService>) {
    tokio::time::sleep(Duration::from_secs(REAP_FIRST_DELAY_SECS)).await;
    let mut tick = tokio::time::interval(Duration::from_secs(REAP_INTERVAL_SECS));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tick.tick().await;
        // The daemon's own session map is the liveness source: it holds live
        // entries only (an exited session becomes a `Lookup::Dead` tombstone),
        // so this needs no control round-trip to ourselves.
        let live_ids: Vec<String> = { svc.sessions.lock().await.keys().cloned().collect() };
        let db = svc.db.clone();
        // best-effort: a failed pass is retried on the next tick; a reap that
        // cannot read git must never take the daemon down.
        let _ = tokio::task::spawn_blocking(move || reap_pass(&db, &live_ids)).await;
    }
}

/// One reconciliation pass. Separated from the timer so the policy is readable
/// (and so a future caller — `thegn doctor`, say — can run it directly).
fn reap_pass(db: &super::service::SharedDb, live_ids: &[String]) {
    let db = match db.lock() {
        Ok(db) => db,
        Err(_) => return,
    };
    let plan = match crate::cmd::dispatch::reap_plan(&db, live_ids) {
        Ok(plan) => plan,
        Err(e) => {
            tracing::debug!(target: "thegn::pipeline", error = %e, "reap pass could not plan");
            return;
        }
    };
    for r in &plan {
        match &r.verdict {
            ReapVerdict::CloseDone => {
                if db
                    .update_dispatch_status(r.id, AgentDispatchStatus::Done)
                    .is_ok()
                {
                    tracing::info!(
                        target: "thegn::pipeline",
                        row = r.id,
                        "reaped: artifact committed and report filed — closing done"
                    );
                }
            }
            ReapVerdict::MarkFailed { why } => {
                // Park, do not fail: the daemon records what it saw and leaves
                // the verdict to a supervisor.
                // best-effort: the note is context; losing it must not stop the
                // status from moving off `running`, which is the point.
                let _ = db.append_dispatch_note(r.id, &format!("reaped (daemon): {why}"));
                if db
                    .update_dispatch_status(r.id, AgentDispatchStatus::WaitingHuman)
                    .is_ok()
                {
                    tracing::info!(
                        target: "thegn::pipeline",
                        row = r.id, why = %why,
                        "reaped: parked for a supervisor"
                    );
                }
            }
            // A human's call, or nothing to do.
            ReapVerdict::NeedsDecision { .. } | ReapVerdict::Live | ReapVerdict::Closed => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The boundary this module is allowed to cross, pinned so a later edit
    /// cannot quietly widen it. `CloseDone` is arithmetic on a committed,
    /// tracked artifact plus a filed report; everything else the daemon either
    /// parks or leaves alone.
    #[test]
    fn the_daemon_finishes_only_the_mechanically_gated_verdict() {
        let daemon_writes = |v: &ReapVerdict| -> Option<AgentDispatchStatus> {
            match v {
                ReapVerdict::CloseDone => Some(AgentDispatchStatus::Done),
                ReapVerdict::MarkFailed { .. } => Some(AgentDispatchStatus::WaitingHuman),
                ReapVerdict::NeedsDecision { .. } | ReapVerdict::Live | ReapVerdict::Closed => None,
            }
        };

        assert_eq!(
            daemon_writes(&ReapVerdict::CloseDone),
            Some(AgentDispatchStatus::Done)
        );
        assert_eq!(
            daemon_writes(&ReapVerdict::MarkFailed { why: "gone" }),
            Some(AgentDispatchStatus::WaitingHuman),
            "the daemon parks rather than failing — a verdict belongs to a supervisor"
        );
        assert_eq!(
            daemon_writes(&ReapVerdict::NeedsDecision { why: "ambiguous" }),
            None,
            "an ambiguous row is explicitly a human's call"
        );
        assert_eq!(daemon_writes(&ReapVerdict::Live), None);
    }

    #[test]
    fn the_first_pass_is_delayed_past_a_restarts_re_adoption() {
        // A restart re-adopts sessions asynchronously; reaping immediately would
        // read them as absent and park rows that are about to check back in.
        assert!(
            REAP_FIRST_DELAY_SECS >= 30,
            "first pass must not race a restart's session re-adoption"
        );
        assert!(
            REAP_INTERVAL_SECS >= 60,
            "this pass does per-row git I/O; it must stay a slow timer"
        );
    }
}
