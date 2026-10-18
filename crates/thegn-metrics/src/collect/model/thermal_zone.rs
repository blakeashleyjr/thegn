//! Thermal-zone readings and static trip metadata.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TripKind {
    Passive,
    Active,
    Hot,
    Critical,
    Unknown,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ThermalTrip {
    pub index: u16,
    pub kind: TripKind,
    pub temp_c: Option<f32>,
    pub hyst_c: Option<f32>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ThermalZone {
    /// Stable across kernel zone renumbering when the device path is stable.
    pub key: String,
    pub zone_type: String,
    pub device: String,
    pub policy: Option<String>,
    pub mode: Option<String>,
    pub temp_c: Option<f32>,
    pub trips: Vec<ThermalTrip>,
    pub alias: bool,
    pub alarm_events: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CoolingAggregate {
    pub kind: String,
    pub engaged: u16,
    pub total: u16,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct ThermalZonePart {
    pub zones: Vec<ThermalZone>,
    pub cooling: Vec<CoolingAggregate>,
    pub reads: u32,
    pub held_fds: u16,
}

impl crate::collect::frame::Part for ThermalZonePart {
    fn clear(&mut self) {
        self.zones.clear();
        self.cooling.clear();
        self.reads = 0;
        self.held_fds = 0;
    }
}
