use super::*;
use crate::calendar::{Recurrence, TzRef};
use chrono::{NaiveDate, TimeZone};

fn date_event(uid: &str, start: NaiveDate, end: NaiveDate) -> CalEvent {
    CalEvent::new(
        uid,
        "meeting",
        EventTime::Date { date: start },
        EventTime::Date { date: end },
    )
}

#[test]
fn rejects_empty_or_whitespace_uid() {
    let event = date_event(
        "  \t",
        NaiveDate::from_ymd_opt(2026, 10, 12).unwrap(),
        NaiveDate::from_ymd_opt(2026, 10, 13).unwrap(),
    );
    assert_eq!(validate_event(&event), Err(EventDefect::EmptyUid));
}

#[test]
fn rejects_mixed_date_and_timed_endpoints() {
    let date = NaiveDate::from_ymd_opt(2026, 10, 12).unwrap();
    let mut event = date_event("event", date, date);
    event.end = EventTime::Instant {
        at: chrono::Utc
            .with_ymd_and_hms(2026, 10, 12, 10, 0, 0)
            .unwrap(),
    };
    assert_eq!(validate_event(&event), Err(EventDefect::MixedShape));
}

#[test]
fn rejects_reversed_endpoints() {
    let event = date_event(
        "event",
        NaiveDate::from_ymd_opt(2026, 10, 13).unwrap(),
        NaiveDate::from_ymd_opt(2026, 10, 12).unwrap(),
    );
    assert_eq!(validate_event(&event), Err(EventDefect::Reversed));
}

#[test]
fn rejects_recurrence_values_incompatible_with_start_shape() {
    let date = NaiveDate::from_ymd_opt(2026, 10, 12).unwrap();
    let mut event = date_event("event", date, date);
    event.recurrence = Some(Recurrence {
        rdates: vec![EventTime::Instant {
            at: chrono::Utc.with_ymd_and_hms(2026, 10, 13, 9, 0, 0).unwrap(),
        }],
        ..Recurrence::default()
    });
    assert_eq!(validate_event(&event), Err(EventDefect::RecurrenceShape));
}

#[test]
fn rejects_recurrence_when_the_nominal_span_has_no_shared_frame() {
    let date = NaiveDate::from_ymd_opt(2026, 10, 12).unwrap();
    let mut event = CalEvent::new(
        "event",
        "meeting",
        EventTime::Zoned {
            local: date.and_hms_opt(9, 0, 0).unwrap(),
            zone: TzRef::new("UTC"),
        },
        EventTime::Instant {
            at: chrono::Utc
                .with_ymd_and_hms(2026, 10, 12, 10, 0, 0)
                .unwrap(),
        },
    );
    event.recurrence = Some(Recurrence {
        rdates: vec![EventTime::Instant {
            at: chrono::Utc
                .with_ymd_and_hms(2026, 10, 13, 10, 0, 0)
                .unwrap(),
        }],
        ..Recurrence::default()
    });
    assert_eq!(validate_event(&event), Err(EventDefect::RecurrenceShape));
}

#[test]
fn accepts_recurrence_members_compatible_with_start_shape() {
    let start = NaiveDate::from_ymd_opt(2026, 10, 12).unwrap();
    let next = start.succ_opt().unwrap();
    let mut event = date_event("event", start, start);
    event.recurrence = Some(Recurrence {
        rdates: vec![EventTime::Date { date: next }],
        ..Recurrence::default()
    });
    assert_eq!(validate_event(&event), Ok(()));
}

#[test]
fn accepts_valid_zero_duration_event() {
    let date = NaiveDate::from_ymd_opt(2026, 10, 12).unwrap();
    assert_eq!(validate_event(&date_event("event", date, date)), Ok(()));
}

#[test]
fn accepts_nonrecurring_zoned_endpoints_in_different_zones() {
    let date = NaiveDate::from_ymd_opt(2026, 10, 12).unwrap();
    let event = CalEvent::new(
        "flight",
        "flight",
        EventTime::Zoned {
            local: date.and_hms_opt(9, 0, 0).unwrap(),
            zone: TzRef::new("America/Los_Angeles"),
        },
        EventTime::Zoned {
            local: date.and_hms_opt(18, 0, 0).unwrap(),
            zone: TzRef::new("Europe/London"),
        },
    );
    assert_eq!(validate_event(&event), Ok(()));
}

#[test]
fn unknown_zones_are_rejected_while_blank_and_iana_zones_are_admitted() {
    let date = NaiveDate::from_ymd_opt(2026, 10, 12).unwrap();
    let local = date.and_hms_opt(9, 0, 0).unwrap();
    let end = date.and_hms_opt(10, 0, 0).unwrap();
    let zoned_event = |zone: &str| {
        CalEvent::new(
            "event",
            "meeting",
            EventTime::Zoned {
                local,
                zone: TzRef::new(zone),
            },
            EventTime::Zoned {
                local: end,
                zone: TzRef::new(zone),
            },
        )
    };

    assert_eq!(
        validate_event(&zoned_event("Nowhere/Land")),
        Err(EventDefect::UnknownZone)
    );
    let mut unknown_end = zoned_event("Europe/Berlin");
    unknown_end.end = EventTime::Zoned {
        local: end,
        zone: TzRef::new("Nowhere/Land"),
    };
    assert_eq!(validate_event(&unknown_end), Err(EventDefect::UnknownZone));
    assert_eq!(validate_event(&zoned_event("  ")), Ok(()));
    assert_eq!(validate_event(&zoned_event("Europe/Berlin")), Ok(()));
}

#[test]
fn event_defect_display_values_are_stable() {
    assert_eq!(EventDefect::EmptyUid.to_string(), "empty UID");
    assert_eq!(
        EventDefect::MixedShape.to_string(),
        "mixed all-day and timed endpoints"
    );
    assert_eq!(EventDefect::Reversed.to_string(), "end precedes start");
    assert_eq!(
        EventDefect::RecurrenceShape.to_string(),
        "incompatible recurrence values"
    );
    assert_eq!(EventDefect::UnknownZone.to_string(), "unknown time zone");
}
