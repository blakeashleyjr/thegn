//! A minimal fake language server for tests: speaks just enough LSP base
//! protocol over stdio to exercise `LspClient` (and the host's live wiring)
//! end-to-end with no real server installed. It answers `initialize` (and
//! immediately pushes one `publishDiagnostics`), and returns canned results for
//! the request methods the client wraps.
//!
//! Deliberately self-contained — it depends only on `serde_json`, not the
//! `thegn-svc` lib, so the binary stays tiny and spawns fast (a heavyweight
//! debug binary made the e2e cold-start flaky). The framing is re-implemented
//! inline; `thegn_svc::lsp::framing` has the canonical, unit-tested copy.

use std::io::{Read, Write, stdin, stdout};

use serde_json::{Value, json};

fn frame(body: &str) -> Vec<u8> {
    let mut out = format!("Content-Length: {}\r\n\r\n", body.len()).into_bytes();
    out.extend_from_slice(body.as_bytes());
    out
}

fn send(out: &mut impl Write, body: &Value) {
    let _ = out.write_all(&frame(&body.to_string()));
    let _ = out.flush();
}

fn reply(out: &mut impl Write, id: &Value, result: Value) {
    send(
        out,
        &json!({ "jsonrpc": "2.0", "id": id, "result": result }),
    );
}

/// Pull the next complete message body out of `buf`, draining it.
fn next_message(buf: &mut Vec<u8>) -> Option<String> {
    let sep = buf.windows(4).position(|w| w == b"\r\n\r\n")?;
    let header = std::str::from_utf8(&buf[..sep]).ok()?;
    let len: usize = header.split("\r\n").find_map(|line| {
        let (k, v) = line.split_once(':')?;
        k.trim()
            .eq_ignore_ascii_case("content-length")
            .then(|| v.trim().parse().ok())
            .flatten()
    })?;
    let start = sep + 4;
    if buf.len() < start + len {
        return None;
    }
    let body = String::from_utf8_lossy(&buf[start..start + len]).into_owned();
    buf.drain(..start + len);
    Some(body)
}

fn main() {
    let mut buf: Vec<u8> = Vec::new();
    let mut input = stdin().lock();
    let mut out = stdout().lock();
    let mut chunk = [0u8; 4096];

    loop {
        let n = match input.read(&mut chunk) {
            Ok(0) | Err(_) => break,
            Ok(n) => n,
        };
        buf.extend_from_slice(&chunk[..n]);

        while let Some(body) = next_message(&mut buf) {
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
                    // A name no tree-sitter parse of the fixture would produce, so
                    // a test seeing it knows the result came from the server.
                    json!([{
                        "name": "lspProbe", "kind": 12,
                        "range": { "start": { "line": 0, "character": 0 }, "end": { "line": 2, "character": 0 } },
                        "selectionRange": { "start": { "line": 0, "character": 3 }, "end": { "line": 0, "character": 8 } }
                    }]),
                ),
                "workspace/symbol" => reply(
                    &mut out,
                    &id.unwrap_or(Value::Null),
                    json!([{
                        "name": "lspProbe", "kind": 12,
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
