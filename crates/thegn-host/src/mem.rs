//! Allocator hygiene and trim observability for the long-running host.
//!
//! `tune_allocator` records whether the requested glibc arena cap applied and
//! anchors the process epoch. Idle loop wakes and completed scan bursts share
//! one trim limiter with a 30-second floor; scan drains back off further after
//! slow trims and run only after the last worker exits
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
const TRIM_MAX_INTERVAL: u64 = 3_600_000;

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
    skipped_swap: AtomicU64,
    last_duration_us: AtomicU64,
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
            skipped_swap: AtomicU64::new(0),
            last_duration_us: AtomicU64::new(0),
        }
    }
}

#[derive(Clone, Copy)]
struct TrimLimiter<'a> {
    last: &'a AtomicU64,
    last_duration_us: Option<&'a AtomicU64>,
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
    let raw = std::env::var_os("THEGN_MALLOC_ARENA_MAX").and_then(|value| value.into_string().ok());
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

/// Next trim not before last + clamp(last_duration_us * 1000 µs, 30 s, 1 h),
/// for both causes. `duration_us * 1000` µs is `duration_us` ms.
fn backoff_ms(last_duration_us: u64) -> u64 {
    last_duration_us.clamp(TRIM_MIN_INTERVAL, TRIM_MAX_INTERVAL)
}

fn claim(limiter: TrimLimiter<'_>) -> bool {
    let adaptive_ms = limiter
        .last_duration_us
        .map(|duration_us| backoff_ms(duration_us.load(Ordering::Acquire)))
        .unwrap_or(0);
    let min_ms = limiter.min_ms.max(adaptive_ms);
    let mut previous = limiter.last.load(Ordering::Acquire);
    loop {
        if limiter.now_ms.saturating_sub(previous) < min_ms {
            return false;
        }
        match limiter.last.compare_exchange_weak(
            previous,
            limiter.now_ms,
            Ordering::AcqRel,
            Ordering::Acquire,
        ) {
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
    try_trim_with_status(
        cause,
        limiter,
        counters,
        crate::platform::proc::memory_status,
        trim,
    )
}

fn try_trim_with_status(
    cause: TrimCause,
    limiter: TrimLimiter<'_>,
    counters: &TrimCounters,
    read_memory_status: impl FnOnce() -> Option<crate::platform::proc::ProcessMemoryStatus>,
    trim: impl FnOnce() -> Option<TrimOutcome>,
) -> bool {
    if !claim(limiter) {
        return false;
    }
    if let Some(status) = read_memory_status()
        && should_skip_scan_trim(status.swap_kb, status.rss_kb)
    {
        counters.skipped_swap.fetch_add(1, Ordering::Relaxed);
        tracing::debug!(
            target: "thegn::mem",
            vm_swap_kb = status.swap_kb,
            vm_rss_kb = status.rss_kb,
            "skipping trim while swap is elevated"
        );
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
    counters.last_duration_us.store(elapsed, Ordering::Release);
    true
}

fn should_skip_scan_trim(swap_kb: u64, rss_kb: u64) -> bool {
    swap_kb > rss_kb / 4
}

/// Background trim worker: one named thread parked in `recv`; requests are
/// dropped when one is already queued. No timer.
fn trim_worker() -> Option<&'static std::sync::mpsc::SyncSender<TrimCause>> {
    static WORKER: OnceLock<Option<std::sync::mpsc::SyncSender<TrimCause>>> = OnceLock::new();
    WORKER
        .get_or_init(|| {
            spawn_worker(|cause| {
                let _trimmed = try_trim_with(
                    cause,
                    TrimLimiter {
                        last: &LAST_TRIM_MS,
                        last_duration_us: Some(&TRIMS.last_duration_us),
                        now_ms: epoch_ms(),
                        min_ms: TRIM_MIN_INTERVAL,
                    },
                    &TRIMS,
                    trim_now,
                );
            })
        })
        .as_ref()
}

fn spawn_worker(
    handler: impl Fn(TrimCause) + Send + 'static,
) -> Option<std::sync::mpsc::SyncSender<TrimCause>> {
    let (tx, rx) = std::sync::mpsc::sync_channel::<TrimCause>(1);
    thegn_core::thread_name::try_spawn_named("thegn-mem-trim", move || {
        crate::platform::qos::set_self(crate::platform::qos::Qos::Background);
        while let Ok(cause) = rx.recv() {
            handler(cause);
        }
    })
    .ok()
    .map(|_| tx)
}

/// Return freed pages when the loop has been idle. The caller only checks
/// the idle delay and queues a request; the limiter, swap check and trim run
/// on the worker. Adds no timer or wake source.
pub fn trim_if_idle(last_activity: Instant) {
    if let Some(tx) = trim_worker() {
        let _queued = trim_if_idle_with(last_activity, tx);
    }
}

fn trim_if_idle_with(last_activity: Instant, tx: &std::sync::mpsc::SyncSender<TrimCause>) -> bool {
    last_activity.elapsed() >= IDLE_TRIM_DELAY && tx.try_send(TrimCause::Idle).is_ok()
}

pub(crate) fn trim_after_scan_burst() {
    TRIMS.scan_drains.fetch_add(1, Ordering::Relaxed);
    if let Some(tx) = trim_worker() {
        let _queued = tx.try_send(TrimCause::ScanDrain);
    }
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
            last_duration_us: None,
            now_ms: 30_000,
            min_ms: TRIM_MIN_INTERVAL,
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
        assert!(!claim(TrimLimiter {
            last: &last,
            last_duration_us: None,
            now_ms: 29_999,
            min_ms: 30_000
        }));
        assert!(claim(TrimLimiter {
            last: &last,
            last_duration_us: None,
            now_ms: 30_000,
            min_ms: 30_000
        }));
        assert_eq!(last.load(Ordering::Relaxed), 30_000);
        assert!(!claim(TrimLimiter {
            last: &last,
            last_duration_us: None,
            now_ms: 59_999,
            min_ms: 30_000
        }));
        assert!(claim(TrimLimiter {
            last: &last,
            last_duration_us: None,
            now_ms: 60_000,
            min_ms: 30_000
        }));
    }

    #[test]
    fn fast_scan_trim_keeps_the_thirty_second_floor() {
        let last = AtomicU64::new(30_000);
        let duration = AtomicU64::new(25_000);
        assert!(!claim(TrimLimiter {
            last: &last,
            last_duration_us: Some(&duration),
            now_ms: 59_999,
            min_ms: TRIM_MIN_INTERVAL,
        }));
        assert!(claim(TrimLimiter {
            last: &last,
            last_duration_us: Some(&duration),
            now_ms: 60_000,
            min_ms: TRIM_MIN_INTERVAL,
        }));
    }

    #[test]
    fn slow_trim_pushes_next_admission_by_its_duration_in_ms() {
        let last = AtomicU64::new(30_000);
        let duration = AtomicU64::new(1_300_000);
        assert!(!claim(TrimLimiter {
            last: &last,
            last_duration_us: Some(&duration),
            now_ms: 1_329_999,
            min_ms: TRIM_MIN_INTERVAL,
        }));
        assert!(claim(TrimLimiter {
            last: &last,
            last_duration_us: Some(&duration),
            now_ms: 1_330_000,
            min_ms: TRIM_MIN_INTERVAL,
        }));
    }

    #[test]
    fn claim_admits_exactly_one_racer() {
        let last = std::sync::Arc::new(AtomicU64::new(0));
        let winners = std::sync::Arc::new(AtomicUsize::new(0));
        let threads: Vec<_> = (0..8)
            .map(|_| {
                let (last, winners) = (last.clone(), winners.clone());
                std::thread::spawn(move || {
                    if claim(TrimLimiter {
                        last: &last,
                        last_duration_us: None,
                        now_ms: 30_000,
                        min_ms: 30_000,
                    }) {
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
            last_duration_us: None,
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
        assert!(!try_trim_with_status(
            TrimCause::ScanDrain,
            TrimLimiter {
                now_ms: 59_999,
                ..limiter
            },
            &counters,
            || None,
            || {
                called.fetch_add(1, Ordering::Relaxed);
                Some(TrimOutcome {
                    released: false,
                    bytes: 0,
                })
            }
        ));
        assert_eq!(called.load(Ordering::Relaxed), 0);
        assert!(!try_trim_with_status(
            TrimCause::ScanDrain,
            TrimLimiter {
                now_ms: 60_000,
                ..limiter
            },
            &counters,
            || None,
            || None
        ));
        assert!(try_trim_with_status(
            TrimCause::ScanDrain,
            TrimLimiter {
                now_ms: 90_000,
                ..limiter
            },
            &counters,
            || None,
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
        assert!(counters.last_duration_us.load(Ordering::Acquire) > 0);
    }

    #[test]
    fn backoff_ms_table() {
        assert_eq!(backoff_ms(1_000), 30_000);
        assert_eq!(backoff_ms(0), 30_000);
        assert_eq!(backoff_ms(2_370_000), 2_370_000);
        assert_eq!(backoff_ms(10_000_000), 3_600_000);
    }

    #[test]
    fn idle_check_only_queues_and_never_trims_on_the_caller() {
        let (tx, rx) = std::sync::mpsc::sync_channel::<TrimCause>(1);
        let before = TRIMS.idle.load(Ordering::Relaxed);
        assert!(!trim_if_idle_with(Instant::now(), &tx), "recent activity");
        assert!(rx.try_recv().is_err());
        let idle = Instant::now() - Duration::from_secs(20);
        assert!(trim_if_idle_with(idle, &tx));
        assert!(!trim_if_idle_with(idle, &tx), "second request is dropped");
        assert!(matches!(rx.try_recv(), Ok(TrimCause::Idle)));
        assert!(rx.try_recv().is_err());
        assert_eq!(TRIMS.idle.load(Ordering::Relaxed), before);
    }

    #[test]
    fn worker_thread_is_named() {
        let (name_tx, name_rx) = std::sync::mpsc::channel();
        let tx = spawn_worker(move |_| {
            let _ = name_tx.send(std::thread::current().name().map(str::to_owned));
        })
        .expect("spawn");
        tx.send(TrimCause::Idle).unwrap();
        let name = name_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        assert_eq!(name.as_deref(), Some("thegn-mem-trim"));
    }

    #[test]
    fn idle_trim_skips_elevated_swap() {
        let counters = TrimCounters::new();
        let last = AtomicU64::new(0);
        let called = AtomicUsize::new(0);
        assert!(!try_trim_with_status(
            TrimCause::Idle,
            TrimLimiter {
                last: &last,
                last_duration_us: None,
                now_ms: 30_000,
                min_ms: TRIM_MIN_INTERVAL,
            },
            &counters,
            || Some(crate::platform::proc::ProcessMemoryStatus {
                swap_kb: 26,
                rss_kb: 100,
            }),
            || {
                called.fetch_add(1, Ordering::Relaxed);
                None
            }
        ));
        assert_eq!(called.load(Ordering::Relaxed), 0);
        assert_eq!(counters.skipped_swap.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn swap_skip_decision_has_an_exact_quarter_boundary() {
        assert!(!should_skip_scan_trim(0, 0));
        assert!(!should_skip_scan_trim(25, 100));
        assert!(should_skip_scan_trim(26, 100));
        assert!(!should_skip_scan_trim(1, u64::MAX));
        assert!(should_skip_scan_trim(u64::MAX, u64::MAX));
    }

    #[test]
    fn admitted_scan_trim_counts_and_skips_elevated_swap() {
        let counters = TrimCounters::new();
        let last = AtomicU64::new(30_000);
        let called = AtomicUsize::new(0);
        assert!(!try_trim_with_status(
            TrimCause::ScanDrain,
            TrimLimiter {
                last: &last,
                last_duration_us: Some(&counters.last_duration_us),
                now_ms: 60_000,
                min_ms: TRIM_MIN_INTERVAL,
            },
            &counters,
            || Some(crate::platform::proc::ProcessMemoryStatus {
                swap_kb: 26,
                rss_kb: 100,
            }),
            || {
                called.fetch_add(1, Ordering::Relaxed);
                Some(TrimOutcome {
                    released: false,
                    bytes: 0,
                })
            }
        ));
        assert_eq!(called.load(Ordering::Relaxed), 0);
        assert_eq!(counters.skipped_swap.load(Ordering::Relaxed), 1);
        assert_eq!(counters.scan.load(Ordering::Relaxed), 0);
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
