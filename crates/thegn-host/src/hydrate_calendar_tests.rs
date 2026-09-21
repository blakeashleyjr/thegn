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

/// A complete page admitted against a private pool.
fn page(events: Vec<CalEvent>, deleted: Vec<String>, sync_token: &str) -> EventPage {
    EventPage::try_new(
        events,
        deleted,
        sync_token,
        &thegn_svc::calendar::AccountAdmission::isolated(
            thegn_core::calendar::AdmissionBudget::default(),
        ),
    )
    .unwrap()
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
    let full = page(vec![event("e1")], vec![], "");
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
        &page(vec![event("e1")], vec![], "etag-1"),
        from,
        to,
    );
    let before = t.db.get_calendar_sync("work").unwrap().unwrap().fetched_at;

    let not_modified = EventPage::unchanged("etag-1");
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
        &page(vec![event("a"), event("b")], vec![], ""),
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
        &page(vec![updated], vec!["b".into()], "tok-2"),
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
        &page(vec![event("a"), event("b")], vec![], ""),
        from,
        to,
    );
    apply_page(
        &t.db,
        "work",
        "ics",
        &page(vec![event("c")], vec![], ""),
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
        &page(vec![recurring, event("once")], vec![], ""),
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
        &page(vec![event("good")], vec![], ""),
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
fn remote_transport_failures_persist_only_redacted_calendar_diagnostics() {
    let t = TmpDb::new("redacted-remote-error");
    thegn_core::connectivity::report_success();
    let cfg = CalendarConfig {
        accounts: vec![thegn_core::config_calendar::CalendarAccount {
            name: "remote".into(),
            provider: thegn_core::config_calendar::CalendarProviderKind::IcsUrl,
            url: "http://127.0.0.1/feed?subscription-secret=opaque".into(),
            token: "sync-token-secret".into(),
            ..Default::default()
        }],
        ..CalendarConfig::default()
    };
    let (from, to) = window();
    assert!(!sync_accounts(&t.db, &cfg, from, to, true, &mut |_| {}));
    let sync = t.db.get_calendar_sync("remote").unwrap().unwrap();
    assert!(sync.last_error.contains("calendar destination refused"));
    assert!(!sync.last_error.contains("subscription-secret"));
    assert!(!sync.last_error.contains("sync-token-secret"));
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
        &page(vec![event("e1")], vec![], ""),
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

#[test]
fn an_over_budget_source_keeps_the_prior_cache_and_cursor() {
    // Truncating and publishing would replace the whole account with a prefix
    // and advance the cursor past the events that were cut — lost for good.
    // Overflow must instead leave both untouched and say why.
    let t = TmpDb::new("over-budget");
    let (from, to) = window();
    apply_page(
        &t.db,
        "work",
        "ics",
        &page(vec![event("keep-1"), event("keep-2")], vec![], "cursor-1"),
        from,
        to,
    );
    let mut feed = String::from("BEGIN:VCALENDAR\r\n");
    for i in 0..5 {
        feed.push_str(&format!(
            "BEGIN:VEVENT\r\nUID:new{i}\r\nDTSTART:20260821T090000Z\r\nEND:VEVENT\r\n"
        ));
    }
    feed.push_str("END:VCALENDAR\r\n");
    let file = t.dir.join("feed.ics");
    std::fs::write(&file, feed).unwrap();
    let cfg = CalendarConfig {
        max_events: 3,
        accounts: vec![thegn_core::config_calendar::CalendarAccount {
            name: "work".into(),
            provider: thegn_core::config_calendar::CalendarProviderKind::Ics,
            path: file.display().to_string(),
            ..Default::default()
        }],
        ..CalendarConfig::default()
    };
    let mut toasts = Vec::new();
    assert!(!sync_accounts(&t.db, &cfg, from, to, true, &mut |m| {
        toasts.push(m)
    }));
    // The refusal is visible: one toast naming the account and the knob.
    assert_eq!(toasts.len(), 1, "{toasts:?}");
    assert!(toasts[0].contains("\"work\""), "{toasts:?}");
    assert!(toasts[0].contains("max_events"), "{toasts:?}");
    // The same condition on the next sync is not re-announced.
    assert!(!sync_accounts(&t.db, &cfg, from, to, true, &mut |m| {
        toasts.push(m)
    }));
    assert_eq!(toasts.len(), 1, "{toasts:?}");
    let mut uids: Vec<_> = load_cached(&t.db, from, to)
        .into_iter()
        .map(|e| e.uid)
        .collect();
    uids.sort();
    assert_eq!(uids, vec!["keep-1", "keep-2"]);
    let sync = t.db.get_calendar_sync("work").unwrap().unwrap();
    assert_eq!(sync.sync_token, "cursor-1", "the cursor must not advance");
    assert!(sync.last_error.contains("max_events"), "{sync:?}");

    // Within the budget the same source replaces the cache normally.
    let cfg = CalendarConfig {
        max_events: 5,
        ..cfg
    };
    assert!(sync_accounts(&t.db, &cfg, from, to, true, &mut |_| {}));
    assert_eq!(load_cached(&t.db, from, to).len(), 5);
}

#[test]
fn a_contention_refusal_backs_the_account_off_instead_of_stamping_it() {
    // A shared-budget refusal records nothing in the DB (a stamp would look
    // like an attempt and hold the account back for `ttl_secs`), so the
    // backoff is what stops every popup open re-fetching it.
    let account = format!("contended-{}", std::process::id());
    let now = thegn_core::util::now();
    assert!(!contention_backoff(&account, now));
    note_contention(&account, now);
    assert!(contention_backoff(&account, now));
    assert!(contention_backoff(
        &account,
        now + CONTENTION_BACKOFF_SECS - 1
    ));
    // Once it lapses the account is eligible again, and the entry is dropped.
    assert!(!contention_backoff(&account, now + CONTENTION_BACKOFF_SECS));
    assert!(
        !contention_seen()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .contains_key(&account)
    );
}

#[test]
fn a_contention_refusal_leaves_the_accounts_record_untouched() {
    let t = TmpDb::new("contention-record");
    let (from, to) = window();
    // A pid-scoped name: the backoff map is process-wide, and `just coverage`
    // runs the whole suite in ONE process, so a literal name could couple this
    // test to another if a panic skipped its cleanup.
    let account = format!("contended-record-{}", std::process::id());
    apply_page(
        &t.db,
        &account,
        "ics_url",
        &page(vec![event("e1")], vec![], "cursor-1"),
        from,
        to,
    );
    let before = t.db.get_calendar_sync(&account).unwrap().unwrap();
    let mut toasts = Vec::new();
    record_failure(
        &t.db,
        &account,
        "ics_url",
        &CalendarError::Admission(thegn_core::calendar::AdmissionError::new(
            thegn_core::calendar::AdmissionLimit::GlobalBytes,
        )),
        &BTreeMap::new(),
        &mut |m| toasts.push(m),
    );
    let after = t.db.get_calendar_sync(&account).unwrap().unwrap();
    assert_eq!(after.fetched_at, before.fetched_at, "no attempt stamp");
    assert_eq!(after.sync_token, "cursor-1");
    assert!(after.last_error.is_empty(), "{after:?}");
    assert!(toasts.is_empty(), "contention is not the user's problem");
    // …but the account is backed off, so the popup won't re-fetch it.
    assert!(contention_backoff(&account, thegn_core::util::now()));
}
