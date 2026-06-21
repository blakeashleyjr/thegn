//! End-to-end exercise of `LspClient` against the hermetic `fake_lsp` server
//! (selected via `CARGO_BIN_EXE_fake_lsp`) — no real language server needed.

use std::sync::mpsc;
use std::time::Duration;

use superzej_core::semantic::Lang;
use superzej_svc::lsp::{LspClient, Position, ServerSpec, SymbolKind, framing::FrameDecoder};

fn start_fake() -> (
    LspClient,
    mpsc::Receiver<superzej_svc::lsp::PublishedDiagnostics>,
) {
    let spec = ServerSpec {
        lang: Lang::Rust,
        command: env!("CARGO_BIN_EXE_fake_lsp").to_string(),
        args: vec![],
    };
    let (diag_tx, diag_rx) = mpsc::channel();
    let root = std::env::temp_dir();
    let client = LspClient::start(&spec, &root, diag_tx).expect("spawn fake server");
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
fn framing_smoke_for_test_helpers() {
    // Guards that the shared codec the fake server uses is sane.
    let mut d = FrameDecoder::new();
    d.push(&superzej_svc::lsp::framing::encode("{\"x\":1}"));
    assert_eq!(d.next_message().as_deref(), Some("{\"x\":1}"));
}
