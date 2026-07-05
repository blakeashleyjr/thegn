//! The sampler: one reused `sysinfo` instance set, refreshed selectively and in
//! tiers on the host's ticker thread. Cheap metrics (CPU/mem/net) refresh every
//! tick; slow-moving ones (frequency, temperatures, disk enumeration + IO)
//! refresh every [`SLOW_EVERY`]-th tick and are cached in between. Processes are
//! never enumerated — that is sysinfo's expensive path and we don't need it.

use std::time::Instant;

use sysinfo::{
    Components, CpuRefreshKind, Disks, MemoryRefreshKind, Networks, RefreshKind, System,
};

use crate::gpu::GpuProbe;
use crate::{DiskInfo, DiskKind, StatsSnapshot, disk_space, read_battery, read_battery_power};

/// Refresh the slow tier (frequency / temperatures / disks) once every N
/// samples. At the host's default ~1s cadence that is roughly every 5s — these
/// move slowly and the enumeration is the most expensive part of a sample.
const SLOW_EVERY: u64 = 5;

/// Static, read-once system identity (hostname / kernel / OS). Cheap but
/// constant, so the host collects it once rather than per sample.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct SystemInfo {
    pub hostname: Option<String>,
    pub kernel: Option<String>,
    pub os_name: Option<String>,
    pub os_version: Option<String>,
}

impl SystemInfo {
    pub fn collect() -> Self {
        SystemInfo {
            hostname: System::host_name(),
            kernel: System::kernel_version(),
            os_name: System::name(),
            os_version: System::os_version(),
        }
    }
}

/// Stateful sampler. Lives on the ticker thread; holds one reused instance of
/// each sysinfo collector plus the GPU probe and timing/caches for rates.
pub struct StatsSampler {
    sys: System,
    nets: Networks,
    disks: Disks,
    comps: Components,
    gpu: GpuProbe,
    disk_path: std::path::PathBuf,
    tick: u64,
    /// CPU usage needs a delta; the first sample only primes it.
    cpu_primed: bool,
    /// When the network counters were last read (for bytes/sec).
    prev_net: Instant,
    /// When disk IO counters were last read (for bytes/sec).
    prev_disk: Instant,
    /// Cached slow-tier results, reused between refreshes.
    last_disks: Vec<DiskInfo>,
    last_temps: Vec<(String, f32)>,
}

impl StatsSampler {
    /// `disk_path` is any path on the filesystem whose free-space % feeds the
    /// `disk` masthead widget (the worktrees dir).
    pub fn new(disk_path: std::path::PathBuf) -> Self {
        // Only the subsystems we read; processes are deliberately excluded.
        let sys = System::new_with_specifics(
            RefreshKind::nothing()
                .with_cpu(CpuRefreshKind::nothing().with_cpu_usage().with_frequency())
                .with_memory(MemoryRefreshKind::everything()),
        );
        let now = Instant::now();
        StatsSampler {
            sys,
            nets: Networks::new_with_refreshed_list(),
            disks: Disks::new_with_refreshed_list(),
            comps: Components::new_with_refreshed_list(),
            gpu: GpuProbe::probe(),
            disk_path,
            tick: 0,
            cpu_primed: false,
            prev_net: now,
            prev_disk: now,
            last_disks: Vec::new(),
            last_temps: Vec::new(),
        }
    }

    /// Take one reading (blocking refreshes — ticker-thread only).
    pub fn sample(&mut self) -> StatsSnapshot {
        let mut snap = StatsSnapshot::default();
        let now = Instant::now();
        let slow = self.tick.is_multiple_of(SLOW_EVERY);

        // --- CPU (every tick; needs a delta, so the first sample primes it) ---
        self.sys.refresh_cpu_usage();
        if self.cpu_primed {
            snap.cpu_pct = Some(pct_u8(self.sys.global_cpu_usage()));
            snap.cpu_cores = self
                .sys
                .cpus()
                .iter()
                .map(|c| pct_u8(c.cpu_usage()))
                .collect();
            let freqs: Vec<u64> = self
                .sys
                .cpus()
                .iter()
                .map(|c| c.frequency())
                .filter(|f| *f > 0)
                .collect();
            if !freqs.is_empty() {
                snap.cpu_freq_mhz = Some(freqs.iter().sum::<u64>() / freqs.len() as u64);
            }
        } else {
            self.cpu_primed = true;
        }

        // --- Memory + swap (every tick) ---
        self.sys.refresh_memory();
        let gib = |b: u64| b as f32 / (1024.0 * 1024.0 * 1024.0);
        let total = self.sys.total_memory();
        if total > 0 {
            snap.mem_gib = Some((gib(self.sys.used_memory()), gib(total)));
        }
        let swap_total = self.sys.total_swap();
        if swap_total > 0 {
            snap.swap_gib = Some((gib(self.sys.used_swap()), gib(swap_total)));
        }

        // --- Network (every tick): bytes/sec since the previous read ---
        self.nets.refresh(false);
        let dt_net = now.duration_since(self.prev_net).as_secs_f64().max(0.001);
        self.prev_net = now;
        let mut sum_rx = 0u64;
        let mut sum_tx = 0u64;
        for (name, data) in self.nets.iter() {
            if name == "lo" || name.starts_with("lo") {
                continue;
            }
            let rx = (data.received() as f64 / dt_net) as u64;
            let tx = (data.transmitted() as f64 / dt_net) as u64;
            sum_rx += rx;
            sum_tx += tx;
            if rx > 0 || tx > 0 {
                snap.net_ifaces.push((name.to_string(), rx, tx));
            }
        }
        snap.net_bps = Some((sum_rx, sum_tx));

        // --- Battery + GPU + disk-free (every tick; all cheap) ---
        let psu = std::path::Path::new("/sys/class/power_supply");
        snap.battery = read_battery(psu);
        (snap.battery_power_w, snap.battery_eta_secs) = read_battery_power(psu);
        let gpu = self.gpu.read();
        snap.gpu_pct = gpu.util_pct;
        snap.gpu_mem_mib = gpu.mem_mib;
        snap.gpu_temp_c = gpu.temp_c;
        snap.gpu_power_w = gpu.power_w;
        if let Some((total, avail, pct)) = disk_space(&self.disk_path) {
            snap.disk_free_pct = Some(pct);
            snap.disk_bytes = Some((total, avail));
        }

        // --- Slow tier (every SLOW_EVERY-th tick): frequency, temps, disks ---
        if slow {
            self.comps.refresh(false);
            self.last_temps = self
                .comps
                .iter()
                .filter_map(|c| {
                    c.temperature()
                        .filter(|t| t.is_finite())
                        .map(|t| (c.label().to_string(), t))
                })
                .collect();

            let dt_disk = now.duration_since(self.prev_disk).as_secs_f64().max(0.001);
            self.prev_disk = now;
            self.disks.refresh(false);
            self.last_disks = self
                .disks
                .iter()
                .map(|d| {
                    let total = d.total_space();
                    let free_pct = if total > 0 {
                        ((d.available_space() as f64 / total as f64) * 100.0).round() as u8
                    } else {
                        0
                    };
                    let usage = d.usage();
                    DiskInfo {
                        name: d.name().to_string_lossy().into_owned(),
                        mount: d.mount_point().to_string_lossy().into_owned(),
                        free_pct: free_pct.min(100),
                        read_bps: (usage.read_bytes as f64 / dt_disk) as u64,
                        write_bps: (usage.written_bytes as f64 / dt_disk) as u64,
                        kind: match d.kind() {
                            sysinfo::DiskKind::HDD => DiskKind::Hdd,
                            sysinfo::DiskKind::SSD => DiskKind::Ssd,
                            _ => DiskKind::Unknown,
                        },
                    }
                })
                .collect();
        }
        snap.disks = self.last_disks.clone();
        snap.temps = self.last_temps.clone();
        snap.cpu_temp_c = cpu_temp(&snap.temps);

        // --- Load average + uptime (every tick; cheap) ---
        #[cfg(unix)]
        {
            let la = System::load_average();
            snap.load_avg = Some((la.one as f32, la.five as f32, la.fifteen as f32));
        }
        snap.uptime_secs = Some(System::uptime());

        self.tick = self.tick.wrapping_add(1);
        snap
    }
}

/// Round an f32 percentage into a clamped 0–100 byte.
fn pct_u8(v: f32) -> u8 {
    v.round().clamp(0.0, 100.0) as u8
}

/// Pick the CPU/package temperature from labelled sensors: the hottest sensor
/// whose label looks CPU-ish, else the hottest sensor overall.
fn cpu_temp(temps: &[(String, f32)]) -> Option<f32> {
    const CPUISH: [&str; 6] = ["cpu", "package", "tctl", "core", "coretemp", "k10temp"];
    let hottest = |it: &mut dyn Iterator<Item = &(String, f32)>| {
        it.map(|(_, t)| *t)
            .filter(|t| t.is_finite())
            .fold(None::<f32>, |acc, t| Some(acc.map_or(t, |a| a.max(t))))
    };
    let cpuish = hottest(&mut temps.iter().filter(|(l, _)| {
        let l = l.to_ascii_lowercase();
        CPUISH.iter().any(|k| l.contains(k))
    }));
    cpuish.or_else(|| hottest(&mut temps.iter()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cpu_temp_prefers_cpuish_then_max() {
        let temps = vec![
            ("acpitz".into(), 40.0),
            ("Package id 0".into(), 55.0),
            ("Core 0".into(), 52.0),
        ];
        assert_eq!(cpu_temp(&temps), Some(55.0)); // hottest cpu-ish
        // No cpu-ish label → overall hottest.
        let other = vec![("nvme".into(), 38.0), ("acpitz".into(), 44.0)];
        assert_eq!(cpu_temp(&other), Some(44.0));
        assert_eq!(cpu_temp(&[]), None);
    }
}
