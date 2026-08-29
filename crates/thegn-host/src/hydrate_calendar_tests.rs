//! The don't-clobber rules, tested against a real DB.
//!
//! These are the failure modes that lose a user's data silently, so they are
//! worth testing at the seam that actually writes.

use super::*;
use thegn_core::calendar::{CalEvent, EventTime, TzRef};

/// An isolated DB. `Db::open` reads `XDG_STATE_HOME`, and this shell often runs
/// *inside* a live thegn, so tests must never touch the real one.
struct TmpDb {
    dir: std::path::PathBuf,
    db: Db,
}

impl TmpDb {
    fn new(tag: &str) -> TmpDb {
        let dir = std::env::temp_dir().join(format!(
            "thegn-hcal-{tag}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&dir); // best-effort: test cleanup: scratch removal must never fail the test
        std::fs::create_dir_all(&dir).unwrap();
        let db = Db::open_at(&dir.join("thegn.db")).unwrap();
        TmpDb { dir, db }
    }
}

impl Drop for TmpDb {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir); // best-effort: test cleanup: scratch removal must never fail the test
    }
}

fn d(y: i32, m: u32, day: u32) -> NaiveDate {
    NaiveDate::from_ymd_opt(y, m, day).unwrap()
}

fn event(uid: &str) -> CalEvent {
    CalEvent::new(
        uid,
        uid,
        EventTime::Zoned {
            local: d(2026, 8, 21).and_hms_opt(9, 0, 0).unwrap(),
            zone: TzRef::new("UTC"),
        },
        EventTime::Zoned {
            local: d(2026, 8, 21).and_hms_opt(10, 0, 0).unwrap(),
            zone: TzRef::new("UTC"),
        },
    )
}

fn window() -> (NaiveDate, NaiveDate) {
    (d(2026, 8, 1), d(2026, 8, 31))
}

#[test]
fn an_empty_full_fetch_does_not_erase_a_populated_cache() {
    // THE data-loss guard: a 200 with an empty body from a flaky proxy must not
    // wipe a month of meetings. Unlike an error, nothing else would warn.
    let t = TmpDb::new("empty");
    let (from, to) = window();
    let full = EventPage {
        events: vec![event("e1")],
        ..Default::default()
    };
    assert!(apply_page(&t.db, "work", "ics_url", &full, from, to));
    assert!(t.db.has_calendar_events("work").unwrap());

    let empty = EventPage::default();
    assert!(!apply_page(&t.db, "work", "ics_url", &empty, from, to));
    assert!(
        t.db.has_calendar_events("work").unwrap(),
        "the prior events must survive an empty full fetch"
    );
    // And it is recorded, so a persistently empty source is visible.
    let sync = t.db.get_calendar_sync("work").unwrap().unwrap();
    assert!(sync.last_error.contains("empty"));
}

#[test]
fn an_empty_first_fetch_is_believed() {
    // With nothing cached there is nothing to protect — an empty calendar is
    // simply an empty calendar, and must not be treated as suspicious forever.
    let t = TmpDb::new("first-empty");
    let (from, to) = window();
    assert!(!apply_page(
        &t.db,
        "work",
        "ics",
        &EventPage::default(),
        from,
        to
    ));
    assert!(!t.db.has_calendar_events("work").unwrap());
    let sync = t.db.get_calendar_sync("work").unwrap().unwrap();
    assert!(
        sync.last_error.is_empty(),
        "an honestly-empty calendar is not an error: {sync:?}"
    );
}

#[test]
fn a_304_advances_the_stamp_without_touching_the_events() {
    // Otherwise the provider gets re-hit on every single tick.
    let t = TmpDb::new("304");
    let (from, to) = window();
    apply_page(
        &t.db,
        "work",
        "ics_url",
        &EventPage {
            events: vec![event("e1")],
            sync_token: "etag-1".into(),
            ..Default::default()
        },
        from,
        to,
    );
    let before = t.db.get_calendar_sync("work").unwrap().unwrap().fetched_at;

    let not_modified = EventPage {
        sync_token: "etag-1".into(),
        unchanged: true,
        ..Default::default()
    };
    assert!(
        !apply_page(&t.db, "work", "ics_url", &not_modified, from, to),
        "nothing changed, so no repaint"
    );
    assert!(t.db.has_calendar_events("work").unwrap());
    let after = t.db.get_calendar_sync("work").unwrap().unwrap();
    assert_eq!(after.sync_token, "etag-1");
    assert!(after.fetched_at >= before, "the freshness stamp advanced");
}

#[test]
fn an_incremental_page_applies_deltas_and_tombstones() {
    let t = TmpDb::new("incr");
    let (from, to) = window();
    apply_page(
        &t.db,
        "work",
        "caldav",
        &EventPage {
            events: vec![event("a"), event("b")],
            ..Default::default()
        },
        from,
        to,
    );
    let cached = load_cached(&t.db, from, to);
    assert_eq!(cached.len(), 2);

    // A token makes it incremental: `a` is updated, `b` deleted, and the rest
    // of the cache is NOT replaced.
    let mut updated = event("a");
    updated.title = "renamed".into();
    apply_page(
        &t.db,
        "work",
        "caldav",
        &EventPage {
            events: vec![updated],
            deleted: vec!["b".into()],
            sync_token: "tok-2".into(),
            ..Default::default()
        },
        from,
        to,
    );
    let cached = load_cached(&t.db, from, to);
    assert_eq!(cached.len(), 1);
    assert_eq!(cached[0].title, "renamed");
    assert_eq!(
        t.db.get_calendar_sync("work").unwrap().unwrap().sync_token,
        "tok-2"
    );
}

#[test]
fn a_full_fetch_replaces_rather_than_merging() {
    // Without a sync token there are no tombstones, so anything absent from the
    // new page is gone — merging would resurrect deleted events forever.
    let t = TmpDb::new("full");
    let (from, to) = window();
    apply_page(
        &t.db,
        "work",
        "ics",
        &EventPage {
            events: vec![event("a"), event("b")],
            ..Default::default()
        },
        from,
        to,
    );
    apply_page(
        &t.db,
        "work",
        "ics",
        &EventPage {
            events: vec![event("c")],
            ..Default::default()
        },
        from,
        to,
    );
    let cached = load_cached(&t.db, from, to);
    assert_eq!(cached.len(), 1);
    assert_eq!(cached[0].uid, "c");
}

#[test]
fn a_recurrence_master_is_flagged_so_the_range_query_keeps_it() {
    let t = TmpDb::new("master");
    let (from, to) = window();
    let mut recurring = event("weekly");
    recurring.recurrence = Some(thegn_core::calendar::Recurrence {
        rules: vec![thegn_core::calendar::RRule::parse("FREQ=WEEKLY").unwrap()],
        ..Default::default()
    });
    apply_page(
        &t.db,
        "work",
        "ics",
        &EventPage {
            events: vec![recurring, event("once")],
            ..Default::default()
        },
        from,
        to,
    );
    // A window years away still returns the master — its old DTSTART generates
    // today's occurrences — but not the one-shot.
    let far = load_cached(&t.db, d(2030, 1, 1), d(2030, 1, 31));
    assert_eq!(far.len(), 1);
    assert_eq!(far[0].uid, "weekly");
}

#[test]
fn an_undeserializable_row_is_skipped_not_fatal() {
    // A row written by a newer schema must cost that one event, not the month.
    let t = TmpDb::new("corrupt");
    let (from, to) = window();
    apply_page(
        &t.db,
        "work",
        "ics",
        &EventPage {
            events: vec![event("good")],
            ..Default::default()
        },
        from,
        to,
    );
    t.db.put_calendar_events(
        "work",
        &[CalendarRow {
            uid: "bad".into(),
            calendar: String::new(),
            start_ms: day_ms(d(2026, 8, 21)),
            end_ms: day_ms(d(2026, 8, 21)) + 3_600_000,
            recurring: false,
            json: "{ not json at all".into(),
        }],
    )
    .unwrap();
    let cached = load_cached(&t.db, from, to);
    assert_eq!(cached.len(), 1);
    assert_eq!(cached[0].uid, "good");
}

#[test]
fn the_grid_window_is_widened_past_the_month_edges() {
    // An event on Jan 31 shows in February's first cell, so the fetch has to
    // reach outside the calendar month.
    let (from, to) = widen(d(2026, 2, 1), d(2026, 2, 28));
    assert!(from < d(2026, 2, 1));
    assert!(to > d(2026, 2, 28));
    assert_eq!(from, d(2026, 1, 25));
    assert_eq!(to, d(2026, 3, 7));
}

#[test]
fn the_sync_horizon_follows_config() {
    let cfg = CalendarConfig {
        horizon_past_days: 30,
        horizon_future_days: 90,
        ..CalendarConfig::default()
    };
    let (from, to) = horizon(&cfg, d(2026, 8, 21));
    assert_eq!(from, d(2026, 7, 22));
    assert_eq!(to, d(2026, 11, 19));
}

#[test]
fn a_recorded_failure_throttles_the_next_attempt() {
    // Without an attempt stamp, a broken provider is re-hit on every popup
    // open instead of on the normal cadence.
    let t = TmpDb::new("throttle");
    let before = thegn_core::util::now();
    t.db.set_calendar_error("work", "connection refused")
        .unwrap();
    let sync = t.db.get_calendar_sync("work").unwrap().unwrap();
    assert!(
        sync.fetched_at >= before,
        "the attempt stamp must advance: {sync:?}"
    );
    // ...but neither the events nor the resume cursor are disturbed.
    assert!(sync.sync_token.is_empty());
    assert!(!t.db.has_calendar_events("work").unwrap());
}

#[test]
fn an_empty_full_fetch_also_throttles_its_retry() {
    // The guard records an anomaly; it must throttle too, or a provider stuck
    // returning nothing is polled every time the popup opens.
    let t = TmpDb::new("empty-throttle");
    let (from, to) = window();
    apply_page(
        &t.db,
        "work",
        "ics_url",
        &EventPage {
            events: vec![event("e1")],
            ..Default::default()
        },
        from,
        to,
    );
    let before = thegn_core::util::now();
    apply_page(&t.db, "work", "ics_url", &EventPage::default(), from, to);
    let sync = t.db.get_calendar_sync("work").unwrap().unwrap();
    assert!(sync.fetched_at >= before);
    assert!(t.db.has_calendar_events("work").unwrap());
}

#[test]
fn reminders_are_inert_without_configuration() {
    // No accounts, or the switch off, must cost nothing at all — the reminder
    // slot rides the ticker and runs unconditionally.
    let off = CalendarConfig {
        reminders_enabled: false,
        ..CalendarConfig::default()
    };
    assert!(due_reminders(&off, 0).is_empty());
    let no_sources = CalendarConfig::default();
    assert!(due_reminders(&no_sources, 0).is_empty());
}
