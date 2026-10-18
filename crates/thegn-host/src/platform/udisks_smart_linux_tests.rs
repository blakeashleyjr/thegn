use std::collections::HashMap;
use std::time::Duration;

use super::{
    DiskHealthReport, DriveProperties, SmartValue, decode_attributes, finish_report, health_label,
    make_disk_health, retry_delay, smart_failure_reason, smart_options, unavailable,
};

#[test]
fn captured_nvme_dictionary_decodes_expected_metrics_and_masks_serial() {
    let attributes = HashMap::from([
        ("percent_used".to_string(), SmartValue::Unsigned(16)),
        ("avail_spare".to_string(), SmartValue::Unsigned(100)),
        ("media_errors".to_string(), SmartValue::Unsigned(0)),
        (
            "unsafe_shutdowns".to_string(),
            SmartValue::Unsigned(156_193),
        ),
        ("power_on_hours".to_string(), SmartValue::Unsigned(6_119)),
        ("critical_warning".to_string(), SmartValue::Unsigned(0)),
    ]);
    let drive = make_disk_health(
        "/org/freedesktop/UDisks2/drives/nvme0",
        DriveProperties {
            model: Some("Example NVMe".to_string()),
            serial: Some("SERIAL-ABCD".to_string()),
            temperature_kelvin: Some(333),
            ..DriveProperties::default()
        },
        &attributes,
    );

    assert_eq!(drive.percent_used, Some(16));
    assert_eq!(drive.available_spare, Some(100));
    assert_eq!(drive.media_errors, Some(0));
    assert_eq!(drive.unsafe_shutdowns, Some(156_193));
    assert_eq!(drive.power_on_hours, Some(6_119));
    assert_eq!(drive.temperature_c, Some(60));
    assert_eq!(drive.serial_last4.as_deref(), Some("ABCD"));
    assert_eq!(
        drive.health.as_deref(),
        Some("healthy (no critical warnings)")
    );
    assert_eq!(drive.critical_warning, Some(0));
    assert_eq!(decode_attributes(&attributes).spare_threshold, None);
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
    let drive = make_disk_health(
        "/org/freedesktop/UDisks2/drives/nvme0",
        DriveProperties {
            temperature_kelvin: Some(333),
            ..DriveProperties::default()
        },
        &HashMap::new(),
    );
    assert_eq!(
        finish_report(vec![drive], &["polkit authorization denied".to_string()]),
        DiskHealthReport::Degraded {
            drives: vec![make_disk_health(
                "/org/freedesktop/UDisks2/drives/nvme0",
                DriveProperties {
                    temperature_kelvin: Some(333),
                    ..DriveProperties::default()
                },
                &HashMap::new(),
            )],
            reason: "polkit authorization denied".to_string(),
        }
    );
}

#[test]
fn smart_request_disables_interactive_authorization() {
    assert!(matches!(
        smart_options().get("auth.no_user_interaction"),
        Some(zbus::zvariant::Value::Bool(true))
    ));
}

#[test]
fn retry_backoff_starts_at_five_minutes_then_uses_thirty() {
    assert_eq!(retry_delay(0), Duration::from_secs(5 * 60));
    assert_eq!(retry_delay(1), Duration::from_secs(30 * 60));
    assert_eq!(retry_delay(7), Duration::from_secs(30 * 60));
}

#[test]
fn live_udisks_probe_degrades_when_service_or_authorization_is_unavailable() {
    match super::disk_health(Duration::from_secs(5)) {
        // Without a system bus, UDisks2, or an authorization grant, the live
        // probe is intentionally treated as an unavailable optional source.
        DiskHealthReport::Unavailable(_) => return,
        DiskHealthReport::Available(drives) => assert!(!drives.is_empty()),
        DiskHealthReport::Degraded { drives, reason } => {
            assert!(!drives.is_empty());
            assert!(!reason.is_empty());
        }
    }
}
