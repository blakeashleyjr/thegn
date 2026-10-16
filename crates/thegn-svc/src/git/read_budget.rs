//! Scheduling and deadline budgets shared by local git reads.

use std::cell::Cell;
use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, OnceLock};
use std::time::{Duration, Instant};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ReadPriority {
    Foreground,
    Background,
}

thread_local! {
    static CURRENT_PRIORITY: Cell<ReadPriority> = const { Cell::new(ReadPriority::Foreground) };
}

struct PriorityRestore(ReadPriority);

impl Drop for PriorityRestore {
    fn drop(&mut self) {
        CURRENT_PRIORITY.with(|priority| priority.set(self.0));
    }
}

pub fn with_read_priority<T>(priority: ReadPriority, f: impl FnOnce() -> T) -> T {
    let previous = CURRENT_PRIORITY.with(|current| current.replace(priority));
    let _restore = PriorityRestore(previous);
    f()
}

pub fn current_read_priority() -> ReadPriority {
    CURRENT_PRIORITY.with(Cell::get)
}

fn status_thread_limit(priority: ReadPriority, available: usize) -> Option<usize> {
    Some(match priority {
        ReadPriority::Foreground => available.clamp(1, 4),
        ReadPriority::Background => 1,
    })
}

pub(super) fn budgeted_status<'repo>(
    repo: &'repo gix::Repository,
) -> anyhow::Result<gix::status::Platform<'repo, gix::progress::Discard>> {
    let available = std::thread::available_parallelism()
        .map(usize::from)
        .unwrap_or(1);
    let status = repo
        .status(gix::progress::Discard)?
        .index_worktree_options_mut(|options| {
            options.thread_limit = status_thread_limit(current_read_priority(), available);
        });
    Ok(status)
}

struct Watchdog {
    registrations: Mutex<BTreeMap<(Instant, u64), Arc<AtomicBool>>>,
    changed: Condvar,
    next_id: AtomicU64,
}

impl Watchdog {
    fn new() -> Self {
        Self {
            registrations: Mutex::new(BTreeMap::new()),
            changed: Condvar::new(),
            next_id: AtomicU64::new(0),
        }
    }

    fn register(self: &Arc<Self>, deadline: Instant, flag: Arc<AtomicBool>) -> Registration {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let key = (deadline, id);
        let notify = {
            let mut registrations = self.registrations.lock().unwrap();
            let was_earliest = registrations
                .first_key_value()
                .is_none_or(|(earliest, _)| key < *earliest);
            if registrations.insert(key, flag).is_some() {
                unreachable!("deadline registration ids must be unique");
            }
            was_earliest
        };
        if notify {
            self.changed.notify_one();
        }
        Registration {
            watchdog: Arc::clone(self),
            key,
        }
    }
}

struct Registration {
    watchdog: Arc<Watchdog>,
    key: (Instant, u64),
}

impl Drop for Registration {
    fn drop(&mut self) {
        let removed_earliest = {
            let mut registrations = self.watchdog.registrations.lock().unwrap();
            let was_earliest = registrations
                .first_key_value()
                .is_some_and(|(key, _)| *key == self.key);
            let removed = registrations.remove(&self.key).is_some();
            was_earliest && removed
        };
        if removed_earliest {
            self.watchdog.changed.notify_one();
        }
    }
}

static WATCHDOG: OnceLock<Option<Arc<Watchdog>>> = OnceLock::new();

fn watchdog() -> Option<&'static Arc<Watchdog>> {
    WATCHDOG
        .get_or_init(|| {
            let watchdog = Arc::new(Watchdog::new());
            let worker_watchdog = Arc::clone(&watchdog);
            match std::thread::Builder::new()
                .name("git-read-deadline".into())
                .spawn(move || run_watchdog(worker_watchdog))
            {
                Ok(thread) => {
                    #[cfg(test)]
                    WATCHDOG_SPAWN_COUNT.fetch_add(1, Ordering::SeqCst);
                    drop(thread);
                    Some(watchdog)
                }
                Err(error) => {
                    tracing::warn!(target: "thegn::git", %error, "could not start git read deadline watchdog");
                    None
                }
            }
        })
        .as_ref()
}

fn run_watchdog(watchdog: Arc<Watchdog>) {
    loop {
        let mut registrations = watchdog.registrations.lock().unwrap();
        loop {
            let Some((&(deadline, _), _)) = registrations.first_key_value() else {
                registrations = watchdog.changed.wait(registrations).unwrap();
                continue;
            };
            let now = Instant::now();
            if deadline <= now {
                let expired: Vec<_> = registrations
                    .range(..=(now, u64::MAX))
                    .map(|(key, _)| *key)
                    .collect();
                for key in expired {
                    if let Some(flag) = registrations.remove(&key) {
                        flag.store(true, Ordering::SeqCst);
                    }
                }
                continue;
            }
            let wait = deadline.saturating_duration_since(now);
            let (next, _) = watchdog.changed.wait_timeout(registrations, wait).unwrap();
            registrations = next;
        }
    }
}

pub(super) fn with_read_deadline<T>(
    timeout: Option<Duration>,
    f: impl FnOnce(&Arc<AtomicBool>) -> T,
) -> T {
    let flag = Arc::new(AtomicBool::new(false));
    let Some(timeout) = timeout else {
        return f(&flag);
    };
    let registration = Instant::now().checked_add(timeout).and_then(|deadline| {
        watchdog().map(|watchdog| watchdog.register(deadline, Arc::clone(&flag)))
    });
    let out = f(&flag);
    drop(registration);
    out
}

#[cfg(test)]
static WATCHDOG_SPAWN_COUNT: AtomicU64 = AtomicU64::new(0);

#[cfg(test)]
fn registration_count() -> usize {
    watchdog()
        .map(|watchdog| watchdog.registrations.lock().unwrap().len())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::{
        ReadPriority, WATCHDOG_SPAWN_COUNT, current_read_priority, registration_count,
        status_thread_limit, with_read_deadline, with_read_priority,
    };
    use std::sync::atomic::Ordering;
    use std::sync::{Arc, Barrier, Mutex};
    use std::time::{Duration, Instant};

    static DEADLINE_TEST_LOCK: Mutex<()> = Mutex::new(());

    #[test]
    fn background_status_is_single_threaded_foreground_is_capped() {
        for available in [1, 2, 8, 64] {
            assert_eq!(
                status_thread_limit(ReadPriority::Background, available),
                Some(1)
            );
            let foreground = status_thread_limit(ReadPriority::Foreground, available);
            assert!(foreground.is_some_and(|limit| (1..=4).contains(&limit)));
        }
    }

    #[test]
    fn priority_scope_nests_and_restores_after_panic() {
        assert_eq!(current_read_priority(), ReadPriority::Foreground);
        let result = std::panic::catch_unwind(|| {
            with_read_priority(ReadPriority::Background, || {
                assert_eq!(current_read_priority(), ReadPriority::Background);
                with_read_priority(ReadPriority::Foreground, || {
                    assert_eq!(current_read_priority(), ReadPriority::Foreground);
                });
                assert_eq!(current_read_priority(), ReadPriority::Background);
                panic!("exercise priority restoration");
            });
        });
        assert!(result.is_err());
        assert_eq!(current_read_priority(), ReadPriority::Foreground);
    }

    #[test]
    fn many_deadline_reads_share_one_watchdog_thread() {
        let _lock = DEADLINE_TEST_LOCK.lock().unwrap();
        let initial = WATCHDOG_SPAWN_COUNT.load(Ordering::SeqCst);
        for _ in 0..50 {
            with_read_deadline(Some(Duration::from_secs(30)), |flag| {
                assert!(!flag.load(Ordering::SeqCst));
            });
        }
        let barrier = Arc::new(Barrier::new(9));
        let workers: Vec<_> = (0..8)
            .map(|_| {
                let barrier = Arc::clone(&barrier);
                std::thread::spawn(move || {
                    barrier.wait();
                    with_read_deadline(Some(Duration::from_secs(30)), |flag| {
                        assert!(!flag.load(Ordering::SeqCst));
                    });
                })
            })
            .collect();
        barrier.wait();
        for worker in workers {
            worker.join().unwrap();
        }
        assert!(WATCHDOG_SPAWN_COUNT.load(Ordering::SeqCst) - initial <= 1);
        assert_eq!(registration_count(), 0);
    }

    #[test]
    fn overlapping_deadlines_raise_only_the_expired_one() {
        let _lock = DEADLINE_TEST_LOCK.lock().unwrap();
        let (long_ready_tx, long_ready_rx) = std::sync::mpsc::channel();
        let (short_expired_tx, short_expired_rx) = std::sync::mpsc::channel();
        let long = std::thread::spawn(move || {
            with_read_deadline(Some(Duration::from_secs(30)), |flag| {
                long_ready_tx.send(()).unwrap();
                let short_expired = short_expired_rx
                    .recv_timeout(Duration::from_secs(5))
                    .unwrap();
                (short_expired, !flag.load(Ordering::SeqCst))
            })
        });
        long_ready_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        let short = std::thread::spawn(move || {
            let raised = with_read_deadline(Some(Duration::from_millis(30)), |flag| {
                let start = std::time::Instant::now();
                while !flag.load(Ordering::SeqCst) && start.elapsed() < Duration::from_secs(5) {
                    std::thread::sleep(Duration::from_millis(2));
                }
                flag.load(Ordering::SeqCst)
            });
            short_expired_tx.send(raised).unwrap();
            raised
        });
        assert!(short.join().unwrap());
        assert_eq!(long.join().unwrap(), (true, true));
        assert_eq!(registration_count(), 0);
    }

    #[test]
    fn read_deadline_raises_flag_for_a_blocked_read() {
        let _lock = DEADLINE_TEST_LOCK.lock().unwrap();
        let raised = with_read_deadline(Some(Duration::from_millis(30)), |flag| {
            let start = Instant::now();
            while !flag.load(Ordering::SeqCst) && start.elapsed() < Duration::from_secs(5) {
                std::thread::sleep(Duration::from_millis(5));
            }
            flag.load(Ordering::SeqCst)
        });
        assert!(raised, "deadline must interrupt a read that never finishes");
        assert_eq!(registration_count(), 0);
    }

    #[test]
    fn read_deadline_stays_down_for_a_fast_read_or_no_budget() {
        let _lock = DEADLINE_TEST_LOCK.lock().unwrap();
        let fast = with_read_deadline(Some(Duration::from_secs(30)), |flag| {
            flag.load(Ordering::SeqCst)
        });
        assert!(!fast);
        let off = with_read_deadline(None, |flag| flag.load(Ordering::SeqCst));
        assert!(!off);
        assert_eq!(registration_count(), 0);
    }

    #[test]
    fn every_status_walk_goes_through_the_budget() {
        let mod_source = include_str!("mod.rs");
        let native_source = include_str!("native_diff.rs");
        for source in [mod_source, native_source] {
            let production = source.split("#[cfg(test)]").next().unwrap_or(source);
            assert_eq!(
                production
                    .matches(".status(gix::progress::Discard)")
                    .count(),
                0
            );
        }
    }
}
