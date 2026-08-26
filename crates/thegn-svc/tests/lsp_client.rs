//! End-to-end exercise of `LspClient` against the hermetic `fake_lsp` server
//! (selected via `CARGO_BIN_EXE_fake_lsp`) — no real language server needed.

use std::sync::mpsc;
use std::time::Duration;

use thegn_svc::lsp::{
    LspClient, LspError, Position, ServerSpec, SymbolKind, framing::FrameDecoder,
};

fn spec_with(args: Vec<String>) -> ServerSpec {
    ServerSpec {
        key: "rust".to_string(),
        language_id: "rust".to_string(),
        command: env!("CARGO_BIN_EXE_fake_lsp").to_string(),
        args,
    }
}

fn start_fake() -> (
    LspClient,
    mpsc::Receiver<thegn_svc::lsp::PublishedDiagnostics>,
) {
    start_fake_args(vec![])
}

fn start_fake_args(
    args: Vec<String>,
) -> (
    LspClient,
    mpsc::Receiver<thegn_svc::lsp::PublishedDiagnostics>,
) {
    let (diag_tx, diag_rx) = mpsc::channel();
    let root = std::env::temp_dir();
    let client = LspClient::start(&spec_with(args), &root, diag_tx).expect("spawn fake server");
    (client, diag_rx)
}

#[test]
fn initialize_handshake_and_pushed_diagnostics() {
    let (client, diag_rx) = start_fake();
    client
        .initialize(&std::env::temp_dir())
        .expect("initialize");

    let pd = diag_rx
        .recv_timeout(Duration::from_secs(3))
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
    d.push(&thegn_svc::lsp::framing::encode("{\"x\":1}"));
    assert_eq!(d.next_message().as_deref(), Some("{\"x\":1}"));
}
