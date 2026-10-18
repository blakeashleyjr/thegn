//! The sampler: one reused `sysinfo` instance set, refreshed selectively and in
//! tiers on the host's ticker thread. Cheap metrics (CPU/mem/net) refresh every
//! tick; slow-moving ones (frequency, temperatures, disk enumeration + IO)
//! refresh every [`SLOW_EVERY`]-th tick and are cached in between. Processes are
//! never enumerated — that is sysinfo's expensive path and we don't need it.

use std::time::Instant;

#[cfg(not(target_os = "linux"))]
use sysinfo::Components;
#[cfg(not(target_os = "linux"))]
use sysinfo::Disks;
#[cfg(not(target_os = "linux"))]
use sysinfo::Networks;
use sysinfo::{MemoryRefreshKind, Pid, ProcessRefreshKind, ProcessesToUpdate, RefreshKind, System};

#[cfg(not(target_os = "linux"))]
use crate::DiskKind;
use crate::gpu_monitor::GpuMonitor;
use crate::thermal::ThermalProbe;
use crate::{ChildGroup, DiskInfo, NetMedium, StatsSnapshot, disk_space};

/// Refresh the slow tier (frequency / temperatures / disks) once every N
/// samples. At the host's default ~1s cadence that is roughly every 5s — these
/// move slowly and the enumeration is the most expensive part of a sample.
const SLOW_EVERY: u64 = 5;

/// Keep sysinfo's base refresh set aligned with the platform-specific readers.
/// Linux's base CPU and memory readers supply those values; other platforms
/// continue to use sysinfo for CPU and memory.
fn refresh_kind_for_platform(is_linux: bool) -> RefreshKind {
    if is_linux {
        RefreshKind::nothing()
    } else {
        RefreshKind::nothing()
            .with_cpu(crate::cpu_freq::cpu_refresh_kind())
            .with_memory(MemoryRefreshKind::everything())
    }
}

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
    #[cfg(not(target_os = "linux"))]
    nets: Networks,
    #[cfg(not(target_os = "linux"))]
    disks: Disks,
    #[cfg(target_os = "linux")]
    disk_aggregate: crate::disk_aggregate::DiskAggregate,
    #[cfg(not(target_os = "linux"))]
    comps: Components,
    gpu: GpuMonitor,
    /// How temperatures are read. `Components` on Linux/Intel; the Apple-vendor
    /// HID sensors on Apple silicon, where `Components` is empty.
    thermal: ThermalProbe,
    #[cfg(target_os = "linux")]
    base_temp: crate::base_temp::BaseTemp,
    battery: crate::battery::BatteryReader,
    power_present: bool,
    disk_path: std::path::PathBuf,
    tick: u64,
    /// CPU usage needs a delta; the first sample only primes it.
    cpu_primed: bool,
    cpu_freq: crate::cpu_freq::CpuFreqProbe,
    last_cpu_freq: Option<u64>,
    #[cfg(target_os = "linux")]
    base_cpu: crate::base_cpu::BaseCpu,
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
    #[cfg(not(target_os = "linux"))]
    prev_disk: Instant,
    /// Previous `pgsteal_direct` reading and when, for the direct-reclaim RATE.
    /// `None` until the first sample — a monotonic counter cannot yield a rate
    /// without a predecessor, and reporting the raw total would be meaningless.
    prev_reclaim: Option<(u64, Instant)>,
    vmstat: crate::vmstat::VmstatReader,
    #[cfg(target_os = "linux")]
    base_mem: crate::base_mem::BaseMem,
    /// Cached slow-tier results, reused between refreshes.
    last_disks: std::sync::Arc<[DiskInfo]>,
    #[cfg(not(target_os = "linux"))]
    last_disk_io: Option<(u64, u64)>,
    last_temps: Vec<(String, f32)>,
    last_net_medium: NetMedium,
    net_classes: std::collections::BTreeMap<String, crate::net_kind::IfaceClass>,
    net_up: std::collections::BTreeMap<String, bool>,
    net_probed: bool,
    #[cfg(target_os = "linux")]
    base_net: crate::base_net::BaseNet,
}

/// Parse Linux `/proc/net/route`, returning every interface tied for the
/// lowest-metric usable IPv4 default route. Ties remain ambiguous downstream.
#[cfg(any(target_os = "linux", test))]
pub(crate) fn parse_default_route(contents: &str) -> Vec<String> {
    let mut candidates: Vec<(u64, String)> = contents
        .lines()
        .skip(1)
        .filter_map(|line| {
            let mut fields = line.split_whitespace();
            let iface = fields.next()?;
            let destination = fields.next()?;
            let _gateway = fields.next()?;
            let flags = u32::from_str_radix(fields.next()?, 16).ok()?;
            let _refcnt = fields.next()?;
            let _use = fields.next()?;
            let metric = fields.next()?.parse::<u64>().ok()?;
            (destination == "00000000" && flags & 1 != 0).then(|| (metric, iface.to_string()))
        })
        .collect();
    let Some(min_metric) = candidates.iter().map(|(metric, _)| *metric).min() else {
        return Vec::new();
    };
    candidates.retain(|(metric, _)| *metric == min_metric);
    candidates.sort_by(|a, b| a.1.cmp(&b.1));
    candidates.dedup_by(|a, b| a.1 == b.1);
    candidates.into_iter().map(|(_, iface)| iface).collect()
}

#[cfg(target_os = "linux")]
pub fn probe_net_path() -> crate::MediumSelection {
    let mut cache = std::collections::BTreeMap::new();
    probe_net_links(&mut cache).0
}

#[cfg(target_os = "linux")]
fn probe_net_links(
    cache: &mut std::collections::BTreeMap<String, crate::net_kind::IfaceClass>,
) -> (
    crate::MediumSelection,
    std::collections::BTreeMap<String, bool>,
) {
    match std::fs::read_to_string("/proc/net/route") {
        Ok(routes) => {
            let routes = parse_default_route(&routes);
            crate::net_kind::probe_links_at(std::path::Path::new("/sys/class/net"), &routes, cache)
        }
        Err(_) => (
            crate::MediumSelection::probe_unavailable(),
            std::collections::BTreeMap::new(),
        ),
    }
}

#[cfg(not(target_os = "linux"))]
fn probe_net_links(
    _cache: &mut std::collections::BTreeMap<String, crate::net_kind::IfaceClass>,
) -> (
    crate::MediumSelection,
    std::collections::BTreeMap<String, bool>,
) {
    (
        crate::MediumSelection::probe_unavailable(),
        std::collections::BTreeMap::new(),
    )
}

#[cfg(not(target_os = "linux"))]
pub fn probe_net_path() -> crate::MediumSelection {
    crate::MediumSelection::probe_unavailable()
}

impl StatsSampler {
    /// `disk_path` is any path on the filesystem whose free-space % feeds the
    /// `disk` masthead widget (the worktrees dir).
    pub fn new(disk_path: std::path::PathBuf) -> Self {
        // Only the subsystems we read; processes are deliberately excluded.
        let sys = System::new_with_specifics(refresh_kind_for_platform(cfg!(target_os = "linux")));
        let now = Instant::now();
        StatsSampler {
            sys,
            #[cfg(not(target_os = "linux"))]
            nets: Networks::new_with_refreshed_list(),
            #[cfg(not(target_os = "linux"))]
            disks: Disks::new_with_refreshed_list(),
            #[cfg(target_os = "linux")]
            disk_aggregate: crate::disk_aggregate::DiskAggregate::new(),
            #[cfg(not(target_os = "linux"))]
            comps: Components::new_with_refreshed_list(),
            gpu: GpuMonitor::new(),
            thermal: ThermalProbe::probe(),
            #[cfg(target_os = "linux")]
            base_temp: crate::base_temp::BaseTemp::new(),
            battery: crate::battery::BatteryReader::new(std::path::PathBuf::from(
                "/sys/class/power_supply",
            )),
            power_present: crate::power_presence::probe(std::path::Path::new("/sys")),
            disk_path,
            tick: 0,
            cpu_primed: false,
            cpu_freq: crate::cpu_freq::CpuFreqProbe::discover(std::path::Path::new(
                "/sys/devices/system/cpu",
            )),
            last_cpu_freq: None,
            #[cfg(target_os = "linux")]
            base_cpu: crate::base_cpu::BaseCpu::new(),
            self_pid: Pid::from_u32(std::process::id()),
            daemon_pid: None,
            proc_primed: false,
            tracked: Vec::new(),
            prev_net: now,
            #[cfg(not(target_os = "linux"))]
            prev_disk: now,
            prev_reclaim: None,
            vmstat: crate::vmstat::VmstatReader::new(),
            #[cfg(target_os = "linux")]
            base_mem: crate::base_mem::BaseMem::new(),
            last_disks: std::sync::Arc::from([]),
            #[cfg(not(target_os = "linux"))]
            last_disk_io: None,
            last_temps: Vec::new(),
            last_net_medium: NetMedium::Unknown,
            net_classes: std::collections::BTreeMap::new(),
            net_up: std::collections::BTreeMap::new(),
            net_probed: false,
            #[cfg(target_os = "linux")]
            base_net: crate::base_net::BaseNet::new(),
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

    fn refresh_net_links(
        &mut self,
        slow: bool,
        probe: impl FnOnce(
            &mut std::collections::BTreeMap<String, crate::net_kind::IfaceClass>,
        ) -> (
            crate::MediumSelection,
            std::collections::BTreeMap<String, bool>,
        ),
    ) {
        if slow || !self.net_probed {
            let (network, up) = probe(&mut self.net_classes);
            self.last_net_medium = network.medium;
            self.net_up = up;
            self.net_probed = true;
        }
    }

    /// Take one reading (blocking refreshes — ticker-thread only).
    pub fn sample(&mut self) -> StatsSnapshot {
        let mut snap = StatsSnapshot {
            power_present: self.power_present,
            ..StatsSnapshot::default()
        };
        let now = Instant::now();
        let slow = self.tick.is_multiple_of(SLOW_EVERY);

        // --- CPU (every tick; needs a delta, so the first sample primes it) ---
        if slow {
            self.last_cpu_freq = self.cpu_freq.read_mhz(&mut self.sys);
        }
        #[cfg(target_os = "linux")]
        {
            let (cpu_pct, cpu_cores) = self.base_cpu.fill(&mut self.cpu_primed);
            snap.cpu_pct = cpu_pct.map(pct_u8);
            snap.cpu_cores = cpu_cores.into_iter().map(pct_u8).collect();
        }
        #[cfg(not(target_os = "linux"))]
        {
            self.sys.refresh_cpu_usage();
            if self.cpu_primed {
                snap.cpu_pct = Some(pct_u8(self.sys.global_cpu_usage()));
                snap.cpu_cores = self
                    .sys
                    .cpus()
                    .iter()
                    .map(|c| pct_u8(c.cpu_usage()))
                    .collect();
            } else {
                self.cpu_primed = true;
            }
        }
        snap.cpu_freq_mhz = self.last_cpu_freq;

        // --- Memory + swap (every tick) ---
        let gib = |b: u64| b as f32 / (1024.0 * 1024.0 * 1024.0);
        #[cfg(target_os = "linux")]
        {
            let reading = self.base_mem.fill();
            if let Some((used, total)) = crate::base_mem::nonzero_total(reading.mem) {
                snap.mem_gib = Some((gib(used), gib(total)));
                snap.mem_free_gib = reading.free.map(gib);
            }
            if let Some((used, total)) = crate::base_mem::nonzero_total(reading.swap) {
                snap.swap_gib = Some((gib(used), gib(total)));
            }
        }
        #[cfg(not(target_os = "linux"))]
        {
            self.sys.refresh_memory();
            let total = self.sys.total_memory();
            if total > 0 {
                snap.mem_gib = Some((gib(self.sys.used_memory()), gib(total)));
                snap.mem_free_gib = Some(gib(self.sys.free_memory()));
            }
            let swap_total = self.sys.total_swap();
            if swap_total > 0 {
                snap.swap_gib = Some((gib(self.sys.used_swap()), gib(swap_total)));
            }
        }

        // --- Direct reclaim (every tick, Linux only) ---
        // Swap *occupancy* is a lagging proxy; this is the thing that actually
        // stalls the machine. Reported as a rate because the counter is
        // monotonic since boot. The first sample only primes: no predecessor
        // means no rate, and an absent signal must stay absent rather than
        // become a comfortable zero.
        if let Some(cur) = self.vmstat.pgsteal_direct() {
            if let Some((prev, at)) = self.prev_reclaim {
                let dt = now.duration_since(at).as_secs_f32().max(0.001);
                snap.reclaim_per_s = Some(cur.saturating_sub(prev) as f32 / dt);
            }
            self.prev_reclaim = Some((cur, now));
        }

        // --- Network (every tick): bytes/sec since the previous read ---
        let dt_net = now.duration_since(self.prev_net).as_secs_f64().max(0.001);
        self.prev_net = now;
        self.refresh_net_links(slow, probe_net_links);
        let network = {
            #[cfg(target_os = "linux")]
            {
                self.base_net.rates(dt_net, slow).map(|rates| {
                    crate::net_kind::aggregate(
                        rates.iter().map(|(name, rx, tx)| (name.as_str(), *rx, *tx)),
                        &self.net_classes,
                        &self.net_up,
                        cfg!(target_os = "linux"),
                    )
                })
            }
            #[cfg(not(target_os = "linux"))]
            {
                self.nets.refresh(false);
                Some(crate::net_kind::aggregate(
                    self.nets.iter().map(|(name, data)| {
                        (
                            name.as_str(),
                            (data.received() as f64 / dt_net) as u64,
                            (data.transmitted() as f64 / dt_net) as u64,
                        )
                    }),
                    &self.net_classes,
                    &self.net_up,
                    cfg!(target_os = "linux"),
                ))
            }
        };
        if let Some((ifaces, physical, all)) = network {
            snap.net_ifaces = ifaces;
            snap.net_bps = Some(physical);
            snap.net_bps_all = Some(all);
        }

        // --- Battery + GPU + disk-free ---
        // Battery and disk-free are cheap file reads every tick. GPU depends on
        // the backend: sysfs is two file reads, but `nvidia-smi` and `ioreg` are
        // process spawns (ioreg measured at 30-40ms), and paying that every ~2s
        // tick is real background CPU against the ~0%-idle invariant. Charge
        // those to the slow tier, run off-thread and bounded by `GpuMonitor`, which
        // serves the latest good reading: a wedged helper cannot stall this
        // shared ticker. (The
        // subprocess cost predates macOS: the nvidia arm has always spawned one
        // per tick.)
        let battery = self.battery.sample(now);
        snap.battery = battery.battery;
        snap.battery_power_w = battery.power_w;
        snap.battery_eta_secs = battery.eta_secs;
        let gpu = self.gpu.sample(slow);
        snap.gpu_pct = gpu.util_pct;
        snap.gpu_mem_mib = gpu.mem_mib;
        snap.gpu_mem_shared = gpu.mem_shared;
        snap.gpu_temp_c = gpu.temp_c;
        snap.gpu_power_w = gpu.power_w;
        if let Some((total, avail, pct)) = disk_space(&self.disk_path) {
            snap.disk_free_pct = Some(pct);
            snap.disk_bytes = Some((total, avail));
        }

        // --- Slow tier (every SLOW_EVERY-th tick): frequency, temps, disks ---
        if slow {
            // Linux uses held hwmon reads; other platforms retain sysinfo's
            // components, with Apple silicon's HID reader where available.
            #[cfg(target_os = "linux")]
            {
                self.last_temps = if matches!(self.thermal, ThermalProbe::AppleHid) {
                    self.thermal.read()
                } else {
                    self.base_temp.fill()
                };
            }
            #[cfg(not(target_os = "linux"))]
            {
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
            }

            #[cfg(not(target_os = "linux"))]
            let dt_disk = now.duration_since(self.prev_disk).as_secs_f64().max(0.001);
            #[cfg(not(target_os = "linux"))]
            {
                self.prev_disk = now;
            }
            #[cfg(not(target_os = "linux"))]
            {
                self.disks.refresh(false);
                self.last_disks = crate::disk_aggregate::dedupe_by_name(
                    self.disks
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
                                extra_mounts: 0,
                            }
                        })
                        .collect(),
                )
                .into();
            }
            #[cfg(not(target_os = "linux"))]
            {
                self.last_disk_io = if self.last_disks.is_empty() {
                    None
                } else {
                    Some(self.last_disks.iter().fold((0u64, 0u64), |(r, w), d| {
                        (r.saturating_add(d.read_bps), w.saturating_add(d.write_bps))
                    }))
                };
            }
        }
        #[cfg(target_os = "linux")]
        {
            (snap.disk_io, self.last_disks) = self.disk_aggregate.sample(slow, now);
        }
        #[cfg(not(target_os = "linux"))]
        {
            snap.disk_io = self.last_disk_io;
        }
        snap.disks = std::sync::Arc::clone(&self.last_disks);
        snap.net_medium = self.last_net_medium;
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
                true, // remove requested PIDs that exited since the last sample
                // nothing() keeps tasks on; we never read them.
                ProcessRefreshKind::nothing()
                    .with_cpu()
                    .with_memory()
                    .without_tasks(),
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
pub(crate) fn cpu_temp(temps: &[(String, f32)]) -> Option<f32> {
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

    struct ChildGuard(std::process::Child);

    impl Drop for ChildGuard {
        fn drop(&mut self) {
            match self.0.kill() {
                Ok(()) | Err(_) => {}
            }
            match self.0.wait() {
                Ok(_) | Err(_) => {}
            }
        }
    }

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
    fn refresh_kind_tracks_platform_base_readers() {
        for (is_linux, expects_cpu_memory) in [(true, false), (false, true)] {
            let kind = refresh_kind_for_platform(is_linux);
            assert_eq!(kind.cpu().is_some(), expects_cpu_memory);
            assert_eq!(kind.memory().is_some(), expects_cpu_memory);
        }
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn linux_sampler_starts_without_sysinfo_cpu_or_memory_state() {
        let sampler = StatsSampler::new(std::env::temp_dir());
        assert!(sampler.sys.cpus().is_empty());
        assert_eq!(sampler.sys.total_memory(), 0);
    }

    #[test]
    fn default_route_parser_selects_lowest_up_route_and_keeps_metric_ties() {
        let routes = "Iface Destination Gateway Flags RefCnt Use Metric Mask MTU Window IRTT\n\
                      wlan0 00000000 0102A8C0 0003 0 0 50 00000000 0 0 0\n\
                      enp0s1 00000000 0102A8C0 0001 0 0 10 00000000 0 0 0\n\
                      tun0 00000000 0102A8C0 0000 0 0 1 00000000 0 0 0\n\
                      enp2s0 00000000 0102A8C0 0001 0 0 10 00000000 0 0 0\n\
                      lo 00000001 00000000 0003 0 0 0 00000000 0 0 0";
        assert_eq!(
            parse_default_route(routes),
            vec!["enp0s1".to_string(), "enp2s0".to_string()]
        );
        assert!(
            parse_default_route("Iface Destination Gateway Flags RefCnt Use Metric\n").is_empty()
        );
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
    fn reaped_tracked_process_is_pruned_and_its_proc_fd_is_released() {
        // This exercises sysinfo's Linux /proc handle cache. Other platforms
        // have no /proc fd links to inspect, and the targeted refresh behavior
        // is already covered by the platform-independent sampler tests.
        if !cfg!(target_os = "linux") {
            return;
        }
        let fd_dir = std::path::Path::new("/proc/self/fd");

        let mut child = ChildGuard(
            std::process::Command::new("sh")
                .args(["-c", "exec sleep 30"])
                .spawn()
                .expect("spawn short-lived tracked process"),
        );
        let pid = child.0.id();
        let mut sampler = StatsSampler::new(std::env::temp_dir());
        sampler.set_tracked(vec![spec(pid, "test-child")]);
        let first = sampler.sample();
        let first_has_child = first
            .children
            .iter()
            .any(|group| group.group == "test-child");

        let stat_path = format!("/proc/{pid}/stat");
        let cached_fds = || {
            std::fs::read_dir(fd_dir)
                .expect("read own fd directory")
                .filter_map(|entry| match entry {
                    Ok(entry) => Result::ok(std::fs::read_link(entry.path())),
                    Err(_) => None,
                })
                .filter(|target| target == std::path::Path::new(&stat_path))
                .count()
        };
        let process_was_cached = sampler.sys.process(Pid::from_u32(pid)).is_some();
        let cached_fd_count = cached_fds();

        child.0.kill().expect("kill tracked process");
        child.0.wait().expect("reap tracked process");
        let mut after = None;
        for _ in 0..3 {
            std::thread::sleep(sysinfo::MINIMUM_CPU_UPDATE_INTERVAL);
            sampler.set_tracked(vec![spec(pid, "test-child")]);
            after = Some(sampler.sample());
        }
        let after = after.expect("three post-exit samples were taken");

        assert!(
            first_has_child,
            "the live child should appear in the rollup"
        );
        assert!(process_was_cached, "sysinfo should track the live child");
        assert!(
            cached_fd_count > 0,
            "sysinfo should cache the child's stat fd"
        );
        assert!(
            !after
                .children
                .iter()
                .any(|group| group.group == "test-child"),
            "an exited child must disappear from the snapshot"
        );
        assert!(
            sampler.tracked == vec![spec(pid, "test-child")],
            "the sampler must preserve the host's tracked-process mirror"
        );
        assert!(
            sampler.sys.process(Pid::from_u32(pid)).is_none(),
            "sysinfo must drop the exited process entry"
        );
        assert_eq!(cached_fds(), 0, "the cached /proc stat fd must be closed");
        assert!(
            after.self_cpu_pct.is_some(),
            "repeated host registration must not reset live process CPU deltas"
        );
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

    #[test]
    fn tracked_refresh_skips_tasks_but_keeps_cpu_and_memory() {
        if !cfg!(target_os = "linux") {
            return;
        }

        let (ready_tx, ready_rx) = std::sync::mpsc::channel();
        let helper = std::thread::spawn(move || {
            ready_tx.send(()).unwrap();
            std::thread::park();
        });
        ready_rx.recv().unwrap();

        let mut sampler = StatsSampler::new(std::env::temp_dir());
        sampler.sample();
        sampler.sample();
        let pids = refresh_pid_list(sampler.self_pid, sampler.daemon_pid, &sampler.tracked);

        helper.thread().unpark();
        helper.join().unwrap();

        assert!(
            sampler.sys.processes().len() <= pids.len(),
            "tracked refresh retained tasks or unrelated processes: {} entries for {} PIDs",
            sampler.sys.processes().len(),
            pids.len()
        );
    }

    #[test]
    fn net_probe_runs_once_per_slow_period_with_no_interfaces() {
        let dir = tempfile::tempdir().unwrap();
        let mut sampler = StatsSampler::new(dir.path().to_path_buf());
        let mut probes = 0;
        for tick in 0..(3 * SLOW_EVERY) {
            sampler.refresh_net_links(tick.is_multiple_of(SLOW_EVERY), |_| {
                probes += 1;
                (
                    crate::MediumSelection::probe_unavailable(),
                    std::collections::BTreeMap::new(),
                )
            });
        }
        assert_eq!(probes, 3);
    }

    #[test]
    fn net_links_are_classified_on_the_first_call() {
        let dir = tempfile::tempdir().unwrap();
        let mut sampler = StatsSampler::new(dir.path().to_path_buf());
        let mut probes = 0;
        sampler.refresh_net_links(false, |classes| {
            probes += 1;
            classes.insert(
                "eth0".to_owned(),
                crate::net_kind::IfaceClass {
                    kind: crate::net_kind::IfaceKind::Ethernet,
                    bond_slave: false,
                    has_device: true,
                    medium: NetMedium::Ethernet,
                },
            );
            (
                crate::MediumSelection::probe_unavailable(),
                std::collections::BTreeMap::from([("eth0".to_owned(), true)]),
            )
        });
        assert_eq!(probes, 1);
        assert!(!sampler.net_classes.is_empty());
    }
}
