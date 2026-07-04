//! Cross-platform system metrics for the superzej masthead + "LOOP" telemetry
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
//! [`StatsSampler::sample`], preserving superzej's ~0%-idle invariant.

mod battery;
mod gpu;
mod sample;

pub use battery::read_battery;
pub use sample::{StatsSampler, SystemInfo};

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
    /// GPU utilization 0–100 (Linux sysfs / NVIDIA only; absent otherwise).
    pub gpu_pct: Option<u8>,
    /// Network as (rx, tx) bytes/sec summed across non-loopback interfaces.
    pub net_bps: Option<(u64, u64)>,
    /// Per-interface (name, rx bytes/sec, tx bytes/sec), non-loopback.
    pub net_ifaces: Vec<(String, u64, u64)>,
    /// Battery as (percent 0–100, on AC). The bool is "plugged in", not
    /// "actively charging", so a charge-capped battery still reads as on AC.
    /// Absent on desktops / machines without a battery.
    pub battery: Option<(u8, bool)>,
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
pub fn disk_space(path: &std::path::Path) -> Option<(u64, u64, u8)> {
    use std::os::unix::ffi::OsStrExt;
    let mut p = path;
    while !p.exists() {
        p = p.parent()?;
    }
    let c = std::ffi::CString::new(p.as_os_str().as_bytes()).ok()?;
    // SAFETY: `c` is a valid NUL-terminated path; `st` is zeroed before the
    // call and only read on success.
    let mut st: libc::statvfs = unsafe { std::mem::zeroed() };
    if unsafe { libc::statvfs(c.as_ptr(), &mut st) } != 0 {
        return None;
    }
    let blocks = st.f_blocks as u64;
    if blocks == 0 {
        return None;
    }
    // f_bavail = blocks available to unprivileged users (the headroom you'd
    // actually get), which is what "free" should reflect. f_frsize is the
    // fundamental block size the block counts are expressed in.
    let avail_blocks = st.f_bavail as u64;
    let frsize = st.f_frsize as u64;
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
    }
}
