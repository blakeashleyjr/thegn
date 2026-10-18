//! Shared disk device rows and aggregate counters.
use crate::collect::{frame::Part, instances::InstanceId};
use std::sync::Arc;

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub enum DevKind {
    Disk,
    Partition,
    Dm,
    Md,
    Loop,
    Zram,
    #[default]
    Other,
}
#[derive(Debug, Clone, PartialEq)]
pub struct DiskDevice {
    pub id: InstanceId,
    pub name: Arc<str>,
    pub label: Option<Arc<str>>,
    pub kind: DevKind,
    pub physical: bool,
    pub parent: Option<InstanceId>,
    pub read_bps: Option<f64>,
    pub write_bps: Option<f64>,
    pub read_iops: Option<f64>,
    pub write_iops: Option<f64>,
    pub util_pct: Option<f64>,
    pub in_flight: u64,
}
#[derive(Debug, Default, Clone, Copy, PartialEq)]
pub struct DiskTotals {
    pub read_bps: Option<f64>,
    pub write_bps: Option<f64>,
    pub read_iops: Option<f64>,
    pub write_iops: Option<f64>,
    pub sectors_read_total: u64,
    pub sectors_written_total: u64,
}
#[derive(Debug, Default, Clone, PartialEq)]
pub struct DiskstatsPart {
    pub devices: Vec<DiskDevice>,
    pub aggregate: DiskTotals,
    pub physical: u16,
    pub topology_rev: u32,
    pub truncated: bool,
}
impl Part for DiskstatsPart {
    fn clear(&mut self) {
        self.devices.clear();
        self.aggregate = DiskTotals::default();
        self.physical = 0;
        self.topology_rev = 0;
        self.truncated = false;
    }
}

#[cfg(test)]
mod tests {
    use super::{DiskDevice, DiskstatsPart};
    use crate::collect::{frame::Part, instances::InstanceId};
    use std::sync::Arc;

    #[test]
    fn clear_resets_values_and_keeps_device_capacity() {
        let mut part = DiskstatsPart {
            devices: vec![DiskDevice {
                id: InstanceId(0),
                name: Arc::from("sda"),
                label: None,
                kind: Default::default(),
                physical: true,
                parent: None,
                read_bps: Some(1.0),
                write_bps: None,
                read_iops: None,
                write_iops: None,
                util_pct: None,
                in_flight: 0,
            }],
            physical: 1,
            topology_rev: 2,
            ..DiskstatsPart::default()
        };
        let capacity = part.devices.capacity();
        part.clear();
        assert!(part.devices.is_empty());
        assert!(part.devices.capacity() >= capacity);
        assert_eq!(part.physical, 0);
        assert_eq!(part.topology_rev, 0);
    }
}
