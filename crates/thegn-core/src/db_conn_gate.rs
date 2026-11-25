//! Process-wide bound on concurrently open `thegn.db` connections made by
//! `Db::open` (THE-2630).
//!
//! SQLite's unix VFS cannot `close()` a main-database fd while another
//! connection in the process holds a POSIX lock on the same inode, so it parks
//! the fd until the last connection closes. A startup burst of N concurrent
//! short-lived connections therefore leaves about N fds open for seconds.
//! Bounding the concurrency bounds the parked fds.
//!
//! The wait is bounded: a thread that nests opens (or a long-lived holder
//! pinning slots) proceeds after [`WAIT`] rather than deadlocking, and simply
//! runs without a slot.

use std::sync::{Condvar, Mutex};
use std::time::{Duration, Instant};

/// Concurrent `Db::open` connections allowed before callers wait.
pub const SLOTS: usize = 4;
/// Longest a caller waits for a slot before proceeding without one.
pub const WAIT: Duration = Duration::from_millis(250);

/// A counting gate with a bounded wait.
pub struct ConnGate {
    free: Mutex<usize>,
    ready: Condvar,
}

/// Returns its slot on drop.
pub struct ConnSlot {
    gate: &'static ConnGate,
}

impl ConnGate {
    pub const fn new(slots: usize) -> Self {
        Self {
            free: Mutex::new(slots),
            ready: Condvar::new(),
        }
    }

    /// Take a slot, waiting at most `wait`. `None` means the wait expired and
    /// the caller proceeds ungated.
    pub fn acquire(&'static self, wait: Duration) -> Option<ConnSlot> {
        let deadline = Instant::now() + wait;
        let mut free = self.free.lock().unwrap_or_else(|e| e.into_inner());
        while *free == 0 {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                tracing::debug!("db connection gate wait expired; opening ungated");
                return None;
            }
            let (guard, _) = self
                .ready
                .wait_timeout(free, left)
                .unwrap_or_else(|e| e.into_inner());
            free = guard;
        }
        *free -= 1;
        Some(ConnSlot { gate: self })
    }
}

impl Drop for ConnSlot {
    fn drop(&mut self) {
        let mut free = self.gate.free.lock().unwrap_or_else(|e| e.into_inner());
        *free += 1;
        self.gate.ready.notify_one();
    }
}

static GATE: ConnGate = ConnGate::new(SLOTS);

/// Take a slot from the process-wide gate for one `Db::open` connection.
pub(crate) fn acquire() -> Option<ConnSlot> {
    GATE.acquire(WAIT)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[test]
    fn slots_release_on_drop() {
        static G: ConnGate = ConnGate::new(1);
        let a = G.acquire(Duration::from_millis(10)).expect("first slot");
        assert!(G.acquire(Duration::from_millis(10)).is_none());
        drop(a);
        assert!(G.acquire(Duration::from_millis(10)).is_some());
    }

    #[test]
    fn wait_is_bounded() {
        static G: ConnGate = ConnGate::new(1);
        let _held = G.acquire(Duration::from_millis(10)).expect("slot");
        let start = Instant::now();
        assert!(G.acquire(Duration::from_millis(40)).is_none());
        let waited = start.elapsed();
        assert!(waited >= Duration::from_millis(30), "{waited:?}");
        assert!(waited < Duration::from_secs(5), "{waited:?}");
    }

    #[test]
    fn peak_concurrency_is_bounded() {
        static G: ConnGate = ConnGate::new(3);
        static LIVE: AtomicUsize = AtomicUsize::new(0);
        static PEAK: AtomicUsize = AtomicUsize::new(0);
        let threads: Vec<_> = (0..12)
            .map(|_| {
                std::thread::spawn(|| {
                    let slot = G.acquire(Duration::from_secs(30)).expect("slot");
                    let now = LIVE.fetch_add(1, Ordering::SeqCst) + 1;
                    PEAK.fetch_max(now, Ordering::SeqCst);
                    std::thread::sleep(Duration::from_millis(15));
                    LIVE.fetch_sub(1, Ordering::SeqCst);
                    drop(slot);
                })
            })
            .collect();
        for t in threads {
            t.join().expect("worker");
        }
        assert!(PEAK.load(Ordering::SeqCst) <= 3);
        assert!(PEAK.load(Ordering::SeqCst) >= 1);
    }
}
