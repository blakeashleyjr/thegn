//! Tokio runtime wake and blocking-pool attribution for the perf rollup.
//!
//! Tokio's stable park/unpark hooks run on scheduler workers. `on_thread_park`
//! is only called by the multi-thread worker loop, so a thread-local marker
//! lets the stop hook split worker stops from blocking-thread stops. Runtime
//! workers are started inside `Builder::build()`, before `perf::init()` runs
//! in `run::main`; with `THEGN_PERF=1` from launch, counted starts therefore
//! describe blocking-pool starts. Counter values are retained while perf is
//! disabled, so events from before a live re-enable appear in its first
//! subsequent interval.

use std::cell::Cell;
use std::sync::atomic::{AtomicU64, Ordering};

static BLOCKING_THREAD_STARTS: AtomicU64 = AtomicU64::new(0);
static WORKER_THREAD_STOPS: AtomicU64 = AtomicU64::new(0);
static BLOCKING_THREAD_STOPS: AtomicU64 = AtomicU64::new(0);
static WORKER_PARKS: AtomicU64 = AtomicU64::new(0);
static WORKER_UNPARKS: AtomicU64 = AtomicU64::new(0);

thread_local! {
    static IS_WORKER: Cell<bool> = const { Cell::new(false) };
}

#[derive(Clone, Copy, Debug, Default, PartialEq)]
struct RuntimeRollup {
    interval_seconds: f64,
    blocking_thread_starts: u64,
    worker_thread_stops: u64,
    blocking_thread_stops: u64,
    worker_parks: u64,
    worker_unparks: u64,
    blocking_thread_starts_per_s: f64,
    worker_thread_stops_per_s: f64,
    blocking_thread_stops_per_s: f64,
    worker_parks_per_s: f64,
    worker_unparks_per_s: f64,
    num_workers: usize,
    num_alive_tasks: usize,
    global_queue_depth: usize,
}

impl RuntimeRollup {
    fn new(counts: Counts, interval_seconds: f64, metrics: RuntimeMetrics) -> Self {
        let seconds = if interval_seconds.is_finite() && interval_seconds > 0.0 {
            interval_seconds
        } else {
            0.0
        };
        let rate = |count: u64| {
            if seconds > 0.0 {
                count as f64 / seconds
            } else {
                0.0
            }
        };
        Self {
            interval_seconds,
            blocking_thread_starts: counts.blocking_thread_starts,
            worker_thread_stops: counts.worker_thread_stops,
            blocking_thread_stops: counts.blocking_thread_stops,
            worker_parks: counts.worker_parks,
            worker_unparks: counts.worker_unparks,
            blocking_thread_starts_per_s: rate(counts.blocking_thread_starts),
            worker_thread_stops_per_s: rate(counts.worker_thread_stops),
            blocking_thread_stops_per_s: rate(counts.blocking_thread_stops),
            worker_parks_per_s: rate(counts.worker_parks),
            worker_unparks_per_s: rate(counts.worker_unparks),
            num_workers: metrics.num_workers,
            num_alive_tasks: metrics.num_alive_tasks,
            global_queue_depth: metrics.global_queue_depth,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct Counts {
    blocking_thread_starts: u64,
    worker_thread_stops: u64,
    blocking_thread_stops: u64,
    worker_parks: u64,
    worker_unparks: u64,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct RuntimeMetrics {
    num_workers: usize,
    num_alive_tasks: usize,
    global_queue_depth: usize,
}

fn add(counter: &AtomicU64, enabled: bool) {
    if enabled {
        counter.fetch_add(1, Ordering::Relaxed);
    }
}

fn record_blocking_thread_start(enabled: bool) {
    add(&BLOCKING_THREAD_STARTS, enabled);
}

fn record_worker_thread_stop(enabled: bool) {
    add(&WORKER_THREAD_STOPS, enabled);
}

fn record_blocking_thread_stop(enabled: bool) {
    add(&BLOCKING_THREAD_STOPS, enabled);
}

fn record_worker_park(enabled: bool) {
    add(&WORKER_PARKS, enabled);
}

fn record_worker_unpark(enabled: bool) {
    add(&WORKER_UNPARKS, enabled);
}

pub(crate) fn on_thread_start() {
    record_blocking_thread_start(crate::perf::enabled());
}

pub(crate) fn on_thread_stop() {
    let enabled = crate::perf::enabled();
    if enabled {
        IS_WORKER.with(|is_worker| {
            if is_worker.get() {
                record_worker_thread_stop(true);
            } else {
                record_blocking_thread_stop(true);
            }
        });
    }
}

pub(crate) fn on_thread_park() {
    let enabled = crate::perf::enabled();
    if enabled {
        IS_WORKER.with(|is_worker| is_worker.set(true));
        record_worker_park(true);
    }
}

pub(crate) fn on_thread_unpark() {
    record_worker_unpark(crate::perf::enabled());
}

fn take_counts() -> Counts {
    Counts {
        blocking_thread_starts: BLOCKING_THREAD_STARTS.swap(0, Ordering::Relaxed),
        worker_thread_stops: WORKER_THREAD_STOPS.swap(0, Ordering::Relaxed),
        blocking_thread_stops: BLOCKING_THREAD_STOPS.swap(0, Ordering::Relaxed),
        worker_parks: WORKER_PARKS.swap(0, Ordering::Relaxed),
        worker_unparks: WORKER_UNPARKS.swap(0, Ordering::Relaxed),
    }
}

pub(crate) fn report(interval_seconds: f64) {
    if !crate::perf::enabled() {
        return;
    }

    let metrics = match tokio::runtime::Handle::try_current() {
        Ok(handle) => {
            let metrics = handle.metrics();
            RuntimeMetrics {
                num_workers: metrics.num_workers(),
                num_alive_tasks: metrics.num_alive_tasks(),
                global_queue_depth: metrics.global_queue_depth(),
            }
        }
        Err(_) => RuntimeMetrics::default(),
    };
    let rollup = RuntimeRollup::new(take_counts(), interval_seconds, metrics);

    tracing::info!(
        target: "thegn::perf::rt",
        interval_seconds = rollup.interval_seconds,
        blocking_thread_starts = rollup.blocking_thread_starts,
        worker_thread_stops = rollup.worker_thread_stops,
        blocking_thread_stops = rollup.blocking_thread_stops,
        worker_parks = rollup.worker_parks,
        worker_unparks = rollup.worker_unparks,
        blocking_thread_starts_per_s = rollup.blocking_thread_starts_per_s,
        worker_thread_stops_per_s = rollup.worker_thread_stops_per_s,
        blocking_thread_stops_per_s = rollup.blocking_thread_stops_per_s,
        worker_parks_per_s = rollup.worker_parks_per_s,
        worker_unparks_per_s = rollup.worker_unparks_per_s,
        num_workers = rollup.num_workers,
        num_alive_tasks = rollup.num_alive_tasks,
        global_queue_depth = rollup.global_queue_depth,
        "Tokio runtime rollup"
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    static TEST_GUARD: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn clear_counts() {
        let _counts = take_counts();
    }

    #[test]
    fn callbacks_accumulate_by_kind_and_report_resets_counts() {
        let _guard = TEST_GUARD
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        clear_counts();
        record_blocking_thread_start(true);
        record_blocking_thread_start(true);
        record_worker_thread_stop(true);
        record_blocking_thread_stop(true);
        record_worker_park(true);
        record_worker_park(true);
        record_worker_park(true);
        record_worker_unpark(true);

        let first = take_counts();
        assert_eq!(first.blocking_thread_starts, 2);
        assert_eq!(first.worker_thread_stops, 1);
        assert_eq!(first.blocking_thread_stops, 1);
        assert_eq!(first.worker_parks, 3);
        assert_eq!(first.worker_unparks, 1);
        assert_eq!(take_counts(), Counts::default());
    }

    #[test]
    fn callbacks_do_not_change_counts_when_perf_is_off() {
        let _guard = TEST_GUARD
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        clear_counts();
        crate::perf::set_enabled(false);
        on_thread_start();
        on_thread_stop();
        on_thread_park();
        on_thread_unpark();

        assert_eq!(take_counts(), Counts::default());
    }

    #[test]
    fn rollup_rates_are_counts_divided_by_the_interval() {
        let rollup = RuntimeRollup::new(
            Counts {
                blocking_thread_starts: 2,
                worker_thread_stops: 1,
                blocking_thread_stops: 1,
                worker_parks: 6,
                worker_unparks: 4,
            },
            2.0,
            RuntimeMetrics {
                num_workers: 8,
                num_alive_tasks: 5,
                global_queue_depth: 3,
            },
        );

        assert_eq!(rollup.blocking_thread_starts_per_s, 1.0);
        assert_eq!(rollup.worker_thread_stops_per_s, 0.5);
        assert_eq!(rollup.blocking_thread_stops_per_s, 0.5);
        assert_eq!(rollup.worker_parks_per_s, 3.0);
        assert_eq!(rollup.worker_unparks_per_s, 2.0);
        assert_eq!(rollup.num_workers, 8);
        assert_eq!(rollup.num_alive_tasks, 5);
        assert_eq!(rollup.global_queue_depth, 3);
    }

    #[test]
    fn zero_or_invalid_interval_has_zero_rates() {
        let counts = Counts {
            blocking_thread_starts: 1,
            worker_thread_stops: 1,
            blocking_thread_stops: 1,
            worker_parks: 1,
            worker_unparks: 1,
        };

        for interval in [0.0, -1.0, f64::NAN] {
            let rollup = RuntimeRollup::new(counts, interval, RuntimeMetrics::default());
            assert_eq!(rollup.blocking_thread_starts_per_s, 0.0);
            assert_eq!(rollup.worker_thread_stops_per_s, 0.0);
            assert_eq!(rollup.blocking_thread_stops_per_s, 0.0);
            assert_eq!(rollup.worker_parks_per_s, 0.0);
            assert_eq!(rollup.worker_unparks_per_s, 0.0);
        }
    }
}
