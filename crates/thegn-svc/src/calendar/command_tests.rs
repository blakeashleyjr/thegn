use super::*;
use std::collections::BTreeMap;
use thegn_core::calendar::admission::{AdmissionBudget, AdmissionMeter};
use thegn_core::calendar::conformance::{FIXTURES, Verdict};

fn sink() -> AdmittingSink {
    sink_with(SecretScrub::default())
}

fn sink_with(scrub: SecretScrub) -> AdmittingSink {
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
        scrub,
        junk: Vec::new(),
        error: None,
        saw_events: false,
        saw_manifest: false,
    }
}

fn plugin(argv: Vec<String>, env: &[(&str, &str)]) -> CommandBackend {
    let account = CalendarAccount {
        name: "scrub-test".into(),
        provider: thegn_core::config_calendar::CalendarProviderKind::Command,
        command: argv,
        env: env
            .iter()
            .map(|(key, value)| ((*key).to_string(), (*value).to_string()))
            .collect(),
        ..CalendarAccount::default()
    };
    CommandBackend::new(
        &account,
        super::AccountAdmission::isolated(
            AdmissionBudget::default(),
            thegn_core::calendar::Tz::UTC,
        ),
    )
}

fn block_on<F: std::future::Future>(future: F) -> F::Output {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(future)
}

#[test]
#[allow(
    clippy::disallowed_macros,
    reason = "conformance matrix prints every fixture x seam verdict before asserting"
)]
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
fn plugin_stderr_never_carries_an_env_secret() {
    let backend = plugin(
        vec![
            "sh".into(),
            "-c".into(),
            "echo \"auth failed for $CAL_TOKEN\" >&2; exit 1".into(),
        ],
        &[("CAL_TOKEN", "tok-sentinel-7f3a91")],
    );
    let from = chrono::NaiveDate::from_ymd_opt(2026, 8, 1).unwrap();
    let to = chrono::NaiveDate::from_ymd_opt(2026, 8, 31).unwrap();
    let error = block_on(backend.list_events(from, to, "")).unwrap_err();
    let display = format!("{error}");
    let debug = format!("{error:?}");
    for text in [&display, &debug] {
        assert!(text.contains("auth failed for"), "{text}");
        assert!(text.contains("<redacted>"), "{text}");
        assert!(!text.contains("tok-sentinel-7f3a91"), "{text}");
    }
}

#[test]
fn plugin_stderr_never_carries_an_argument_secret() {
    let backend = plugin(
        vec![
            "sh".into(),
            "-c".into(),
            "echo \"bad key $0\" >&2; exit 2".into(),
            "arg-sentinel-c41d".into(),
        ],
        &[],
    );
    let from = chrono::NaiveDate::from_ymd_opt(2026, 8, 1).unwrap();
    let to = chrono::NaiveDate::from_ymd_opt(2026, 8, 31).unwrap();
    let error = block_on(backend.list_events(from, to, "")).unwrap_err();
    let display = format!("{error}");
    let debug = format!("{error:?}");
    for text in [&display, &debug] {
        assert!(text.contains("bad key"), "{text}");
        assert!(text.contains("<redacted>"), "{text}");
        assert!(!text.contains("arg-sentinel-c41d"), "{text}");
    }
}

#[test]
fn sink_scrubs_secrets_from_junk_lines() {
    let env = BTreeMap::from([("K".to_string(), "tok-sentinel-7f3a91".to_string())]);
    let argv = vec!["p".into(), "--token=arg-sentinel-c41d".into()];
    let scrub = SecretScrub::for_account(&argv, &env);
    let mut sink = sink_with(scrub);
    assert!(sink.line("not json tok-sentinel-7f3a91 --token=arg-sentinel-c41d"));
    assert!(sink.junk[0].contains("<redacted>"), "{}", sink.junk[0]);
    assert!(!sink.junk[0].contains("tok-sentinel-7f3a91"));
    assert!(!sink.junk[0].contains("arg-sentinel-c41d"));
}

#[test]
fn sink_scrubs_secrets_from_plugin_log_messages() {
    use std::sync::{Arc, Mutex};
    use tracing::field::{Field, Visit};
    use tracing::span;
    use tracing::{Event, Metadata, Subscriber};

    #[derive(Clone, Default)]
    struct Captured(Arc<Mutex<Vec<BTreeMap<String, String>>>>);
    struct Fields(BTreeMap<String, String>);
    impl Visit for Fields {
        fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
            self.0
                .insert(field.name().to_string(), format!("{value:?}"));
        }
        fn record_str(&mut self, field: &Field, value: &str) {
            self.0.insert(field.name().to_string(), value.to_string());
        }
    }
    impl Subscriber for Captured {
        fn enabled(&self, metadata: &Metadata<'_>) -> bool {
            metadata.target().starts_with("thegn::calendar")
        }
        fn new_span(&self, _: &span::Attributes<'_>) -> span::Id {
            span::Id::from_u64(1)
        }
        fn record(&self, _: &span::Id, _: &span::Record<'_>) {}
        fn record_follows_from(&self, _: &span::Id, _: &span::Id) {}
        fn event(&self, event: &Event<'_>) {
            let mut fields = Fields(BTreeMap::new());
            event.record(&mut fields);
            self.0.lock().unwrap().push(fields.0);
        }
        fn enter(&self, _: &span::Id) {}
        fn exit(&self, _: &span::Id) {}
    }

    let captured = Captured::default();
    let _guard = tracing::subscriber::set_default(captured.clone());
    let env = BTreeMap::from([("K".to_string(), "tok-sentinel-7f3a91".to_string())]);
    let argv = vec!["p".into(), "arg-sentinel-c41d".into()];
    let mut sink = sink_with(SecretScrub::for_account(&argv, &env));
    assert!(sink.line(
        r#"{"method":"log","params":{"level":"warn","message":"token is tok-sentinel-7f3a91"}}"#
    ));
    let records = captured.0.lock().unwrap();
    assert!(!records.is_empty());
    assert!(records.iter().all(|record| {
        record
            .values()
            .all(|value| !value.contains("tok-sentinel-7f3a91"))
    }));
    assert!(records.iter().any(|record| {
        record
            .get("message")
            .is_some_and(|value| value.contains("<redacted>"))
    }));
}

#[test]
fn scrub_error_redacts_every_string_variant() {
    let scrub = SecretScrub::for_account(
        &[],
        &BTreeMap::from([("K".to_string(), "tok-sentinel-7f3a91".to_string())]),
    );
    let errors = [
        (CalendarError::Network("x tok-sentinel-7f3a91 y".into()), 0),
        (CalendarError::Auth("x tok-sentinel-7f3a91 y".into()), 1),
        (CalendarError::Api("x tok-sentinel-7f3a91 y".into()), 2),
        (
            CalendarError::Subprocess("x tok-sentinel-7f3a91 y".into()),
            3,
        ),
        (CalendarError::Parse("x tok-sentinel-7f3a91 y".into()), 4),
        (CalendarError::Io("x tok-sentinel-7f3a91 y".into()), 5),
    ];
    for (error, expected_variant) in errors {
        let scrubbed = scrub.scrub_error(error);
        let text = scrubbed.to_string();
        assert!(text.contains("<redacted>"), "{text}");
        assert!(!text.contains("tok-sentinel-7f3a91"), "{text}");
        assert!(matches!(
            (&scrubbed, expected_variant),
            (CalendarError::Network(_), 0)
                | (CalendarError::Auth(_), 1)
                | (CalendarError::Api(_), 2)
                | (CalendarError::Subprocess(_), 3)
                | (CalendarError::Parse(_), 4)
                | (CalendarError::Io(_), 5)
        ));
    }
    assert!(matches!(
        scrub.scrub_error(CalendarError::Timeout("t")),
        CalendarError::Timeout("t")
    ));
    assert!(matches!(
        scrub.scrub_error(CalendarError::NotConfigured),
        CalendarError::NotConfigured
    ));
}

// Positive control: ordinary stderr remains visible on main and after scrubbing.
#[test]
fn plugin_stderr_without_secrets_is_kept_verbatim() {
    let backend = plugin(
        vec![
            "sh".into(),
            "-c".into(),
            "echo 'khal not found' >&2; exit 1".into(),
        ],
        &[("CAL_TOKEN", "tok-sentinel-7f3a91")],
    );
    let from = chrono::NaiveDate::from_ymd_opt(2026, 8, 1).unwrap();
    let to = chrono::NaiveDate::from_ymd_opt(2026, 8, 31).unwrap();
    let error = block_on(backend.list_events(from, to, "")).unwrap_err();
    assert!(error.to_string().contains("khal not found"));
    assert!(!error.to_string().contains("<redacted>"));
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
