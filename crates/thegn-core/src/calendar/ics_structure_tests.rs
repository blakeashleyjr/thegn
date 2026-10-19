use super::*;

#[test]
fn recurrence_id_parses_for_overrides_and_keeps_the_master() {
    let input = "BEGIN:VCALENDAR\n\
BEGIN:VEVENT\n\
UID:standup@x\n\
DTSTART;TZID=Europe/Berlin:20261012T090000\n\
DTEND;TZID=Europe/Berlin:20261012T093000\n\
RRULE:FREQ=DAILY;COUNT=5\n\
END:VEVENT\n\
BEGIN:VEVENT\n\
UID:standup@x\n\
RECURRENCE-ID;TZID=Europe/Berlin:20261013T090000\n\
DTSTART;TZID=Europe/Berlin:20261013T110000\n\
DTEND;TZID=Europe/Berlin:20261013T113000\n\
END:VEVENT\n\
BEGIN:VEVENT\n\
UID:standup@x\n\
RECURRENCE-ID;VALUE=DATE:20261014\n\
DTSTART;VALUE=DATE:20261014\n\
DTEND;VALUE=DATE:20261015\n\
END:VEVENT\n\
END:VCALENDAR";
    let events = parse_ics(input, "UTC");
    assert_eq!(events.len(), 3);
    assert_eq!(events[0].recurrence_id, None);
    assert_eq!(events[0].recurrence_key(), "");
    assert_eq!(events[0].id().as_str(), "/standup@x");
    assert_eq!(events[1].recurrence_key(), "20261013T070000Z");
    assert_eq!(events[2].recurrence_key(), "20261014");
    assert!(events[1].recurrence_id.is_some());
    assert!(events[2].recurrence_id.is_some());
    assert_ne!(events[0].id(), events[1].id());
    assert_ne!(events[0].id(), events[2].id());
    assert_ne!(events[1].id(), events[2].id());
}

#[test]
fn duplicate_recurrence_id_quarantines_vevent() {
    let input = one_event(
        "UID:x\nDTSTART:20261012T090000Z\nRECURRENCE-ID:20261012T090000Z\nRECURRENCE-ID:20261013T090000Z",
    );
    assert!(parse_ics(&input, "UTC").is_empty());

    let positive_control =
        one_event("UID:x\nDTSTART:20261012T090000Z\nRECURRENCE-ID:20261012T090000Z");
    let events = parse_ics(&positive_control, "UTC");
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].recurrence_key(), "20261012T090000Z");
}

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
    assert_eq!(events.len(), 1);
    assert_legacy_matches(&input, &events);
}

#[test]
fn duplicate_shape_singletons_quarantine_the_event() {
    let input = one_event("UID:x\nDTSTART;VALUE=DATE:20261012\nDTSTART;VALUE=DATE:20261020");
    let (events, report) = parse_report(&input);
    assert!(events.is_empty());
    assert!(report.diagnostics().contains(&IcsDiagnostic::InvalidEvent));
    assert!(
        report
            .diagnostics()
            .contains(&IcsDiagnostic::DuplicateProperty("DTSTART"))
    );
    assert_legacy_matches(&input, &events);
}

#[test]
fn invalid_dtend_quarantines_instead_of_defaulting() {
    let input = one_event("UID:x\nDTSTART:20261012T090000Z\nDTEND:garbage");
    let (events, report) = parse_report(&input);
    assert!(events.is_empty());
    assert!(report.diagnostics().contains(&IcsDiagnostic::InvalidEvent));
    assert_legacy_matches(&input, &events);
}

#[test]
fn invalid_duration_quarantines_instead_of_defaulting() {
    let input = one_event("UID:x\nDTSTART:20261012T090000Z\nDURATION:bogus");
    let (events, report) = parse_report(&input);
    assert!(events.is_empty());
    assert!(report.diagnostics().contains(&IcsDiagnostic::InvalidEvent));
    assert_legacy_matches(&input, &events);
}

#[test]
fn duration_parser_accepts_rfc_units_and_rejects_malformed_or_overflowing_values() {
    assert_eq!(parse_duration("+P1W"), Some(chrono::Duration::days(7)));
    assert_eq!(
        parse_duration("P1DT2H3M4S"),
        Some(chrono::Duration::seconds(93_784))
    );
    assert_eq!(
        parse_duration("-PT30S"),
        Some(chrono::Duration::seconds(-30))
    );
    for invalid in [
        "",
        "P",
        "P1",
        "P1DT",
        "PT",
        "P1D1D",
        "P1WD",
        "PT1M1H",
        "P1DT1HT1M",
    ] {
        assert!(parse_duration(invalid).is_none(), "accepted {invalid:?}");
    }
    assert!(parse_duration("PT9223372036854775807S").is_none());
}

#[test]
fn invalid_rrule_quarantines_instead_of_becoming_one_off() {
    let input = one_event("UID:x\nDTSTART:20261012T090000Z\nRRULE:FREQ=DAILY;INTERVAL=0");
    let (events, report) = parse_report(&input);
    assert!(events.is_empty());
    assert!(report.diagnostics().contains(&IcsDiagnostic::InvalidEvent));
    assert_legacy_matches(&input, &events);
}

#[test]
fn invalid_exdate_member_quarantines_instead_of_reappearing() {
    let input = one_event(
        "UID:x\nDTSTART:20261012T090000Z\nRRULE:FREQ=DAILY\nEXDATE:20261013T090000Z,nonsense",
    );
    let (events, report) = parse_report(&input);
    assert!(events.is_empty());
    assert!(report.diagnostics().contains(&IcsDiagnostic::InvalidEvent));
    assert_legacy_matches(&input, &events);
}

#[test]
fn empty_exdate_members_are_skipped() {
    for exdate in ["20261013T090000Z,", ",20261013T090000Z,,", ""] {
        let input = one_event(&format!(
            "UID:x\nDTSTART:20261012T090000Z\nRRULE:FREQ=DAILY\nEXDATE:{exdate}"
        ));
        let (events, report) = parse_report(&input);
        assert_eq!(events.len(), 1, "EXDATE {exdate:?}");
        assert!(
            !report.diagnostics().contains(&IcsDiagnostic::InvalidEvent),
            "EXDATE {exdate:?}"
        );
    }
}

#[test]
fn conflicting_repeated_timezone_parameter_quarantines_event() {
    let input =
        one_event("UID:x\nDTSTART;TZID=America/New_York;TZID=Europe/London:20261012T090000");
    let (events, report) = parse_report(&input);
    assert!(events.is_empty());
    assert!(report.diagnostics().contains(&IcsDiagnostic::MalformedLine));
    assert!(report.diagnostics().contains(&IcsDiagnostic::InvalidEvent));
    assert_legacy_matches(&input, &events);
}

#[test]
fn identical_repeated_timezone_parameter_is_accepted() {
    let line = parse_line("DTSTART;TZID=Europe/London;TZID=Europe/London:20261012T090000");
    assert!(line.is_some());
    assert_eq!(
        line.and_then(|line| line.params.get("TZID").cloned()),
        Some("Europe/London".to_string())
    );
}

#[test]
fn malformed_shape_property_quarantines_only_its_event() {
    let input = "BEGIN:VCALENDAR\nBEGIN:VEVENT\nUID:bad\nDTSTART;VALUE=DATE:20261012\nRRULE;broken\nEND:VEVENT\nBEGIN:VEVENT\nUID:good\nDTSTART;VALUE=DATE:20261013\nEND:VEVENT\nEND:VCALENDAR";
    let (events, report) = parse_report(input);
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].uid, "good");
    assert!(report.diagnostics().contains(&IcsDiagnostic::MalformedLine));
    assert!(report.diagnostics().contains(&IcsDiagnostic::InvalidEvent));
    assert!(!report.is_document_failure());
    assert_legacy_matches(input, &events);
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

#[test]
fn mixed_feed_reports_invalid_rdate_and_keeps_valid_event() {
    let input = "BEGIN:VCALENDAR\nBEGIN:VEVENT\nUID:good\nDTSTART;VALUE=DATE:20261012\nRRULE:FREQ=DAILY;COUNT=2\nEND:VEVENT\nBEGIN:VEVENT\nUID:bad\nDTSTART;VALUE=DATE:20261012\nRRULE:FREQ=DAILY;COUNT=2\nRDATE:20261015T090000Z\nEND:VEVENT\nEND:VCALENDAR";
    let (events, report) = parse_report(input);
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].uid, "good");
    assert!(
        report
            .diagnostics()
            .contains(&IcsDiagnostic::InvalidRecurrenceValue(
                super::super::recur::RecurValueError {
                    property: super::super::recur::RecurProperty::Rdate,
                    index: 0,
                    defect: super::super::recur::RecurValueDefect::ShapeMismatch,
                }
            ))
    );
    assert!(report.diagnostics().contains(&IcsDiagnostic::InvalidEvent));
    assert!(!report.is_document_failure());
    assert_legacy_matches(input, &events);
}

#[test]
fn invalid_recurrence_value_reports_member_index() {
    let input = one_event(
        "UID:indexed\nDTSTART;VALUE=DATE:20261012\nRRULE:FREQ=DAILY\nRDATE;VALUE=DATE:20261014\nRDATE:20261016T090000Z",
    );
    let (events, report) = parse_report(&input);
    assert!(events.is_empty());
    assert!(
        report
            .diagnostics()
            .contains(&IcsDiagnostic::InvalidRecurrenceValue(
                super::super::recur::RecurValueError {
                    property: super::super::recur::RecurProperty::Rdate,
                    index: 1,
                    defect: super::super::recur::RecurValueDefect::ShapeMismatch,
                }
            ))
    );
    assert!(report.diagnostics().contains(&IcsDiagnostic::InvalidEvent));
    assert_legacy_matches(&input, &events);
}

#[test]
fn recurrence_value_diagnostic_carries_no_feed_text() {
    let input = one_event(
        "UID:private-feed-identity\nDTSTART;VALUE=DATE:20261012\nRRULE:FREQ=DAILY\nRDATE:20261015T090000Z",
    );
    let (events, report) = parse_report(&input);
    let diagnostics = format!("{:?}", report.diagnostics());
    assert!(diagnostics.contains("InvalidRecurrenceValue"));
    assert!(!diagnostics.contains("20261015T090000Z"));
    assert!(!diagnostics.contains("private-feed-identity"));
    assert_legacy_matches(&input, &events);
}
