use super::*;
use crate::calendar::GapPolicy;
use chrono::{TimeZone, Utc};

fn parse_reported(input: &str) -> (Vec<CalEvent>, IcsReport) {
    parse_reported_in(input, "UTC")
}

fn parse_reported_in(input: &str, default_zone: &str) -> (Vec<CalEvent>, IcsReport) {
    let mut meter = AdmissionMeter::isolated(AdmissionBudget::default());
    let mut events = Vec::new();
    let mut report = IcsReport::default();
    parse_ics_window_reported(
        input,
        default_zone,
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
fn timezone_definition_without_observances_quarantines_its_event() {
    let input = format!(
        "BEGIN:VCALENDAR\n{}BEGIN:VTIMEZONE\nTZID:Vendor/Custom\nEND:VTIMEZONE\nEND:VCALENDAR",
        event("custom", "TZID=Vendor/Custom:20250115T090000")
    );
    let (events, report) = parse_reported(&input);
    assert!(events.is_empty());
    assert!(
        report
            .diagnostics()
            .contains(&IcsDiagnostic::UnmappedTimeZone)
    );
    assert!(
        report
            .diagnostics()
            .contains(&IcsDiagnostic::UnknownTimeZone)
    );
}

#[test]
fn timezone_prescan_caps_bindings_and_oversized_metadata() {
    let mut input = String::from("BEGIN:VCALENDAR\n");
    for index in 0..65 {
        input.push_str(&format!(
            "BEGIN:VTIMEZONE\nTZID:Vendor/Zone{index}\nBEGIN:STANDARD\nDTSTART:20250101T000000\nTZOFFSETFROM:+0000\nTZOFFSETTO:+0000\nEND:STANDARD\nEND:VTIMEZONE\n"
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

fn cet_definition() -> &'static str {
    "BEGIN:VTIMEZONE\nTZID:Customized Time Zone\nBEGIN:DAYLIGHT\nDTSTART:19700329T020000\nTZOFFSETFROM:+0100\nTZOFFSETTO:+0200\nRRULE:FREQ=YEARLY;BYMONTH=3;BYDAY=-1SU\nEND:DAYLIGHT\nBEGIN:STANDARD\nDTSTART:19701025T030000\nTZOFFSETFROM:+0200\nTZOFFSETTO:+0100\nRRULE:FREQ=YEARLY;BYMONTH=10;BYDAY=-1SU\nEND:STANDARD\nEND:VTIMEZONE\n"
}

#[test]
fn custom_daylight_rules_match_event_year_and_default_zone_is_preferred() {
    let input = format!(
        "BEGIN:VCALENDAR\n{}{}{}END:VCALENDAR",
        event("winter", "TZID=Customized Time Zone:20250115T090000"),
        event("summer", "TZID=Customized Time Zone:20250715T090000"),
        cet_definition()
    );
    let (events, report) = parse_reported(&input);
    assert_eq!(events.len(), 2);
    assert_eq!(
        start_instant(&events[0]),
        Utc.with_ymd_and_hms(2025, 1, 15, 8, 0, 0).unwrap()
    );
    assert_eq!(
        start_instant(&events[1]),
        Utc.with_ymd_and_hms(2025, 7, 15, 7, 0, 0).unwrap()
    );
    assert!(
        !report
            .diagnostics()
            .contains(&IcsDiagnostic::UnknownTimeZone)
    );

    let (events, _) = parse_reported_in(&input, "Europe/Berlin");
    assert!(
        matches!(&events[0].start, EventTime::Zoned { zone, .. } if zone.as_str() == "Europe/Berlin")
    );
}

#[test]
fn fixed_half_hour_observance_resolves_and_unknown_rule_quarantines_locally() {
    let fixed = "BEGIN:VTIMEZONE\nTZID:Vendor/Half\nBEGIN:STANDARD\nDTSTART:20250101T000000\nTZOFFSETFROM:+0530\nTZOFFSETTO:+0530\nEND:STANDARD\nEND:VTIMEZONE\n";
    let input = format!(
        "BEGIN:VCALENDAR\n{}{}{}END:VCALENDAR",
        event("fixed", "TZID=Vendor/Half:20250115T090000"),
        event("unknown", "TZID=Vendor/Unknown:20250115T090000"),
        fixed
    );
    let (events, report) = parse_reported(&input);
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].uid, "fixed");
    assert_eq!(
        start_instant(&events[0]),
        Utc.with_ymd_and_hms(2025, 1, 15, 3, 30, 0).unwrap()
    );
    assert!(
        report
            .diagnostics()
            .contains(&IcsDiagnostic::UnknownTimeZone)
    );

    let invalid = "BEGIN:VCALENDAR\nBEGIN:VTIMEZONE\nTZID:Vendor/Bad\nBEGIN:STANDARD\nDTSTART:20250101T000000\nTZOFFSETFROM:+0000\nTZOFFSETTO:+0117\nEND:STANDARD\nEND:VTIMEZONE\n";
    let input = format!(
        "{invalid}{}{}END:VCALENDAR",
        event("bad", "TZID=Vendor/Bad:20250115T090000"),
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
fn outlook_india_fixed_offset_with_1601_onset_resolves_for_modern_event() {
    let definition = "BEGIN:VTIMEZONE\nTZID:India Standard Time\nBEGIN:STANDARD\nDTSTART:16010101T000000\nTZOFFSETFROM:+0530\nTZOFFSETTO:+0530\nEND:STANDARD\nEND:VTIMEZONE\n";
    let input = format!(
        "BEGIN:VCALENDAR\n{}{}END:VCALENDAR",
        event("india", "TZID=India Standard Time:20250115T090000"),
        definition
    );
    let (events, report) = parse_reported(&input);
    assert_eq!(events.len(), 1);
    assert_eq!(
        start_instant(&events[0]),
        Utc.with_ymd_and_hms(2025, 1, 15, 3, 30, 0).unwrap()
    );
    assert!(
        !report
            .diagnostics()
            .contains(&IcsDiagnostic::UnknownTimeZone)
    );
}

#[test]
fn malformed_vtimezone_observances_are_unmapped() {
    let open = "BEGIN:VTIMEZONE\nTZID:Vendor/Open\nBEGIN:STANDARD\nDTSTART:20250101T000000\nTZOFFSETFROM:+0000\nTZOFFSETTO:+0000\nEND:STANDARD\nBEGIN:DAYLIGHT\nDTSTART:20250301T000000\nTZOFFSETFROM:+0000\nTZOFFSETTO:+0100\nEND:VTIMEZONE\n";
    let (events, report) = parse_reported(&format!(
        "BEGIN:VCALENDAR\n{}{}END:VCALENDAR",
        event("open", "TZID=Vendor/Open:20250115T090000"),
        open
    ));
    assert!(events.is_empty());
    assert!(
        report
            .diagnostics()
            .contains(&IcsDiagnostic::UnknownTimeZone)
    );

    for duplicate in ["DTSTART", "TZOFFSETFROM", "TZOFFSETTO", "RRULE"] {
        let mut observance = String::from(
            "BEGIN:VTIMEZONE\nTZID:Vendor/Duplicate\nBEGIN:STANDARD\nDTSTART:20250101T000000\nTZOFFSETFROM:+0000\nTZOFFSETTO:+0000\n",
        );
        let repeated = match duplicate {
            "DTSTART" => "DTSTART:20250101T000000\n",
            "TZOFFSETFROM" => "TZOFFSETFROM:+0000\n",
            "TZOFFSETTO" => "TZOFFSETTO:+0000\n",
            "RRULE" => "RRULE:FREQ=YEARLY;BYMONTH=1;BYDAY=1MO\n",
            _ => unreachable!(),
        };
        observance.push_str(repeated);
        observance.push_str("END:STANDARD\nEND:VTIMEZONE\n");
        let (events, report) = parse_reported(&format!(
            "BEGIN:VCALENDAR\n{}{}END:VCALENDAR",
            event("duplicate", "TZID=Vendor/Duplicate:20250115T090000"),
            observance
        ));
        assert!(events.is_empty(), "accepted duplicate {duplicate}");
        assert!(
            report
                .diagnostics()
                .contains(&IcsDiagnostic::UnknownTimeZone),
            "missing unknown timezone diagnostic for duplicate {duplicate}"
        );
    }
}

#[test]
fn custom_rules_keep_weekly_recurrence_at_nine_local_across_dst() {
    let recurring = "BEGIN:VEVENT\nUID:weekly\nSUMMARY:Weekly\nDTSTART;TZID=Customized Time Zone:20250323T090000\nRRULE:FREQ=WEEKLY;COUNT=3\nEND:VEVENT\n";
    let input = format!(
        "BEGIN:VCALENDAR\n{recurring}{}END:VCALENDAR",
        cet_definition()
    );
    let (events, report) = parse_reported(&input);
    assert_eq!(events.len(), 1);
    assert!(
        !report
            .diagnostics()
            .contains(&IcsDiagnostic::UnknownTimeZone)
    );
    let occurrences = events[0].occurrences(
        chrono::NaiveDate::from_ymd_opt(2025, 3, 23).unwrap(),
        chrono::NaiveDate::from_ymd_opt(2025, 4, 6).unwrap(),
        chrono_tz::UTC,
    );
    assert_eq!(occurrences.len(), 3);
    let starts: Vec<_> = occurrences
        .iter()
        .map(|occurrence| {
            occurrence
                .start
                .instant_in(chrono_tz::UTC, GapPolicy::default())
                .unwrap()
        })
        .collect();
    assert_eq!(
        starts[0],
        Utc.with_ymd_and_hms(2025, 3, 23, 8, 0, 0).unwrap()
    );
    assert_eq!(
        starts[1],
        Utc.with_ymd_and_hms(2025, 3, 30, 7, 0, 0).unwrap()
    );
    assert_eq!(
        starts[2],
        Utc.with_ymd_and_hms(2025, 4, 6, 7, 0, 0).unwrap()
    );
}

#[test]
fn custom_zone_match_cache_stops_after_128_tzid_year_pairs() {
    let mut input = String::from("BEGIN:VCALENDAR\n");
    for year in 1900..2029 {
        input.push_str(&event(
            &format!("year-{year}"),
            &format!("TZID=Vendor/Fixed:{year}0115T090000"),
        ));
    }
    input.push_str("BEGIN:VTIMEZONE\nTZID:Vendor/Fixed\nBEGIN:STANDARD\nDTSTART:16010101T000000\nTZOFFSETFROM:+0000\nTZOFFSETTO:+0000\nEND:STANDARD\nEND:VTIMEZONE\nEND:VCALENDAR");
    let (events, report) = parse_reported(&input);
    assert_eq!(events.len(), 128);
    assert!(events.iter().any(|event| event.uid == "year-2027"));
    assert!(!events.iter().any(|event| event.uid == "year-2028"));
    assert!(
        report
            .diagnostics()
            .contains(&IcsDiagnostic::UnknownTimeZone)
    );
}
