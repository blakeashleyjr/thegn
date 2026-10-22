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
