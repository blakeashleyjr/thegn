//! Concurrent provisioning lanes (THE-2708).
//!
//! A cold provider env spends its first minute and a half on two unrelated
//! jobs: putting the repository in the workdir (bundle upload + clone, local
//! parity) and installing Nix and direnv under `/nix` and `$HOME`. Neither reads
//! what the other writes, so `run_window` runs the two lanes of a
//! `thegn_core::envplan::OverlapWindow` on their own threads. Each lane keeps
//! its order, the loading screen updates from the caller's thread, and the
//! steps after the window start only when both lanes have finished.
//!
//! Failure handling matches the serial loop: a best-effort failure shows on its
//! row and the lane continues; a fatal one stops both lanes before their next
//! step and fails the window.

use std::ops::Range;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc;
use std::time::Instant;

use thegn_core::envplan::{OverlapWindow, ProvisionStep};

use crate::agent::ProvisionState;
use crate::provision_recover::{
    sanitize_detail, step_is_fatal, step_is_warm_only, step_signals_sandbox_restart,
};

/// One row change from a lane, applied to the loading-screen views by the
/// thread that called `run_window`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct LaneEvent {
    /// Index into the plan's step list.
    pub index: usize,
    pub state: ProvisionState,
    pub detail: Option<String>,
}

/// Everything a lane needs, shared by reference between the two threads.
struct Lanes<'a> {
    steps: &'a [ProvisionStep],
    exec: &'a (dyn Fn(&ProvisionStep) -> anyhow::Result<()> + Sync),
    on_restart: &'a (dyn Fn() + Sync),
    hint: fn(&str) -> Option<&'static str>,
    abort: AtomicBool,
    /// Sum of every step's own run time, to log what the overlap saved.
    busy_ms: AtomicU64,
}

/// Run the window's repository lane and tool lane at the same time.
///
/// `exec` runs one step (the same per-kind runner the serial loop uses),
/// `on_restart` waits for a sandbox that a failed step may have restarted, and
/// `hint` gives the sub-line for a slow step. `on_event` sees every row change,
/// on this thread. Returns the first fatal error (the repository lane's when
/// both failed), after both lanes have stopped.
pub(crate) fn run_window(
    steps: &[ProvisionStep],
    window: &OverlapWindow,
    exec: &(dyn Fn(&ProvisionStep) -> anyhow::Result<()> + Sync),
    on_restart: &(dyn Fn() + Sync),
    hint: fn(&str) -> Option<&'static str>,
    mut on_event: impl FnMut(LaneEvent),
) -> anyhow::Result<()> {
    let lanes = Lanes {
        steps,
        exec,
        on_restart,
        hint,
        abort: AtomicBool::new(false),
        busy_ms: AtomicU64::new(0),
    };
    let lanes = &lanes;
    let started = Instant::now();
    let (tx, rx) = mpsc::channel::<LaneEvent>();
    let repo_range = window.repo.clone();
    let tools_range = window.tools.clone();
    let (repo, tools) = std::thread::scope(|scope| {
        let repo_tx = tx.clone();
        let repo = scope.spawn(move || run_lane(lanes, "repo", repo_range, &repo_tx));
        let tools = scope.spawn(move || run_lane(lanes, "tools", tools_range, &tx));
        // Ends once both lanes have returned and dropped their senders.
        for event in rx {
            on_event(event);
        }
        (repo.join(), tools.join())
    });
    let wall_ms = elapsed_ms(started);
    let busy_ms = lanes.busy_ms.load(Ordering::Relaxed);
    tracing::info!(
        target: "thegn::startup",
        wall_ms,
        busy_ms,
        saved_ms = busy_ms.saturating_sub(wall_ms),
        "provision overlap window done"
    );
    let repo = repo.unwrap_or_else(|_| Err(anyhow::anyhow!("provision repo lane panicked")));
    let tools = tools.unwrap_or_else(|_| Err(anyhow::anyhow!("provision tool lane panicked")));
    repo.and(tools)
}

fn elapsed_ms(since: Instant) -> u64 {
    u64::try_from(since.elapsed().as_millis()).unwrap_or(u64::MAX)
}

fn send(tx: &mpsc::Sender<LaneEvent>, index: usize, state: ProvisionState, detail: Option<String>) {
    if tx
        .send(LaneEvent {
            index,
            state,
            detail,
        })
        .is_err()
    {
        // The receiver lives until both lanes return, so this cannot happen;
        // a lost row update must not fail provisioning.
        tracing::debug!(target: "thegn::startup", index, "provision lane event dropped");
    }
}

/// Run one lane's steps in order. Stops before the next step once either lane
/// has hit a fatal failure; returns this lane's fatal error, if any.
fn run_lane(
    lanes: &Lanes<'_>,
    lane: &'static str,
    range: Range<usize>,
    tx: &mpsc::Sender<LaneEvent>,
) -> anyhow::Result<()> {
    for index in range {
        if lanes.abort.load(Ordering::Acquire) {
            return Ok(());
        }
        let step = &lanes.steps[index];
        send(
            tx,
            index,
            ProvisionState::Active,
            (lanes.hint)(&step.id).map(str::to_string),
        );
        let step_t0 = Instant::now();
        tracing::info!(target: "thegn::startup", step = %step.id, lane, "provision step start");
        let result = (lanes.exec)(step);
        let ms = elapsed_ms(step_t0);
        lanes.busy_ms.fetch_add(ms, Ordering::Relaxed);
        let Err(e) = result else {
            tracing::info!(target: "thegn::startup", step = %step.id, lane, ms, "provision step done");
            send(tx, index, ProvisionState::Done, None);
            continue;
        };
        // Same presentation as the serial loop: a pure pre-warm reads as done.
        let warm_only = step_is_warm_only(&step.id);
        let (state, detail) = if warm_only {
            (
                ProvisionState::Done,
                "deferred — the dev shell builds on first entry".to_string(),
            )
        } else {
            (ProvisionState::Failed, sanitize_detail(&e.to_string()))
        };
        send(tx, index, state, Some(detail));
        if step_is_fatal(&step.id) {
            lanes.abort.store(true, Ordering::Release);
            tracing::warn!(target: "thegn::startup", step = %step.id, lane, ms, error = %e, "provision step failed (fatal — aborting)");
            return Err(e);
        }
        tracing::warn!(target: "thegn::startup", step = %step.id, lane, ms, error = %e, "provision step failed (best-effort — continuing)");
        if step_signals_sandbox_restart(&e.to_string()) {
            (lanes.on_restart)();
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;
    use std::time::Duration;

    use thegn_core::envplan::StepKind;

    use super::*;

    fn step(id: &str) -> ProvisionStep {
        ProvisionStep {
            id: id.into(),
            label: id.into(),
            kind: StepKind::Exec("true".into()),
        }
    }

    fn no_hint(_: &str) -> Option<&'static str> {
        None
    }

    fn window() -> (Vec<ProvisionStep>, OverlapWindow) {
        let steps = vec![
            step("bundle_seed"),
            step("git_exclude"),
            step("nix"),
            step("nix_verify"),
            step("direnv"),
        ];
        let window = OverlapWindow {
            repo: 0..2,
            tools: 2..5,
        };
        (steps, window)
    }

    #[test]
    fn lanes_run_concurrently_and_every_row_ends_done() {
        let (steps, window) = window();
        let per_step = Duration::from_millis(150);
        let exec = |_: &ProvisionStep| -> anyhow::Result<()> {
            std::thread::sleep(per_step);
            Ok(())
        };
        let mut finals = vec![None; steps.len()];
        let started = Instant::now();
        run_window(&steps, &window, &exec, &|| {}, no_hint, |event| {
            finals[event.index] = Some(event.state);
        })
        .expect("window");
        let elapsed = started.elapsed();
        // Serial would be 5 × 150 ms; the longer lane is 3 × 150 ms.
        assert!(
            elapsed < per_step * 5,
            "lanes did not overlap: {elapsed:?} for 5 steps of {per_step:?}"
        );
        assert!(elapsed >= per_step * 3, "{elapsed:?}");
        assert!(
            finals.iter().all(|s| *s == Some(ProvisionState::Done)),
            "{finals:?}"
        );
    }

    #[test]
    fn each_lane_keeps_its_own_order() {
        let (steps, window) = window();
        let order = Mutex::new(Vec::new());
        let exec = |s: &ProvisionStep| -> anyhow::Result<()> {
            order.lock().expect("order").push(s.id.clone());
            Ok(())
        };
        run_window(&steps, &window, &exec, &|| {}, no_hint, |_| {}).expect("window");
        let order = order.into_inner().expect("order");
        let pos = |id: &str| order.iter().position(|s| s == id).expect(id);
        assert!(pos("bundle_seed") < pos("git_exclude"), "{order:?}");
        assert!(pos("nix") < pos("nix_verify"), "{order:?}");
        assert!(pos("nix_verify") < pos("direnv"), "{order:?}");
    }

    #[test]
    fn a_fatal_failure_stops_the_other_lane_and_fails_the_window() {
        let (steps, window) = window();
        let ran = Mutex::new(Vec::new());
        // Both first steps start before the seed fails, so `nix` is in flight.
        let both_started = std::sync::Barrier::new(2);
        let exec = |s: &ProvisionStep| -> anyhow::Result<()> {
            ran.lock().expect("ran").push(s.id.clone());
            match s.id.as_str() {
                "bundle_seed" => {
                    let _started = both_started.wait();
                    Err(anyhow::anyhow!("seed failed"))
                }
                // Still running when the seed fails, so the lane stops after it.
                "nix" => {
                    let _started = both_started.wait();
                    std::thread::sleep(Duration::from_millis(300));
                    Ok(())
                }
                _ => Ok(()),
            }
        };
        let mut finals = vec![None; steps.len()];
        let err = run_window(&steps, &window, &exec, &|| {}, no_hint, |event| {
            finals[event.index] = Some(event.state);
        })
        .expect_err("a fatal seed fails the window");
        assert!(err.to_string().contains("seed failed"), "{err}");
        let ran = ran.into_inner().expect("ran");
        assert!(!ran.iter().any(|s| s == "git_exclude"), "{ran:?}");
        assert!(!ran.iter().any(|s| s == "nix_verify"), "{ran:?}");
        assert_eq!(finals[0], Some(ProvisionState::Failed));
        assert_eq!(finals[2], Some(ProvisionState::Done));
        assert_eq!(finals[3], None, "never started: {finals:?}");
    }

    #[test]
    fn a_best_effort_failure_continues_and_may_wait_for_a_restart() {
        let (steps, window) = window();
        let restarts = AtomicU64::new(0);
        let exec = |s: &ProvisionStep| -> anyhow::Result<()> {
            if s.id == "git_exclude" {
                Err(anyhow::anyhow!("Hide (exit 137): killed"))
            } else {
                Ok(())
            }
        };
        let on_restart = || {
            restarts.fetch_add(1, Ordering::Relaxed);
        };
        let mut finals = vec![None; steps.len()];
        let mut details = vec![None; steps.len()];
        run_window(&steps, &window, &exec, &on_restart, no_hint, |event| {
            finals[event.index] = Some(event.state);
            details[event.index] = event.detail;
        })
        .expect("best-effort failures do not fail the window");
        assert_eq!(finals[1], Some(ProvisionState::Failed));
        assert!(
            details[1]
                .as_deref()
                .is_some_and(|d| d.contains("exit 137")),
            "{details:?}"
        );
        assert_eq!(finals[4], Some(ProvisionState::Done));
        assert_eq!(restarts.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn the_active_row_carries_the_slow_step_hint() {
        let (steps, window) = window();
        fn hint(id: &str) -> Option<&'static str> {
            (id == "nix").then_some("slow")
        }
        let mut active = Vec::new();
        run_window(&steps, &window, &|_| Ok(()), &|| {}, hint, |event| {
            if event.state == ProvisionState::Active {
                active.push((event.index, event.detail));
            }
        })
        .expect("window");
        assert!(
            active.contains(&(2, Some("slow".to_string()))),
            "{active:?}"
        );
        assert!(active.contains(&(0, None)), "{active:?}");
    }
}
