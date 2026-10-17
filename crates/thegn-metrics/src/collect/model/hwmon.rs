//! Shared hwmon layout and readings for §2.3 monitor consumers.
use std::sync::Arc;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SensorKind {
    Temp,
    Fan,
    Pwm,
    Power,
    Voltage,
    Current,
    Freq,
    Energy,
}
impl SensorKind {
    /// Multiplier converting the kernel attribute unit to the displayed unit.
    pub const fn scale(self) -> f32 {
        match self {
            Self::Temp | Self::Voltage | Self::Current => 0.001,
            Self::Fan | Self::Pwm | Self::Energy => 1.0,
            Self::Power => 0.000_001,
            Self::Freq => 0.000_001,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChipGroup {
    Cpu,
    Gpu,
    Nvme,
    Sata,
    Dimm,
    Acpi,
    BoardEc,
    Wifi,
    Other,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CostClass {
    Fast,
    Slow,
    VerySlow,
}
impl CostClass {
    pub const fn period(self) -> u16 {
        match self {
            Self::Fast => 1,
            Self::Slow => 5,
            Self::VerySlow => 30,
        }
    }
}

#[derive(Debug, Default, Clone, Copy, PartialEq)]
pub struct Thresholds {
    pub min: Option<f32>,
    pub lcrit: Option<f32>,
    pub max: Option<f32>,
    pub crit: Option<f32>,
    pub emergency: Option<f32>,
    pub crit_hyst: Option<f32>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Level {
    #[default]
    Normal,
    UnderMin,
    OverMax,
    OverCrit,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum ValueStatus {
    #[default]
    Fresh,
    Carried,
    Gated,
    Implausible,
    Error,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HwmonChip {
    pub name: String,
    pub device: String,
    pub group: ChipGroup,
    pub ordinal: u16,
    pub gated: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct HwmonSensor {
    pub key: String,
    pub chip: u16,
    pub kind: SensorKind,
    pub channel: u16,
    pub label: Option<String>,
    pub thresholds: Thresholds,
    pub cost: CostClass,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct HwmonLayout {
    pub generation: u64,
    pub chips: Vec<HwmonChip>,
    pub sensors: Vec<HwmonSensor>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct HwmonValue {
    pub value: Option<f32>,
    pub level: Level,
    pub status: ValueStatus,
}

#[derive(Debug, Clone, PartialEq)]
pub struct HwmonPart {
    pub layout: Option<Arc<HwmonLayout>>,
    pub values: Vec<HwmonValue>,
    pub reads: u32,
    pub held_fds: u16,
    pub gated_chips: u16,
}
impl Default for HwmonPart {
    fn default() -> Self {
        Self {
            layout: None,
            values: Vec::with_capacity(256),
            reads: 0,
            held_fds: 0,
            gated_chips: 0,
        }
    }
}
impl crate::collect::frame::Part for HwmonPart {
    fn clear(&mut self) {
        self.layout = None;
        self.values.clear();
        self.reads = 0;
        self.held_fds = 0;
        self.gated_chips = 0;
    }
}

#[cfg(test)]
mod tests {
    use super::{HwmonPart, HwmonValue};
    use crate::collect::frame::Part;

    #[test]
    fn clear_resets_state_and_preserves_value_capacity() {
        let mut part = HwmonPart {
            values: vec![HwmonValue::default()],
            reads: 3,
            ..HwmonPart::default()
        };
        let capacity = part.values.capacity();
        part.clear();
        assert!(part.values.is_empty());
        assert_eq!(part.values.capacity(), capacity);
        assert_eq!(part.reads, 0);
    }
}
