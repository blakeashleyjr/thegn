//! Scoped cancellation probe shared by the fold and gate paths.
//!
//! A thread-local probe keeps cancellation attached to the one synchronous
//! fold worker that owns the gate process. Nested scopes restore their parent.

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Condvar, Mutex, PoisonError};
use std::time::{Duration, Instant};

thread_local! {
    static PROBE: RefCell<Option<Rc<dyn Fn() -> bool>>> = RefCell::new(None);
}

pub(crate) struct CancelScope {
    previous: Option<Rc<dyn Fn() -> bool>>,
}

pub(crate) fn scope(probe: impl Fn() -> bool + 'static) -> CancelScope {
    let probe: Rc<dyn Fn() -> bool> = Rc::new(probe);
    let previous = PROBE.with(|slot| slot.replace(Some(probe)));
    CancelScope { previous }
}

pub(crate) fn requested() -> bool {
    PROBE.with(|slot| slot.borrow().as_ref().is_some_and(|probe| probe()))
}

impl Drop for CancelScope {
    fn drop(&mut self) {
        PROBE.with(|slot| {
            drop(slot.replace(self.previous.take()));
        });
    }
}

/// Tracks synchronous merge workers owned by the interactive application.
/// Shutdown flips their probe and waits only for the supplied bounded period.
pub(crate) struct AppFoldWorkers {
    shutdown: AtomicBool,
    active: Mutex<usize>,
    idle: Condvar,
}

impl AppFoldWorkers {
    pub(crate) const fn new() -> Self {
        Self {
            shutdown: AtomicBool::new(false),
            active: Mutex::new(0),
            idle: Condvar::new(),
        }
    }

    fn enter(&self) -> AppFoldGuard<'_> {
        let mut active = self.active.lock().unwrap_or_else(PoisonError::into_inner);
        *active += 1;
        AppFoldGuard { workers: self }
    }

    pub(crate) fn cancelled(&self) -> bool {
        self.shutdown.load(Ordering::Acquire)
    }

    pub(crate) fn cancel_and_wait(&self, timeout: Duration) -> usize {
        self.shutdown.store(true, Ordering::Release);
        let deadline = Instant::now() + timeout;
        let mut active = self.active.lock().unwrap_or_else(PoisonError::into_inner);
        while *active > 0 {
            let now = Instant::now();
            if now >= deadline {
                break;
            }
            let wait = deadline.saturating_duration_since(now);
            let (next_active, result) = self
                .idle
                .wait_timeout(active, wait)
                .unwrap_or_else(PoisonError::into_inner);
            active = next_active;
            if result.timed_out() {
                break;
            }
        }
        *active
    }

    fn run_worker<T>(&'static self, f: impl FnOnce() -> T) -> T {
        let guard = self.enter();
        let scope = scope(move || self.cancelled());
        let result = f();
        drop(scope);
        drop(guard);
        result
    }
}

impl Default for AppFoldWorkers {
    fn default() -> Self {
        Self::new()
    }
}

struct AppFoldGuard<'a> {
    workers: &'a AppFoldWorkers,
}

impl Drop for AppFoldGuard<'_> {
    fn drop(&mut self) {
        let mut active = self
            .workers
            .active
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        *active -= 1;
        self.workers.idle.notify_all();
    }
}

pub(crate) static APP_FOLD_WORKERS: AppFoldWorkers = AppFoldWorkers::new();

pub(crate) fn run_app_worker<T>(f: impl FnOnce() -> T) -> T {
    APP_FOLD_WORKERS.run_worker(f)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;
    use std::rc::Rc;
    use std::sync::mpsc;
    use thegn_core::thread_name::spawn_named;

    #[test]
    fn no_scope_is_not_cancelled_and_nested_scope_restores_outer_probe() {
        assert!(!requested());
        let outer_value = Rc::new(Cell::new(false));
        let probe_value = Rc::clone(&outer_value);
        let _outer = scope(move || probe_value.get());
        assert!(!requested());
        outer_value.set(true);
        {
            let _inner = scope(|| true);
            assert!(requested());
        }
        assert!(requested());
    }

    #[test]
    fn cancel_without_workers_returns_zero_immediately() {
        let workers = Box::leak(Box::new(AppFoldWorkers::new()));

        assert_eq!(workers.cancel_and_wait(Duration::from_secs(1)), 0);
    }

    #[test]
    fn app_worker_observes_shutdown_and_exits_before_wait_deadline() {
        let workers = Box::leak(Box::new(AppFoldWorkers::new()));
        let (entered_tx, entered_rx) = mpsc::channel();
        let worker = spawn_named("thegn-fold-wait", move || {
            workers.run_worker(|| {
                entered_tx.send(()).unwrap();
                while !requested() {
                    std::thread::sleep(Duration::from_millis(1));
                }
            });
        });
        entered_rx.recv_timeout(Duration::from_secs(2)).unwrap();

        assert_eq!(workers.cancel_and_wait(Duration::from_secs(3)), 0);
        worker.join().unwrap();
    }

    #[test]
    fn shutdown_wait_reports_workers_that_ignore_the_probe() {
        let workers = Box::leak(Box::new(AppFoldWorkers::new()));
        let (entered_tx, entered_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let worker = spawn_named("thegn-fold-hold", move || {
            workers.run_worker(|| {
                entered_tx.send(()).unwrap();
                release_rx.recv().unwrap();
            });
        });
        entered_rx.recv_timeout(Duration::from_secs(2)).unwrap();

        assert_eq!(workers.cancel_and_wait(Duration::from_millis(50)), 1);
        release_tx.send(()).unwrap();
        worker.join().unwrap();
        assert_eq!(workers.cancel_and_wait(Duration::from_secs(1)), 0);
    }

    #[test]
    fn nested_scope_restores_the_app_worker_probe() {
        let workers = Box::leak(Box::new(AppFoldWorkers::new()));
        let (entered_tx, entered_rx) = mpsc::channel();
        let worker = spawn_named("thegn-fold-nest", move || {
            workers.run_worker(|| {
                entered_tx.send(()).unwrap();
                while !workers.cancelled() {
                    std::thread::sleep(Duration::from_millis(1));
                }
                {
                    let _inner = scope(|| false);
                    assert!(!requested());
                }
                assert!(requested());
            });
        });
        entered_rx.recv_timeout(Duration::from_secs(2)).unwrap();

        assert_eq!(workers.cancel_and_wait(Duration::from_secs(3)), 0);
        worker.join().unwrap();
    }
}
