//! Portable power supply readings shared by Linux collectors and consumers.
use std::sync::Arc;

use super::super::frame::Part;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SupplyKind {
    Battery,
    Mains,
    Usb,
    Other,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SupplyStatus {
    Charging,
    Discharging,
    NotCharging,
    Full,
    Unknown,
}

impl SupplyStatus {
    pub fn parse(bytes: &[u8]) -> Option<Self> {
        match trim_ascii(bytes) {
            b"Charging" => Some(Self::Charging),
            b"Discharging" => Some(Self::Discharging),
            b"Not charging" => Some(Self::NotCharging),
            b"Full" => Some(Self::Full),
            b"Unknown" => Some(Self::Unknown),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UsbType {
    Sdp,
    Dcp,
    Cdp,
    Pd,
    PdDrp,
    PdPps,
    Other,
}

#[derive(Debug, Clone, PartialEq)]
pub struct SupplyReading {
    pub name: Arc<str>,
    pub kind: SupplyKind,
    pub device_scope: bool,
    pub online: Option<bool>,
    pub status: Option<SupplyStatus>,
    pub capacity_pct: Option<u8>,
    pub energy_now_uwh: Option<u64>,
    pub energy_full_uwh: Option<u64>,
    pub energy_full_design_uwh: Option<u64>,
    pub voltage_now_uv: Option<u64>,
    pub voltage_min_design_uv: Option<u64>,
    pub current_now_ua: Option<i64>,
    pub power_now_uw: Option<u64>,
    pub cycle_count: Option<u32>,
    pub usb_type_active: Option<UsbType>,
}

#[derive(Debug, Default, Clone, PartialEq)]
pub struct PowerSupplyPart {
    pub supplies: Vec<SupplyReading>,
}
impl Part for PowerSupplyPart {
    fn clear(&mut self) {
        self.supplies.clear();
    }
}

pub fn charge_to_energy_uwh(charge_uah: u64, voltage_uv: u64) -> Option<u64> {
    let energy = u128::from(charge_uah).checked_mul(u128::from(voltage_uv))? / 1_000_000;
    let Ok(energy) = u64::try_from(energy) else {
        return None;
    };
    Some(energy)
}

pub fn health_pct(full: u64, design: u64) -> Option<f32> {
    (design != 0).then_some((full as f64 / design as f64 * 100.0) as f32)
}

pub fn active_usb_type(bytes: &[u8]) -> Option<UsbType> {
    let bytes = trim_ascii(bytes);
    let start = bytes.iter().position(|b| *b == b'[')? + 1;
    let end = bytes[start..].iter().position(|b| *b == b']')? + start;
    match &bytes[start..end] {
        b"SDP" => Some(UsbType::Sdp),
        b"DCP" => Some(UsbType::Dcp),
        b"CDP" => Some(UsbType::Cdp),
        b"PD" => Some(UsbType::Pd),
        b"PD_DR" | b"PD_DRP" => Some(UsbType::PdDrp),
        b"PD_PPS" => Some(UsbType::PdPps),
        _ => Some(UsbType::Other),
    }
}

fn trim_ascii(mut bytes: &[u8]) -> &[u8] {
    while bytes.first().is_some_and(u8::is_ascii_whitespace) {
        bytes = &bytes[1..];
    }
    while bytes.last().is_some_and(u8::is_ascii_whitespace) {
        bytes = &bytes[..bytes.len() - 1];
    }
    bytes
}

#[cfg(test)]
mod tests {
    use super::{SupplyStatus, UsbType, active_usb_type, charge_to_energy_uwh, health_pct};

    #[test]
    fn charge_conversion_health_and_usb_active_token() {
        let energy = charge_to_energy_uwh(3_462_000, 15_480_000);
        assert_eq!(energy, Some(53_591_760));
        assert_eq!(energy.unwrap() as f64 / 1_000_000.0, 53.59176);
        assert_eq!((energy.unwrap() as f64 / 10_000.0).round() / 100.0, 53.59);
        assert_eq!(charge_to_energy_uwh(u64::MAX, u64::MAX), None);
        assert!((health_pct(3_463_000, 3_915_000).unwrap() - 88.45).abs() < 0.01);
        assert_eq!(health_pct(1, 0), None);
        assert_eq!(active_usb_type(b"C [PD] PD_PPS\n"), Some(UsbType::Pd));
        assert_eq!(
            SupplyStatus::parse(b"Charging\n"),
            Some(SupplyStatus::Charging)
        );
    }
}
