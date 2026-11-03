use super::*;

#[test]
fn diagnostics_notice_fires_for_a_new_nonzero_set() {
    let diagnostics = thegn_svc::calendar::PageDiagnostics {
        quarantined_events: 2,
        ..Default::default()
    };

    let notice = diagnostics_notice(None, &diagnostics, "diag-test-account").unwrap();

    assert!(notice.contains("2 problem(s) reading events"), "{notice}");
    assert!(notice.contains("affected events are not shown"), "{notice}");
}

#[test]
fn diagnostics_notice_is_silent_for_the_same_set() {
    let diagnostics = thegn_svc::calendar::PageDiagnostics {
        quarantined_events: 1,
        unresolved_zones: 1,
        ..Default::default()
    };

    assert_eq!(
        diagnostics_notice(Some(&diagnostics), &diagnostics, "diag-test-account"),
        None
    );
}

#[test]
fn diagnostics_notice_is_silent_for_an_empty_set() {
    assert_eq!(
        diagnostics_notice(
            None,
            &thegn_svc::calendar::PageDiagnostics::default(),
            "diag-test-account"
        ),
        None
    );
}

#[test]
fn diagnostics_notice_names_zones_only_when_present() {
    let without_zones = thegn_svc::calendar::PageDiagnostics {
        other: 1,
        ..Default::default()
    };
    let with_zones = thegn_svc::calendar::PageDiagnostics {
        unresolved_zones: 2,
        ..Default::default()
    };

    let plain = diagnostics_notice(None, &without_zones, "diag-test-account").unwrap();
    let zoned = diagnostics_notice(None, &with_zones, "diag-test-account").unwrap();

    assert!(!plain.contains("unknown time zone"), "{plain}");
    assert!(zoned.contains("2 with an unknown time zone"), "{zoned}");
}

#[test]
fn diagnostics_notice_names_rejected_defects() {
    let diagnostics = thegn_svc::calendar::PageDiagnostics {
        quarantined_events: 2,
        rejected: thegn_svc::calendar::RejectedEvents {
            reversed: 1,
            mixed_shape: 1,
            ..Default::default()
        },
        ..Default::default()
    };

    let notice = diagnostics_notice(None, &diagnostics, "diag-test-account").unwrap();

    assert!(
        notice.contains("rejected: mixed all-day and timed endpoints (1), end precedes start (1)"),
        "{notice}"
    );
}

#[test]
fn diagnostics_notice_omits_rejections_when_none() {
    let diagnostics = thegn_svc::calendar::PageDiagnostics {
        quarantined_events: 1,
        ..Default::default()
    };

    let notice = diagnostics_notice(None, &diagnostics, "diag-test-account").unwrap();

    assert!(!notice.contains("rejected:"), "{notice}");
}

#[test]
fn note_page_diagnostics_rearms_after_a_clean_page() {
    let account = "diag-rearm-test";
    let diagnostics = thegn_svc::calendar::PageDiagnostics {
        quarantined_events: 1,
        ..Default::default()
    };

    assert!(note_page_diagnostics(account, diagnostics).is_some());
    assert_eq!(note_page_diagnostics(account, diagnostics), None);
    assert_eq!(
        note_page_diagnostics(account, thegn_svc::calendar::PageDiagnostics::default()),
        None
    );
    assert!(note_page_diagnostics(account, diagnostics).is_some());
}

#[test]
fn local_ics_page_diagnostics_are_persisted_for_doctor() {
    use thegn_core::store::CalendarStore as _;

    let dir = tempfile::tempdir().unwrap();
    let db = thegn_core::db::Db::open_at(&dir.path().join("thegn.db")).unwrap();
    let file = dir.path().join("feed.ics");
    std::fs::write(
        &file,
        "BEGIN:VCALENDAR\r\nBEGIN:VEVENT\r\nUID:good\r\nDTSTART:20260821T090000Z\r\nDTEND:20260821T100000Z\r\nEND:VEVENT\r\nBEGIN:VEVENT\r\nUID:nostart\r\nSUMMARY:x\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n",
    )
    .unwrap();
    let cfg = CalendarConfig {
        accounts: vec![CalendarAccount {
            name: "diag-persist-local".into(),
            provider: thegn_core::config_calendar::CalendarProviderKind::Ics,
            path: file.display().to_string(),
            ..Default::default()
        }],
        ..CalendarConfig::default()
    };
    let from = NaiveDate::from_ymd_opt(2026, 8, 1).unwrap();
    let to = NaiveDate::from_ymd_opt(2026, 8, 31).unwrap();
    assert!(sync_accounts(&db, &cfg, from, to, true, &mut |_| {}));
    let rows = db.calendar_diagnostics().unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].account, "diag-persist-local");
    assert_eq!(rows[0].counts.quarantined_events, 1);

    std::fs::write(
        &file,
        "BEGIN:VCALENDAR\r\nBEGIN:VEVENT\r\nUID:good\r\nDTSTART:20260821T090000Z\r\nDTEND:20260821T100000Z\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n",
    )
    .unwrap();
    assert!(sync_accounts(&db, &cfg, from, to, true, &mut |_| {}));
    assert_eq!(
        db.calendar_diagnostics().unwrap()[0]
            .counts
            .quarantined_events,
        0
    );
}

#[test]
fn an_unchanged_page_keeps_the_last_persisted_diagnostics() {
    use thegn_core::store::{CalendarDiagnosticCounts, CalendarStore as _};

    let db = thegn_core::db::Db::open_memory().unwrap();
    db.put_calendar_diagnostics(
        "diag-unchanged",
        &CalendarDiagnosticCounts {
            quarantined_events: 2,
            ..Default::default()
        },
    )
    .unwrap();
    let page = EventPage::unchanged("tok");
    let window = CalendarWindow::new(
        NaiveDate::from_ymd_opt(2026, 8, 1).unwrap(),
        NaiveDate::from_ymd_opt(2026, 8, 31).unwrap(),
        chrono_tz::Tz::UTC,
    )
    .unwrap();

    assert!(!apply_page_with_generation(
        &db,
        "diag-unchanged",
        "ics",
        &page,
        window,
        None,
    ));
    assert_eq!(
        db.calendar_diagnostics().unwrap()[0]
            .counts
            .quarantined_events,
        2
    );
}
