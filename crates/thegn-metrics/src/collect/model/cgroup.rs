//! Fixed-shape cgroup v2 samples. Missing kernel files remain absent values.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CpuStat {
    pub usage_usec: u64,
    pub user_usec: u64,
    pub system_usec: u64,
    pub nr_periods: Option<u64>,
    pub nr_throttled: Option<u64>,
    pub throttled_usec: Option<u64>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct MemStat {
    pub anon: Option<u64>,
    pub file: Option<u64>,
    pub kernel: Option<u64>,
    pub sock: Option<u64>,
    pub shmem: Option<u64>,
    pub slab: Option<u64>,
    pub pagetables: Option<u64>,
    pub file_dirty: Option<u64>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct MemEvents {
    pub low: Option<u64>,
    pub high: Option<u64>,
    pub max: Option<u64>,
    pub oom: Option<u64>,
    pub oom_kill: Option<u64>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct IoTotals {
    pub rbytes: u64,
    pub wbytes: u64,
    pub rios: u64,
    pub wios: u64,
}

#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Pressure {
    pub some_avg10: Option<f32>,
    pub some_total_us: Option<u64>,
    pub full_avg10: Option<f32>,
    pub full_total_us: Option<u64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Limit {
    Max,
    Value(u64),
}

#[derive(Debug, Default, Clone, PartialEq)]
pub struct CgroupSample {
    pub cpu: Option<CpuStat>,
    pub mem_current: Option<u64>,
    pub mem_max: Option<Limit>,
    pub mem_high: Option<Limit>,
    pub mem_peak: Option<u64>,
    pub swap_current: Option<u64>,
    pub mem_stat: Option<MemStat>,
    pub mem_events: Option<MemEvents>,
    pub pids_current: Option<u64>,
    pub pids_max: Option<Limit>,
    pub io: Option<IoTotals>,
    pub cpu_pressure: Option<Pressure>,
    pub mem_pressure: Option<Pressure>,
    pub io_pressure: Option<Pressure>,
}

#[derive(Debug, Default, Clone, PartialEq)]
pub struct GroupRow {
    pub target: u16,
    pub sample: CgroupSample,
}

pub const MAX_GROUPS: usize = 64;

#[derive(Debug, Default, Clone, PartialEq)]
pub struct CgroupPart {
    pub groups: Vec<GroupRow>,
    pub truncated: bool,
}

impl crate::collect::frame::Part for CgroupPart {
    fn clear(&mut self) {
        self.groups.clear();
        self.truncated = false;
    }
}

#[cfg(test)]
mod tests {
    use super::{CgroupPart, GroupRow, MAX_GROUPS};
    use crate::collect::frame::Part;

    #[test]
    fn clear_reuses_group_capacity() {
        let mut part = CgroupPart {
            groups: vec![GroupRow::default(); 3],
            truncated: true,
        };
        let capacity = part.groups.capacity();
        part.clear();
        assert!(part.groups.is_empty());
        assert_eq!(part.groups.capacity(), capacity);
        assert!(!part.truncated);
        assert_eq!(MAX_GROUPS, 64);
    }
}
