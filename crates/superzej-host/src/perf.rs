//! Runtime performance self-profiler (`szhost::perf`).
//!
//! Attributes the event loop's wakeups, renders, and per-subsystem CPU so a
//! perf regression is one log line away instead of an afternoon of `/proc`
//! spelunking. The whole thing is gated behind a single process-global atomic
//! ([`enabled`]): when off, every hook collapses to one relaxed load + a
//! predictable branch, so leaving the instrumentation compiled in is free —
//! the same property the tracing subscriber has when `SUPERZEJ_LOG` is unset.
//!
//! Three pieces live here:
//!   * [`LoopPerf`] — a loop-owned (single-threaded, lock-free) tally of wakes,
//!     renders, render-skips, per-source drain counts, and a render-latency
//!     histogram. Threaded through `run::event_loop`.
//!   * [`CpuLedger`] / [`measure`] — per-subsystem thread-CPU accounting for the
//!     off-thread producers (hydration, stats, metrics, …), summed across the
//!     `spawn_blocking` pool via atomics.
//!   * [`thread_cpu_ns`] — `CLOCK_THREAD_CPUTIME_ID` wrapper (0 where unsupported).

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

/// Master switch. One relaxed load gates every hot-path hook.
static PERF_ON: AtomicBool = AtomicBool::new(false);

/// Set once at startup when the bench-window stop thread should fire (see
/// [`request_stop_after`]); reused by the event loop's existing `shutdown`
/// flag, so nothing here is checked on the hot path.
static STOP_REQUESTED: AtomicBool = AtomicBool::new(false);

/// True when perf accounting is live. Inlined so the off-case is a single load.
#[inline(always)]
pub fn enabled() -> bool {
    PERF_ON.load(Ordering::Relaxed)
}

/// Flip accounting on/off (startup [`init`], or the live Telemetry overlay).
pub fn set_enabled(on: bool) {
    PERF_ON.store(on, Ordering::Relaxed);
}

/// Enable accounting when `SUPERZEJ_PERF=1`/`true` or `SUPERZEJ_LOG` selects the
/// `szhost::perf` target. Called from `run::main` right after `log::init` so the
/// state is set before the loop spins up. Cheap and idempotent.
pub fn init() {
    let on = match std::env::var("SUPERZEJ_PERF") {
        Ok(v) => {
            let v = v.trim();
            v == "1" || v.eq_ignore_ascii_case("true")
        }
        Err(_) => false,
    } || std::env::var("SUPERZEJ_LOG")
        .map(|s| s.contains("szhost::perf"))
        .unwrap_or(false);
    set_enabled(on);
}

/// The rollup cadence (`SUPERZEJ_PERF_INTERVAL_MS`, default 10s). The loop emits
/// a `szhost::perf` summary at most this often, piggy-backing on an existing
/// wake — never its own timer thread (that would add a wake source).
pub fn report_interval() -> Duration {
    let ms = std::env::var("SUPERZEJ_PERF_INTERVAL_MS")
        .ok()
        .and_then(|s| s.parse::<u64>().ok())
        .filter(|&v| v >= 250)
        .unwrap_or(10_000);
    Duration::from_millis(ms)
}

/// Wakes/sec above which, while the loop is otherwise idle, we shout a wake
/// storm warning (`SUPERZEJ_PERF_WAKE_LIMIT`, default 20).
pub fn wake_storm_limit() -> f64 {
    std::env::var("SUPERZEJ_PERF_WAKE_LIMIT")
        .ok()
        .and_then(|s| s.parse::<f64>().ok())
        .filter(|v| *v > 0.0)
        .unwrap_or(20.0)
}

// ---------------------------------------------------------------------------
// Bench window (SUPERZEJ_BENCH_RUN_MS): run the full loop for a fixed window
// then exit, so the idle-CPU harness can measure steady state. Honors the
// no-timeout invariant by reusing the loop's `shutdown` flag — a one-shot
// thread sleeps, sets the flag, and pulses the waker exactly once.
// ---------------------------------------------------------------------------

/// If `SUPERZEJ_BENCH_RUN_MS` is set, spawn a one-shot thread that, after the
/// window elapses, sets `shutdown` and pulses `waker` a single time. Returns
/// whether a window was armed (for a startup log line). No-op otherwise.
pub fn request_stop_after(
    shutdown: std::sync::Arc<AtomicBool>,
    waker: termwiz::terminal::TerminalWaker,
) -> Option<u64> {
    let ms = std::env::var("SUPERZEJ_BENCH_RUN_MS")
        .ok()
        .and_then(|s| s.parse::<u64>().ok())
        .filter(|&v| v > 0)?;
    STOP_REQUESTED.store(true, Ordering::Relaxed);
    std::thread::Builder::new()
        .name("szhost-bench-window".into())
        .spawn(move || {
            std::thread::sleep(Duration::from_millis(ms));
            shutdown.store(true, Ordering::Relaxed);
            let _ = waker.wake();
        })
        .ok();
    Some(ms)
}

// ---------------------------------------------------------------------------
// Thread CPU clock.
// ---------------------------------------------------------------------------

/// Calling thread's consumed CPU time in nanoseconds, or `0` where the clock is
/// unavailable. Uses `CLOCK_THREAD_CPUTIME_ID` so an I/O-blocked producer (a
/// gix fan-out waiting on the filesystem) reports the CPU it actually burned,
/// not the wall-clock it spent blocked.
#[cfg(unix)]
#[inline]
pub fn thread_cpu_ns() -> u64 {
    let mut ts = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // SAFETY: `ts` is a valid, fully-owned timespec for the duration of the call.
    let rc = unsafe { libc::clock_gettime(libc::CLOCK_THREAD_CPUTIME_ID, &mut ts) };
    if rc == 0 {
        (ts.tv_sec as u64)
            .wrapping_mul(1_000_000_000)
            .wrapping_add(ts.tv_nsec as u64)
    } else {
        0
    }
}

#[cfg(not(unix))]
#[inline]
pub fn thread_cpu_ns() -> u64 {
    0
}

// ---------------------------------------------------------------------------
// Per-subsystem CPU ledger.
// ---------------------------------------------------------------------------

/// Off-thread producers whose CPU we attribute. Order matches [`CpuLedger`]'s
/// arrays; keep [`Subsys::ALL`] in sync.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(usize)]
pub enum Subsys {
    Hydrate = 0,
    Pr,
    Issues,
    Stats,
    Container,
    Metrics,
    Lsp,
    Sandbox,
    Diff,
}

impl Subsys {
    pub const ALL: [Subsys; 9] = [
        Subsys::Hydrate,
        Subsys::Pr,
        Subsys::Issues,
        Subsys::Stats,
        Subsys::Container,
        Subsys::Metrics,
        Subsys::Lsp,
        Subsys::Sandbox,
        Subsys::Diff,
    ];
    pub const N: usize = Self::ALL.len();

    #[allow(dead_code)] // subsystem label, used by the Telemetry overlay's CPU breakdown
    pub fn label(self) -> &'static str {
        match self {
            Subsys::Hydrate => "hydrate",
            Subsys::Pr => "pr",
            Subsys::Issues => "issues",
            Subsys::Stats => "stats",
            Subsys::Container => "container",
            Subsys::Metrics => "metrics",
            Subsys::Lsp => "lsp",
            Subsys::Sandbox => "sandbox",
            Subsys::Diff => "diff",
        }
    }
}

/// Global accumulator of CPU-ns and invocation counts per [`Subsys`]. Producers
/// run on many threads (the `spawn_blocking` pool), so the counters are atomic
/// and additive; the loop drains them with [`CpuLedger::take`] per rollup.
pub struct CpuLedger {
    ns: [AtomicU64; Subsys::N],
    calls: [AtomicU64; Subsys::N],
}

impl CpuLedger {
    const fn new() -> Self {
        // `AtomicU64::new(0)` isn't `Copy`, so the array can't be built with
        // `[AtomicU64::new(0); N]`; spell out the (small, fixed) list.
        #[allow(clippy::declare_interior_mutable_const)]
        const Z: AtomicU64 = AtomicU64::new(0);
        CpuLedger {
            ns: [Z; Subsys::N],
            calls: [Z; Subsys::N],
        }
    }

    fn add(&self, s: Subsys, ns: u64) {
        let i = s as usize;
        self.ns[i].fetch_add(ns, Ordering::Relaxed);
        self.calls[i].fetch_add(1, Ordering::Relaxed);
    }

    /// Read-and-reset every counter, returning `(cpu_ns, calls)` per subsystem
    /// in [`Subsys::ALL`] order. Called once per rollup interval.
    pub fn take(&self) -> [(u64, u64); Subsys::N] {
        let mut out = [(0u64, 0u64); Subsys::N];
        for (i, slot) in out.iter_mut().enumerate() {
            *slot = (
                self.ns[i].swap(0, Ordering::Relaxed),
                self.calls[i].swap(0, Ordering::Relaxed),
            );
        }
        out
    }
}

/// The process-global ledger. Producers `fetch_add` into it via [`measure`].
pub static CPU: CpuLedger = CpuLedger::new();

/// RAII span charging the calling thread's CPU delta to `sub` on drop. Returns
/// `None` when accounting is off, so the call site pays nothing:
/// `let _g = perf::measure(Subsys::Hydrate);`
#[inline]
pub fn measure(sub: Subsys) -> Option<CpuGuard> {
    if enabled() {
        Some(CpuGuard {
            sub,
            t0: thread_cpu_ns(),
        })
    } else {
        None
    }
}

pub struct CpuGuard {
    sub: Subsys,
    t0: u64,
}

impl Drop for CpuGuard {
    fn drop(&mut self) {
        let dt = thread_cpu_ns().saturating_sub(self.t0);
        CPU.add(self.sub, dt);
    }
}

// ---------------------------------------------------------------------------
// Latency histogram.
// ---------------------------------------------------------------------------

/// A tiny power-of-two bucket histogram: `bucket[k]` counts samples in
/// `[2^k, 2^(k+1))` microseconds. O(1) record, no allocation, no sorting —
/// good enough for p50/p99 of render latency. 32 buckets cover up to ~4s.
#[derive(Clone)]
pub struct Histo {
    buckets: [u64; Self::N],
    count: u64,
}

impl Histo {
    const N: usize = 32;

    pub fn new() -> Self {
        Histo {
            buckets: [0; Self::N],
            count: 0,
        }
    }

    /// Record a microsecond sample.
    #[inline]
    pub fn record_us(&mut self, us: u64) {
        let k = if us == 0 {
            0
        } else {
            // floor(log2(us)) clamped to the last bucket.
            (63 - us.leading_zeros() as usize).min(Self::N - 1)
        };
        self.buckets[k] += 1;
        self.count += 1;
    }

    #[allow(dead_code)] // histogram helper, used by the Telemetry overlay
    pub fn is_empty(&self) -> bool {
        self.count == 0
    }

    /// Approximate percentile (0.0..=1.0) in microseconds. Returns the lower
    /// edge of the bucket the rank falls into — coarse but allocation-free.
    pub fn percentile_us(&self, p: f64) -> u64 {
        if self.count == 0 {
            return 0;
        }
        let target = (p.clamp(0.0, 1.0) * self.count as f64).ceil() as u64;
        let mut seen = 0u64;
        for (k, &c) in self.buckets.iter().enumerate() {
            seen += c;
            if seen >= target {
                return if k == 0 { 0 } else { 1u64 << k };
            }
        }
        1u64 << (Self::N - 1)
    }

    pub fn reset(&mut self) {
        self.buckets = [0; Self::N];
        self.count = 0;
    }
}

impl Default for Histo {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------------------
// Wake-source attribution + loop self-profiler.
// ---------------------------------------------------------------------------

/// Every distinct producer that drives the event loop. A wake is attributed by
/// which channel-drain block produced messages, not by the (reasonless) waker.
/// Keep [`WakeSource::ALL`] in sync.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(usize)]
pub enum WakeSource {
    Pty = 0,
    Model,
    Pr,
    Issues,
    Stats,
    Container,
    Metrics,
    App,
    Docs,
    Lsp,
    GitOp,
    GitDoc,
    Hunk,
    Prefetch,
    FilePreview,
    Spec,
    Create,
    Watcher,
    Config,
    Refresh,
    Outline,
    Hover,
    Input,
    Other,
}

impl WakeSource {
    pub const ALL: [WakeSource; 24] = [
        WakeSource::Pty,
        WakeSource::Model,
        WakeSource::Pr,
        WakeSource::Issues,
        WakeSource::Stats,
        WakeSource::Container,
        WakeSource::Metrics,
        WakeSource::App,
        WakeSource::Docs,
        WakeSource::Lsp,
        WakeSource::GitOp,
        WakeSource::GitDoc,
        WakeSource::Hunk,
        WakeSource::Prefetch,
        WakeSource::FilePreview,
        WakeSource::Spec,
        WakeSource::Create,
        WakeSource::Watcher,
        WakeSource::Config,
        WakeSource::Refresh,
        WakeSource::Outline,
        WakeSource::Hover,
        WakeSource::Input,
        WakeSource::Other,
    ];
    pub const N: usize = Self::ALL.len();

    pub fn label(self) -> &'static str {
        match self {
            WakeSource::Pty => "Pty",
            WakeSource::Model => "Model",
            WakeSource::Pr => "Pr",
            WakeSource::Issues => "Issues",
            WakeSource::Stats => "Stats",
            WakeSource::Container => "Container",
            WakeSource::Metrics => "Metrics",
            WakeSource::App => "App",
            WakeSource::Docs => "Docs",
            WakeSource::Lsp => "Lsp",
            WakeSource::GitOp => "GitOp",
            WakeSource::GitDoc => "GitDoc",
            WakeSource::Hunk => "Hunk",
            WakeSource::Prefetch => "Prefetch",
            WakeSource::FilePreview => "FilePreview",
            WakeSource::Spec => "Spec",
            WakeSource::Create => "Create",
            WakeSource::Watcher => "Watcher",
            WakeSource::Config => "Config",
            WakeSource::Refresh => "Refresh",
            WakeSource::Outline => "Outline",
            WakeSource::Hover => "Hover",
            WakeSource::Input => "Input",
            WakeSource::Other => "Other",
        }
    }
}

/// Loop-owned tally. Single-threaded (lives on the event-loop thread), so plain
/// fields — no atomics, no locks on the hot path. Every mutator short-circuits
/// when accounting is off. Reset each rollup interval by [`LoopPerf::take`].
pub struct LoopPerf {
    drain_items: [u64; WakeSource::N],
    pub wakes: u64,
    pub renders: u64,
    pub render_skips: u64,
    pub render_us: Histo,
    pub pty_chunks: u64,
    pub pty_budget_hits: u64,
    /// Wall-clock the loop spent awake (not blocked in `poll_input`) this
    /// interval — drives the idle ratio.
    pub busy: Duration,
    report_t0: Instant,
}

impl LoopPerf {
    pub fn new() -> Self {
        LoopPerf {
            drain_items: [0; WakeSource::N],
            wakes: 0,
            renders: 0,
            render_skips: 0,
            render_us: Histo::new(),
            pty_chunks: 0,
            pty_budget_hits: 0,
            busy: Duration::ZERO,
            report_t0: Instant::now(),
        }
    }

    /// One wakeup observed (loop returned from `poll_input`).
    #[inline]
    pub fn wake(&mut self) {
        if enabled() {
            self.wakes += 1;
        }
    }

    /// One message drained from `src`'s channel. Called per-message at each
    /// drain site (the first statement in the loop body), so attribution needs
    /// just a single inserted line and no trailing bookkeeping.
    #[inline]
    pub fn tick(&mut self, src: WakeSource) {
        if enabled() {
            self.drain_items[src as usize] += 1;
        }
    }

    /// Add `n` messages to `src` at once (used by the budgeted PTY drain, which
    /// already counts its own chunks).
    #[inline]
    pub fn tick_n(&mut self, src: WakeSource, n: u64) {
        if n > 0 && enabled() {
            self.drain_items[src as usize] += n;
        }
    }

    /// PTY drain stats for the iteration (chunks consumed, whether the 64-chunk
    /// budget was hit). Also counts as `chunks` `Pty` drains.
    #[inline]
    pub fn pty(&mut self, chunks: u64, budget_hit: bool) {
        if enabled() {
            self.pty_chunks += chunks;
            if budget_hit {
                self.pty_budget_hits += 1;
            }
            self.tick_n(WakeSource::Pty, chunks);
        }
    }

    /// A frame was composed + flushed in `dt`.
    #[inline]
    pub fn render(&mut self, dt: Duration) {
        if enabled() {
            self.renders += 1;
            self.render_us.record_us(dt.as_micros() as u64);
        }
    }

    /// Woke but nothing was dirty — a wasted wakeup.
    #[inline]
    pub fn render_skip(&mut self) {
        if enabled() {
            self.render_skips += 1;
        }
    }

    /// Add `dt` to the busy-time accumulator (time awake handling a wake).
    #[inline]
    pub fn add_busy(&mut self, dt: Duration) {
        if enabled() {
            self.busy += dt;
        }
    }

    /// Has the rollup interval elapsed? (Cheap; the loop checks this on wake.)
    pub fn due(&self, interval: Duration) -> bool {
        self.report_t0.elapsed() >= interval
    }

    /// The dominant non-Pty wake source this interval (by message count), for
    /// the rollup headline and the wake-storm warning.
    pub fn hot_source(&self) -> WakeSource {
        let mut best = WakeSource::Other;
        let mut best_n = 0u64;
        for &s in &WakeSource::ALL {
            // PTY output is foreground/user-driven; the headline is about which
            // *background* producer dominates (and might be storming).
            if s == WakeSource::Pty {
                continue;
            }
            let n = self.drain_items[s as usize];
            if n > best_n {
                best_n = n;
                best = s;
            }
        }
        best
    }

    /// Per-source message counts in [`WakeSource::ALL`] order.
    #[allow(dead_code)] // consumed by the Telemetry overlay's wake-source breakdown
    pub fn per_source(&self) -> [u64; WakeSource::N] {
        self.drain_items
    }

    /// Message count for one source this interval.
    pub fn items(&self, src: WakeSource) -> u64 {
        self.drain_items[src as usize]
    }

    /// Seconds since the last reset.
    pub fn elapsed_secs(&self) -> f64 {
        self.report_t0.elapsed().as_secs_f64().max(1e-9)
    }

    /// Reset all counters and restart the interval clock.
    pub fn take(&mut self) {
        self.drain_items = [0; WakeSource::N];
        self.wakes = 0;
        self.renders = 0;
        self.render_skips = 0;
        self.render_us.reset();
        self.pty_chunks = 0;
        self.pty_budget_hits = 0;
        self.busy = Duration::ZERO;
        self.report_t0 = Instant::now();
    }
}

impl Default for LoopPerf {
    fn default() -> Self {
        Self::new()
    }
}

/// A periodic snapshot of loop + subsystem perf, produced by [`LoopPerf::rollup`]
/// and consumed by the live Telemetry overlay. All rates are per-second.
#[derive(Clone, Debug, Default)]
pub struct PerfSnapshot {
    #[allow(dead_code)] // rollup interval length; surfaced by the Telemetry overlay
    pub secs: f64,
    pub wakes_per_s: f64,
    pub renders_per_s: f64,
    pub render_skips_per_s: f64,
    pub render_p50_us: u64,
    pub render_p99_us: u64,
    pub idle_ratio: f64,
    pub hot_source: &'static str,
    pub hot_items_per_s: f64,
    pub pty_chunks_per_s: f64,
    /// Per-subsystem CPU ms this interval, in [`Subsys::ALL`] order.
    #[allow(dead_code)] // surfaced by the Telemetry overlay's per-subsystem breakdown
    pub cpu_ms: [f64; Subsys::N],
}

impl LoopPerf {
    /// Compute rates, emit the `szhost::perf` tracing rollup (+ a wake-storm
    /// warning if idle but pulsing), drain the CPU ledger, reset, and return a
    /// snapshot for the live overlay. Called by the loop when [`due`](Self::due)
    /// — never on its own timer (that would add a wake source).
    pub fn rollup(&mut self) -> PerfSnapshot {
        let secs = self.elapsed_secs();
        let cpu = CPU.take();
        let mut cpu_ms = [0.0f64; Subsys::N];
        for i in 0..Subsys::N {
            cpu_ms[i] = cpu[i].0 as f64 / 1.0e6;
        }
        let hot = self.hot_source();
        let hot_items = self.items(hot);
        let busy_ratio = (self.busy.as_secs_f64() / secs).clamp(0.0, 1.0);
        let snap = PerfSnapshot {
            secs,
            wakes_per_s: self.wakes as f64 / secs,
            renders_per_s: self.renders as f64 / secs,
            render_skips_per_s: self.render_skips as f64 / secs,
            render_p50_us: self.render_us.percentile_us(0.50),
            render_p99_us: self.render_us.percentile_us(0.99),
            idle_ratio: 1.0 - busy_ratio,
            hot_source: hot.label(),
            hot_items_per_s: hot_items as f64 / secs,
            pty_chunks_per_s: self.pty_chunks as f64 / secs,
            cpu_ms,
        };

        tracing::info!(
            target: "szhost::perf",
            wakes_per_s = snap.wakes_per_s,
            renders_per_s = snap.renders_per_s,
            render_skips_per_s = snap.render_skips_per_s,
            render_p50_us = snap.render_p50_us,
            render_p99_us = snap.render_p99_us,
            idle_ratio = snap.idle_ratio,
            hot_source = snap.hot_source,
            hot_items_per_s = snap.hot_items_per_s,
            pty_chunks_per_s = snap.pty_chunks_per_s,
            pty_budget_hits = self.pty_budget_hits,
            cpu_hydrate_ms = cpu_ms[Subsys::Hydrate as usize],
            cpu_stats_ms = cpu_ms[Subsys::Stats as usize],
            cpu_pr_ms = cpu_ms[Subsys::Pr as usize],
            cpu_metrics_ms = cpu_ms[Subsys::Metrics as usize],
            cpu_diff_ms = cpu_ms[Subsys::Diff as usize],
            "perf rollup"
        );

        // Per-source breakdown at debug level (mirrors the startup/frame tiers).
        for &s in &WakeSource::ALL {
            let n = self.items(s);
            if n > 0 {
                tracing::debug!(
                    target: "szhost::perf",
                    source = s.label(),
                    items_per_s = n as f64 / secs,
                    "perf source"
                );
            }
        }

        // Wake storm: loop is essentially idle (doing no real work) yet a
        // background source keeps pulsing the waker — exactly the diff-watcher
        // `.git/` storm this codebase has hit. WARN shows at the default level.
        if snap.idle_ratio > 0.95 && snap.wakes_per_s > wake_storm_limit() {
            tracing::warn!(
                target: "szhost::perf",
                wakes_per_s = snap.wakes_per_s,
                hot_source = snap.hot_source,
                hot_items_per_s = snap.hot_items_per_s,
                "wake storm while idle: {} dominating",
                snap.hot_source
            );
        }

        self.take();
        snap
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // The master switch is process-global; serialize the two tests that flip it
    // so cargo's parallel test threads don't clobber each other's state.
    static TEST_GUARD: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[test]
    fn histo_percentiles_are_monotonic_and_bounded() {
        let mut h = Histo::new();
        assert!(h.is_empty());
        // 900 samples clustered around ~1ms (512us bucket: floor(log2(1000))=9).
        for _ in 0..900 {
            h.record_us(1000);
        }
        // 100 slow outliers near ~16ms (8192us bucket: floor(log2(16000))=13).
        for _ in 0..100 {
            h.record_us(16_000);
        }
        assert!(!h.is_empty());
        let p50 = h.percentile_us(0.5);
        let p99 = h.percentile_us(0.99);
        assert!(p50 <= p99, "p50 {p50} should be <= p99 {p99}");
        // p50 lands in the fast bucket; p99 (rank 990 > 900 fast) reaches the outliers.
        assert_eq!(p50, 512);
        assert!(p99 >= 8192, "p99 {p99} should reflect the 16ms outliers");
    }

    #[test]
    fn histo_handles_zero_and_reset() {
        let mut h = Histo::new();
        h.record_us(0);
        assert_eq!(h.percentile_us(0.5), 0);
        h.reset();
        assert!(h.is_empty());
        assert_eq!(h.percentile_us(0.99), 0);
    }

    #[test]
    fn cpu_ledger_add_take_resets() {
        let ledger = CpuLedger::new();
        ledger.add(Subsys::Hydrate, 100);
        ledger.add(Subsys::Hydrate, 50);
        ledger.add(Subsys::Stats, 7);
        let first = ledger.take();
        assert_eq!(first[Subsys::Hydrate as usize], (150, 2));
        assert_eq!(first[Subsys::Stats as usize], (7, 1));
        assert_eq!(first[Subsys::Pr as usize], (0, 0));
        // take() resets, so a second read is all zeros.
        let second = ledger.take();
        assert_eq!(second[Subsys::Hydrate as usize], (0, 0));
    }

    #[test]
    fn loop_perf_noop_when_disabled() {
        let _g = TEST_GUARD.lock().unwrap_or_else(|e| e.into_inner());
        set_enabled(false);
        let mut lp = LoopPerf::new();
        lp.wake();
        lp.tick(WakeSource::Model);
        lp.render(Duration::from_micros(900));
        assert_eq!(lp.wakes, 0);
        assert_eq!(lp.renders, 0);
        assert_eq!(lp.items(WakeSource::Model), 0);
    }

    #[test]
    fn loop_perf_counts_when_enabled() {
        let _g = TEST_GUARD.lock().unwrap_or_else(|e| e.into_inner());
        set_enabled(true);
        let mut lp = LoopPerf::new();
        lp.wake();
        lp.wake();
        lp.tick(WakeSource::Model);
        lp.tick(WakeSource::Model);
        lp.tick(WakeSource::Model);
        lp.tick(WakeSource::Model);
        lp.tick(WakeSource::Model);
        lp.tick(WakeSource::Stats);
        lp.pty(10, true);
        lp.render(Duration::from_micros(900));
        lp.render_skip();
        assert_eq!(lp.wakes, 2);
        assert_eq!(lp.renders, 1);
        assert_eq!(lp.render_skips, 1);
        assert_eq!(lp.pty_chunks, 10);
        assert_eq!(lp.pty_budget_hits, 1);
        assert_eq!(lp.items(WakeSource::Model), 5);
        assert_eq!(lp.hot_source(), WakeSource::Model);
        assert_eq!(lp.items(WakeSource::Pty), 10);
        set_enabled(false);
    }

    #[test]
    fn thread_cpu_ns_is_monotonic() {
        // Burns a little CPU; the clock must not go backwards. On platforms
        // without the clock this is 0 == 0 and still passes.
        let a = thread_cpu_ns();
        let mut acc = 0u64;
        for i in 0..200_000u64 {
            acc = acc.wrapping_add(i);
        }
        let b = thread_cpu_ns();
        std::hint::black_box(acc);
        assert!(b >= a);
    }
}
