use super::*;
use std::collections::BTreeMap;
use std::sync::atomic::AtomicUsize;
use thegn_core::config::InboxKind;

fn account(argv: Vec<String>, env: &[(&str, &str)]) -> InboxAccount {
    InboxAccount {
        name: "feed".into(),
        kind: InboxKind::Command,
        command: argv,
        env: env
            .iter()
            .map(|(key, value)| ((*key).to_string(), (*value).to_string()))
            .collect(),
        timeout_secs: 20,
        ..InboxAccount::default()
    }
}

/// A backend running `sh -c <script>` (tests only).
fn script(body: &str, env: &[(&str, &str)]) -> CommandBackend {
    CommandBackend::new(&account(vec!["sh".into(), "-c".into(), body.into()], env))
}

fn block_on<F: std::future::Future>(future: F) -> F::Output {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(future)
}

fn sink(resumed: bool, max_items: usize) -> ItemSink {
    ItemSink::new(SecretScrub::default(), max_items, resumed)
}

fn stream(sink: ItemSink) -> StreamRun<ItemSink> {
    StreamRun {
        sink,
        stderr: String::new(),
        truncated: false,
        outcome: Default::default(),
    }
}

fn parse_err(result: Result<InboxPage, InboxError>) -> String {
    match result {
        Err(InboxError::Parse(why)) => why,
        other => panic!("expected a parse error, got {other:?}"),
    }
}

/// A 0600 file holding `value`, as a `file:` ref.
fn file_ref(dir: &std::path::Path, value: &str) -> String {
    let path = dir.join("token");
    std::fs::write(&path, value).unwrap();
    thegn_core::fsperm::restrict_to_owner(&path).unwrap();
    format!("file:{}", path.display())
}

#[test]
fn snapshot_items_parse_across_repeated_messages() {
    let backend = script(
        r#"
echo '{"method":"items","params":{"items":[{"id":"a1","title":"First","url":"https://x.test/a1","from":"Ann","labels":["l1"],"due":"2026-11-20","updated_at":1700000000,"priority":3}],"cursor":"c-1","mode":"snapshot"}}'
echo '{"method":"items","params":{"items":[{"id":"b2"}],"cursor":"c-1"}}'
"#,
        &[],
    );
    let page = block_on(backend.fetch("", 10)).unwrap();
    assert_eq!(page.mode, PageMode::Snapshot);
    assert_eq!(page.cursor.as_deref(), Some("c-1"));
    assert_eq!(page.items.len(), 2);
    let first = &page.items[0];
    assert_eq!(first.id, "a1");
    assert_eq!(first.title, "First");
    assert_eq!(first.url, "https://x.test/a1");
    assert_eq!(first.from, "Ann");
    assert_eq!(first.labels, ["l1"]);
    assert_eq!(first.due.as_deref(), Some("2026-11-20"));
    assert_eq!(first.updated_at, Some(1_700_000_000));
    assert_eq!(first.priority, Some(3));
    assert_eq!(page.items[1].id, "b2");
    assert!(page.removed.is_empty());
    assert!(!page.truncated);
}

#[test]
fn the_query_reaches_the_command_in_its_environment() {
    let backend = script(
        r#"printf '{"method":"items","params":{"items":[{"id":"%s"},{"id":"%s"},{"id":"%s"}],"mode":"delta"}}\n' "$THEGN_INBOX_ACCOUNT" "$THEGN_INBOX_CURSOR" "$THEGN_INBOX_MAX_ITEMS""#,
        &[],
    );
    let page = block_on(backend.fetch("cur-7", 7)).unwrap();
    assert_eq!(page.mode, PageMode::Delta);
    let ids: Vec<_> = page.items.iter().map(|i| i.id.as_str()).collect();
    assert_eq!(ids, ["feed", "cur-7", "7"]);
}

#[test]
fn a_delta_without_a_cursor_fails() {
    let backend = script(
        r#"echo '{"method":"items","params":{"items":[{"id":"a"}],"mode":"delta"}}'"#,
        &[],
    );
    let why = parse_err(block_on(backend.fetch("", 10)));
    assert!(why.contains("no cursor"), "{why}");

    // The sink refuses it too, before the run finishes.
    let mut s = sink(false, 10);
    assert!(!s.line(r#"{"method":"items","params":{"mode":"delta"}}"#));
    assert!(matches!(s.error, Some(InboxError::Parse(_))));
}

#[test]
fn more_than_max_items_in_total_fails() {
    let backend = script(
        r#"
echo '{"method":"items","params":{"items":[{"id":"a"},{"id":"b"}]}}'
echo '{"method":"items","params":{"items":[{"id":"c"}]}}'
"#,
        &[],
    );
    let why = parse_err(block_on(backend.fetch("", 2)));
    assert!(why.contains("more than 2 items"), "{why}");
    // Exactly the budget is fine.
    assert_eq!(block_on(backend.fetch("", 3)).unwrap().items.len(), 3);
}

#[test]
fn unknown_verbs_conflicting_cursors_and_modes_fail_the_run() {
    let mut s = sink(true, 10);
    assert!(!s.line(r#"{"method":"events","params":{}}"#));
    assert!(matches!(s.error, Some(InboxError::Parse(_))));

    let mut s = sink(true, 10);
    assert!(s.line(r#"{"method":"items","params":{"cursor":"a"}}"#));
    assert!(s.line(r#"{"method":"items","params":{"cursor":""}}"#));
    assert!(s.line(r#"{"method":"items","params":{"cursor":"a"}}"#));
    assert!(!s.line(r#"{"method":"items","params":{"cursor":"b"}}"#));
    assert!(
        matches!(&s.error, Some(InboxError::Parse(w)) if w.contains("conflicting cursors")),
        "{:?}",
        s.error
    );

    let mut s = sink(true, 10);
    assert!(s.line(r#"{"method":"items","params":{"mode":"delta"}}"#));
    assert!(!s.line(r#"{"method":"items","params":{"mode":"snapshot"}}"#));

    let mut s = sink(true, 10);
    assert!(!s.line(r#"{"method":"items","params":{"mode":"bogus"}}"#));
}

#[test]
fn a_snapshot_with_removed_ids_and_a_run_with_no_items_message_fail() {
    let mut s = sink(false, 10);
    assert!(s.line(r#"{"method":"items","params":{"removed":["x"]}}"#));
    let why = parse_err(finish(stream(s), false));
    assert!(why.contains("removed"), "{why}");

    let mut s = sink(false, 10);
    assert!(s.line(r#"{"method":"log","params":{"message":"hi"}}"#));
    let why = parse_err(finish(stream(s), false));
    assert!(why.contains("no items message"), "{why}");

    // An empty snapshot is a real answer.
    let mut s = sink(false, 10);
    assert!(s.line(r#"{"method":"items","params":{}}"#));
    assert!(finish(stream(s), false).unwrap().items.is_empty());
}

#[test]
fn junk_lines_are_counted_kept_three_and_quoted_in_the_error() {
    let mut s = sink(false, 10);
    for n in 0..5 {
        assert!(s.line(&format!("junk line {n}")));
    }
    assert_eq!(s.junk_lines, 5);
    assert_eq!(s.junk.len(), MAX_JUNK_KEPT);
    let why = parse_err(finish(stream(s), false));
    assert!(why.contains("junk line 0"), "{why}");
    assert!(why.contains("junk line 2"), "{why}");
    assert!(!why.contains("junk line 3"), "{why}");

    let mut long = sink(false, 10);
    assert!(long.line(&"z".repeat(1000)));
    assert_eq!(long.junk[0].chars().count(), MAX_JUNK_CHARS);

    // Junk alongside a good items message does not fail the run.
    let mut s = sink(false, 10);
    assert!(s.line("hello"));
    assert!(s.line(r#"{"method":"items","params":{"items":[{"id":"a"}]}}"#));
    assert_eq!(finish(stream(s), false).unwrap().items.len(), 1);

    // A truncated run is refused.
    let mut s = sink(false, 10);
    assert!(s.line(r#"{"method":"items","params":{"items":[{"id":"a"}]}}"#));
    let mut run = stream(s);
    run.truncated = true;
    assert!(matches!(finish(run, false), Err(InboxError::Parse(_))));
}

#[test]
fn a_secret_ref_in_env_is_resolved_and_never_reaches_an_error() {
    let dir = tempfile::tempdir().unwrap();
    let secret = "inbox-sentinel-secret-5c2e91";
    let token = file_ref(dir.path(), secret);
    let seen = dir.path().join("seen");

    // The child gets the resolved value.
    let backend = script(
        r#"printf %s "$TOKEN" > "$SEEN"; echo '{"method":"items","params":{}}'"#,
        &[("TOKEN", &token), ("SEEN", &seen.to_string_lossy())],
    );
    block_on(backend.fetch("", 5)).unwrap();
    assert_eq!(std::fs::read_to_string(&seen).unwrap(), secret);

    // Echoed to stderr on a failing exit: scrubbed.
    let backend = script(
        r#"echo "token is $TOKEN" >&2; exit 3"#,
        &[("TOKEN", &token)],
    );
    match block_on(backend.fetch("", 5)) {
        Err(InboxError::Subprocess(text)) => {
            assert!(!text.contains(secret), "{text}");
            assert!(text.contains("<redacted>"), "{text}");
        }
        other => panic!("expected a subprocess error, got {other:?}"),
    }

    // Echoed as junk stdout: scrubbed in the error.
    let backend = script(r#"echo "leak $TOKEN""#, &[("TOKEN", &token)]);
    let err = block_on(backend.fetch("", 5)).unwrap_err();
    assert!(!err.to_string().contains(secret), "{err}");
    assert!(err.to_string().contains("<redacted>"), "{err}");

    // Echoed into an unknown verb: scrubbed.
    let backend = script(
        r#"printf '{"method":"%s"}\n' "$TOKEN""#,
        &[("TOKEN", &token)],
    );
    let err = block_on(backend.fetch("", 5)).unwrap_err();
    assert!(!err.to_string().contains(secret), "{err}");
}

#[test]
fn a_long_secret_cut_into_an_unknown_verb_quote_is_still_redacted() {
    let secret = "a-much-longer-sentinel-secret-value-0123456789";
    let env = BTreeMap::from([("K".to_string(), secret.to_string())]);
    let mut s = ItemSink::new(SecretScrub::for_account(&[], &env), 10, false);
    assert!(!s.line(&format!(r#"{{"method":"{secret}"}}"#)));
    let text = s.error.unwrap().to_string();
    assert!(!text.contains(&secret[..20]), "{text}");
    assert!(text.contains("<redacted>"), "{text}");
}

static RUNS: AtomicUsize = AtomicUsize::new(0);

fn counting_runner(
    _argv: &[String],
    _env: &BTreeMap<String, String>,
    _cwd: Option<&Path>,
    _timeout: Duration,
    sink: ItemSink,
    _cancel: &dyn Fn() -> bool,
) -> Result<StreamRun<ItemSink>, PluginError> {
    RUNS.fetch_add(1, Ordering::SeqCst);
    let mut sink = sink;
    sink.line(r#"{"method":"items","params":{}}"#);
    Ok(stream(sink))
}

#[test]
fn an_unresolved_ref_spawns_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let missing = format!("file:{}", dir.path().join("absent").display());
    let backend = CommandBackend::new(&account(
        vec!["true".into()],
        &[("PLAIN", "literal"), ("TOKEN", &missing)],
    ))
    .with_runner(counting_runner);
    match block_on(backend.fetch("", 5)) {
        Err(InboxError::Auth(why)) => {
            assert_eq!(why, "env TOKEN secret ref unresolved");
        }
        other => panic!("expected an auth error, got {other:?}"),
    }
    assert_eq!(RUNS.load(Ordering::SeqCst), 0, "nothing may be spawned");

    // The seam is live: a resolvable env does reach the runner.
    let backend = CommandBackend::new(&account(vec!["true".into()], &[("PLAIN", "literal")]))
        .with_runner(counting_runner);
    block_on(backend.fetch("", 5)).unwrap();
    assert_eq!(RUNS.load(Ordering::SeqCst), 1);
}

// Unix-only: probes the killed child with `libc::kill(pid, 0)`, and `libc` is a
// unix-only dependency of this crate.
#[cfg(unix)]
#[test]
fn a_timeout_kills_the_process_group() {
    let dir = tempfile::tempdir().unwrap();
    let pid_file = dir.path().join("child.pid");
    let mut acct = account(
        vec![
            "sh".into(),
            "-c".into(),
            "sleep 120 & echo $! > \"$1\"; wait".into(),
            "sh".into(),
            pid_file.to_string_lossy().into_owned(),
        ],
        &[],
    );
    acct.timeout_secs = 1;
    let backend = CommandBackend::new(&acct);
    assert_eq!(block_on(backend.fetch("", 5)), Err(InboxError::Timeout));

    let child_pid: i32 = std::fs::read_to_string(&pid_file)
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    loop {
        // SAFETY: signal 0 only checks whether this pid still exists.
        if unsafe { libc::kill(child_pid, 0) } == -1
            && std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH)
        {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "timed-out inbox command left child {child_pid} alive"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn a_missing_program_and_an_empty_command_are_errors() {
    let backend = CommandBackend::new(&account(
        vec!["/definitely/not/here/thegn-inbox-cmd".into()],
        &[],
    ));
    assert!(matches!(
        block_on(backend.fetch("", 5)),
        Err(InboxError::Io(_))
    ));
    let backend = CommandBackend::new(&account(Vec::new(), &[]));
    assert_eq!(
        block_on(backend.fetch("", 5)),
        Err(InboxError::NotConfigured)
    );
}

#[test]
fn log_lines_go_to_tracing_only() {
    use std::sync::Mutex;
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
            metadata.target().starts_with("thegn::inbox")
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
    let env = BTreeMap::from([("K".to_string(), "tok-sentinel-inbox-77".to_string())]);
    let mut s = ItemSink::new(SecretScrub::for_account(&[], &env), 10, false);
    assert!(s.line(
        r#"{"method":"log","params":{"level":"warn","message":"from the command tok-sentinel-inbox-77"}}"#
    ));
    assert!(s.line(r#"{"method":"items","params":{"items":[{"id":"a"}]}}"#));
    let page = finish(stream(s), false).unwrap();
    // Nothing from the log line reaches the page.
    assert_eq!(page.items.len(), 1);
    assert_eq!(page.items[0].id, "a");
    assert!(page.items[0].title.is_empty());

    let records = captured.0.lock().unwrap();
    assert!(records.iter().any(|record| {
        record
            .get("text")
            .is_some_and(|value| value.contains("from the command <redacted>"))
    }));
    assert!(records.iter().all(|record| {
        record
            .values()
            .all(|value| !value.contains("tok-sentinel-inbox-77"))
    }));
}

#[test]
fn scrub_redacts_env_values_and_arguments_longest_first() {
    let scrub = SecretScrub::for_account(
        &["prog".into(), "arg-secret".into()],
        &BTreeMap::from([
            ("A".to_string(), "abc".to_string()),
            ("B".to_string(), "abcdef".to_string()),
            ("BLANK".to_string(), "  ".to_string()),
        ]),
    );
    assert_eq!(
        scrub.scrub("x abcdef y abc z arg-secret prog"),
        "x <redacted> y <redacted> z <redacted> prog"
    );
    // Single pass: an inserted marker is never rewritten.
    let scrub = SecretScrub::for_account(
        &[],
        &BTreeMap::from([
            ("A".to_string(), "a".to_string()),
            ("B".to_string(), "ed".to_string()),
        ]),
    );
    assert_eq!(scrub.scrub("a"), "<redacted>");
}

#[test]
fn capped_stderr_redacts_a_secret_prefix_at_the_limit() {
    let secret = "sentinel-secret-value";
    let prefix = &secret[..secret.len() / 2];
    let scrub = SecretScrub::for_account(
        &[],
        &BTreeMap::from([("K".to_string(), secret.to_string())]),
    );
    let stderr = format!("{}{}", "x".repeat(proc::MAX_STDERR - prefix.len()), prefix);
    let scrubbed = scrub.scrub_capped_stderr(&stderr);
    assert!(scrubbed.ends_with("<redacted>"), "{scrubbed:?}");
}

#[test]
fn scrub_error_redacts_every_string_variant() {
    let scrub = SecretScrub::for_account(
        &[],
        &BTreeMap::from([("K".to_string(), "s3cr3t".to_string())]),
    );
    for error in [
        InboxError::Network("s3cr3t".into()),
        InboxError::Auth("s3cr3t".into()),
        InboxError::Api("s3cr3t".into()),
        InboxError::Subprocess("s3cr3t".into()),
        InboxError::Parse("s3cr3t".into()),
        InboxError::Io("s3cr3t".into()),
    ] {
        let scrubbed = scrub.scrub_error(error);
        assert!(!scrubbed.to_string().contains("s3cr3t"), "{scrubbed:?}");
    }
    assert_eq!(scrub.scrub_error(InboxError::Timeout), InboxError::Timeout);
}

#[test]
fn literal_env_values_pass_through_and_refs_are_tagged_by_account() {
    assert_eq!(resolve_secret("feed", "plain").as_deref(), Some("plain"));
    assert_eq!(resolve_secret("feed", "").as_deref(), Some(""));
    let dir = tempfile::tempdir().unwrap();
    let token = file_ref(dir.path(), "v4lue");
    assert_eq!(resolve_secret("feed", &token).as_deref(), Some("v4lue"));
    assert_eq!(resolve_secret("feed", "file:/definitely/absent"), None);
}

#[test]
fn caps_kind_and_timeout_clamp() {
    let mut acct = account(vec!["true".into()], &[]);
    acct.timeout_secs = 0;
    assert_eq!(CommandBackend::new(&acct).timeout, Duration::from_secs(1));
    acct.timeout_secs = 10_000;
    assert_eq!(
        CommandBackend::new(&acct).timeout,
        Duration::from_secs(MAX_TIMEOUT_SECS)
    );
    let backend = CommandBackend::new(&acct);
    assert_eq!(backend.kind(), "command");
    assert!(backend.caps().cursor);
    assert!(!backend.caps().mark_read);
}

#[test]
fn probe_reports_availability_and_the_authority_note() {
    let report = probe(&account(vec!["sh".into()], &[]));
    assert_eq!(report.id, "command:feed");
    assert!(report.availability.is_ready());
    assert!(report.notes.iter().any(|n| n == AUTHORITY_NOTE));

    let report = probe(&account(vec!["/definitely/not/here".into()], &[]));
    assert!(report.availability.is_unavailable());

    let dir = tempfile::tempdir().unwrap();
    let bin = dir.path().join("fetch.sh");
    std::fs::write(&bin, "#!/bin/sh\n").unwrap();
    let report = probe(&account(vec![bin.to_string_lossy().into_owned()], &[]));
    assert!(report.availability.is_ready());

    let report = probe(&account(
        vec!["thegn-definitely-not-a-binary-xyz".into()],
        &[],
    ));
    assert!(report.availability.is_unavailable());
    assert!(
        probe(&account(Vec::new(), &[]))
            .availability
            .is_unavailable()
    );
}
