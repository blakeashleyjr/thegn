//! Per-container cgroup statistics derived from consecutive samples.

#[derive(Debug, Clone, PartialEq)]
pub struct ContainerStatsRow {
    pub id: String,
    pub cpu_pct: Option<f64>,
    pub mem_current: Option<u64>,
    pub mem_peak: Option<u64>,
    pub pids: Option<u64>,
    pub io_read_bps: Option<f64>,
    pub io_write_bps: Option<f64>,
    pub gap: bool,
}
