//! The don't-clobber rules, tested against a real DB.
//!
//! These are the failure modes that lose a user's data silently, so they are
//! worth testing at the seam that actually writes.

use super::*;
use thegn_core::calendar::{CalEvent, EventTime, TzRef};

#[test]
fn periodic_sync_redelivers_the_visible_month_as_well_as_today() {
    let today = NaiveDate::from_ymd_opt(2026, 10, 19).unwrap();
    assert_eq!(
        periodic_months(today, Some((2026, 12))),
        vec![(2026, 10), (2026, 12)]
    );
    assert_eq!(periodic_months(today, Some((2026, 10))), vec![(2026, 10)]);
    assert_eq!(periodic_months(today, None), vec![(2026, 10)]);
}

#[test]
fn visible_month_round_trips_and_resets_on_open() {
    note_visible_month((2027, 1));
    assert_eq!(visible_month(), Some((2027, 1)));

    // Opening a popup records its initial month through the same seam used by
    // detail::calendar::open, replacing the previous popup's navigation.
    note_visible_month((2027, 2));
    assert_eq!(visible_month(), Some((2027, 2)));
}

#[test]
fn navigation_a_b_c_only_c_is_current() {
    let a = register_month_request((2071, 1));
    let b = register_month_request((2071, 2));
    let c = register_month_request((2071, 3));

    assert!(navigation_superseded(a));
    assert!(navigation_superseded(b));
    // The current-id assertion relies on nextest's one-process-per-test
    // isolation; a parallel plain `cargo test` case could register afterward.
    assert!(!navigation_superseded(c));
}

#[test]
fn a_superseded_month_fetch_delivers_nothing_and_never_opens_db() {
    let month = (2072, 4);
    let request = register_month_request(month);
    register_month_request((2072, 5));
    let from = NaiveDate::from_ymd_opt(month.0, month.1, 1).unwrap();
    let to = NaiveDate::from_ymd_opt(month.0, month.1, 30).unwrap();
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let mut opened = false;

    run_month_fetch(
        CalendarConfig::default(),
        MonthCoverage {
            year: month.0,
            month: month.1,
            covered: widen(from, to),
        },
        request,
        tx,
        None,
        || {
            opened = true;
            None
        },
    );

    assert!(!opened);
    assert!(rx.try_recv().is_err());
}

#[test]
fn dropped_month_completion_sends_cache_unavailable_payload() {
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let completion = MonthFetchCompletion {
        tx,
        waker: None,
        coverage: MonthCoverage {
            year: 2026,
            month: 10,
            covered: widen(
                NaiveDate::from_ymd_opt(2026, 10, 1).unwrap(),
                NaiveDate::from_ymd_opt(2026, 10, 31).unwrap(),
            ),
        },
        request: 0,
        sent: false,
    };
    drop(completion);

    let result = rx.try_recv().unwrap();
    let RefreshKind::CalendarMonth(payload) = result else {
        panic!("completion should send a calendar month payload");
    };
    assert_eq!(payload.month, (2026, 10));
    assert_eq!(payload.error, Some(CalendarViewError::CacheUnavailable));
}

#[test]
fn refused_month_fetch_sends_cache_unavailable_payload() {
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    deliver_unavailable_if_refused(
        crate::sched::Admission::Saturated,
        &tx,
        None,
        MonthCoverage {
            year: 2026,
            month: 10,
            covered: widen(
                NaiveDate::from_ymd_opt(2026, 10, 1).unwrap(),
                NaiveDate::from_ymd_opt(2026, 10, 31).unwrap(),
            ),
        },
        None,
    );

    let result = rx.try_recv().unwrap();
    let RefreshKind::CalendarMonth(payload) = result else {
        panic!("refused fetch should send a calendar month payload");
    };
    assert_eq!(payload.month, (2026, 10));
    assert_eq!(payload.error, Some(CalendarViewError::CacheUnavailable));
}

#[test]
fn delivered_month_payload_carries_its_widened_coverage() {
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    deliver(
        &tx,
        None,
        MonthCoverage {
            year: 2026,
            month: 8,
            covered: widen(
                NaiveDate::from_ymd_opt(2026, 8, 1).unwrap(),
                NaiveDate::from_ymd_opt(2026, 8, 31).unwrap(),
            ),
        },
        MonthView {
            events: Some(Vec::new()),
            error: None,
        },
        None,
        None,
    );
    let RefreshKind::CalendarMonth(payload) = rx.try_recv().unwrap() else {
        panic!("delivery should contain a calendar month payload");
    };
    let (from, to) = thegn_core::calendar::month_bounds(2026, 8).unwrap();
    assert_eq!(payload.covered, widen(from, to));
    assert_eq!(payload.request, None);
}

#[test]
fn calendar_connectivity_evidence_uses_transport_and_retryability_separately() {
    let success = Ok(EventPage::default());
    let command_timeout = Err(CalendarError::Timeout(
        "calendar command exceeded its deadline",
    ));
    let remote_timeout = Err(CalendarError::Timeout("remote request timed out"));
    let remote_success = Some(true);
    assert_eq!(connectivity_evidence(false, &success), None);
    assert_eq!(connectivity_evidence(false, &command_timeout), None);
    assert_eq!(connectivity_evidence(true, &success), remote_success);
    assert_eq!(connectivity_evidence(true, &remote_timeout), Some(false));
    assert_eq!(
        connectivity_evidence(true, &Err(CalendarError::Auth("401".into()))),
        None
    );
}

#[test]
fn connectivity_lookup_uses_each_configured_account_transport() {
    use thegn_core::config_calendar::{CalendarAccount, CalendarProviderKind};

    let accounts = vec![
        CalendarAccount {
            name: "local-command".into(),
            provider: CalendarProviderKind::Command,
            ..CalendarAccount::default()
        },
        CalendarAccount {
            name: "subscription".into(),
            provider: CalendarProviderKind::IcsUrl,
            ..CalendarAccount::default()
        },
    ];
    let network_accounts = network_account_names(&accounts);

    assert!(!network_accounts.contains("local-command"));
    assert!(network_accounts.contains("subscription"));
    assert_eq!(
        connectivity_evidence(
            network_accounts.contains("local-command"),
            &Ok(EventPage::default()),
        ),
        None
    );
    assert_eq!(
        connectivity_evidence(
            network_accounts.contains("subscription"),
            &Ok(EventPage::default()),
        ),
        Some(true)
    );
}

#[test]
fn local_timeouts_and_local_successes_cannot_change_connectivity_state() {
    let mut state = thegn_core::connectivity::ConnState::with_thresholds(3, 1);
    state.report_success(0);
    let mut batch = ConnectivityEvidenceBatch::default();
    for _ in 0..3 {
        batch.observe(connectivity_evidence(
            false,
            &Err(CalendarError::Timeout("command")),
        ));
    }
    match batch.outcome() {
        Some(ConnEvidence::Reachable) => {
            state.report_success(1);
        }
        Some(ConnEvidence::Unreachable(failures)) => {
            for _ in 0..failures {
                state.report_failure(1);
            }
        }
        None => {}
    }
    assert_eq!(
        state.state(),
        thegn_core::connectivity::Connectivity::Online
    );
    assert_eq!(state.consecutive_failures(), 0);

    let mut state = thegn_core::connectivity::ConnState::with_thresholds(3, 1);
    for _ in 0..3 {
        state.report_failure(1);
    }
    let mut batch = ConnectivityEvidenceBatch::default();
    for _ in 0..3 {
        batch.observe(connectivity_evidence(false, &Ok(EventPage::default())));
    }
    match batch.outcome() {
        Some(ConnEvidence::Reachable) => {
            state.report_success(2);
        }
        Some(ConnEvidence::Unreachable(failures)) => {
            for _ in 0..failures {
                state.report_failure(2);
            }
        }
        None => {}
    }
    assert_eq!(
        state.state(),
        thegn_core::connectivity::Connectivity::Offline
    );
}

#[test]
fn mixed_remote_and_local_outcomes_are_order_independent_at_threshold() {
    fn apply_batch(
        outcomes: impl IntoIterator<Item = Option<bool>>,
    ) -> thegn_core::connectivity::ConnState {
        let mut state = thegn_core::connectivity::ConnState::with_thresholds(3, 1);
        let mut batch = ConnectivityEvidenceBatch::default();
        for outcome in outcomes {
            batch.observe(outcome);
        }
        match batch.outcome() {
            Some(ConnEvidence::Reachable) => {
                state.report_success(1);
            }
            Some(ConnEvidence::Unreachable(failures)) => {
                for _ in 0..failures {
                    state.report_failure(1);
                }
            }
            None => {}
        }
        state
    }
    let local_timeout = connectivity_evidence(false, &Err(CalendarError::Timeout("command")));
    let local_success = connectivity_evidence(false, &Ok(EventPage::default()));
    let remote_failure =
        connectivity_evidence(true, &Err(CalendarError::Network("offline".into())));
    let remote_success = connectivity_evidence(true, &Ok(EventPage::default()));
    let first = apply_batch([
        local_timeout,
        remote_failure,
        local_success,
        remote_failure,
        remote_success,
        remote_failure,
    ]);
    let second = apply_batch([
        remote_success,
        remote_failure,
        remote_failure,
        local_success,
        remote_failure,
        local_timeout,
    ]);
    assert_eq!(
        first.state(),
        thegn_core::connectivity::Connectivity::Online
    );
    assert_eq!(second.state(), first.state());
    assert_eq!(first.consecutive_failures(), 0);
    assert_eq!(second.consecutive_failures(), 0);
}

#[test]
fn three_remote_failures_reach_offline_but_local_results_do_not_count() {
    let mut state = thegn_core::connectivity::ConnState::with_thresholds(3, 1);
    let mut batch = ConnectivityEvidenceBatch::default();
    for evidence in [
        connectivity_evidence(true, &Err(CalendarError::Network("one".into()))),
        connectivity_evidence(false, &Err(CalendarError::Timeout("command"))),
        connectivity_evidence(true, &Err(CalendarError::Timeout("two"))),
        connectivity_evidence(false, &Ok(EventPage::default())),
        connectivity_evidence(true, &Err(CalendarError::Network("three".into()))),
    ] {
        batch.observe(evidence);
    }
    match batch.outcome() {
        Some(ConnEvidence::Reachable) => {
            state.report_success(1);
        }
        Some(ConnEvidence::Unreachable(failures)) => {
            for _ in 0..failures {
                state.report_failure(1);
            }
        }
        None => {}
    }
    assert_eq!(
        state.state(),
        thegn_core::connectivity::Connectivity::Offline
    );
}

#[test]
fn same_month_payloads_are_fenced_in_both_completion_orders() {
    fn payload(month: (i32, u32), request: u64) -> crate::detail::CalendarPayload {
        let (from, to) = thegn_core::calendar::month_bounds(month.0, month.1).unwrap();
        let covered = widen(from, to);
        crate::detail::CalendarPayload {
            month,
            covered,
            request: Some(request),
            events: Some(Vec::new()),
            error: None,
        }
    }

    for month in [(2050, 1), (2051, 2)] {
        let older = register_month_request(month);
        let newer = register_month_request(month);
        assert!(request_is_latest(
            &BTreeMap::from([(month, newer)]),
            month,
            newer
        ));
        assert!(!request_is_latest(
            &BTreeMap::from([(month, newer)]),
            month,
            older
        ));
        let old_payload = payload(month, older);
        let new_payload = payload(month, newer);

        let mut old_first = crate::calendar_docs::CalendarDocs::default();
        old_first.merge(&old_payload);
        assert!(!old_first.loaded.contains(&month));
        old_first.merge(&new_payload);
        assert!(old_first.loaded.contains(&month));

        let mut new_first = crate::calendar_docs::CalendarDocs::default();
        new_first.merge(&new_payload);
        new_first.merge(&old_payload);
        assert!(new_first.loaded.contains(&month));
        assert_eq!(new_first.events.len(), 0);
    }
}

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
    // Legacy shorthand: a cursor-less page is a snapshot, otherwise a delta.
    let mode = if sync_token.is_empty() {
        PageMode::Snapshot
    } else {
        PageMode::Delta
    };
    page_in(mode, events, deleted, sync_token)
}

fn page_in(
    mode: PageMode,
    events: Vec<CalEvent>,
    deleted: Vec<String>,
    sync_token: &str,
) -> EventPage {
    EventPage::try_new(
        mode,
        events,
        deleted,
        sync_token,
        &thegn_svc::calendar::AccountAdmission::isolated(
            thegn_core::calendar::AdmissionBudget::default(),
            thegn_core::calendar::Tz::UTC,
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
fn opaque_caldav_tombstone_deletes_the_cached_resource() {
    let t = TmpDb::new("opaque-href");
    let (from, to) = window();
    let admission = || {
        thegn_svc::calendar::AccountAdmission::isolated(
            thegn_core::calendar::AdmissionBudget::default(),
            thegn_core::calendar::Tz::UTC,
        )
    };
    let href = "/cal/random-server-key.ics".to_string();
    let full = EventPage::try_new_with_hrefs(
        PageMode::Snapshot,
        vec![event("meeting@example")],
        vec![],
        vec![href.clone()],
        vec![],
        "",
        &admission(),
    )
    .unwrap();
    assert!(apply_page(&t.db, "work", "caldav", &full, from, to));
    assert!(t.db.has_calendar_events("work").unwrap());

    let delta = EventPage::try_new_with_hrefs(
        PageMode::Delta,
        vec![],
        vec!["random-server-key".into()],
        vec![],
        vec![href],
        "next-token",
        &admission(),
    )
    .unwrap();
    assert!(apply_page(&t.db, "work", "caldav", &delta, from, to));
    assert!(!t.db.has_calendar_events("work").unwrap());
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
    let cached = load_cached(&t.db, from, to).unwrap().events;
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
    let cached = load_cached(&t.db, from, to).unwrap().events;
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
    let cached = load_cached(&t.db, from, to).unwrap().events;
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
    let far = load_cached(&t.db, d(2030, 1, 1), d(2030, 1, 31))
        .unwrap()
        .events;
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
            start_ms: CalendarWindow::new(d(2026, 8, 21), d(2026, 8, 21), chrono_tz::Tz::UTC)
                .unwrap()
                .start_ms,
            end_ms: CalendarWindow::new(d(2026, 8, 21), d(2026, 8, 21), chrono_tz::Tz::UTC)
                .unwrap()
                .start_ms
                + 3_600_000,
            recurring: false,
            json: "{ not json at all".into(),
            ..Default::default()
        }],
    )
    .unwrap();
    let cached = load_cached(&t.db, from, to).unwrap();
    assert_eq!(cached.events.len(), 1);
    assert_eq!(cached.events[0].uid, "good");
    // ...but the loss is counted, so the month is flagged incomplete rather
    // than presented as the whole calendar.
    assert_eq!(cached.skipped, 1);
    let view = expand_month(&t.db, from, to, chrono_tz::Tz::UTC);
    assert_eq!(
        view.error,
        Some(CalendarViewError::MalformedCache(CacheSkips {
            undecodable: 1,
            ..Default::default()
        }))
    );
    let events = view.events.expect("the readable rows still show");
    assert_eq!(events.len(), 1);
}

#[test]
fn cache_horizon_metadata_uses_the_same_home_zone_window_as_queries() {
    let t = TmpDb::new("home-zone-range");
    let from = d(2026, 3, 8);
    let to = d(2026, 3, 8);
    let zone = chrono_tz::Tz::America__New_York;
    let range = CalendarWindow::new(from, to, zone).unwrap();
    let loaded = page(vec![event("e1")], vec![], "token");

    assert!(apply_page_with_generation(
        &t.db, "work", "caldav", &loaded, range, None,
    ));
    let metadata = t.db.get_calendar_sync("work").unwrap().unwrap();
    assert_eq!(metadata.horizon_from_ms, range.start_ms);
    assert_eq!(metadata.horizon_to_ms, range.end_exclusive_ms);
    assert_eq!(
        metadata.horizon_to_ms - metadata.horizon_from_ms,
        23 * 3_600_000
    );
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
    let (from, to) = horizon(&cfg, d(2026, 8, 21)).unwrap();
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

fn reminder_window(generation: u64, from_ms: i64, to_ms: i64) -> ReminderWindow {
    ReminderWindow {
        generation,
        from_ms,
        to_ms,
    }
}

fn reminder_cfg() -> CalendarConfig {
    CalendarConfig {
        enabled: true,
        reminders_enabled: true,
        home_zone: "UTC".into(),
        accounts: vec![thegn_core::config_calendar::CalendarAccount {
            name: "work".into(),
            provider: thegn_core::config_calendar::CalendarProviderKind::Ics,
            enabled: true,
            ..Default::default()
        }],
        ..CalendarConfig::default()
    }
}

fn utc_ms(y: i32, m: u32, day: u32, h: u32, mi: u32) -> i64 {
    d(y, m, day)
        .and_hms_opt(h, mi, 0)
        .unwrap()
        .and_utc()
        .timestamp_millis()
}

#[test]
fn reminders_are_inert_without_configuration() {
    // No accounts, or the switch off, must cost nothing at all — the reminder
    // slot rides the ticker and runs unconditionally.
    let t = TmpDb::new("rem-inert");
    let off = CalendarConfig {
        reminders_enabled: false,
        ..reminder_cfg()
    };
    let w = reminder_window(0, 0, utc_ms(2026, 8, 21, 9, 0));
    assert!(due_reminders(&t.db, &off, w).unwrap().reminders.is_empty());
    let no_sources = CalendarConfig::default();
    assert!(!reminders_configured(&no_sources));
    assert!(
        due_reminders(&t.db, &no_sources, w)
            .unwrap()
            .reminders
            .is_empty()
    );
}

#[test]
fn unsupported_imported_reminder_leads_are_counted_for_host_diagnostics() {
    let t = TmpDb::new("rem-over-cap");
    let mut e = event("too-far");
    e.reminders = vec![thegn_core::calendar::Reminder {
        minutes_before: thegn_core::calendar::reminders::MAX_LEAD_MINS + 1,
    }];
    apply_page(
        &t.db,
        "work",
        "ics",
        &page(vec![e], vec![], ""),
        d(2026, 8, 1),
        d(2026, 8, 31),
    );
    let window = reminder_window(0, utc_ms(2026, 8, 21, 8, 59), utc_ms(2026, 8, 21, 9, 1));
    let due = due_reminders(&t.db, &reminder_cfg(), window).unwrap();
    assert!(due.reminders.is_empty());
    assert_eq!(due.unsupported_leads, 1);
}

#[test]
fn a_multi_day_event_raises_one_reminder_for_exactly_the_given_window() {
    let t = TmpDb::new("rem-multiday");
    let mut trip = CalEvent::new(
        "trip",
        "Trip",
        EventTime::Zoned {
            local: d(2026, 8, 20).and_hms_opt(9, 0, 0).unwrap(),
            zone: TzRef::new("UTC"),
        },
        EventTime::Zoned {
            local: d(2026, 8, 22).and_hms_opt(17, 0, 0).unwrap(),
            zone: TzRef::new("UTC"),
        },
    );
    trip.reminders = vec![thegn_core::calendar::Reminder { minutes_before: 10 }];
    apply_page(
        &t.db,
        "work",
        "ics",
        &page(vec![trip], vec![], ""),
        d(2026, 8, 1),
        d(2026, 8, 31),
    );
    let cfg = reminder_cfg();
    // The trigger (08:50) is inside the supplied window: exactly one reminder,
    // although the occurrence occupies three day buckets.
    let hit = reminder_window(3, utc_ms(2026, 8, 20, 8, 49), utc_ms(2026, 8, 20, 8, 51));
    let due = due_reminders(&t.db, &cfg, hit).unwrap();
    assert_eq!(due.reminders.len(), 1, "{:?}", due.reminders);
    assert_eq!(due.skipped, 0);
    // The window's END is the one the caller supplied, not a fresh clock
    // reading: a window ending just before the trigger raises nothing.
    let before = reminder_window(4, utc_ms(2026, 8, 20, 8, 40), utc_ms(2026, 8, 20, 8, 49));
    assert!(
        due_reminders(&t.db, &cfg, before)
            .unwrap()
            .reminders
            .is_empty()
    );
}

#[test]
fn one_malformed_row_costs_itself_not_the_month_or_every_reminder() {
    // The bad row stays in the cache, so failing the whole evaluation would
    // re-fail identically on every tick: reminders would stop for good.
    let t = TmpDb::new("rem-invalid");
    let mut bad = event("inverted");
    std::mem::swap(&mut bad.start, &mut bad.end);
    bad.reminders = vec![thegn_core::calendar::Reminder { minutes_before: 10 }];
    let mut good = event("good");
    good.reminders = vec![thegn_core::calendar::Reminder { minutes_before: 10 }];
    let (from, to) = window();
    apply_page(
        &t.db,
        "work",
        "ics",
        &page(vec![good, bad], vec![], ""),
        from,
        to,
    );
    let w = reminder_window(0, utc_ms(2026, 8, 21, 8, 49), utc_ms(2026, 8, 21, 8, 51));
    let due = due_reminders(&t.db, &reminder_cfg(), w).unwrap();
    assert_eq!(due.reminders.len(), 1, "the good event still fires");
    // The cached row carries no SourceId (the account is the DB key).
    assert_eq!(due.reminders[0].event_id, "/good");
    assert_eq!(due.skipped, 1, "and the loss is reported");

    // The month shows the readable events, marked incomplete — not blanked.
    let view = expand_month(&t.db, from, to, chrono_tz::Tz::UTC);
    assert_eq!(
        view.error,
        Some(CalendarViewError::MalformedCache(CacheSkips {
            rejected: thegn_svc::calendar::RejectedEvents {
                reversed: 1,
                ..Default::default()
            },
            ..Default::default()
        }))
    );
    let events = view.events.expect("the readable rows still show");
    assert!(
        events
            .iter()
            .any(|(_, evs)| evs.iter().any(|e| e.uid == "good"))
    );
    assert!(
        !events
            .iter()
            .any(|(_, evs)| evs.iter().any(|e| e.uid == "inverted"))
    );
}

#[test]
fn cache_skip_categories_reach_the_month_and_reminder_errors() {
    let t = TmpDb::new("cache-skip-categories");
    let (from, to) = window();
    let mut reversed = event("SENTINEL-uid");
    std::mem::swap(&mut reversed.start, &mut reversed.end);
    apply_page(
        &t.db,
        "work",
        "ics",
        &page(vec![reversed], vec![], ""),
        from,
        to,
    );
    let row_window =
        CalendarWindow::new(d(2026, 8, 21), d(2026, 8, 21), chrono_tz::Tz::UTC).unwrap();
    t.db.put_calendar_events(
        "work",
        &[CalendarRow {
            uid: "bad".into(),
            calendar: String::new(),
            start_ms: row_window.start_ms,
            end_ms: row_window.start_ms + 3_600_000,
            recurring: false,
            json: "{ not json at all".into(),
            ..Default::default()
        }],
    )
    .unwrap();

    let view = expand_month(&t.db, from, to, chrono_tz::Tz::UTC);
    let error = view.error.expect("the month reports skipped rows");
    let CalendarViewError::MalformedCache(skips) = error else {
        panic!("expected categorized cache error: {error:?}");
    };
    let error_text = CalendarViewError::MalformedCache(skips).to_string();
    assert_eq!(skips.undecodable, 1, "{error_text}");
    assert_eq!(skips.rejected.reversed, 1, "{error_text}");
    assert!(error_text.contains("1 undecodable"), "{error_text}");
    assert!(
        error_text.contains("end precedes start (1)"),
        "{error_text}"
    );
    assert!(!error_text.contains("SENTINEL"), "{error_text}");

    let reminder = reminder_window(1, utc_ms(2026, 8, 21, 8, 49), utc_ms(2026, 8, 21, 8, 51));
    let due = due_reminders(&t.db, &reminder_cfg(), reminder).unwrap();
    assert_eq!(due.skips, skips, "{error_text}");
    assert_eq!(due.skipped, 2, "{error_text}");
}

#[test]
fn the_reminder_cursor_advances_only_on_a_current_success() {
    let mut c = ReminderCursor::new(1_000);
    let w = c.begin(2_000).expect("nothing in flight");
    assert_eq!((w.from_ms, w.to_ms), (1_000, 2_000));
    // One evaluation at a time.
    assert_eq!(c.begin(3_000), None);

    // Failure: the cursor stays put and the SAME start is retried.
    let failed = ReminderOutcome::Failed(CalendarViewError::CacheUnavailable);
    assert!(c.finish(w, failed));
    assert_eq!(c.last_checked_ms(), 1_000);
    let retry = c.begin(3_000).unwrap();
    assert_eq!((retry.from_ms, retry.to_ms), (1_000, 3_000));
    assert_ne!(retry.generation, w.generation);

    // A stale acknowledgment for the old window neither clears the new one
    // nor moves the cursor.
    assert!(!c.finish(w, ReminderOutcome::Complete));
    assert_eq!(c.last_checked_ms(), 1_000);
    assert_eq!(c.begin(4_000), None, "the retry is still in flight");

    // Success advances to exactly the acknowledged window's end.
    assert!(c.finish(retry, ReminderOutcome::Complete));
    assert_eq!(c.last_checked_ms(), 3_000);
    // A replayed acknowledgment is ignored.
    assert!(!c.finish(retry, ReminderOutcome::Complete));

    // An incomplete (unreadable rows) evaluation still completes its window.
    let w = c.begin(5_000).unwrap();
    assert!(c.finish(
        w,
        ReminderOutcome::Incomplete(CalendarViewError::MalformedCache(CacheSkips::default()))
    ));
    assert_eq!(c.last_checked_ms(), 5_000);

    // A completion can never move the cursor backwards (clock jump).
    let w = c.begin(4_500).unwrap();
    assert!(c.finish(w, ReminderOutcome::Complete));
    assert_eq!(c.last_checked_ms(), 5_000);
}

#[test]
fn abandoning_a_stale_reminder_ack_releases_the_cursor_for_retry() {
    let mut cursor = ReminderCursor::new(1_000);
    let stale = cursor.begin(2_000).expect("first evaluation starts");
    cursor.abandon(stale);

    let retry = cursor
        .begin(3_000)
        .expect("a dropped stale envelope must not strand the cursor");
    assert_eq!((retry.from_ms, retry.to_ms), (1_000, 3_000));
}

#[test]
fn an_undispatched_or_skipped_reminder_window_is_not_lost() {
    let mut c = ReminderCursor::new(1_000);
    let w = c.begin(2_000).unwrap();
    // The lane was full: forget the window, keep the cursor.
    c.abandon(w);
    assert_eq!(c.last_checked_ms(), 1_000);
    let again = c.begin(2_500).unwrap();
    assert_eq!(again.from_ms, 1_000);
    // A skip (nothing configured) never races an in-flight window...
    c.skip(9_000);
    assert_eq!(c.last_checked_ms(), 1_000);
    c.abandon(again);
    // ...and otherwise completes trivially.
    c.skip(9_000);
    assert_eq!(c.last_checked_ms(), 9_000);
    // Abandoning a window that is not in flight is a no-op.
    let w = c.begin(9_500).unwrap();
    c.abandon(again);
    assert_eq!(c.begin(9_600), None);
    assert!(c.finish(w, ReminderOutcome::Complete));
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
        .unwrap()
        .events
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
    assert_eq!(load_cached(&t.db, from, to).unwrap().events.len(), 5);
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
        None,
    );
    let after = t.db.get_calendar_sync(&account).unwrap().unwrap();
    assert_eq!(after.fetched_at, before.fetched_at, "no attempt stamp");
    assert_eq!(after.sync_token, "cursor-1");
    assert!(after.last_error.is_empty(), "{after:?}");
    assert!(toasts.is_empty(), "contention is not the user's problem");
    // …but the account is backed off, so the popup won't re-fetch it.
    assert!(contention_backoff(&account, thegn_core::util::now()));
}

#[test]
fn an_invalid_horizon_is_refused_and_never_collapses_to_today() {
    let today = d(2026, 8, 21);
    for (past, future) in [(u32::MAX, 365), (90, u32::MAX), (u32::MAX, u32::MAX)] {
        let cfg = CalendarConfig {
            horizon_past_days: past,
            horizon_future_days: future,
            ..CalendarConfig::default()
        };
        assert!(horizon(&cfg, today).is_err(), "{past}/{future}");
    }
    let mut seen = Vec::new();
    refuse_horizon("calendar.horizon_past_days: bad", &mut |m| seen.push(m));
    assert_eq!(seen.len(), 1);
    assert!(seen[0].contains("horizon_past_days"));
}

#[test]
fn a_snapshot_that_returns_a_cursor_still_replaces_the_cache() {
    // THE-452: the host used to read a non-empty token as "delta" and merge,
    // resurrecting everything the authoritative snapshot omitted.
    let t = TmpDb::new("snapshot-with-token");
    let (from, to) = window();
    apply_page(
        &t.db,
        "work",
        "caldav",
        &page_in(PageMode::Snapshot, vec![event("a"), event("b")], vec![], ""),
        from,
        to,
    );
    apply_page(
        &t.db,
        "work",
        "caldav",
        &page_in(PageMode::Snapshot, vec![event("c")], vec![], "tok-9"),
        from,
        to,
    );
    let cached = load_cached(&t.db, from, to).unwrap().events;
    assert_eq!(cached.len(), 1);
    assert_eq!(cached[0].uid, "c");
    assert_eq!(
        t.db.get_calendar_sync("work").unwrap().unwrap().sync_token,
        "tok-9"
    );
}

#[test]
fn a_delta_without_a_cursor_never_erases_unrelated_rows() {
    let t = TmpDb::new("delta-no-token");
    let (from, to) = window();
    apply_page(
        &t.db,
        "work",
        "caldav",
        &page_in(
            PageMode::Snapshot,
            vec![event("a"), event("b")],
            vec![],
            "t1",
        ),
        from,
        to,
    );
    apply_page(
        &t.db,
        "work",
        "caldav",
        &page_in(PageMode::Delta, vec![event("c")], vec!["a".into()], ""),
        from,
        to,
    );
    let mut uids: Vec<String> = load_cached(&t.db, from, to)
        .unwrap()
        .events
        .into_iter()
        .map(|e| e.uid)
        .collect();
    uids.sort();
    assert_eq!(uids, ["b", "c"]);
    // The lost cursor is dropped, so the next fetch is a full one.
    assert!(
        t.db.get_calendar_sync("work")
            .unwrap()
            .unwrap()
            .sync_token
            .is_empty()
    );
}

#[test]
fn a_reset_is_believed_even_when_empty_but_an_empty_snapshot_is_not() {
    let t = TmpDb::new("reset");
    let (from, to) = window();
    apply_page(
        &t.db,
        "work",
        "command",
        &page_in(PageMode::Snapshot, vec![event("a")], vec![], ""),
        from,
        to,
    );
    apply_page(
        &t.db,
        "work",
        "command",
        &page_in(PageMode::Snapshot, vec![], vec![], "x"),
        from,
        to,
    );
    assert!(
        t.db.has_calendar_events("work").unwrap(),
        "guard still holds"
    );
    apply_page(
        &t.db,
        "work",
        "command",
        &page_in(PageMode::Reset, vec![], vec![], ""),
        from,
        to,
    );
    assert!(!t.db.has_calendar_events("work").unwrap());
}

#[test]
fn a_week_long_reminder_lead_is_a_candidate_not_just_yesterday_to_tomorrow() {
    // THE-460: the scan used to stop at tomorrow, so an event a week out with
    // a week-long lead had its trigger due now and was never considered.
    let t = TmpDb::new("rem-week");
    let (from, to) = window();
    let mut trip = event("trip"); // 2026-08-21 09:00 UTC
    trip.reminders = vec![thegn_core::calendar::Reminder {
        minutes_before: 7 * 24 * 60,
    }];
    apply_page(
        &t.db,
        "work",
        "ics",
        &page(vec![trip], vec![], ""),
        from,
        to,
    );
    let hit = reminder_window(1, utc_ms(2026, 8, 14, 8, 59), utc_ms(2026, 8, 14, 9, 1));
    let due = due_reminders(&t.db, &reminder_cfg(), hit).unwrap();
    assert_eq!(due.reminders.len(), 1, "{:?}", due.reminders);
    assert_eq!(due.reminders[0].trigger_mins, 7 * 24 * 60);
    // The tick after the trigger does not repeat it.
    let after = reminder_window(2, utc_ms(2026, 8, 14, 9, 1), utc_ms(2026, 8, 14, 9, 3));
    assert!(
        due_reminders(&t.db, &reminder_cfg(), after)
            .unwrap()
            .reminders
            .is_empty()
    );
}

#[test]
fn a_long_configured_default_lead_reaches_events_beyond_tomorrow() {
    let t = TmpDb::new("rem-default-week");
    let (from, to) = window();
    apply_page(
        &t.db,
        "work",
        "ics",
        &page(vec![event("plain")], vec![], ""),
        from,
        to,
    );
    let cfg = CalendarConfig {
        reminder_default_mins: vec![3 * 24 * 60],
        ..reminder_cfg()
    };
    let hit = reminder_window(1, utc_ms(2026, 8, 18, 8, 59), utc_ms(2026, 8, 18, 9, 1));
    assert_eq!(due_reminders(&t.db, &cfg, hit).unwrap().reminders.len(), 1);
}

#[test]
fn calendar_sync_uses_shared_runtime() {
    let source = include_str!("hydrate_calendar.rs");
    assert!(!source.contains("new_current_thread"));
    assert!(source.contains("thegn_svc::bg_runtime::block_on("));
}

// THE-470 regression lock: expected to pass on main.
#[test]
fn transp_free_survives_the_cache_round_trip_with_overrides() {
    let t = TmpDb::new("transp-round-trip");
    let (from, to) = window();

    let mut master = event("series");
    master.busy = thegn_core::calendar::Busy::Free;
    master.recurrence = Some(thegn_core::calendar::Recurrence {
        rules: vec![thegn_core::calendar::RRule::parse("FREQ=WEEKLY").unwrap()],
        ..Default::default()
    });

    let mut override_event = event("series");
    override_event.recurrence_id = Some(EventTime::Zoned {
        local: d(2026, 8, 28).and_hms_opt(9, 0, 0).unwrap(),
        zone: TzRef::new("UTC"),
    });

    let mut one_shot = event("free-once");
    one_shot.busy = thegn_core::calendar::Busy::Free;
    assert!(apply_page(
        &t.db,
        "work",
        "ics",
        &page(vec![master, override_event, one_shot], vec![], ""),
        from,
        to,
    ));

    let cached = load_cached(&t.db, from, to).unwrap().events;
    let cached_master = cached
        .iter()
        .find(|cached_event| cached_event.uid == "series" && cached_event.recurrence_id.is_none())
        .unwrap();
    assert_eq!(cached_master.busy, thegn_core::calendar::Busy::Free);

    let cached_override = cached
        .iter()
        .find(|cached_event| cached_event.uid == "series" && cached_event.recurrence_id.is_some())
        .unwrap();
    assert_eq!(cached_override.busy, thegn_core::calendar::Busy::Busy);
    assert_eq!(
        cached_override.recurrence_id,
        Some(EventTime::Zoned {
            local: d(2026, 8, 28).and_hms_opt(9, 0, 0).unwrap(),
            zone: TzRef::new("UTC"),
        })
    );

    let cached_one_shot = cached
        .iter()
        .find(|cached_event| cached_event.uid == "free-once")
        .unwrap();
    assert_eq!(cached_one_shot.busy, thegn_core::calendar::Busy::Free);
}

#[test]
fn an_unchanged_page_advances_the_cursor_without_a_repaint() {
    let t = TmpDb::new("unchanged-cursor");
    let (from, to) = window();
    assert!(apply_page(
        &t.db,
        "work",
        "ics",
        &page(vec![event("existing")], vec![], "t1"),
        from,
        to,
    ));
    let before =
        t.db.get_calendar_events(0, i64::MAX, &["work".to_owned()])
            .unwrap();
    assert!(!apply_page(
        &t.db,
        "work",
        "ics",
        &EventPage::unchanged("tok-2"),
        from,
        to,
    ));
    let observed =
        t.db.get_calendar_events(0, i64::MAX, &["work".to_owned()])
            .unwrap();
    assert_eq!(observed, before, "rows: {observed:?}");
    let sync = t.db.get_calendar_sync("work").unwrap().unwrap();
    assert_eq!(sync.sync_token, "tok-2", "token: {}", sync.sync_token);
}

#[test]
fn an_empty_snapshot_records_the_suspect_error_and_keeps_the_old_cursor() {
    let t = TmpDb::new("empty-snapshot-cursor");
    let (from, to) = window();
    assert!(apply_page(
        &t.db,
        "work",
        "ics",
        &page(vec![event("existing")], vec![], "t1"),
        from,
        to,
    ));
    let before =
        t.db.get_calendar_events(0, i64::MAX, &["work".to_owned()])
            .unwrap();
    let empty_snapshot = page_in(PageMode::Snapshot, vec![], vec![], "x");
    assert!(!apply_page(&t.db, "work", "ics", &empty_snapshot, from, to,));
    let observed =
        t.db.get_calendar_events(0, i64::MAX, &["work".to_owned()])
            .unwrap();
    assert_eq!(observed, before, "rows: {observed:?}");
    let sync = t.db.get_calendar_sync("work").unwrap().unwrap();
    assert_eq!(sync.last_error, "provider returned an empty calendar");
    assert_eq!(sync.sync_token, "t1", "token: {}", sync.sync_token);
}

#[test]
fn stale_calendar_cache_failure_does_not_stamp_the_account() {
    use std::sync::{Arc, atomic::AtomicU64};

    let t = TmpDb::new("stale-cache-failure");
    let current = Arc::new(AtomicU64::new(2));
    let stale = (Arc::clone(&current), 1);
    record_calendar_cache_failure(&t.db, "work", Some(&stale));
    assert!(
        t.db.get_calendar_sync("work").unwrap().is_none(),
        "stale cache failure must not create or stamp sync metadata"
    );

    let current_fence = (current, 2);
    record_calendar_cache_failure(&t.db, "work", Some(&current_fence));
    let sync = t.db.get_calendar_sync("work").unwrap().unwrap();
    assert_eq!(sync.last_error, "calendar cache write failed");
}

#[test]
fn plugin_secret_sentinel_never_reaches_the_stored_error_logs_or_status() {
    if !crate::platform::sound_process::test_support::UNIX {
        // This fixture is a POSIX sh script and cannot run on non-Unix hosts.
        return;
    }

    use std::io::Write;
    use std::sync::{Arc, Mutex};
    use thegn_core::config_calendar::{CalendarAccount, CalendarConfig, CalendarProviderKind};

    struct Capture(Arc<Mutex<Vec<u8>>>);

    impl Write for Capture {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(bytes);
            Ok(bytes.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    let fixture = tempfile::tempdir_in("/tmp").unwrap();
    crate::platform::set_owner_executable(fixture.path()).unwrap();
    let script = fixture.path().join("plugin.sh");
    std::fs::write(
        &script,
        "#!/bin/sh\necho \"token=$CAL_SENTINEL_TOKEN arg=$1\" >&2\necho \"stdout junk $CAL_SENTINEL_TOKEN $1\"\nexit 3\n",
    )
    .unwrap();
    crate::platform::set_owner_executable(&script).unwrap();

    let env_sentinel = "cal-env-sentinel-7f3a";
    let arg_sentinel = "--token=cal-arg-sentinel-91c2";
    let cfg = CalendarConfig {
        accounts: vec![CalendarAccount {
            name: "sentinel".into(),
            provider: CalendarProviderKind::Command,
            command: vec![script.display().to_string(), arg_sentinel.into()],
            env: std::collections::BTreeMap::from([(
                "CAL_SENTINEL_TOKEN".into(),
                env_sentinel.into(),
            )]),
            timeout_secs: 10,
            ..CalendarAccount::default()
        }],
        ..CalendarConfig::default()
    };
    let t = TmpDb::new("plugin-secret-sentinel");
    let capture = Capture(Arc::new(Mutex::new(Vec::new())));
    let writer = Capture(Arc::clone(&capture.0));
    let subscriber = tracing_subscriber::fmt()
        .with_max_level(tracing::Level::DEBUG)
        .without_time()
        .with_ansi(false)
        .with_writer(move || Capture(Arc::clone(&writer.0)))
        .finish();
    let mut toasts = Vec::new();

    tracing::subscriber::with_default(subscriber, || {
        sync_accounts(
            &t.db,
            &cfg,
            d(2026, 10, 1),
            d(2026, 10, 31),
            true,
            &mut |message| toasts.push(message),
        );
    });

    let sync =
        t.db.get_calendar_sync("sentinel")
            .unwrap()
            .expect("command failure is recorded");
    let log = String::from_utf8(capture.0.lock().unwrap().clone()).unwrap();
    let evidence = format!(
        "stored error: {}; log: {log}; toasts: {toasts:?}",
        sync.last_error
    );
    assert!(!sync.last_error.is_empty(), "{evidence}");
    assert!(sync.last_error.contains("<redacted>"), "{evidence}");
    assert!(log.contains("calendar sync failed"), "{evidence}");

    for sentinel in [env_sentinel, arg_sentinel, "cal-arg-sentinel-91c2"] {
        assert!(!sync.last_error.contains(sentinel), "{evidence}");
        assert!(!log.contains(sentinel), "{evidence}");
        assert!(
            toasts.iter().all(|message| !message.contains(sentinel)),
            "{evidence}"
        );
    }
}

type CalendarAuditBuffer = std::sync::Arc<std::sync::Mutex<Vec<u8>>>;

struct CalendarAuditWriter(CalendarAuditBuffer);

impl std::io::Write for CalendarAuditWriter {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

fn calendar_audit_subscriber() -> (CalendarAuditBuffer, impl tracing::Subscriber + Send + Sync) {
    let capture = CalendarAuditBuffer::default();
    let writer = std::sync::Arc::clone(&capture);
    let subscriber = tracing_subscriber::fmt()
        .with_max_level(tracing::Level::DEBUG)
        .without_time()
        .with_ansi(false)
        .with_writer(move || CalendarAuditWriter(std::sync::Arc::clone(&writer)))
        .finish();
    (capture, subscriber)
}

fn calendar_audit_config(
    alpha_token: &str,
    beta_token: &str,
) -> thegn_core::config_calendar::CalendarConfig {
    use thegn_core::config_calendar::{CalendarAccount, CalendarConfig, CalendarProviderKind};

    CalendarConfig {
        enabled: true,
        accounts: vec![
            CalendarAccount {
                name: "alpha".into(),
                provider: CalendarProviderKind::IcsUrl,
                enabled: true,
                url: "https://calendar.invalid/a.ics".into(),
                token: alpha_token.into(),
                ..CalendarAccount::default()
            },
            CalendarAccount {
                name: "beta".into(),
                provider: CalendarProviderKind::IcsUrl,
                enabled: true,
                url: "https://calendar.invalid/b.ics".into(),
                token: beta_token.into(),
                ..CalendarAccount::default()
            },
        ],
        ..CalendarConfig::default()
    }
}

fn calendar_audit_lines(capture: &str) -> Vec<&str> {
    capture
        .lines()
        .filter(|line| line.contains("secret resolve"))
        .collect()
}

#[test]
fn calendar_token_resolution_audits_each_account_without_values() {
    thegn_svc::secret::install_resolver(crate::secret::resolve_ref_for);

    let fixture = tempfile::tempdir_in("/tmp").unwrap();
    let token_path = fixture.path().join("calendar-token");
    let file_sentinel = "cal-file-sentinel-b72c";
    std::fs::write(&token_path, file_sentinel).unwrap();
    let literal_sentinel = "cal-lit-sentinel-a41f";
    let literal_ref = format!("literal:{}", thegn_core::redact::PLACEHOLDER);
    let cfg = calendar_audit_config(literal_sentinel, &format!("file:{}", token_path.display()));
    let (capture, subscriber) = calendar_audit_subscriber();

    tracing::subscriber::with_default(subscriber, || {
        drop(thegn_svc::calendar::CalendarRouter::from_config(&cfg));
    });

    let log = String::from_utf8(
        capture
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone(),
    )
    .unwrap();
    let lines = calendar_audit_lines(&log);
    let alpha: Vec<_> = lines
        .iter()
        .filter(|line| line.contains("consumer=calendar:alpha"))
        .collect();
    let beta: Vec<_> = lines
        .iter()
        .filter(|line| line.contains("consumer=calendar:beta"))
        .collect();
    assert_eq!(lines.len(), 2, "capture: {log}");
    assert_eq!(alpha.len(), 1, "capture: {log}");
    assert!(alpha[0].contains("backend=\"literal\""), "capture: {log}");
    assert!(alpha[0].contains(&literal_ref), "capture: {log}");
    assert_eq!(beta.len(), 1, "capture: {log}");
    assert!(beta[0].contains("backend=\"file\""), "capture: {log}");
    assert!(
        beta[0].contains(&format!("file:{}", token_path.display())),
        "capture: {log}"
    );
    assert!(!log.contains(literal_sentinel), "capture: {log}");
    assert!(!log.contains(file_sentinel), "capture: {log}");
}

#[test]
fn calendar_token_reload_rebinds_audit_events_to_their_own_accounts() {
    thegn_svc::secret::install_resolver(crate::secret::resolve_ref_for);

    let fixture = tempfile::tempdir_in("/tmp").unwrap();
    let token_path = fixture.path().join("calendar-token");
    let file_sentinel = "cal-file-sentinel-b72c";
    std::fs::write(&token_path, file_sentinel).unwrap();
    let literal_sentinel = "cal-lit-sentinel-a41f";
    let literal_ref = format!("literal:{}", thegn_core::redact::PLACEHOLDER);
    let initial =
        calendar_audit_config(literal_sentinel, &format!("file:{}", token_path.display()));
    let reloaded =
        calendar_audit_config(&format!("file:{}", token_path.display()), literal_sentinel);
    let (first_capture, first_subscriber) = calendar_audit_subscriber();
    tracing::subscriber::with_default(first_subscriber, || {
        drop(thegn_svc::calendar::CalendarRouter::from_config(&initial));
    });
    let first_log = String::from_utf8(
        first_capture
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone(),
    )
    .unwrap();

    let (capture, subscriber) = calendar_audit_subscriber();
    tracing::subscriber::with_default(subscriber, || {
        drop(thegn_svc::calendar::CalendarRouter::from_config(&reloaded));
    });

    let log = String::from_utf8(
        capture
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone(),
    )
    .unwrap();
    let lines = calendar_audit_lines(&log);
    let alpha: Vec<_> = lines
        .iter()
        .filter(|line| line.contains("consumer=calendar:alpha"))
        .collect();
    let beta: Vec<_> = lines
        .iter()
        .filter(|line| line.contains("consumer=calendar:beta"))
        .collect();
    let evidence = format!("initial capture: {first_log}; reloaded capture: {log}");
    assert_eq!(lines.len(), 2, "{evidence}");
    assert_eq!(alpha.len(), 1, "{evidence}");
    assert!(alpha[0].contains("backend=\"file\""), "{evidence}");
    assert!(
        alpha[0].contains(&format!("file:{}", token_path.display())),
        "{evidence}"
    );
    assert_eq!(beta.len(), 1, "{evidence}");
    assert!(beta[0].contains("backend=\"literal\""), "{evidence}");
    assert!(beta[0].contains(&literal_ref), "{evidence}");
    assert!(
        lines.iter().all(|line| {
            !(line.contains("consumer=calendar:alpha") && line.contains(&literal_ref))
                && !(line.contains("consumer=calendar:beta")
                    && line.contains(&format!("file:{}", token_path.display())))
        }),
        "{evidence}"
    );
    for sentinel in [literal_sentinel, file_sentinel] {
        assert!(!first_log.contains(sentinel), "{evidence}");
        assert!(!log.contains(sentinel), "{evidence}");
    }
}
