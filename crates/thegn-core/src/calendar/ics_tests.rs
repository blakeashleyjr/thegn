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

// --- admission ---------------------------------------------------------------

use crate::calendar::admission::{
    AdmissionBudget, AdmissionLimit, AdmissionMeter, AdmissionPool, MAX_COMPONENT_DEPTH,
    MAX_EVENT_BYTES, MAX_LINE_BYTES,
};

fn feed(n: usize) -> String {
    let mut s = String::from("BEGIN:VCALENDAR\r\n");
    for i in 0..n {
        s.push_str(&format!(
            "BEGIN:VEVENT\r\nUID:e{i}\r\nSUMMARY:Event {i}\r\nDTSTART:20260821T090000Z\r\nEND:VEVENT\r\n"
        ));
    }
    s.push_str("END:VCALENDAR\r\n");
    s
}

fn admitted(input: &str, max: usize) -> (Result<(), AdmissionError>, Vec<CalEvent>) {
    let mut meter = AdmissionMeter::isolated(AdmissionBudget::new(max).unwrap());
    let mut out = Vec::new();
    let r = parse_ics_admitted(input, "UTC", &mut meter, &mut out);
    (r, out)
}

#[test]
fn incremental_unfolding_matches_the_folding_rules() {
    let doc = "A:1\r\n B\r\n\tC\r\nD:2\n E\nF:3";
    let lines: Vec<_> = LogicalLines::new(doc).collect();
    assert_eq!(lines.len(), 3);
    assert_eq!(lines[0].text(), "A:1BC");
    assert_eq!(lines[0].len(), "A:1BC".len());
    assert_eq!(lines[1].text(), "D:2E");
    assert_eq!(lines[2].text(), "F:3");
    // An unfolded line is borrowed, never copied.
    assert!(matches!(lines[2].text(), std::borrow::Cow::Borrowed(_)));
    assert!(!lines[2].is_empty());
    // The same shapes the old whole-document unfold produced.
    assert_eq!(unfold("A\n"), vec!["A", ""]);
    assert_eq!(unfold(""), vec![""]);
}

#[test]
fn exactly_max_events_are_admitted() {
    let (r, out) = admitted(&feed(3), 3);
    r.unwrap();
    assert_eq!(out.len(), 3);
}

#[test]
fn one_event_over_the_budget_refuses_the_fetch_before_building_it() {
    let (r, out) = admitted(&feed(4), 3);
    assert_eq!(r.unwrap_err().limit, AdmissionLimit::AccountRecords);
    // The overflowing event was never materialized.
    assert_eq!(out.len(), 3);
}

#[test]
fn a_budget_of_one_admits_one() {
    let (r, _) = admitted(&feed(1), 1);
    r.unwrap();
    let (r, out) = admitted(&feed(2), 1);
    assert!(r.is_err());
    assert_eq!(out.len(), 1);
}

#[test]
fn a_huge_feed_stops_at_the_cap_not_at_the_end() {
    // Accounting is incremental: a feed of 50k events is refused after the
    // cap's worth, never after allocating all of them.
    let pool = AdmissionPool::new(1_000_000, 1 << 30);
    let mut meter = AdmissionMeter::new(AdmissionBudget::new(100).unwrap(), pool.clone());
    let mut out = Vec::new();
    let r = parse_ics_admitted(&feed(50_000), "UTC", &mut meter, &mut out);
    assert_eq!(r.unwrap_err().limit, AdmissionLimit::AccountRecords);
    assert_eq!(out.len(), 100);
    assert_eq!(pool.in_use().0, 100);
}

#[test]
fn events_without_a_start_are_not_admitted_or_charged() {
    let doc = "BEGIN:VEVENT\r\nUID:x\r\nSUMMARY:no start\r\nEND:VEVENT\r\n";
    let mut meter = AdmissionMeter::isolated(AdmissionBudget::new(1).unwrap());
    let mut out = Vec::new();
    parse_ics_admitted(doc, "UTC", &mut meter, &mut out).unwrap();
    assert!(out.is_empty());
    assert_eq!(meter.records(), 0);
    assert_eq!(meter.retained_bytes(), 0);
    // And a document truncated mid-event keeps nothing of it either.
    parse_ics_admitted("BEGIN:VEVENT\r\nSUMMARY:x\r\n", "UTC", &mut meter, &mut out).unwrap();
    assert_eq!(meter.retained_bytes(), 0);
}

#[test]
fn an_oversized_retained_property_refuses_its_event() {
    let big = "x".repeat(MAX_EVENT_BYTES);
    let doc = format!(
        "BEGIN:VEVENT\r\nUID:a\r\nDTSTART:20260821T090000Z\r\nDESCRIPTION:{big}\r\nEND:VEVENT\r\n"
    );
    let (r, out) = admitted(&doc, 10);
    assert_eq!(r.unwrap_err().limit, AdmissionLimit::EventBytes);
    assert!(out.is_empty());
}

#[test]
fn an_oversized_line_is_refused_before_it_is_unfolded() {
    // Over the line cap and folded: a retained property refuses the event…
    let mut folded = String::from("DESCRIPTION:");
    for _ in 0..(MAX_LINE_BYTES / 70 + 2) {
        folded.push_str(&"y".repeat(70));
        folded.push_str("\r\n ");
    }
    let doc = format!("BEGIN:VEVENT\r\nDTSTART:20260821T090000Z\r\n{folded}\r\nEND:VEVENT\r\n");
    let (r, _) = admitted(&doc, 10);
    assert_eq!(r.unwrap_err().limit, AdmissionLimit::EventBytes);

    // …but one we never keep (an inline attachment) is skipped, and the event
    // still arrives.
    let attach = folded.replacen("DESCRIPTION", "ATTACH;ENCODING=BASE64", 1);
    let doc =
        format!("BEGIN:VEVENT\r\nUID:a\r\nDTSTART:20260821T090000Z\r\n{attach}\r\nEND:VEVENT\r\n");
    let (r, out) = admitted(&doc, 10);
    r.unwrap();
    assert_eq!(out.len(), 1);

    // A huge calendar name outside any event is refused as a line.
    let doc = format!("X-WR-CALNAME:{}\r\n", "n".repeat(MAX_LINE_BYTES + 1));
    let (r, _) = admitted(&doc, 10);
    assert_eq!(r.unwrap_err().limit, AdmissionLimit::LineBytes);
    // An oversized line of an unknown shape is simply skipped.
    let (r, _) = admitted(&"z".repeat(MAX_LINE_BYTES + 1), 10);
    r.unwrap();
    // Inside a VALARM only TRIGGER is kept.
    let doc = format!(
        "BEGIN:VEVENT\r\nDTSTART:20260821T090000Z\r\nBEGIN:VALARM\r\nTRIGGER:{}\r\nEND:VALARM\r\nEND:VEVENT\r\n",
        "-".repeat(MAX_LINE_BYTES + 1)
    );
    assert_eq!(
        admitted(&doc, 10).0.unwrap_err().limit,
        AdmissionLimit::EventBytes
    );
    let doc = doc.replace("TRIGGER", "DESCRIPTION");
    admitted(&doc, 10).0.unwrap();
}

#[test]
fn the_calendar_name_copy_is_charged_to_every_event() {
    // A large X-WR-CALNAME is copied into each event; the copies are what
    // amplify, so they are what is counted.
    // 256 KiB fits one event, but 128 copies fill the 32 MiB account budget.
    let name = "n".repeat(256 * 1024);
    let doc = format!("X-WR-CALNAME:{name}\r\n{}", feed(200));
    let (r, out) = admitted(&doc, 200);
    assert_eq!(r.unwrap_err().limit, AdmissionLimit::AccountBytes);
    assert!(
        out.len() < 128,
        "stopped at the byte budget, got {}",
        out.len()
    );
    // A name that alone fills an event's budget refuses the first event.
    let doc = format!(
        "X-WR-CALNAME:{}\r\n{}",
        "n".repeat(MAX_LINE_BYTES - 20),
        feed(1)
    );
    assert_eq!(
        admitted(&doc, 10).0.unwrap_err().limit,
        AdmissionLimit::EventBytes
    );
}

#[test]
fn child_values_are_bounded_per_event() {
    let dates: Vec<String> = (0..20_000)
        .map(|_| "20260821T090000Z".to_string())
        .collect();
    let doc = format!(
        "BEGIN:VEVENT\r\nDTSTART:20260821T090000Z\r\nEXDATE:{}\r\nEND:VEVENT\r\n",
        dates[..1_000].join(",")
    );
    admitted(&doc, 10).0.unwrap();
    let mut doc = String::from("BEGIN:VEVENT\r\nDTSTART:20260821T090000Z\r\n");
    for chunk in dates.chunks(1_000) {
        doc.push_str(&format!("RDATE:{}\r\n", chunk.join(",")));
    }
    doc.push_str("END:VEVENT\r\n");
    // Each child also carries bookkeeping bytes, so whichever per-event
    // ceiling is reached first refuses it — never a silently shortened list.
    let per_event = |l| {
        matches!(
            l,
            AdmissionLimit::EventChildren | AdmissionLimit::EventBytes
        )
    };
    assert!(per_event(admitted(&doc, 10).0.unwrap_err().limit));
    let mut doc = String::from("BEGIN:VEVENT\r\nDTSTART:20260821T090000Z\r\n");
    for i in 0..20_000 {
        doc.push_str(&format!("X-K{i}:v\r\n"));
    }
    doc.push_str("END:VEVENT\r\n");
    assert!(per_event(admitted(&doc, 10).0.unwrap_err().limit));
}

#[test]
fn component_nesting_is_bounded() {
    let mut doc = String::from("BEGIN:VEVENT\r\nDTSTART:20260821T090000Z\r\n");
    for _ in 0..MAX_COMPONENT_DEPTH {
        doc.push_str("BEGIN:X-THING\r\n");
    }
    let ok = format!(
        "{doc}{}END:VEVENT\r\n",
        "END:X-THING\r\n".repeat(MAX_COMPONENT_DEPTH)
    );
    let (r, out) = admitted(&ok, 10);
    r.unwrap();
    assert_eq!(out.len(), 1);
    doc.push_str("BEGIN:X-THING\r\n");
    assert_eq!(
        admitted(&doc, 10).0.unwrap_err().limit,
        AdmissionLimit::Nesting
    );
}

#[test]
fn the_fixture_wrapper_never_returns_a_truncated_calendar() {
    // Over the default budget: nothing rather than a prefix.
    assert!(parse_ics(&feed(AdmissionBudget::default().max_events() + 1), "UTC").is_empty());
}

fn windowed(input: &str, max: usize) -> (Result<(), AdmissionError>, Vec<CalEvent>) {
    let mut meter = AdmissionMeter::isolated(AdmissionBudget::new(max).unwrap());
    let mut out = Vec::new();
    let r = parse_ics_window(
        input,
        "UTC",
        Some((d(2026, 8, 1), d(2026, 8, 31))),
        &mut meter,
        &mut out,
    );
    // Nothing outside the window stays charged.
    assert_eq!(meter.records(), out.len());
    (r, out)
}

fn vevent(uid: &str, body: &str) -> String {
    format!("BEGIN:VEVENT\r\nUID:{uid}\r\n{body}END:VEVENT\r\n")
}

#[test]
fn whole_document_history_outside_the_window_is_not_admitted() {
    // Years of past one-offs plus one event this month: only the one counts,
    // so a long-history subscribed feed fits a small budget.
    let mut doc = String::from("BEGIN:VCALENDAR\r\n");
    for i in 0..5_000 {
        doc.push_str(&vevent(
            &format!("old{i}"),
            "DTSTART:20200105T090000Z\r\nDTEND:20200105T100000Z\r\n",
        ));
    }
    doc.push_str(&vevent("now", "DTSTART:20260821T090000Z\r\n"));
    doc.push_str(&vevent("future", "DTSTART:20300101T090000Z\r\n"));
    doc.push_str("END:VCALENDAR\r\n");
    let (r, out) = windowed(&doc, 1);
    r.unwrap();
    assert_eq!(out.len(), 1);
    assert_eq!(out[0].uid, "now");
}

#[test]
fn anything_that_can_reach_the_window_is_admitted() {
    let doc = [
        // An open-ended weekly series from years ago.
        vevent("weekly", "DTSTART:20200106T090000Z\r\nRRULE:FREQ=WEEKLY\r\n"),
        // A counted series: can't be ruled out without expanding it.
        vevent("counted", "DTSTART:20200106T090000Z\r\nRRULE:FREQ=DAILY;COUNT=5\r\n"),
        // A long event that started before the window and ends inside it.
        vevent("long", "DTSTART;VALUE=DATE:20260701\r\nDTEND;VALUE=DATE:20260805\r\n"),
        // An old event with an extra date inside the window.
        vevent("rdate", "DTSTART:20200106T090000Z\r\nRDATE:20260815T090000Z\r\n"),
        // A series whose UNTIL plus its length reaches into the window.
        vevent(
            "until-span",
            "DTSTART;VALUE=DATE:20260601\r\nDTEND;VALUE=DATE:20260710\r\nRRULE:FREQ=MONTHLY;UNTIL=20260701T000000Z\r\n",
        ),
        // A one-off exactly on the edge (a day of zone slack).
        vevent("edge", "DTSTART:20260731T233000Z\r\nDTEND:20260731T235900Z\r\n"),
    ]
    .concat();
    let (r, out) = windowed(&doc, 10);
    r.unwrap();
    let uids: Vec<_> = out.iter().map(|e| e.uid.as_str()).collect();
    assert_eq!(
        uids,
        ["weekly", "counted", "long", "rdate", "until-span", "edge"]
    );
}

#[test]
fn a_series_that_ended_before_the_window_is_not_admitted() {
    let doc = [
        vevent(
            "ended",
            "DTSTART:20200106T090000Z\r\nRRULE:FREQ=WEEKLY;UNTIL=20210101T000000Z\r\n",
        ),
        vevent(
            "starts-later",
            "DTSTART:20300106T090000Z\r\nRRULE:FREQ=WEEKLY\r\n",
        ),
        vevent("no-start", "SUMMARY:x\r\n"),
    ]
    .concat();
    let (r, out) = windowed(&doc, 1);
    r.unwrap();
    assert!(out.is_empty());
}

#[test]
fn an_oversized_line_with_a_folded_name_is_refused_inside_an_event() {
    // `DESCRIP` + fold + `TION:…`: the name can't be read from the first
    // physical line, so the line is refused rather than guessed skippable.
    let mut line = String::from("DESCRIP\r\n TION:");
    for _ in 0..(MAX_LINE_BYTES / 70 + 2) {
        line.push_str(&"y".repeat(70));
        line.push_str("\r\n ");
    }
    let doc = format!("BEGIN:VEVENT\r\nDTSTART:20260821T090000Z\r\n{line}\r\nEND:VEVENT\r\n");
    assert_eq!(
        admitted(&doc, 10).0.unwrap_err().limit,
        AdmissionLimit::EventBytes
    );
    // An oversized BEGIN inside an event would desynchronize the component
    // stack if skipped; refused too.
    let doc = format!(
        "BEGIN:VEVENT\r\nDTSTART:20260821T090000Z\r\nBEGIN:VALARM\r\nBEGIN:{}\r\nEND:VALARM\r\nEND:VEVENT\r\n",
        "X".repeat(MAX_LINE_BYTES + 1)
    );
    assert_eq!(
        admitted(&doc, 10).0.unwrap_err().limit,
        AdmissionLimit::EventBytes
    );
    // Outside any event the same shapes are skipped harmlessly.
    let doc = format!("{line}\r\n{}", feed(1));
    let (r, out) = admitted(&doc, 10);
    r.unwrap();
    assert_eq!(out.len(), 1);
}
