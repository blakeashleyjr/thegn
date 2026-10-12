use super::*;

fn parse_report(input: &str) -> (Vec<CalEvent>, IcsReport) {
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

fn assert_legacy_matches(input: &str, events: &[CalEvent]) {
    assert_eq!(parse_ics(input, "UTC").as_slice(), events);
}

fn one_event(properties: &str) -> String {
    format!("BEGIN:VCALENDAR\nBEGIN:VEVENT\n{properties}\nEND:VEVENT\nEND:VCALENDAR")
}

#[test]
fn clean_calendar_has_no_diagnostics_and_preserves_legacy_events() {
    let input = one_event("UID:x\nDTSTART;VALUE=DATE:20261012\nSUMMARY:feed");
    let (events, report) = parse_report(&input);
    assert!(report.diagnostics().is_empty());
    assert_eq!(report.overflow(), 0);
    assert!(!report.is_document_failure());
    assert_eq!(events.len(), 1);
    assert_legacy_matches(&input, &events);
}

#[test]
fn valid_empty_calendar_is_clean() {
    let input = "BEGIN:VCALENDAR\nEND:VCALENDAR";
    let (events, report) = parse_report(input);
    assert!(events.is_empty());
    assert!(report.diagnostics().is_empty());
    assert!(!report.is_document_failure());
    assert_legacy_matches(input, &events);
}

#[test]
fn named_extension_component_is_matched_and_skipped() {
    let input = one_event(
        "DTSTART;VALUE=DATE:20261012\nBEGIN:X-PROVIDER-COMPONENT\nX-PROVIDER-DATA:ignored\nEND:X-PROVIDER-COMPONENT",
    );
    let (events, report) = parse_report(&input);
    assert!(report.diagnostics().is_empty());
    assert_eq!(events.len(), 1);
    assert_legacy_matches(&input, &events);
}

#[test]
fn malformed_line_is_reported_inside_component() {
    let input = one_event("DTSTART;VALUE=DATE:20261012\nno-colon");
    let (events, report) = parse_report(&input);
    assert!(report.diagnostics().contains(&IcsDiagnostic::MalformedLine));
    assert_legacy_matches(&input, &events);
}

#[test]
fn mismatched_end_is_document_failure() {
    let input = "BEGIN:VCALENDAR\nEND:VTODO\nEND:VCALENDAR";
    let (events, report) = parse_report(input);
    assert!(report.diagnostics().contains(&IcsDiagnostic::MismatchedEnd));
    assert!(report.is_document_failure());
    assert_legacy_matches(input, &events);
}

#[test]
fn event_end_with_open_child_reports_unclosed_child() {
    let input = "BEGIN:VCALENDAR\nBEGIN:VEVENT\nDTSTART;VALUE=DATE:20261012\nBEGIN:VALARM\nEND:VEVENT\nEND:VCALENDAR";
    let (events, report) = parse_report(input);
    assert!(report.diagnostics().contains(&IcsDiagnostic::UnclosedChild));
    assert!(report.is_document_failure());
    assert_legacy_matches(input, &events);
}

#[test]
fn nested_event_is_document_failure() {
    let input = "BEGIN:VCALENDAR\nBEGIN:VEVENT\nDTSTART;VALUE=DATE:20261012\nBEGIN:VEVENT\nDTSTART;VALUE=DATE:20261013\nEND:VEVENT\nEND:VEVENT\nEND:VCALENDAR";
    let (events, report) = parse_report(input);
    assert!(report.diagnostics().contains(&IcsDiagnostic::NestedEvent));
    assert!(report.is_document_failure());
    assert_legacy_matches(input, &events);
}

#[test]
fn event_without_start_is_reported() {
    let input = one_event("SUMMARY:no start");
    let (events, report) = parse_report(&input);
    assert!(
        report
            .diagnostics()
            .contains(&IcsDiagnostic::EventWithoutStart)
    );
    assert_legacy_matches(&input, &events);
}

#[test]
fn invalid_typed_event_value_is_reported() {
    let input = one_event("DTSTART;VALUE=DATE:not-a-date");
    let (events, report) = parse_report(&input);
    assert!(report.diagnostics().contains(&IcsDiagnostic::InvalidEvent));
    assert_legacy_matches(&input, &events);
}

#[test]
fn duplicate_singleton_property_is_reported() {
    let input = one_event("UID:first\nDTSTART;VALUE=DATE:20261012\nSUMMARY:first\nSUMMARY:second");
    let (events, report) = parse_report(&input);
    assert!(
        report
            .diagnostics()
            .contains(&IcsDiagnostic::DuplicateProperty("SUMMARY"))
    );
    assert_legacy_matches(&input, &events);
}

#[test]
fn singleton_property_set_is_recognized() {
    let input = one_event(
        "UID:x\nSUMMARY:s\nDESCRIPTION:d\nLOCATION:l\nURL:https://example.test\nSTATUS:CONFIRMED\nDTSTART;VALUE=DATE:20261012\nDTEND;VALUE=DATE:20261013\nDURATION:PT1H\nORGANIZER:mailto:a@example.test\nLAST-MODIFIED:20261012T000000Z\nDTSTAMP:20261012T000000Z",
    );
    let (events, report) = parse_report(&input);
    assert!(report.diagnostics().is_empty());
    assert_eq!(events.len(), 1);
    assert_legacy_matches(&input, &events);
}

#[test]
fn eof_with_open_calendar_is_truncated() {
    let input = "BEGIN:VCALENDAR";
    let (events, report) = parse_report(input);
    assert!(report.diagnostics().contains(&IcsDiagnostic::Truncated));
    assert!(report.is_document_failure());
    assert_legacy_matches(input, &events);
}

#[test]
fn missing_calendar_envelope_is_reported() {
    let input = "";
    let (events, report) = parse_report(input);
    assert!(
        report
            .diagnostics()
            .contains(&IcsDiagnostic::MissingEnvelope)
    );
    assert_legacy_matches(input, &events);
}

#[test]
fn diagnostics_are_capped_and_overflow_is_counted() {
    let input = format!(
        "BEGIN:VCALENDAR\n{}\nEND:VCALENDAR",
        "no-colon\n".repeat(40)
    );
    let (_, report) = parse_report(&input);
    assert_eq!(report.diagnostics().len(), MAX_ICS_DIAGNOSTICS);
    assert_eq!(report.overflow(), 8);
    assert!(report.is_document_failure());
}
