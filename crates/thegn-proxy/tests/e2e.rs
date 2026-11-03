use std::collections::BTreeMap;
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{Value, json};
use tempfile::TempDir;
use thegn_core::config_model_proxy::{
    AUTO_ALIAS, BudgetBreach, BudgetLimit, ModelProviderKind, ModelProxyConfig, ProviderEntry,
    RouteBackend, RouteEntry, RoutingStrategy,
};
use thegn_core::db::Db;
use thegn_core::proxy::attribution::{AttributionScope, encode_signed};
use thegn_core::store::ModelProxyStore;
use thegn_proxy::budget::{self, Identity};
use thegn_proxy::shared::{SharedDb, now_ms};
use thegn_proxy::{config, server, state::AppState};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::task::JoinHandle;

const SECRET: &[u8] = b"e2e-attribution-secret";
const PROMPT_MARKER: &str = "e2e-secret-prompt-7f3a";

struct Task(JoinHandle<()>);

impl Drop for Task {
    fn drop(&mut self) {
        self.0.abort();
    }
}

#[derive(Clone)]
enum Script {
    Json(u16, Value, Vec<(String, String)>),
    JsonThen {
        first: (u16, Value, Vec<(String, String)>),
        rest: Value,
    },
    Stall,
    Sse(Vec<&'static str>),
}

struct Upstream {
    url: String,
    hits: Arc<AtomicUsize>,
    bodies: Arc<Mutex<Vec<Value>>>,
    _task: Task,
}

async fn upstream(script: Script) -> Upstream {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let hits = Arc::new(AtomicUsize::new(0));
    let bodies = Arc::new(Mutex::new(Vec::new()));
    let task_hits = hits.clone();
    let task_bodies = bodies.clone();
    let task = tokio::spawn(async move {
        let mut script = script;
        loop {
            let (mut socket, _) = listener.accept().await.unwrap();
            let request = read_request(&mut socket).await;
            let body = serde_json::from_slice::<Value>(&request).unwrap();
            task_bodies.lock().unwrap().push(body);
            let index = task_hits.fetch_add(1, Ordering::SeqCst);
            match &mut script {
                Script::Json(status, body, headers) => {
                    write_json(&mut socket, *status, body, headers).await;
                }
                Script::JsonThen { first, rest } => {
                    if index == 0 {
                        write_json(&mut socket, first.0, &first.1, &first.2).await;
                    } else {
                        write_json(&mut socket, 200, rest, &[]).await;
                    }
                }
                Script::Stall => {
                    let _hold_task = tokio::spawn(async move {
                        let _hold = socket;
                        std::future::pending::<()>().await
                    });
                }
                Script::Sse(events) => write_sse(&mut socket, events).await,
            }
        }
    });
    Upstream {
        url: format!("http://{address}/v1"),
        hits,
        bodies,
        _task: Task(task),
    }
}

async fn read_request(socket: &mut tokio::net::TcpStream) -> Vec<u8> {
    let mut request = Vec::new();
    let header_end = loop {
        if let Some(index) = request.windows(4).position(|part| part == b"\r\n\r\n") {
            break index + 4;
        }
        let mut chunk = [0_u8; 4096];
        let count = socket.read(&mut chunk).await.unwrap();
        assert!(count > 0, "upstream request ended before headers");
        request.extend_from_slice(&chunk[..count]);
    };
    let headers = String::from_utf8_lossy(&request[..header_end]);
    let content_length = headers
        .lines()
        .find_map(|line| {
            let (name, value) = line.split_once(':')?;
            name.eq_ignore_ascii_case("content-length")
                .then(|| value.trim().parse::<usize>().unwrap())
        })
        .unwrap_or(0);
    let body_end = header_end + content_length;
    while request.len() < body_end {
        let mut chunk = [0_u8; 4096];
        let count = socket.read(&mut chunk).await.unwrap();
        assert!(count > 0, "upstream request ended before its body");
        request.extend_from_slice(&chunk[..count]);
    }
    request[header_end..body_end].to_vec()
}

async fn write_json(
    socket: &mut tokio::net::TcpStream,
    status: u16,
    body: &Value,
    headers: &[(String, String)],
) {
    let bytes = serde_json::to_vec(body).unwrap();
    let reason = if status == 429 {
        "Too Many Requests"
    } else if status == 500 {
        "Internal Server Error"
    } else {
        "OK"
    };
    let mut response = format!(
        "HTTP/1.1 {status} {reason}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n",
        bytes.len()
    );
    for (name, value) in headers {
        response.push_str(&format!("{name}: {value}\r\n"));
    }
    response.push_str("\r\n");
    socket.write_all(response.as_bytes()).await.unwrap();
    socket.write_all(&bytes).await.unwrap();
}

async fn write_sse(socket: &mut tokio::net::TcpStream, events: &[&str]) {
    let mut body = events.join("\n\n");
    body.push_str("\n\n");
    body.push_str("data: [DONE]\n\n");
    let response = format!(
        "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
        body.len(),
        body
    );
    socket.write_all(response.as_bytes()).await.unwrap();
}

fn install_secret() {
    match budget::install_attribution_secret(SECRET.to_vec()) {
        Ok(()) => {}
        Err(existing) => assert_eq!(
            existing, SECRET,
            "the process already has the expected e2e secret"
        ),
    }
}

fn signed_key() -> String {
    encode_signed(
        &AttributionScope {
            scope: "worktree:/e2e/wt".into(),
            workspace: Some("workspace:/e2e".into()),
            nonce: "e2e".into(),
            ..Default::default()
        },
        SECRET,
        now_ms() + 600_000,
    )
}

fn key_file(dir: &Path, name: &str) -> String {
    let path = dir.join(name);
    std::fs::write(&path, "e2e-upstream-key\n").unwrap();
    thegn_core::fsperm::restrict_to_owner(&path).unwrap();
    format!("file:{}", path.display())
}

fn provider(name: &str, upstream: &Upstream, key: String) -> ProviderEntry {
    ProviderEntry {
        name: name.into(),
        kind: ModelProviderKind::Openai,
        base_url: upstream.url.clone(),
        api_key: key,
        ..Default::default()
    }
}

fn backend(provider: &str, model: &str) -> RouteBackend {
    RouteBackend {
        provider: provider.into(),
        model: model.into(),
    }
}

async fn start_proxy(cfg: ModelProxyConfig) -> (String, SharedDb, Task) {
    install_secret();
    let proxy_config = config::build(&cfg).unwrap();
    let db = Arc::new(Mutex::new(Db::open_memory().unwrap()));
    let state = AppState::new(proxy_config, db.clone(), now_ms());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let task = tokio::spawn(async move {
        axum::serve(listener, server::app(state)).await.unwrap();
    });
    (format!("http://{address}"), db, Task(task))
}

fn base_config() -> ModelProxyConfig {
    ModelProxyConfig {
        enabled: true,
        routing: RoutingStrategy::Sequential,
        ..Default::default()
    }
}

fn request(model: &str, prompt: &str, stream: bool) -> Value {
    json!({
        "model": model,
        "messages": [{"role": "user", "content": prompt}],
        "stream": stream,
    })
}

async fn post_openai(base: &str, body: Value) -> reqwest::Response {
    reqwest::Client::new()
        .post(format!("{base}/v1/chat/completions"))
        .bearer_auth(signed_key())
        .json(&body)
        .send()
        .await
        .unwrap()
}

async fn content(response: reqwest::Response) -> (u16, String, String) {
    let status = response.status().as_u16();
    let content_type = response
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .unwrap_or("")
        .to_string();
    (status, response.text().await.unwrap(), content_type)
}

fn route(name: &str, backends: Vec<RouteBackend>) -> RouteEntry {
    RouteEntry {
        name: name.into(),
        backends,
        ..Default::default()
    }
}

#[tokio::test]
async fn e2e_sequential_failover_serves_from_the_second_lane() {
    let dir = TempDir::new().unwrap();
    let first = upstream(Script::Json(500, json!({"error":"busy"}), vec![])).await;
    let second = upstream(Script::Json(200, completion("from-b"), vec![])).await;
    let mut cfg = base_config();
    cfg.providers = vec![
        provider("a", &first, key_file(dir.path(), "a-key")),
        provider("b", &second, key_file(dir.path(), "b-key")),
    ];
    cfg.routes = vec![route(
        "standard",
        vec![backend("a", "a-model"), backend("b", "b-model")],
    )];
    let (proxy, _, _proxy_task) = start_proxy(cfg).await;
    let response = content(post_openai(&proxy, request("standard", "hello", false)).await).await;
    assert_eq!(
        response.0, 200,
        "failover HTTP status was {} with body {}",
        response.0, response.1
    );
    assert!(
        response.1.contains("from-b"),
        "failover response body was {}",
        response.1
    );
    assert_eq!(
        first.hits.load(Ordering::SeqCst),
        1,
        "first lane hits: {}",
        first.hits.load(Ordering::SeqCst)
    );
    assert_eq!(
        second.hits.load(Ordering::SeqCst),
        1,
        "second lane hits: {}",
        second.hits.load(Ordering::SeqCst)
    );
}

#[tokio::test]
async fn e2e_rate_limited_lane_cools_down_then_fails_back() {
    let dir = TempDir::new().unwrap();
    let first = upstream(Script::JsonThen {
        first: (
            429,
            json!({"error":"slow down"}),
            vec![("retry-after".into(), "5".into())],
        ),
        rest: completion("from-a"),
    })
    .await;
    let second = upstream(Script::Json(200, completion("from-b"), vec![])).await;
    let mut cfg = base_config();
    cfg.providers = vec![
        provider("a", &first, key_file(dir.path(), "a-key")),
        provider("b", &second, key_file(dir.path(), "b-key")),
    ];
    cfg.routes = vec![route(
        "standard",
        vec![backend("a", "a-model"), backend("b", "b-model")],
    )];
    let (proxy, _, _proxy_task) = start_proxy(cfg).await;

    for _ in 0..2 {
        let response =
            content(post_openai(&proxy, request("standard", "hello", false)).await).await;
        assert_eq!(
            response.0, 200,
            "cooldown response status/body: {} {}",
            response.0, response.1
        );
        assert!(
            response.1.contains("from-b"),
            "cooldown response body: {}",
            response.1
        );
    }
    assert_eq!(
        first.hits.load(Ordering::SeqCst),
        1,
        "cooled first lane hits: {}",
        first.hits.load(Ordering::SeqCst)
    );

    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    let mut failed_back = false;
    while tokio::time::Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(250)).await;
        let response =
            content(post_openai(&proxy, request("standard", "hello", false)).await).await;
        assert_eq!(
            response.0, 200,
            "failback response status/body: {} {}",
            response.0, response.1
        );
        if response.1.contains("from-a") {
            failed_back = true;
            break;
        }
    }
    assert!(
        failed_back,
        "first lane did not fail back; hits={}",
        first.hits.load(Ordering::SeqCst)
    );
    assert_eq!(
        first.hits.load(Ordering::SeqCst),
        2,
        "first lane hits after failback: {}",
        first.hits.load(Ordering::SeqCst)
    );
}

#[tokio::test]
async fn e2e_alias_and_auto_select_their_routes() {
    let dir = TempDir::new().unwrap();
    let fast = upstream(Script::Json(200, completion("fast"), vec![])).await;
    let heavy = upstream(Script::Json(200, completion("heavy"), vec![])).await;
    let mut cfg = base_config();
    cfg.providers = vec![
        provider("fast", &fast, key_file(dir.path(), "fast-key")),
        provider("heavy", &heavy, key_file(dir.path(), "heavy-key")),
    ];
    cfg.routes = vec![
        RouteEntry {
            auto_max_tokens: Some(100),
            ..route("fast", vec![backend("fast", "fast-model")])
        },
        route("heavy", vec![backend("heavy", "heavy-model")]),
    ];
    cfg.aliases = BTreeMap::from([
        ("gpt-e2e".into(), "heavy".into()),
        ("auto".into(), AUTO_ALIAS.into()),
    ]);
    let (proxy, _, _proxy_task) = start_proxy(cfg).await;
    let large_prompt = "large".repeat(500);

    for (model, prompt, expected) in [
        ("gpt-e2e", "alias prompt", "heavy-model"),
        ("model-proxy/fast", "explicit route", "fast-model"),
        ("auto", "small", "fast-model"),
        ("auto", large_prompt.as_str(), "heavy-model"),
    ] {
        let response = content(post_openai(&proxy, request(model, prompt, false)).await).await;
        assert_eq!(
            response.0, 200,
            "routing status/body for {model}: {} {}",
            response.0, response.1
        );
        let served = if expected == "fast-model" {
            &fast
        } else {
            &heavy
        };
        let bodies = served.bodies.lock().unwrap();
        assert_eq!(
            bodies
                .last()
                .and_then(|body| body.get("model"))
                .and_then(Value::as_str),
            Some(expected),
            "upstream model for {model} was {:?}",
            bodies.last()
        );
    }
    assert_eq!(
        fast.hits.load(Ordering::SeqCst),
        2,
        "fast lane hits: {}",
        fast.hits.load(Ordering::SeqCst)
    );
    assert_eq!(
        heavy.hits.load(Ordering::SeqCst),
        2,
        "heavy lane hits: {}",
        heavy.hits.load(Ordering::SeqCst)
    );
}

#[tokio::test]
async fn e2e_stream_falls_through_a_lane_that_sends_no_first_byte() {
    let dir = TempDir::new().unwrap();
    let first = upstream(Script::Stall).await;
    let second = upstream(Script::Sse(vec![
        "data: {\"choices\":[{\"delta\":{\"content\":\"streamed\"}}]}",
    ]))
    .await;
    let mut cfg = base_config();
    cfg.first_byte_timeout_secs = 1;
    cfg.providers = vec![
        provider("a", &first, key_file(dir.path(), "a-key")),
        provider("b", &second, key_file(dir.path(), "b-key")),
    ];
    cfg.routes = vec![route(
        "standard",
        vec![backend("a", "a-model"), backend("b", "b-model")],
    )];
    let (proxy, _, _proxy_task) = start_proxy(cfg).await;
    let response = tokio::time::timeout(
        Duration::from_secs(30),
        post_openai(&proxy, request("standard", "hello", true)),
    )
    .await
    .expect("stream request exceeded its generous deadline");
    let result = content(response).await;
    assert_eq!(
        result.0, 200,
        "stream response status/body: {} {}",
        result.0, result.1
    );
    assert!(
        result.2.contains("text/event-stream"),
        "stream content-type: {}",
        result.2
    );
    assert!(
        result.1.contains("streamed"),
        "stream response body: {}",
        result.1
    );
    assert_eq!(
        first.hits.load(Ordering::SeqCst),
        1,
        "stalled lane hits: {}",
        first.hits.load(Ordering::SeqCst)
    );
}

#[tokio::test]
async fn e2e_anthropic_stream_is_translated_from_openai_sse() {
    let dir = TempDir::new().unwrap();
    let upstream = upstream(Script::Sse(vec!["data: {\"id\":\"chatcmpl-e2e\",\"model\":\"claude-e2e\",\"choices\":[{\"delta\":{\"content\":\"hi\"}}]}"])).await;
    let mut cfg = base_config();
    cfg.providers = vec![provider(
        "openai",
        &upstream,
        key_file(dir.path(), "provider-key"),
    )];
    cfg.routes = vec![route("standard", vec![backend("openai", "claude-e2e")])];
    let (proxy, _, _proxy_task) = start_proxy(cfg).await;
    let response = reqwest::Client::new()
        .post(format!("{proxy}/v1/messages"))
        .header("x-api-key", signed_key())
        .json(&json!({"model":"standard", "max_tokens":32, "messages":[{"role":"user","content":"hello"}], "stream":true}))
        .send()
        .await
        .unwrap();
    let result = content(response).await;
    let start = result.1.find("message_start");
    let delta = result.1.find("content_block_delta");
    let hi = result.1.find("hi");
    let stop = result.1.find("message_stop");
    assert_eq!(
        result.0, 200,
        "Anthropic stream status/body: {} {}",
        result.0, result.1
    );
    assert!(
        result.2.contains("text/event-stream"),
        "Anthropic stream content-type: {}",
        result.2
    );
    assert!(
        matches!((start, delta, hi, stop), (Some(a), Some(b), Some(c), Some(d)) if a < b && b < c && c < d),
        "Anthropic event order/body: {}",
        result.1
    );
}

#[tokio::test]
async fn e2e_budget_refusal_never_reaches_the_upstream() {
    let dir = TempDir::new().unwrap();
    let refused = upstream(Script::Json(200, completion("unexpected"), vec![])).await;
    let mut limited_cfg = single_route_config(dir.path(), &refused);
    limited_cfg.budget.enabled = true;
    limited_cfg.budget.on_breach = BudgetBreach::Refuse;
    limited_cfg.budget.scopes.insert(
        "workspace:/e2e".into(),
        BudgetLimit {
            tokens: Some(1),
            cost_usd: None,
        },
    );
    let (limited_proxy, db, _limited_task) = start_proxy(limited_cfg.clone()).await;
    let runtime = config::build(&limited_cfg).unwrap();
    budget::record_spend(
        &db,
        &runtime.budget,
        &Identity {
            scope: "worktree:/e2e/wt".into(),
            workspace: Some("workspace:/e2e".into()),
            ..Default::default()
        },
        1,
        0.0,
        now_ms(),
    );
    for path in ["/v1/chat/completions", "/v1/messages"] {
        let response = reqwest::Client::new()
            .post(format!("{limited_proxy}{path}"))
            .bearer_auth(signed_key())
            .json(&json!({"model":"standard", "max_tokens":10, "messages":[{"role":"user","content":"hello"}]}))
            .send()
            .await
            .unwrap();
        let result = content(response).await;
        assert_eq!(
            result.0, 402,
            "budget refusal status/body for {path}: {} {}",
            result.0, result.1
        );
        assert!(
            result
                .1
                .contains("budget cap reached for scope 'workspace:/e2e'"),
            "budget refusal reason for {path}: {}",
            result.1
        );
    }
    assert_eq!(
        refused.hits.load(Ordering::SeqCst),
        0,
        "refused upstream hits: {}",
        refused.hits.load(Ordering::SeqCst)
    );

    let allowed = upstream(Script::Json(200, completion("allowed"), vec![])).await;
    let normal_cfg = single_route_config(dir.path(), &allowed);
    let (normal_proxy, _, _normal_task) = start_proxy(normal_cfg).await;
    let response =
        content(post_openai(&normal_proxy, request("standard", "hello", false)).await).await;
    assert_eq!(
        response.0, 200,
        "positive-control status/body: {} {}",
        response.0, response.1
    );
    assert_eq!(
        allowed.hits.load(Ordering::SeqCst),
        1,
        "positive-control upstream hits: {}",
        allowed.hits.load(Ordering::SeqCst)
    );
}

#[tokio::test]
async fn e2e_subscription_lane_records_zero_cost_and_cache_tokens() {
    let dir = TempDir::new().unwrap();
    let upstream = upstream(Script::Json(200, json!({
        "id":"chatcmpl-subscription",
        "object":"chat.completion",
        "model":"subscription-model",
        "choices":[{"index":0,"message":{"role":"assistant","content":"ok"},"finish_reason":"stop"}],
        "usage":{"prompt_tokens":7,"completion_tokens":3,"total_tokens":10,"prompt_tokens_details":{"cached_tokens":5}}
    }), vec![])).await;
    let mut cfg = single_route_config(dir.path(), &upstream);
    cfg.providers[0].subscription = true;
    cfg.providers[0].input_usd_per_mtok = 12.0;
    cfg.providers[0].output_usd_per_mtok = 34.0;
    let (proxy, db, _proxy_task) = start_proxy(cfg).await;
    let response =
        content(post_openai(&proxy, request("standard", PROMPT_MARKER, false)).await).await;
    assert_eq!(
        response.0, 200,
        "subscription response status/body: {} {}",
        response.0, response.1
    );

    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    let rows = loop {
        let rows = db
            .lock()
            .unwrap()
            .model_proxy_requests_since(0, 10)
            .unwrap();
        if !rows.is_empty() {
            break rows;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "audit row was not written after response; status/body: {} {}",
            response.0,
            response.1
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    };
    let row = &rows[0];
    assert_eq!(row.cost_usd, 0.0, "subscription cost_usd: {:?}", row);
    assert_eq!(
        row.cost_source, "subscription",
        "subscription cost_source: {:?}",
        row
    );
    assert_eq!(row.input_tokens, 7, "subscription input_tokens: {:?}", row);
    assert_eq!(
        row.output_tokens, 3,
        "subscription output_tokens: {:?}",
        row
    );
    assert_eq!(
        row.cache_read_tokens, 5,
        "subscription cache_read_tokens: {:?}",
        row
    );
    assert!(
        row.outcome.starts_with("ok"),
        "subscription outcome: {:?}",
        row
    );
    let audit = format!("{row:?}");
    assert!(
        !audit.contains(PROMPT_MARKER),
        "audit row included request prompt: {audit}"
    );
    assert!(
        !audit.contains("e2e-upstream-key"),
        "audit row included provider key: {audit}"
    );
}

fn single_route_config(dir: &Path, upstream: &Upstream) -> ModelProxyConfig {
    let mut cfg = base_config();
    cfg.providers = vec![provider(
        "provider",
        upstream,
        key_file(dir, "provider-key"),
    )];
    cfg.routes = vec![route(
        "standard",
        vec![backend("provider", "subscription-model")],
    )];
    cfg
}

fn completion(content: &str) -> Value {
    json!({
        "id":"chatcmpl-e2e",
        "object":"chat.completion",
        "created":1,
        "model":"test-model",
        "choices":[{"index":0,"message":{"role":"assistant","content":content},"finish_reason":"stop"}],
        "usage":{"prompt_tokens":1,"completion_tokens":1,"total_tokens":2}
    })
}
