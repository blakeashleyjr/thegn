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
}

impl HostSource {
    pub fn new() -> Self {
        let ring: Ring = Arc::new(Mutex::new(VecDeque::with_capacity(RING_CAP)));
        let stop = Arc::new(AtomicBool::new(false));
        let ring_bg = ring.clone();
        let stop_bg = stop.clone();
        // Dedicated sampler thread: `StatsSampler::sample()` blocks (refreshes
        // sysinfo) and needs a warm-up read to prime the CPU delta, so it lives
        // off the UI thread and off the tokio runtime entirely.
        std::thread::Builder::new()
            .name("gtui-host-metrics".into())
            .spawn(move || {
                let disk_path = std::env::current_dir().unwrap_or_else(|_| "/".into());
                let mut sampler = StatsSampler::new(disk_path);
                while !stop_bg.load(Ordering::Relaxed) {
                    let snap = sampler.sample();
                    let ts = now_secs();
                    if let Ok(mut r) = ring_bg.lock() {
                        r.push_back((ts, snap));
                        while r.len() > RING_CAP {
                            r.pop_front();
                        }
                    }
                    std::thread::sleep(Duration::from_secs(1));
                }
            })
            .expect("spawn host-metrics sampler thread");
        Self { ring, stop }
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

impl Default for HostSource {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for HostSource {
    fn drop(&mut self) {
        // Stop the sampler thread when the source (and thus the tab) goes away.
        self.stop.store(true, Ordering::Relaxed);
    }
}

impl DataSource for HostSource {
    fn query(
        &self,
        queries: Vec<Query>,
    ) -> Pin<Box<dyn Future<Output = Result<Vec<Frame>, QueryError>> + Send>> {
        let frames: Vec<Frame> = queries.iter().map(|q| self.frame_for(&q.expr)).collect();
        Box::pin(async move { Ok(frames) })
    }
}

fn now_secs() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
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
        let source = HostSource::new();
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
        let source = HostSource::new();
        let res = source.query(vec![q("nope")]).await.unwrap();
        assert_eq!(res[0].fields[1].len(), 0);
    }
}
