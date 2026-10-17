//! Portable hwmon naming, grouping, value parsing and safety policy.
use crate::collect::model::hwmon::{ChipGroup, CostClass, Level, SensorKind, Thresholds};

pub fn parse_attr(attr: &str) -> Option<(SensorKind, u16, &str)> {
    let split = attr.find(|c: char| !c.is_ascii_alphabetic())?;
    let (prefix, rest) = attr.split_at(split);
    let split = rest.find('_').unwrap_or(rest.len());
    let channel = rest.get(..split)?.parse::<u16>().ok()?;
    let suffix = rest.get(split..)?;
    let kind = match prefix {
        "temp" => SensorKind::Temp,
        "fan" => SensorKind::Fan,
        "power" => SensorKind::Power,
        "in" => SensorKind::Voltage,
        "curr" => SensorKind::Current,
        "freq" => SensorKind::Freq,
        _ => return None,
    };
    let valid = matches!(
        (kind, suffix),
        (
            SensorKind::Temp,
            "_input" | "_min" | "_lcrit" | "_max" | "_crit" | "_emergency" | "_crit_hyst",
        ) | (
            SensorKind::Fan,
            "_input" | "_target" | "_min" | "_max" | "_crit"
        ) | (
            SensorKind::Power,
            "_average" | "_input" | "_min" | "_max" | "_crit"
        ) | (_, "_input" | "_min" | "_max" | "_crit")
    );
    valid.then_some((kind, channel, suffix))
}

pub fn stable_key(name: &str, device_rel: Option<&str>, kind: SensorKind, channel: u16) -> String {
    let device = device_rel.filter(|s| !s.is_empty()).unwrap_or("virtual");
    format!("{name}@{device}#{}{channel}", kind_slug(kind))
}

pub const fn kind_slug(kind: SensorKind) -> &'static str {
    match kind {
        SensorKind::Temp => "temp",
        SensorKind::Fan => "fan",
        SensorKind::Power => "power",
        SensorKind::Voltage => "in",
        SensorKind::Current => "curr",
        SensorKind::Freq => "freq",
    }
}

pub fn group_for(name: &str) -> Option<ChipGroup> {
    if name.starts_with("ucsi") || name.starts_with("BAT") || name.starts_with("AC") {
        return None;
    }
    Some(match name {
        "k10temp" | "coretemp" | "zenpower" => ChipGroup::Cpu,
        "amdgpu" | "nouveau" | "i915" | "xe" => ChipGroup::Gpu,
        "nvme" => ChipGroup::Nvme,
        "drivetemp" => ChipGroup::Sata,
        "spd5118" | "jc42" => ChipGroup::Dimm,
        "acpitz" => ChipGroup::Acpi,
        "cros_ec" | "thinkpad" | "dell_smm" => ChipGroup::BoardEc,
        _ if name.starts_with("nct") || name.starts_with("it87") => ChipGroup::BoardEc,
        _ if name.starts_with("mt79") || name.starts_with("iwlwifi") => ChipGroup::Wifi,
        _ => ChipGroup::Other,
    })
}

pub fn cost_for(name: &str) -> CostClass {
    match name {
        "spd5118" | "jc42" => CostClass::Slow,
        "drivetemp" => CostClass::VerySlow,
        _ => CostClass::Fast,
    }
}

pub fn parse_milli(bytes: &[u8]) -> Option<i64> {
    let bytes = bytes.trim_ascii();
    if bytes.is_empty() {
        return None;
    }
    let (negative, digits) = match bytes.first() {
        Some(b'-') => (true, &bytes[1..]),
        Some(b'+') => (false, &bytes[1..]),
        _ => (false, bytes),
    };
    if digits.is_empty() || !digits.iter().all(u8::is_ascii_digit) {
        return None;
    }
    let value = digits.iter().try_fold(0_i64, |n, b| {
        n.checked_mul(10)?.checked_add(i64::from(*b - b'0'))
    })?;
    if negative {
        value.checked_neg()
    } else {
        Some(value)
    }
}

pub fn plausible_threshold(kind: SensorKind, value: i64) -> Option<f32> {
    let scaled = value as f32 / kind.divisor();
    match kind {
        SensorKind::Temp if !(-40.0..=200.0).contains(&scaled) => None,
        _ if scaled.is_finite() => Some(scaled),
        _ => None,
    }
}

pub fn plausible_input(kind: SensorKind, value: i64) -> Option<f32> {
    let scaled = value as f32 / kind.divisor();
    let plausible = match kind {
        SensorKind::Temp => (-40.0..=200.0).contains(&scaled),
        SensorKind::Fan => (0.0..=50_000.0).contains(&scaled),
        _ => scaled.is_finite(),
    };
    plausible.then_some(scaled)
}

pub fn level(value: Option<f32>, thresholds: &Thresholds) -> Level {
    let Some(value) = value else {
        return Level::Normal;
    };
    if thresholds.crit.is_some_and(|threshold| value >= threshold)
        || thresholds
            .emergency
            .is_some_and(|threshold| value >= threshold)
    {
        Level::OverCrit
    } else if thresholds.max.is_some_and(|threshold| value >= threshold) {
        Level::OverMax
    } else if thresholds.min.is_some_and(|threshold| value < threshold) {
        Level::UnderMin
    } else {
        Level::Normal
    }
}

pub fn wake_gate(runtime_status: &[u8], scsi_state: Option<&[u8]>) -> bool {
    let status = runtime_status.trim_ascii();
    status == b"suspended"
        || status == b"suspending"
        || scsi_state.is_some_and(|state| state.trim_ascii() != b"running")
}

#[cfg(test)]
mod tests {
    use super::{
        group_for, level, parse_attr, parse_milli, plausible_input, plausible_threshold,
        stable_key, wake_gate,
    };
    use crate::collect::model::hwmon::{ChipGroup, Level, SensorKind, Thresholds};

    #[test]
    fn parses_attributes_groups_signed_values_and_thresholds() {
        for (attr, kind, channel, suffix) in [
            ("temp10_input", SensorKind::Temp, 10, "_input"),
            ("fan1_target", SensorKind::Fan, 1, "_target"),
            ("power1_average", SensorKind::Power, 1, "_average"),
            ("in0_input", SensorKind::Voltage, 0, "_input"),
            ("curr1_input", SensorKind::Current, 1, "_input"),
            ("freq1_input", SensorKind::Freq, 1, "_input"),
            ("temp2_crit_hyst", SensorKind::Temp, 2, "_crit_hyst"),
        ] {
            assert_eq!(parse_attr(attr), Some((kind, channel, suffix)));
        }
        assert_eq!(parse_attr("temp_input"), None);
        assert_eq!(parse_attr("tempX_input"), None);
        assert_eq!(parse_milli(b"-273150\n"), Some(-273150));
        assert_eq!(parse_milli(b"65261850"), Some(65261850));
        for bad in [&b""[..], b"12a", b"9223372036854775808"] {
            assert_eq!(parse_milli(bad), None);
        }
        assert_eq!(plausible_threshold(SensorKind::Temp, 65261850), None);
        assert_eq!(plausible_threshold(SensorKind::Temp, -273150), None);
        assert_eq!(plausible_input(SensorKind::Fan, 50001), None);
    }

    #[test]
    fn groups_keys_gates_and_level_edges_are_stable() {
        assert_eq!(group_for("spd5118"), Some(ChipGroup::Dimm));
        assert_eq!(group_for("ucsi_source_psy"), None);
        assert_eq!(group_for("BAT1"), None);
        assert_eq!(group_for("ACAD"), None);
        assert_eq!(
            stable_key("nvme", Some("/devices/pci/1"), SensorKind::Temp, 2),
            "nvme@/devices/pci/1#temp2"
        );
        assert_eq!(
            stable_key("virtual", None, SensorKind::Temp, 1),
            "virtual@virtual#temp1"
        );
        assert!(wake_gate(b"suspended\n", None));
        assert!(wake_gate(b"active", Some(b"offline")));
        assert!(!wake_gate(b"active", Some(b"running")));
        let thresholds = Thresholds {
            min: Some(10.0),
            max: Some(80.0),
            crit: Some(90.0),
            ..Thresholds::default()
        };
        assert_eq!(level(Some(9.0), &thresholds), Level::UnderMin);
        assert_eq!(level(Some(80.0), &thresholds), Level::OverMax);
        assert_eq!(level(Some(90.0), &thresholds), Level::OverCrit);
    }
}
