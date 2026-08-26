//! Rolling telemetry rings: every stats snapshot the ticker delivers is
//! appended here, so the panel's Telemetry section and the system-monitor modal
//! have history the moment they open.
//!
//! # Raw in, normalized out
//!
//! Values are stored in **natural units** — percent, °C, bytes/sec, bytes — and
//! scaled only at read time. That is what makes the monitor's scale toggle
//! meaningful: normalizing on the way *in* (as this module used to) destroys
//! absolute magnitude, and re-scaling an already-normalized series is
//! arithmetic on nothing.
//!
//! # Absent is NaN, not zero
//!
//! A metric the platform does not expose records `f32::NAN`. Recording `0.0`
//! would draw a flat line at zero — a *wrong* reading rather than a missing
//! one, which is exactly how a Windows box with no thermal sensor used to
//! render a confident 0 °C. NaN propagates through
//! [`thegn_core::series::bucket_timed`] as a gap and lets the UI hide the row.
//!
//! # Timestamps
//!
//! One shared ring of unix-millisecond stamps rides alongside the values. The
//! cadence is not fixed — `[stats] refresh_secs` is user-cyclable and the UI
//! raises sampling to 500ms while a live surface is open — so samples are
//! non-uniformly spaced and a window expressed in *seconds* can only be resolved
//! against real timestamps. Bucketing by index instead would render eight
//! minutes of fast samples with the same x-extent as an hour of slow ones.

use std::collections::VecDeque;

use thegn_core::series::{self, Agg, Gap, Scale};
use thegn_core::viz::Unit;
use thegn_metrics::StatsSnapshot;

/// Wall-clock retention target: "all" in the UI means at most this much.
const RETAIN_SECS: u64 = 3600;
/// The fastest cadence the ticker can produce (the `stats_live` half-tick).
const MIN_INTERVAL_MS: u64 = 500;
/// Samples retained per series — enough for [`RETAIN_SECS`] at the fastest
/// cadence, which is four hours at the default 2s one.
///
/// Cost: [`Metric::ALL`] rings × `CAP` × 4 bytes, plus one shared timestamp ring
/// × 8 bytes ≈ 620 KiB, allocated once up front so a push never reallocates on
/// the ticker thread.
const CAP: usize = (RETAIN_SECS * 1000 / MIN_INTERVAL_MS) as usize;

/// Samples the [`TelemetryHistory::cadence_ms`] estimate looks back over.
#[allow(dead_code)] // used by `cadence_ms`
const CADENCE_WINDOW: usize = 20;

/// A fixed-capacity value ring.
///
/// The capacity is explicit rather than a module constant because the two
/// histories here want very different depths: the metric rings hold an hour,
/// while [`LoopPerfHistory`] only backs a small sparkline and has no business
/// retaining two hours of 1 Hz rollups.
#[derive(Debug, Clone)]
struct Ring {
    q: VecDeque<f32>,
    cap: usize,
}

impl Ring {
    fn new(cap: usize) -> Self {
        Ring {
            q: VecDeque::with_capacity(cap),
            cap,
        }
    }

    fn push(&mut self, v: f32) {
        if self.q.len() == self.cap {
            self.q.pop_front();
        }
        self.q.push_back(v);
    }

    /// The most recent value, or NaN when empty.
    fn last(&self) -> f32 {
        self.q.back().copied().unwrap_or(f32::NAN)
    }
}

/// One recorded series.
///
/// Ordering here is the ring index, so the array and the enum cannot fall out
/// of step; [`Metric::index`] is the only mapping.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Metric {
    Cpu,
    Mem,
    Swap,
    Gpu,
    Battery,
    Temp,
    Load,
    NetRx,
    NetTx,
    DiskIo,
    SelfRss,
    SelfCpu,
    DaemonRss,
    DaemonCpu,
    GpuMem,
    GpuPower,
    BatteryPower,
    CpuFreq,
    /// Free bytes on the worktrees filesystem. Recorded (never plotted on a
    /// tab) so the Disk tab's days-to-full projection and the `disk_eta` alert
    /// have a free-space trend to fit — see [`thegn_core::disk_fill`]. Appended
    /// last so the existing positional ring indices are undisturbed.
    DiskFree,
}

impl Metric {
    pub const ALL: [Metric; 19] = [
        Metric::Cpu,
        Metric::Mem,
        Metric::Swap,
        Metric::Gpu,
        Metric::Battery,
        Metric::Temp,
        Metric::Load,
        Metric::NetRx,
        Metric::NetTx,
        Metric::DiskIo,
        Metric::SelfRss,
        Metric::SelfCpu,
        Metric::DaemonRss,
        Metric::DaemonCpu,
        Metric::GpuMem,
        Metric::GpuPower,
        Metric::BatteryPower,
        Metric::CpuFreq,
        Metric::DiskFree,
    ];

    pub fn index(self) -> usize {
        self as usize
    }

    /// Short uppercase label for a graph header.
    pub fn label(self) -> &'static str {
        match self {
            Metric::Cpu => "CPU",
            Metric::Mem => "MEM",
            Metric::Swap => "SWAP",
            Metric::Gpu => "GPU",
            Metric::Battery => "BATTERY",
            Metric::Temp => "TEMP",
            Metric::Load => "LOAD",
            Metric::NetRx => "RX",
            Metric::NetTx => "TX",
            Metric::DiskIo => "DISK IO",
            Metric::SelfRss => "RSS",
            Metric::SelfCpu => "CPU",
            Metric::DaemonRss => "DAEMON RSS",
            Metric::DaemonCpu => "DAEMON CPU",
            Metric::GpuMem => "VRAM",
            Metric::GpuPower => "GPU POWER",
            Metric::BatteryPower => "DRAW",
            Metric::CpuFreq => "FREQ",
            Metric::DiskFree => "DISK FREE",
        }
    }

    pub fn unit(self) -> Unit {
        match self {
            Metric::Cpu
            | Metric::Mem
            | Metric::Swap
            | Metric::Gpu
            | Metric::Battery
            | Metric::SelfCpu
            | Metric::DaemonCpu => Unit::Percent,
            Metric::Temp => Unit::Celsius,
            Metric::Load => Unit::Ratio,
            Metric::NetRx | Metric::NetTx | Metric::DiskIo => Unit::BytesPerSec,
            Metric::SelfRss | Metric::DaemonRss | Metric::GpuMem | Metric::DiskFree => Unit::Bytes,
            Metric::GpuPower | Metric::BatteryPower => Unit::Watts,
            Metric::CpuFreq => Unit::Megahertz,
        }
    }

    /// The natural full scale, where one exists. `None` for an unbounded
    /// quantity (a rate, a load average, an RSS), which therefore cannot be
    /// drawn on a fixed axis and falls back to window-relative scaling.
    ///
    /// Per-process CPU is deliberately absent: it is a per-core sum and can
    /// legitimately exceed 100%.
    pub fn full_scale(self) -> Option<f32> {
        match self {
            Metric::Cpu | Metric::Mem | Metric::Swap | Metric::Gpu | Metric::Battery => Some(100.0),
            Metric::Temp => Some(100.0),
            _ => None,
        }
    }

    /// How to read a bucket no sample landed in.
    ///
    /// A **rate** reads zero — no samples means no traffic was observed. A
    /// **level** holds the previous value: a temperature does not fall to
    /// absolute zero because sampling paused.
    pub fn gap_policy(self) -> Gap {
        match self {
            Metric::NetRx | Metric::NetTx | Metric::DiskIo => Gap::Zero,
            _ => Gap::Hold,
        }
    }

    /// Value to record from one snapshot, or NaN when the platform does not
    /// expose it.
    fn read(self, s: &StatsSnapshot) -> f32 {
        fn or_nan<T>(v: Option<T>, f: impl Fn(T) -> f32) -> f32 {
            v.map(f).unwrap_or(f32::NAN)
        }
        match self {
            Metric::Cpu => or_nan(s.cpu_pct, |p| p as f32),
            Metric::Mem => or_nan(s.mem_gib.filter(|(_, t)| *t > 0.0), |(u, t)| u / t * 100.0),
            Metric::Swap => or_nan(s.swap_gib.filter(|(_, t)| *t > 0.0), |(u, t)| u / t * 100.0),
            Metric::Gpu => or_nan(s.gpu_pct, |p| p as f32),
            Metric::Battery => or_nan(s.battery, |(p, _)| p as f32),
            Metric::Temp => or_nan(s.cpu_temp_c, |c| c),
            Metric::Load => or_nan(s.load_avg, |(one, _, _)| one),
            Metric::NetRx => or_nan(s.net_bps, |(rx, _)| rx as f32),
            Metric::NetTx => or_nan(s.net_bps, |(_, tx)| tx as f32),
            // Absent only when no disk is enumerated at all; an idle disk is a
            // real zero, not a gap.
            Metric::DiskIo if s.disks.is_empty() => f32::NAN,
            Metric::DiskIo => s
                .disks
                .iter()
                .map(|d| d.read_bps + d.write_bps)
                .sum::<u64>() as f32,
            Metric::SelfRss => or_nan(s.self_rss_bytes, |b| b as f32),
            Metric::SelfCpu => or_nan(s.self_cpu_pct, |p| p),
            Metric::DaemonRss => or_nan(s.daemon_rss_bytes, |b| b as f32),
            Metric::DaemonCpu => or_nan(s.daemon_cpu_pct, |p| p),
            Metric::GpuMem => or_nan(s.gpu_mem_mib, |(u, _)| u as f32 * 1024.0 * 1024.0),
            Metric::GpuPower => or_nan(s.gpu_power_w, |w| w),
            Metric::BatteryPower => or_nan(s.battery_power_w, |w| w),
            Metric::CpuFreq => or_nan(s.cpu_freq_mhz, |f| f as f32),
            // Available bytes on the worktrees filesystem (the projection's y
            // axis). Absent off unix / on a statvfs error, exactly like the
            // disk widgets.
            Metric::DiskFree => or_nan(s.disk_bytes, |(_, avail)| avail as f32),
        }
    }
}

/// How much history a plot shows, and the ladder the `[`/`]` keys walk.
///
/// Both live in [`thegn_core::series_window`] — the span is a configurable
/// duration rather than a fixed enum, and the parser plus ladder arithmetic
/// belong where the coverage gate is. Re-exported here so every
/// `crate::telemetry::Window` call site keeps reading as it did.
pub use thegn_core::series_window::{Window, WindowLadder};

/// A request for one plottable series.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SeriesReq {
    pub metric: Metric,
    pub window: Window,
    pub scale: Scale,
    /// Dot columns to produce — `cols * 2` for a braille plot.
    pub buckets: usize,
    pub agg: Agg,
    /// Wall-clock "now" in unix milliseconds.
    ///
    /// The caller reads the clock **once per frame** and passes it in, rather
    /// than this anchoring on the newest sample: anchoring on the sample would
    /// make a slow-cadence plot lag by up to a full interval and would hide the
    /// empty right edge that tells you sampling has stalled.
    pub now_ms: u64,
}

/// A series reduced to plot columns, with everything needed to label it.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct SeriesOut {
    /// Upper edge, 0..=1, `buckets` long.
    pub hi: Vec<f32>,
    /// Lower edge, 0..=1. Equal to `hi` unless [`Agg::MinMax`].
    pub lo: Vec<f32>,
    /// `hi` in natural units, for axis labels and tooltips.
    pub raw_hi: Vec<f32>,
    /// True where no sample landed in that bucket.
    pub gap: Vec<bool>,
    /// Raw value at the top of the plot.
    pub axis_max: f32,
    /// Raw value at the bottom of the plot (always 0 today).
    pub axis_min: f32,
    /// Most recent raw value, or NaN when there is none.
    pub last: f32,
    /// Wall-clock seconds the plot actually covers — which is *not* the window
    /// span when history is shorter than the request. The UI prints it so a 1h
    /// window over three minutes of data reads "1h · 3m of history" instead of
    /// quietly implying an hour.
    pub covered_secs: f32,
    /// True when every sample in the window was absent, so the caller can hide
    /// the row rather than draw a convincing flat line at zero.
    pub empty: bool,
}

/// Rolling per-metric history, pushed on every stats drain in the loop.
///
/// Deliberately **not** `Clone`: at [`CAP`] this is ~620 KiB, and an accidental
/// clone on the render path would be a real cost. Borrow it.
#[derive(Debug)]
pub struct TelemetryHistory {
    /// Unix-millisecond stamps, one per push. Monotone by construction (a
    /// single producer pushing in order) and always the same length as every
    /// value ring, which is what lets [`Self::series`] zip them.
    at: VecDeque<u64>,
    rings: [Ring; Metric::ALL.len()],
    /// Bumped on every push. A memo key for callers that cache a [`SeriesOut`]:
    /// a static picture must not be re-bucketed on every frame.
    revision: u64,
    /// Latest registered-child rollup (language servers, plugin hosts,
    /// watchers). Not a `Metric` ring: the groups are open-ended and their
    /// count varies from zero to dozens, which is exactly what a fixed enum of
    /// scalar series cannot express. Only the newest reading is kept — the
    /// status modal lists it; nothing graphs it over time yet.
    children: Vec<thegn_metrics::ChildGroup>,
}

impl Default for TelemetryHistory {
    fn default() -> Self {
        TelemetryHistory {
            children: Vec::new(),
            at: VecDeque::with_capacity(CAP),
            rings: std::array::from_fn(|_| Ring::new(CAP)),
            revision: 0,
        }
    }
}

impl TelemetryHistory {
    /// Record one sample, stamped `at_ms` (unix milliseconds).
    pub fn push(&mut self, snap: &StatsSnapshot, at_ms: u64) {
        if self.at.len() == CAP {
            self.at.pop_front();
        }
        self.at.push_back(at_ms);
        for m in Metric::ALL {
            self.rings[m.index()].push(m.read(snap));
        }
        self.children.clone_from(&snap.children);
        self.revision = self.revision.wrapping_add(1);
    }

    /// The latest registered-child rollup, heaviest group first. Empty when the
    /// user runs no language servers, plugins or watchers — the ordinary case,
    /// not a failure.
    pub fn children(&self) -> &[thegn_metrics::ChildGroup] {
        &self.children
    }

    /// Resident bytes across every registered child.
    pub fn children_rss(&self) -> u64 {
        self.children
            .iter()
            .fold(0u64, |a, g| a.saturating_add(g.rss_bytes))
    }

    /// Memo key — changes only when a sample lands.
    ///
    /// Lets a caller cache a [`SeriesOut`] across frames: a static picture must
    /// not be re-bucketed at the frame rate when a 60 Hz dirty source (a mouse
    /// drag, key repeat) is running.
    #[allow(dead_code)] // render-cache key; see the module docs
    pub fn generation(&self) -> u64 {
        self.revision
    }

    /// Timestamp of the oldest retained sample.
    pub fn oldest_ms(&self) -> Option<u64> {
        self.at.front().copied()
    }

    /// Timestamp of the newest sample.
    #[allow(dead_code)] // staleness checks; paired with `oldest_ms`
    pub fn newest_ms(&self) -> Option<u64> {
        self.at.back().copied()
    }

    /// The most recent raw value for `m`, in natural units (NaN when absent).
    pub fn last_raw(&self, m: Metric) -> f32 {
        self.rings[m.index()].last()
    }

    /// Finite `(unix_ms, value)` samples for `m` at or after `cut_ms`, in
    /// chronological order — the raw series a trend fit needs (bucketing to plot
    /// columns would hide the real slope). Absent (`NaN`) samples are dropped, so
    /// a metric that only sometimes reports contributes just its real readings.
    pub fn raw_since(&self, m: Metric, cut_ms: u64) -> Vec<(u64, f32)> {
        let idx = self.start_index(cut_ms);
        let ring = &self.rings[m.index()];
        self.at
            .iter()
            .skip(idx)
            .zip(ring.q.iter().skip(idx))
            .filter(|(_, v)| v.is_finite())
            .map(|(&t, &v)| (t, v))
            .collect()
    }

    /// Project time-to-full for the worktrees filesystem from all retained
    /// free-space history. `None` unless the trend is a real, sustained decline
    /// over enough span — the honesty gates live in [`thegn_core::disk_fill`].
    ///
    /// Cheap `O(retained)` arithmetic over the [`Metric::DiskFree`] ring; safe to
    /// call per frame while the Disk tab is open and per sample for the alert.
    pub fn disk_fill_eta(&self) -> Option<thegn_core::disk_fill::DiskFillEta> {
        let pts: Vec<(f64, f64)> = self
            .raw_since(Metric::DiskFree, 0)
            .into_iter()
            .map(|(t, v)| (t as f64 / 1000.0, v as f64))
            .collect();
        thegn_core::disk_fill::project(&pts)
    }

    /// Median inter-sample gap over the last [`CADENCE_WINDOW`] samples.
    ///
    /// The cadence is not a constant — the config value is cyclable and the UI
    /// raises it while a live surface is open — so anything that needs to reason
    /// about sample spacing (is an empty bucket a real gap, or just a slow
    /// cadence?) has to measure it. Falls back to [`MIN_INTERVAL_MS`] before
    /// there are two samples to compare.
    #[allow(dead_code)] // gap-policy input; see `Metric::gap_policy`
    pub fn cadence_ms(&self) -> u64 {
        let n = self.at.len();
        if n < 2 {
            return MIN_INTERVAL_MS;
        }
        let start = n.saturating_sub(CADENCE_WINDOW + 1);
        let mut gaps: Vec<u64> = self
            .at
            .iter()
            .skip(start)
            .zip(self.at.iter().skip(start + 1))
            .map(|(a, b)| b.saturating_sub(*a))
            .collect();
        if gaps.is_empty() {
            return MIN_INTERVAL_MS;
        }
        gaps.sort_unstable();
        gaps[gaps.len() / 2].max(1)
    }

    /// Wall-clock seconds of history actually available inside `window` at
    /// `now_ms`, when that is **less** than the window asks for; `None` once
    /// history fills it.
    ///
    /// Lets the UI say "1h · 4m of history" instead of drawing an hour-wide
    /// axis over four minutes of data and implying the rest was flat.
    pub fn coverage_secs(&self, now_ms: u64, window: Window) -> Option<f32> {
        let oldest = self.oldest_ms()?;
        let have = now_ms.saturating_sub(oldest) as f32 / 1000.0;
        match window.secs() {
            // A few seconds of slack: a window is never exactly full, and
            // flagging that as a shortfall would make the note permanent.
            Some(w) if have + 2.0 < w as f32 => Some(have),
            Some(_) => None,
            // "All" is by definition fully covered.
            None => None,
        }
    }

    /// Index of the first sample at or after `cut_ms`. `O(log n)`.
    fn start_index(&self, cut_ms: u64) -> usize {
        // `partition_point` needs a slice; a VecDeque is two of them.
        let (a, b) = self.at.as_slices();
        match a.last() {
            Some(&t) if t < cut_ms => a.len() + b.partition_point(|&x| x < cut_ms),
            _ => a.partition_point(|&x| x < cut_ms),
        }
    }

    /// Reduce one metric to plot columns.
    pub fn series(&self, req: &SeriesReq) -> SeriesOut {
        let buckets = req.buckets;
        let empty_out = || SeriesOut {
            hi: vec![0.0; buckets],
            lo: vec![0.0; buckets],
            raw_hi: vec![f32::NAN; buckets],
            gap: vec![true; buckets],
            axis_max: req.metric.full_scale().unwrap_or(1.0),
            axis_min: 0.0,
            last: f32::NAN,
            covered_secs: 0.0,
            empty: true,
        };
        if buckets == 0 || self.at.is_empty() {
            return empty_out();
        }
        let t1 = req.now_ms;
        let span_ms = req.window.secs().map(|s| u64::from(s) * 1000);
        let t0 = match span_ms {
            Some(ms) => t1.saturating_sub(ms),
            // `All` spans from the oldest retained sample, capped at the
            // retention horizon so a paused-for-days session can't ask for a
            // window the ring never held.
            None => self
                .oldest_ms()
                .unwrap_or(t1)
                .max(t1.saturating_sub(RETAIN_SECS * 1000)),
        };
        if t1 <= t0 {
            return empty_out();
        }

        let idx = self.start_index(t0);
        let ring = &self.rings[req.metric.index()];
        // `at` and every ring are pushed together, so they are the same length
        // and this zip is aligned by construction.
        let it = self
            .at
            .iter()
            .skip(idx)
            .zip(ring.q.iter().skip(idx))
            .map(|(&t, &v)| (t, v));

        let mut b = series::bucket_timed(it, t0, t1, buckets, req.agg);
        let empty = b.iter().all(|x| x.is_none());
        let gap = series::fill_gaps(&mut b, req.metric.gap_policy());

        let raw_lo: Vec<f32> = b.iter().map(|x| x.map_or(f32::NAN, |(lo, _)| lo)).collect();
        let raw_hi: Vec<f32> = b.iter().map(|x| x.map_or(f32::NAN, |(_, hi)| hi)).collect();

        // Both edges must share one divisor, or the band would be skewed — so
        // normalize `hi` (which sets the axis) and reuse its scale for `lo`.
        let (hi, axis_max) = series::normalize(&raw_hi, req.scale);
        let (lo, _) = series::normalize(&raw_lo, Scale::Fixed(axis_max));

        // How much wall time the window really covers: the request, clipped to
        // the history that actually exists.
        let first = self.at.get(idx).copied().unwrap_or(t1);
        let covered_secs = (t1.saturating_sub(first.max(t0))) as f32 / 1000.0;

        SeriesOut {
            hi,
            lo,
            raw_hi,
            gap,
            axis_max,
            axis_min: 0.0,
            last: ring.last(),
            covered_secs,
            empty,
        }
    }

    /// The scale a metric should use under a UI scale mode, resolving
    /// [`Scale::Fixed`] against the metric's own full scale and falling back to
    /// window-relative for an unbounded quantity that has none.
    pub fn scale_for(m: Metric, mode: ScaleMode) -> Scale {
        match mode {
            ScaleMode::Window => Scale::Window,
            ScaleMode::Fixed => m.full_scale().map(Scale::Fixed).unwrap_or(Scale::Window),
            // A floor of 1.0 in the metric's own unit: one byte/sec, one watt,
            // one percent. Below that there is nothing to distinguish.
            ScaleMode::Log => Scale::Log { floor: 1.0 },
        }
    }

    // --- Legacy accessors -------------------------------------------------
    //
    // The panel's Telemetry section and the bar-item popups still read
    // right-aligned, pre-normalized windows. These keep that contract (0..=1,
    // front-padded with zeros, "now" at the right edge) on top of the raw
    // storage, so those call sites can migrate to `series()` one at a time.

    /// The last `n` values of `m`, right-aligned and front-padded, scaled by
    /// `denom` with NaN mapped to 0.0.
    fn legacy(&self, m: Metric, n: usize, denom: f32) -> Vec<f32> {
        let q = &self.rings[m.index()].q;
        let take = q.len().min(n);
        let mut out = vec![0.0; n - take];
        out.extend(q.iter().skip(q.len() - take).map(|v| {
            if v.is_finite() {
                (v / denom).clamp(0.0, 1.0)
            } else {
                0.0
            }
        }));
        out
    }

    /// The last `n` values of `m` normalized against the window's own max.
    fn legacy_norm(&self, m: Metric, n: usize) -> Vec<f32> {
        let q = &self.rings[m.index()].q;
        let take = q.len().min(n);
        let raw: Vec<f32> = q.iter().skip(q.len() - take).copied().collect();
        // Floor the divisor at 1.0 so an idle series reads flat, matching the
        // long-standing behavior of the accessors this replaced.
        let max = raw
            .iter()
            .copied()
            .filter(|v| v.is_finite())
            .fold(1.0_f32, f32::max);
        let mut out = vec![0.0; n - take];
        out.extend(raw.into_iter().map(|v| {
            if v.is_finite() {
                (v / max).clamp(0.0, 1.0)
            } else {
                0.0
            }
        }));
        out
    }

    /// CPU series (0..=1), right-aligned to `n` values.
    pub fn cpu_series(&self, n: usize) -> Vec<f32> {
        self.legacy(Metric::Cpu, n, 100.0)
    }

    /// Memory series (0..=1), right-aligned to `n` values.
    pub fn mem_series(&self, n: usize) -> Vec<f32> {
        self.legacy(Metric::Mem, n, 100.0)
    }

    /// Receive-rate series normalized by the window's rolling max.
    pub fn rx_series(&self, n: usize) -> Vec<f32> {
        self.legacy_norm(Metric::NetRx, n)
    }

    /// Transmit-rate series normalized by the window's rolling max.
    pub fn tx_series(&self, n: usize) -> Vec<f32> {
        self.legacy_norm(Metric::NetTx, n)
    }

    /// The latest raw (rx, tx) rates in bytes/s, for the NET headline.
    pub fn last_rates(&self) -> (u64, u64) {
        (
            finite_u64(self.last_raw(Metric::NetRx)),
            finite_u64(self.last_raw(Metric::NetTx)),
        )
    }

    /// Temperature series scaled to a fixed 0..=1 (0–100 °C), right-aligned.
    pub fn temp_series(&self, n: usize) -> Vec<f32> {
        self.legacy(Metric::Temp, n, 100.0)
    }

    /// Swap series (0..=1), right-aligned to `n` values.
    pub fn swap_series(&self, n: usize) -> Vec<f32> {
        self.legacy(Metric::Swap, n, 100.0)
    }

    /// Aggregate disk-IO series normalized by the window's rolling max.
    pub fn disk_io_series(&self, n: usize) -> Vec<f32> {
        self.legacy_norm(Metric::DiskIo, n)
    }

    /// Load-average series normalized by the window's rolling max.
    pub fn load_series(&self, n: usize) -> Vec<f32> {
        self.legacy_norm(Metric::Load, n)
    }

    /// Latest aggregate disk-IO rate in bytes/s, for the headline.
    pub fn last_disk_io(&self) -> u64 {
        finite_u64(self.last_raw(Metric::DiskIo))
    }

    /// GPU utilization series (0..=1, fixed scale), right-aligned to `n`.
    pub fn gpu_series(&self, n: usize) -> Vec<f32> {
        self.legacy(Metric::Gpu, n, 100.0)
    }

    /// Battery charge series (0..=1, fixed scale), right-aligned to `n`.
    pub fn battery_series(&self, n: usize) -> Vec<f32> {
        self.legacy(Metric::Battery, n, 100.0)
    }

    /// thegn's RSS series normalized by the window's rolling max, right-aligned.
    pub fn self_rss_series(&self, n: usize) -> Vec<f32> {
        self.legacy_norm(Metric::SelfRss, n)
    }

    /// The pane-daemon's RSS series normalized by the window's rolling max.
    pub fn daemon_rss_series(&self, n: usize) -> Vec<f32> {
        self.legacy_norm(Metric::DaemonRss, n)
    }

    /// The pane-daemon's CPU series normalized by the window's rolling max.
    pub fn daemon_cpu_series(&self, n: usize) -> Vec<f32> {
        self.legacy_norm(Metric::DaemonCpu, n)
    }

    /// Latest raw (thegn RSS bytes, thegn CPU %, daemon RSS bytes, daemon CPU %)
    /// for headlines.
    pub fn last_proc(&self) -> (u64, f32, u64, f32) {
        (
            finite_u64(self.last_raw(Metric::SelfRss)),
            finite_or_zero(self.last_raw(Metric::SelfCpu)),
            finite_u64(self.last_raw(Metric::DaemonRss)),
            finite_or_zero(self.last_raw(Metric::DaemonCpu)),
        )
    }
}

/// How the UI wants a series scaled; resolved per metric by
/// [`TelemetryHistory::scale_for`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ScaleMode {
    /// Relative to the visible window's own maximum.
    #[default]
    Window,
    /// Against the metric's natural full scale, where it has one.
    Fixed,
    /// Logarithmic — for rates spanning orders of magnitude.
    Log,
}

impl ScaleMode {
    pub const ALL: [ScaleMode; 3] = [ScaleMode::Window, ScaleMode::Fixed, ScaleMode::Log];

    pub fn label(self) -> &'static str {
        match self {
            ScaleMode::Window => "window",
            ScaleMode::Fixed => "fixed",
            ScaleMode::Log => "log",
        }
    }

    /// Stable persistence slug.
    pub fn key(self) -> &'static str {
        self.label()
    }

    pub fn from_key(s: &str) -> Option<ScaleMode> {
        ScaleMode::ALL.into_iter().find(|m| m.key() == s)
    }

    pub fn next(self) -> ScaleMode {
        let i = ScaleMode::ALL.iter().position(|m| *m == self).unwrap_or(0);
        ScaleMode::ALL[(i + 1) % ScaleMode::ALL.len()]
    }
}

fn finite_u64(v: f32) -> u64 {
    if v.is_finite() && v > 0.0 {
        v as u64
    } else {
        0
    }
}

fn finite_or_zero(v: f32) -> f32 {
    if v.is_finite() { v } else { 0.0 }
}

/// Samples retained by [`LoopPerfHistory`] — a small sparkline's worth. The
/// metric rings hold an hour; the 1 Hz loop rollup has no reason to, which is
/// why the two capacities are separate constants rather than one shared one.
const PERF_CAP: usize = 192;
const _: () = assert!(PERF_CAP < CAP, "the perf ring must stay the shallow one");

/// Rolling history of the event-loop self-profiler, fed by each `thegn::perf`
/// rollup. Powers the Telemetry section's "Loop" sub-block: how hard the loop is
/// working (wakes/s), how much it repaints (renders/s), and the tail render
/// latency — the live view of the same data the `thegn::perf` log emits.
#[derive(Debug, Clone)]
pub struct LoopPerfHistory {
    wakes: Ring,
    /// The most recent snapshot, for the headline.
    last: crate::perf::PerfSnapshot,
    any: bool,
}

impl Default for LoopPerfHistory {
    fn default() -> Self {
        LoopPerfHistory {
            wakes: Ring::new(PERF_CAP),
            last: crate::perf::PerfSnapshot::default(),
            any: false,
        }
    }
}

impl LoopPerfHistory {
    pub fn push(&mut self, snap: &crate::perf::PerfSnapshot) {
        self.wakes.push(snap.wakes_per_s as f32);
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
        let q = &self.wakes.q;
        let take = q.len().min(n);
        let raw: Vec<f32> = q.iter().skip(q.len() - take).copied().collect();
        let max = raw
            .iter()
            .copied()
            .filter(|v| v.is_finite())
            .fold(1.0_f32, f32::max);
        let mut out = vec![0.0; n - take];
        out.extend(raw.into_iter().map(|v| (v / max).clamp(0.0, 1.0)));
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SEC: u64 = 1000;

    fn snap(cpu: u8, used: f32, total: f32, rx: u64, tx: u64) -> StatsSnapshot {
        StatsSnapshot {
            cpu_pct: Some(cpu),
            mem_gib: Some((used, total)),
            net_bps: Some((rx, tx)),
            ..Default::default()
        }
    }

    /// Push `vals` as CPU percentages at a uniform `step_ms`, ending at `t_end`.
    fn history(vals: &[u8], step_ms: u64, t_end: u64) -> TelemetryHistory {
        let mut h = TelemetryHistory::default();
        let n = vals.len() as u64;
        for (i, &v) in vals.iter().enumerate() {
            let t = t_end - (n - 1 - i as u64) * step_ms;
            h.push(&snap(v, 1.0, 4.0, 0, 0), t);
        }
        h
    }

    fn req(m: Metric, w: Window, buckets: usize, now: u64) -> SeriesReq {
        SeriesReq {
            metric: m,
            window: w,
            scale: Scale::Window,
            buckets,
            agg: Agg::Max,
            now_ms: now,
        }
    }

    #[test]
    fn push_caps_each_series_at_capacity() {
        let mut h = TelemetryHistory::default();
        for i in 0..(CAP + 10) {
            h.push(
                &snap((i % 100) as u8, 1.0, 4.0, i as u64, 0),
                i as u64 * 500,
            );
        }
        assert_eq!(h.at.len(), CAP);
        assert_eq!(h.rings[Metric::NetRx.index()].q.len(), CAP);
        // The oldest 10 fell off: the front is sample #10.
        assert_eq!(
            h.rings[Metric::NetRx.index()].q.front().copied(),
            Some(10.0)
        );
        assert_eq!(h.oldest_ms(), Some(10 * 500));
        // Timestamps and values stay the same length — `series` zips them.
        for m in Metric::ALL {
            assert_eq!(h.rings[m.index()].q.len(), h.at.len(), "{m:?}");
        }
    }

    #[test]
    fn values_are_stored_raw_not_normalized() {
        // The whole point of the rewrite: absolute magnitude must survive to
        // the reader, or a fixed/log scale has nothing to work with.
        let mut h = TelemetryHistory::default();
        h.push(&snap(42, 2.0, 8.0, 5_000, 900), SEC);
        assert_eq!(h.last_raw(Metric::Cpu), 42.0);
        assert_eq!(h.last_raw(Metric::Mem), 25.0);
        assert_eq!(h.last_raw(Metric::NetRx), 5_000.0);
        assert_eq!(h.last_raw(Metric::NetTx), 900.0);
    }

    #[test]
    fn absent_metrics_record_nan_not_zero() {
        // A flat 0 °C on a machine with no sensor is a WRONG reading; NaN is a
        // missing one, and the caller can tell the difference.
        let mut h = TelemetryHistory::default();
        h.push(&StatsSnapshot::default(), SEC);
        for m in [
            Metric::Cpu,
            Metric::Temp,
            Metric::Load,
            Metric::Battery,
            Metric::Gpu,
            Metric::DiskIo,
        ] {
            assert!(h.last_raw(m).is_nan(), "{m:?} should be absent");
        }
        // ...and an all-absent window reports `empty`, so the UI can hide it.
        let out = h.series(&req(Metric::Temp, Window::from_secs(30), 8, 2 * SEC));
        assert!(out.empty);
        assert!(out.gap.iter().all(|g| *g));
    }

    #[test]
    fn an_idle_disk_is_a_real_zero_not_a_gap() {
        // Distinguishing "no disks enumerated" from "disks are idle" matters:
        // the first should hide the row, the second should draw a flat line.
        let mut h = TelemetryHistory::default();
        h.push(
            &StatsSnapshot {
                disks: vec![thegn_metrics::DiskInfo {
                    name: "sda".into(),
                    mount: "/".into(),
                    free_pct: 50,
                    read_bps: 0,
                    write_bps: 0,
                    kind: thegn_metrics::DiskKind::Ssd,
                }],
                ..Default::default()
            },
            SEC,
        );
        assert_eq!(h.last_raw(Metric::DiskIo), 0.0);
        assert!(
            !h.series(&req(Metric::DiskIo, Window::from_secs(30), 4, SEC + 1))
                .empty
        );
    }

    #[test]
    fn a_window_is_seconds_of_wall_clock_not_a_sample_count() {
        // 120 samples one second apart. A 30s window must show the last 30
        // seconds regardless of how many samples that is.
        let vals: Vec<u8> = (0..120).map(|i| i as u8).collect();
        let h = history(&vals, SEC, 120 * SEC);
        let short = h.series(&req(Metric::Cpu, Window::from_secs(30), 8, 120 * SEC));
        assert!((short.covered_secs - 30.0).abs() < 1.5, "{short:?}");
        let med = h.series(&req(Metric::Cpu, Window::from_secs(120), 8, 120 * SEC));
        assert!((med.covered_secs - 119.0).abs() < 2.0, "{med:?}");
        // Same wall-clock window at a FASTER cadence covers the same seconds —
        // this is what index bucketing would get wrong.
        let fast = history(&vals, 250, 120 * SEC);
        let s2 = fast.series(&req(Metric::Cpu, Window::from_secs(30), 8, 120 * SEC));
        assert!((s2.covered_secs - 30.0).abs() < 1.5, "{s2:?}");
    }

    #[test]
    fn covered_secs_reports_real_history_not_the_request() {
        // Three minutes requested, thirty seconds recorded: the UI must be able
        // to say so rather than implying an hour of data it never had.
        let vals: Vec<u8> = (0..30).map(|i| i as u8).collect();
        let h = history(&vals, SEC, 100 * SEC);
        let out = h.series(&req(Metric::Cpu, Window::from_secs(3600), 16, 100 * SEC));
        assert!(out.covered_secs <= 30.0, "{}", out.covered_secs);
        assert!(out.covered_secs > 28.0, "{}", out.covered_secs);
    }

    #[test]
    fn a_spike_survives_a_compressed_window() {
        // 600 samples, one spike, squeezed into 8 columns. Agg::Max must keep
        // it — this is the guarantee the whole aggregation choice rests on.
        let mut vals = vec![1u8; 600];
        vals[300] = 100;
        let h = history(&vals, SEC, 600 * SEC);
        let out = h.series(&SeriesReq {
            agg: Agg::Max,
            ..req(Metric::Cpu, Window::from_secs(600), 8, 600 * SEC)
        });
        assert!(out.raw_hi.contains(&100.0), "spike lost: {:?}", out.raw_hi);
    }

    #[test]
    fn minmax_produces_a_band_sharing_one_divisor() {
        let vals: Vec<u8> = (0..60).map(|i| if i % 2 == 0 { 10 } else { 90 }).collect();
        let h = history(&vals, SEC, 60 * SEC);
        let out = h.series(&SeriesReq {
            agg: Agg::MinMax,
            ..req(Metric::Cpu, Window::from_secs(120), 4, 60 * SEC)
        });
        // lo must sit under hi everywhere; a separately-normalized lo could
        // exceed it and invert the band.
        for (l, hh) in out.lo.iter().zip(out.hi.iter()) {
            assert!(l <= hh, "band inverted: {:?} {:?}", out.lo, out.hi);
        }
        assert!(out.hi.iter().any(|v| *v > 0.0));
    }

    #[test]
    fn fixed_scale_uses_the_metrics_full_scale_and_falls_back_when_absent() {
        assert_eq!(
            TelemetryHistory::scale_for(Metric::Cpu, ScaleMode::Fixed),
            Scale::Fixed(100.0)
        );
        // A rate has no natural full scale — pretending otherwise would pick an
        // arbitrary ceiling, so it falls back to window-relative.
        assert_eq!(
            TelemetryHistory::scale_for(Metric::NetRx, ScaleMode::Fixed),
            Scale::Window
        );
        // Per-core CPU sums can exceed 100%, so they have no fixed scale either.
        assert_eq!(
            TelemetryHistory::scale_for(Metric::SelfCpu, ScaleMode::Fixed),
            Scale::Window
        );
    }

    #[test]
    fn fixed_scale_keeps_a_quiet_metric_quiet() {
        // Window scaling amplifies noise to full height; fixed scaling doesn't.
        let h = history(&[3, 4, 3, 5], SEC, 4 * SEC);
        let win = h.series(&req(Metric::Cpu, Window::from_secs(30), 4, 4 * SEC));
        let fixed = h.series(&SeriesReq {
            scale: Scale::Fixed(100.0),
            ..req(Metric::Cpu, Window::from_secs(30), 4, 4 * SEC)
        });
        assert!(win.hi.iter().any(|v| *v > 0.9), "window scaling maxes out");
        assert!(
            fixed.hi.iter().all(|v| *v < 0.1),
            "fixed scaling stays low: {:?}",
            fixed.hi
        );
        assert_eq!(fixed.axis_max, 100.0);
    }

    #[test]
    fn gap_policy_differs_for_rates_and_levels() {
        // A rate reads zero across a gap (no traffic observed); a level holds
        // (the temperature didn't drop to absolute zero because we stopped
        // looking).
        assert_eq!(Metric::NetRx.gap_policy(), Gap::Zero);
        assert_eq!(Metric::Temp.gap_policy(), Gap::Hold);
        assert_eq!(Metric::Cpu.gap_policy(), Gap::Hold);
    }

    #[test]
    fn cadence_is_measured_not_assumed() {
        let h = history(&[1, 2, 3, 4, 5], 2000, 10 * SEC);
        assert_eq!(h.cadence_ms(), 2000);
        let fast = history(&[1, 2, 3, 4, 5], 500, 10 * SEC);
        assert_eq!(fast.cadence_ms(), 500);
        // Too little history to measure: fall back rather than divide by zero.
        assert_eq!(TelemetryHistory::default().cadence_ms(), MIN_INTERVAL_MS);
    }

    #[test]
    fn start_index_finds_the_window_edge_across_a_wrapped_ring() {
        // Overfill so the VecDeque is genuinely two slices, then confirm the
        // binary search still lands correctly — `partition_point` needs a
        // slice, and a naive version would search only the first one.
        let mut h = TelemetryHistory::default();
        for i in 0..(CAP + 500) {
            h.push(&snap(1, 1.0, 4.0, 0, 0), i as u64 * 500);
        }
        assert!(!h.at.as_slices().1.is_empty(), "ring should be wrapped");
        let cut = h.at[h.at.len() / 2];
        let idx = h.start_index(cut);
        assert_eq!(h.at[idx], cut);
        assert!(h.at[idx - 1] < cut);
        // Before everything / after everything.
        assert_eq!(h.start_index(0), 0);
        assert_eq!(h.start_index(u64::MAX), h.at.len());
    }

    #[test]
    fn degenerate_requests_never_panic() {
        let h = history(&[1, 2, 3], SEC, 3 * SEC);
        assert!(
            h.series(&req(Metric::Cpu, Window::from_secs(30), 0, 3 * SEC))
                .hi
                .is_empty()
        );
        // now_ms before all history.
        let out = h.series(&req(Metric::Cpu, Window::from_secs(30), 4, 0));
        assert!(out.empty);
        // Empty history.
        let e = TelemetryHistory::default();
        let out = e.series(&req(Metric::Cpu, Window::EVERYTHING, 4, SEC));
        assert!(out.empty && out.hi.len() == 4);
        assert!(out.last.is_nan());
    }

    #[test]
    fn window_spans_and_slugs_survive_the_move_to_core() {
        // Cycling and slug round-tripping now belong to
        // `thegn_core::series_window` (a configurable ladder, not a fixed enum)
        // and are tested there. What this file still owns is that the spans the
        // rest of the module reasons about are the ones it always used.
        assert_eq!(Window::from_secs(30).secs(), Some(30));
        assert_eq!(Window::from_secs(3600).secs(), Some(3600));
        assert_eq!(Window::EVERYTHING.secs(), None);
        assert_eq!(Window::from_key("10m"), Some(Window::from_secs(600)));
        assert_eq!(Window::from_key("nope"), None);
    }

    #[test]
    fn scale_mode_cycles_and_round_trips() {
        assert_eq!(ScaleMode::Window.next(), ScaleMode::Fixed);
        assert_eq!(ScaleMode::Log.next(), ScaleMode::Window);
        for m in ScaleMode::ALL {
            assert_eq!(ScaleMode::from_key(m.key()), Some(m));
        }
    }

    #[test]
    fn metric_indices_match_declaration_order() {
        // The ring array is indexed positionally, so a reordered enum with a
        // stale `index()` would silently swap two metrics' history.
        for (i, m) in Metric::ALL.iter().enumerate() {
            assert_eq!(m.index(), i, "{m:?}");
        }
    }

    #[test]
    fn disk_free_history_feeds_a_fill_projection() {
        // A worktrees filesystem losing 1 MiB/s from 8 GiB free, sampled once a
        // second for 20 minutes, must yield a downward projection through the
        // ring; a flat disk yields none. Free stays positive across the window.
        let total = 16u64 * 1024 * 1024 * 1024;
        let free0 = 8u64 * 1024 * 1024 * 1024;
        let rate = 1024u64 * 1024;
        let mut h = TelemetryHistory::default();
        for s in 0..=1200u64 {
            let snap = StatsSnapshot {
                disk_bytes: Some((total, free0 - rate * s)),
                ..Default::default()
            };
            h.push(&snap, s * 1000);
        }
        let eta = h.disk_fill_eta().expect("declining free space projects");
        assert!(
            (eta.bytes_per_sec - rate as f64).abs() < 4096.0,
            "{}",
            eta.bytes_per_sec
        );
        assert!(eta.hours > 0.0 && eta.hours.is_finite());

        // A stable disk: no projection.
        let mut flat = TelemetryHistory::default();
        for s in 0..=1200u64 {
            let snap = StatsSnapshot {
                disk_bytes: Some((total, free0)),
                ..Default::default()
            };
            flat.push(&snap, s * 1000);
        }
        assert!(flat.disk_fill_eta().is_none());
    }

    // --- Legacy accessor contract ----------------------------------------

    #[test]
    fn series_right_aligns_short_history() {
        let mut h = TelemetryHistory::default();
        h.push(&snap(50, 2.0, 4.0, 100, 200), SEC);
        h.push(&snap(100, 4.0, 4.0, 300, 400), 2 * SEC);
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
        h.push(&snap(0, 0.0, 0.0, 50, 0), SEC);
        h.push(&snap(0, 0.0, 0.0, 100, 0), 2 * SEC);
        let rx = h.rx_series(2);
        assert_eq!(rx, vec![0.5, 1.0]);
        // All-zero traffic stays flat (no divide-by-zero spike).
        let tx = h.tx_series(2);
        assert_eq!(tx, vec![0.0, 0.0]);
        assert_eq!(h.last_rates(), (100, 0));
    }

    #[test]
    fn proc_series_track_both_processes() {
        let mut h = TelemetryHistory::default();
        for (i, (rss, cpu, drss, dcpu)) in [(100u64, 5.0f32, 10u64, 1.0f32), (200, 10.0, 40, 4.0)]
            .into_iter()
            .enumerate()
        {
            h.push(
                &StatsSnapshot {
                    self_rss_bytes: Some(rss),
                    self_cpu_pct: Some(cpu),
                    daemon_rss_bytes: Some(drss),
                    daemon_cpu_pct: Some(dcpu),
                    ..Default::default()
                },
                (i as u64 + 1) * SEC,
            );
        }
        assert_eq!(h.daemon_cpu_series(2), vec![0.25, 1.0]);
        assert_eq!(h.daemon_rss_series(2), vec![0.25, 1.0]);
        // Short history front-pads with zeros ("now" sits at the right edge).
        assert_eq!(h.daemon_cpu_series(3), vec![0.0, 0.25, 1.0]);
        assert_eq!(h.last_proc(), (200, 10.0, 40, 4.0));
    }

    #[test]
    fn legacy_accessors_render_absent_metrics_as_zero() {
        // The legacy contract is 0..=1 with no NaN — the panel section draws
        // these directly and has no gap handling of its own.
        let mut h = TelemetryHistory::default();
        h.push(&StatsSnapshot::default(), SEC);
        assert_eq!(h.cpu_series(1), vec![0.0]);
        assert_eq!(h.mem_series(1), vec![0.0]);
        assert_eq!(h.gpu_series(1), vec![0.0]);
        assert_eq!(h.battery_series(1), vec![0.0]);
        assert_eq!(h.temp_series(1), vec![0.0]);
        assert_eq!(h.load_series(1), vec![0.0]);
        assert_eq!(h.last_rates(), (0, 0));
        assert_eq!(h.last_proc(), (0, 0.0, 0, 0.0));
        assert_eq!(h.last_disk_io(), 0);
        assert!(h.cpu_series(4).iter().all(|v| v.is_finite()));
    }

    #[test]
    fn gpu_and_battery_use_fixed_scale() {
        let mut h = TelemetryHistory::default();
        let mut s = StatsSnapshot {
            gpu_pct: Some(25),
            battery: Some((80, false)),
            ..Default::default()
        };
        h.push(&s, SEC);
        s.gpu_pct = Some(50);
        s.battery = Some((40, false));
        h.push(&s, 2 * SEC);
        // Fixed 0..=100 scale: 25%→0.25, 50%→0.5 (NOT window-normalized to 1.0).
        assert_eq!(h.gpu_series(2), vec![0.25, 0.5]);
        assert_eq!(h.battery_series(2), vec![0.8, 0.4]);
    }

    #[test]
    fn generation_bumps_only_on_push() {
        let mut h = TelemetryHistory::default();
        let g = h.generation();
        // Reads must not invalidate a caller's memo.
        let _ = h.cpu_series(4);
        let _ = h.series(&req(Metric::Cpu, Window::from_secs(30), 4, SEC));
        assert_eq!(h.generation(), g);
        h.push(&snap(1, 1.0, 4.0, 0, 0), SEC);
        assert_ne!(h.generation(), g);
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

    #[test]
    fn loop_perf_keeps_its_own_shallow_ring() {
        // The metric rings hold an hour; the 1 Hz loop rollup must not silently
        // inherit that depth just because it shares a push helper.
        let mut h = LoopPerfHistory::default();
        for i in 0..(PERF_CAP + 50) {
            h.push(&crate::perf::PerfSnapshot {
                wakes_per_s: i as f64,
                ..Default::default()
            });
        }
        assert_eq!(h.wakes.q.len(), PERF_CAP);
    }
}
