use super::*;
use chrono::{NaiveDate, Timelike};

fn d(y: i32, m: u32, day: u32) -> NaiveDate {
    NaiveDate::from_ymd_opt(y, m, day).unwrap()
}

const SAMPLE: &str = "\
BEGIN:VCALENDAR\r
VERSION:2.0\r
X-WR-CALNAME:Work\r
BEGIN:VEVENT\r
UID:abc-123\r
SUMMARY:Standup\r
DTSTART;TZID=America/New_York:20260821T093000\r
DTEND;TZID=America/New_York:20260821T094500\r
LOCATION:Room 3\r
URL:https://example.com/meet\r
RRULE:FREQ=WEEKLY;BYDAY=MO,TU,WE,TH,FR\r
BEGIN:VALARM\r
TRIGGER:-PT10M\r
ACTION:DISPLAY\r
END:VALARM\r
END:VEVENT\r
END:VCALENDAR\r
";

#[test]
fn parses_a_realistic_event_end_to_end() {
    let evs = parse_ics(SAMPLE, "UTC");
    assert_eq!(evs.len(), 1);
    let e = &evs[0];
    assert_eq!(e.uid, "abc-123");
    assert_eq!(e.title, "Standup");
    assert_eq!(e.location, "Room 3");
    assert_eq!(e.url, "https://example.com/meet");
    assert_eq!(e.calendar, "Work", "X-WR-CALNAME names the calendar");
    assert_eq!(
        e.start,
        EventTime::Zoned {
            local: d(2026, 8, 21).and_hms_opt(9, 30, 0).unwrap(),
            zone: TzRef::new("America/New_York"),
        }
    );
    // The VALARM became a reminder, and its ACTION did not leak into the event.
    assert_eq!(e.reminders, vec![Reminder { minutes_before: 10 }]);
    let rec = e.recurrence.as_ref().unwrap();
    assert_eq!(rec.rules[0].freq, super::super::Freq::Weekly);
    assert_eq!(rec.rules[0].by_day.len(), 5);
}

#[test]
fn line_folding_is_unfolded_before_anything_else() {
    // Feeds wrap at 75 octets mid-word; without unfolding the summary is cut.
    let ics = "BEGIN:VCALENDAR\r\nBEGIN:VEVENT\r\nUID:1\r\nSUMMARY:A very long summ\r\n ary that was folded\r\nDTSTART:20260821T090000Z\r\nEND:VEVENT\r\nEND:VCALENDAR";
    let evs = parse_ics(ics, "UTC");
    assert_eq!(evs[0].title, "A very long summary that was folded");
    // Tab continuations count too.
    assert_eq!(unfold("A:1\r\n\tcont"), vec!["A:1cont"]);
    // A leading continuation with nothing to attach to is kept, not dropped.
    assert_eq!(unfold(" orphan"), vec![" orphan"]);
}

#[test]
fn text_escaping_round_trips() {
    assert_eq!(unescape(r"a\,b\;c\nd\\e"), "a,b;c\nd\\e");
    assert_eq!(escape("a,b;c\nd\\e"), r"a\,b\;c\nd\\e");
    // A trailing lone backslash must not panic or eat the string.
    assert_eq!(unescape(r"trailing\"), "trailing\\");
    // No backslash at all takes the fast path unchanged.
    assert_eq!(unescape("plain"), "plain");
}

#[test]
fn a_colon_inside_a_quoted_parameter_does_not_split_the_line() {
    // The classic parser bug: `mailto:` in a quoted param value.
    let l = parse_line(r#"ORGANIZER;CN="Smith:Jane":mailto:jane@example.com"#).unwrap();
    assert_eq!(l.name, "ORGANIZER");
    assert_eq!(l.params.get("CN").map(String::as_str), Some("Smith:Jane"));
    assert_eq!(l.value, "mailto:jane@example.com");
}

#[test]
fn a_line_without_a_colon_is_skipped_not_fatal() {
    assert!(parse_line("GARBAGE").is_none());
    assert!(parse_line(":novalue").is_none());
}

#[test]
fn property_names_are_case_insensitive() {
    let l = parse_line("dtstart;tzid=UTC:20260821T090000").unwrap();
    assert_eq!(l.name, "DTSTART");
    assert_eq!(l.params.get("TZID").map(String::as_str), Some("UTC"));
}

#[test]
fn the_three_time_shapes_parse_distinctly() {
    let date = parse_line("DTSTART;VALUE=DATE:20260821").unwrap();
    assert_eq!(
        parse_time(&date, "UTC"),
        Some(EventTime::Date {
            date: d(2026, 8, 21)
        })
    );
    // A bare YYYYMMDD is a date even without VALUE=DATE.
    let bare = parse_line("DTSTART:20260821").unwrap();
    assert!(matches!(
        parse_time(&bare, "UTC"),
        Some(EventTime::Date { .. })
    ));

    let utc = parse_line("DTSTART:20260821T093000Z").unwrap();
    assert!(matches!(
        parse_time(&utc, "UTC"),
        Some(EventTime::Instant { .. })
    ));

    let zoned = parse_line("DTSTART;TZID=Asia/Tokyo:20260821T093000").unwrap();
    match parse_time(&zoned, "UTC").unwrap() {
        EventTime::Zoned { zone, .. } => assert_eq!(zone.as_str(), "Asia/Tokyo"),
        other => panic!("expected zoned, got {other:?}"),
    }
}

#[test]
fn a_floating_time_is_anchored_to_the_accounts_zone() {
    let ics = "BEGIN:VEVENT\nUID:1\nSUMMARY:Floating\nDTSTART:20260821T093000\nEND:VEVENT";
    let evs = parse_ics(ics, "Europe/Berlin");
    match &evs[0].start {
        EventTime::Zoned { zone, .. } => assert_eq!(zone.as_str(), "Europe/Berlin"),
        other => panic!("expected zoned, got {other:?}"),
    }
}

#[test]
fn a_missing_dtend_is_derived() {
    // An all-day event with no DTEND lasts one day (exclusive end).
    let all_day = parse_ics(
        "BEGIN:VEVENT\nUID:1\nSUMMARY:Holiday\nDTSTART;VALUE=DATE:20260821\nEND:VEVENT",
        "UTC",
    );
    assert_eq!(
        all_day[0].end,
        EventTime::Date {
            date: d(2026, 8, 22)
        }
    );
    assert_eq!(
        all_day[0].dates_in(chrono_tz::Tz::UTC),
        vec![d(2026, 8, 21)]
    );

    // A DURATION is applied when there is no DTEND.
    let dur = parse_ics(
        "BEGIN:VEVENT\nUID:2\nSUMMARY:Long\nDTSTART;TZID=UTC:20260821T090000\nDURATION:PT1H30M\nEND:VEVENT",
        "UTC",
    );
    match &dur[0].end {
        EventTime::Zoned { local, .. } => {
            assert_eq!(local.hour(), 10);
            assert_eq!(local.minute(), 30);
        }
        other => panic!("expected zoned, got {other:?}"),
    }
}

#[test]
fn trigger_durations_convert_to_minutes_before() {
    assert_eq!(parse_trigger_minutes("-PT10M"), Some(10));
    assert_eq!(parse_trigger_minutes("-PT1H"), Some(60));
    assert_eq!(parse_trigger_minutes("-PT1H30M"), Some(90));
    assert_eq!(parse_trigger_minutes("-P1D"), Some(24 * 60));
    assert_eq!(parse_trigger_minutes("-P1W"), Some(7 * 24 * 60));
    // A trigger AFTER the start is not a reminder we can raise.
    assert_eq!(parse_trigger_minutes("PT10M"), None);
}

#[test]
fn a_valarm_does_not_leak_its_properties_into_the_event() {
    // VALARM has its own SUMMARY/DESCRIPTION; a parser that ignores nesting
    // silently overwrites the event's.
    let ics = "\
BEGIN:VEVENT
UID:1
SUMMARY:Real title
DTSTART:20260821T090000Z
BEGIN:VALARM
TRIGGER:-PT5M
SUMMARY:Alarm title
DESCRIPTION:Alarm body
END:VALARM
END:VEVENT";
    let evs = parse_ics(ics, "UTC");
    assert_eq!(evs[0].title, "Real title");
    assert_eq!(evs[0].description, "");
    assert_eq!(evs[0].reminders, vec![Reminder { minutes_before: 5 }]);
}

#[test]
fn a_vtimezone_component_produces_no_events() {
    let ics = "\
BEGIN:VCALENDAR
BEGIN:VTIMEZONE
TZID:America/New_York
BEGIN:DAYLIGHT
DTSTART:20260308T020000
END:DAYLIGHT
END:VTIMEZONE
END:VCALENDAR";
    assert!(parse_ics(ics, "UTC").is_empty());
}

#[test]
fn one_malformed_event_does_not_lose_the_others() {
    // The lenient contract: a feed with a broken entry still shows the rest.
    let ics = "\
BEGIN:VCALENDAR
BEGIN:VEVENT
UID:good-1
SUMMARY:Fine
DTSTART:20260821T090000Z
END:VEVENT
BEGIN:VEVENT
UID:bad
SUMMARY:No start at all
END:VEVENT
BEGIN:VEVENT
UID:good-2
SUMMARY:Also fine
DTSTART:20260822T090000Z
END:VEVENT
END:VCALENDAR";
    let evs = parse_ics(ics, "UTC");
    assert_eq!(evs.len(), 2, "the event with no DTSTART is dropped alone");
    assert_eq!(evs[0].uid, "good-1");
    assert_eq!(evs[1].uid, "good-2");
}

#[test]
fn an_event_without_a_uid_gets_a_stable_synthetic_one() {
    // Otherwise every sync would look like a full replacement.
    let ics = "BEGIN:VEVENT\nSUMMARY:Anonymous\nDTSTART:20260821T090000Z\nEND:VEVENT";
    let a = parse_ics(ics, "UTC");
    let b = parse_ics(ics, "UTC");
    assert!(!a[0].uid.is_empty());
    assert_eq!(a[0].uid, b[0].uid, "same input ⇒ same id");
    // A different event gets a different id.
    let other = parse_ics(
        "BEGIN:VEVENT\nSUMMARY:Different\nDTSTART:20260821T090000Z\nEND:VEVENT",
        "UTC",
    );
    assert_ne!(a[0].uid, other[0].uid);
}

#[test]
fn status_and_x_properties_are_preserved() {
    let ics = "\
BEGIN:VEVENT
UID:1
SUMMARY:Maybe
DTSTART:20260821T090000Z
STATUS:TENTATIVE
X-CUSTOM-THING:hello
END:VEVENT";
    let e = &parse_ics(ics, "UTC")[0];
    assert_eq!(e.status, EventStatus::Tentative);
    assert_eq!(
        e.extra.get("X-CUSTOM-THING").map(String::as_str),
        Some("hello")
    );

    let cancelled = parse_ics(
        "BEGIN:VEVENT\nUID:2\nSUMMARY:x\nDTSTART:20260821T090000Z\nSTATUS:CANCELLED\nEND:VEVENT",
        "UTC",
    );
    assert_eq!(cancelled[0].status, EventStatus::Cancelled);
}

#[test]
fn exdate_and_rdate_accept_comma_separated_lists() {
    let ics = "\
BEGIN:VEVENT
UID:1
SUMMARY:Weekly
DTSTART;TZID=UTC:20260803T090000
RRULE:FREQ=WEEKLY;BYDAY=MO
EXDATE;TZID=UTC:20260810T090000,20260817T090000
END:VEVENT";
    let e = &parse_ics(ics, "UTC")[0];
    let rec = e.recurrence.as_ref().unwrap();
    assert_eq!(rec.exdates.len(), 2);
    // And the exclusions actually take effect.
    let occ = e.occurrences(d(2026, 8, 1), d(2026, 8, 31), chrono_tz::Tz::UTC);
    let dates: Vec<_> = occ
        .iter()
        .filter_map(|o| o.start.date_in(chrono_tz::Tz::UTC))
        .collect();
    assert_eq!(dates, vec![d(2026, 8, 3), d(2026, 8, 24), d(2026, 8, 31)]);
}

#[test]
fn an_empty_document_is_not_an_error() {
    assert!(parse_ics("", "UTC").is_empty());
    assert!(parse_ics("BEGIN:VCALENDAR\nEND:VCALENDAR", "UTC").is_empty());
}

#[test]
fn a_recurring_event_expands_with_its_duration_intact() {
    let e = &parse_ics(SAMPLE, "UTC")[0];
    let occ = e.occurrences(d(2026, 8, 24), d(2026, 8, 28), chrono_tz::Tz::UTC);
    assert_eq!(occ.len(), 5, "Mon–Fri");
    for o in &occ {
        // Each instance keeps the original 15-minute wall-clock span.
        match (&o.start, &o.end) {
            (EventTime::Zoned { local: a, .. }, EventTime::Zoned { local: b, .. }) => {
                assert_eq!((*b - *a).num_minutes(), 15);
                assert_eq!(a.hour(), 9);
                assert_eq!(a.minute(), 30);
            }
            other => panic!("expected zoned pair, got {other:?}"),
        }
    }
}
