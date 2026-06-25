//! Rolling telemetry series for the panel's Telemetry section (8): every stats
//! snapshot the ticker delivers is appended here, so the section's braille
//! graphs have history the moment it opens. Raw rates are stored (bytes/s) and
//! normalized at render time against the visible window's rolling max.

use std::collections::VecDeque;

use crate::stats::StatsSnapshot;

/// Samples retained per series. The widest graph reads 2 values per braille
/// cell, so 192 covers a 96-cell layer with room to spare.
const CAP: usize = 192;

fn push_cap(q: &mut VecDeque<f32>, v: f32) {
    if q.len() == CAP {
        q.pop_front();
    }
    q.push_back(v);
}

/// The last `n` values right-aligned: graphs read left → right with "now" at
/// the right edge, so a short history is front-padded with zeros.
fn series(q: &VecDeque<f32>, n: usize) -> Vec<f32> {
    let take = q.len().min(n);
    let mut out = vec![0.0; n - take];
    out.extend(q.iter().skip(q.len() - take));
    out
}

/// Normalize a raw-rate window against its own max (≥1 so an idle link stays
/// flat at zero rather than dividing by nothing).
fn norm(vals: Vec<f32>) -> Vec<f32> {
    let max = vals.iter().copied().fold(1.0_f32, f32::max);
    vals.into_iter().map(|v| v / max).collect()
}

/// Rolling per-metric history, pushed on every stats drain in the loop.
#[derive(Debug, Clone, Default)]
pub struct TelemetryHistory {
    /// CPU utilization 0..=1.
    cpu: VecDeque<f32>,
    /// Memory used/total 0..=1.
    mem: VecDeque<f32>,
    /// Raw receive rate, bytes/s.
    rx: VecDeque<f32>,
    /// Raw transmit rate, bytes/s.
    tx: VecDeque<f32>,
}

impl TelemetryHistory {
    pub fn push(&mut self, snap: &StatsSnapshot) {
        push_cap(
            &mut self.cpu,
            snap.cpu_pct.map(|p| p as f32 / 100.0).unwrap_or(0.0),
        );
        push_cap(
            &mut self.mem,
            snap.mem_gib
                .filter(|(_, t)| *t > 0.0)
                .map(|(u, t)| u / t)
                .unwrap_or(0.0),
        );
        let (rx, tx) = snap.net_bps.unwrap_or((0, 0));
        push_cap(&mut self.rx, rx as f32);
        push_cap(&mut self.tx, tx as f32);
    }

    /// CPU series (0..=1), right-aligned to `n` values.
    pub fn cpu_series(&self, n: usize) -> Vec<f32> {
        series(&self.cpu, n)
    }

    /// Memory series (0..=1), right-aligned to `n` values.
    pub fn mem_series(&self, n: usize) -> Vec<f32> {
        series(&self.mem, n)
    }

    /// Receive-rate series normalized by the window's rolling max.
    pub fn rx_series(&self, n: usize) -> Vec<f32> {
        norm(series(&self.rx, n))
    }

    /// Transmit-rate series normalized by the window's rolling max.
    pub fn tx_series(&self, n: usize) -> Vec<f32> {
        norm(series(&self.tx, n))
    }

    /// The latest raw (rx, tx) rates in bytes/s, for the NET headline.
    pub fn last_rates(&self) -> (u64, u64) {
        (
            self.rx.back().copied().unwrap_or(0.0) as u64,
            self.tx.back().copied().unwrap_or(0.0) as u64,
        )
    }
}

/// Rolling history of the event-loop self-profiler, fed by each `szhost::perf`
/// rollup. Powers the Telemetry section's "Loop" sub-block: how hard the loop is
/// working (wakes/s), how much it repaints (renders/s), and the tail render
/// latency — the live view of the same data the `szhost::perf` log emits.
#[derive(Debug, Clone, Default)]
pub struct LoopPerfHistory {
    wakes: VecDeque<f32>,
    renders: VecDeque<f32>,
    render_p99_us: VecDeque<f32>,
    /// The most recent snapshot, for the headline.
    last: crate::perf::PerfSnapshot,
    any: bool,
}

impl LoopPerfHistory {
    pub fn push(&mut self, snap: &crate::perf::PerfSnapshot) {
        push_cap(&mut self.wakes, snap.wakes_per_s as f32);
        push_cap(&mut self.renders, snap.renders_per_s as f32);
        push_cap(&mut self.render_p99_us, snap.render_p99_us as f32);
        self.last = snap.clone();
        self.any = true;
    }

    /// True once at least one rollup has landed (else the sub-block shows a hint).
    pub fn has_data(&self) -> bool {
        self.any
    }

    /// The most recent snapshot (for the headline line).
    pub fn last(&self) -> &crate::perf::PerfSnapshot {
        &self.last
    }

    /// Wakes/s series normalized by the window max.
    pub fn wakes_series(&self, n: usize) -> Vec<f32> {
        norm(series(&self.wakes, n))
    }

    /// Renders/s series normalized by the window max.
    #[allow(dead_code)] // sparkline series for the Telemetry overlay
    pub fn renders_series(&self, n: usize) -> Vec<f32> {
        norm(series(&self.renders, n))
    }

    /// Render p99 (µs) series normalized by the window max.
    #[allow(dead_code)] // sparkline series for the Telemetry overlay
    pub fn render_p99_series(&self, n: usize) -> Vec<f32> {
        norm(series(&self.render_p99_us, n))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snap(cpu: u8, used: f32, total: f32, rx: u64, tx: u64) -> StatsSnapshot {
        StatsSnapshot {
            cpu_pct: Some(cpu),
            mem_gib: Some((used, total)),
            net_bps: Some((rx, tx)),
            ..Default::default()
        }
    }

    #[test]
    fn push_caps_each_series_at_capacity() {
        let mut h = TelemetryHistory::default();
        for i in 0..(CAP + 10) {
            h.push(&snap((i % 100) as u8, 1.0, 4.0, i as u64, 0));
        }
        assert_eq!(h.cpu.len(), CAP);
        assert_eq!(h.rx.len(), CAP);
        // The oldest 10 fell off: the front is sample #10.
        assert_eq!(h.rx.front().copied(), Some(10.0));
    }

    #[test]
    fn series_right_aligns_short_history() {
        let mut h = TelemetryHistory::default();
        h.push(&snap(50, 2.0, 4.0, 100, 200));
        h.push(&snap(100, 4.0, 4.0, 300, 400));
        let s = h.cpu_series(4);
        assert_eq!(s, vec![0.0, 0.0, 0.5, 1.0]);
        let m = h.mem_series(3);
        assert_eq!(m, vec![0.0, 0.5, 1.0]);
        // A window narrower than history keeps the most recent values.
        assert_eq!(h.cpu_series(1), vec![1.0]);
    }

    #[test]
    fn rate_series_normalize_against_window_max() {
        let mut h = TelemetryHistory::default();
        h.push(&snap(0, 0.0, 0.0, 50, 0));
        h.push(&snap(0, 0.0, 0.0, 100, 0));
        let rx = h.rx_series(2);
        assert_eq!(rx, vec![0.5, 1.0]);
        // All-zero traffic stays flat (no divide-by-zero spike).
        let tx = h.tx_series(2);
        assert_eq!(tx, vec![0.0, 0.0]);
        assert_eq!(h.last_rates(), (100, 0));
    }

    #[test]
    fn absent_fields_record_zero() {
        let mut h = TelemetryHistory::default();
        h.push(&StatsSnapshot::default());
        assert_eq!(h.cpu_series(1), vec![0.0]);
        assert_eq!(h.mem_series(1), vec![0.0]);
        assert_eq!(h.last_rates(), (0, 0));
    }

    #[test]
    fn loop_perf_history_tracks_snapshots() {
        let mut h = LoopPerfHistory::default();
        assert!(!h.has_data());
        h.push(&crate::perf::PerfSnapshot {
            wakes_per_s: 5.0,
            renders_per_s: 4.0,
            render_p99_us: 800,
            hot_source: "Model",
            ..Default::default()
        });
        h.push(&crate::perf::PerfSnapshot {
            wakes_per_s: 10.0,
            renders_per_s: 8.0,
            render_p99_us: 1600,
            hot_source: "Stats",
            ..Default::default()
        });
        assert!(h.has_data());
        assert_eq!(h.last().hot_source, "Stats");
        // Normalized against the window max (the second sample).
        assert_eq!(h.wakes_series(2), vec![0.5, 1.0]);
    }
}
