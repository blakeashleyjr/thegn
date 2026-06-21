//! End-to-end tests: a real proxy bound to an ephemeral port, routing to mock
//! upstream HTTP servers. Exercises a served response, ordered failover past an
//! exhausted backend, and budget-cap refusal.

use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::Mutex;

use axum::{Router, extract::State, http::StatusCode, routing::post};
use serde_json::{Value, json};
use superzej_core::db::Db;
use superzej_core::proxy::ratelimit::RatePolicy;
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

/// Spawns the proxy against `routes` + `db`, returns its base URL.
async fn spawn_proxy(routes: Vec<Route>, db: SharedDb) -> String {
    let config = ProxyConfig {
        listen: "127.0.0.1:0".parse::<SocketAddr>().unwrap(),
        routes,
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
