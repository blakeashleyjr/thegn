use std::collections::HashMap;
use std::time::Duration;

use zbus::names::OwnedInterfaceName;
use zbus::zvariant::{OwnedValue, Value};

use super::{DiskHealth, DiskHealthReport};

const UDISKS_NAME: &str = "org.freedesktop.UDisks2";
const DRIVE: &str = "org.freedesktop.UDisks2.Drive";
const NVME: &str = "org.freedesktop.UDisks2.NVMe.Controller";
const ATA: &str = "org.freedesktop.UDisks2.Drive.Ata";

type Properties = HashMap<String, OwnedValue>;
type Interfaces = HashMap<OwnedInterfaceName, Properties>;

fn decode_attributes(attributes: &Properties) -> DecodedSmart {
    let number = |key: &str| attributes.get(key).and_then(numeric_value);
    let temperature_c = attributes
        .get("temp_sensors")
        .and_then(u16_array_value)
        .and_then(|sensors| sensors.first().copied())
        .map(|kelvin| i64::from(kelvin) - 273);
    DecodedSmart {
        percent_used: number("percent_used"),
        available_spare: number("avail_spare"),
        spare_threshold: number("spare_thresh"),
        data_read_bytes: number("total_data_read"),
        data_written_bytes: number("total_data_written"),
        power_cycles: number("power_cycles"),
        unsafe_shutdowns: number("unsafe_shutdowns"),
        media_errors: number("media_errors"),
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
    power_cycles: Option<u64>,
    unsafe_shutdowns: Option<u64>,
    media_errors: Option<u64>,
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
    temperature_c: Option<i64>,
    critical_warning: Option<Vec<String>>,
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

fn health_label(failing: Option<bool>, critical_warning: Option<&[String]>) -> Option<String> {
    if failing == Some(true) {
        Some("failing".to_string())
    } else if let Some(warnings) = critical_warning {
        Some(if warnings.is_empty() {
            "healthy (no critical warnings)".to_string()
        } else {
            format!("critical warning: {}", warnings.join(", "))
        })
    } else if failing == Some(false) {
        Some("healthy".to_string())
    } else {
        None
    }
}

fn decode_drive(path: &str, interfaces: &Interfaces, attributes: &Properties) -> DiskHealth {
    let drive = interfaces.get(DRIVE);
    let nvme = interfaces.get(NVME);
    let ata = interfaces.get(ATA);
    let mut properties = drive.map_or_else(DriveProperties::default, drive_properties);
    if let Some(ata) = ata {
        let ata_props = drive_properties(ata);
        properties.smart_failing = properties.smart_failing.or(ata_props.smart_failing);
        properties.bad_sectors = properties.bad_sectors.or(ata_props.bad_sectors);
        properties.power_on_hours = properties.power_on_hours.or(ata_props.power_on_hours);
        properties.power_on_seconds = properties.power_on_seconds.or(ata_props.power_on_seconds);
        properties.temperature_c = properties.temperature_c.or(ata_props.temperature_c);
    }
    if let Some(nvme) = nvme {
        let nvme_props = drive_properties(nvme);
        properties.power_on_hours = properties.power_on_hours.or(nvme_props.power_on_hours);
        properties.temperature_c = properties.temperature_c.or(nvme_props.temperature_c);
        properties.critical_warning = properties.critical_warning.or(nvme_props.critical_warning);
    }
    if properties.power_on_hours.is_none() {
        properties.power_on_hours = properties.power_on_seconds.map(|seconds| seconds / 3_600);
    }

    let decoded = decode_attributes(attributes);
    let temperature_c = decoded.temperature_c.or(properties.temperature_c);
    let critical_warning = properties.critical_warning;
    let model = properties.model.filter(|model| !model.is_empty());
    DiskHealth {
        name: model.unwrap_or_else(|| path.rsplit('/').next().unwrap_or("drive").to_string()),
        serial_last4: properties.serial.as_deref().map(serial_last4),
        health: health_label(properties.smart_failing, critical_warning.as_deref()),
        critical_warning,
        percent_used: decoded.percent_used,
        available_spare: decoded.available_spare,
        spare_threshold: decoded.spare_threshold,
        data_read_bytes: decoded.data_read_bytes,
        data_written_bytes: decoded.data_written_bytes,
        power_on_hours: properties.power_on_hours,
        power_cycles: decoded.power_cycles,
        unsafe_shutdowns: decoded.unsafe_shutdowns,
        media_errors: decoded.media_errors,
        ata_bad_sectors: properties.bad_sectors,
        temperature_c,
    }
}

fn numeric_value(value: &OwnedValue) -> Option<u64> {
    match &*value {
        Value::U8(n) => Some(u64::from(*n)),
        Value::U16(n) => Some(u64::from(*n)),
        Value::U32(n) => Some(u64::from(*n)),
        Value::U64(n) => Some(*n),
        Value::I16(n) => u64::try_from(*n).ok(),
        Value::I32(n) => u64::try_from(*n).ok(),
        Value::I64(n) => u64::try_from(*n).ok(),
        _ => None,
    }
}

fn float_value(value: &OwnedValue) -> Option<f64> {
    match &*value {
        Value::F64(n) => Some(*n),
        _ => None,
    }
}

fn temperature_value(value: &OwnedValue) -> Option<i64> {
    numeric_value(value)
        .and_then(|kelvin| i64::try_from(kelvin).ok())
        .map(|kelvin| kelvin - 273)
        .or_else(|| float_value(value).map(kelvin_to_celsius))
}

fn kelvin_to_celsius(kelvin: f64) -> i64 {
    (kelvin.round() as i64) - 273
}

fn u16_array_value(value: &OwnedValue) -> Option<Vec<u16>> {
    Vec::<u16>::try_from(value.clone()).ok()
}

fn string_array_value(value: &OwnedValue) -> Option<Vec<String>> {
    Vec::<String>::try_from(value.clone()).ok()
}

fn bool_value(value: &OwnedValue) -> Option<bool> {
    bool::try_from(value.clone()).ok()
}

fn string_value(value: &OwnedValue) -> Option<String> {
    String::try_from(value.clone()).ok()
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
        temperature_c: properties
            .get("SmartTemperature")
            .and_then(temperature_value),
        critical_warning: property(properties, "SmartCriticalWarning", string_array_value),
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
            let nvme = interfaces.get(NVME);
            let drive = interfaces.get(DRIVE);
            let ata = interfaces.get(ATA);
            if drive.is_none() && nvme.is_none() && ata.is_none() {
                continue;
            }
            let mut attributes = Properties::new();
            if nvme.is_some() {
                match zbus::Proxy::new(&connection, UDISKS_NAME, path.as_str(), NVME).await {
                    Ok(proxy) => {
                        let options = smart_options();
                        match proxy
                            .call::<_, _, HashMap<String, OwnedValue>>(
                                "SmartGetAttributes",
                                &(options,),
                            )
                            .await
                        {
                            Ok(raw) => {
                                tracing::info!(
                                    drive = %path,
                                    attributes = ?raw.iter().map(|(key, value)| {
                                        (key, value.value_signature().to_string())
                                    }).collect::<Vec<_>>(),
                                    "UDisks2 SMART attribute wire types"
                                );
                                attributes = raw;
                            }
                            Err(error) => failures.push(error.to_string()),
                        }
                    }
                    Err(error) => failures.push(error.to_string()),
                }
            }
            drives.push(decode_drive(path.as_str(), &interfaces, &attributes));
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
