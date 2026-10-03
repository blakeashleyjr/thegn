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
use gtui_core::frame::{Field, FieldType, Frame};
use thegn_metrics::{StatsSampler, StatsSnapshot};

/// How many samples to retain (≈ this many seconds at the 1 Hz sample rate).
const RING_CAP: usize = 600;

type Ring = Arc<Mutex<VecDeque<(f64, StatsSnapshot)>>>;

pub struct HostSource {
    ring: Ring,
    stop: Arc<AtomicBool>,
    /// Set while the consuming tab is hidden: the sampler parks (no sampling,
    /// no `nvidia-smi`/`ioreg` spawns, no timer) until [`DataSource::set_active`].
    parked: Arc<AtomicBool>,
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
        let ring: Ring = Arc::new(Mutex::new(VecDeque::with_capacity(RING_CAP)));
        let stop = Arc::new(AtomicBool::new(false));
        let ring_bg = ring.clone();
        let stop_bg = stop.clone();
        let parked = Arc::new(AtomicBool::new(false));
        let parked_bg = parked.clone();
        // Dedicated sampler thread: `StatsSampler::sample()` blocks (refreshes
        // sysinfo) and needs a warm-up read to prime the CPU delta, so it lives
        // off the UI thread and off the tokio runtime entirely.
        let worker = spawn(Box::new(move || {
            on_thread_start();
            let disk_path = std::env::current_dir().unwrap_or_else(|_| "/".into());
            let mut sampler = StatsSampler::new(disk_path);
            while !stop_bg.load(Ordering::Acquire) {
                if parked_bg.load(Ordering::Acquire) {
                    // Hidden: block untimed. `set_active(true)` and shutdown
                    // both `unpark`; a spurious wake just re-checks the flags.
                    std::thread::park();
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
                    r.push_back((ts, snap));
                    while r.len() > RING_CAP {
                        r.pop_front();
                    }
                }
                // `unpark` interrupts this wait, retaining a 1 Hz cadence
                // while making idle shutdown immediate.
                std::thread::park_timeout(Duration::from_secs(1));
            }
        }))
        .map_err(HostSourceSpawnError)?;
        Ok(Self {
            ring,
            stop,
            parked,
            worker: Some(worker),
        })
    }

    /// Build a `(time, value)` frame for `expr` from the ring, keeping only
    /// samples where the metric is present.
    fn frame_for(&self, expr: &str) -> Frame {
        let extract: fn(&StatsSnapshot) -> Option<f64> = match expr {
            "host_cpu_pct" => |s| s.cpu_pct.map(|v| v as f64),
            "host_mem_used" => |s| s.mem_gib.map(|(used, _total)| used as f64),
            "host_load1" => |s| s.load_avg.map(|(one, _, _)| one as f64),
            _ => |_| None,
        };
        let mut times = Vec::new();
        let mut values = Vec::new();
        if let Ok(r) = self.ring.lock() {
            for (ts, snap) in r.iter() {
                if let Some(v) = extract(snap) {
                    times.push(*ts);
                    values.push(v);
                }
            }
        }
        Frame::new(vec![
            Field::new("time", FieldType::Time, times),
            Field::new(expr, FieldType::Float64, values),
        ])
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
        let frames: Vec<Frame> = queries.iter().map(|q| self.frame_for(&q.expr)).collect();
        Box::pin(async move { Ok(frames) })
    }
}

fn now_secs() -> f64 {
    timestamp_secs(SystemTime::now())
}

fn timestamp_secs(now: SystemTime) -> f64 {
    now.duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;
    use gtui_core::datasource::TimeRange;

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
        let frame = source.frame_for("host_cpu_pct");
        assert_eq!(frame.fields[0].len(), 0);
        assert_eq!(frame.fields[1].len(), 0);
    }

    #[test]
    fn clock_before_epoch_uses_the_existing_zero_fallback() {
        let before_epoch = UNIX_EPOCH - Duration::from_secs(1);
        assert_eq!(timestamp_secs(before_epoch), 0.0);
    }

    #[test]
    fn a_parked_sampler_stops_sampling_and_resumes_on_activation() {
        let source = HostSource::try_new().unwrap();
        let len = || source.ring.lock().unwrap().len();
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while len() == 0 {
            assert!(std::time::Instant::now() < deadline, "no first sample");
            std::thread::sleep(Duration::from_millis(20));
        }
        source.set_active(false);
        // Let a sample already in flight land, then span more than one period.
        std::thread::sleep(Duration::from_millis(500));
        let parked_len = len();
        std::thread::sleep(Duration::from_millis(2300));
        assert_eq!(len(), parked_len, "parked sampler kept sampling");
        source.set_active(true);
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while len() == parked_len {
            assert!(std::time::Instant::now() < deadline, "did not resume");
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    #[test]
    fn dropping_a_parked_sampler_still_joins_promptly() {
        let source = HostSource::try_new().unwrap();
        source.set_active(false);
        std::thread::sleep(Duration::from_millis(300));
        let start = std::time::Instant::now();
        drop(source);
        assert!(start.elapsed() < Duration::from_millis(500));
    }
}
