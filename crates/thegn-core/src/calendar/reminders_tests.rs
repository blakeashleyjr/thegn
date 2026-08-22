use super::*;
use crate::calendar::{EventStatus, EventTime, Reminder, SourceId, TzRef};
use chrono::TimeZone;

fn at(y: i32, m: u32, d: u32, h: u32, mi: u32) -> i64 {
    Utc.with_ymd_and_hms(y, m, d, h, mi, 0)
        .unwrap()
        .timestamp_millis()
}

/// A UTC event at the given wall time, with the given reminder lead times.
fn ev(uid: &str, h: u32, mi: u32, mins: &[u32]) -> CalEvent {
    let mut e = CalEvent::new(
        uid,
        uid,
        EventTime::Zoned {
            local: chrono::NaiveDate::from_ymd_opt(2026, 8, 21)
                .unwrap()
                .and_hms_opt(h, mi, 0)
                .unwrap(),
            zone: TzRef::new("UTC"),
        },
        EventTime::Zoned {
            local: chrono::NaiveDate::from_ymd_opt(2026, 8, 21)
                .unwrap()
                .and_hms_opt(h + 1, mi, 0)
                .unwrap(),
            zone: TzRef::new("UTC"),
        },
    );
    e.source = SourceId("ics:work".into());
    e.reminders = mins
        .iter()
        .map(|m| Reminder { minutes_before: *m })
        .collect();
    e
}

const UTC: chrono_tz::Tz = chrono_tz::Tz::UTC;

#[test]
fn a_reminder_fires_on_the_tick_that_straddles_its_trigger() {
    let events = [ev("standup", 9, 30, &[10])];
    // Trigger is 09:20. A window ending before it fires nothing...
    assert!(
        due(
            &events,
            UTC,
            &[],
            at(2026, 8, 21, 9, 0),
            at(2026, 8, 21, 9, 19)
        )
        .is_empty()
    );
    // ...the window containing it fires once...
    let hit = due(
        &events,
        UTC,
        &[],
        at(2026, 8, 21, 9, 19),
        at(2026, 8, 21, 9, 21),
    );
    assert_eq!(hit.len(), 1);
    assert_eq!(hit[0].title, "standup");
    assert_eq!(hit[0].trigger_mins, 10);
    assert_eq!(hit[0].starts_in_mins, 9);
    // ...and a later window does not fire it again. This is what stops a
    // reminder repeating on every tick until the meeting starts.
    assert!(
        due(
            &events,
            UTC,
            &[],
            at(2026, 8, 21, 9, 21),
            at(2026, 8, 21, 9, 29)
        )
        .is_empty()
    );
}

#[test]
fn an_event_with_several_lead_times_fires_each_once() {
    let events = [ev("review", 15, 0, &[60, 10])];
    let hour = due(
        &events,
        UTC,
        &[],
        at(2026, 8, 21, 13, 55),
        at(2026, 8, 21, 14, 5),
    );
    assert_eq!(hour.len(), 1);
    assert_eq!(hour[0].trigger_mins, 60);

    let ten = due(
        &events,
        UTC,
        &[],
        at(2026, 8, 21, 14, 45),
        at(2026, 8, 21, 14, 55),
    );
    assert_eq!(ten.len(), 1);
    assert_eq!(ten[0].trigger_mins, 10);
}

#[test]
fn the_config_default_applies_only_when_the_source_supplied_none() {
    let no_own = [ev("plain", 9, 30, &[])];
    let with_own = [ev("explicit", 9, 30, &[5])];
    let defaults = [Reminder { minutes_before: 10 }];

    // No reminders of its own ⇒ the default's 10-minute lead is used.
    let d = due(
        &no_own,
        UTC,
        &defaults,
        at(2026, 8, 21, 9, 19),
        at(2026, 8, 21, 9, 21),
    );
    assert_eq!(d.len(), 1);
    assert_eq!(d[0].trigger_mins, 10);

    // Its own reminders win outright — the default is not *added*.
    let at_default = due(
        &with_own,
        UTC,
        &defaults,
        at(2026, 8, 21, 9, 19),
        at(2026, 8, 21, 9, 21),
    );
    assert!(at_default.is_empty(), "10-min default must not also fire");
    let at_own = due(
        &with_own,
        UTC,
        &defaults,
        at(2026, 8, 21, 9, 24),
        at(2026, 8, 21, 9, 26),
    );
    assert_eq!(at_own.len(), 1);
    assert_eq!(at_own[0].trigger_mins, 5);

    // With no defaults configured, an event without its own is silent.
    assert!(
        due(
            &no_own,
            UTC,
            &[],
            at(2026, 8, 21, 9, 0),
            at(2026, 8, 21, 9, 35)
        )
        .is_empty()
    );
}

#[test]
fn a_cancelled_event_does_not_nag() {
    let mut e = ev("dropped", 9, 30, &[10]);
    e.status = EventStatus::Cancelled;
    assert!(
        due(
            &[e],
            UTC,
            &[],
            at(2026, 8, 21, 9, 19),
            at(2026, 8, 21, 9, 21)
        )
        .is_empty()
    );
}

#[test]
fn a_clock_jump_cannot_replay_hours_of_reminders() {
    // After a suspend/resume `last_checked` can be far behind. Without the
    // catch-up clamp, every reminder from the whole gap would fire at once.
    let events = [
        ev("early", 1, 0, &[0]),
        ev("mid", 5, 0, &[0]),
        ev("recent", 9, 30, &[10]),
    ];
    let fired = due(
        &events,
        UTC,
        &[],
        at(2026, 8, 21, 0, 0), // "last checked" ten hours ago
        at(2026, 8, 21, 9, 21),
    );
    assert_eq!(fired.len(), 1, "only the last hour: {fired:?}");
    assert_eq!(fired[0].title, "recent");
}

#[test]
fn a_backwards_window_is_not_a_panic_or_a_replay() {
    let events = [ev("standup", 9, 30, &[10])];
    // now < last_checked (clock stepped back).
    let out = due(
        &events,
        UTC,
        &[],
        at(2026, 8, 21, 12, 0),
        at(2026, 8, 21, 9, 21),
    );
    assert!(out.is_empty());
}

#[test]
fn source_ref_is_unique_per_event_occurrence_and_lead_time() {
    // The restart-durable dedupe key. All three parts matter: one event can
    // have several lead times, and a recurring event fires per occurrence.
    let d = due(
        &[ev("standup", 9, 30, &[10])],
        UTC,
        &[],
        at(2026, 8, 21, 9, 19),
        at(2026, 8, 21, 9, 21),
    );
    let key = d[0].source_ref();
    assert!(key.starts_with("cal:ics:work/standup@"));
    assert!(key.ends_with("+10"));

    let other = DueReminder {
        trigger_mins: 60,
        ..d[0].clone()
    };
    assert_ne!(key, other.source_ref(), "lead time is part of identity");
    let later = DueReminder {
        occurrence_start_ms: d[0].occurrence_start_ms + 86_400_000,
        ..d[0].clone()
    };
    assert_ne!(key, later.source_ref(), "occurrence is part of identity");
}

#[test]
fn reminders_come_back_in_chronological_order() {
    let events = [ev("later", 11, 0, &[600]), ev("sooner", 10, 0, &[540])];
    // Both triggers land at 01:00 and 01:00 respectively — force one window
    // that catches both by using a wide-but-clamped hour.
    let d = due(
        &events,
        UTC,
        &[],
        at(2026, 8, 21, 0, 30),
        at(2026, 8, 21, 1, 5),
    );
    assert_eq!(d.len(), 2);
    assert!(
        d[0].occurrence_start_ms <= d[1].occurrence_start_ms,
        "sorted by when the event starts"
    );
}

#[test]
fn next_event_skips_the_past_and_the_cancelled() {
    let mut cancelled = ev("cancelled", 10, 0, &[]);
    cancelled.status = EventStatus::Cancelled;
    let events = [
        ev("past", 8, 0, &[]),
        cancelled,
        ev("next", 11, 0, &[]),
        ev("after", 14, 0, &[]),
    ];
    let now = Utc.with_ymd_and_hms(2026, 8, 21, 9, 0, 0).unwrap();
    assert_eq!(next_event(&events, UTC, now).unwrap().title, "next");

    // Nothing left today.
    let late = Utc.with_ymd_and_hms(2026, 8, 21, 23, 0, 0).unwrap();
    assert!(next_event(&events, UTC, late).is_none());
    assert!(next_event(&[], UTC, now).is_none());
}
