//! A minimal fake language server for tests: speaks just enough LSP base
//! protocol over stdio to exercise `LspClient` end-to-end with no real server
//! installed. It answers `initialize` (and immediately pushes one
//! `publishDiagnostics`), and returns canned results for the request methods
//! the client wraps. Selected by the integration test via `CARGO_BIN_EXE_fake_lsp`.

use std::io::{Read, Write, stdin, stdout};

use serde_json::{Value, json};
use superzej_svc::lsp::framing::{FrameDecoder, encode};

fn send(out: &mut impl Write, body: &Value) {
    let _ = out.write_all(&encode(&body.to_string()));
    let _ = out.flush();
}

fn reply(out: &mut impl Write, id: &Value, result: Value) {
    send(
        out,
        &json!({ "jsonrpc": "2.0", "id": id, "result": result }),
    );
}

fn main() {
    let mut decoder = FrameDecoder::new();
    let mut input = stdin().lock();
    let mut out = stdout().lock();
    let mut chunk = [0u8; 4096];

    loop {
        let n = match input.read(&mut chunk) {
            Ok(0) | Err(_) => break,
            Ok(n) => n,
        };
        decoder.push(&chunk[..n]);

        while let Some(body) = decoder.next_message() {
            let Ok(msg) = serde_json::from_str::<Value>(&body) else {
                continue;
            };
            let id = msg.get("id").cloned();
            let method = msg.get("method").and_then(Value::as_str).unwrap_or("");

            match method {
                "initialize" => {
                    reply(
                        &mut out,
                        &id.unwrap_or(Value::Null),
                        json!({ "capabilities": {} }),
                    );
                    // Push a diagnostic so the client's notification path is exercised.
                    send(
                        &mut out,
                        &json!({
                            "jsonrpc": "2.0",
                            "method": "textDocument/publishDiagnostics",
                            "params": {
                                "uri": "file:///proj/src/lib.rs",
                                "diagnostics": [{
                                    "range": { "start": { "line": 2, "character": 4 },
                                               "end": { "line": 2, "character": 9 } },
                                    "severity": 1,
                                    "message": "fake error",
                                    "source": "fake-lsp",
                                    "code": "F001"
                                }]
                            }
                        }),
                    );
                }
                "textDocument/documentSymbol" => reply(
                    &mut out,
                    &id.unwrap_or(Value::Null),
                    json!([{
                        "name": "greet", "kind": 12,
                        "range": { "start": { "line": 0, "character": 0 }, "end": { "line": 2, "character": 0 } },
                        "selectionRange": { "start": { "line": 0, "character": 3 }, "end": { "line": 0, "character": 8 } }
                    }]),
                ),
                "workspace/symbol" => reply(
                    &mut out,
                    &id.unwrap_or(Value::Null),
                    json!([{
                        "name": "greet", "kind": 12,
                        "location": { "uri": "file:///proj/src/lib.rs",
                            "range": { "start": { "line": 0, "character": 3 }, "end": { "line": 0, "character": 8 } } }
                    }]),
                ),
                "textDocument/definition" => reply(
                    &mut out,
                    &id.unwrap_or(Value::Null),
                    json!({ "uri": "file:///proj/src/lib.rs",
                        "range": { "start": { "line": 0, "character": 3 }, "end": { "line": 0, "character": 8 } } }),
                ),
                "textDocument/references" => reply(
                    &mut out,
                    &id.unwrap_or(Value::Null),
                    json!([
                        { "uri": "file:///proj/src/lib.rs",
                          "range": { "start": { "line": 5, "character": 4 }, "end": { "line": 5, "character": 9 } } },
                        { "uri": "file:///proj/src/main.rs",
                          "range": { "start": { "line": 9, "character": 8 }, "end": { "line": 9, "character": 13 } } }
                    ]),
                ),
                "textDocument/hover" => reply(
                    &mut out,
                    &id.unwrap_or(Value::Null),
                    json!({ "contents": { "kind": "markdown", "value": "fn greet() -> u8" } }),
                ),
                "shutdown" => reply(&mut out, &id.unwrap_or(Value::Null), Value::Null),
                "exit" => break,
                _ => {
                    if let Some(id) = id {
                        reply(&mut out, &id, Value::Null);
                    }
                }
            }
        }
    }
}
