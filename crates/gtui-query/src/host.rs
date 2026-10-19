//! `HostSource` — the zero-dependency datasource backing the built-in Observe
//! dashboard. It samples the local machine's CPU/mem/load with the host's own
//! [`thegn_metrics::StatsSampler`] on a dedicated thread (the sampler is
//! blocking, stateful, and primes a CPU delta over two reads — so it can never
//! touch the UI thread), keeps a rolling ring of recent snapshots, and answers
//! queries by slicing that ring into a [`Frame`].
//!
//! Recognized exprs: `host_cpu_pct`, `host_mem_used` (GiB), `host_load1`.

use std::collections::VecDeque;
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use gtui_core::datasource::{DataSource, Query, QueryError};
use gtui_core::frame::{Coverage, Field, FieldType, Frame};
use thegn_metrics::{StatsSampler, StatsSnapshot};

/// Bound host retention by time and memory: at 1 Hz, a full day is about 2.7 MiB
/// of samples (about 4 MiB of deque capacity). The 24-hour window matches
/// `embed.rs::adjust_window`'s maximum supported query range.
const RING_CAP: usize = 86_400;
const RETENTION_SECS: f64 = 86_400.0;
/// Sampling cadence plus margin: retention eviction may trim a full window's
/// first sample by one second without making the window meaningfully partial.
const COVERAGE_SLACK_SECS: f64 = 2.0;
/// One nominal sampling interval plus the same tolerance used for leading
/// coverage. Larger intervals between retained samples indicate a hole.
const MAX_SAMPLE_GAP_SECS: f64 = 1.0 + COVERAGE_SLACK_SECS;

#[derive(Clone, Copy)]
struct HostSample {
    cpu_pct: Option<u8>,
    mem_used: Option<f32>,
    load1: Option<f32>,
}

impl From<&StatsSnapshot> for HostSample {
    fn from(snapshot: &StatsSnapshot) -> Self {
        Self {
            cpu_pct: snapshot.cpu_pct,
            mem_used: snapshot.mem_gib.map(|(used, _total)| used),
            load1: snapshot.load_avg.map(|(one, _, _)| one),
        }
    }
}

type Ring = Arc<Mutex<VecDeque<(f64, HostSample)>>>;

pub struct HostSource {
    ring: Ring,
    stop: Arc<AtomicBool>,
    /// Set while the consuming tab is hidden: the sampler parks (no sampling,
    /// no `nvidia-smi`/`ioreg` spawns, no timer) until [`DataSource::set_active`].
    parked: Arc<AtomicBool>,
    /// Set by the worker while it is blocked in its parked wait (test seam).
    #[cfg_attr(not(test), expect(dead_code, reason = "read only by tests"))]
    in_park: Arc<AtomicBool>,
    /// Set by the worker as its last act (test seam for prompt shutdown).
    #[cfg_attr(not(test), expect(dead_code, reason = "read only by tests"))]
    exited: Arc<AtomicBool>,
    worker: Option<JoinHandle<()>>,
}

/// Failure to start the host metrics sampler.
#[derive(Debug)]
pub struct HostSourceSpawnError(std::io::Error);

impl std::fmt::Display for HostSourceSpawnError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "could not start host metrics sampler: {}", self.0)
    }
}

impl std::error::Error for HostSourceSpawnError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&self.0)
    }
}

impl HostSource {
    pub fn try_new() -> Result<Self, HostSourceSpawnError> {
        Self::try_new_with(|| {})
    }

    /// Start the sampler, running `on_thread_start` before its first sample.
    /// Hosts use this hook to assign their platform-specific worker QoS.
    pub fn try_new_with(
        on_thread_start: impl FnOnce() + Send + 'static,
    ) -> Result<Self, HostSourceSpawnError> {
        Self::try_new_with_spawner(on_thread_start, |worker| {
            std::thread::Builder::new()
                .name("gtui-host-metrics".into())
                .spawn(worker)
        })
    }

    fn try_new_with_spawner(
        on_thread_start: impl FnOnce() + Send + 'static,
        spawn: impl FnOnce(Box<dyn FnOnce() + Send>) -> std::io::Result<JoinHandle<()>>,
    ) -> Result<Self, HostSourceSpawnError> {
        let ring: Ring = Arc::new(Mutex::new(VecDeque::new()));
        let stop = Arc::new(AtomicBool::new(false));
        let ring_bg = ring.clone();
        let stop_bg = stop.clone();
        let parked = Arc::new(AtomicBool::new(false));
        let parked_bg = parked.clone();
        let in_park = Arc::new(AtomicBool::new(false));
        let in_park_bg = in_park.clone();
        let exited = Arc::new(AtomicBool::new(false));
        let exited_bg = exited.clone();
        // Dedicated sampler thread: `StatsSampler::sample()` blocks (refreshes
        // sysinfo) and needs a warm-up read to prime the CPU delta, so it lives
        // off the UI thread and off the tokio runtime entirely.
        let worker = spawn(Box::new(move || {
            on_thread_start();
            let disk_path = std::env::current_dir().unwrap_or_else(|_| "/".into());
            let mut sampler = StatsSampler::new(disk_path);
            let mut was_parked = false;
            while !stop_bg.load(Ordering::Acquire) {
                if parked_bg.load(Ordering::Acquire) {
                    // Hidden: block untimed. `set_active(true)` and shutdown
                    // both `unpark`; a spurious wake just re-checks the flags.
                    was_parked = true;
                    in_park_bg.store(true, Ordering::Release);
                    std::thread::park();
                    in_park_bg.store(false, Ordering::Release);
                    continue;
                }
                if was_parked {
                    was_parked = false;
                    // The CPU delta spans everything since the last read, so
                    // the first reading after a long park would be one bogus
                    // average. Take a discarded priming read, let the delta
                    // window open, then sample normally.
                    let _priming = sampler.sample();
                    std::thread::park_timeout(Duration::from_millis(300));
                    continue;
                }
                let snap = sampler.sample();
                let ts = now_secs();
                // Cancellation can arrive during sample(); skip publishing a
                // sample taken after shutdown began (one post-sample check;
                // a stop landing after it can still let one sample through).
                if stop_bg.load(Ordering::Acquire) {
                    break;
                }
                if let Ok(mut r) = ring_bg.lock() {
                    admit(&mut r, ts, snap);
                }
                // `unpark` interrupts this wait, retaining a 1 Hz cadence
                // while making idle shutdown immediate.
                std::thread::park_timeout(Duration::from_secs(1));
            }
            exited_bg.store(true, Ordering::Release);
        }))
        .map_err(HostSourceSpawnError)?;
        Ok(Self {
            ring,
            stop,
            parked,
            in_park,
            exited,
            worker: Some(worker),
        })
    }

    /// Build a frame using the current host wall clock as the live-clock bound.
    fn frame_for(&self, expr: &str, range: &gtui_core::datasource::TimeRange) -> Frame {
        self.frame_for_at(expr, range, now_secs())
    }

    /// Build a `(time, value)` frame for `expr` from the ring, keeping only
    /// samples where the metric is present. Both UTC range endpoints are
    /// inclusive. Coverage uses Unix seconds and marks a leading gap when the
    /// earliest retained sample is more than two seconds after `from`. For
    /// recognized metrics, a gap in the retained timestamps clipped to the
    /// requested window is partial when consecutive points are more than one
    /// sampling interval plus the coverage slack apart. `now` is the live host
    /// wall-clock bound for the effective window end, `min(to, now)`; when no
    /// retained sample follows the requested end, this endpoint detects a
    /// trailing gap without extending coverage into the future.
    fn frame_for_at(
        &self,
        expr: &str,
        range: &gtui_core::datasource::TimeRange,
        now: f64,
    ) -> Frame {
        let (extract, recognized): (fn(&HostSample) -> Option<f64>, bool) = match expr {
            "host_cpu_pct" => (|s| s.cpu_pct.map(|v| v as f64), true),
            "host_mem_used" => (|s| s.mem_used.map(|v| v as f64), true),
            "host_load1" => (|s| s.load1.map(|v| v as f64), true),
            _ => (|_| None, false),
        };
        let mut times = Vec::new();
        let mut values = Vec::new();
        let from = range.from.timestamp() as f64
            + f64::from(range.from.timestamp_subsec_nanos()) / 1_000_000_000.0;
        let to = range.to.timestamp() as f64
            + f64::from(range.to.timestamp_subsec_nanos()) / 1_000_000_000.0;
        let coverage = if recognized {
            match self.ring.lock() {
                Ok(r) => {
                    let mut coverage = coverage_for(r.front().map(|(ts, _)| *ts), from);
                    // Samples are sorted; both UTC endpoints are inclusive.
                    let start = r.partition_point(|(ts, _)| *ts < from);
                    let end = r.partition_point(|(ts, _)| *ts <= to);
                    let sample_start = start.min(end);
                    let last_point = if end > sample_start {
                        Some(r[end - 1].0)
                    } else {
                        (sample_start > 0).then_some(from)
                    };
                    let trailing_point = if end < r.len() {
                        Some(to)
                    } else {
                        let effective_end = to.min(now);
                        (effective_end >= from
                            && last_point.is_some_and(|last| effective_end > last))
                        .then_some(effective_end)
                    };
                    let has_gap = has_interior_gap(
                        std::iter::once(from)
                            .filter(|_| sample_start > 0)
                            .chain(
                                r.iter()
                                    .skip(sample_start)
                                    .take(end - sample_start)
                                    .map(|(ts, _)| *ts),
                            )
                            .chain(trailing_point),
                    );
                    if has_gap && matches!(coverage, Coverage::Complete) {
                        coverage = Coverage::Partial {
                            retained_from: r.front().map(|(ts, _)| *ts),
                        };
                    }
                    for index in sample_start..end {
                        let (ts, sample) = &r[index];
                        if let Some(v) = extract(sample) {
                            times.push(*ts);
                            values.push(v);
                        }
                    }
                    coverage
                }
                Err(_) => Coverage::Partial {
                    retained_from: None,
                },
            }
        } else {
            Coverage::Complete
        };
        Frame::new(vec![
            Field::new("time", FieldType::Time, times),
            Field::new(expr, FieldType::Float64, values),
        ])
        .with_coverage(coverage)
    }
}

impl Drop for HostSource {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(worker) = self.worker.take() {
            worker.thread().unpark();
            // The last owner often drops on a tokio runtime worker, and the
            // sampler can be mid-`sample()` (spawning nvidia-smi/ioreg), so
            // joining here could block that worker for 100s of ms. Hand the
            // join to a short-lived reaper thread; if even that cannot be
            // spawned, join inline rather than leak the handle silently.
            let slot = Arc::new(Mutex::new(Some(worker)));
            let reaper_slot = slot.clone();
            let spawned = std::thread::Builder::new()
                .name("gtui-host-reaper".into())
                .spawn(move || {
                    let w = reaper_slot.lock().ok().and_then(|mut g| g.take());
                    if let Some(w) = w {
                        reap(w);
                    }
                });
            if spawned.is_err() {
                let w = slot.lock().ok().and_then(|mut g| g.take());
                if let Some(w) = w {
                    reap(w);
                }
            }
        }
    }
}

fn reap(worker: JoinHandle<()>) {
    if worker.join().is_err() {
        tracing::warn!(target: "gtui::host", "host metrics sampler panicked");
    }
}

impl DataSource for HostSource {
    fn set_active(&self, active: bool) {
        self.parked.store(!active, Ordering::Release);
        if active && let Some(worker) = &self.worker {
            worker.thread().unpark();
        }
    }

    fn query(
        &self,
        queries: Vec<Query>,
    ) -> Pin<Box<dyn Future<Output = Result<Vec<Frame>, QueryError>> + Send>> {
        let frames: Vec<Frame> = queries
            .iter()
            .map(|q| self.frame_for(&q.expr, &q.time_range))
            .collect();
        Box::pin(async move { Ok(frames) })
    }
}

fn now_secs() -> f64 {
    timestamp_secs(SystemTime::now())
}

/// Admit a sample against the current wall clock, keeping a sorted, bounded
/// window. Samples newer than `ts` are discarded after a backward clock step;
/// equal timestamps retain arrival order.
fn admit(ring: &mut VecDeque<(f64, HostSample)>, ts: f64, snap: StatsSnapshot) {
    let cutoff = ts - RETENTION_SECS;
    while ring
        .front()
        .is_some_and(|(sample_ts, _)| *sample_ts < cutoff)
    {
        ring.pop_front();
    }
    while ring.back().is_some_and(|(sample_ts, _)| *sample_ts > ts) {
        ring.pop_back();
    }
    let index = ring.partition_point(|(sample_ts, _)| *sample_ts <= ts);
    ring.insert(index, (ts, HostSample::from(&snap)));
    while ring.len() > RING_CAP {
        ring.pop_front();
    }
}

fn timestamp_secs(now: SystemTime) -> f64 {
    now.duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

fn coverage_for(front: Option<f64>, from: f64) -> Coverage {
    match front {
        Some(retained_from) if retained_from <= from + COVERAGE_SLACK_SECS => Coverage::Complete,
        Some(retained_from) => Coverage::Partial {
            retained_from: Some(retained_from),
        },
        None => Coverage::Partial {
            retained_from: None,
        },
    }
}

fn has_interior_gap(points: impl IntoIterator<Item = f64>) -> bool {
    let mut previous = None;
    for point in points {
        if let Some(previous) = previous
            && point - previous > MAX_SAMPLE_GAP_SECS
        {
            return true;
        }
        previous = Some(point);
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;
    use gtui_core::datasource::TimeRange;

    fn range(from: i64, to: i64) -> TimeRange {
        TimeRange {
            from: chrono::DateTime::from_timestamp(from, 0).unwrap(),
            to: chrono::DateTime::from_timestamp(to, 0).unwrap(),
        }
    }

    fn admit_cpu_range(source: &HostSource, ranges: &[std::ops::RangeInclusive<i64>]) {
        source.set_active(false);
        wait_for("worker parked", || source.in_park.load(Ordering::Acquire));
        let mut ring = source.ring.lock().unwrap();
        for range in ranges {
            for ts in range.clone() {
                admit(
                    &mut ring,
                    ts as f64,
                    StatsSnapshot {
                        cpu_pct: Some(ts as u8),
                        ..Default::default()
                    },
                );
            }
        }
    }

    fn admit_cpu_range_only(source: &HostSource, ranges: &[std::ops::RangeInclusive<i64>]) {
        source.set_active(false);
        wait_for("worker parked", || source.in_park.load(Ordering::Acquire));
        source.ring.lock().unwrap().clear();
        admit_cpu_range(source, ranges);
    }

    fn q(expr: &str) -> Query {
        Query {
            ref_id: "A".into(),
            expr: expr.into(),
            time_range: TimeRange {
                from: Utc::now(),
                to: Utc::now(),
            },
        }
    }

    #[tokio::test]
    async fn query_returns_a_frame_per_query_with_two_fields() {
        let source = HostSource::try_new().unwrap();
        let res = source
            .query(vec![q("host_cpu_pct"), q("host_load1")])
            .await
            .unwrap();
        assert_eq!(res.len(), 2);
        // Each frame carries a time field + a value field, even before any
        // sample has landed (empty series).
        assert_eq!(res[0].fields.len(), 2);
        assert_eq!(res[0].fields[0].ty, FieldType::Time);
        assert_eq!(res[0].fields[1].ty, FieldType::Float64);
        assert_eq!(res[0].fields[1].name, "host_cpu_pct");
    }

    #[tokio::test]
    async fn unknown_expr_yields_empty_value_series() {
        let source = HostSource::try_new().unwrap();
        let res = source.query(vec![q("nope")]).await.unwrap();
        assert_eq!(res[0].fields[1].len(), 0);
    }

    #[test]
    fn query_uses_inclusive_range_and_sorted_sample_order() {
        let source = HostSource::try_new().unwrap();
        source.set_active(false);
        wait_for("worker parked", || source.in_park.load(Ordering::Acquire));
        let mut ring = source.ring.lock().unwrap();
        for (ts, cpu) in [(10.0, 10), (30.0, 30), (20.0, 20), (20.0, 21)] {
            admit(
                &mut ring,
                ts,
                StatsSnapshot {
                    cpu_pct: Some(cpu),
                    ..Default::default()
                },
            );
        }
        drop(ring);

        let range = TimeRange {
            from: chrono::DateTime::from_timestamp(20, 0).unwrap(),
            to: chrono::DateTime::from_timestamp(20, 0).unwrap(),
        };
        let frame = source.frame_for("host_cpu_pct", &range);
        assert_eq!(frame.fields[0].floats(), &[20.0, 20.0]);
        assert_eq!(frame.fields[1].floats(), &[20.0, 21.0]);
    }

    #[test]
    fn query_marks_a_window_starting_before_retention_as_partial() {
        let source = HostSource::try_new().unwrap();
        source.set_active(false);
        wait_for("worker parked", || source.in_park.load(Ordering::Acquire));
        let mut ring = source.ring.lock().unwrap();
        for ts in [1000.0, 1001.0] {
            admit(
                &mut ring,
                ts,
                StatsSnapshot {
                    cpu_pct: Some(ts as u8),
                    ..Default::default()
                },
            );
        }
        drop(ring);

        let range = |from, to| TimeRange {
            from: chrono::DateTime::from_timestamp(from, 0).unwrap(),
            to: chrono::DateTime::from_timestamp(to, 0).unwrap(),
        };
        assert_eq!(
            source.frame_for("host_cpu_pct", &range(900, 1001)).coverage,
            Coverage::Partial {
                retained_from: Some(1000.0)
            }
        );
        assert_eq!(
            source
                .frame_for("host_cpu_pct", &range(1000, 1001))
                .coverage,
            Coverage::Complete
        );
    }

    #[test]
    fn query_marks_interior_and_straddling_ring_gaps_as_partial() {
        let source = HostSource::try_new().unwrap();
        admit_cpu_range(&source, &[100..=105, 200..=205]);

        assert_eq!(
            source.frame_for("host_cpu_pct", &range(100, 205)).coverage,
            Coverage::Partial {
                retained_from: Some(100.0)
            }
        );
        assert_eq!(
            source.frame_for("host_cpu_pct", &range(200, 205)).coverage,
            Coverage::Complete
        );
        assert_eq!(
            source.frame_for("host_cpu_pct", &range(120, 180)).coverage,
            Coverage::Partial {
                retained_from: Some(100.0)
            }
        );
    }

    #[test]
    fn query_marks_a_gap_straddling_the_window_start_as_partial() {
        let source = HostSource::try_new().unwrap();
        admit_cpu_range(&source, &[100..=105, 300..=305]);

        assert_eq!(
            source.frame_for("host_cpu_pct", &range(150, 305)).coverage,
            Coverage::Partial {
                retained_from: Some(100.0)
            }
        );
    }

    #[test]
    fn query_keeps_a_contiguous_window_complete() {
        let source = HostSource::try_new().unwrap();
        admit_cpu_range(&source, &[100..=110]);

        assert_eq!(
            source.frame_for("host_cpu_pct", &range(100, 110)).coverage,
            Coverage::Complete
        );
    }

    #[test]
    fn trailing_hole_after_the_newest_sample_is_partial() {
        let source = HostSource::try_new().unwrap();
        admit_cpu_range_only(&source, &[100..=110]);

        assert_eq!(
            source
                .frame_for_at("host_cpu_pct", &range(100, 200), 200.0)
                .coverage,
            Coverage::Partial {
                retained_from: Some(100.0)
            }
        );
    }

    #[test]
    fn window_ending_at_the_newest_sample_stays_complete() {
        let source = HostSource::try_new().unwrap();
        admit_cpu_range_only(&source, &[100..=110]);

        assert_eq!(
            source
                .frame_for_at("host_cpu_pct", &range(100, 110), 500.0)
                .coverage,
            Coverage::Complete
        );
        assert_eq!(
            source
                .frame_for_at("host_cpu_pct", &range(100, 113), 113.0)
                .coverage,
            Coverage::Complete
        );
        assert_eq!(
            source
                .frame_for_at("host_cpu_pct", &range(100, 114), 114.0)
                .coverage,
            Coverage::Partial {
                retained_from: Some(100.0)
            }
        );
    }

    #[test]
    fn future_window_end_is_clamped_to_now() {
        let source = HostSource::try_new().unwrap();
        admit_cpu_range_only(&source, &[100..=110]);

        assert_eq!(
            source
                .frame_for_at("host_cpu_pct", &range(100, 400), 111.0)
                .coverage,
            Coverage::Complete
        );
        assert_eq!(
            source
                .frame_for_at("host_cpu_pct", &range(100, 400), 300.0)
                .coverage,
            Coverage::Partial {
                retained_from: Some(100.0)
            }
        );
    }

    #[test]
    fn window_after_all_samples_is_partial() {
        let source = HostSource::try_new().unwrap();
        admit_cpu_range_only(&source, &[100..=110]);

        assert_eq!(
            source
                .frame_for_at("host_cpu_pct", &range(150, 160), 160.0)
                .coverage,
            Coverage::Partial {
                retained_from: Some(100.0)
            }
        );
    }

    #[test]
    fn unknown_expression_stays_complete_with_a_trailing_hole() {
        let source = HostSource::try_new().unwrap();
        admit_cpu_range_only(&source, &[100..=110]);

        assert_eq!(
            source
                .frame_for_at("nope", &range(100, 200), 200.0)
                .coverage,
            Coverage::Complete
        );
    }

    #[test]
    fn interior_gap_boundary_is_strict() {
        for (points, expected) in [
            (vec![10.0, 10.0 + MAX_SAMPLE_GAP_SECS], false),
            (vec![10.0, 10.0 + MAX_SAMPLE_GAP_SECS + 0.000_001], true),
        ] {
            assert_eq!(has_interior_gap(points), expected);
        }
    }

    #[test]
    fn empty_ring_is_reported_as_partial_retention() {
        let source = HostSource::try_new().unwrap();
        source.set_active(false);
        wait_for("worker parked", || source.in_park.load(Ordering::Acquire));
        source.ring.lock().unwrap().clear();
        let range = TimeRange {
            from: chrono::DateTime::from_timestamp(900, 0).unwrap(),
            to: chrono::DateTime::from_timestamp(1000, 0).unwrap(),
        };
        assert_eq!(
            source.frame_for("host_cpu_pct", &range).coverage,
            Coverage::Partial {
                retained_from: None
            }
        );
        assert_eq!(
            source.frame_for("unknown", &range).coverage,
            Coverage::Complete
        );
    }

    #[test]
    fn coverage_slack_boundary_is_inclusive() {
        assert_eq!(coverage_for(Some(102.0), 100.0), Coverage::Complete);
        assert_eq!(
            coverage_for(Some(102.000_001), 100.0),
            Coverage::Partial {
                retained_from: Some(102.000_001)
            }
        );
        assert_eq!(
            coverage_for(None, 100.0),
            Coverage::Partial {
                retained_from: None
            }
        );
    }

    #[test]
    fn backward_clock_step_discards_future_samples() {
        let mut ring = VecDeque::new();
        for ts in [1000.0, 1100.0, 900.0] {
            admit(
                &mut ring,
                ts,
                StatsSnapshot {
                    cpu_pct: Some(ts as u8),
                    ..Default::default()
                },
            );
        }
        assert_eq!(
            ring.iter().map(|(ts, _)| *ts).collect::<Vec<_>>(),
            vec![900.0]
        );
    }

    #[test]
    fn retention_evicts_strictly_older_samples_and_keeps_boundary() {
        let mut ring = VecDeque::new();
        for ts in [100.0, 86_501.0] {
            admit(
                &mut ring,
                ts,
                StatsSnapshot {
                    cpu_pct: Some(ts as u8),
                    ..Default::default()
                },
            );
        }
        assert_eq!(
            ring.iter().map(|(ts, _)| *ts).collect::<Vec<_>>(),
            vec![86_501.0]
        );

        let mut ring = VecDeque::new();
        admit(&mut ring, 700.0, StatsSnapshot::default());
        admit(&mut ring, 87_100.0, StatsSnapshot::default());
        assert_eq!(
            ring.iter().map(|(ts, _)| *ts).collect::<Vec<_>>(),
            vec![700.0, 87_100.0]
        );
    }

    #[test]
    fn retention_covers_the_default_fifteen_minute_window() {
        let mut ring = VecDeque::new();
        admit(&mut ring, 0.0, StatsSnapshot::default());
        admit(&mut ring, 900.0, StatsSnapshot::default());
        assert_eq!(
            ring.iter().map(|(ts, _)| *ts).collect::<Vec<_>>(),
            vec![0.0, 900.0]
        );
    }

    #[test]
    fn retention_covers_the_maximum_zoom_window() {
        let mut ring = VecDeque::new();
        admit(&mut ring, 0.0, StatsSnapshot::default());
        admit(&mut ring, 86_400.0, StatsSnapshot::default());
        assert_eq!(
            ring.iter().map(|(ts, _)| *ts).collect::<Vec<_>>(),
            vec![0.0, 86_400.0]
        );

        admit(&mut ring, 86_401.0, StatsSnapshot::default());
        assert_eq!(
            ring.iter().map(|(ts, _)| *ts).collect::<Vec<_>>(),
            vec![86_400.0, 86_401.0]
        );
    }

    #[test]
    fn seeded_timestamp_interleavings_are_sorted_and_stable_for_duplicates() {
        let mut ring = VecDeque::new();
        let mut seed = 0x5eed_u64;
        for arrival in 0..256 {
            seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1);
            let ts = ((seed >> 32) % 80) as f64;
            admit(
                &mut ring,
                ts,
                StatsSnapshot {
                    cpu_pct: Some(arrival as u8),
                    ..Default::default()
                },
            );
        }
        assert!(ring.len() > 1, "seeded ring should retain multiple samples");
        let samples: Vec<_> = ring.iter().collect();
        let mut saw_duplicate_timestamp = false;
        for pair in samples.windows(2) {
            if pair[0].0 == pair[1].0 {
                saw_duplicate_timestamp = true;
                assert!(
                    pair[0].1.cpu_pct.unwrap() < pair[1].1.cpu_pct.unwrap(),
                    "equal-timestamp samples must retain arrival order"
                );
            }
        }
        assert!(
            saw_duplicate_timestamp,
            "seeded ring should exercise duplicate timestamps"
        );

        let source = HostSource::try_new().unwrap();
        source.set_active(false);
        wait_for("worker parked", || source.in_park.load(Ordering::Acquire));
        *source.ring.lock().unwrap() = ring.clone();
        let range = TimeRange {
            from: chrono::DateTime::from_timestamp(0, 0).unwrap(),
            to: chrono::DateTime::from_timestamp(100, 0).unwrap(),
        };
        let frame = source.frame_for("host_cpu_pct", &range);
        let times = frame.fields[0].floats();
        assert!(times.windows(2).all(|pair| pair[0] <= pair[1]));
    }

    #[tokio::test]
    async fn final_source_drop_does_not_invalidate_an_in_flight_query_future() {
        let source = HostSource::try_new().unwrap();
        let query = source.query(vec![q("host_cpu_pct")]);
        drop(source);
        let frames = query.await.unwrap();
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0].fields.len(), 2);
    }

    #[test]
    fn spawn_failure_is_returned_to_the_caller() {
        let error = HostSource::try_new_with_spawner(
            || {},
            |_| {
                Err(std::io::Error::new(
                    std::io::ErrorKind::WouldBlock,
                    "injected thread exhaustion",
                ))
            },
        )
        .err()
        .expect("injected spawn failure must be returned");
        assert!(error.to_string().contains("injected thread exhaustion"));
    }

    #[test]
    fn dropping_an_idle_sampler_wakes_and_joins_it_promptly() {
        let source = HostSource::try_new().unwrap();
        let deadline = std::time::Instant::now() + Duration::from_secs(3);
        while source.ring.lock().unwrap().is_empty() && std::time::Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(
            !source.ring.lock().unwrap().is_empty(),
            "sampler never published"
        );

        // The worker holds a clone of the ring; it is released only when the
        // worker exits. Without stop+unpark the worker stays parked for 1s
        // per cycle and never exits, so the ring would stay alive.
        let weak = Arc::downgrade(&source.ring);
        let start = std::time::Instant::now();
        drop(source);
        // Join happens on a reaper thread, so wait (bounded) for release.
        while weak.upgrade().is_some() && start.elapsed() < Duration::from_secs(2) {
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(
            weak.upgrade().is_none(),
            "sampler worker did not exit promptly after drop"
        );
    }

    #[test]
    fn repeated_construction_and_drop_does_not_leave_workers_running() {
        let starts = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        for _ in 0..8 {
            let started = Arc::clone(&starts);
            let source = HostSource::try_new_with(move || {
                started.fetch_add(1, Ordering::SeqCst);
            })
            .unwrap();
            drop(source);
        }
        // Joins now happen on reaper threads; wait (bounded) for all 8 workers.
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        while starts.load(Ordering::SeqCst) < 8 && std::time::Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(starts.load(Ordering::SeqCst), 8);
    }

    #[test]
    fn worker_panic_is_joined_without_panicking_drop() {
        let source = HostSource::try_new_with(|| panic!("injected sampler worker panic")).unwrap();
        drop(source);
    }

    #[test]
    fn poisoned_ring_is_reported_as_an_empty_frame() {
        let source = HostSource::try_new().unwrap();
        let ring = Arc::clone(&source.ring);
        let poisoner = std::thread::spawn(move || {
            let _guard = ring.lock().unwrap();
            panic!("inject ring lock poisoning");
        });
        assert!(poisoner.join().is_err());
        let query = q("host_cpu_pct");
        let frame = source.frame_for("host_cpu_pct", &query.time_range);
        assert_eq!(frame.fields[0].len(), 0);
        assert_eq!(frame.fields[1].len(), 0);
        assert_eq!(
            frame.coverage,
            Coverage::Partial {
                retained_from: None
            }
        );
    }

    #[test]
    fn clock_before_epoch_uses_the_existing_zero_fallback() {
        let before_epoch = UNIX_EPOCH - Duration::from_secs(1);
        assert_eq!(timestamp_secs(before_epoch), 0.0);
    }

    fn wait_for(what: &str, cond: impl Fn() -> bool) {
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while !cond() {
            assert!(std::time::Instant::now() < deadline, "timed out: {what}");
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    #[test]
    fn a_parked_sampler_stops_sampling_and_resumes_on_activation() {
        let source = HostSource::try_new().unwrap();
        let len = || source.ring.lock().unwrap().len();
        wait_for("first sample", || len() > 0);
        source.set_active(false);
        // The worker signals once it is blocked in the parked wait; nothing
        // after that point may publish a sample.
        wait_for("worker parked", || source.in_park.load(Ordering::Acquire));
        let parked_len = len();
        std::thread::sleep(Duration::from_millis(1300));
        assert_eq!(len(), parked_len, "parked sampler kept sampling");
        source.set_active(true);
        wait_for("sampling resumed", || len() > parked_len);
    }

    #[test]
    fn dropping_a_parked_sampler_actually_exits_the_worker_promptly() {
        let source = HostSource::try_new().unwrap();
        source.set_active(false);
        wait_for("worker parked", || source.in_park.load(Ordering::Acquire));
        let exited = source.exited.clone();
        drop(source);
        wait_for("worker exit", || exited.load(Ordering::Acquire));
    }
}
