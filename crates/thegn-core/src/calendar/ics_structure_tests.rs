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
fn blank_line_inside_event_is_ignored_without_diagnostic() {
    let input = one_event("DTSTART;VALUE=DATE:20261012\n\nSUMMARY:feed");
    let (events, report) = parse_report(&input);
    assert!(report.diagnostics().is_empty());
    assert_eq!(events.len(), 1);
    assert_legacy_matches(&input, &events);
}

#[test]
fn trigger_in_nested_extension_is_not_a_reminder() {
    let input = one_event(
        "DTSTART;VALUE=DATE:20261012\nBEGIN:VALARM\nBEGIN:X-CHILD\nTRIGGER:-PT5M\nEND:X-CHILD\nEND:VALARM",
    );
    let (events, report) = parse_report(&input);
    assert!(report.diagnostics().is_empty());
    assert_eq!(events.len(), 1);
    assert!(events[0].reminders.is_empty());
    assert_legacy_matches(&input, &events);
}

#[test]
fn oversized_trigger_in_nested_extension_is_skipped() {
    let trigger = format!("TRIGGER:{}", "-".repeat(MAX_LINE_BYTES));
    let input = one_event(&format!(
        "DTSTART;VALUE=DATE:20261012\nBEGIN:VALARM\nBEGIN:X-CHILD\n{trigger}\nEND:X-CHILD\nEND:VALARM"
    ));
    let (events, report) = parse_report(&input);
    assert!(report.diagnostics().is_empty());
    assert_eq!(events.len(), 1);
    assert!(events[0].reminders.is_empty());
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
fn unclosed_alarm_does_not_accumulate_across_events() {
    let events_input = (1..=20)
        .map(|day| {
            format!(
                "BEGIN:VEVENT\nUID:{day}\nDTSTART;VALUE=DATE:202610{day:02}\nBEGIN:VALARM\nEND:VEVENT"
            )
        })
        .collect::<Vec<_>>()
        .join("\n");
    let input = format!("BEGIN:VCALENDAR\n{events_input}\nEND:VCALENDAR");
    let (events, report) = parse_report(&input);
    assert_eq!(events.len(), 20);
    assert_eq!(
        report
            .diagnostics()
            .iter()
            .filter(|diagnostic| **diagnostic == IcsDiagnostic::UnclosedChild)
            .count(),
        20
    );
    assert_legacy_matches(&input, &events);
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
    let input = "BEGIN:VCALENDAR\nBEGIN:VEVENT\nUID:x\nSUMMARY:s\nDESCRIPTION:d\nLOCATION:l\nURL:https://example.test\nSTATUS:CONFIRMED\nDTSTART;VALUE=DATE:20261012\nDTEND;VALUE=DATE:20261013\nORGANIZER:mailto:a@example.test\nLAST-MODIFIED:20261012T000000Z\nDTSTAMP:20261012T000000Z\nEND:VEVENT\nBEGIN:VEVENT\nUID:y\nDTSTART;VALUE=DATE:20261013\nDURATION:P1D\nEND:VEVENT\nEND:VCALENDAR";
    let (events, report) = parse_report(input);
    assert!(report.diagnostics().is_empty());
    assert_eq!(events.len(), 2);
    assert_legacy_matches(input, &events);
}

#[test]
fn all_day_duration_adds_its_full_number_of_days() {
    let input = one_event("UID:x\nDTSTART;VALUE=DATE:20261012\nDURATION:P3D");
    let (events, report) = parse_report(&input);
    assert!(report.diagnostics().is_empty());
    assert_eq!(events.len(), 1);
    assert_eq!(
        events[0].end,
        EventTime::Date {
            date: NaiveDate::from_ymd_opt(2026, 10, 15).unwrap()
        }
    );
    assert_legacy_matches(&input, &events);
}

#[test]
fn non_day_all_day_duration_is_reported_and_uses_one_day_default() {
    let input = one_event("UID:x\nDTSTART;VALUE=DATE:20261012\nDURATION:PT1H");
    let (events, report) = parse_report(&input);
    assert!(report.diagnostics().contains(&IcsDiagnostic::InvalidEvent));
    assert_eq!(events.len(), 1);
    assert_eq!(
        events[0].end,
        EventTime::Date {
            date: NaiveDate::from_ymd_opt(2026, 10, 13).unwrap()
        }
    );
    assert_legacy_matches(&input, &events);
}

#[test]
fn dtend_and_duration_together_are_reported_and_dropped() {
    let input =
        one_event("UID:x\nDTSTART;VALUE=DATE:20261012\nDTEND;VALUE=DATE:20261013\nDURATION:P1D");
    let (events, report) = parse_report(&input);
    assert!(report.diagnostics().contains(&IcsDiagnostic::InvalidEvent));
    assert_eq!(
        report
            .diagnostics()
            .iter()
            .filter(|diagnostic| **diagnostic == IcsDiagnostic::InvalidEvent)
            .count(),
        1
    );
    assert!(events.is_empty());
    assert_legacy_matches(&input, &events);
}

#[test]
fn date_start_with_timed_end_is_reported_and_dropped() {
    let input = one_event("UID:x\nDTSTART;VALUE=DATE:20261012\nDTEND:20261012T100000Z");
    let (events, report) = parse_report(&input);
    assert!(report.diagnostics().contains(&IcsDiagnostic::InvalidEvent));
    assert!(events.is_empty());
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
    assert!(!report.is_document_failure());
}

#[test]
fn per_event_diagnostic_overflow_is_not_document_failure() {
    let summaries = std::iter::repeat_n("SUMMARY:duplicate", MAX_ICS_DIAGNOSTICS + 2)
        .collect::<Vec<_>>()
        .join("\n");
    let input = one_event(&format!("DTSTART;VALUE=DATE:20261012\n{summaries}"));
    let (_, report) = parse_report(&input);
    assert_eq!(report.diagnostics().len(), MAX_ICS_DIAGNOSTICS);
    assert_eq!(report.overflow(), 1);
    assert!(!report.is_document_failure());
}

#[test]
fn structural_diagnostic_overflow_remains_document_failure() {
    let mismatched_ends = std::iter::repeat_n("END:VTODO", MAX_ICS_DIAGNOSTICS + 1)
        .collect::<Vec<_>>()
        .join("\n");
    let input = format!("BEGIN:VCALENDAR\n{mismatched_ends}\nEND:VCALENDAR");
    let (_, report) = parse_report(&input);
    assert_eq!(report.diagnostics().len(), MAX_ICS_DIAGNOSTICS);
    assert_eq!(report.overflow(), 1);
    assert!(report.is_document_failure());
}
