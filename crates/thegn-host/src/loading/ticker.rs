//! `SplashTicker` — the splash-scoped repaint source that animates the
//! loading screen (spinner frames, elapsed counters, slow-step hints).
//!
//! The 0%-idle contract: when no splash is visible there must be NO wake
//! source at all. So this is not a resident ticker — `set_visible(true)`
//! spawns a short-lived thread that fires the tick callback every
//! `PERIOD` and EXITS (within one period) once visibility drops; a session
//! with no loading screen never has the thread at all. The callback is the
//! sanctioned off-thread producer contract: send on a channel + pulse the
//! `TerminalWaker`; a straggler tick after the splash cleared drains into an
//! empty damage set and `render_plan` returns `Skip` (locked by its tests).
//!
//! Benign race, documented: a `false→true` flip that lands exactly as the
//! old thread is exiting can miss spawning a replacement for one frame — the
//! loop calls `set_visible` after every derive pass while a splash is up, so
//! the next frame respawns it; a 250ms animation hiccup, nothing more.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use crate::chrome::{LoadStep, StepState};

/// One spinner frame per tick (see `plan::SPINNER_FRAME_MS`).
const PERIOD: Duration = Duration::from_millis(250);

/// Whether the derived splash needs periodic animation repaints. A terminal
/// Done/Failed row can remain drawn for context, but its elapsed value is
/// frozen and it must not keep the event loop awake.
pub(crate) fn is_animating(steps: &[LoadStep]) -> bool {
    steps.iter().any(|step| step.state == StepState::Active)
}

pub(crate) struct SplashTicker {
    active: Arc<AtomicBool>,
    running: Arc<AtomicBool>,
    tick: Arc<dyn Fn() + Send + Sync>,
    period: Duration,
}

impl SplashTicker {
    /// `tick` is called from the ticker thread each period while visible —
    /// wire it to `refresh_tx.send(RefreshKind::SplashTick)` + `waker.wake()`.
    pub(crate) fn new(tick: impl Fn() + Send + Sync + 'static) -> Self {
        Self {
            active: Arc::default(),
            running: Arc::default(),
            tick: Arc::new(tick),
            period: PERIOD,
        }
    }

    #[cfg(test)]
    fn with_period(tick: impl Fn() + Send + Sync + 'static, period: Duration) -> Self {
        Self {
            period,
            ..Self::new(tick)
        }
    }

    /// Declare whether any splash is currently visible. Cheap and idempotent —
    /// the loop calls this every frame after deriving the model. Spawns the
    /// ticker thread on the false→true edge; the thread parks itself out of
    /// existence (no send, no wake) within one period of `false`.
    pub(crate) fn set_visible(&self, visible: bool) {
        self.active.store(visible, Ordering::Relaxed);
        if visible
            && self
                .running
                .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
                .is_ok()
        {
            let active = self.active.clone();
            let running = self.running.clone();
            let tick = self.tick.clone();
            let period = self.period;
            std::thread::Builder::new()
                .name("thegn-splash-tick".into())
                .spawn(move || {
                    loop {
                        std::thread::sleep(period);
                        if !active.load(Ordering::Relaxed) {
                            running.store(false, Ordering::Release);
                            return;
                        }
                        tick();
                    }
                })
                .map(|_| ())
                .unwrap_or_else(|_| self.running.store(false, Ordering::Release));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;

    #[test]
    fn only_steps_with_live_animation_keep_the_ticker_visible() {
        let active = [LoadStep::active("probing bwrap")];
        assert!(is_animating(&active));

        let failed = [LoadStep::failed("provisioning")];
        assert!(!is_animating(&failed));

        let pending = [LoadStep::pending("shell")];
        assert!(!is_animating(&pending));
    }

    #[test]
    fn podman_failure_then_bwrap_resolution_keeps_splash_without_a_ticker() {
        let key = ("app/home".to_string(), 0);
        let session = crate::session::Session {
            id: "test".into(),
            worktrees: vec![crate::session::WorktreeGroup::new(
                "app/home",
                crate::session::GroupKind::Home,
                "",
            )],
            active: 0,
        };
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let seed = crate::loading::catalog::generic_seed();
        let mut observer = crate::loading::observe::MaterializeObserver::from_steps(&seed);
        for event in [
            thegn_core::progress::SandboxPhase::Resolve,
            thegn_core::progress::SandboxPhase::ResolveProbe {
                backend: "podman-rootless".into(),
            },
            thegn_core::progress::SandboxPhase::PhaseDone,
            thegn_core::progress::SandboxPhase::ContainerCreate { backend: "podman" },
            thegn_core::progress::SandboxPhase::PhaseFailed {
                err: "could not start podman container".into(),
            },
            thegn_core::progress::SandboxPhase::Resolve,
            thegn_core::progress::SandboxPhase::ResolveProbe {
                backend: "bwrap".into(),
            },
            thegn_core::progress::SandboxPhase::PhaseDone,
        ] {
            tx.send((key.0.clone(), key.1, observer.on_event(event)))
                .unwrap();
        }
        drop(tx);
        let mut tracker = crate::loading::track::LoadingTracker::default();
        let mut remote = std::collections::HashMap::new();
        let retired = std::collections::HashSet::new();
        let mut perf = crate::perf::LoopPerf::new();

        crate::handlers::provision::drain_provision(
            &mut rx,
            &session,
            &mut tracker,
            &mut remote,
            &retired,
            &mut perf,
        );

        let (steps, ticker_visible) = crate::loading::derive_splash(&tracker, &session);
        assert!(
            !steps.is_empty(),
            "materialize shell-wait splash remains drawn"
        );
        assert_eq!(
            steps
                .iter()
                .find(|step| step.label == "container (podman)")
                .map(|s| s.state),
            None,
            "failed podman create row is removed when fallback resolves"
        );
        assert_eq!(steps.last().map(|step| step.label.as_str()), Some("shell"));
        assert!(crate::loading::should_clear_splash_on_output(
            &tracker, &key
        ));
        assert!(
            !ticker_visible,
            "no Active step means run.rs hides the ticker"
        );

        tracker.set(key.clone(), vec![LoadStep::active("provisioning")]);
        assert!(crate::loading::derive_splash(&tracker, &session).1);
        tracker.set(key, vec![LoadStep::done("provisioning")]);
        assert!(!crate::loading::derive_splash(&tracker, &session).1);
    }

    /// Poll until `cond` holds (a loaded test machine makes fixed sleeps
    /// flaky); panics with `what` after a generous deadline.
    fn wait_for(what: &str, cond: impl Fn() -> bool) {
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while !cond() {
            assert!(std::time::Instant::now() < deadline, "timed out: {what}");
            std::thread::sleep(Duration::from_millis(2));
        }
    }

    #[test]
    fn ticks_while_visible_and_stops_after_hidden() {
        let count = Arc::new(AtomicUsize::new(0));
        let c = count.clone();
        let t = SplashTicker::with_period(
            move || {
                c.fetch_add(1, Ordering::Relaxed);
            },
            Duration::from_millis(5),
        );
        t.set_visible(true);
        wait_for("ticks while visible", || count.load(Ordering::Relaxed) >= 3);
        t.set_visible(false);
        // The thread notices within one period and exits.
        wait_for("thread exits after hidden", || {
            !t.running.load(Ordering::Relaxed)
        });
        let settled = count.load(Ordering::Relaxed);
        std::thread::sleep(Duration::from_millis(40));
        assert_eq!(
            count.load(Ordering::Relaxed),
            settled,
            "no ticks after the splash cleared — the idle contract"
        );
    }

    #[test]
    fn revisibility_respawns_and_repeat_calls_do_not_stack() {
        let count = Arc::new(AtomicUsize::new(0));
        let c = count.clone();
        let t = SplashTicker::with_period(
            move || {
                c.fetch_add(1, Ordering::Relaxed);
            },
            Duration::from_millis(5),
        );
        // Idempotent while up: many calls, one thread (the CAS admits one).
        t.set_visible(true);
        t.set_visible(true);
        t.set_visible(true);
        wait_for("ticking", || count.load(Ordering::Relaxed) >= 2);
        t.set_visible(false);
        wait_for("thread exits", || !t.running.load(Ordering::Relaxed));
        let settled = count.load(Ordering::Relaxed);
        // A later splash brings it back.
        t.set_visible(true);
        wait_for("respawned", || count.load(Ordering::Relaxed) > settled);
        t.set_visible(false);
        wait_for("thread exits again", || !t.running.load(Ordering::Relaxed));
    }

    #[test]
    fn never_spawns_when_invisible() {
        let count = Arc::new(AtomicUsize::new(0));
        let c = count.clone();
        let t = SplashTicker::with_period(
            move || {
                c.fetch_add(1, Ordering::Relaxed);
            },
            Duration::from_millis(5),
        );
        t.set_visible(false);
        std::thread::sleep(Duration::from_millis(30));
        assert_eq!(count.load(Ordering::Relaxed), 0);
        assert!(!t.running.load(Ordering::Relaxed));
    }
}
