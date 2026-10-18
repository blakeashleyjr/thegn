use std::collections::HashMap;
use std::time::Duration;

use zbus::zvariant::{OwnedValue, Value};

use super::{DiskHealth, DiskHealthReport};

const UDISKS_NAME: &str = "org.freedesktop.UDisks2";
const DRIVE: &str = "org.freedesktop.UDisks2.Drive";
const NVME: &str = "org.freedesktop.UDisks2.NVMe.Controller";
const ATA: &str = "org.freedesktop.UDisks2.Drive.Ata";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SmartValue {
    Unsigned(u64),
    Signed(i64),
}

/// The representation of metrics after crossing the D-Bus type boundary.
/// Keeping this decoder independent of zbus makes captured dictionaries usable
/// as stable regression fixtures.
fn decode_attributes(attributes: &HashMap<String, SmartValue>) -> DecodedSmart {
    let number = |key: &str| match attributes.get(key) {
        Some(SmartValue::Unsigned(value)) => Some(*value),
        Some(SmartValue::Signed(value)) if *value >= 0 => u64::try_from(*value).ok(),
        _ => None,
    };
    let temperature_c = number("temperature")
        .or_else(|| number("temperature_sensor_1"))
        .and_then(|kelvin| i64::try_from(kelvin).ok())
        .map(|kelvin| kelvin - 273);
    DecodedSmart {
        percent_used: number("percent_used"),
        available_spare: number("avail_spare"),
        spare_threshold: number("spare_thresh"),
        data_read_bytes: number("total_data_read"),
        data_written_bytes: number("total_data_written"),
        power_on_hours: number("power_on_hours"),
        power_cycles: number("power_cycles"),
        unsafe_shutdowns: number("unsafe_shutdowns"),
        media_errors: number("media_errors").or_else(|| number("num_err_log_entries")),
        critical_warning: number("critical_warning"),
        temperature_c,
    }
}

#[derive(Debug, Default)]
struct DecodedSmart {
    percent_used: Option<u64>,
    available_spare: Option<u64>,
    spare_threshold: Option<u64>,
    data_read_bytes: Option<u64>,
    data_written_bytes: Option<u64>,
    power_on_hours: Option<u64>,
    power_cycles: Option<u64>,
    unsafe_shutdowns: Option<u64>,
    media_errors: Option<u64>,
    critical_warning: Option<u64>,
    temperature_c: Option<i64>,
}

#[derive(Default)]
struct DriveProperties {
    model: Option<String>,
    serial: Option<String>,
    smart_failing: Option<bool>,
    bad_sectors: Option<u64>,
    power_on_hours: Option<u64>,
    power_on_seconds: Option<u64>,
    temperature_kelvin: Option<u64>,
    critical_warning: Option<u64>,
}

fn smart_options() -> HashMap<&'static str, Value<'static>> {
    HashMap::from([("auth.no_user_interaction", Value::Bool(true))])
}

fn unavailable(reason: impl ToString) -> DiskHealthReport {
    DiskHealthReport::Unavailable(reason.to_string())
}

fn smart_failure_reason(failures: &[String]) -> Option<String> {
    (!failures.is_empty()).then(|| failures.join("; "))
}

fn finish_report(drives: Vec<DiskHealth>, failures: &[String]) -> DiskHealthReport {
    if drives.is_empty() {
        unavailable("UDisks2 returned no Drive objects")
    } else if let Some(reason) = smart_failure_reason(failures) {
        DiskHealthReport::Degraded { drives, reason }
    } else {
        DiskHealthReport::Available(drives)
    }
}

fn serial_last4(serial: &str) -> String {
    let chars = serial.chars().collect::<Vec<_>>();
    chars[chars.len().saturating_sub(4)..].iter().collect()
}

fn health_label(failing: Option<bool>, critical_warning: Option<u64>) -> Option<String> {
    if failing == Some(true) {
        Some("failing".to_string())
    } else if let Some(bits) = critical_warning {
        Some(if bits == 0 {
            "healthy (no critical warnings)".to_string()
        } else {
            format!("critical warning 0x{bits:x}")
        })
    } else if failing == Some(false) {
        Some("healthy".to_string())
    } else {
        None
    }
}

fn make_disk_health(
    path: &str,
    properties: DriveProperties,
    attributes: &HashMap<String, SmartValue>,
) -> DiskHealth {
    let decoded = decode_attributes(attributes);
    let temperature_c = decoded.temperature_c.or_else(|| {
        properties
            .temperature_kelvin
            .and_then(|kelvin| i64::try_from(kelvin).ok())
            .map(|kelvin| kelvin - 273)
    });
    let critical_warning = decoded.critical_warning.or(properties.critical_warning);
    DiskHealth {
        name: properties
            .model
            .unwrap_or_else(|| path.rsplit('/').next().unwrap_or("drive").to_string()),
        serial_last4: properties.serial.as_deref().map(serial_last4),
        health: health_label(properties.smart_failing, critical_warning),
        critical_warning,
        percent_used: decoded.percent_used,
        available_spare: decoded.available_spare,
        spare_threshold: decoded.spare_threshold,
        data_read_bytes: decoded.data_read_bytes,
        data_written_bytes: decoded.data_written_bytes,
        power_on_hours: decoded.power_on_hours.or(properties.power_on_hours),
        power_cycles: decoded.power_cycles,
        unsafe_shutdowns: decoded.unsafe_shutdowns,
        media_errors: decoded.media_errors,
        ata_bad_sectors: properties.bad_sectors,
        temperature_c,
    }
}

fn numeric_value(value: &OwnedValue) -> Option<u64> {
    u64::try_from(value.clone()).ok().or_else(|| {
        i64::try_from(value.clone())
            .ok()
            .and_then(|n| u64::try_from(n).ok())
    })
}

fn bool_value(value: &OwnedValue) -> Option<bool> {
    bool::try_from(value.clone()).ok()
}

fn string_value(value: &OwnedValue) -> Option<String> {
    String::try_from(value.clone()).ok()
}

fn smart_value(value: &OwnedValue) -> Option<SmartValue> {
    if let Ok(value) = u64::try_from(value.clone()) {
        Some(SmartValue::Unsigned(value))
    } else if let Ok(value) = i64::try_from(value.clone()) {
        Some(SmartValue::Signed(value))
    } else {
        None
    }
}

fn property<T>(
    properties: &HashMap<String, OwnedValue>,
    key: &str,
    decode: fn(&OwnedValue) -> Option<T>,
) -> Option<T> {
    properties.get(key).and_then(decode)
}

fn drive_properties(properties: &HashMap<String, OwnedValue>) -> DriveProperties {
    DriveProperties {
        model: property(properties, "Model", string_value),
        serial: property(properties, "Serial", string_value),
        smart_failing: property(properties, "SmartFailing", bool_value),
        bad_sectors: property(properties, "SmartNumBadSectors", numeric_value),
        power_on_hours: property(properties, "SmartPowerOnHours", numeric_value),
        power_on_seconds: property(properties, "SmartPowerOnSeconds", numeric_value),
        temperature_kelvin: property(properties, "SmartTemperature", numeric_value),
        critical_warning: property(properties, "SmartCriticalWarning", numeric_value),
    }
}

async fn collect(deadline: Duration) -> DiskHealthReport {
    use zbus::fdo::ObjectManagerProxy;
    let result = tokio::time::timeout(deadline, async {
        let connection = zbus::Connection::system()
            .await
            .map_err(|error| error.to_string())?;
        let builder = ObjectManagerProxy::builder(&connection)
            .destination(UDISKS_NAME)
            .map_err(|error| error.to_string())?;
        let manager = builder
            .path("/")
            .map_err(|error| error.to_string())?
            .build()
            .await
            .map_err(|error| error.to_string())?;
        let managed = manager
            .get_managed_objects()
            .await
            .map_err(|error| error.to_string())?;
        let mut drives = Vec::new();
        let mut failures = Vec::new();
        for (path, interfaces) in managed {
            let drive = interfaces.get(DRIVE);
            let nvme = interfaces.get(NVME);
            let ata = interfaces.get(ATA);
            if drive.is_none() && nvme.is_none() && ata.is_none() {
                continue;
            }
            let mut properties = drive.map_or_else(DriveProperties::default, drive_properties);
            if let Some(ata) = ata {
                let ata_props = drive_properties(ata);
                properties.smart_failing = properties.smart_failing.or(ata_props.smart_failing);
                properties.bad_sectors = properties.bad_sectors.or(ata_props.bad_sectors);
                properties.power_on_hours = properties.power_on_hours.or(ata_props.power_on_hours);
                properties.power_on_seconds =
                    properties.power_on_seconds.or(ata_props.power_on_seconds);
            }
            if let Some(nvme) = nvme {
                let nvme_props = drive_properties(nvme);
                properties.power_on_hours = properties.power_on_hours.or(nvme_props.power_on_hours);
                properties.temperature_kelvin = properties
                    .temperature_kelvin
                    .or(nvme_props.temperature_kelvin);
                properties.critical_warning =
                    properties.critical_warning.or(nvme_props.critical_warning);
            }
            if properties.power_on_hours.is_none() {
                properties.power_on_hours =
                    properties.power_on_seconds.map(|seconds| seconds / 3_600);
            }

            let mut attributes = HashMap::new();
            if nvme.is_some() {
                let proxy = zbus::Proxy::new(&connection, UDISKS_NAME, path.as_str(), NVME)
                    .await
                    .map_err(|error| error.to_string())?;
                let options = smart_options();
                match proxy
                    .call::<_, _, HashMap<String, OwnedValue>>("SmartGetAttributes", &(options,))
                    .await
                {
                    Ok(raw) => {
                        attributes.extend(raw.into_iter().filter_map(|(key, value)| {
                            smart_value(&value).map(|value| (key, value))
                        }));
                    }
                    Err(error) => failures.push(error.to_string()),
                }
            }
            drives.push(make_disk_health(path.as_str(), properties, &attributes));
        }
        Ok(finish_report(drives, &failures))
    })
    .await;
    match result {
        Err(_) => DiskHealthReport::Unavailable("request deadline exceeded".to_string()),
        Ok(Err(error)) => unavailable(error),
        Ok(Ok(report)) => report,
    }
}

pub(crate) fn disk_health(deadline: Duration) -> DiskHealthReport {
    match thegn_svc::bg_runtime::block_on(collect(deadline)) {
        Some(report) => report,
        None => unavailable("background runtime unavailable"),
    }
}

/// First retry after five minutes; repeated failures back off to thirty minutes.
#[allow(dead_code)] // The monitor pump owns cadence when V-DSK wires this reader.
fn retry_delay(failures: u32) -> Duration {
    if failures == 0 {
        Duration::from_secs(5 * 60)
    } else {
        Duration::from_secs(30 * 60)
    }
}

#[cfg(test)]
#[path = "udisks_smart_linux_tests.rs"]
mod tests;
