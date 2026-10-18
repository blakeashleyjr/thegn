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
use std::sync::{OnceLock, mpsc};

use crate::platform::thread_census::{self, ThreadSample};

static CENSUS_TX: OnceLock<mpsc::SyncSender<f64>> = OnceLock::new();
const CENSUS_CAP: usize = 256;
const CENSUS_LOG_LIMIT: usize = 12;

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

    let Some(sender) = census_sender(crate::perf::enabled()) else {
        return;
    };
    if let Err(error) = sender.try_send(interval_seconds) {
        tracing::debug!(target: "thegn::perf::rt", %error, "thread census interval skipped");
    }
}

fn census_sender(enabled: bool) -> Option<&'static mpsc::SyncSender<f64>> {
    if !enabled {
        return None;
    }
    Some(CENSUS_TX.get_or_init(|| {
        let (sender, receiver) = mpsc::sync_channel::<f64>(1);
        let _worker = thegn_core::thread_name::spawn_named("thegn-perf-thr", move || {
            crate::platform::qos::set_self(crate::platform::qos::Qos::Background);
            census_loop(receiver);
        });
        sender
    }))
}

#[derive(Clone, Copy, Debug, Default, PartialEq)]
struct ThreadRate {
    sample: ThreadSample,
    wakes_per_s: f64,
    cpu_ms_per_s: f64,
}

#[derive(Clone, Copy, Debug, Default, PartialEq)]
struct CensusSummary {
    threads: usize,
    rt_threads: usize,
    rt_wakes_per_s: f64,
    others_wakes_per_s: f64,
}

fn rank_thread_deltas(
    current: &[ThreadSample],
    previous: &mut Vec<ThreadSample>,
    ranked: &mut Vec<ThreadRate>,
    interval_seconds: f64,
    clock_ticks_per_second: u64,
) -> CensusSummary {
    let seconds = if interval_seconds.is_finite() && interval_seconds > 0.0 {
        interval_seconds
    } else {
        0.0
    };
    ranked.clear();
    let mut summary = CensusSummary {
        threads: current.len(),
        ..CensusSummary::default()
    };
    for sample in current {
        if sample.comm_str() == "thegn-rt" {
            summary.rt_threads += 1;
        }
        let Some(old) = previous.iter().find(|old| old.tid == sample.tid) else {
            continue;
        };
        let (Some(voluntary), Some(old_voluntary), Some(nonvoluntary), Some(old_nonvoluntary)) = (
            sample.voluntary,
            old.voluntary,
            sample.nonvoluntary,
            old.nonvoluntary,
        ) else {
            continue;
        };
        let wakes = voluntary
            .saturating_sub(old_voluntary)
            .saturating_add(nonvoluntary.saturating_sub(old_nonvoluntary));
        let cpu_ticks = match (sample.utime_ticks, old.utime_ticks) {
            (Some(current), Some(previous)) => current.saturating_sub(previous),
            _ => 0,
        };
        let wakes_per_s = if seconds > 0.0 {
            wakes as f64 / seconds
        } else {
            0.0
        };
        let cpu_ms_per_s = if seconds > 0.0 && clock_ticks_per_second > 0 {
            cpu_ticks as f64 * 1000.0 / (clock_ticks_per_second as f64 * seconds)
        } else {
            0.0
        };
        if sample.comm_str() == "thegn-rt" {
            summary.rt_wakes_per_s += wakes_per_s;
        } else {
            summary.others_wakes_per_s += wakes_per_s;
        }
        ranked.push(ThreadRate {
            sample: *sample,
            wakes_per_s,
            cpu_ms_per_s,
        });
    }
    previous.clear();
    previous.extend_from_slice(current);
    ranked.sort_by(|left, right| right.wakes_per_s.total_cmp(&left.wakes_per_s));
    ranked.truncate(CENSUS_LOG_LIMIT);
    summary
}

fn census_loop(receiver: mpsc::Receiver<f64>) {
    let mut samples = Vec::with_capacity(CENSUS_CAP);
    let mut previous = Vec::with_capacity(CENSUS_CAP);
    let mut ranked = Vec::with_capacity(CENSUS_CAP);
    let clock_ticks_per_second = thread_census::clock_ticks_per_second();
    while let Ok(interval_seconds) = receiver.recv() {
        thread_census::sample_self(&mut samples, CENSUS_CAP);
        let summary = rank_thread_deltas(
            &samples,
            &mut previous,
            &mut ranked,
            interval_seconds,
            clock_ticks_per_second,
        );
        tracing::info!(
            target: "thegn::perf::rt",
            threads = summary.threads,
            rt_threads = summary.rt_threads,
            rt_wakes_per_s = summary.rt_wakes_per_s,
            others_wakes_per_s = summary.others_wakes_per_s,
            "thread census summary"
        );
        for rate in &ranked {
            tracing::info!(
                target: "thegn::perf::rt",
                tid = rate.sample.tid,
                comm = rate.sample.comm_str(),
                wakes_per_s = rate.wakes_per_s,
                cpu_ms_per_s = rate.cpu_ms_per_s,
                syscall = rate.sample.syscall,
                fd_target = rate.sample.fd_target_str(),
                wchan = rate.sample.wchan_str(),
                "thread census"
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    static TEST_GUARD: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn clear_counts() {
        let _counts = take_counts();
    }

    fn sample(tid: u32, comm: &str, voluntary: u64, nonvoluntary: u64, utime: u64) -> ThreadSample {
        let mut sample = ThreadSample {
            tid,
            voluntary: Some(voluntary),
            nonvoluntary: Some(nonvoluntary),
            utime_ticks: Some(utime),
            ..ThreadSample::default()
        };
        sample.comm_len = comm.len() as u8;
        sample.comm[..comm.len()].copy_from_slice(comm.as_bytes());
        sample
    }

    #[test]
    fn census_delta_drops_vanished_threads_and_ignores_new_or_wrapped_counters() {
        let mut previous = vec![
            sample(1, "thegn-rt", 10, 4, 10),
            sample(2, "thegn-rt", 20, 2, 20),
        ];
        let current = [
            sample(1, "thegn-rt", 15, 6, 14),
            sample(3, "new-thread", 50, 50, 50),
            sample(4, "thegn-rt", 1, 1, 1),
        ];
        let mut ranked = Vec::with_capacity(4);
        let summary = rank_thread_deltas(&current, &mut previous, &mut ranked, 2.0, 100);

        assert_eq!(
            ranked.len(),
            1,
            "new tids have no first interval and vanished tids are removed"
        );
        assert_eq!(ranked[0].sample.tid, 1);
        assert_eq!(ranked[0].wakes_per_s, 3.5);
        assert_eq!(ranked[0].cpu_ms_per_s, 20.0);
        assert_eq!(previous.as_slice(), current.as_slice());
        assert_eq!(summary.threads, 3);
        assert_eq!(summary.rt_threads, 2);
        assert_eq!(summary.rt_wakes_per_s, 3.5);
        assert_eq!(summary.others_wakes_per_s, 0.0);
    }

    #[test]
    fn disabled_perf_does_not_start_the_census_worker() {
        assert!(CENSUS_TX.get().is_none());
        assert!(census_sender(false).is_none());
        assert!(CENSUS_TX.get().is_none());
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
