//! Allocator hygiene and trim observability for the long-running host.
//!
//! `tune_allocator` records whether the requested glibc arena cap applied and
//! anchors the process epoch. Idle loop wakes and completed scan bursts share
//! one 30-second trim limiter; scan drains run only after the last worker exits
//! and switches to Background QoS first. The cumulative counters report trims
//! by cause, drained scan bursts (including throttled ones), successful
//! allocator releases, macOS released bytes, and trim duration. The cap can be
//! set with `THEGN_MALLOC_ARENA_MAX` (default 2; zero or less leaves glibc's
//! default uncapped).

use std::sync::OnceLock;
use std::sync::atomic::{AtomicI32, AtomicU64, Ordering};
use std::time::{Duration, Instant};

const DEFAULT_ARENA_MAX: i32 = 2;
const IDLE_TRIM_DELAY: Duration = Duration::from_secs(10);
const TRIM_MIN_INTERVAL: u64 = 30_000;

static EPOCH: OnceLock<Instant> = OnceLock::new();
static LAST_TRIM_MS: AtomicU64 = AtomicU64::new(0);
static ARENA_MAX: AtomicI32 = AtomicI32::new(DEFAULT_ARENA_MAX);
static ARENA_CAP_APPLIED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
static TRIMS: TrimCounters = TrimCounters::new();

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub(crate) struct ArenaCap {
    pub max: i32,
    pub applied: bool,
}

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub(crate) struct AllocStats {
    pub arena_cap: ArenaCap,
    pub trims_idle: u64,
    pub trims_scan: u64,
    pub scan_drains: u64,
    pub trims_released: u64,
    pub trim_bytes: u64,
    pub trim_total_us: u64,
    pub trim_max_us: u64,
}

#[derive(Clone, Copy)]
struct TrimOutcome {
    released: bool,
    bytes: u64,
}

#[derive(Clone, Copy)]
enum TrimCause {
    Idle,
    ScanDrain,
}

struct TrimCounters {
    idle: AtomicU64,
    scan: AtomicU64,
    scan_drains: AtomicU64,
    released: AtomicU64,
    bytes: AtomicU64,
    total_us: AtomicU64,
    max_us: AtomicU64,
}

impl TrimCounters {
    const fn new() -> Self {
        Self {
            idle: AtomicU64::new(0),
            scan: AtomicU64::new(0),
            scan_drains: AtomicU64::new(0),
            released: AtomicU64::new(0),
            bytes: AtomicU64::new(0),
            total_us: AtomicU64::new(0),
            max_us: AtomicU64::new(0),
        }
    }
}

#[derive(Clone, Copy)]
struct TrimLimiter<'a> {
    last: &'a AtomicU64,
    now_ms: u64,
    min_ms: u64,
}

/// Parse exactly as the environment path does: no whitespace trimming.
fn parse_arena_max(raw: Option<&str>) -> i32 {
    raw.and_then(|value| value.parse::<i32>().ok())
        .map(|value| value.max(0))
        .unwrap_or(DEFAULT_ARENA_MAX)
}

/// Cap the process's glibc arena count before worker threads start.
pub fn tune_allocator() {
    let _epoch = EPOCH.get_or_init(Instant::now);
    let raw = std::env::var("THEGN_MALLOC_ARENA_MAX").ok();
    let max = parse_arena_max(raw.as_deref());
    ARENA_MAX.store(max, Ordering::Relaxed);
    let applied = max > 0 && set_arena_max(max);
    ARENA_CAP_APPLIED.store(applied, Ordering::Relaxed);
}

pub(crate) fn arena_cap() -> ArenaCap {
    ArenaCap {
        max: ARENA_MAX.load(Ordering::Relaxed),
        applied: ARENA_CAP_APPLIED.load(Ordering::Relaxed),
    }
}

fn epoch() -> Instant {
    *EPOCH.get_or_init(Instant::now)
}

fn epoch_ms() -> u64 {
    epoch().elapsed().as_millis().min(u64::MAX as u128) as u64
}

fn claim(last: &AtomicU64, now_ms: u64, min_ms: u64) -> bool {
    let mut previous = last.load(Ordering::Acquire);
    loop {
        if now_ms.saturating_sub(previous) < min_ms {
            return false;
        }
        match last.compare_exchange_weak(previous, now_ms, Ordering::AcqRel, Ordering::Acquire) {
            Ok(_) => return true,
            Err(observed) => previous = observed,
        }
    }
}

fn try_trim_with(
    cause: TrimCause,
    limiter: TrimLimiter<'_>,
    counters: &TrimCounters,
    trim: impl FnOnce() -> Option<TrimOutcome>,
) -> bool {
    if !claim(limiter.last, limiter.now_ms, limiter.min_ms) {
        return false;
    }
    let started = Instant::now();
    let Some(outcome) = trim() else {
        return false;
    };
    let elapsed = started.elapsed().as_micros().min(u64::MAX as u128) as u64;
    match cause {
        TrimCause::Idle => counters.idle.fetch_add(1, Ordering::Relaxed),
        TrimCause::ScanDrain => counters.scan.fetch_add(1, Ordering::Relaxed),
    };
    if outcome.released {
        counters.released.fetch_add(1, Ordering::Relaxed);
    }
    counters.bytes.fetch_add(outcome.bytes, Ordering::Relaxed);
    counters.total_us.fetch_add(elapsed, Ordering::Relaxed);
    counters.max_us.fetch_max(elapsed, Ordering::Relaxed);
    true
}

/// Return freed pages when the loop has been idle, sharing a limiter with
/// scan-drain trims. Called on idle wakes and adds no timer or wake source.
pub fn trim_if_idle(last_activity: Instant) {
    let _trimmed = trim_if_idle_with(
        last_activity,
        TrimLimiter {
            last: &LAST_TRIM_MS,
            now_ms: epoch_ms(),
            min_ms: TRIM_MIN_INTERVAL.as_millis() as u64,
        },
        &TRIMS,
        trim_now,
    );
}

fn trim_if_idle_with(
    last_activity: Instant,
    limiter: TrimLimiter<'_>,
    counters: &TrimCounters,
    trim: impl FnOnce() -> Option<TrimOutcome>,
) -> bool {
    last_activity.elapsed() >= IDLE_TRIM_DELAY
        && try_trim_with(TrimCause::Idle, limiter, counters, trim)
}

pub(crate) fn trim_after_scan_burst() {
    TRIMS.scan_drains.fetch_add(1, Ordering::Relaxed);
    try_trim_with(
        TrimCause::ScanDrain,
        TrimLimiter {
            last: &LAST_TRIM_MS,
            now_ms: epoch_ms(),
            min_ms: TRIM_MIN_INTERVAL.as_millis() as u64,
        },
        &TRIMS,
        trim_now,
    );
}

pub(crate) fn alloc_stats() -> AllocStats {
    AllocStats {
        arena_cap: arena_cap(),
        trims_idle: TRIMS.idle.load(Ordering::Relaxed),
        trims_scan: TRIMS.scan.load(Ordering::Relaxed),
        scan_drains: TRIMS.scan_drains.load(Ordering::Relaxed),
        trims_released: TRIMS.released.load(Ordering::Relaxed),
        trim_bytes: TRIMS.bytes.load(Ordering::Relaxed),
        trim_total_us: TRIMS.total_us.load(Ordering::Relaxed),
        trim_max_us: TRIMS.max_us.load(Ordering::Relaxed),
    }
}

#[cfg(all(target_os = "linux", target_env = "gnu"))]
fn set_arena_max(max: i32) -> bool {
    // SAFETY: mallopt is thread-safe and called before worker threads exist.
    unsafe { libc::mallopt(libc::M_ARENA_MAX, max) == 1 }
}

#[cfg(not(all(target_os = "linux", target_env = "gnu")))]
fn set_arena_max(_max: i32) -> bool {
    false
}

#[cfg(all(target_os = "linux", target_env = "gnu"))]
fn trim_now() -> Option<TrimOutcome> {
    // SAFETY: malloc_trim is thread-safe; 1 means memory was released.
    let released = unsafe { libc::malloc_trim(0) == 1 };
    Some(TrimOutcome { released, bytes: 0 })
}

#[cfg(target_os = "macos")]
fn trim_now() -> Option<TrimOutcome> {
    unsafe extern "C" {
        fn malloc_zone_pressure_relief(zone: *mut std::ffi::c_void, goal: usize) -> usize;
    }
    // SAFETY: NULL and zero request all releasable bytes from all zones.
    let bytes = unsafe { malloc_zone_pressure_relief(std::ptr::null_mut(), 0) as u64 };
    Some(TrimOutcome {
        released: bytes > 0,
        bytes,
    })
}

#[cfg(not(any(all(target_os = "linux", target_env = "gnu"), target_os = "macos")))]
fn trim_now() -> Option<TrimOutcome> {
    None
}

#[cfg(test)]
pub(crate) fn record_test_idle_trim() {
    let limiter = AtomicU64::new(0);
    let _trimmed = try_trim_with(
        TrimCause::Idle,
        TrimLimiter {
            last: &limiter,
            now_ms: 30_000,
            min_ms: TRIM_MIN_INTERVAL.as_millis() as u64,
        },
        &TRIMS,
        || {
            Some(TrimOutcome {
                released: false,
                bytes: 0,
            })
        },
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;

    #[test]
    fn parse_arena_max_table() {
        for (raw, expected) in [
            (None, 2),
            (Some("4"), 4),
            (Some("0"), 0),
            (Some("-3"), 0),
            (Some("x"), 2),
            (Some(""), 2),
        ] {
            assert_eq!(parse_arena_max(raw), expected);
        }
    }

    #[test]
    fn claim_respects_the_shared_interval() {
        let last = AtomicU64::new(0);
        assert!(!claim(&last, 29_999, 30_000));
        assert!(claim(&last, 30_000, 30_000));
        assert_eq!(last.load(Ordering::Relaxed), 30_000);
        assert!(!claim(&last, 59_999, 30_000));
        assert!(claim(&last, 60_000, 30_000));
    }

    #[test]
    fn claim_admits_exactly_one_racer() {
        let last = std::sync::Arc::new(AtomicU64::new(0));
        let winners = std::sync::Arc::new(AtomicUsize::new(0));
        let threads: Vec<_> = (0..8)
            .map(|_| {
                let (last, winners) = (last.clone(), winners.clone());
                std::thread::spawn(move || {
                    if claim(&last, 30_000, 30_000) {
                        winners.fetch_add(1, Ordering::SeqCst);
                    }
                })
            })
            .collect();
        for thread in threads {
            thread.join().unwrap();
        }
        assert_eq!(winners.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn try_trim_counts_by_cause_and_outcome() {
        let counters = TrimCounters::new();
        let last = AtomicU64::new(0);
        let limiter = TrimLimiter {
            last: &last,
            now_ms: 30_000,
            min_ms: 30_000,
        };
        assert!(try_trim_with(TrimCause::Idle, limiter, &counters, || {
            std::thread::sleep(Duration::from_millis(2));
            Some(TrimOutcome {
                released: true,
                bytes: 4096,
            })
        }));
        let called = AtomicUsize::new(0);
        assert!(!try_trim_with(
            TrimCause::ScanDrain,
            TrimLimiter {
                now_ms: 59_999,
                ..limiter
            },
            &counters,
            || {
                called.fetch_add(1, Ordering::Relaxed);
                Some(TrimOutcome {
                    released: false,
                    bytes: 0,
                })
            }
        ));
        assert_eq!(called.load(Ordering::Relaxed), 0);
        assert!(!try_trim_with(
            TrimCause::ScanDrain,
            TrimLimiter {
                now_ms: 60_000,
                ..limiter
            },
            &counters,
            || None
        ));
        assert!(try_trim_with(
            TrimCause::ScanDrain,
            TrimLimiter {
                now_ms: 90_000,
                ..limiter
            },
            &counters,
            || {
                std::thread::sleep(Duration::from_millis(1));
                Some(TrimOutcome {
                    released: false,
                    bytes: 0,
                })
            }
        ));
        assert_eq!(counters.idle.load(Ordering::Relaxed), 1);
        assert_eq!(counters.scan.load(Ordering::Relaxed), 1);
        assert_eq!(counters.released.load(Ordering::Relaxed), 1);
        assert_eq!(counters.bytes.load(Ordering::Relaxed), 4096);
        assert!(counters.max_us.load(Ordering::Relaxed) >= 1_000);
        assert!(
            counters.max_us.load(Ordering::Relaxed) <= counters.total_us.load(Ordering::Relaxed)
        );
    }

    #[test]
    fn trims_only_when_idle_and_not_too_often() {
        let counters = TrimCounters::new();
        let last = AtomicU64::new(0);
        let recent = Instant::now();
        let called = AtomicUsize::new(0);
        let limiter = TrimLimiter {
            last: &last,
            now_ms: 30_000,
            min_ms: 30_000,
        };
        assert!(!trim_if_idle_with(recent, limiter, &counters, || {
            called.fetch_add(1, Ordering::Relaxed);
            Some(TrimOutcome {
                released: false,
                bytes: 0,
            })
        }));
        assert_eq!(
            called.load(Ordering::Relaxed),
            0,
            "recent activity must not trim"
        );
        let idle = Instant::now() - Duration::from_secs(20);
        assert!(trim_if_idle_with(idle, limiter, &counters, || {
            called.fetch_add(1, Ordering::Relaxed);
            Some(TrimOutcome {
                released: false,
                bytes: 0,
            })
        }));
        assert_eq!(called.load(Ordering::Relaxed), 1);
        assert!(!trim_if_idle_with(
            idle,
            TrimLimiter {
                last: &last,
                now_ms: 59_999,
                min_ms: 30_000,
            },
            &counters,
            || {
                called.fetch_add(1, Ordering::Relaxed);
                Some(TrimOutcome {
                    released: false,
                    bytes: 0,
                })
            }
        ));
        assert_eq!(called.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn tune_allocator_records_the_cap() {
        let _env = crate::testenv::EnvVarGuard::set(&[("THEGN_MALLOC_ARENA_MAX", "3")]);
        tune_allocator();
        assert_eq!(arena_cap().max, 3);
        assert_eq!(
            arena_cap().applied,
            cfg!(all(target_os = "linux", target_env = "gnu"))
        );
    }

    #[test]
    fn tune_allocator_records_zero_as_uncapped() {
        let _env = crate::testenv::EnvVarGuard::set(&[("THEGN_MALLOC_ARENA_MAX", "0")]);
        tune_allocator();
        assert_eq!(
            arena_cap(),
            ArenaCap {
                max: 0,
                applied: false
            }
        );
    }
}
