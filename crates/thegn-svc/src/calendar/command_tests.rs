use super::*;
use thegn_core::calendar::admission::{AdmissionBudget, AdmissionMeter};
use thegn_core::calendar::conformance::{FIXTURES, Verdict};

fn sink() -> AdmittingSink {
    AdmittingSink {
        admitted: Admitted {
            meter: AdmissionMeter::isolated(AdmissionBudget::default()),
            events: Vec::new(),
            deleted: Vec::new(),
            sync_token: String::new(),
            mode: None,
            refusal: None,
            invalid_event: None,
            cursor_conflict: false,
        },
        granted: Vec::new(),
        junk: Vec::new(),
        error: None,
        saw_events: false,
        saw_manifest: false,
    }
}

#[test]
fn conformance_command_plugin_matches_every_fixture() {
    let mut table = Vec::new();
    for fixture in FIXTURES {
        if let Some((json, expected)) = fixture.json {
            let line =
                format!(r#"{{"method":"events","params":{{"events":[{json}],"sync_token":""}}}}"#);
            let mut sink = sink();
            let accepted = sink.line(&line);
            let actual = if accepted && sink.admitted.events.len() == 1 {
                Verdict::Admit
            } else {
                Verdict::Reject
            };
            let rejection_shape = expected != Verdict::Reject
                || (sink.admitted.events.is_empty()
                    && (sink.admitted.invalid_event.is_some() || sink.error.is_some()));
            table.push((
                fixture.name,
                "command plugin",
                expected,
                actual,
                rejection_shape,
            ));
        }
    }
    eprintln!("calendar conformance command table: {table:#?}");
    assert!(
        table
            .iter()
            .all(|(_, _, expected, actual, rejection_shape)| {
                expected == actual && *rejection_shape
            }),
        "calendar conformance disagreement: {table:#?}"
    );
}

fn manifest(contributions: &str, capabilities: &str) -> String {
    format!(
        r#"{{"method":"manifest","params":{{"id":"p","name":"p","version":"1","api":"0.1.0","capabilities":{capabilities},"contributions":{contributions}}}}}"#
    )
}

#[test]
fn unknown_verb_fails_the_run() {
    let mut sink = sink();
    assert!(!sink.line(r#"{"method":"mystery","params":{"secret":"not echoed"}}"#));
    assert!(
        matches!(sink.error, Some(CalendarError::Parse(message)) if message.contains("unknown message") && !message.contains("secret"))
    );
}

#[test]
fn manifest_after_events_fails_the_run() {
    let mut sink = sink();
    assert!(sink.line(r#"{"method":"events","params":{}}"#));
    assert!(!sink.line(&manifest("[]", "[]")));
    assert!(
        matches!(sink.error, Some(CalendarError::Parse(message)) if message.contains("after events"))
    );
}

#[test]
fn second_manifest_fails_the_run() {
    let mut sink = sink();
    let line = manifest("[]", "[]");
    assert!(sink.line(&line));
    assert!(!sink.line(&line));
    assert!(
        matches!(sink.error, Some(CalendarError::Parse(message)) if message.contains("second manifest"))
    );
}

#[test]
fn denied_capability_fails_the_run() {
    let mut sink = sink();
    assert!(!sink.line(&manifest("[]", r#"["network:evil.example.com"]"#)));
    assert!(
        matches!(sink.error, Some(CalendarError::Api(message)) if message.contains("network:evil.example.com") && message.contains("not granted"))
    );
}

#[test]
fn unsupported_contribution_fails_the_run() {
    let mut sink = sink();
    let contributions = r#"[{"id":"p.sidebar","extension_point":"SidebarTab","label":"p"}]"#;
    assert!(!sink.line(&manifest(contributions, "[]")));
    assert!(
        matches!(sink.error, Some(CalendarError::Api(message)) if message.contains("SidebarTab"))
    );
}

#[test]
fn conflicting_sync_tokens_fail_the_run() {
    let mut sink = sink();
    assert!(sink.line(r#"{"method":"events","params":{"sync_token":"first"}}"#));
    assert!(!sink.line(r#"{"method":"events","params":{"sync_token":"second"}}"#));
    assert!(
        matches!(sink.error, Some(CalendarError::Parse(message)) if message.contains("conflicting sync tokens") && !message.contains("first") && !message.contains("second"))
    );
}

#[test]
fn manifest_less_events_still_succeed() {
    let mut sink = sink();
    assert!(sink.line(r#"{"method":"events","params":{"sync_token":"cursor"}}"#));
    assert!(sink.error.is_none());
}

#[test]
fn repeated_identical_sync_token_is_accepted() {
    let mut sink = sink();
    assert!(sink.line(r#"{"method":"events","params":{"sync_token":"cursor"}}"#));
    assert!(sink.line(r#"{"method":"events","params":{"sync_token":"cursor"}}"#));
    assert!(sink.line(r#"{"method":"events","params":{"sync_token":""}}"#));
    assert_eq!(sink.admitted.sync_token, "cursor");
    assert!(sink.error.is_none());
}

#[test]
fn log_before_manifest_is_fine() {
    let mut sink = sink();
    assert!(sink.line(r#"{"method":"log","params":{"message":"ready"}}"#));
    assert!(sink.line(&manifest("[]", "[]")));
    assert!(sink.error.is_none());
}

#[test]
fn plugin_event_with_empty_uid_is_refused_before_admission() {
    let mut sink = sink();
    let line = r#"{"method":"events","params":{"events":[{"uid":"","title":"meeting","start":{"kind":"date","date":"2026-10-12"},"end":{"kind":"date","date":"2026-10-13"}}]}}"#;

    assert!(matches!(
        sink.events_line(line),
        Err(CalendarError::Parse(message)) if message.contains("invalid event (element 0: empty UID)")
    ));
    assert!(sink.admitted.events.is_empty());
}

#[test]
fn plugin_event_with_mixed_date_and_timed_end_is_refused() {
    let mut sink = sink();
    let line = r#"{"method":"events","params":{"events":[{"uid":"meeting","title":"meeting","start":{"kind":"date","date":"2026-10-12"},"end":{"kind":"zoned","local":"2026-10-12T10:00:00","zone":"UTC"}}]}}"#;

    assert!(matches!(
        sink.events_line(line),
        Err(CalendarError::Parse(message)) if message.contains("invalid event (element 0: mixed all-day and timed endpoints)")
    ));
    assert!(sink.admitted.events.is_empty());
}

// THE-470 regression lock: expected to pass on main.
#[test]
fn transp_command_plugin_value_is_kept_as_sent() {
    let mut sink = sink();
    let line = r#"{"method":"events","params":{"events":[{"uid":"plugin-free","title":"Travel","start":{"kind":"date","date":"2026-10-12"},"end":{"kind":"date","date":"2026-10-13"},"busy":"free"},{"uid":"plugin-default","title":"Meeting","start":{"kind":"date","date":"2026-10-14"},"end":{"kind":"date","date":"2026-10-15"}}]}}"#;

    assert!(sink.line(line));
    assert_eq!(sink.admitted.events.len(), 2);
    assert_eq!(
        sink.admitted.events[0].busy,
        thegn_core::calendar::Busy::Free
    );
    assert_eq!(
        sink.admitted.events[1].busy,
        thegn_core::calendar::Busy::Busy
    );
}
