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

/// One spinner frame per tick (see `plan::SPINNER_FRAME_MS`).
const PERIOD: Duration = Duration::from_millis(250);

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
