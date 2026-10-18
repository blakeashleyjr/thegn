use std::time::Duration;

use super::{
    ATA, DRIVE, DiskHealthReport, Interfaces, NVME, Properties, decode_drive, finish_report,
    health_label, retry_delay, smart_failure_reason, smart_options, unavailable,
};
use zbus::names::OwnedInterfaceName;
use zbus::zvariant::{OwnedValue, Value};

fn owned(value: Value<'static>) -> OwnedValue {
    OwnedValue::try_from(value).unwrap()
}

fn properties(entries: Vec<(&str, Value<'static>)>) -> Properties {
    entries
        .into_iter()
        .map(|(key, value)| (key.to_string(), owned(value)))
        .collect()
}

fn interfaces(entries: Vec<(&str, Properties)>) -> Interfaces {
    entries
        .into_iter()
        .map(|(name, properties)| (OwnedInterfaceName::try_from(name).unwrap(), properties))
        .collect()
}

fn captured_nvme_fixture() -> (Interfaces, Properties) {
    let interfaces = interfaces(vec![
        (
            DRIVE,
            properties(vec![
                ("Model", Value::from("Example NVMe")),
                ("Serial", Value::from("SERIAL-ABCD")),
            ]),
        ),
        (
            NVME,
            properties(vec![
                ("SmartPowerOnHours", Value::U64(6_119)),
                ("SmartTemperature", Value::U16(333)),
                (
                    "SmartCriticalWarning",
                    Value::Array(Vec::<String>::new().into()),
                ),
            ]),
        ),
    ]);
    let attributes = properties(vec![
        ("percent_used", Value::U8(16)),
        ("avail_spare", Value::U8(100)),
        ("spare_thresh", Value::U8(10)),
        ("total_data_read", Value::U64(1_234_567)),
        ("total_data_written", Value::U64(9_876_543)),
        ("media_errors", Value::U64(0)),
        ("unsafe_shutdowns", Value::U64(156_193)),
        ("temp_sensors", Value::Array(vec![333_u16, 334].into())),
    ]);
    (interfaces, attributes)
}

#[test]
fn captured_nvme_wire_values_decode_metrics_properties_and_health() {
    let (interfaces, attributes) = captured_nvme_fixture();
    let drive = decode_drive(
        "/org/freedesktop/UDisks2/drives/nvme0",
        &interfaces,
        &attributes,
    );

    assert_eq!(drive.percent_used, Some(16));
    assert_eq!(drive.available_spare, Some(100));
    assert_eq!(drive.spare_threshold, Some(10));
    assert_eq!(drive.data_read_bytes, Some(1_234_567));
    assert_eq!(drive.data_written_bytes, Some(9_876_543));
    assert_eq!(drive.media_errors, Some(0));
    assert_eq!(drive.unsafe_shutdowns, Some(156_193));
    assert_eq!(drive.power_on_hours, Some(6_119));
    assert_eq!(drive.temperature_c, Some(60));
    assert_eq!(drive.serial_last4.as_deref(), Some("ABCD"));
    assert_eq!(
        drive.health.as_deref(),
        Some("healthy (no critical warnings)")
    );
    assert_eq!(drive.critical_warning, Some(Vec::new()));
}

#[test]
fn ata_wire_properties_decode_float_temperature_and_signed_bad_sector_count() {
    let interfaces = interfaces(vec![
        (
            DRIVE,
            properties(vec![
                ("Model", Value::from("")),
                ("Serial", Value::from("ATA-XYZ9")),
            ]),
        ),
        (
            ATA,
            properties(vec![
                ("SmartTemperature", Value::F64(310.15)),
                ("SmartNumBadSectors", Value::I64(0)),
                ("SmartPowerOnSeconds", Value::U64(7_200)),
                ("SmartFailing", Value::Bool(false)),
            ]),
        ),
    ]);
    let drive = decode_drive(
        "/org/freedesktop/UDisks2/drives/ata0",
        &interfaces,
        &Properties::new(),
    );

    assert_eq!(drive.name, "ata0");
    assert_eq!(drive.ata_bad_sectors, Some(0));
    assert_eq!(drive.power_on_hours, Some(2));
    assert_eq!(drive.temperature_c, Some(37));
    assert_eq!(drive.health.as_deref(), Some("healthy"));
}

#[test]
fn nonempty_nvme_critical_warning_names_are_reported_as_critical() {
    let interfaces = interfaces(vec![(
        NVME,
        properties(vec![(
            "SmartCriticalWarning",
            Value::Array(vec![String::from("spare"), String::from("temperature")].into()),
        )]),
    )]);
    let drive = decode_drive("/drive/nvme1", &interfaces, &Properties::new());

    assert_eq!(
        drive.critical_warning,
        Some(vec!["spare".into(), "temperature".into()])
    );
    assert_eq!(
        drive.health.as_deref(),
        Some("critical warning: spare, temperature")
    );
}

#[test]
fn denied_and_absent_udisks_errors_remain_unavailable_with_the_reason() {
    for reason in [
        "org.freedesktop.UDisks2 is not provided",
        "polkit authorization denied",
    ] {
        let report = unavailable(reason);
        assert_eq!(report, DiskHealthReport::Unavailable(reason.to_string()));
    }
    assert_eq!(
        smart_failure_reason(&["polkit authorization denied".to_string()]),
        Some("polkit authorization denied".to_string())
    );
    assert_eq!(health_label(None, None), None);
    let (interfaces, attributes) = captured_nvme_fixture();
    let drive = decode_drive("/drive/nvme0", &interfaces, &attributes);
    assert_eq!(
        finish_report(
            vec![drive.clone()],
            &["polkit authorization denied".to_string()]
        ),
        DiskHealthReport::Degraded {
            drives: vec![drive],
            reason: "polkit authorization denied".to_string(),
        }
    );
}

#[test]
fn smart_request_disables_interactive_authorization() {
    assert_eq!(
        smart_options()
            .get("auth.no_user_interaction")
            .and_then(|value| bool::try_from(value.clone()).ok()),
        Some(true)
    );
}

#[test]
fn retry_backoff_starts_at_five_minutes_then_uses_thirty() {
    assert_eq!(retry_delay(0), Duration::from_secs(5 * 60));
    assert_eq!(retry_delay(1), Duration::from_secs(30 * 60));
    assert_eq!(retry_delay(7), Duration::from_secs(30 * 60));
}

#[test]
#[ignore = "live UDisks2 probe"]
fn live_udisks_probe_logs_raw_attribute_keys_and_signatures() {
    match super::disk_health(Duration::from_secs(5)) {
        DiskHealthReport::Unavailable(reason) => panic!("live UDisks2 probe unavailable: {reason}"),
        DiskHealthReport::Available(drives) => assert!(!drives.is_empty()),
        DiskHealthReport::Degraded { drives, reason } => {
            assert!(!drives.is_empty());
            assert!(!reason.is_empty());
        }
    }
}
