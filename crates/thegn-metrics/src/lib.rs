//! Cross-platform system metrics for the thegn masthead + "LOOP" telemetry
//! overlay. Sampled on the host's refresh-ticker thread (never the event loop)
//! and handed over as a [`StatsSnapshot`].
//!
//! The substrate is `sysinfo` (CPU/mem/swap/net/disk/components/system) on
//! every platform, plus two things sysinfo does not cover:
//! - **GPU** — Linux sysfs (`/sys/class/drm`) + `nvidia-smi`; `None` elsewhere.
//! - **Battery** — native sysfs + adapter `online` flag on Linux,
//!   `starship-battery` on other platforms.
//!
//! sysinfo does no background work; cost is paid only when the host calls
//! [`StatsSampler::sample`], preserving thegn's ~0%-idle invariant.

mod battery;
mod gpu;
mod procs;
mod sample;
mod thermal;

pub use battery::{read_battery, read_battery_power};
pub use procs::{ProcOwner, ProcSample, ProcSampler, ProcSnapshot};
pub use sample::{StatsSampler, SystemInfo, TrackedSpec};

/// One sampled reading; `None`/empty fields render as absent widgets, so a
/// platform that cannot supply a metric (e.g. temperatures on Windows) simply
/// hides it rather than showing a wrong value.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct StatsSnapshot {
    /// CPU utilization 0–100 (delta over the sample interval).
    pub cpu_pct: Option<u8>,
    /// Per-core utilization 0–100, in core order. Empty until the first delta
    /// is available.
    pub cpu_cores: Vec<u8>,
    /// Mean CPU frequency in MHz across cores.
    pub cpu_freq_mhz: Option<u64>,
    /// CPU/package temperature in °C (the hottest CPU-ish sensor).
    pub cpu_temp_c: Option<f32>,
    /// Memory as (used GiB, total GiB).
    pub mem_gib: Option<(f32, f32)>,
    /// Swap as (used GiB, total GiB). Absent when there is no swap.
    pub swap_gib: Option<(f32, f32)>,
    /// Pages reclaimed **synchronously** (`pgsteal_direct`), per second.
    ///
    /// Direct reclaim is a thread being made to free memory before its own
    /// allocation can proceed — it is the mechanism by which a machine stops
    /// responding, as distinct from swap simply being *occupied*. Swap fullness
    /// is a lagging proxy and can sit comfortably under any threshold while this
    /// is enormous, which is exactly what happened on a box that stalled at 41%
    /// swap. Linux only (`/proc/vmstat`); `None` elsewhere, never `0.0`.
    ///
    /// A RATE, not the raw counter: `pgsteal_direct` is monotonic since boot, so
    /// an absolute value says nothing about now.
    pub reclaim_per_s: Option<f32>,
    /// GPU utilization 0–100 (Linux sysfs / NVIDIA only; absent otherwise).
    pub gpu_pct: Option<u8>,
    /// GPU memory as (used MiB, total MiB). NVIDIA (`nvidia-smi`) and AMD/Intel
    /// sysfs (`mem_info_vram_*`) where exposed; absent otherwise.
    pub gpu_mem_mib: Option<(u64, u64)>,
    /// GPU temperature in °C (NVIDIA only today; absent otherwise).
    pub gpu_temp_c: Option<f32>,
    /// GPU board power draw in watts (NVIDIA only today; absent otherwise).
    pub gpu_power_w: Option<f32>,
    /// Network as (rx, tx) bytes/sec summed across non-loopback interfaces.
    pub net_bps: Option<(u64, u64)>,
    /// Per-interface (name, rx bytes/sec, tx bytes/sec), non-loopback.
    pub net_ifaces: Vec<(String, u64, u64)>,
    /// Battery as (percent 0–100, on AC). The bool is "plugged in", not
    /// "actively charging", so a charge-capped battery still reads as on AC.
    /// Absent on desktops / machines without a battery.
    pub battery: Option<(u8, bool)>,
    /// Battery power flow in watts (magnitude; discharging or charging rate).
    /// Absent when the platform exposes no power/current reading.
    pub battery_power_w: Option<f32>,
    /// Estimated seconds to empty (discharging) or to full (charging), computed
    /// from the native energy/charge and power counters. Absent when idle or
    /// unavailable.
    pub battery_eta_secs: Option<u64>,
    /// Free space on the worktrees' filesystem, as a percentage 0–100.
    pub disk_free_pct: Option<u8>,
    /// Worktrees' filesystem capacity as (total bytes, available bytes). Absent
    /// on non-unix targets or a `statvfs` error, exactly like `disk_free_pct`.
    pub disk_bytes: Option<(u64, u64)>,
    /// All mounted physical disks (name, mount, free %, IO rates, kind).
    pub disks: Vec<DiskInfo>,
    /// Temperature sensors as (label, °C). Drives the telemetry thermal row.
    pub temps: Vec<(String, f32)>,
    /// Load average (1, 5, 15 min). `None` on platforms without it (Windows).
    pub load_avg: Option<(f32, f32, f32)>,
    /// System uptime in seconds.
    pub uptime_secs: Option<u64>,
    /// Resident-set size of the thegn (compositor) process itself, in bytes.
    /// Feeds the daemon/status modal's "this process" history graph.
    pub self_rss_bytes: Option<u64>,
    /// CPU utilization of the thegn process (delta over the interval). May
    /// exceed 100 on a multi-threaded burst — it is a per-core sum, not clamped.
    pub self_cpu_pct: Option<f32>,
    /// Resident-set size of the pane-daemon process, in bytes. Absent until the
    /// daemon PID is known (see [`StatsSampler::set_daemon_pid`]).
    pub daemon_rss_bytes: Option<u64>,
    /// CPU utilization of the pane-daemon process (delta over the interval).
    pub daemon_cpu_pct: Option<f32>,
    /// Registered child processes, rolled up per group and sorted heaviest
    /// first — language servers, plugin hosts, watchers.
    ///
    /// Empty is the ordinary zero-language-server case, not an error. These are
    /// the costs that used to appear in no metric at all: on one session the
    /// compositor reported 507 MB while its children held 1,016 MB.
    ///
    /// Pane processes are NOT here. They belong to the user, and they hang off
    /// the pane daemon rather than the compositor, so they are excluded by
    /// process topology rather than by filtering.
    pub children: Vec<ChildGroup>,
}

impl StatsSnapshot {
    /// Total resident memory of all registered children.
    pub fn children_rss_bytes(&self) -> u64 {
        self.children
            .iter()
            .fold(0u64, |a, g| a.saturating_add(g.rss_bytes))
    }

    /// Total CPU of all registered children (per-core sum, so it can exceed 100).
    pub fn children_cpu_pct(&self) -> f32 {
        self.children.iter().map(|g| g.cpu_pct).sum()
    }

    /// How many child processes are accounted for.
    pub fn children_count(&self) -> usize {
        self.children.iter().map(|g| g.count).sum()
    }

    /// thegn's whole resident footprint: compositor + daemon + registered
    /// children. This is the number a user means by "how much RAM is thegn
    /// using", and the one `self_rss_bytes` alone under-reports.
    pub fn total_rss_bytes(&self) -> u64 {
        self.self_rss_bytes
            .unwrap_or(0)
            .saturating_add(self.daemon_rss_bytes.unwrap_or(0))
            .saturating_add(self.children_rss_bytes())
    }
}

/// Registered children of one group, summed.
#[derive(Debug, Clone, PartialEq)]
pub struct ChildGroup {
    /// The group key, e.g. `"lsp"` — `thegn_core::proc_registry`'s `GROUP_*`.
    pub group: String,
    /// How many live processes contributed.
    pub count: usize,
    pub rss_bytes: u64,
    /// Per-core sum across the group, unclamped (same convention as
    /// [`StatsSnapshot::self_cpu_pct`]).
    pub cpu_pct: f32,
}

/// A mounted disk's snapshot. `read_bps`/`write_bps` are bytes/sec over the
/// sample interval (0 when IO accounting is unavailable on the platform).
#[derive(Debug, Clone, PartialEq)]
pub struct DiskInfo {
    pub name: String,
    pub mount: String,
    pub free_pct: u8,
    pub read_bps: u64,
    pub write_bps: u64,
    pub kind: DiskKind,
}

/// Storage medium, mirrored from sysinfo so consumers needn't depend on it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiskKind {
    Hdd,
    Ssd,
    Unknown,
}

/// Capacity of the filesystem containing `path` as `(total bytes, available
/// bytes, free percentage 0–100)`. Walks up to the first existing ancestor so a
/// not-yet-created worktrees dir still reports its parent fs. `None` on a
/// non-unix target or `statvfs` error.
#[cfg(unix)]
// The statvfs widenings below are identity conversions on 64-bit Linux
// (fsblkcnt_t/c_ulong are u64 there) but real u32→u64 widenings on macOS —
// clippy only sees the native target, so it calls them useless.
#[allow(clippy::useless_conversion)]
pub fn disk_space(path: &std::path::Path) -> Option<(u64, u64, u8)> {
    let mut p = path;
    while !p.exists() {
        p = p.parent()?;
    }
    let st = nix::sys::statvfs::statvfs(p).ok()?;
    // Widen everything to u64 up front: `fsblkcnt_t` is u32 on macOS (u64 on
    // Linux) while `f_frsize` is c_ulong, so the field types only agree on
    // 64-bit Linux. `Into` is an identity there and lossless on macOS —
    // unlike `as u64`, it can't clippy-warn as an unnecessary cast natively.
    let blocks: u64 = st.blocks().into();
    if blocks == 0 {
        return None;
    }
    // f_bavail = blocks available to unprivileged users (the headroom you'd
    // actually get), which is what "free" should reflect. f_frsize is the
    // fundamental block size the block counts are expressed in.
    let avail_blocks: u64 = st.blocks_available().into();
    let frsize: u64 = st.fragment_size().into();
    let total_bytes = blocks.saturating_mul(frsize);
    let avail_bytes = avail_blocks.saturating_mul(frsize);
    let pct = ((avail_blocks as f64 / blocks as f64) * 100.0)
        .round()
        .clamp(0.0, 100.0) as u8;
    Some((total_bytes, avail_bytes, pct))
}

#[cfg(not(unix))]
pub fn disk_space(_path: &std::path::Path) -> Option<(u64, u64, u8)> {
    // sysinfo's per-disk free % still populates `StatsSnapshot::disks` on
    // Windows; this convenience value is the unix-only statvfs fast path.
    None
}

/// Free space on the filesystem containing `path`, as a percentage (0–100).
/// Thin wrapper over [`disk_space`] for callers that only need the percentage.
pub fn disk_free_pct(path: &std::path::Path) -> Option<u8> {
    disk_space(path).map(|(_, _, pct)| pct)
}

/// Fixed-width (6 char) bytes/sec for the NET widget — stable width so the
/// right-aligned stats block never shifts as numbers grow.
pub fn fmt_rate(bps: u64) -> String {
    let s = match bps {
        b if b >= 1024 * 1024 * 1024 => format!("{:.1}G", b as f64 / (1u64 << 30) as f64),
        b if b >= 1024 * 1024 => format!("{:.1}M", b as f64 / (1 << 20) as f64),
        b if b >= 1024 => format!("{:.0}K", b as f64 / 1024.0),
        b => format!("{b}B"),
    };
    format!("{s:>6}")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The registry's whole point: a registered child's memory must actually
    /// reach the snapshot. Uses THIS process as a stand-in child — it certainly
    /// exists and certainly has RSS — so the assertion needs no fixture.
    #[test]
    fn a_registered_child_lands_in_the_rollup_and_the_total() {
        let mut s = StatsSampler::new(std::env::temp_dir());
        // A group name that is not one of the real ones, so this can never be
        // confused with a genuine reading.
        s.set_tracked(vec![TrackedSpec {
            pid: std::process::id(),
            group: "test-group".into(),
        }]);
        let _ = s.sample();
        std::thread::sleep(sysinfo::MINIMUM_CPU_UPDATE_INTERVAL);
        let snap = s.sample();

        let g = snap
            .children
            .iter()
            .find(|g| g.group == "test-group")
            .expect("the registered child is accounted for");
        assert_eq!(g.count, 1);
        assert!(g.rss_bytes > 0, "a live process has resident memory");
        assert_eq!(snap.children_count(), 1);
        assert_eq!(snap.children_rss_bytes(), g.rss_bytes);
        // The headline number must include it — under-reporting the total is
        // the bug this whole seam exists to fix.
        assert!(
            snap.total_rss_bytes() >= g.rss_bytes,
            "total folds in the children"
        );
    }

    /// No language servers, no plugins, no watchers: the ordinary case. The
    /// snapshot must degrade to exactly the old self+daemon shape rather than
    /// reporting an empty group or a zero row.
    #[test]
    fn no_registered_children_reports_nothing_rather_than_zeroes() {
        let mut s = StatsSampler::new(std::env::temp_dir());
        let _ = s.sample();
        std::thread::sleep(sysinfo::MINIMUM_CPU_UPDATE_INTERVAL);
        let snap = s.sample();
        assert!(snap.children.is_empty());
        assert_eq!(snap.children_rss_bytes(), 0);
        assert_eq!(snap.children_count(), 0);
    }

    /// A registration is a hint, not a promise: a handle can outlive a process
    /// that crashed or was killed. A PID the OS no longer knows must contribute
    /// nothing, or the totals would report memory that does not exist.
    #[test]
    fn a_dead_pid_contributes_nothing() {
        let mut s = StatsSampler::new(std::env::temp_dir());
        // PID 0 is never a real user process on any supported platform.
        s.set_tracked(vec![TrackedSpec {
            pid: 0,
            group: "ghost".into(),
        }]);
        let _ = s.sample();
        std::thread::sleep(sysinfo::MINIMUM_CPU_UPDATE_INTERVAL);
        let snap = s.sample();
        assert!(
            !snap.children.iter().any(|g| g.group == "ghost"),
            "a vanished process must not appear at all"
        );
    }

    #[test]
    fn rate_formatting_is_fixed_width() {
        assert_eq!(fmt_rate(12), "   12B");
        assert_eq!(fmt_rate(2048), "    2K");
        assert_eq!(fmt_rate(3 * 1024 * 1024 / 2), "  1.5M");
        assert_eq!(fmt_rate(3 * 1024 * 1024 * 1024 / 2), "  1.5G");
        for v in [0, 999, 10_240, 5 << 20, 3 << 30] {
            assert_eq!(fmt_rate(v).chars().count(), 6, "{v}");
        }
    }

    /// Cross-platform contract: whatever backend compiled in, two samples
    /// (CPU rates need a delta) must yield a well-formed snapshot — never a
    /// panic, never an out-of-range value. This is the per-platform regression
    /// gate that runs under `cargo test` on Linux/macOS/Windows alike.
    #[test]
    fn sample_is_well_formed() {
        let mut s = StatsSampler::new(std::env::temp_dir());
        let _ = s.sample();
        std::thread::sleep(sysinfo::MINIMUM_CPU_UPDATE_INTERVAL);
        let snap = s.sample();

        if let Some(p) = snap.cpu_pct {
            assert!(p <= 100, "cpu {p}");
        }
        for (i, &c) in snap.cpu_cores.iter().enumerate() {
            assert!(c <= 100, "core {i} = {c}");
        }
        if let Some((u, t)) = snap.mem_gib {
            assert!(t >= 0.0 && u <= t + 0.001, "mem {u}/{t}");
        }
        if let Some((u, t)) = snap.swap_gib {
            assert!(t >= 0.0 && u <= t + 0.001, "swap {u}/{t}");
        }
        if let Some(p) = snap.gpu_pct {
            assert!(p <= 100, "gpu {p}");
        }
        if let Some((p, _)) = snap.battery {
            assert!(p <= 100, "battery {p}");
        }
        if let Some(p) = snap.disk_free_pct {
            assert!(p <= 100, "disk {p}");
        }
        if let Some((total, avail)) = snap.disk_bytes {
            assert!(total > 0, "disk total {total}");
            assert!(avail <= total, "disk avail {avail} > total {total}");
        }
        for d in &snap.disks {
            assert!(d.free_pct <= 100, "disk {} free {}", d.name, d.free_pct);
        }
        for (label, c) in &snap.temps {
            assert!(c.is_finite(), "temp {label} = {c}");
        }
        // The sampler always watches its own PID, so the second reading has a
        // valid RSS and a primed (finite, non-negative) CPU delta.
        assert!(snap.self_rss_bytes.unwrap_or(0) > 0, "self rss");
        if let Some(c) = snap.self_cpu_pct {
            assert!(c.is_finite() && c >= 0.0, "self cpu {c}");
        }
    }

    #[test]
    fn daemon_pid_reprimes_without_panicking() {
        let mut s = StatsSampler::new(std::env::temp_dir());
        // Point at our own PID as a stand-in daemon: it exists, so the field
        // populates; toggling it re-primes the CPU delta cleanly.
        s.set_daemon_pid(Some(std::process::id()));
        let _ = s.sample();
        std::thread::sleep(sysinfo::MINIMUM_CPU_UPDATE_INTERVAL);
        let snap = s.sample();
        assert!(snap.daemon_rss_bytes.unwrap_or(0) > 0, "daemon rss");
        s.set_daemon_pid(None);
        let snap = s.sample();
        assert!(snap.daemon_rss_bytes.is_none(), "cleared daemon pid");
    }
}

#[cfg(test)]
mod platform_ratchet_tests;
#[cfg(test)]
mod ratchet;
