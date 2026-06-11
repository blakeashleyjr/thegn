//! Lightweight system stats for the top bar: CPU / MEM / GPU / NET sampled on
//! the refresh-ticker thread (never the event loop) and handed over as a
//! [`StatsSnapshot`]. The `/proc` parsers and formatters are pure and
//! unit-tested; the sampling shell is the thin I/O layer around them.

/// One sampled reading; `None` fields render as absent widgets.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct StatsSnapshot {
    /// CPU utilization 0–100 (delta over the sample interval).
    pub cpu_pct: Option<u8>,
    /// Memory as (used GiB, total GiB).
    pub mem_gib: Option<(f32, f32)>,
    /// GPU utilization 0–100 (NVIDIA only; absent when undetected).
    pub gpu_pct: Option<u8>,
    /// Network as (rx, tx) bytes/sec across non-loopback interfaces.
    pub net_bps: Option<(u64, u64)>,
    /// Battery as (percent 0–100, charging). Absent on desktops.
    pub battery: Option<(u8, bool)>,
}

/// Read the first battery under `/sys/class/power_supply` (capacity %, and
/// whether it's charging/full — i.e. on AC). Pure given a base dir, so the
/// parser is unit-testable against a fixture tree.
pub fn read_battery(base: &std::path::Path) -> Option<(u8, bool)> {
    let entries = std::fs::read_dir(base).ok()?;
    for e in entries.flatten() {
        let p = e.path();
        // Batteries advertise type "Battery" (AC adapters say "Mains").
        let is_battery = std::fs::read_to_string(p.join("type"))
            .map(|t| t.trim() == "Battery")
            .unwrap_or(false);
        if !is_battery {
            continue;
        }
        let pct = std::fs::read_to_string(p.join("capacity"))
            .ok()?
            .trim()
            .parse::<u8>()
            .ok()?;
        let status = std::fs::read_to_string(p.join("status")).unwrap_or_default();
        let charging = matches!(status.trim(), "Charging" | "Full" | "Not charging");
        return Some((pct.min(100), charging));
    }
    None
}

/// Stateful sampler: keeps the previous counters so CPU/NET deltas are real
/// rates. Lives on the ticker thread.
pub struct StatsSampler {
    prev_cpu: Option<(u64, u64)>, // (total, idle) jiffies
    prev_net: Option<(u64, u64, std::time::Instant)>,
    gpu: GpuProbe,
}

/// How GPU utilization is read (probed once at startup).
enum GpuProbe {
    /// amdgpu/i915 expose a percent file in sysfs.
    Sysfs(std::path::PathBuf),
    /// NVIDIA via nvidia-smi.
    NvidiaSmi,
    None,
}

fn probe_gpu() -> GpuProbe {
    // Sysfs first (AMD/Intel — no subprocess per sample).
    if let Ok(cards) = std::fs::read_dir("/sys/class/drm") {
        for card in cards.flatten() {
            let p = card.path().join("device/gpu_busy_percent");
            if p.is_file() {
                return GpuProbe::Sysfs(p);
            }
        }
    }
    let nvidia = std::process::Command::new("nvidia-smi")
        .arg("--version")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    if nvidia {
        GpuProbe::NvidiaSmi
    } else {
        GpuProbe::None
    }
}

impl StatsSampler {
    pub fn new() -> Self {
        StatsSampler {
            prev_cpu: None,
            prev_net: None,
            gpu: probe_gpu(),
        }
    }

    /// Take one reading (blocking file reads + optional nvidia-smi subprocess —
    /// ticker-thread only).
    pub fn sample(&mut self) -> StatsSnapshot {
        let mut snap = StatsSnapshot::default();

        if let Ok(stat) = std::fs::read_to_string("/proc/stat")
            && let Some((total, idle)) = parse_proc_stat(&stat)
        {
            if let Some((pt, pi)) = self.prev_cpu {
                snap.cpu_pct = cpu_pct((pt, pi), (total, idle));
            }
            self.prev_cpu = Some((total, idle));
        }

        if let Ok(mem) = std::fs::read_to_string("/proc/meminfo") {
            snap.mem_gib = parse_meminfo(&mem);
        }

        snap.battery = read_battery(std::path::Path::new("/sys/class/power_supply"));

        if let Ok(net) = std::fs::read_to_string("/proc/net/dev") {
            let (rx, tx) = parse_net_dev(&net);
            let now = std::time::Instant::now();
            if let Some((prx, ptx, pt)) = self.prev_net {
                let dt = now.duration_since(pt).as_secs_f64().max(0.001);
                snap.net_bps = Some((
                    ((rx.saturating_sub(prx)) as f64 / dt) as u64,
                    ((tx.saturating_sub(ptx)) as f64 / dt) as u64,
                ));
            }
            self.prev_net = Some((rx, tx, now));
        }

        snap.gpu_pct = match &self.gpu {
            GpuProbe::Sysfs(path) => std::fs::read_to_string(path)
                .ok()
                .and_then(|v| v.trim().parse::<u8>().ok()),
            GpuProbe::NvidiaSmi => std::process::Command::new("nvidia-smi")
                .args([
                    "--query-gpu=utilization.gpu",
                    "--format=csv,noheader,nounits",
                ])
                .output()
                .ok()
                .and_then(|o| {
                    String::from_utf8_lossy(&o.stdout)
                        .lines()
                        .next()
                        .and_then(|l| l.trim().parse::<u8>().ok())
                }),
            GpuProbe::None => None,
        };

        snap
    }
}

/// `/proc/stat` first line → (total jiffies, idle jiffies). Idle includes
/// iowait, matching the conventional utilization formula.
pub fn parse_proc_stat(text: &str) -> Option<(u64, u64)> {
    let line = text.lines().find(|l| l.starts_with("cpu "))?;
    let fields: Vec<u64> = line
        .split_whitespace()
        .skip(1)
        .filter_map(|f| f.parse().ok())
        .collect();
    if fields.len() < 5 {
        return None;
    }
    let total: u64 = fields.iter().sum();
    let idle = fields[3] + fields.get(4).copied().unwrap_or(0);
    Some((total, idle))
}

/// Utilization percentage between two `/proc/stat` readings.
pub fn cpu_pct(prev: (u64, u64), cur: (u64, u64)) -> Option<u8> {
    let dt = cur.0.checked_sub(prev.0)?;
    if dt == 0 {
        return None;
    }
    let didle = cur.1.saturating_sub(prev.1);
    Some((((dt - didle.min(dt)) * 100) / dt) as u8)
}

/// `/proc/meminfo` → (used GiB, total GiB), used = total − available.
pub fn parse_meminfo(text: &str) -> Option<(f32, f32)> {
    let kb = |key: &str| {
        text.lines()
            .find(|l| l.starts_with(key))
            .and_then(|l| l.split_whitespace().nth(1))
            .and_then(|v| v.parse::<u64>().ok())
    };
    let total = kb("MemTotal:")?;
    let avail = kb("MemAvailable:")?;
    let gib = |k: u64| k as f32 / (1024.0 * 1024.0);
    Some((gib(total.saturating_sub(avail)), gib(total)))
}

/// `/proc/net/dev` → cumulative (rx, tx) bytes across non-loopback interfaces.
pub fn parse_net_dev(text: &str) -> (u64, u64) {
    let mut rx = 0u64;
    let mut tx = 0u64;
    for line in text.lines().skip(2) {
        let Some((iface, rest)) = line.split_once(':') else {
            continue;
        };
        if iface.trim() == "lo" {
            continue;
        }
        let f: Vec<u64> = rest
            .split_whitespace()
            .filter_map(|v| v.parse().ok())
            .collect();
        if f.len() >= 9 {
            rx += f[0];
            tx += f[8];
        }
    }
    (rx, tx)
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
    fn proc_stat_parses_and_rates() {
        let a = "cpu  100 0 100 700 100 0 0 0 0 0\ncpu0 1 2 3 4 5\n";
        let b = "cpu  150 0 150 750 150 0 0 0 0 0\n";
        let pa = parse_proc_stat(a).unwrap();
        let pb = parse_proc_stat(b).unwrap();
        assert_eq!(pa, (1000, 800));
        assert_eq!(pb, (1200, 900));
        // Δtotal=200, Δidle=100 → 50% busy.
        assert_eq!(cpu_pct(pa, pb), Some(50));
        // No progress → None.
        assert_eq!(cpu_pct(pb, pb), None);
        assert_eq!(parse_proc_stat("intr 0 0\n"), None);
    }

    #[test]
    fn meminfo_used_is_total_minus_available() {
        let text = "MemTotal:       16777216 kB\nMemFree:         1000000 kB\nMemAvailable:    8388608 kB\n";
        let (used, total) = parse_meminfo(text).unwrap();
        assert!((total - 16.0).abs() < 0.01, "total {total}");
        assert!((used - 8.0).abs() < 0.01, "used {used}");
        assert_eq!(parse_meminfo("nope"), None);
    }

    #[test]
    fn net_dev_sums_non_loopback() {
        let text = "\
Inter-|   Receive                                                |  Transmit
 face |bytes    packets errs drop fifo frame compressed multicast|bytes    packets errs drop fifo colls carrier compressed
    lo: 9999999    100    0    0    0     0          0         0  9999999     100    0    0    0     0       0          0
  eth0:    1000     10    0    0    0     0          0         0     2000      20    0    0    0     0       0          0
 wlan0:     500      5    0    0    0     0          0         0      700       7    0    0    0     0       0          0
";
        assert_eq!(parse_net_dev(text), (1500, 2700));
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

    #[test]
    fn read_battery_parses_fixture_tree() {
        let base = std::env::temp_dir().join(format!("sz-batt-{}", std::process::id()));
        let bat = base.join("BAT0");
        let ac = base.join("AC");
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&bat).unwrap();
        std::fs::create_dir_all(&ac).unwrap();
        std::fs::write(ac.join("type"), "Mains\n").unwrap();
        std::fs::write(bat.join("type"), "Battery\n").unwrap();
        std::fs::write(bat.join("capacity"), "73\n").unwrap();
        std::fs::write(bat.join("status"), "Charging\n").unwrap();
        assert_eq!(super::read_battery(&base), Some((73, true)));

        std::fs::write(bat.join("status"), "Discharging\n").unwrap();
        assert_eq!(super::read_battery(&base), Some((73, false)));

        // No battery dir at all → None (desktop).
        let empty = base.join("none");
        std::fs::create_dir_all(&empty).unwrap();
        assert_eq!(super::read_battery(&empty), None);
        let _ = std::fs::remove_dir_all(&base);
    }
}
