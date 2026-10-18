//! F3a collector contract. Phase-0 readers absorbed by this seam: SR-2
//! (`cpu_freq.rs`), M-1 (`vmstat.rs`), D-1 (`disk_aggregate.rs`), N-1/N-5
//! (`net_kind.rs`), G-1/G-2 (`gpu_sysfs.rs`), and T-2 (`battery.rs`).
//! Consumers are the §2.3 system-monitor panels and future F3b registry.

pub mod delta;
pub mod env;
pub mod frame;
pub mod health;
pub mod instances;
#[cfg(target_os = "linux")]
pub mod linux;
pub mod model;
pub mod parse;
pub mod policy;
pub mod registry;
pub mod schedule;
pub mod stub;

use std::io;

use env::Env;
use frame::MonitorFrame;
use health::Health;
use schedule::Cadence;

/// Stable, append-only identity for a shared system reader.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum CollectorId {
    Loadavg,
    Uptime,
    Psi,
    CpuStat,
    CpuFreq,
    Hwmon,
    Cgroup,
    CgroupSlice,
    AmdSmu,
    Meminfo,
    Vmstat,
    Netdev,
    Diskstats,
    Powercap,
    PowerSupply,
    CpuIrq,
    CpuSched,
    ThermalZone,
    CpuIdle,
    CpuTopology,
    Filesystems,
    GpuDevices,
}

impl CollectorId {
    pub const ALL: [Self; 22] = [
        Self::Loadavg,
        Self::Uptime,
        Self::Psi,
        Self::CpuStat,
        Self::CpuFreq,
        Self::Hwmon,
        Self::Cgroup,
        Self::CgroupSlice,
        Self::AmdSmu,
        Self::Meminfo,
        Self::Vmstat,
        Self::Netdev,
        Self::Diskstats,
        Self::Powercap,
        Self::PowerSupply,
        Self::CpuIrq,
        Self::CpuSched,
        Self::ThermalZone,
        Self::CpuIdle,
        Self::CpuTopology,
        Self::Filesystems,
        Self::GpuDevices,
    ];

    pub const fn slug(self) -> &'static str {
        match self {
            Self::Loadavg => "loadavg",
            Self::Uptime => "uptime",
            Self::Psi => "psi",
            Self::CpuStat => "cpu_stat",
            Self::CpuFreq => "cpu_freq",
            Self::Hwmon => "hwmon",
            Self::Cgroup => "cgroup",
            Self::CgroupSlice => "cgroup_slice",
            Self::AmdSmu => "amd_smu",
            Self::Meminfo => "meminfo",
            Self::Vmstat => "vmstat",
            Self::Netdev => "netdev",
            Self::Diskstats => "diskstats",
            Self::Powercap => "powercap",
            Self::PowerSupply => "power_supply",
            Self::CpuIrq => "cpu_irq",
            Self::CpuSched => "cpu_sched",
            Self::ThermalZone => "thermal_zone",
            Self::CpuIdle => "cpu_idle",
            Self::CpuTopology => "cpu_topology",
            Self::Filesystems => "filesystems",
            Self::GpuDevices => "gpu_devices",
        }
    }

    pub const fn index(self) -> usize {
        self as usize
    }
}

pub const COLLECTORS: usize = CollectorId::ALL.len();
const _: () = assert!(COLLECTORS <= 64);

/// Compact selection of collectors.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CollectorSet(u64);

impl CollectorSet {
    pub const fn empty() -> Self {
        Self(0)
    }
    pub fn insert(&mut self, id: CollectorId) {
        self.0 |= 1_u64 << id.index();
    }
    pub const fn contains(self, id: CollectorId) -> bool {
        self.0 & (1_u64 << id.index()) != 0
    }
    pub const fn len(self) -> usize {
        self.0.count_ones() as usize
    }
    pub const fn is_empty(self) -> bool {
        self.0 == 0
    }
    pub fn iter(self) -> impl Iterator<Item = CollectorId> {
        CollectorId::ALL
            .into_iter()
            .filter(move |id| self.contains(*id))
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct TickCtx {
    pub tick: u64,
    pub now_ms: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CollectError {
    Io(io::ErrorKind),
    Parse(&'static str),
    Gone,
}

impl From<io::Error> for CollectError {
    fn from(error: io::Error) -> Self {
        Self::Io(error.kind())
    }
}

pub trait Collector: Send {
    fn id(&self) -> CollectorId;
    fn cadence(&self) -> Cadence;
    fn discover(&mut self, env: &Env) -> Health;
    fn sample(&mut self, ctx: &TickCtx, frame: &mut MonitorFrame) -> Result<(), CollectError>;
    fn close(&mut self);
}

#[cfg(target_os = "linux")]
pub fn default_collectors() -> Vec<Box<dyn Collector>> {
    use std::path::PathBuf;

    let mut collectors = stub::stub_collectors();
    collectors[CollectorId::Loadavg.index()] =
        Box::new(linux::loadavg::LoadavgCollector::at("/proc/loadavg"));
    collectors[CollectorId::Uptime.index()] =
        Box::new(linux::uptime::UptimeCollector::at("/proc/uptime"));
    collectors[CollectorId::Psi.index()] = Box::new(linux::psi::PsiCollector::at("/proc/pressure"));
    collectors[CollectorId::CpuStat.index()] = Box::new(linux::cpu_stat::CpuStatCollector::new());
    collectors[CollectorId::CpuFreq.index()] =
        Box::new(linux::cpufreq::CpuFreqCollector::new(PathBuf::from("/")));
    collectors[CollectorId::Hwmon.index()] = Box::new(linux::hwmon::HwmonCollector::new());
    collectors[CollectorId::Cgroup.index()] = Box::new(linux::cgroup::CgroupCollector::default());
    collectors[CollectorId::CgroupSlice.index()] =
        Box::new(linux::cgroup_slice::CgroupSliceCollector::default());
    collectors[CollectorId::AmdSmu.index()] = Box::new(linux::amd_smu::AmdSmuCollector::new());
    collectors[CollectorId::Meminfo.index()] = Box::new(linux::meminfo::MeminfoCollector::new());
    collectors[CollectorId::Vmstat.index()] = Box::new(linux::vmstat::VmstatCollector::new());
    collectors[CollectorId::Netdev.index()] = Box::new(linux::netdev::NetdevCollector::at(
        "/proc/net/dev",
        "/sys/class/net",
    ));
    collectors[CollectorId::Diskstats.index()] = Box::new(
        linux::diskstats::DiskstatsCollector::at("/proc/diskstats", "/sys/block"),
    );
    collectors[CollectorId::PowerSupply.index()] = Box::new(
        linux::power_supply::PowerSupplyCollector::at("/sys/class/power_supply"),
    );
    collectors[CollectorId::CpuIrq.index()] = Box::new(linux::cpu_irq::CpuIrqCollector::new());
    collectors[CollectorId::CpuSched.index()] =
        Box::new(linux::cpu_sched::CpuSchedCollector::new());
    collectors[CollectorId::ThermalZone.index()] =
        Box::new(linux::thermal_zones::ThermalZoneCollector::new());
    collectors[CollectorId::CpuIdle.index()] = Box::new(linux::cpu_idle::CpuIdleCollector::new());
    collectors[CollectorId::CpuTopology.index()] =
        Box::new(linux::cpu_topology::CpuTopologyCollector::new());
    collectors[CollectorId::Filesystems.index()] =
        Box::new(linux::mounts_fs::FilesystemsCollector::new());
    collectors[CollectorId::GpuDevices.index()] =
        Box::new(linux::gpu_discover::GpuDevicesCollector::new());
    collectors[CollectorId::Powercap.index()] = Box::new(linux::powercap::PowercapCollector::at(
        "/sys/class/powercap",
    ));
    collectors
}

#[cfg(not(target_os = "linux"))]
pub fn default_collectors() -> Vec<Box<dyn Collector>> {
    stub::stub_collectors()
}

#[cfg(all(test, target_os = "linux"))]
mod default_collectors_tests {
    use super::{CollectorId, default_collectors};
    use crate::collect::schedule::Cadence;

    #[test]
    fn linux_defaults_register_real_collectors_in_identity_order() {
        let collectors = default_collectors();
        let ids: Vec<_> = collectors.iter().map(|collector| collector.id()).collect();
        assert_eq!(ids.as_slice(), CollectorId::ALL.as_slice());
        for id in [
            CollectorId::Loadavg,
            CollectorId::Uptime,
            CollectorId::Psi,
            CollectorId::CpuStat,
            CollectorId::CpuFreq,
            CollectorId::Hwmon,
            CollectorId::Cgroup,
            CollectorId::CgroupSlice,
            CollectorId::AmdSmu,
            CollectorId::CpuIrq,
            CollectorId::CpuSched,
            CollectorId::ThermalZone,
            CollectorId::CpuIdle,
            CollectorId::CpuTopology,
            CollectorId::Filesystems,
            CollectorId::GpuDevices,
            CollectorId::Powercap,
        ] {
            assert_ne!(
                collectors[id.index()].cadence(),
                Cadence::Static,
                "{id:?} should be a production collector"
            );
        }
    }
}
