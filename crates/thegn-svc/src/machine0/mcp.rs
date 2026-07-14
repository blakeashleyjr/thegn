//! A minimal **MCP-over-HTTP client** — thegn's first (the rest of the codebase
//! only *serves* MCP or passes declared servers to the agent; this connects
//! *out* to a remote MCP server and calls its tools). Purpose-built for
//! [machine0](https://docs.machine0.io)'s remote MCP endpoint
//! (`https://app.machine0.io/mcp`, auth via an `x-api-key` header), but the
//! transport is generic JSON-RPC 2.0 `tools/call`.
//!
//! The endpoint is a **Streamable-HTTP** MCP server: a `POST` may answer with a
//! single `application/json` body *or* a one-shot `text/event-stream` (SSE)
//! frame carrying the same JSON-RPC response. machine0's server is documented as
//! *stateless* JSON-over-HTTP, so the `initialize` handshake is best-effort: we
//! run it once (capturing any `Mcp-Session-Id`) but never let its absence block a
//! `tools/call`.
//!
//! Request/response shaping lives in **pure** functions (`tools_call_body`,
//! `sse_last_data`, `unwrap_tool_result`) so the wire format is unit-tested
//! without a live endpoint; the async methods are thin reqwest wrappers.

use std::sync::atomic::{AtomicU64, Ordering};

use anyhow::{Context, Result, anyhow};
use serde_json::{Value, json};
use thegn_core::mcp::protocol::JsonRpcResponse;

use crate::provider::{CONTROL_TIMEOUT, transient_status};

/// Whether a reqwest transport error is worth retrying. Broader than the shared
/// `provider::transient_err`: it also covers a generic "error sending request"
/// (`is_request`) — the failure mode when a **pooled keep-alive socket** to the
/// MCP endpoint's edge (Cloudflare/Railway) is reset between calls. We pair this
/// with a no-keep-alive client (below) so such a send is safe to re-dial.
fn retryable_err(e: &reqwest::Error) -> bool {
    e.is_timeout() || e.is_connect() || e.is_request()
}

/// A dedicated HTTP client for the MCP endpoint that **disables keep-alive
/// pooling** (`pool_max_idle_per_host(0)`), so every call dials a fresh
/// connection instead of risking a reused-but-reset socket — the observed
/// "error sending request" mid-poll. Bounds connection setup like the shared
/// provider client.
fn mcp_http_client() -> reqwest::Client {
    reqwest::Client::builder()
        .connect_timeout(std::time::Duration::from_secs(15))
        .pool_max_idle_per_host(0)
        .build()
        .unwrap_or_else(|_| reqwest::Client::new())
}

/// The protocol version we advertise in `initialize` (and echo on later calls
/// via the `MCP-Protocol-Version` header). A recent spec revision; the server
/// negotiates down if it wants an older one.
const PROTOCOL_VERSION: &str = "2025-06-18";

/// machine0's default remote MCP endpoint (empty config ⇒ this).
pub const DEFAULT_ENDPOINT: &str = "https://app.machine0.io/mcp";

/// Control-plane retry budget for the idempotent JSON-RPC calls (mirrors the
/// provider create/list policy): a transient 5xx/429/408 or a request timeout
/// can clear on a retry; other 4xx are terminal.
const ATTEMPTS: u32 = 3;
const BACKOFF: std::time::Duration = std::time::Duration::from_millis(500);

/// A JSON-RPC MCP client over HTTP. Owns its reqwest client; cheap to clone the
/// endpoint/key but the `session`/`id` state is per-instance.
pub struct Mcp0Client {
    http: reqwest::Client,
    endpoint: String,
    api_key: String,
    /// Streamable-HTTP session id captured from `initialize` (if the server is
    /// stateful). `None` for a stateless server — tool calls still work.
    session: tokio::sync::Mutex<Option<String>>,
    /// Whether the one-shot `initialize` handshake has been attempted.
    initialized: tokio::sync::Mutex<bool>,
    /// Monotonic JSON-RPC request id.
    next_id: AtomicU64,
}

impl Mcp0Client {
    /// `endpoint` empty ⇒ [`DEFAULT_ENDPOINT`]; `api_key` is the resolved
    /// `x-api-key` (never logged / never on an argv).
    pub fn new(endpoint: &str, api_key: &str) -> Self {
        let ep = endpoint.trim().trim_end_matches('/');
        Mcp0Client {
            http: mcp_http_client(),
            endpoint: if ep.is_empty() {
                DEFAULT_ENDPOINT.to_string()
            } else {
                ep.to_string()
            },
            api_key: api_key.to_string(),
            session: tokio::sync::Mutex::new(None),
            initialized: tokio::sync::Mutex::new(false),
            next_id: AtomicU64::new(1),
        }
    }

    pub fn endpoint(&self) -> &str {
        &self.endpoint
    }

    fn id(&self) -> u64 {
        self.next_id.fetch_add(1, Ordering::Relaxed)
    }

    /// Best-effort, once: run the MCP `initialize` handshake so a *stateful*
    /// server hands us a session id (captured for later calls) and then send the
    /// `notifications/initialized` acknowledgement. A stateless server (machine0)
    /// may 4xx or omit the session — we swallow that and proceed; the point is to
    /// never *block* a tool call on the handshake.
    async fn ensure_initialized(&self) {
        let mut done = self.initialized.lock().await;
        if *done {
            return;
        }
        *done = true; // attempt at most once regardless of outcome
        let body = json!({
            "jsonrpc": "2.0",
            "id": self.id(),
            "method": "initialize",
            "params": {
                "protocolVersion": PROTOCOL_VERSION,
                "capabilities": {},
                "clientInfo": { "name": "thegn", "version": env!("CARGO_PKG_VERSION") },
            },
        });
        let resp = self
            .http
            .post(&self.endpoint)
            .header("x-api-key", &self.api_key)
            .header("MCP-Protocol-Version", PROTOCOL_VERSION)
            .header(reqwest::header::ACCEPT, "application/json, text/event-stream")
            .timeout(CONTROL_TIMEOUT)
            .json(&body)
            .send()
            .await;
        if let Ok(r) = resp {
            if let Some(sid) = r
                .headers()
                .get("mcp-session-id")
                .and_then(|v| v.to_str().ok())
                .map(str::to_string)
            {
                *self.session.lock().await = Some(sid);
            }
            // Acknowledge — again best-effort (a stateless server ignores it).
            let sid = self.session.lock().await.clone();
            let mut req = self
                .http
                .post(&self.endpoint)
                .header("x-api-key", &self.api_key)
                .header("MCP-Protocol-Version", PROTOCOL_VERSION)
                .header(reqwest::header::ACCEPT, "application/json, text/event-stream")
                .timeout(CONTROL_TIMEOUT)
                .json(&json!({ "jsonrpc": "2.0", "method": "notifications/initialized" }));
            if let Some(s) = sid {
                req = req.header("Mcp-Session-Id", s);
            }
            let _ = req.send().await;
        }
    }

    /// Call an MCP tool by name and return its **unwrapped** result value
    /// (`structuredContent` when present, else the parsed text content). Errors
    /// on a JSON-RPC error, an `isError` tool result, or a non-success HTTP
    /// status after the retry budget.
    pub async fn call_tool(&self, name: &str, arguments: Value) -> Result<Value> {
        self.ensure_initialized().await;
        let session = self.session.lock().await.clone();
        let mut last = String::new();
        for attempt in 0..ATTEMPTS {
            let body = tools_call_body(self.id(), name, arguments.clone());
            let mut req = self
                .http
                .post(&self.endpoint)
                .header("x-api-key", &self.api_key)
                .header("MCP-Protocol-Version", PROTOCOL_VERSION)
                .header(reqwest::header::ACCEPT, "application/json, text/event-stream")
                .timeout(CONTROL_TIMEOUT)
                .json(&body);
            if let Some(s) = &session {
                req = req.header("Mcp-Session-Id", s);
            }
            match req.send().await {
                Ok(resp) => {
                    let status = resp.status();
                    let ctype = resp
                        .headers()
                        .get(reqwest::header::CONTENT_TYPE)
                        .and_then(|v| v.to_str().ok())
                        .unwrap_or("")
                        .to_string();
                    let text = resp.text().await.unwrap_or_default();
                    if !status.is_success() {
                        last = format!("machine0 mcp {name}: HTTP {status}: {text}");
                        if transient_status(status) && attempt + 1 < ATTEMPTS {
                            tokio::time::sleep(BACKOFF).await;
                            continue;
                        }
                        return Err(anyhow!(last));
                    }
                    let rpc = parse_rpc_body(&ctype, &text)
                        .with_context(|| format!("machine0 mcp {name}: decode response"))?;
                    if let Some(err) = rpc.error {
                        return Err(anyhow!(
                            "machine0 mcp {name}: rpc error {}: {}",
                            err.code,
                            err.message
                        ));
                    }
                    let result = rpc
                        .result
                        .ok_or_else(|| anyhow!("machine0 mcp {name}: response had no result"))?;
                    return unwrap_tool_result(result)
                        .with_context(|| format!("machine0 mcp {name}: tool error"));
                }
                Err(e) => {
                    last = format!("machine0 mcp {name}: {e}");
                    if retryable_err(&e) && attempt + 1 < ATTEMPTS {
                        tokio::time::sleep(BACKOFF).await;
                        continue;
                    }
                    return Err(anyhow!(last));
                }
            }
        }
        Err(anyhow!(last))
    }
}

// --- pure request/response shaping (unit-tested) ---------------------------

/// The JSON-RPC `tools/call` envelope for `name` + `arguments`.
pub fn tools_call_body(id: u64, name: &str, arguments: Value) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "method": "tools/call",
        "params": { "name": name, "arguments": arguments },
    })
}

/// Parse a JSON-RPC response body that is either a plain `application/json`
/// object or a `text/event-stream` (SSE) frame carrying it.
pub fn parse_rpc_body(content_type: &str, body: &str) -> Result<JsonRpcResponse> {
    let payload = if content_type.contains("text/event-stream") {
        sse_last_data(body).ok_or_else(|| anyhow!("empty SSE stream"))?
    } else {
        body.trim().to_string()
    };
    serde_json::from_str::<JsonRpcResponse>(&payload).context("json-rpc decode")
}

/// Extract the final event's concatenated `data:` payload from an SSE stream.
/// Events are separated by blank lines; `data:` lines within an event are joined
/// with newlines (per the SSE spec). Non-`data:` fields (`event:`, `id:`, …) are
/// ignored. Pure.
pub fn sse_last_data(body: &str) -> Option<String> {
    let mut events: Vec<String> = Vec::new();
    let mut cur = String::new();
    let mut have = false;
    for line in body.lines() {
        if line.is_empty() {
            if have {
                events.push(std::mem::take(&mut cur));
                have = false;
            }
        } else if let Some(rest) = line.strip_prefix("data:") {
            let rest = rest.strip_prefix(' ').unwrap_or(rest);
            if have {
                cur.push('\n');
            }
            cur.push_str(rest);
            have = true;
        }
    }
    if have {
        events.push(cur);
    }
    events.pop()
}

/// Unwrap an MCP `tools/call` result into the useful value:
/// - `structuredContent` when the tool returns it;
/// - else the first text content block, parsed as JSON if it is JSON, otherwise
///   returned as a JSON string;
/// - an `isError: true` result (or an error text block) becomes an `Err`.
///
/// Pure (unit-tested).
pub fn unwrap_tool_result(result: Value) -> Result<Value> {
    let text = first_text_content(&result);
    if result.get("isError").and_then(Value::as_bool) == Some(true) {
        return Err(anyhow!(text.unwrap_or_else(|| result.to_string())));
    }
    if let Some(sc) = result.get("structuredContent")
        && !sc.is_null()
    {
        return Ok(sc.clone());
    }
    match text {
        Some(t) => Ok(serde_json::from_str::<Value>(&t).unwrap_or(Value::String(t))),
        // No content at all — hand back the raw result so callers can inspect it.
        None => Ok(result),
    }
}

/// The first `content[].text` block of a tool result, if any. Pure.
fn first_text_content(result: &Value) -> Option<String> {
    result
        .get("content")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .find_map(|c| {
            (c.get("type").and_then(Value::as_str) == Some("text"))
                .then(|| c.get("text").and_then(Value::as_str))
                .flatten()
                .map(str::to_string)
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tools_call_body_shape() {
        let b = tools_call_body(7, "vm_list", json!({}));
        assert_eq!(b["jsonrpc"], "2.0");
        assert_eq!(b["id"], 7);
        assert_eq!(b["method"], "tools/call");
        assert_eq!(b["params"]["name"], "vm_list");
        assert_eq!(b["params"]["arguments"], json!({}));
    }

    #[test]
    fn sse_last_data_takes_final_event() {
        let stream = "event: message\ndata: {\"a\":1}\n\nevent: message\ndata: {\"b\":2}\n\n";
        assert_eq!(sse_last_data(stream).as_deref(), Some("{\"b\":2}"));
        // consecutive data: lines within one event are newline-joined (SSE spec)
        let multi = "data: a\ndata: b\n\n";
        assert_eq!(sse_last_data(multi).as_deref(), Some("a\nb"));
        assert_eq!(sse_last_data(""), None);
    }

    #[test]
    fn parse_rpc_body_json_and_sse() {
        let json_body = r#"{"jsonrpc":"2.0","id":1,"result":{"ok":true}}"#;
        let r = parse_rpc_body("application/json", json_body).unwrap();
        assert_eq!(r.result.unwrap()["ok"], true);
        let sse = "data: {\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{\"ok\":true}}\n\n";
        let r = parse_rpc_body("text/event-stream; charset=utf-8", sse).unwrap();
        assert_eq!(r.result.unwrap()["ok"], true);
    }

    #[test]
    fn unwrap_prefers_structured_content() {
        let result = json!({
            "content": [{"type":"text","text":"ignored"}],
            "structuredContent": {"id":"abc"},
        });
        assert_eq!(unwrap_tool_result(result).unwrap(), json!({"id":"abc"}));
    }

    #[test]
    fn unwrap_parses_json_text_content() {
        let result = json!({ "content": [{"type":"text","text":"{\"id\":\"xyz\"}"}] });
        assert_eq!(unwrap_tool_result(result).unwrap(), json!({"id":"xyz"}));
        // plain (non-JSON) text comes back as a JSON string
        let result = json!({ "content": [{"type":"text","text":"hello"}] });
        assert_eq!(unwrap_tool_result(result).unwrap(), json!("hello"));
    }

    #[test]
    fn unwrap_is_error_becomes_err() {
        let result = json!({ "isError": true, "content": [{"type":"text","text":"boom"}] });
        let e = unwrap_tool_result(result).unwrap_err();
        assert!(e.to_string().contains("boom"));
    }
}
