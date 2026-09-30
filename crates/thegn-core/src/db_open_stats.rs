//! How often, and how expensively, the **event loop** opens the database.
//!
//! # Why this exists before any refactor
//!
//! `Db::open()` has 428 call sites, 59 of them in `run.rs`. Each one opens a
//! fresh connection, and SQLite's `busy_timeout` is 5 seconds — so a contended
//! open on the render/input thread is a five-second freeze, not a slow frame.
//! That is THE-179.
//!
//! What the tail actually costs was never measured, and the refactor it implies
//! — routing 59 call sites in a pinned god-file through `db_task::persist` — is
//! large enough that guessing is the wrong way to start. A live measurement
//! showed the loop doing **3.6 read syscalls a second**, which says this is not
//! a throughput problem; it says nothing about the tail. This module answers
//! that, so the conversion can be aimed at the sites that actually stall rather
//! than applied to all of them on principle.
//!
//! # Loop-thread only
//!
//! Background opens are fine by construction: that is where slow work belongs.
//! Only an open on the loop can freeze a frame, so only the loop is counted,
//! which keeps the numbers meaningful instead of being dominated by scanners.
//!
//! `thegn-core` is substrate-free, so the loop cannot be identified by any
//! runtime handle — the host marks it once at startup with [`mark_loop_thread`]
//! and everything here compares `ThreadId`.
//!
//! Free when unused: an unmarked process takes one relaxed load and returns.

use std::sync::atomic::{AtomicU64, Ordering};

/// The loop's thread id, once the host has named it.
static LOOP_THREAD: std::sync::OnceLock<std::thread::ThreadId> = std::sync::OnceLock::new();

static CALLS: AtomicU64 = AtomicU64::new(0);
static TOTAL_US: AtomicU64 = AtomicU64::new(0);
static MAX_US: AtomicU64 = AtomicU64::new(0);

/// Declare the calling thread as the render/input loop. Called once, from the
/// host, before the loop starts. First call wins; later calls are ignored, so a
/// test that marks a thread cannot corrupt a real process's accounting.
pub fn mark_loop_thread() {
    // `get_or_init`, not `set`: first call wins, which is the behaviour we want,
    // and there is no Result to swallow. A later call must not retarget the
    // accounting mid-run — a test that marked a thread would otherwise make every
    // later test in the same process record.
    LOOP_THREAD.get_or_init(|| std::thread::current().id());
}

/// Whether the calling thread is the one marked as the loop.
fn on_loop_thread() -> bool {
    LOOP_THREAD.get() == Some(&std::thread::current().id())
}

/// Record one completed `Db::open` of `micros`, if it happened on the loop.
///
/// Takes the duration rather than timing internally so the caller brackets
/// exactly the work it means to measure.
pub fn record_open(micros: u64) {
    if !on_loop_thread() {
        return;
    }
    CALLS.fetch_add(1, Ordering::Relaxed);
    TOTAL_US.fetch_add(micros, Ordering::Relaxed);
    MAX_US.fetch_max(micros, Ordering::Relaxed);
}

/// Loop-thread database opens observed so far.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct OpenStats {
    /// Opens performed on the loop thread.
    pub calls: u64,
    /// Their total cost, microseconds.
    pub total_us: u64,
    /// The worst single open, microseconds — **the number that matters**. The
    /// mean hides exactly the case this exists to find: one contended open
    /// waiting out the 5s `busy_timeout` averages away against thousands of
    /// fast ones, while being a five-second freeze.
    pub max_us: u64,
}

impl OpenStats {
    /// Mean cost of a loop-thread open, or 0 when there have been none.
    pub fn mean_us(self) -> u64 {
        self.total_us.checked_div(self.calls).unwrap_or(0)
    }
}

/// Cumulative counts since process start. Monotonic, so a caller that wants a
/// rate or a delta takes two snapshots — this deliberately does not reset, so
/// two consumers cannot steal each other's samples.
pub fn snapshot() -> OpenStats {
    OpenStats {
        calls: CALLS.load(Ordering::Relaxed),
        total_us: TOTAL_US.load(Ordering::Relaxed),
        max_us: MAX_US.load(Ordering::Relaxed),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An unmarked process must record nothing, and must not pay for the check.
    ///
    /// This also pins the ordering hazard: these are process-global statics, and
    /// `mark_loop_thread` is first-set-wins, so a test that marked a thread
    /// would make every later test in the same process record. The suite must
    /// never mark, which is why this test asserts from a *spawned* thread —
    /// never the loop — and only checks monotonicity.
    #[test]
    fn an_unmarked_thread_records_nothing() {
        let before = snapshot();
        std::thread::spawn(|| {
            record_open(1_000);
            record_open(9_999);
        })
        .join()
        .unwrap();
        let after = snapshot();
        assert_eq!(
            before, after,
            "a thread that was never marked as the loop must not be counted"
        );
    }

    #[test]
    fn stats_report_the_worst_case_not_just_the_average() {
        // Built directly rather than through the statics: the point under test
        // is the arithmetic, and touching the globals would leak into whatever
        // test runs next in this process.
        let s = OpenStats {
            calls: 1_000,
            total_us: 1_000 * 50 + 5_000_000,
            max_us: 5_000_000,
        };
        // A single 5s open among a thousand 50µs ones barely moves the mean —
        // which is the whole reason `max_us` is reported alongside it.
        assert_eq!(s.mean_us(), 5_050);
        assert_eq!(s.max_us, 5_000_000);
    }

    #[test]
    fn mean_of_no_calls_is_zero_not_a_division_by_zero() {
        assert_eq!(OpenStats::default().mean_us(), 0);
    }

    #[test]
    fn a_snapshot_is_cumulative_and_never_resets() {
        let a = snapshot();
        let b = snapshot();
        assert_eq!(a, b, "snapshot must not consume the counters");
    }
}
