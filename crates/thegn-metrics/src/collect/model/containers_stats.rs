//! Per-container cgroup statistics derived from consecutive samples.

use super::super::frame::Part;

#[derive(Debug, Default, Clone, PartialEq)]
pub struct ContainersStatsPart {
    pub rows: Vec<ContainerStatsRow>,
    pub scopes: u32,
    pub truncated: bool,
    pub rescans: u32,
}

impl Part for ContainersStatsPart {
    fn clear(&mut self) {
        self.rows.clear();
        self.scopes = 0;
        self.truncated = false;
        self.rescans = 0;
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct ContainerStatsRow {
    pub id: std::sync::Arc<str>,
    pub cpu_pct: Option<f64>,
    pub mem_current: Option<u64>,
    pub mem_peak: Option<u64>,
    pub pids: Option<u64>,
    pub io_read_bps: Option<f64>,
    pub io_write_bps: Option<f64>,
    pub gap: bool,
}
