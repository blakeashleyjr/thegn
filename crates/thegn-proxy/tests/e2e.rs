//! End-to-end tests: a real proxy bound to an ephemeral port, routing to mock
//! upstream HTTP servers. Exercises a served response, ordered failover past an
//! exhausted backend, and budget-cap refusal.

use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;

use axum::body::Body;
use axum::response::IntoResponse;
use axum::{
    Router, extract::State, http::HeaderMap, http::StatusCode, http::header, routing::post,
};
use futures::StreamExt as _;
use serde_json::{Value, json};
use thegn_core::db::Db;
use thegn_core::proxy::compress::Level;
use thegn_core::proxy::ratelimit::RatePolicy;
use thegn_core::proxy::transform::CompressPolicy;
use thegn_core::store::ProxyStore;
use thegn_proxy::model::{Backend, ProxyConfig, Route};
use thegn_proxy::server;
use thegn_proxy::shared::{SharedDb, now_ms};
use thegn_proxy::state::AppState;

/// Spawns a mock upstream returning a fixed status + JSON body for
/// `/chat/completions`. Returns its base URL (`http://127.0.0.1:PORT`).
async fn spawn_mock(status: u16, body: Value) -> String {
    async fn handler(State(s): State<(u16, Value)>) -> (StatusCode, String) {
        (StatusCode::from_u16(s.0).unwrap(), s.1.to_string())
    }
    let app = Router::new()
        .route("/chat/completions", post(handler))
        .with_state((status, body));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    format!("http://{addr}")
}

/// Spawns a mock upstream that streams a fixed OpenAI SSE body for
/// `/chat/completions`. Returns its base URL. Rejects requests that don't ask
/// for `stream: true` — a live-relay dispatch that forgot the flag must fail
/// here (real upstreams would return a JSON completion, not SSE).
async fn spawn_mock_sse(body: &'static str) -> String {
    async fn handler(
        State(body): State<&'static str>,
        req: axum::body::Bytes,
    ) -> axum::response::Response {
        let streaming = serde_json::from_slice::<Value>(&req)
            .ok()
            .and_then(|v| v.get("stream").and_then(Value::as_bool))
            .unwrap_or(false);
        if !streaming {
            return (
                StatusCode::BAD_REQUEST,
                r#"{"error":{"message":"expected stream:true"}}"#,
            )
                .into_response();
        }
        (
            [(axum::http::header::CONTENT_TYPE, "text/event-stream")],
            body,
        )
            .into_response()
    }
    let app = Router::new()
        .route("/chat/completions", post(handler))
        .with_state(body);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    format!("http://{addr}")
}

const OPENAI_SSE: &str = "data: {\"choices\":[{\"delta\":{\"role\":\"assistant\"}}]}\n\n\
data: {\"choices\":[{\"delta\":{\"content\":\"Hello\"}}]}\n\n\
data: {\"choices\":[{\"delta\":{\"content\":\" world\"}}]}\n\n\
data: {\"choices\":[{\"finish_reason\":\"stop\"}],\"usage\":{\"prompt_tokens\":10,\"completion_tokens\":3,\"total_tokens\":13}}\n\n\
data: [DONE]\n\n";

fn backend(name: &str, base_url: &str) -> Backend {
    Backend {
        name: name.to_string(),
        key_id: String::new(),
        base_url: base_url.to_string(),
        model: "test-model".to_string(),
        api_key: "k".to_string(),
        anthropic: false,
        context_limit: 0,
        defaults: serde_json::Map::new(),
        rate: RatePolicy {
            rpm: 600.0,
            burst: 100.0,
        },
        inflight_cap: 0,
        pool: None,
    }
}

/// Spawns the proxy against `routes` + `db` (no compression), returns its base URL.
async fn spawn_proxy(routes: Vec<Route>, db: SharedDb) -> String {
    spawn_proxy_cfg(routes, db, CompressPolicy::off()).await
}

/// Spawns the proxy with an explicit token-reduction policy.
async fn spawn_proxy_cfg(routes: Vec<Route>, db: SharedDb, compression: CompressPolicy) -> String {
    let config = ProxyConfig {
        listen: "127.0.0.1:0".parse::<SocketAddr>().unwrap(),
        routes,
        relay: thegn_proxy::relay::RelayConfig::default(),
        compression,
        aliases: Default::default(),
        last_resort: false,
    };
    // Bind first so we know the port, then hand the listener to axum::serve.
    let listener = tokio::net::TcpListener::bind(config.listen).await.unwrap();
    let addr = listener.local_addr().unwrap();
    let state = AppState::new(config, db, now_ms());
    let app = server::app(state);
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    format!("http://{addr}")
}

fn completion_body() -> Value {
    json!({
        "id": "cmpl-1",
        "object": "chat.completion",
        "choices": [{"index": 0, "message": {"role": "assistant", "content": "hello from upstream"}, "finish_reason": "stop"}],
        "usage": {"prompt_tokens": 10, "completion_tokens": 5, "total_tokens": 15}
    })
}

fn chat_request() -> Value {
    json!({"model": "model-proxy/standard", "messages": [{"role": "user", "content": "hi"}]})
}

#[tokio::test]
async fn serves_a_completion() {
    let up = spawn_mock(200, completion_body()).await;
    let db: SharedDb = Arc::new(Mutex::new(Db::open_memory().unwrap()));
    let routes = vec![Route {
        name: "standard".into(),
        strategy: thegn_core::config::RoutingStrategy::Sequential,
        order_pool: None,
        priority: vec![backend("primary", &up)],
    }];
    let proxy = spawn_proxy(routes, db.clone()).await;

    let resp = reqwest::Client::new()
        .post(format!("{proxy}/v1/chat/completions"))
        .json(&chat_request())
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let v: Value = resp.json().await.unwrap();
    assert_eq!(v["choices"][0]["message"]["content"], "hello from upstream");

    // An audit row should have been written attributing spend to global.
    let g = db.lock().unwrap();
    let budget = g.proxy_budget("global").unwrap().unwrap();
    assert_eq!(budget.spent_tokens, 15);
}

#[tokio::test]
async fn fails_over_past_exhausted_backend() {
    let bad = spawn_mock(429, json!({"error": {"message": "rate limited"}})).await;
    let good = spawn_mock(200, completion_body()).await;
    let db: SharedDb = Arc::new(Mutex::new(Db::open_memory().unwrap()));
    let routes = vec![Route {
        name: "standard".into(),
        strategy: thegn_core::config::RoutingStrategy::Sequential,
        order_pool: None,
        priority: vec![backend("primary", &bad), backend("secondary", &good)],
    }];
    let proxy = spawn_proxy(routes, db).await;

    let resp = reqwest::Client::new()
        .post(format!("{proxy}/v1/chat/completions"))
        .json(&chat_request())
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let v: Value = resp.json().await.unwrap();
    assert_eq!(v["choices"][0]["message"]["content"], "hello from upstream");

    // /resolved should report the secondary as the server for the route.
    let resolved: Value = reqwest::get(format!("{proxy}/resolved"))
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(resolved["standard"], "secondary");
}

#[tokio::test]
async fn refuses_when_budget_kill_switch_set() {
    let up = spawn_mock(200, completion_body()).await;
    let db: SharedDb = Arc::new(Mutex::new(Db::open_memory().unwrap()));
    db.lock()
        .unwrap()
        .set_proxy_kill_switch("global", true)
        .unwrap();
    let routes = vec![Route {
        name: "standard".into(),
        strategy: thegn_core::config::RoutingStrategy::Sequential,
        order_pool: None,
        priority: vec![backend("primary", &up)],
    }];
    let proxy = spawn_proxy(routes, db).await;

    let resp = reqwest::Client::new()
        .post(format!("{proxy}/v1/chat/completions"))
        .json(&chat_request())
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 402);
    let v: Value = resp.json().await.unwrap();
    assert!(
        v["error"]["message"]
            .as_str()
            .unwrap()
            .contains("kill-switch")
    );
}

#[tokio::test]
async fn all_backends_failed_returns_503() {
    let bad = spawn_mock(500, json!({"error": {"message": "boom"}})).await;
    let db: SharedDb = Arc::new(Mutex::new(Db::open_memory().unwrap()));
    let routes = vec![Route {
        name: "standard".into(),
        strategy: thegn_core::config::RoutingStrategy::Sequential,
        order_pool: None,
        priority: vec![backend("only", &bad)],
    }];
    let proxy = spawn_proxy(routes, db).await;

    let resp = reqwest::Client::new()
        .post(format!("{proxy}/v1/chat/completions"))
        .json(&chat_request())
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 503);
}

/// Polls the global budget's spent tokens until it reaches `want` or times out.
async fn wait_for_global_tokens(db: &SharedDb, want: i64) -> i64 {
    for _ in 0..50 {
        let got = db
            .lock()
            .unwrap()
            .proxy_budget("global")
            .unwrap()
            .map(|b| b.spent_tokens)
            .unwrap_or(0);
        if got >= want {
            return got;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    0
}

#[tokio::test]
async fn openai_surface_streams_and_reconciles_usage() {
    let up = spawn_mock_sse(OPENAI_SSE).await;
    let db: SharedDb = Arc::new(Mutex::new(Db::open_memory().unwrap()));
    let routes = vec![Route {
        name: "standard".into(),
        strategy: thegn_core::config::RoutingStrategy::Sequential,
        order_pool: None,
        priority: vec![backend("primary", &up)],
    }];
    let proxy = spawn_proxy(routes, db.clone()).await;

    let body = serde_json::json!({"model": "model-proxy/standard", "stream": true, "messages": [{"role": "user", "content": "hi"}]});
    let resp = reqwest::Client::new()
        .post(format!("{proxy}/v1/chat/completions"))
        .json(&body)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let text = resp.text().await.unwrap();
    // Passthrough: the upstream SSE content reaches the client.
    assert!(text.contains("\"content\":\"Hello\""));
    assert!(text.contains("[DONE]"));
    // Usage was reconciled from the trailing usage chunk (prompt 10 + completion 3).
    assert_eq!(wait_for_global_tokens(&db, 13).await, 13);
}

#[tokio::test]
async fn anthropic_surface_translates_stream_to_events() {
    let up = spawn_mock_sse(OPENAI_SSE).await;
    let db: SharedDb = Arc::new(Mutex::new(Db::open_memory().unwrap()));
    let routes = vec![Route {
        name: "standard".into(),
        strategy: thegn_core::config::RoutingStrategy::Sequential,
        order_pool: None,
        priority: vec![backend("primary", &up)],
    }];
    let proxy = spawn_proxy(routes, db).await;

    let body = serde_json::json!({"model": "model-proxy/standard", "stream": true, "max_tokens": 100, "messages": [{"role": "user", "content": "hi"}]});
    let resp = reqwest::Client::new()
        .post(format!("{proxy}/v1/messages"))
        .json(&body)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let text = resp.text().await.unwrap();
    // Well-formed Anthropic event sequence translated from the OpenAI stream.
    assert!(text.contains("event: message_start"));
    assert!(text.contains("event: content_block_start"));
    assert!(text.contains("\"text_delta\""));
    assert!(text.contains("Hello"));
    assert!(text.contains("event: message_delta"));
    assert!(text.contains("event: message_stop"));
}

#[tokio::test]
async fn empty_stream_peek_falls_through() {
    // The first backend streams only [DONE] (no usable output): the peek must
    // reject it and fall through to the second, whose content reaches the client.
    let empty = spawn_mock_sse("data: [DONE]\n\n").await;
    let good = spawn_mock_sse(OPENAI_SSE).await;
    let db: SharedDb = Arc::new(Mutex::new(Db::open_memory().unwrap()));
    let routes = vec![Route {
        name: "standard".into(),
        strategy: thegn_core::config::RoutingStrategy::Sequential,
        order_pool: None,
        priority: vec![backend("empty", &empty), backend("secondary", &good)],
    }];
    let proxy = spawn_proxy(routes, db).await;

    let body = serde_json::json!({"model": "model-proxy/standard", "stream": true, "messages": [{"role": "user", "content": "hi"}]});
    let resp = reqwest::Client::new()
        .post(format!("{proxy}/v1/chat/completions"))
        .json(&body)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let text = resp.text().await.unwrap();
    assert!(
        text.contains("Hello"),
        "expected second backend's content, got: {text}"
    );

    // /resolved should report the secondary as the server.
    let resolved: Value = reqwest::get(format!("{proxy}/resolved"))
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(resolved["standard"], "secondary");
}

/// Spawns a mock that records the request body it received and returns a normal
/// completion — so a test can assert what the proxy forwarded upstream.
async fn spawn_echo_mock(captured: Arc<Mutex<Option<Value>>>) -> String {
    async fn handler(
        State(cap): State<Arc<Mutex<Option<Value>>>>,
        body: axum::body::Bytes,
    ) -> (StatusCode, String) {
        if let Ok(v) = serde_json::from_slice::<Value>(&body) {
            *cap.lock().unwrap() = Some(v);
        }
        (StatusCode::OK, completion_body().to_string())
    }
    let app = Router::new()
        .route("/chat/completions", post(handler))
        .with_state(captured);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    format!("http://{addr}")
}

fn tool_request() -> Value {
    // Realistically noisy command output: ANSI color, blank-line padding, and a
    // long run of identical log lines — so compression clearly pays off.
    let repeated = "[INFO] processing record id=42 status=ok\n".repeat(40);
    let noisy = format!("\u{1b}[31mERROR\u{1b}[0m\n\n\n\n\n{repeated}done");
    json!({
        "model": "model-proxy/standard",
        "messages": [
            {"role": "system", "content": "keep  me  verbatim"},
            {"role": "assistant", "tool_calls": [{"id": "c1", "function": {"name": "bash"}}]},
            {"role": "tool", "tool_call_id": "c1", "content": noisy},
        ]
    })
}

#[tokio::test]
async fn compresses_tool_output_in_flight() {
    let captured = Arc::new(Mutex::new(None));
    let up = spawn_echo_mock(captured.clone()).await;
    let db: SharedDb = Arc::new(Mutex::new(Db::open_memory().unwrap()));
    let routes = vec![Route {
        name: "standard".into(),
        strategy: thegn_core::config::RoutingStrategy::Sequential,
        order_pool: None,
        priority: vec![backend("primary", &up)],
    }];
    let policy = CompressPolicy {
        level: Level::Balanced,
        ..Default::default()
    };
    let proxy = spawn_proxy_cfg(routes, db, policy).await;

    let resp = reqwest::Client::new()
        .post(format!("{proxy}/v1/chat/completions"))
        .json(&tool_request())
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);

    let got = captured.lock().unwrap().clone().unwrap();
    // The client's `model-proxy/standard` was rewritten to the lane's model.
    assert_eq!(got["model"], "test-model");
    let tool_content = got["messages"][2]["content"].as_str().unwrap();
    assert!(
        !tool_content.contains('\u{1b}'),
        "ANSI should be stripped: {tool_content:?}"
    );
    assert!(
        tool_content.contains("identical lines omitted"),
        "repeats should fold: {tool_content:?}"
    );
    // The system prompt (cacheable prefix) is forwarded byte-identical.
    assert_eq!(got["messages"][0]["content"], "keep  me  verbatim");

    // Savings are tracked on /metrics.
    let metrics = reqwest::get(format!("{proxy}/metrics"))
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    let saved_line = metrics
        .lines()
        .find(|l| l.starts_with("model_proxy_tokens_saved_total{"))
        .unwrap_or("");
    let saved: u64 = saved_line
        .rsplit(' ')
        .next()
        .unwrap_or("0")
        .parse()
        .unwrap_or(0);
    assert!(
        saved > 0,
        "expected non-zero tokens saved, metrics:\n{metrics}"
    );
}

#[tokio::test]
async fn compression_disabled_forwards_unchanged() {
    let captured = Arc::new(Mutex::new(None));
    let up = spawn_echo_mock(captured.clone()).await;
    let db: SharedDb = Arc::new(Mutex::new(Db::open_memory().unwrap()));
    let routes = vec![Route {
        name: "standard".into(),
        strategy: thegn_core::config::RoutingStrategy::Sequential,
        order_pool: None,
        priority: vec![backend("primary", &up)],
    }];
    let proxy = spawn_proxy_cfg(routes, db, CompressPolicy::off()).await;

    let req = tool_request();
    reqwest::Client::new()
        .post(format!("{proxy}/v1/chat/completions"))
        .json(&req)
        .send()
        .await
        .unwrap();

    let got = captured.lock().unwrap().clone().unwrap();
    // Tool content forwarded verbatim (ANSI + repeats intact).
    assert_eq!(got["messages"][2]["content"], req["messages"][2]["content"]);
}

/// Spawns a mock that keys off the `Authorization: Bearer` token: it records
/// every bearer it sees and returns 429 for `bad_key`, 200 otherwise.
async fn spawn_keyed_mock(bad_key: Option<&'static str>, seen: Arc<Mutex<Vec<String>>>) -> String {
    type S = (Option<&'static str>, Arc<Mutex<Vec<String>>>);
    async fn handler(State((bad, seen)): State<S>, headers: HeaderMap) -> (StatusCode, String) {
        let bearer = headers
            .get(axum::http::header::AUTHORIZATION)
            .and_then(|v| v.to_str().ok())
            .and_then(|s| s.strip_prefix("Bearer "))
            .unwrap_or("")
            .to_string();
        seen.lock().unwrap().push(bearer.clone());
        if Some(bearer.as_str()) == bad {
            return (
                StatusCode::TOO_MANY_REQUESTS,
                json!({"error": {"message": "rate limited"}}).to_string(),
            );
        }
        (StatusCode::OK, completion_body().to_string())
    }
    let app = Router::new()
        .route("/chat/completions", post(handler))
        .with_state((bad_key, seen));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    format!("http://{addr}")
}

/// Builds a route whose single provider has two keys (k0, k1) sharing a pool,
/// pointed at `up`, via the real config parser (so lane expansion is exercised).
fn two_key_route(up: &str, strategy: &str) -> Vec<Route> {
    let doc = format!(
        r#"{{"routes":[{{"name":"standard","backends":[
            {{"name":"echo","base_url":"{up}","model":"m","api_keys":["k0","k1"],"key_strategy":"{strategy}"}}
        ]}}]}}"#
    );
    thegn_proxy::config::parse_config(&doc).unwrap().routes
}

#[tokio::test]
async fn multi_key_failover_cools_only_one_key() {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let up = spawn_keyed_mock(Some("k0"), seen.clone()).await; // k0 is rate-limited
    let db: SharedDb = Arc::new(Mutex::new(Db::open_memory().unwrap()));
    let proxy = spawn_proxy(two_key_route(&up, "failover"), db).await;

    let resp = reqwest::Client::new()
        .post(format!("{proxy}/v1/chat/completions"))
        .json(&chat_request())
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);

    // The k0 lane 429'd, the k1 lane served — failover within the provider.
    let observed = seen.lock().unwrap().clone();
    assert_eq!(observed, vec!["k0".to_string(), "k1".to_string()]);
    // /resolved attributes the serve to the #1 lane, not a wholesale provider cool.
    let resolved: Value = reqwest::get(format!("{proxy}/resolved"))
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(resolved["standard"], "echo#1");
}

#[tokio::test]
async fn multi_key_round_robin_spreads_across_keys() {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let up = spawn_keyed_mock(None, seen.clone()).await; // both keys healthy
    let db: SharedDb = Arc::new(Mutex::new(Db::open_memory().unwrap()));
    let proxy = spawn_proxy(two_key_route(&up, "roundrobin"), db).await;

    let client = reqwest::Client::new();
    for _ in 0..2 {
        let r = client
            .post(format!("{proxy}/v1/chat/completions"))
            .json(&chat_request())
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), 200);
    }
    // Round-robin advances the first-choice lane each request → both keys used.
    let observed = seen.lock().unwrap().clone();
    assert!(
        observed.contains(&"k0".to_string()),
        "observed: {observed:?}"
    );
    assert!(
        observed.contains(&"k1".to_string()),
        "observed: {observed:?}"
    );
}

/// Parses a routes doc with two backends (`a`, `b`) at `up` under `strategy`.
fn two_backend_route(
    up: &str,
    strategy: &str,
    names: (&str, &str),
    models: (&str, &str),
) -> Vec<Route> {
    let doc = format!(
        r#"{{"routes":[{{"name":"standard","strategy":"{strategy}","backends":[
            {{"name":"{}","base_url":"{up}","model":"{}","api_key":"k"}},
            {{"name":"{}","base_url":"{up}","model":"{}","api_key":"k"}}
        ]}}]}}"#,
        names.0, models.0, names.1, models.1
    );
    thegn_proxy::config::parse_config(&doc).unwrap().routes
}

async fn resolved_backend(proxy: &str) -> String {
    let v: Value = reqwest::get(format!("{proxy}/resolved"))
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    v["standard"].as_str().unwrap_or("").to_string()
}

#[tokio::test]
async fn load_balanced_spreads_across_backends() {
    let up = spawn_mock(200, completion_body()).await;
    let db: SharedDb = Arc::new(Mutex::new(Db::open_memory().unwrap()));
    let routes = two_backend_route(&up, "load_balanced", ("a", "b"), ("m", "m"));
    let proxy = spawn_proxy(routes, db).await;

    let client = reqwest::Client::new();
    let mut served = std::collections::HashSet::new();
    for _ in 0..2 {
        client
            .post(format!("{proxy}/v1/chat/completions"))
            .json(&chat_request())
            .send()
            .await
            .unwrap();
        served.insert(resolved_backend(&proxy).await);
    }
    // Round-robin rotates the first-choice backend → both serve across 2 requests.
    assert!(served.contains("a"), "served: {served:?}");
    assert!(served.contains("b"), "served: {served:?}");
}

#[tokio::test]
async fn speculative_serves_cheapest_despite_config_order() {
    let up = spawn_mock(200, completion_body()).await;
    let db: SharedDb = Arc::new(Mutex::new(Db::open_memory().unwrap()));
    // Configured paid-first; the cheap (subscription) lane must win.
    let routes = two_backend_route(
        &up,
        "speculative",
        ("openrouter", "codex"),
        ("deepseek/deepseek-v4-pro", "gpt-5.5"),
    );
    let proxy = spawn_proxy(routes, db).await;

    let resp = reqwest::Client::new()
        .post(format!("{proxy}/v1/chat/completions"))
        .json(&chat_request())
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    // codex (subscription, $0) is cheaper than the paid openrouter lane.
    assert_eq!(resolved_backend(&proxy).await, "codex");
}

// ── upstream.rs integration ─────────────────────────────────────────────────

/// Mock serving the Anthropic `/messages` surface with a fixed status + body.
async fn spawn_anthropic_mock(status: u16, body: Value) -> String {
    async fn handler(State(s): State<(u16, Value)>) -> (StatusCode, String) {
        (StatusCode::from_u16(s.0).unwrap(), s.1.to_string())
    }
    let app = Router::new()
        .route("/messages", post(handler))
        .with_state((status, body));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    format!("http://{addr}")
}

fn anthropic_backend(name: &str, base_url: &str) -> Backend {
    let mut b = backend(name, base_url);
    b.anthropic = true;
    b.model = "claude-x".into();
    b
}

#[tokio::test]
async fn upstream_anthropic_backend_translates_both_ways() {
    let up = spawn_anthropic_mock(
        200,
        json!({"id":"m1","content":[{"type":"text","text":"hi there"}],"stop_reason":"end_turn","usage":{"input_tokens":4,"output_tokens":2}}),
    )
    .await;
    let b = anthropic_backend("kimi", &up);
    let resp = thegn_proxy::upstream::call_backend(
        &reqwest::Client::new(),
        &b,
        br#"{"model":"m","messages":[{"role":"user","content":"hi"}]}"#,
    )
    .await
    .unwrap();
    assert_eq!(resp.status, 200);
    // Anthropic response translated back to OpenAI shape.
    let v: Value = serde_json::from_slice(&resp.body).unwrap();
    assert_eq!(v["choices"][0]["message"]["content"], "hi there");
    assert_eq!(v["usage"]["completion_tokens"], 2);
}

#[tokio::test]
async fn upstream_anthropic_non_2xx_passes_through() {
    let up = spawn_anthropic_mock(429, json!({"error": {"message": "rate limited"}})).await;
    let b = anthropic_backend("kimi", &up);
    let resp = thegn_proxy::upstream::call_backend(
        &reqwest::Client::new(),
        &b,
        br#"{"model":"m","messages":[]}"#,
    )
    .await
    .unwrap();
    // Upstream error returned unchanged for the router to classify.
    assert_eq!(resp.status, 429);
    assert!(String::from_utf8_lossy(&resp.body).contains("rate limited"));
}

#[tokio::test]
async fn upstream_network_error_is_err() {
    // Nothing listening on this port.
    let b = backend("dead", "http://127.0.0.1:1");
    let res = thegn_proxy::upstream::call_backend(
        &reqwest::Client::new(),
        &b,
        br#"{"model":"m","messages":[]}"#,
    )
    .await;
    assert!(res.is_err());

    let stream =
        thegn_proxy::upstream::open_openai_stream(&reqwest::Client::new(), &b, br#"{"model":"m"}"#)
            .await;
    assert!(stream.is_err());
}

// ── relay.rs timing (real Response) ─────────────────────────────────────────

/// Mock that delays `delay` before sending the (one-chunk) body — exercises the
/// relay's first-byte timeout.
async fn spawn_slow_first_byte_mock(delay: Duration) -> String {
    async fn handler(State(delay): State<Duration>) -> impl IntoResponse {
        let s = futures::stream::once(async move {
            tokio::time::sleep(delay).await;
            Ok::<_, std::io::Error>(axum::body::Bytes::from(
                "data: {\"choices\":[{\"delta\":{\"content\":\"x\"}}]}\n\n",
            ))
        });
        (
            [(header::CONTENT_TYPE, "text/event-stream")],
            Body::from_stream(s),
        )
    }
    let app = Router::new()
        .route("/chat/completions", post(handler))
        .with_state(delay);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    format!("http://{addr}")
}

/// Mock that yields one content chunk immediately then stalls forever — exercises
/// the relay's idle watchdog + heartbeat.
async fn spawn_stall_mock() -> String {
    async fn handler() -> impl IntoResponse {
        let s = futures::stream::once(async {
            Ok::<_, std::io::Error>(axum::body::Bytes::from(
                "data: {\"choices\":[{\"delta\":{\"content\":\"first\"}}]}\n\n",
            ))
        })
        .chain(futures::stream::pending());
        (
            [(header::CONTENT_TYPE, "text/event-stream")],
            Body::from_stream(s),
        )
    }
    let app = Router::new().route("/chat/completions", post(handler));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    format!("http://{addr}")
}

use thegn_proxy::relay::{self, OpenAiSink, Peek, RelayConfig};

async fn post_stream(url: &str) -> reqwest::Response {
    reqwest::Client::new()
        .post(format!("{url}/chat/completions"))
        .body("{}")
        .send()
        .await
        .unwrap()
}

#[tokio::test]
async fn relay_peek_times_out_on_slow_first_byte() {
    let up = spawn_slow_first_byte_mock(Duration::from_millis(500)).await;
    let resp = post_stream(&up).await;
    let cfg = RelayConfig {
        first_byte: Duration::from_millis(50),
        idle: Duration::from_secs(1),
        heartbeat: Duration::from_secs(1),
    };
    assert!(matches!(
        relay::peek(resp, OpenAiSink::default(), cfg).await,
        Peek::TimedOut
    ));
}

#[tokio::test]
async fn relay_peek_empty_stream() {
    let up = spawn_mock_sse("data: [DONE]\n\n").await;
    let resp = post_stream(&up).await;
    assert!(matches!(
        relay::peek(resp, OpenAiSink::default(), RelayConfig::default()).await,
        Peek::Empty
    ));
}

#[tokio::test]
async fn relay_idle_watchdog_and_heartbeat_terminate_a_stalled_stream() {
    let up = spawn_stall_mock().await;
    let resp = post_stream(&up).await;
    let cfg = RelayConfig {
        first_byte: Duration::from_secs(1),
        idle: Duration::from_millis(300),
        heartbeat: Duration::from_millis(50),
    };
    match relay::peek(resp, OpenAiSink::default(), cfg).await {
        Peek::Commit {
            prefix_out,
            rest,
            sink,
        } => {
            let body = relay::spawn_relay(prefix_out, rest, sink, cfg, |_stats| {});
            // The idle watchdog must terminate the stalled stream so this returns.
            let bytes = axum::body::to_bytes(body, usize::MAX).await.unwrap();
            let text = String::from_utf8_lossy(&bytes);
            assert!(text.contains("first"), "prefix relayed: {text}");
            assert!(text.contains("keep-alive"), "heartbeat emitted: {text}");
        }
        other => panic!("expected commit, got {}", matches!(other, Peek::Empty)),
    }
}

// ── server.rs Anthropic surface (non-streaming) ─────────────────────────────

#[tokio::test]
async fn anthropic_messages_non_streaming_translates_back() {
    let up = spawn_mock(200, completion_body()).await;
    let db: SharedDb = Arc::new(Mutex::new(Db::open_memory().unwrap()));
    let routes = vec![Route {
        name: "standard".into(),
        strategy: thegn_core::config::RoutingStrategy::Sequential,
        order_pool: None,
        priority: vec![backend("primary", &up)],
    }];
    let proxy = spawn_proxy(routes, db).await;

    let resp = reqwest::Client::new()
        .post(format!("{proxy}/v1/messages"))
        .json(&json!({"model": "model-proxy/standard", "max_tokens": 100, "messages": [{"role": "user", "content": "hi"}]}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let v: Value = resp.json().await.unwrap();
    assert_eq!(v["type"], "message");
    assert_eq!(v["role"], "assistant");
    assert_eq!(v["content"][0]["text"], "hello from upstream");
    assert_eq!(v["stop_reason"], "end_turn");
}

#[tokio::test]
async fn anthropic_count_tokens_and_invalid_request() {
    let db: SharedDb = Arc::new(Mutex::new(Db::open_memory().unwrap()));
    let routes = vec![Route {
        name: "standard".into(),
        strategy: thegn_core::config::RoutingStrategy::Sequential,
        order_pool: None,
        priority: vec![],
    }];
    let proxy = spawn_proxy(routes, db).await;

    let ct: Value = reqwest::Client::new()
        .post(format!("{proxy}/v1/messages/count_tokens"))
        .body("some body text to estimate")
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(ct["input_tokens"].as_u64().unwrap() > 0);

    // Malformed JSON → 400 Anthropic-shaped error.
    let bad = reqwest::Client::new()
        .post(format!("{proxy}/v1/messages"))
        .body("not json")
        .send()
        .await
        .unwrap();
    assert_eq!(bad.status(), 400);
    let v: Value = bad.json().await.unwrap();
    assert_eq!(v["type"], "error");
}

// ── router.rs additional flows ──────────────────────────────────────────────

#[tokio::test]
async fn streaming_anthropic_backend_synthesizes_sse() {
    // An Anthropic-surface backend in streaming mode is buffered then re-streamed.
    let up = spawn_anthropic_mock(
        200,
        json!({"id":"m","content":[{"type":"text","text":"streamed"}],"stop_reason":"end_turn","usage":{"input_tokens":2,"output_tokens":1}}),
    )
    .await;
    let db: SharedDb = Arc::new(Mutex::new(Db::open_memory().unwrap()));
    let routes = vec![Route {
        name: "standard".into(),
        strategy: thegn_core::config::RoutingStrategy::Sequential,
        order_pool: None,
        priority: vec![anthropic_backend("kimi", &up)],
    }];
    let proxy = spawn_proxy(routes, db).await;

    let resp = reqwest::Client::new()
        .post(format!("{proxy}/v1/chat/completions"))
        .json(&json!({"model": "model-proxy/standard", "stream": true, "messages": [{"role": "user", "content": "hi"}]}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let text = resp.text().await.unwrap();
    assert!(text.contains("streamed"), "synthesized SSE: {text}");
    assert!(text.contains("[DONE]"));
}

#[tokio::test]
async fn soft_failure_falls_through_without_cooldown() {
    // A 400 with a non-availability body is a soft failure → fall through, no cool.
    let soft = spawn_mock(400, json!({"error": {"message": "bad tool args"}})).await;
    let good = spawn_mock(200, completion_body()).await;
    let db: SharedDb = Arc::new(Mutex::new(Db::open_memory().unwrap()));
    let routes = vec![Route {
        name: "standard".into(),
        strategy: thegn_core::config::RoutingStrategy::Sequential,
        order_pool: None,
        priority: vec![backend("primary", &soft), backend("secondary", &good)],
    }];
    let proxy = spawn_proxy(routes, db).await;

    let resp = reqwest::Client::new()
        .post(format!("{proxy}/v1/chat/completions"))
        .json(&chat_request())
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    assert_eq!(resolved_backend(&proxy).await, "secondary");
}

#[tokio::test]
async fn network_error_falls_through_to_next_backend() {
    let good = spawn_mock(200, completion_body()).await;
    let db: SharedDb = Arc::new(Mutex::new(Db::open_memory().unwrap()));
    let routes = vec![Route {
        name: "standard".into(),
        strategy: thegn_core::config::RoutingStrategy::Sequential,
        order_pool: None,
        // First backend points at a dead port → transient network error.
        priority: vec![
            backend("dead", "http://127.0.0.1:1"),
            backend("secondary", &good),
        ],
    }];
    let proxy = spawn_proxy(routes, db).await;

    let resp = reqwest::Client::new()
        .post(format!("{proxy}/v1/chat/completions"))
        .json(&chat_request())
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    assert_eq!(resolved_backend(&proxy).await, "secondary");
}

// ── stats / latency / tokens-per-second ─────────────────────────────────────

/// Waits until `pred` over the DB's recent audit rows holds (rows are written
/// off the request path for streams).
async fn wait_for_rows(
    db: &SharedDb,
    n: usize,
    pred: impl Fn(&[thegn_core::db::ProxyRequestRow]) -> bool,
) -> Vec<thegn_core::db::ProxyRequestRow> {
    for _ in 0..50 {
        let rows = db
            .lock()
            .unwrap()
            .proxy_requests_since(0, 100)
            .unwrap_or_default();
        if rows.len() >= n && pred(&rows) {
            return rows;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    db.lock().unwrap().proxy_requests_since(0, 100).unwrap()
}

#[tokio::test]
async fn audit_rows_record_duration_and_stream_ttfb() {
    let up = spawn_mock(200, completion_body()).await;
    let sse = spawn_mock_sse(OPENAI_SSE).await;
    let db: SharedDb = Arc::new(Mutex::new(Db::open_memory().unwrap()));
    let routes = vec![
        Route {
            name: "standard".into(),
            strategy: thegn_core::config::RoutingStrategy::Sequential,
            order_pool: None,
            priority: vec![backend("plain", &up)],
        },
        Route {
            name: "fast".into(),
            strategy: thegn_core::config::RoutingStrategy::Sequential,
            order_pool: None,
            priority: vec![backend("streamy", &sse)],
        },
    ];
    let proxy = spawn_proxy(routes, db.clone()).await;
    let client = reqwest::Client::new();

    client
        .post(format!("{proxy}/v1/chat/completions"))
        .json(&chat_request())
        .send()
        .await
        .unwrap();
    let stream_req = json!({"model": "model-proxy/fast", "stream": true, "messages": [{"role": "user", "content": "hi"}]});
    client
        .post(format!("{proxy}/v1/chat/completions"))
        .json(&stream_req)
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();

    let rows = wait_for_rows(&db, 2, |rows| rows.iter().any(|r| r.outcome == "ok_stream")).await;
    let plain = rows.iter().find(|r| r.backend == "plain").unwrap();
    // Non-streaming: duration measured, no TTFB.
    assert!(plain.duration_ms >= 0);
    assert!(plain.ttfb_ms.is_none());
    let stream = rows.iter().find(|r| r.backend == "streamy").unwrap();
    // Streaming: both measured; generation time = duration - ttfb.
    assert!(stream.ttfb_ms.is_some());
    assert!(stream.duration_ms >= stream.ttfb_ms.unwrap());
    assert_eq!(stream.outcome, "ok_stream");
}

#[tokio::test]
async fn stats_endpoint_rolls_up_tokens_per_second_and_budgets() {
    let up = spawn_mock(200, completion_body()).await;
    let db: SharedDb = Arc::new(Mutex::new(Db::open_memory().unwrap()));
    let routes = vec![Route {
        name: "standard".into(),
        strategy: thegn_core::config::RoutingStrategy::Sequential,
        order_pool: None,
        priority: vec![backend("primary", &up)],
    }];
    let proxy = spawn_proxy(routes, db.clone()).await;

    for _ in 0..2 {
        reqwest::Client::new()
            .post(format!("{proxy}/v1/chat/completions"))
            .json(&chat_request())
            .send()
            .await
            .unwrap();
    }
    wait_for_rows(&db, 2, |_| true).await;

    let v: Value = reqwest::get(format!("{proxy}/stats"))
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(v["stats"]["totals"]["requests"], 2);
    assert_eq!(v["stats"]["totals"]["ok"], 2);
    assert_eq!(v["stats"]["totals"]["output_tokens"], 10); // 2 × 5
    assert_eq!(v["stats"]["by_backend"][0]["name"], "primary");
    assert_eq!(v["stats"]["by_route"][0]["name"], "standard");
    // Throughput is measured (mock responds instantly → clamped ≥ huge tok/s).
    assert!(v["stats"]["totals"]["tokens_per_sec"].as_f64().unwrap() > 0.0);
    // Budgets include the global rollup with the spend attributed.
    let budgets = v["budgets"].as_array().unwrap();
    let global = budgets.iter().find(|b| b["scope"] == "global").unwrap();
    assert_eq!(global["spent_tokens"], 30); // 2 × 15
    assert!(v["uptime_secs"].as_i64().unwrap() >= 0);
}

#[tokio::test]
async fn metrics_include_duration_histogram_and_uptime() {
    let up = spawn_mock(200, completion_body()).await;
    let db: SharedDb = Arc::new(Mutex::new(Db::open_memory().unwrap()));
    let routes = vec![Route {
        name: "standard".into(),
        strategy: thegn_core::config::RoutingStrategy::Sequential,
        order_pool: None,
        priority: vec![backend("primary", &up)],
    }];
    let proxy = spawn_proxy(routes, db).await;
    reqwest::Client::new()
        .post(format!("{proxy}/v1/chat/completions"))
        .json(&chat_request())
        .send()
        .await
        .unwrap();

    let metrics = reqwest::get(format!("{proxy}/metrics"))
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(metrics.contains("model_proxy_request_duration_seconds_count 1"));
    assert!(metrics.contains("model_proxy_request_duration_seconds_bucket{le=\"+Inf\"} 1"));
    assert!(metrics.contains("model_proxy_uptime_seconds"));
}

// ── upstream cost headers ────────────────────────────────────────────────────

/// Mock returning a completion plus an upstream cost header.
async fn spawn_cost_header_mock(cost: &'static str) -> String {
    async fn handler(State(cost): State<&'static str>) -> impl IntoResponse {
        (
            [
                (header::CONTENT_TYPE, "application/json"),
                (axum::http::HeaderName::from_static("x-nanogpt-cost"), cost),
            ],
            completion_body().to_string(),
        )
    }
    let app = Router::new()
        .route("/chat/completions", post(handler))
        .with_state(cost);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    format!("http://{addr}")
}

#[tokio::test]
async fn upstream_cost_header_wins_over_estimate() {
    let up = spawn_cost_header_mock("0.42").await;
    let db: SharedDb = Arc::new(Mutex::new(Db::open_memory().unwrap()));
    let routes = vec![Route {
        name: "standard".into(),
        strategy: thegn_core::config::RoutingStrategy::Sequential,
        order_pool: None,
        // "openrouter" is a cost-bearing provider, so the header applies.
        priority: vec![backend("openrouter", &up)],
    }];
    let proxy = spawn_proxy(routes, db.clone()).await;

    reqwest::Client::new()
        .post(format!("{proxy}/v1/chat/completions"))
        .json(&chat_request())
        .send()
        .await
        .unwrap();
    let rows = wait_for_rows(&db, 1, |_| true).await;
    assert_eq!(rows[0].cost_source, "header");
    assert!((rows[0].cost_usd - 0.42).abs() < 1e-9);
}

// ── Retry-After header ───────────────────────────────────────────────────────

/// Mock that 429s with a Retry-After header (no body reset hint).
async fn spawn_retry_after_mock(secs: &'static str) -> String {
    async fn handler(State(secs): State<&'static str>) -> impl IntoResponse {
        (
            StatusCode::TOO_MANY_REQUESTS,
            [(
                axum::http::HeaderName::from_static("retry-after"),
                axum::http::HeaderValue::from_static(secs),
            )],
            json!({"error": {"message": "rate limited"}}).to_string(),
        )
    }
    let app = Router::new()
        .route("/chat/completions", post(handler))
        .with_state(secs);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    format!("http://{addr}")
}

#[tokio::test]
async fn retry_after_header_sets_cooldown_deadline() {
    let bad = spawn_retry_after_mock("3600").await;
    let good = spawn_mock(200, completion_body()).await;
    let db: SharedDb = Arc::new(Mutex::new(Db::open_memory().unwrap()));
    let routes = vec![Route {
        name: "standard".into(),
        strategy: thegn_core::config::RoutingStrategy::Sequential,
        order_pool: None,
        priority: vec![backend("limited", &bad), backend("secondary", &good)],
    }];
    let proxy = spawn_proxy(routes, db).await;

    let resp = reqwest::Client::new()
        .post(format!("{proxy}/v1/chat/completions"))
        .json(&chat_request())
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);

    // /health reports the cooled backend with a re-probe ≈1h out (way past the
    // default rate-limit backoff's 30s initial step).
    let health: Value = reqwest::get(format!("{proxy}/health"))
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let entry = &health["backends"]["limited:test-model"];
    assert_eq!(entry["status"], "cooling");
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64;
    let probe = entry["next_probe_ms"].as_i64().unwrap();
    assert!(
        probe > now_ms + 3_000_000,
        "expected ~1h cooldown, got {}s",
        (probe - now_ms) / 1000
    );
}

// ── model aliasing (U 281) ───────────────────────────────────────────────────

#[tokio::test]
async fn model_alias_selects_route() {
    let std_up = spawn_mock(200, completion_body()).await;
    let fast_up = spawn_mock(200, completion_body()).await;
    let db: SharedDb = Arc::new(Mutex::new(Db::open_memory().unwrap()));
    let doc = format!(
        r#"{{"aliases":{{"claude-sonnet-4-6":"fast"}},"routes":[
            {{"name":"standard","backends":[{{"name":"std","base_url":"{std_up}","model":"m","api_key":"k"}}]}},
            {{"name":"fast","backends":[{{"name":"quick","base_url":"{fast_up}","model":"m","api_key":"k"}}]}}
        ]}}"#
    );
    let cfg = thegn_proxy::config::parse_config(&doc).unwrap();
    assert!(!cfg.last_resort);
    let proxy = spawn_proxy_full(cfg, db).await;

    let resp = reqwest::Client::new()
        .post(format!("{proxy}/v1/chat/completions"))
        .json(
            &json!({"model": "claude-sonnet-4-6", "messages": [{"role": "user", "content": "hi"}]}),
        )
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let resolved: Value = reqwest::get(format!("{proxy}/resolved"))
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    // The aliased model routed to `fast`, not the default first route.
    assert_eq!(resolved["fast"], "quick");
    assert!(resolved.get("standard").is_none());
}

// ── last-resort cross-route fallback ─────────────────────────────────────────

#[tokio::test]
async fn last_resort_borrows_other_routes_backends() {
    let dead = spawn_mock(500, json!({"error": {"message": "boom"}})).await;
    let alive = spawn_mock(200, completion_body()).await;
    let db: SharedDb = Arc::new(Mutex::new(Db::open_memory().unwrap()));
    let doc = format!(
        r#"{{"last_resort":true,"routes":[
            {{"name":"standard","backends":[{{"name":"dead","base_url":"{dead}","model":"m","api_key":"k"}}]}},
            {{"name":"fast","backends":[{{"name":"rescue","base_url":"{alive}","model":"m","api_key":"k"}}]}}
        ]}}"#
    );
    let cfg = thegn_proxy::config::parse_config(&doc).unwrap();
    assert!(cfg.last_resort);
    let proxy = spawn_proxy_full(cfg, db.clone()).await;

    let resp = reqwest::Client::new()
        .post(format!("{proxy}/v1/chat/completions"))
        .json(&chat_request())
        .send()
        .await
        .unwrap();
    // The standard route is dead, but the fast route's backend rescues it.
    assert_eq!(resp.status(), 200);
    let resolved: Value = reqwest::get(format!("{proxy}/resolved"))
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(resolved["standard"], "rescue");
}

#[tokio::test]
async fn last_resort_off_still_fails() {
    let dead = spawn_mock(500, json!({"error": {"message": "boom"}})).await;
    let alive = spawn_mock(200, completion_body()).await;
    let db: SharedDb = Arc::new(Mutex::new(Db::open_memory().unwrap()));
    let doc = format!(
        r#"{{"routes":[
            {{"name":"standard","backends":[{{"name":"dead","base_url":"{dead}","model":"m","api_key":"k"}}]}},
            {{"name":"fast","backends":[{{"name":"rescue","base_url":"{alive}","model":"m","api_key":"k"}}]}}
        ]}}"#
    );
    let cfg = thegn_proxy::config::parse_config(&doc).unwrap();
    let proxy = spawn_proxy_full(cfg, db).await;

    let resp = reqwest::Client::new()
        .post(format!("{proxy}/v1/chat/completions"))
        .json(&chat_request())
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 503);
}

// ── workspace-scoped accounts (V 287) ────────────────────────────────────────

/// Spawns the proxy against a fully-parsed config (aliases/last_resort intact).
async fn spawn_proxy_full(mut config: ProxyConfig, db: SharedDb) -> String {
    config.listen = "127.0.0.1:0".parse().unwrap();
    let listener = tokio::net::TcpListener::bind(config.listen).await.unwrap();
    let addr = listener.local_addr().unwrap();
    let state = AppState::new(config, db, now_ms());
    let app = server::app(state);
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    format!("http://{addr}")
}

#[tokio::test]
async fn workspace_virtual_key_pins_upstream_and_attributes_spend() {
    use thegn_core::store::WorkspaceStore;
    let seen = Arc::new(Mutex::new(Vec::new()));
    let up = spawn_keyed_mock(None, seen.clone()).await;
    let db: SharedDb = Arc::new(Mutex::new(Db::open_memory().unwrap()));
    {
        let g = db.lock().unwrap();
        g.put_workspace("/repo", "ws", "repo").unwrap();
        g.put_worktree("t", "/repo", "/repo/wt", "main", None, None)
            .unwrap();
        // The worktree's key is bound to provider "pinned" — its account.
        g.put_proxy_virtual_key(
            "szk_test",
            "h",
            "wt key",
            "worktree:/repo/wt",
            Some("pinned"),
            1,
        )
        .unwrap();
    }
    // Config order puts "first" ahead; the binding must still win.
    let doc = format!(
        r#"{{"routes":[{{"name":"standard","backends":[
            {{"name":"first","base_url":"{up}","model":"m","api_key":"kf"}},
            {{"name":"pinned","base_url":"{up}","model":"m","api_key":"kp"}}
        ]}}]}}"#
    );
    let cfg = thegn_proxy::config::parse_config(&doc).unwrap();
    let proxy = spawn_proxy_full(cfg, db.clone()).await;

    let resp = reqwest::Client::new()
        .post(format!("{proxy}/v1/chat/completions"))
        .bearer_auth("szk_test")
        .json(&chat_request())
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);

    // The pinned provider's key was used first.
    assert_eq!(seen.lock().unwrap().first().map(String::as_str), Some("kp"));

    // Spend attributed up the chain: worktree → workspace → global.
    let rows = wait_for_rows(&db, 1, |_| true).await;
    assert_eq!(rows[0].worktree.as_deref(), Some("/repo/wt"));
    assert_eq!(rows[0].workspace.as_deref(), Some("/repo"));
    let g = db.lock().unwrap();
    for scope in ["worktree:/repo/wt", "workspace:/repo", "global"] {
        assert_eq!(
            g.proxy_budget(scope).unwrap().unwrap().spent_tokens,
            15,
            "scope {scope}"
        );
    }
}

#[tokio::test]
async fn workspace_cap_refuses_worktree_member_e2e() {
    use thegn_core::store::WorkspaceStore;
    let up = spawn_mock(200, completion_body()).await;
    let db: SharedDb = Arc::new(Mutex::new(Db::open_memory().unwrap()));
    {
        let g = db.lock().unwrap();
        g.put_workspace("/repo", "ws", "repo").unwrap();
        g.put_worktree("t", "/repo", "/repo/wt", "main", None, None)
            .unwrap();
        g.put_proxy_virtual_key("szk_capped", "h", "wt", "worktree:/repo/wt", None, 1)
            .unwrap();
        // The WORKSPACE cap is exhausted; the worktree itself has no cap.
        g.set_proxy_budget_limits("workspace:/repo", "monthly", Some(10), None, 0)
            .unwrap();
        g.add_proxy_spend("workspace:/repo", 20, 0.0, 1).unwrap();
    }
    let routes = vec![Route {
        name: "standard".into(),
        strategy: thegn_core::config::RoutingStrategy::Sequential,
        order_pool: None,
        priority: vec![backend("primary", &up)],
    }];
    let proxy = spawn_proxy(routes, db).await;

    let resp = reqwest::Client::new()
        .post(format!("{proxy}/v1/chat/completions"))
        .bearer_auth("szk_capped")
        .json(&chat_request())
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 402);
    let v: Value = resp.json().await.unwrap();
    assert!(
        v["error"]["message"]
            .as_str()
            .unwrap()
            .contains("workspace:/repo")
    );
}

#[tokio::test]
async fn health_and_models_endpoints() {
    let db: SharedDb = Arc::new(Mutex::new(Db::open_memory().unwrap()));
    let routes = vec![Route {
        name: "standard".into(),
        strategy: thegn_core::config::RoutingStrategy::Sequential,
        order_pool: None,
        priority: vec![],
    }];
    let proxy = spawn_proxy(routes, db).await;

    let health: Value = reqwest::get(format!("{proxy}/health"))
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(health["status"], "ok");

    let models: Value = reqwest::get(format!("{proxy}/v1/models"))
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(models["data"][0]["id"], "model-proxy/standard");
}
