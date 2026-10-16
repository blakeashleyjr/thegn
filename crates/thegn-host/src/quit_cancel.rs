//! Cancellation and bounded settlement for work that must stop before host exit.

use std::sync::{
    Condvar, Mutex,
    atomic::{AtomicBool, Ordering},
};
use std::time::Instant;

#[derive(Default)]
pub(crate) struct QuitLatch {
    cancel: AtomicBool,
    in_flight: Mutex<usize>,
    idle: Condvar,
}

impl QuitLatch {
    pub(crate) fn signal(&self) {
        self.cancel.store(true, Ordering::Relaxed);
    }

    #[cfg(test)]
    pub(crate) fn is_signalled(&self) -> bool {
        self.cancel.load(Ordering::Relaxed)
    }

    pub(crate) fn cancel_flag(&self) -> &AtomicBool {
        &self.cancel
    }

    pub(crate) fn enter(&self) -> InFlight<'_> {
        *self
            .in_flight
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) += 1;
        InFlight { latch: self }
    }

    pub(crate) fn wait_idle(&self, deadline: Instant) -> bool {
        let mut count = self
            .in_flight
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        while *count != 0 {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return false;
            }
            let (next, result) = self
                .idle
                .wait_timeout(count, remaining)
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            count = next;
            if result.timed_out() && *count != 0 {
                return false;
            }
        }
        true
    }
}

pub(crate) struct InFlight<'a> {
    latch: &'a QuitLatch,
}

impl Drop for InFlight<'_> {
    fn drop(&mut self) {
        let mut count = self
            .latch
            .in_flight
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        *count = count.saturating_sub(1);
        if *count == 0 {
            self.latch.idle.notify_all();
        }
    }
}

fn custom_git_latch() -> &'static QuitLatch {
    static LATCH: std::sync::OnceLock<QuitLatch> = std::sync::OnceLock::new();
    LATCH.get_or_init(QuitLatch::default)
}

pub(crate) fn custom_git() -> &'static QuitLatch {
    custom_git_latch()
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct QuitSettlement {
    pub(crate) settled_custom: bool,
    pub(crate) settled_ci: bool,
}

pub(crate) fn signal_all() {
    custom_git().signal();
    crate::ci_refresh::cancel_for_shutdown();
}

pub(crate) fn wait_or_detach(deadline: Instant) -> QuitSettlement {
    QuitSettlement {
        settled_custom: custom_git().wait_idle(deadline),
        settled_ci: crate::ci_refresh::wait_ci_idle(deadline),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::thread;
    use std::time::Duration;

    #[test]
    fn idle_wait_signal_and_guard_drop_are_observable() {
        let latch = Arc::new(QuitLatch::default());
        assert!(latch.wait_idle(Instant::now() + Duration::from_millis(50)));
        thread::scope(|scope| {
            let guard = latch.enter();
            assert!(!latch.wait_idle(Instant::now() + Duration::from_millis(10)));
            let other = Arc::clone(&latch);
            let worker = scope.spawn(move || {
                thread::sleep(Duration::from_millis(20));
                drop(guard);
                other
            });
            assert!(latch.wait_idle(Instant::now() + Duration::from_secs(1)));
            let returned = worker.join().expect("worker");
            returned.signal();
            assert!(returned.is_signalled());
            assert!(returned.cancel_flag().load(Ordering::Relaxed));
        });
    }
}
