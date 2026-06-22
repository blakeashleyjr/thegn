//! End-to-end tests: a real proxy bound to an ephemeral port, routing to mock
//! upstream HTTP servers. Exercises a served response, ordered failover past an
//! exhausted backend, and budget-cap refusal.

use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::Mutex;

use axum::{Router, extract::State, http::HeaderMap, http::StatusCode, routing::post};
use serde_json::{Value, json};
use superzej_core::db::Db;
use superzej_core::proxy::compress::Level;
use superzej_core::proxy::ratelimit::RatePolicy;
use superzej_core::proxy::transform::CompressPolicy;
use superzej_proxy::model::{Backend, ProxyConfig, Route};
use superzej_proxy::server;
use superzej_proxy::shared::{SharedDb, now_ms};
use superzej_proxy::state::AppState;

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
/// `/chat/completions`. Returns its base URL.
async fn spawn_mock_sse(body: &'static str) -> String {
    async fn handler(State(body): State<&'static str>) -> impl axum::response::IntoResponse {
        (
            [(axum::http::header::CONTENT_TYPE, "text/event-stream")],
            body,
        )
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
        relay: superzej_proxy::relay::RelayConfig::default(),
        compression,
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
    superzej_proxy::config::parse_config(&doc).unwrap().routes
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

#[tokio::test]
async fn health_and_models_endpoints() {
    let db: SharedDb = Arc::new(Mutex::new(Db::open_memory().unwrap()));
    let routes = vec![Route {
        name: "standard".into(),
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
