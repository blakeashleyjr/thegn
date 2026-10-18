//! RAPL powercap zone readings.
use std::sync::Arc;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PowerDomain {
    Package,
    Core,
    Uncore,
    Dram,
    Psys,
    Other,
}

#[derive(Debug, Clone, PartialEq)]
pub struct PowercapZone {
    pub name: Arc<str>,
    pub domain: PowerDomain,
    pub watts: Option<f32>,
    pub energy_uj: Option<u64>,
}

#[derive(Debug, Default, Clone, PartialEq)]
pub struct PowercapPart {
    pub zones: Vec<PowercapZone>,
}

impl crate::collect::frame::Part for PowercapPart {
    fn clear(&mut self) {
        self.zones.clear();
    }
}
