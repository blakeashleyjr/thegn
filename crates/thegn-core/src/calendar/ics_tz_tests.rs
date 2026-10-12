use super::*;
use crate::calendar::GapPolicy;
use chrono::{TimeZone, Utc};

fn parse_reported(input: &str) -> (Vec<CalEvent>, IcsReport) {
    let mut meter = AdmissionMeter::isolated(AdmissionBudget::default());
    let mut events = Vec::new();
    let mut report = IcsReport::default();
    parse_ics_window_reported(
        input,
        "UTC",
        None,
        &mut meter,
        &mut events,
        || Ok(()),
        &mut report,
    )
    .unwrap();
    (events, report)
}

fn event(uid: &str, start: &str) -> String {
    format!("BEGIN:VEVENT\nUID:{uid}\nSUMMARY:Meeting\nDTSTART;{start}\nEND:VEVENT\n")
}

fn start_instant(event: &CalEvent) -> chrono::DateTime<Utc> {
    event
        .start
        .instant_in(chrono_tz::UTC, GapPolicy::default())
        .unwrap()
}

#[test]
fn windows_tzid_resolves_to_its_alias_instant() {
    let input = format!(
        "BEGIN:VCALENDAR\n{}END:VCALENDAR",
        event("alias", "TZID=Eastern Standard Time:20250115T090000")
    );
    let (events, _) = parse_reported(&input);
    assert_eq!(events.len(), 1);
    assert_eq!(
        start_instant(&events[0]),
        Utc.with_ymd_and_hms(2025, 1, 15, 14, 0, 0).unwrap()
    );
}

#[test]
fn custom_tzid_uses_its_x_lic_location_even_when_definition_follows_event() {
    let input = format!(
        "BEGIN:VCALENDAR\n{}BEGIN:VTIMEZONE\nTZID:Vendor/Office\nX-LIC-LOCATION:Europe/Berlin\nEND:VTIMEZONE\nEND:VCALENDAR",
        event("custom", "TZID=Vendor/Office:20250115T090000")
    );
    let (events, report) = parse_reported(&input);
    assert_eq!(events.len(), 1);
    assert_eq!(
        start_instant(&events[0]),
        Utc.with_ymd_and_hms(2025, 1, 15, 8, 0, 0).unwrap()
    );
    assert!(
        !report
            .diagnostics()
            .contains(&IcsDiagnostic::UnknownTimeZone)
    );
}

#[test]
fn unknown_tzid_quarantines_only_its_event_and_reports_value_free_diagnostic() {
    let input = format!(
        "BEGIN:VCALENDAR\n{}{}END:VCALENDAR",
        event("bad", "TZID=Mars/Olympus_Mons:20250115T090000"),
        event("good", "TZID=Europe/Berlin:20250115T090000")
    );
    let (events, report) = parse_reported(&input);
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].uid, "good");
    assert!(
        report
            .diagnostics()
            .contains(&IcsDiagnostic::UnknownTimeZone)
    );
}

#[test]
fn empty_tzid_remains_floating_and_is_not_quarantined() {
    let input = format!(
        "BEGIN:VCALENDAR\n{}END:VCALENDAR",
        event("floating", "TZID=:20250115T090000")
    );
    let (events, report) = parse_reported(&input);
    assert_eq!(events.len(), 1);
    assert!(
        !report
            .diagnostics()
            .contains(&IcsDiagnostic::UnknownTimeZone)
    );
    assert_eq!(
        start_instant(&events[0]),
        Utc.with_ymd_and_hms(2025, 1, 15, 9, 0, 0).unwrap()
    );
}

#[test]
fn vendor_path_tzid_resolves_to_the_iana_tail() {
    let input = format!(
        "BEGIN:VCALENDAR\n{}END:VCALENDAR",
        event(
            "vendor",
            "TZID=/mozilla.org/20050126_1/America/New_York:20250115T090000"
        )
    );
    let (events, _) = parse_reported(&input);
    assert_eq!(events.len(), 1);
    assert_eq!(
        start_instant(&events[0]),
        Utc.with_ymd_and_hms(2025, 1, 15, 14, 0, 0).unwrap()
    );
}

#[test]
fn timezone_definition_without_mappable_rules_is_kept_with_diagnostic() {
    let input = format!(
        "BEGIN:VCALENDAR\n{}BEGIN:VTIMEZONE\nTZID:Vendor/Custom\nEND:VTIMEZONE\nEND:VCALENDAR",
        event("custom", "TZID=Vendor/Custom:20250115T090000")
    );
    let (events, report) = parse_reported(&input);
    assert_eq!(events.len(), 1);
    assert!(
        report
            .diagnostics()
            .contains(&IcsDiagnostic::UnmappedTimeZone)
    );
}

#[test]
fn timezone_prescan_caps_bindings_and_oversized_metadata() {
    let mut input = String::from("BEGIN:VCALENDAR\n");
    for index in 0..65 {
        input.push_str(&format!(
            "BEGIN:VTIMEZONE\nTZID:Vendor/Zone{index}\nEND:VTIMEZONE\n"
        ));
    }
    input.push_str(&event("kept", "TZID=Vendor/Zone0:20250115T090000"));
    input.push_str(&event("over-cap", "TZID=Vendor/Zone64:20250115T090000"));
    input.push_str("BEGIN:VTIMEZONE\nTZID:");
    input.push_str(&"x".repeat(129));
    input.push_str("\nX-PADDING:");
    input.push_str(&"y".repeat(MAX_LINE_BYTES));
    input.push_str("\nEND:VTIMEZONE\nEND:VCALENDAR");

    let (events, report) = parse_reported(&input);
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].uid, "kept");
    assert!(!events.iter().any(|event| event.uid == "over-cap"));
    assert!(
        report
            .diagnostics()
            .contains(&IcsDiagnostic::UnmappedTimeZone)
    );
    assert!(!report.is_document_failure());
}
