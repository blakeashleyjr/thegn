//! End-to-end exercise of `LspClient` against the hermetic `fake_lsp` server
//! (selected via `CARGO_BIN_EXE_fake_lsp`) — no real language server needed.

use std::time::{Duration, Instant};

use thegn_svc::lsp::{
    DiagnosticsReceiver, DiagnosticsSender, LspClient, LspError, Position, PublishedDiagnostics,
    ServerSpec, SymbolKind, diagnostics_channel, framing::FrameDecoder, limits,
};

fn spec_with(args: Vec<String>) -> ServerSpec {
    ServerSpec {
        key: "rust".to_string(),
        language_id: "rust".to_string(),
        command: env!("CARGO_BIN_EXE_fake_lsp").to_string(),
        args,
    }
}

fn start_fake() -> (LspClient, DiagnosticsReceiver) {
    start_fake_args(vec![])
}

fn start_fake_args(args: Vec<String>) -> (LspClient, DiagnosticsReceiver) {
    let (diag_tx, diag_rx) = diagnostics_channel();
    let root = std::env::temp_dir();
    let client = LspClient::start(&spec_with(args), &root, diag_tx).expect("spawn fake server");
    (client, diag_rx)
}

fn start_on(bus: &DiagnosticsSender, root: &std::path::Path, args: &[&str]) -> LspClient {
    let client = LspClient::start(
        &spec_with(args.iter().map(|a| a.to_string()).collect()),
        root,
        bus.clone(),
    )
    .expect("spawn fake server");
    client.initialize(root).expect("initialize");
    client
}

fn recv_timeout(rx: &DiagnosticsReceiver, timeout: Duration) -> Option<PublishedDiagnostics> {
    let deadline = Instant::now() + timeout;
    loop {
        if let Ok(pd) = rx.try_recv() {
            return Some(pd);
        }
        if Instant::now() >= deadline {
            return None;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
}

fn temp_root(tag: &str) -> std::path::PathBuf {
    let root = std::env::temp_dir().join(format!("thegn-lsp-{tag}-{}", std::process::id()));
    std::fs::create_dir_all(&root).expect("temp root");
    root
}

#[test]
fn initialize_handshake_and_pushed_diagnostics() {
    let (client, diag_rx) = start_fake();
    client
        .initialize(&std::env::temp_dir())
        .expect("initialize");

    let pd = recv_timeout(&diag_rx, Duration::from_secs(3))
        .expect("diagnostics pushed after initialize");
    assert_eq!(pd.path, "/proj/src/lib.rs");
    assert_eq!(pd.diagnostics.len(), 1);
    assert_eq!(pd.diagnostics[0].message, "fake error");
    assert_eq!(pd.diagnostics[0].code.as_deref(), Some("F001"));
}

#[test]
fn requests_return_mapped_results() {
    let (client, _diag_rx) = start_fake();
    client
        .initialize(&std::env::temp_dir())
        .expect("initialize");

    let uri = "file:///proj/src/lib.rs";

    let symbols = client.document_symbols(uri).expect("documentSymbol");
    assert_eq!(symbols.len(), 1);
    assert_eq!(symbols[0].name, "lspProbe");
    assert_eq!(symbols[0].kind, SymbolKind::Function);
    assert_eq!(symbols[0].location.line_1based(), 1);

    let ws = client.workspace_symbols("gr").expect("workspace/symbol");
    assert_eq!(ws.len(), 1);
    assert_eq!(ws[0].location.path, "/proj/src/lib.rs");

    let defs = client
        .definition(
            uri,
            Position {
                line: 5,
                character: 4,
            },
        )
        .expect("definition");
    assert_eq!(defs.len(), 1);
    assert_eq!(defs[0].line_1based(), 1);

    let refs = client
        .references(
            uri,
            Position {
                line: 0,
                character: 3,
            },
        )
        .expect("references");
    assert_eq!(refs.len(), 2);
    assert_eq!(refs[1].path, "/proj/src/main.rs");
    assert_eq!(refs[1].line_1based(), 10);

    let hover = client
        .hover(
            uri,
            Position {
                line: 0,
                character: 3,
            },
        )
        .expect("hover")
        .expect("hover content");
    assert_eq!(hover.markdown, "fn greet() -> u8");
}

#[test]
fn undeclared_capability_is_not_sent_and_returns_not_available() {
    // The fake declares hoverProvider by default, so hover works…
    let (client, _d) = start_fake();
    client
        .initialize(&std::env::temp_dir())
        .expect("initialize");
    assert!(
        client
            .hover(
                "file:///proj/src/lib.rs",
                Position {
                    line: 0,
                    character: 3,
                },
            )
            .expect("hover ok")
            .is_some(),
        "hover works when the server declares hoverProvider"
    );

    // …but with `--no-hover` the server declares no hoverProvider, so the gate
    // returns NotAvailable WITHOUT sending a request. If the request had been
    // sent, the fake replies with content (Ok(Some)) or Null (Ok(None)) — never
    // NotAvailable — so this distinguishes "gated off" from "sent".
    let (gated, _d) = start_fake_args(vec!["--no-hover".to_string()]);
    gated.initialize(&std::env::temp_dir()).expect("initialize");
    // documentSymbol is still declared, proving the handshake and other methods
    // are unaffected.
    assert_eq!(
        gated
            .document_symbols("file:///proj/src/lib.rs")
            .unwrap()
            .len(),
        1
    );
    let err = gated
        .hover(
            "file:///proj/src/lib.rs",
            Position {
                line: 0,
                character: 3,
            },
        )
        .expect_err("hover gated off");
    assert_eq!(err, LspError::NotAvailable);
}

#[test]
fn framing_smoke_for_test_helpers() {
    // Guards that the shared codec the fake server uses is sane.
    let mut d = FrameDecoder::new();
    d.push(&thegn_svc::lsp::framing::encode("{\"x\":1}"))
        .expect("valid frame fits decoder bounds");
    assert_eq!(
        d.next_message().expect("valid frame decodes").as_deref(),
        Some("{\"x\":1}")
    );
}

#[test]
fn fake_server_deep_symbol_response_fails_promptly_before_value_projection() {
    let (diagnostics, health) = diagnostics_channel();
    let root = std::env::temp_dir();
    let client = LspClient::start(
        &spec_with(vec!["--deep-symbols".to_string()]),
        &root,
        diagnostics,
    )
    .expect("spawn fake server");
    client.initialize(&root).expect("initialize");
    let started = Instant::now();
    let err = client
        .document_symbols("file:///proj/src/lib.rs")
        .expect_err("over-depth response must not be projected");
    assert!(
        matches!(err, LspError::Bounded(ref reason) if reason.contains("json depth limit")),
        "{err:?}"
    );
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "failed at once, not at the deadline"
    );
    assert!(health.health().invalid >= 1);
}

#[test]
fn fake_server_unique_document_flood_is_bounded_and_accounted() {
    let (bus, rx) = diagnostics_channel();
    let root = temp_root("flood");
    let flood = 1_000;
    let _client = start_on(&bus, &root, &["--flood", &flood.to_string()]);
    // initialize() returned ⇒ every flood notification was dispatched.
    let f = rx.footprint();
    assert_eq!(f.bytes, f.recomputed_bytes);
    assert_eq!(f.metadata_bytes, f.recomputed_metadata_bytes);
    assert_eq!(f.documents, limits::MAX_QUEUE_DOCUMENTS);
    assert!(f.bytes <= limits::MAX_QUEUE_BYTES);
    // The flood (plus, racing the reply, the fixture's one post-initialize
    // document) overflowed the queue; every eviction is counted.
    let dropped = rx.health().dropped as usize;
    assert!(
        (flood - limits::MAX_QUEUE_DOCUMENTS..=flood + 1 - limits::MAX_QUEUE_DOCUMENTS)
            .contains(&dropped),
        "dropped={dropped}"
    );
    let marks = rx.take_loss_marks();
    assert_eq!(marks.documents.len(), limits::MAX_LOSS_MARKS);
    assert_eq!(
        marks.streams.len(),
        1,
        "overflowing losses escalate to the stream"
    );
    // Slow consumer recovery: everything retained drains, newest documents survive.
    let mut drained = Vec::new();
    while let Some(pd) = recv_timeout(&rx, Duration::from_millis(200)) {
        drained.push(pd);
    }
    assert!(drained.len() >= limits::MAX_QUEUE_DOCUMENTS);
    assert!(
        drained
            .iter()
            .any(|pd| pd.path == format!("/proj/flood/{}.rs", flood - 1)),
        "the newest document is retained, the oldest were evicted"
    );
    let f = rx.footprint();
    assert_eq!((f.bytes, f.documents), (0, 0));
}

#[test]
fn fake_servers_quiet_root_progresses_beside_a_flooding_root() {
    let (bus, rx) = diagnostics_channel();
    let loud_root = temp_root("loud");
    let quiet_root = temp_root("quiet");
    let _loud = start_on(&bus, &loud_root, &["--flood", "600"]);
    // `--flood 1` queues the quiet server's one document before its
    // initialize reply, so both roots are ready before draining starts.
    let _quiet = start_on(&bus, &quiet_root, &["--flood", "1"]);
    let deadline = Instant::now() + Duration::from_secs(3);
    let mut seen_quiet_at = None;
    let mut index = 0usize;
    while seen_quiet_at.is_none() && Instant::now() < deadline {
        match rx.try_recv() {
            Ok(pd) => {
                if pd.root == quiet_root {
                    seen_quiet_at = Some(index);
                }
                index += 1;
            }
            Err(_) => std::thread::sleep(Duration::from_millis(5)),
        }
    }
    let at = seen_quiet_at.expect("quiet root delivered");
    assert!(
        at <= 1,
        "round-robin serves the quiet root immediately, got {at}"
    );
}

#[test]
fn fake_server_update_then_complete_clear_coalesces_to_the_clear() {
    let (bus, rx) = diagnostics_channel();
    let root = temp_root("clear");
    let _client = start_on(&bus, &root, &["--clear"]);
    let deadline = Instant::now() + Duration::from_secs(3);
    let mut last = None;
    while Instant::now() < deadline {
        if let Some(pd) = recv_timeout(&rx, Duration::from_millis(100)) {
            let clear = pd.diagnostics.is_empty();
            last = Some(pd);
            if clear {
                break;
            }
        }
    }
    let last = last.expect("publication delivered");
    assert!(last.diagnostics.is_empty() && last.complete, "{last:?}");
    assert!(rx.try_recv().is_err(), "nothing stale follows the clear");
}

#[test]
fn fake_server_close_reopen_retires_only_the_old_generation() {
    let (bus, rx) = diagnostics_channel();
    let root = temp_root("reopen");
    let first = start_on(&bus, &root, &[]);
    let old = first.generation();
    let second = start_on(&bus, &root, &[]);
    assert!(second.generation() > old);
    assert!(!bus.is_current(&root, "rust", old));
    drop(first); // late close of the old generation
    assert!(bus.is_current(&root, "rust", second.generation()));
    let mut generations = Vec::new();
    while let Some(pd) = recv_timeout(&rx, Duration::from_millis(300)) {
        generations.push(pd.generation);
    }
    assert!(!generations.is_empty());
    assert!(
        generations.iter().all(|g| *g == second.generation()),
        "{generations:?}"
    );
}
