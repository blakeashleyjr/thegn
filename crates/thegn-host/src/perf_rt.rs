//! Tokio runtime wake and blocking-pool attribution for the perf rollup.
//!
//! Tokio's stable park/unpark hooks run on scheduler workers. Its start/stop
//! hooks describe runtime pool thread lifecycle, so those totals deliberately
//! stay named as pool threads rather than claiming a worker/blocking split.

use std::sync::atomic::{AtomicU64, Ordering};

static POOL_THREAD_STARTS: AtomicU64 = AtomicU64::new(0);
static POOL_THREAD_STOPS: AtomicU64 = AtomicU64::new(0);
static WORKER_PARKS: AtomicU64 = AtomicU64::new(0);
static WORKER_UNPARKS: AtomicU64 = AtomicU64::new(0);

#[derive(Clone, Copy, Debug, Default, PartialEq)]
struct RuntimeRollup {
    interval_seconds: f64,
    pool_thread_starts: u64,
    pool_thread_stops: u64,
    worker_parks: u64,
    worker_unparks: u64,
    pool_thread_starts_per_s: f64,
    pool_thread_stops_per_s: f64,
    worker_parks_per_s: f64,
    worker_unparks_per_s: f64,
    num_workers: usize,
    num_alive_tasks: usize,
    global_queue_depth: usize,
    num_blocking_threads: usize,
}

impl RuntimeRollup {
    fn new(counts: Counts, interval_seconds: f64, metrics: RuntimeMetrics) -> Self {
        let seconds = interval_seconds.max(f64::MIN_POSITIVE);
        Self {
            interval_seconds,
            pool_thread_starts: counts.pool_thread_starts,
            pool_thread_stops: counts.pool_thread_stops,
            worker_parks: counts.worker_parks,
            worker_unparks: counts.worker_unparks,
            pool_thread_starts_per_s: counts.pool_thread_starts as f64 / seconds,
            pool_thread_stops_per_s: counts.pool_thread_stops as f64 / seconds,
            worker_parks_per_s: counts.worker_parks as f64 / seconds,
            worker_unparks_per_s: counts.worker_unparks as f64 / seconds,
            num_workers: metrics.num_workers,
            num_alive_tasks: metrics.num_alive_tasks,
            global_queue_depth: metrics.global_queue_depth,
            num_blocking_threads: metrics.num_blocking_threads,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct Counts {
    pool_thread_starts: u64,
    pool_thread_stops: u64,
    worker_parks: u64,
    worker_unparks: u64,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct RuntimeMetrics {
    num_workers: usize,
    num_alive_tasks: usize,
    global_queue_depth: usize,
    num_blocking_threads: usize,
}

fn add(counter: &AtomicU64, enabled: bool) {
    if enabled {
        counter.fetch_add(1, Ordering::Relaxed);
    }
}

fn record_pool_start(enabled: bool) {
    add(&POOL_THREAD_STARTS, enabled);
}

fn record_pool_stop(enabled: bool) {
    add(&POOL_THREAD_STOPS, enabled);
}

fn record_worker_park(enabled: bool) {
    add(&WORKER_PARKS, enabled);
}

fn record_worker_unpark(enabled: bool) {
    add(&WORKER_UNPARKS, enabled);
}

pub(crate) fn on_thread_start() {
    record_pool_start(crate::perf::enabled());
}

pub(crate) fn on_thread_stop() {
    record_pool_stop(crate::perf::enabled());
}

pub(crate) fn on_thread_park() {
    record_worker_park(crate::perf::enabled());
}

pub(crate) fn on_thread_unpark() {
    record_worker_unpark(crate::perf::enabled());
}

fn take_counts() -> Counts {
    Counts {
        pool_thread_starts: POOL_THREAD_STARTS.swap(0, Ordering::Relaxed),
        pool_thread_stops: POOL_THREAD_STOPS.swap(0, Ordering::Relaxed),
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
                num_blocking_threads: metrics.num_blocking_threads(),
            }
        }
        Err(_) => RuntimeMetrics::default(),
    };
    let rollup = RuntimeRollup::new(take_counts(), interval_seconds, metrics);

    tracing::info!(
        target: "thegn::perf::rt",
        interval_seconds = rollup.interval_seconds,
        pool_thread_starts = rollup.pool_thread_starts,
        pool_thread_stops = rollup.pool_thread_stops,
        worker_parks = rollup.worker_parks,
        worker_unparks = rollup.worker_unparks,
        pool_thread_starts_per_s = rollup.pool_thread_starts_per_s,
        pool_thread_stops_per_s = rollup.pool_thread_stops_per_s,
        worker_parks_per_s = rollup.worker_parks_per_s,
        worker_unparks_per_s = rollup.worker_unparks_per_s,
        num_workers = rollup.num_workers,
        num_alive_tasks = rollup.num_alive_tasks,
        global_queue_depth = rollup.global_queue_depth,
        num_blocking_threads = rollup.num_blocking_threads,
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
        record_pool_start(true);
        record_pool_start(true);
        record_pool_stop(true);
        record_worker_park(true);
        record_worker_park(true);
        record_worker_park(true);
        record_worker_unpark(true);

        let first = take_counts();
        assert_eq!(first.pool_thread_starts, 2);
        assert_eq!(first.pool_thread_stops, 1);
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
        record_pool_start(false);
        record_pool_stop(false);
        record_worker_park(false);
        record_worker_unpark(false);

        assert_eq!(take_counts(), Counts::default());
    }

    #[test]
    fn rollup_rates_are_counts_divided_by_the_interval() {
        let rollup = RuntimeRollup::new(
            Counts {
                pool_thread_starts: 2,
                pool_thread_stops: 1,
                worker_parks: 6,
                worker_unparks: 4,
            },
            2.0,
            RuntimeMetrics {
                num_workers: 8,
                num_alive_tasks: 5,
                global_queue_depth: 3,
                num_blocking_threads: 2,
            },
        );

        assert_eq!(rollup.pool_thread_starts_per_s, 1.0);
        assert_eq!(rollup.pool_thread_stops_per_s, 0.5);
        assert_eq!(rollup.worker_parks_per_s, 3.0);
        assert_eq!(rollup.worker_unparks_per_s, 2.0);
        assert_eq!(rollup.num_workers, 8);
        assert_eq!(rollup.num_alive_tasks, 5);
        assert_eq!(rollup.global_queue_depth, 3);
        assert_eq!(rollup.num_blocking_threads, 2);
    }
}
