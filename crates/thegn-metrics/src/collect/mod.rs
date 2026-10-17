//! F3a collector contract. Phase-0 readers absorbed by this seam: SR-2
//! (`cpu_freq.rs`), M-1 (`vmstat.rs`), D-1 (`disk_aggregate.rs`), N-1/N-5
//! (`net_kind.rs`), G-1/G-2 (`gpu_sysfs.rs`), and T-2 (`battery.rs`).
//! Consumers are the §2.3 system-monitor panels and future F3b registry.

pub mod delta;
pub mod env;
pub mod frame;
pub mod health;
#[cfg(target_os = "linux")]
pub mod linux;
pub mod model;
pub mod parse;
pub mod policy;
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
}

impl CollectorId {
    pub const ALL: [Self; 14] = [
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
