//! The sampler: one reused `sysinfo` instance set, refreshed selectively and in
//! tiers on the host's ticker thread. Cheap metrics (CPU/mem/net) refresh every
//! tick; slow-moving ones (frequency, temperatures, disk enumeration + IO)
//! refresh every [`SLOW_EVERY`]-th tick and are cached in between. Processes are
//! never enumerated — that is sysinfo's expensive path and we don't need it.

use std::time::Instant;

use sysinfo::{
    Components, CpuRefreshKind, Disks, MemoryRefreshKind, Networks, Pid, ProcessRefreshKind,
    ProcessesToUpdate, RefreshKind, System,
};

use crate::gpu::GpuProbe;
use crate::thermal::ThermalProbe;
use crate::{
    ChildGroup, DiskInfo, DiskKind, StatsSnapshot, disk_space, read_battery, read_battery_power,
};

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
    /// How temperatures are read. `Components` on Linux/Intel; the Apple-vendor
    /// HID sensors on Apple silicon, where `Components` is empty.
    thermal: ThermalProbe,
    /// Last GPU sample, for the subprocess-backed probes that only refresh on
    /// the slow tier (see the read site). `Sysfs` never uses this.
    last_gpu: crate::gpu::GpuReading,
    disk_path: std::path::PathBuf,
    tick: u64,
    /// CPU usage needs a delta; the first sample only primes it.
    cpu_primed: bool,
    /// This process's PID — always watched for the daemon/status modal.
    self_pid: Pid,
    /// The pane-daemon's PID once the host resolves it (`None` = unknown).
    daemon_pid: Option<Pid>,
    /// Per-process CPU usage also needs a delta; primed on the first refresh of
    /// the watched set (reset when the daemon PID changes).
    proc_primed: bool,
    /// Child processes registered as thegn's own (language servers, plugin
    /// hosts, watchers) — see `thegn_core::proc_registry`. The host refreshes
    /// this each tick via [`StatsSampler::set_tracked`]; the sampler itself has
    /// no opinion about what belongs, which is what lets a new producer be
    /// accounted for without changing this crate.
    tracked: Vec<TrackedSpec>,
    /// When the network counters were last read (for bytes/sec).
    prev_net: Instant,
    /// When disk IO counters were last read (for bytes/sec).
    prev_disk: Instant,
    /// Previous `pgsteal_direct` reading and when, for the direct-reclaim RATE.
    /// `None` until the first sample — a monotonic counter cannot yield a rate
    /// without a predecessor, and reporting the raw total would be meaningless.
    prev_reclaim: Option<(u64, Instant)>,
    /// Cached slow-tier results, reused between refreshes.
    last_disks: Vec<DiskInfo>,
    last_temps: Vec<(String, f32)>,
}

/// `pgsteal_direct` from `/proc/vmstat` — pages reclaimed synchronously since
/// boot. `None` off Linux, or if the field is absent (it has moved between
/// kernel versions, so a missing field degrades to "not observed" rather than
/// to zero).
#[cfg(target_os = "linux")]
fn read_pgsteal_direct() -> Option<u64> {
    let s = std::fs::read_to_string("/proc/vmstat").ok()?;
    s.lines()
        .find_map(|l| l.strip_prefix("pgsteal_direct "))
        .and_then(|v| v.trim().parse().ok())
}

/// No `/proc/vmstat` here; the metric is reported as unobserved.
#[cfg(not(target_os = "linux"))]
fn read_pgsteal_direct() -> Option<u64> {
    None
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
            thermal: ThermalProbe::probe(),
            last_gpu: crate::gpu::GpuReading::default(),
            disk_path,
            tick: 0,
            cpu_primed: false,
            self_pid: Pid::from_u32(std::process::id()),
            daemon_pid: None,
            proc_primed: false,
            tracked: Vec::new(),
            prev_net: now,
            prev_disk: now,
            prev_reclaim: None,
            last_disks: Vec::new(),
            last_temps: Vec::new(),
        }
    }

    /// Point the per-process sampler at the pane-daemon PID (`None` = unknown /
    /// no daemon). Changing it re-primes the CPU delta so the next reading is
    /// clean. Cheap; the host calls it from the ticker when the daemon connects.
    pub fn set_daemon_pid(&mut self, pid: Option<u32>) {
        let next = pid.map(Pid::from_u32);
        if next != self.daemon_pid {
            self.daemon_pid = next;
            self.proc_primed = false;
        }
    }

    /// Point the sampler at the child processes registered as thegn's own
    /// (`thegn_core::proc_registry::tracked()`), as `(pid, group)` pairs.
    ///
    /// Called every tick — the set is expected to churn as language servers
    /// start and stop. Only a *change* re-primes the CPU delta, so a steady set
    /// keeps its history; without that guard a single server starting would
    /// blank every other child's CPU reading.
    ///
    /// Deliberately takes plain data rather than reading the registry itself:
    /// this crate stays dependency-lean (sysinfo only) and unit-testable with
    /// synthetic PIDs, and the policy about what counts as "thegn's own" lives
    /// in one place instead of two.
    pub fn set_tracked(&mut self, procs: Vec<TrackedSpec>) {
        if procs != self.tracked {
            self.tracked = procs;
            self.proc_primed = false;
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

        // --- Direct reclaim (every tick, Linux only) ---
        // Swap *occupancy* is a lagging proxy; this is the thing that actually
        // stalls the machine. Reported as a rate because the counter is
        // monotonic since boot. The first sample only primes: no predecessor
        // means no rate, and an absent signal must stay absent rather than
        // become a comfortable zero.
        if let Some(cur) = read_pgsteal_direct() {
            if let Some((prev, at)) = self.prev_reclaim {
                let dt = now.duration_since(at).as_secs_f32().max(0.001);
                snap.reclaim_per_s = Some(cur.saturating_sub(prev) as f32 / dt);
            }
            self.prev_reclaim = Some((cur, now));
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

        // --- Battery + GPU + disk-free ---
        // Battery and disk-free are cheap file reads every tick. GPU depends on
        // the backend: sysfs is two file reads, but `nvidia-smi` and `ioreg` are
        // process spawns (ioreg measured at 30-40ms), and paying that every ~2s
        // tick is real background CPU against the ~0%-idle invariant. Charge
        // those to the slow tier and reuse the cached reading in between — the
        // same treatment frequency/temps/disks already get below. (The
        // subprocess cost predates macOS: the nvidia arm has always spawned one
        // per tick.)
        let psu = std::path::Path::new("/sys/class/power_supply");
        snap.battery = read_battery(psu);
        (snap.battery_power_w, snap.battery_eta_secs) = read_battery_power(psu);
        let gpu = if self.gpu.is_subprocess() {
            if slow {
                self.last_gpu = self.gpu.read();
            }
            self.last_gpu.clone()
        } else {
            self.gpu.read()
        };
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
            // Apple silicon publishes no SMC components, so `Components` comes
            // back empty there and the temperature row silently vanished. Ask
            // whichever backend the probe selected; `Components` stays the
            // answer on Linux and on Intel Macs.
            self.last_temps = match self.thermal {
                ThermalProbe::AppleHid => self.thermal.read(),
                _ => {
                    self.comps.refresh(false);
                    self.comps
                        .iter()
                        .filter_map(|c| {
                            c.temperature()
                                .filter(|t| t.is_finite())
                                .map(|t| (c.label().to_string(), t))
                        })
                        .collect()
                }
            };

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

        // --- Per-process footprint (thegn + the pane daemon) ---
        // Only the specific PIDs are refreshed — sysinfo's cheap targeted path,
        // not a full process enumeration. CPU is a delta, so the first refresh
        // of the set only primes it (mirrors `cpu_primed`).
        {
            let pids = refresh_pid_list(self.self_pid, self.daemon_pid, &self.tracked);
            self.sys.refresh_processes_specifics(
                ProcessesToUpdate::Some(&pids),
                false, // keep entries whose delta we still want next tick
                ProcessRefreshKind::nothing().with_cpu().with_memory(),
            );
            if let Some(p) = self.sys.process(self.self_pid) {
                snap.self_rss_bytes = Some(p.memory());
                if self.proc_primed {
                    snap.self_cpu_pct = Some(p.cpu_usage());
                }
            }
            if let Some(d) = self.daemon_pid
                && let Some(p) = self.sys.process(d)
            {
                snap.daemon_rss_bytes = Some(p.memory());
                if self.proc_primed {
                    snap.daemon_cpu_pct = Some(p.cpu_usage());
                }
            }
            // Registered children, rolled up per group. A PID sysinfo can no
            // longer find is simply gone — a registration is a hint, and a
            // handle can outlive a crashed process, so trusting the list over
            // the OS would invent memory that is not there.
            snap.children = roll_up_children(&self.sys, &self.tracked, self.proc_primed);
            self.proc_primed = true;
        }

        self.tick = self.tick.wrapping_add(1);
        snap
    }
}

/// The PID set to hand `refresh_processes_specifics`: us, plus the pane daemon
/// when it is a *different* process.
///
/// **The list must never repeat a PID.** sysinfo 0.39 fans the requested set
/// out across a rayon pool by list position rather than by PID, so a repeated
/// PID hands two worker threads the same `Process` entry at once. Both can
/// `take()` its cached `/proc/<pid>/stat` handle, and whichever loses closes a
/// file descriptor the other still owns. std catches that double close and
/// aborts the entire process:
///
/// ```text
/// fatal runtime error: IO Safety violation: owned file descriptor already closed
/// ```
///
/// Measured against sysinfo 0.39.5 in a debug build (the check is `#[inline]`
/// and gated on the calling crate's debug-assertions, so release builds hide
/// it): one PID listed twice aborted ~1% of refreshes, four times ~24%, eight
/// times ~78%; a list with no repeats never aborted in 200 runs.
///
/// A daemon PID equal to ours is the only way this list can repeat. That is
/// normally impossible — the daemon is a separate process, and only `thegn
/// daemon`/`thegn serve` write their own PID into the registry — but it is
/// reachable if a still-heartbeating row records a PID the OS has since
/// recycled onto us. Dropping the duplicate loses nothing: the daemon fields
/// are read back by PID from the refreshed map, which still holds that entry.
fn refresh_pid_list(self_pid: Pid, daemon_pid: Option<Pid>, tracked: &[TrackedSpec]) -> Vec<Pid> {
    let mut pids = vec![self_pid];
    if let Some(d) = daemon_pid
        && d != self_pid
    {
        pids.push(d);
    }
    pids.extend(tracked.iter().map(|t| Pid::from_u32(t.pid)));
    // MANDATORY, not tidiness. A repeated PID hands two rayon workers the same
    // `Process` entry, both close its cached `/proc` descriptor, and std aborts
    // the process over the double close. The two-PID list this function used to
    // build could be deduplicated by inspection; a registry fed by independent
    // subsystems cannot — two of them may legitimately name one shared server,
    // and one may name the daemon or the compositor itself.
    crate::procs::dedup_pids(pids)
}

/// Sum the registered children into one row per group.
///
/// Groups rather than one row per process, because the count is unbounded: a
/// user may run zero language servers or a hundred, and a status panel that
/// grows a row per server is unreadable at ten and unusable at a hundred. The
/// rolled-up row carries the count, so "lsp · 12 procs · 900 MB" stays one line
/// whatever the number.
///
/// Deduplicated by PID: two subsystems may register the same shared server, and
/// counting its memory twice would overstate the total. The first registration
/// wins the group, which is why `thegn_core::proc_registry`-order stability
/// matters. (Not an intra-doc link: this crate is a leaf and does not depend on
/// `thegn-core` — the registry lives on the other side of that boundary.)
fn roll_up_children(sys: &System, tracked: &[TrackedSpec], primed: bool) -> Vec<ChildGroup> {
    let mut seen: Vec<u32> = Vec::with_capacity(tracked.len());
    let mut out: Vec<ChildGroup> = Vec::new();
    for t in tracked {
        if seen.contains(&t.pid) {
            continue;
        }
        seen.push(t.pid);
        // Absent from sysinfo => exited (or never existed). Skip it entirely
        // rather than contributing a zero row, so a group whose processes have
        // all died disappears instead of lingering at 0 B.
        let Some(p) = sys.process(Pid::from_u32(t.pid)) else {
            continue;
        };
        let rss = p.memory();
        let cpu = if primed { p.cpu_usage() } else { 0.0 };
        match out.iter_mut().find(|g| g.group == t.group) {
            Some(g) => {
                g.count += 1;
                g.rss_bytes = g.rss_bytes.saturating_add(rss);
                g.cpu_pct += cpu;
            }
            None => out.push(ChildGroup {
                group: t.group.clone(),
                count: 1,
                rss_bytes: rss,
                cpu_pct: cpu,
            }),
        }
    }
    // Heaviest first: the row worth acting on should not be below the fold.
    out.sort_by(|a, b| b.rss_bytes.cmp(&a.rss_bytes).then(a.group.cmp(&b.group)));
    out
}

/// One child process the host wants accounted for: its PID and the group it
/// rolls up under (`"lsp"`, `"plugin"`, …).
///
/// A borrowed-free, sysinfo-free shape so `thegn-metrics` needs no dependency on
/// whatever crate defines the groups.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrackedSpec {
    pub pid: u32,
    pub group: String,
}

/// Round an f32 percentage into a clamped 0–100 byte.
fn pct_u8(v: f32) -> u8 {
    v.round().clamp(0.0, 100.0) as u8
}

/// Pick the CPU/package temperature from labelled sensors: the hottest sensor
/// whose label looks CPU-ish, else the hottest sensor overall.
fn cpu_temp(temps: &[(String, f32)]) -> Option<f32> {
    // `tdie` is Apple silicon's die-temperature sensor family ("PMU tdie1", …).
    // Without it the Apple path fell through to "hottest of anything", which
    // picks `PMU tcal` — a calibration reference that reads ~15C above the die
    // and is not a CPU temperature at all.
    const CPUISH: [&str; 7] = [
        "cpu", "package", "tctl", "core", "coretemp", "k10temp", "tdie",
    ];
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

    // A repeated PID makes sysinfo race two rayon workers onto one `Process`
    // and double-close its cached /proc handle, aborting the process. These
    // pin the dedup deterministically — the crash itself only shows up in
    // ~1% of refreshes, far too rare to guard a regression with.

    fn spec(pid: u32, group: &str) -> TrackedSpec {
        TrackedSpec {
            pid,
            group: group.to_string(),
        }
    }

    #[test]
    fn tracked_children_join_the_refresh_list_deduplicated() {
        // The registry is fed by independent subsystems, so it can name the
        // compositor, the daemon, or one shared server twice. Any duplicate
        // reaching `ProcessesToUpdate::Some` risks the double-close abort.
        let me = Pid::from_u32(1234);
        let daemon = Pid::from_u32(5678);
        let tracked = [
            spec(9001, "lsp"),
            spec(9001, "lsp"),    // same server, two registrants
            spec(5678, "plugin"), // a subsystem naming the daemon
            spec(1234, "tool"),   // ...or the compositor itself
        ];
        let got = refresh_pid_list(me, Some(daemon), &tracked);
        let mut want = vec![me, daemon, Pid::from_u32(9001)];
        want.sort_unstable();
        assert_eq!(got, want, "every pid appears exactly once");
    }

    #[test]
    fn zero_tracked_children_is_the_unchanged_two_pid_case() {
        // A user with no language servers, plugins or watchers must behave
        // exactly as before this registry existed.
        let me = Pid::from_u32(1);
        let daemon = Pid::from_u32(2);
        assert_eq!(refresh_pid_list(me, Some(daemon), &[]), vec![me, daemon]);
    }

    #[test]
    fn a_hundred_children_stay_a_handful_of_group_rows() {
        // Scale is the whole reason this rolls up: the UI must not grow a row
        // per language server. 100 processes across 3 groups is 3 rows.
        let tracked: Vec<TrackedSpec> = (0..100)
            .map(|i| spec(10_000 + i, ["lsp", "plugin", "watcher"][(i % 3) as usize]))
            .collect();
        let pids = refresh_pid_list(Pid::from_u32(1), None, &tracked);
        assert_eq!(pids.len(), 101, "self + 100 distinct children");
        // `roll_up_children` needs a live `System`; the grouping invariant it
        // guarantees is asserted against the real process in `lib.rs`. Here we
        // pin only that the PID list scales linearly and stays deduplicated.
        let mut sorted = pids.clone();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(sorted.len(), pids.len());
    }

    #[test]
    fn refresh_pid_list_drops_a_daemon_that_is_us() {
        let me = Pid::from_u32(1234);
        assert_eq!(refresh_pid_list(me, Some(me), &[]), vec![me]);
    }

    #[test]
    fn refresh_pid_list_keeps_a_distinct_daemon() {
        let me = Pid::from_u32(1234);
        let daemon = Pid::from_u32(5678);
        assert_eq!(refresh_pid_list(me, Some(daemon), &[]), vec![me, daemon]);
    }

    #[test]
    fn refresh_pid_list_is_just_us_without_a_daemon() {
        let me = Pid::from_u32(1234);
        assert_eq!(refresh_pid_list(me, None, &[]), vec![me]);
    }

    #[test]
    fn refresh_pid_list_never_repeats() {
        let me = Pid::from_u32(1234);
        for daemon in [None, Some(me), Some(Pid::from_u32(5678))] {
            let pids = refresh_pid_list(me, daemon, &[spec(5678, "lsp"), spec(1234, "tool")]);
            let mut sorted = pids.clone();
            sorted.sort_unstable();
            sorted.dedup();
            assert_eq!(sorted.len(), pids.len(), "duplicate PID in {pids:?}");
        }
    }

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

        // Apple silicon: the die sensors must win over `PMU tcal`, which is a
        // calibration reference reading ~15C hotter and is not a CPU temp.
        // Without "tdie" in CPUISH this fell through to "hottest of anything"
        // and reported 51.8 as the CPU temperature.
        let apple = vec![
            ("PMU tdie1".into(), 36.9),
            ("PMU tdie7".into(), 37.2),
            ("PMU tcal".into(), 51.8),
            ("NAND CH0 temp".into(), 29.0),
        ];
        assert_eq!(cpu_temp(&apple), Some(37.2));
    }
}
