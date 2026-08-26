//! Per-platform metric-coverage classification for `thegn doctor`.
//!
//! Pure logic over a sampled [`StatsSnapshot`]: for every metric family the
//! sampler models, decide whether *this* platform and machine actually yields
//! it, and if not, why. It answers the "cross platform?" half of the monitoring
//! audit — coverage is real but ragged (GPU is Linux sysfs + `nvidia-smi`,
//! reclaim is Linux-only, load is unix-only), and until now nothing reported
//! what a given build can and cannot measure.
//!
//! Two reason classes are distinguished from a snapshot:
//! - [`AbsentReason::NotOnThisOs`] — the family is not sampled on this OS build
//!   at all (reclaim off Linux, load on Windows).
//! - [`AbsentReason::NoHardware`] — the family is sampled here but this machine
//!   has nothing to report (no battery, no discrete GPU, no swap).
//!
//! The classification takes a single *primed* snapshot (the caller samples
//! twice so CPU/net deltas exist) and reads only its `Option`/collection fields,
//! so it mirrors exactly what the widgets and monitor tabs would show — the
//! coverage report and the UI cannot disagree.

use crate::StatsSnapshot;

/// A metric family the sampler models.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MetricFamily {
    Cpu,
    Cores,
    Frequency,
    Temperature,
    Memory,
    Swap,
    Reclaim,
    Gpu,
    Network,
    Battery,
    Disk,
    Load,
    Uptime,
}

impl MetricFamily {
    /// Every family, in report order.
    pub const ALL: [MetricFamily; 13] = [
        MetricFamily::Cpu,
        MetricFamily::Cores,
        MetricFamily::Frequency,
        MetricFamily::Temperature,
        MetricFamily::Memory,
        MetricFamily::Swap,
        MetricFamily::Reclaim,
        MetricFamily::Gpu,
        MetricFamily::Network,
        MetricFamily::Battery,
        MetricFamily::Disk,
        MetricFamily::Load,
        MetricFamily::Uptime,
    ];

    /// Stable lowercase key (doctor `--json` field, log target).
    pub fn key(self) -> &'static str {
        match self {
            MetricFamily::Cpu => "cpu",
            MetricFamily::Cores => "cores",
            MetricFamily::Frequency => "frequency",
            MetricFamily::Temperature => "temperature",
            MetricFamily::Memory => "memory",
            MetricFamily::Swap => "swap",
            MetricFamily::Reclaim => "reclaim",
            MetricFamily::Gpu => "gpu",
            MetricFamily::Network => "network",
            MetricFamily::Battery => "battery",
            MetricFamily::Disk => "disk",
            MetricFamily::Load => "load",
            MetricFamily::Uptime => "uptime",
        }
    }
}

/// Why a family is absent on this platform/machine.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AbsentReason {
    /// The family is not implemented on this OS build (e.g. direct reclaim off
    /// Linux, load average on Windows).
    NotOnThisOs,
    /// The family is sampled here, but this machine exposes nothing (no battery,
    /// no discrete GPU, no swap, no thermal sensor).
    NoHardware,
    /// Present on the machine but unreadable without more privilege. Reserved:
    /// no snapshot-level permission signal exists today, so nothing emits it
    /// yet — kept so the report's vocabulary is complete and forward-stable.
    #[allow(dead_code)]
    NoPermission,
}

impl AbsentReason {
    /// Short reason word for the report line and JSON.
    pub fn word(self) -> &'static str {
        match self {
            AbsentReason::NotOnThisOs => "not on this OS",
            AbsentReason::NoHardware => "no such hardware",
            AbsentReason::NoPermission => "no permission",
        }
    }
}

/// Whether a family is measurable, and if not, why.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Coverage {
    Available,
    Absent(AbsentReason),
}

impl Coverage {
    pub fn is_available(self) -> bool {
        matches!(self, Coverage::Available)
    }
}

/// One family's coverage on this platform.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FamilyReport {
    pub family: MetricFamily,
    pub coverage: Coverage,
}

/// Classify coverage for every metric family from a primed snapshot.
///
/// Pure: reads only the snapshot's presence, so it reports exactly what the
/// widgets/tabs would. Sample twice (a CPU/net delta) before calling, or
/// cpu/cores/frequency read as absent-no-hardware on the priming sample.
pub fn coverage(s: &StatsSnapshot) -> Vec<FamilyReport> {
    MetricFamily::ALL
        .into_iter()
        .map(|family| FamilyReport {
            family,
            coverage: classify(family, s),
        })
        .collect()
}

/// The OS-support half of the classification: is this family sampled at all on
/// this build? A family not sampled here is [`AbsentReason::NotOnThisOs`] when
/// absent; a family that *is* sampled here but empty is [`AbsentReason::NoHardware`].
fn os_supported(family: MetricFamily) -> bool {
    match family {
        // Linux-only: `/proc/vmstat pgsteal_direct`.
        MetricFamily::Reclaim => cfg!(target_os = "linux"),
        // Unix-only: `getloadavg`.
        MetricFamily::Load => cfg!(unix),
        // Everything else is attempted on every supported OS (GPU tries sysfs,
        // `nvidia-smi`, and macOS ioaccel; temps try Components + Apple HID).
        _ => true,
    }
}

fn classify(family: MetricFamily, s: &StatsSnapshot) -> Coverage {
    let present = match family {
        MetricFamily::Cpu => s.cpu_pct.is_some(),
        MetricFamily::Cores => !s.cpu_cores.is_empty(),
        MetricFamily::Frequency => s.cpu_freq_mhz.is_some(),
        MetricFamily::Temperature => s.cpu_temp_c.is_some() || !s.temps.is_empty(),
        MetricFamily::Memory => s.mem_gib.is_some(),
        MetricFamily::Swap => s.swap_gib.is_some(),
        MetricFamily::Reclaim => s.reclaim_per_s.is_some(),
        MetricFamily::Gpu => s.gpu_pct.is_some() || s.gpu_mem_mib.is_some(),
        MetricFamily::Network => s.net_bps.is_some(),
        MetricFamily::Battery => s.battery.is_some(),
        MetricFamily::Disk => !s.disks.is_empty() || s.disk_bytes.is_some(),
        MetricFamily::Load => s.load_avg.is_some(),
        MetricFamily::Uptime => s.uptime_secs.is_some(),
    };
    if present {
        Coverage::Available
    } else if os_supported(family) {
        Coverage::Absent(AbsentReason::NoHardware)
    } else {
        Coverage::Absent(AbsentReason::NotOnThisOs)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{DiskInfo, DiskKind};

    /// A fully-populated snapshot: every family the machine could report reads
    /// Available.
    fn full() -> StatsSnapshot {
        StatsSnapshot {
            cpu_pct: Some(20),
            cpu_cores: vec![10, 20, 30, 40],
            cpu_freq_mhz: Some(2400),
            cpu_temp_c: Some(45.0),
            mem_gib: Some((8.0, 32.0)),
            swap_gib: Some((1.0, 8.0)),
            reclaim_per_s: Some(0.0),
            gpu_pct: Some(5),
            net_bps: Some((100, 200)),
            battery: Some((80, true)),
            disk_bytes: Some((1000, 500)),
            disks: vec![DiskInfo {
                name: "nvme0".into(),
                mount: "/".into(),
                free_pct: 50,
                read_bps: 0,
                write_bps: 0,
                kind: DiskKind::Ssd,
            }],
            temps: vec![("cpu".into(), 45.0)],
            load_avg: Some((1.0, 1.0, 1.0)),
            uptime_secs: Some(1000),
            ..Default::default()
        }
    }

    fn cov(reports: &[FamilyReport], f: MetricFamily) -> Coverage {
        reports.iter().find(|r| r.family == f).unwrap().coverage
    }

    #[test]
    fn a_full_snapshot_reports_every_family_available() {
        let reports = coverage(&full());
        assert_eq!(reports.len(), MetricFamily::ALL.len());
        // Reclaim and load are OS-gated: available only where sampled.
        for r in &reports {
            match r.family {
                MetricFamily::Reclaim if !cfg!(target_os = "linux") => {
                    assert_eq!(r.coverage, Coverage::Absent(AbsentReason::NotOnThisOs));
                }
                MetricFamily::Load if !cfg!(unix) => {
                    assert_eq!(r.coverage, Coverage::Absent(AbsentReason::NotOnThisOs));
                }
                _ => assert!(r.coverage.is_available(), "{:?} not available", r.family),
            }
        }
    }

    #[test]
    fn missing_hardware_reports_no_hardware_not_a_zero() {
        // A desktop: no battery, no GPU, no swap, no thermal sensor. Each is
        // sampled on this OS, so the reason is "no such hardware", never a
        // reassuring available-zero.
        let mut s = full();
        s.battery = None;
        s.gpu_pct = None;
        s.gpu_mem_mib = None;
        s.swap_gib = None;
        s.cpu_temp_c = None;
        s.temps.clear();
        let reports = coverage(&s);
        assert_eq!(
            cov(&reports, MetricFamily::Battery),
            Coverage::Absent(AbsentReason::NoHardware)
        );
        assert_eq!(
            cov(&reports, MetricFamily::Gpu),
            Coverage::Absent(AbsentReason::NoHardware)
        );
        assert_eq!(
            cov(&reports, MetricFamily::Swap),
            Coverage::Absent(AbsentReason::NoHardware)
        );
        assert_eq!(
            cov(&reports, MetricFamily::Temperature),
            Coverage::Absent(AbsentReason::NoHardware)
        );
        // Core families stay available.
        assert!(cov(&reports, MetricFamily::Cpu).is_available());
        assert!(cov(&reports, MetricFamily::Memory).is_available());
        assert!(cov(&reports, MetricFamily::Disk).is_available());
    }

    #[test]
    fn os_gated_families_are_not_on_this_os_when_the_build_omits_them() {
        // Reclaim absent classifies by OS: NotOnThisOs off Linux, NoHardware on.
        let mut s = full();
        s.reclaim_per_s = None;
        s.load_avg = None;
        let reports = coverage(&s);
        let want_reclaim = if cfg!(target_os = "linux") {
            AbsentReason::NoHardware
        } else {
            AbsentReason::NotOnThisOs
        };
        assert_eq!(
            cov(&reports, MetricFamily::Reclaim),
            Coverage::Absent(want_reclaim)
        );
        let want_load = if cfg!(unix) {
            AbsentReason::NoHardware
        } else {
            AbsentReason::NotOnThisOs
        };
        assert_eq!(
            cov(&reports, MetricFamily::Load),
            Coverage::Absent(want_load)
        );
    }

    #[test]
    fn reason_words_and_keys_are_stable() {
        assert_eq!(AbsentReason::NotOnThisOs.word(), "not on this OS");
        assert_eq!(AbsentReason::NoHardware.word(), "no such hardware");
        assert_eq!(AbsentReason::NoPermission.word(), "no permission");
        assert_eq!(MetricFamily::Cpu.key(), "cpu");
        // Keys are unique — they name JSON fields.
        let mut keys: Vec<&str> = MetricFamily::ALL.iter().map(|f| f.key()).collect();
        let n = keys.len();
        keys.sort_unstable();
        keys.dedup();
        assert_eq!(keys.len(), n);
    }
}
