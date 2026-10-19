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
//! # Loop-thread attribution
//!
//! The per-site table and timings attribute only opens on the loop, since those
//! can freeze a frame. A separate total counter counts opens on every thread
//! for tests and diagnostics without adding per-open attribution overhead.
//!
//! `thegn-core` is substrate-free, so the loop cannot be identified by any
//! runtime handle — the host marks it once at startup with [`mark_loop_thread`]
//! and everything here compares `ThreadId`.
//!
//! Every open takes one relaxed increment; an unmarked process does no timing,
//! locking, or call-site attribution.

use std::panic::Location;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

const SITE_CAPACITY: usize = 16;

/// The loop's thread id, once the host has named it.
static LOOP_THREAD: std::sync::OnceLock<std::thread::ThreadId> = std::sync::OnceLock::new();

static CALLS: AtomicU64 = AtomicU64::new(0);
static ALL_OPENS: AtomicU64 = AtomicU64::new(0);
static TOTAL_US: AtomicU64 = AtomicU64::new(0);
static MAX_US: AtomicU64 = AtomicU64::new(0);
static SITES: Mutex<SiteTable> = Mutex::new(SiteTable::new());

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct SiteEntry {
    file: &'static str,
    line: u32,
    calls: u64,
    overestimate: u64,
    max_us: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct SiteTable {
    entries: [Option<SiteEntry>; SITE_CAPACITY],
}

impl SiteTable {
    const fn new() -> Self {
        Self {
            entries: [None; SITE_CAPACITY],
        }
    }

    fn record(&mut self, location: &'static Location<'static>, micros: u64) {
        if let Some(entry) = self
            .entries
            .iter_mut()
            .flatten()
            .find(|entry| entry.file == location.file() && entry.line == location.line())
        {
            entry.calls = entry.calls.saturating_add(1);
            entry.max_us = entry.max_us.max(micros);
            return;
        }

        let new_entry = SiteEntry {
            file: location.file(),
            line: location.line(),
            calls: 1,
            overestimate: 0,
            max_us: micros,
        };
        if let Some(slot) = self.entries.iter_mut().find(|entry| entry.is_none()) {
            *slot = Some(new_entry);
            return;
        }

        // Space-Saving replacement keeps observing new hot sites after startup
        // has filled the fixed table. The displaced minimum is the error bound
        // for the replacement site's estimated count.
        if let Some(slot) = self
            .entries
            .iter_mut()
            .flatten()
            .min_by_key(|entry| entry.calls)
        {
            let minimum = slot.calls;
            *slot = SiteEntry {
                calls: minimum.saturating_add(1),
                overestimate: minimum,
                ..new_entry
            };
        }
    }
}

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
pub fn record_open(micros: u64, location: &'static Location<'static>) {
    ALL_OPENS.fetch_add(1, Ordering::Relaxed);
    if !on_loop_thread() {
        return;
    }
    CALLS.fetch_add(1, Ordering::Relaxed);
    TOTAL_US.fetch_add(micros, Ordering::Relaxed);
    MAX_US.fetch_max(micros, Ordering::Relaxed);
    SITES
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .record(location, micros);
}

/// Total database opens observed on all threads since process start.
pub fn total_opens() -> u64 {
    ALL_OPENS.load(Ordering::Relaxed)
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
    /// Fixed-size call-site counts since process start, ordered by slot.
    pub sites: [Option<OpenSiteStats>; SITE_CAPACITY],
}

/// Loop-thread opens attributed to one source location.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OpenSiteStats {
    /// Source file reported by `#[track_caller]`.
    pub file: &'static str,
    /// Source line reported by `#[track_caller]`.
    pub line: u32,
    /// Opens recorded at this site.
    pub calls: u64,
    /// Maximum count contributed by evicted sites.
    pub overestimate: u64,
    /// Worst open at this site, microseconds.
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
    let stats = OpenStats {
        calls: CALLS.load(Ordering::Relaxed),
        total_us: TOTAL_US.load(Ordering::Relaxed),
        max_us: MAX_US.load(Ordering::Relaxed),
        sites: SITES
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .entries
            .map(|entry| {
                entry.map(|entry| OpenSiteStats {
                    file: entry.file,
                    line: entry.line,
                    calls: entry.calls,
                    overestimate: entry.overestimate,
                    max_us: entry.max_us,
                })
            }),
    };
    tracing::debug!(
        target: "thegn::perf",
        db_open_loop_top_sites_since_start = ?stats.top_sites(),
        "loop-thread database open call sites"
    );
    stats
}

impl OpenStats {
    /// The three most frequently observed call sites since process start.
    pub fn top_sites(self) -> [Option<OpenSiteStats>; 3] {
        let mut top: [Option<OpenSiteStats>; 3] = [None; 3];
        for site in self.sites.into_iter().flatten() {
            let Some(index) = top
                .iter()
                .position(|candidate| candidate.is_none_or(|current| site.calls > current.calls))
            else {
                continue;
            };
            for shift in (index + 1..top.len()).rev() {
                top[shift] = top[shift - 1];
            }
            top[index] = Some(site);
        }
        top
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
            record_open(1_000, Location::caller());
            record_open(9_999, Location::caller());
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
    fn all_thread_counter_counts_opens_from_an_unmarked_thread() {
        let before = total_opens();
        std::thread::scope(|scope| {
            scope
                .spawn(|| {
                    record_open(1_000, Location::caller());
                    record_open(9_999, Location::caller());
                })
                .join()
                .unwrap();
        });
        assert_eq!(total_opens() - before, 2);
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
            ..OpenStats::default()
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

    #[test]
    fn site_table_tracks_file_line_count_and_worst_open() {
        let location = Location::caller();
        let mut table = SiteTable::new();
        table.record(location, 12);
        table.record(location, 45);

        let site = table.entries[0].expect("first call site should be retained");
        assert_eq!(site.file, location.file());
        assert_eq!(site.line, location.line());
        assert_eq!(site.calls, 2);
        assert_eq!(site.max_us, 45);
    }

    #[test]
    fn repeated_site_displaces_startup_entries_when_table_is_full() {
        let location = Location::caller();
        let mut table = SiteTable::new();
        for (index, slot) in table.entries.iter_mut().enumerate() {
            *slot = Some(SiteEntry {
                file: "startup.rs",
                line: index as u32,
                calls: 2,
                overestimate: 0,
                max_us: 1,
            });
        }
        for _ in 0..3 {
            table.record(location, 10);
        }

        let site = table
            .entries
            .iter()
            .flatten()
            .find(|entry| entry.file == location.file() && entry.line == location.line())
            .expect("repeated call site should be retained");
        assert_eq!(site.calls, 5);
        assert_eq!(site.overestimate, 2);
    }

    #[test]
    fn top_sites_orders_counts_with_stable_ties_and_fewer_than_three() {
        let mut stats = OpenStats::default();
        stats.sites[0] = Some(OpenSiteStats {
            file: "a.rs",
            line: 1,
            calls: 4,
            overestimate: 0,
            max_us: 0,
        });
        stats.sites[1] = Some(OpenSiteStats {
            file: "b.rs",
            line: 2,
            calls: 8,
            overestimate: 0,
            max_us: 0,
        });
        assert_eq!(stats.top_sites()[0].map(|site| site.file), Some("b.rs"));
        assert_eq!(stats.top_sites()[1].map(|site| site.file), Some("a.rs"));
        assert!(stats.top_sites()[2].is_none());

        stats.sites[2] = Some(OpenSiteStats {
            file: "c.rs",
            line: 3,
            calls: 8,
            overestimate: 0,
            max_us: 0,
        });
        stats.sites[3] = Some(OpenSiteStats {
            file: "d.rs",
            line: 4,
            calls: 6,
            overestimate: 0,
            max_us: 0,
        });
        let top = stats.top_sites();
        assert_eq!(
            top.map(|site| site.map(|site| site.file)),
            [Some("b.rs"), Some("c.rs"), Some("d.rs")]
        );
    }
}
