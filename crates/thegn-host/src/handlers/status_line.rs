//! Lifecycle of the bottom-bar status message (`FrameModel::status`).
//!
//! The status slot is **loop-owned**: hydration never seeds it and the mode
//! chip only *replaces* it when a non-default input mode is entered. Every
//! other writer (`model.status = …` after a user action, a crash alert, a
//! config error) gets a fixed lifetime — [`STATUS_TTL`] — after which the slot
//! reverts to the mode text (empty in `Normal`). Expiry is driven by a one-shot
//! waker pulse from the single replaceable-deadline [`StatusScheduler`], so an
//! idle loop (which never polls on a timer) still clears it on time.
//!
//! Before this tracker existed the slot had no lifetime at all *and* was wiped
//! by every hydration swap (`build_model` seeded a startup line that defeated
//! the "restore if empty" guard, and `apply_mode_status` then blanked it), so a
//! message lived anywhere between 0 ms and the next 5 s tick. Messages posted
//! in the same iteration as a hydration (`"rebase finished"`) never rendered.

use std::sync::{Arc, Condvar, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use crate::chrome::FrameModel;
use crate::keymap::Mode;

/// How long a transient status message stays on the bar.
pub(crate) const STATUS_TTL: Duration = Duration::from_secs(8);

/// The status text a mode contributes when nothing else is showing.
pub(crate) fn mode_text(mode: Mode) -> String {
    crate::i18n_surface::mode(mode, crate::i18n_surface::ModeStyle::Status)
}

/// True when `s` is one of the mode strings (so it must not be subject to the
/// TTL and may be replaced on a mode switch).
pub(crate) fn is_mode_text(s: &str) -> bool {
    s.is_empty()
        || [Mode::VimNormal, Mode::VimInsert, Mode::Emacs]
            .iter()
            .any(|m| mode_text(*m) == s)
}

/// Reflect an input-mode change in the status slot. A transient message that
/// is currently showing is left alone — the mode chip is always visible, and
/// the message will revert to the mode text when its TTL lapses.
pub(crate) fn apply_mode(model: &mut FrameModel, mode: Mode) {
    if is_mode_text(&model.status) {
        model.status = mode_text(mode);
    }
}

/// Loop-local tracker; call [`StatusLine::tick`] once per iteration, right
/// before rendering. Timer commands are generation-tagged so replacement and
/// expiry can be validated independently of the model's String value.
#[derive(Debug, Default)]
pub(crate) struct StatusLine {
    last: String,
    since: Option<Instant>,
    generation: u64,
}

/// The only commands the loop sends to the status timer. Replacing a message
/// replaces one deadline in the timer owner; it never queues one command per
/// message.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum StatusTimerCommand {
    Arm { generation: u64, delay: Duration },
    Cancel,
}

#[derive(Default)]
struct TimerState {
    deadline: Option<(u64, Instant)>,
    expired: Option<u64>,
    stopping: bool,
}

impl TimerState {
    fn arm(&mut self, generation: u64, now: Instant, delay: Duration) {
        let deadline = now.checked_add(delay).unwrap_or(now);
        self.deadline = Some((generation, deadline));
        self.expired = None;
    }

    fn cancel(&mut self) {
        self.deadline = None;
        self.expired = None;
    }

    fn stop(&mut self) {
        self.stopping = true;
        self.cancel();
    }

    fn due(&mut self, now: Instant) -> Option<u64> {
        let (generation, deadline) = self.deadline?;
        if now < deadline {
            return None;
        }
        self.deadline = None;
        self.expired = Some(generation);
        Some(generation)
    }

    fn take_expired(&mut self) -> Option<u64> {
        self.expired.take()
    }
}

/// One loop-owned timer worker. Its state has one replaceable deadline and one
/// replaceable expiry slot; an idle worker waits on the Condvar indefinitely.
pub(crate) struct StatusScheduler {
    state: Arc<(Mutex<TimerState>, Condvar)>,
    worker: Option<JoinHandle<()>>,
}

impl StatusScheduler {
    pub(crate) fn new(waker: termwiz::terminal::TerminalWaker) -> Self {
        Self::with_wake(Box::new(move || {
            if let Err(error) = waker.wake() {
                tracing::debug!(target: "thegn::status_timer", %error, "status timer wake failed");
            }
        }))
    }

    fn with_wake(wake: Box<dyn Fn() + Send + 'static>) -> Self {
        Self::with_wake_started(wake, None)
    }

    #[cfg(test)]
    fn with_test_wake(
        wake: Box<dyn Fn() + Send + 'static>,
        started: Arc<std::sync::atomic::AtomicUsize>,
    ) -> Self {
        Self::with_wake_started(wake, Some(started))
    }

    fn with_wake_started(
        wake: Box<dyn Fn() + Send + 'static>,
        _started: Option<Arc<std::sync::atomic::AtomicUsize>>,
    ) -> Self {
        let state = Arc::new((Mutex::new(TimerState::default()), Condvar::new()));
        let worker_state = Arc::clone(&state);
        #[cfg(test)]
        let started_for_worker = _started.clone();
        let worker = std::thread::Builder::new()
            .name("status-timer".into())
            .spawn(move || {
                crate::platform::qos::set_self(crate::platform::qos::Qos::Background);
                #[cfg(test)]
                if let Some(started) = started_for_worker {
                    started.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                }
                timer_worker(worker_state, wake);
            });
        let worker = match worker {
            Ok(worker) => Some(worker),
            Err(error) => {
                tracing::warn!(target: "thegn::status_timer", %error, "status timer worker unavailable; expiry will be checked on later wakes");
                None
            }
        };
        Self { state, worker }
    }

    pub(crate) fn apply(&self, command: StatusTimerCommand) {
        let (lock, wake) = &*self.state;
        let Ok(mut state) = lock.lock() else {
            tracing::warn!(target: "thegn::status_timer", "status timer state poisoned; command dropped");
            return;
        };
        if state.stopping {
            tracing::debug!(target: "thegn::status_timer", "status timer command ignored after shutdown");
            return;
        }
        match command {
            StatusTimerCommand::Arm { generation, delay } => {
                state.arm(generation, Instant::now(), delay);
            }
            StatusTimerCommand::Cancel => state.cancel(),
        }
        wake.notify_all();
    }

    pub(crate) fn take_expired(&self) -> Option<u64> {
        let (lock, _) = &*self.state;
        match lock.lock() {
            Ok(mut state) => state.take_expired(),
            Err(_) => {
                tracing::warn!(target: "thegn::status_timer", "status timer state poisoned; expiry dropped");
                None
            }
        }
    }

    pub(crate) fn shutdown(&mut self) {
        let (lock, wake) = &*self.state;
        {
            let mut state = match lock.lock() {
                Ok(state) => state,
                Err(poisoned) => {
                    tracing::warn!(target: "thegn::status_timer", "status timer state poisoned during shutdown; recovering");
                    poisoned.into_inner()
                }
            };
            state.stop();
            wake.notify_all();
        }
        if let Some(worker) = self.worker.take()
            && worker.join().is_err()
        {
            tracing::warn!(target: "thegn::status_timer", "status timer worker panicked during shutdown");
        }
    }
}

impl Drop for StatusScheduler {
    fn drop(&mut self) {
        self.shutdown();
    }
}

fn timer_worker(
    state: Arc<(Mutex<TimerState>, Condvar)>,
    wake_expired: Box<dyn Fn() + Send + 'static>,
) {
    let (lock, wake) = &*state;
    let Ok(mut state) = lock.lock() else {
        tracing::warn!(target: "thegn::status_timer", "status timer state poisoned at worker start");
        return;
    };
    loop {
        while !state.stopping && state.deadline.is_none() {
            state = match wake.wait(state) {
                Ok(state) => state,
                Err(_) => {
                    tracing::warn!(target: "thegn::status_timer", "status timer Condvar wait poisoned");
                    return;
                }
            };
        }
        if state.stopping {
            return;
        }
        let Some((generation, deadline)) = state.deadline else {
            continue;
        };
        let now = Instant::now();
        if now < deadline {
            state = match wake.wait_timeout(state, deadline.saturating_duration_since(now)) {
                Ok((state, _)) => state,
                Err(_) => {
                    tracing::warn!(target: "thegn::status_timer", "status timer timed wait poisoned");
                    return;
                }
            };
            continue;
        }
        if state.deadline == Some((generation, deadline)) && state.due(now).is_some() {
            drop(state);
            wake_expired();
            state = match lock.lock() {
                Ok(state) => state,
                Err(_) => {
                    tracing::warn!(target: "thegn::status_timer", "status timer state poisoned after wake");
                    return;
                }
            };
        }
    }
}

impl StatusLine {
    /// Observe the current status. Returns `true` when the tracker changed the
    /// model (an expired message was cleared) and a repaint is needed.
    ///
    /// `schedule` receives one generation-tagged timer command when a new
    /// message or mode transition changes the deadline.
    pub(crate) fn tick(
        &mut self,
        model: &mut FrameModel,
        mode: Mode,
        now: Instant,
        mut schedule: impl FnMut(StatusTimerCommand),
    ) -> bool {
        if model.status != self.last {
            // A new message landed this iteration: start its clock.
            self.last = model.status.clone();
            self.generation = self
                .generation
                .checked_add(1)
                .expect("status timer generation exhausted");
            if is_mode_text(&model.status) {
                self.since = None;
                schedule(StatusTimerCommand::Cancel);
            } else {
                self.since = Some(now);
                schedule(StatusTimerCommand::Arm {
                    generation: self.generation,
                    delay: STATUS_TTL + Duration::from_millis(50),
                });
            }
            return false;
        }
        match self.since {
            Some(t0) if now.duration_since(t0) >= STATUS_TTL => {
                model.status = mode_text(mode);
                self.last = model.status.clone();
                self.since = None;
                schedule(StatusTimerCommand::Cancel);
                true
            }
            _ => false,
        }
    }

    /// Apply a semantic expiry pulse. The generation check prevents an old
    /// wake from clearing a newer message that replaced its deadline.
    pub(crate) fn expire(
        &mut self,
        model: &mut FrameModel,
        mode: Mode,
        generation: u64,
        now: Instant,
    ) -> bool {
        if generation != self.generation {
            return false;
        }
        let Some(t0) = self.since else { return false };
        if now.duration_since(t0) < STATUS_TTL {
            return false;
        }
        model.status = mode_text(mode);
        self.last = model.status.clone();
        self.since = None;
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn model_with(status: &str) -> FrameModel {
        FrameModel {
            status: status.into(),
            ..FrameModel::default()
        }
    }

    #[test]
    fn message_survives_until_ttl_then_reverts_to_mode_text() {
        let mut sl = StatusLine::default();
        let mut m = model_with("Copied log line");
        let t0 = Instant::now();
        let mut scheduled = Vec::new();
        assert!(!sl.tick(&mut m, Mode::Normal, t0, |d| scheduled.push(d)));
        assert_eq!(scheduled.len(), 1, "one wake scheduled per message");
        // Half-way: untouched, no extra wake.
        assert!(!sl.tick(&mut m, Mode::Normal, t0 + STATUS_TTL / 2, |d| {
            scheduled.push(d)
        }));
        assert_eq!(m.status, "Copied log line");
        assert_eq!(scheduled.len(), 1);
        // Past TTL: cleared, repaint requested.
        assert!(sl.tick(&mut m, Mode::Normal, t0 + STATUS_TTL, |d| scheduled.push(d)));
        assert_eq!(m.status, "");
    }

    #[test]
    fn expiry_reverts_to_the_mode_string_not_empty() {
        let mut sl = StatusLine::default();
        let mut m = model_with("rebase finished");
        let t0 = Instant::now();
        sl.tick(&mut m, Mode::VimNormal, t0, |_| {});
        assert!(sl.tick(&mut m, Mode::VimNormal, t0 + STATUS_TTL, |_| {}));
        assert_eq!(m.status, "VimNormal mode");
    }

    #[test]
    fn identical_payload_does_not_refresh_or_add_a_timer() {
        let mut sl = StatusLine::default();
        let mut m = model_with("same message");
        let t0 = Instant::now();
        let mut scheduled = Vec::new();
        sl.tick(&mut m, Mode::Normal, t0, |command| scheduled.push(command));
        sl.tick(&mut m, Mode::Normal, t0 + STATUS_TTL / 2, |command| {
            scheduled.push(command)
        });
        assert_eq!(scheduled.len(), 1);
        assert_eq!(m.status, "same message");
    }

    #[test]
    fn stale_expiry_generation_cannot_clear_a_newer_message() {
        let mut sl = StatusLine::default();
        let mut m = model_with("first");
        let t0 = Instant::now();
        let mut scheduled = Vec::new();
        sl.tick(&mut m, Mode::Normal, t0, |command| scheduled.push(command));
        let first_generation = match scheduled[0] {
            StatusTimerCommand::Arm { generation, .. } => generation,
            StatusTimerCommand::Cancel => panic!("transient message was canceled"),
        };
        m.status = "second".into();
        sl.tick(
            &mut m,
            Mode::Normal,
            t0 + Duration::from_secs(1),
            |command| scheduled.push(command),
        );
        assert!(!sl.expire(
            &mut m,
            Mode::Normal,
            first_generation,
            t0 + STATUS_TTL + Duration::from_secs(1),
        ));
        assert_eq!(m.status, "second");
        assert!(sl.expire(
            &mut m,
            Mode::Normal,
            first_generation + 1,
            t0 + STATUS_TTL + Duration::from_secs(1),
        ));
        assert_eq!(m.status, "");
    }

    #[test]
    fn mode_transition_cancels_the_active_deadline() {
        let mut sl = StatusLine::default();
        let mut m = model_with("transient");
        let t0 = Instant::now();
        let mut scheduled = Vec::new();
        sl.tick(&mut m, Mode::Normal, t0, |command| scheduled.push(command));
        m.status = mode_text(Mode::VimInsert);
        sl.tick(
            &mut m,
            Mode::VimInsert,
            t0 + Duration::from_secs(1),
            |command| scheduled.push(command),
        );
        assert!(matches!(scheduled[0], StatusTimerCommand::Arm { .. }));
        assert_eq!(scheduled[1], StatusTimerCommand::Cancel);
    }

    #[test]
    fn timer_state_keeps_only_the_latest_deadline_and_expiry() {
        let mut state = TimerState::default();
        let t0 = Instant::now();
        state.arm(1, t0, Duration::from_secs(8));
        assert_eq!(state.due(t0 + Duration::from_secs(7)), None);
        assert_eq!(state.take_expired(), None);
        state.arm(2, t0, Duration::from_secs(9));
        assert_eq!(state.due(t0 + Duration::from_secs(8)), None);
        assert_eq!(state.take_expired(), None);
        assert_eq!(state.due(t0 + Duration::from_secs(9)), Some(2));
        assert_eq!(state.take_expired(), Some(2));
        state.arm(3, t0, Duration::from_secs(8));
        state.cancel();
        assert_eq!(state.due(t0 + Duration::from_secs(8)), None);
        state.arm(4, t0, Duration::from_secs(8));
        state.stop();
        assert!(state.stopping);
        assert_eq!(state.deadline, None);
        assert_eq!(state.take_expired(), None);
    }

    #[test]
    fn scheduler_worker_wakes_through_injectable_sink_and_stops_promptly() {
        let wakes = Arc::new((Mutex::new(0usize), Condvar::new()));
        let starts = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let wake_sink = Arc::clone(&wakes);
        let mut scheduler = StatusScheduler::with_test_wake(
            Box::new(move || {
                let (lock, wake) = &*wake_sink;
                *lock.lock().unwrap() += 1;
                wake.notify_all();
            }),
            Arc::clone(&starts),
        );
        for generation in 1..128 {
            scheduler.apply(StatusTimerCommand::Arm {
                generation,
                delay: Duration::from_secs(3600),
            });
        }
        scheduler.apply(StatusTimerCommand::Arm {
            generation: 128,
            delay: Duration::ZERO,
        });
        let (lock, wake) = &*wakes;
        loop {
            let mut count = lock.lock().unwrap();
            while *count == 0 {
                let (next, result) = wake.wait_timeout(count, Duration::from_secs(1)).unwrap();
                count = next;
                assert!(!result.timed_out(), "timer worker did not wake");
            }
            *count = 0;
            drop(count);
            if scheduler.take_expired() == Some(128) {
                break;
            }
        }
        assert_eq!(starts.load(std::sync::atomic::Ordering::Relaxed), 1);
        scheduler.shutdown();
        assert!(scheduler.worker.is_none());
    }

    #[test]
    fn poisoned_waiter_shutdown_recovers_and_returns_boundedly() {
        let starts = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let mut scheduler = StatusScheduler::with_test_wake(Box::new(|| {}), Arc::clone(&starts));
        scheduler.apply(StatusTimerCommand::Arm {
            generation: 1,
            delay: Duration::from_secs(3600),
        });
        let start_deadline = std::time::Instant::now() + Duration::from_secs(1);
        while starts.load(std::sync::atomic::Ordering::Relaxed) == 0 {
            assert!(
                std::time::Instant::now() < start_deadline,
                "timer worker did not start"
            );
            std::thread::yield_now();
        }

        // Poison the mutex from a separate owner. The worker is parked on its
        // long deadline; shutdown must recover the guard, set stopping, and
        // notify it before joining.
        let state = Arc::clone(&scheduler.state);
        let poisoner = std::thread::spawn(move || {
            let _guard = state.0.lock().unwrap();
            panic!("intentional status timer mutex poison");
        });
        assert!(poisoner.join().is_err());

        let (done_tx, done_rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            scheduler.shutdown();
            done_tx.send(()).expect("shutdown observer still waiting");
        });
        done_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("poisoned status timer shutdown hung");
    }

    #[test]
    fn scheduler_cancel_and_shutdown_do_not_emit_late_wakes() {
        let wakes = Arc::new(Mutex::new(0usize));
        let wake_sink = Arc::clone(&wakes);
        let mut scheduler = StatusScheduler::with_wake(Box::new(move || {
            *wake_sink.lock().unwrap() += 1;
        }));
        scheduler.apply(StatusTimerCommand::Arm {
            generation: 1,
            delay: Duration::from_secs(3600),
        });
        scheduler.apply(StatusTimerCommand::Cancel);
        scheduler.apply(StatusTimerCommand::Arm {
            generation: 2,
            delay: Duration::from_secs(3600),
        });
        scheduler.shutdown();
        scheduler.apply(StatusTimerCommand::Arm {
            generation: 3,
            delay: Duration::ZERO,
        });
        assert_eq!(*wakes.lock().unwrap(), 0);
        assert_eq!(scheduler.take_expired(), None);
    }

    #[test]
    fn status_burst_advances_one_latest_generation() {
        let mut sl = StatusLine::default();
        let t0 = Instant::now();
        let mut latest = None;
        let mut m = model_with("message-0");
        for n in 0..128 {
            m.status = format!("message-{n}");
            sl.tick(&mut m, Mode::Normal, t0, |command| latest = Some(command));
        }
        assert!(matches!(
            latest,
            Some(StatusTimerCommand::Arm {
                generation: 128,
                ..
            })
        ));
        assert_eq!(m.status, "message-127");
    }

    #[test]
    fn mode_text_has_no_ttl() {
        let mut sl = StatusLine::default();
        let mut m = model_with("Emacs mode");
        let t0 = Instant::now();
        let mut commands = Vec::new();
        sl.tick(&mut m, Mode::Emacs, t0, |command| commands.push(command));
        assert_eq!(commands, [StatusTimerCommand::Cancel]);
        assert!(!sl.tick(&mut m, Mode::Emacs, t0 + STATUS_TTL * 10, |_| {}));
        assert_eq!(m.status, "Emacs mode");
    }

    #[test]
    fn a_newer_message_restarts_the_clock() {
        let mut sl = StatusLine::default();
        let mut m = model_with("first");
        let t0 = Instant::now();
        sl.tick(&mut m, Mode::Normal, t0, |_| {});
        m.status = "second".into();
        sl.tick(
            &mut m,
            Mode::Normal,
            t0 + STATUS_TTL - Duration::from_secs(1),
            |_| {},
        );
        // The first message's deadline passes; the second is still young.
        assert!(!sl.tick(&mut m, Mode::Normal, t0 + STATUS_TTL, |_| {}));
        assert_eq!(m.status, "second");
    }

    #[test]
    fn apply_mode_does_not_clobber_a_live_message() {
        let mut m = model_with("Config error: bad toml");
        apply_mode(&mut m, Mode::Normal);
        assert_eq!(m.status, "Config error: bad toml");
        apply_mode(&mut m, Mode::VimNormal);
        assert_eq!(m.status, "Config error: bad toml");
        let mut m = model_with("VimNormal mode");
        apply_mode(&mut m, Mode::Normal);
        assert_eq!(m.status, "");
        apply_mode(&mut m, Mode::Emacs);
        assert_eq!(m.status, "Emacs mode");
    }
}
