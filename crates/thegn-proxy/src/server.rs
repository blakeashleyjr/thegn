//! axum HTTP surface. Endpoints mirror the pre-alpha proxy: the OpenAI
//! `/v1/chat/completions` (streaming + non-streaming), the Anthropic
//! `/v1/messages` (+ `count_tokens`), plus `/v1/models`, `/healthz`, `/metrics`,
//! `/resolved`, and `/stats`.

use axum::{
    Router,
    body::{Body, Bytes},
    extract::{Query, State},
    http::{HeaderMap, StatusCode, header},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use serde_json::{Value, json};
use thegn_core::proxy::route_select::{RequestFeatures, route_select};
use thegn_core::proxy::{transform, translate};
use thegn_core::store::ModelProxyStore;

use crate::budget::{BudgetVerdict, check_budget, resolve_identity};
use crate::model::Route;
use crate::router::{StreamOutcome, Surface, route_nonstreaming, route_streaming};
use crate::shared::{now_ms, now_unix};
use crate::state::SharedState;

/// Builds the axum router with all endpoints bound to `state`.
pub fn app(state: SharedState) -> Router {
    Router::new()
        .route("/v1/chat/completions", post(chat_completions))
        .route("/v1/messages", post(anthropic_messages))
        .route("/v1/messages/count_tokens", post(count_tokens))
        .route("/v1/models", get(models))
        .route("/healthz", get(healthz))
        .route("/metrics", get(metrics))
        .route("/resolved", get(resolved))
        .route("/stats", get(stats))
        .route("/usage", post(push_usage))
        .with_state(state)
}

/// Extracts the virtual key from `Authorization: Bearer <key>` or `x-api-key`.
fn virtual_key(headers: &HeaderMap) -> Option<String> {
    if let Some(v) = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        && let Some(rest) = v.strip_prefix("Bearer ")
    {
        return Some(rest.trim().to_string());
    }
    headers
        .get("x-api-key")
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string())
}

fn model_of(body: &[u8]) -> String {
    serde_json::from_slice::<Value>(body)
        .ok()
        .and_then(|v| v.get("model").and_then(Value::as_str).map(String::from))
        .unwrap_or_default()
}

fn wants_stream(body: &[u8]) -> bool {
    serde_json::from_slice::<Value>(body)
        .ok()
        .and_then(|v| v.get("stream").and_then(Value::as_bool))
        .unwrap_or(false)
}

/// Resolves the client model to a route, running the deterministic `auto` tier
/// classifier when the model resolves to `@auto`.
fn resolve_route<'a>(
    state: &'a SharedState,
    model: &str,
    headers: &HeaderMap,
    body: &[u8],
) -> Option<&'a Route> {
    if state.config.is_auto(model) {
        let parsed: Value = serde_json::from_slice(body).unwrap_or(Value::Null);
        let features = RequestFeatures {
            est_tokens: transform::estimated_request_tokens(body.len()),
            has_tools: transform::request_has_tools(&parsed),
            streaming: wants_stream(body),
            mode_hint: headers
                .get("x-thegn-mode")
                .and_then(|v| v.to_str().ok())
                .map(str::to_string),
        };
        if let Some(tier) = route_select(&state.config.auto_tiers, &features) {
            return state.config.lookup_route(&tier);
        }
    }
    state.config.lookup_route(model)
}

fn json_response(status: u16, body: Vec<u8>) -> Response {
    Response::builder()
        .status(StatusCode::from_u16(status).unwrap_or(StatusCode::BAD_GATEWAY))
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(body))
        .unwrap()
}

fn error_json(status: u16, message: &str, kind: &str) -> Response {
    let body = json!({"error": {"message": message, "type": kind}});
    json_response(status, serde_json::to_vec(&body).unwrap_or_default())
}

fn sse_response(body: Body) -> Response {
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "text/event-stream")
        .header(header::CACHE_CONTROL, "no-cache")
        .body(body)
        .unwrap()
}

// ── OpenAI surface ──────────────────────────────────────────────────────────

async fn chat_completions(
    State(state): State<SharedState>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let identity = resolve_identity(virtual_key(&headers).as_deref());
    let downgrade = match check_budget(&state.db, &state.config.budget, &identity, now_ms()) {
        BudgetVerdict::Refuse(msg) => return error_json(402, &msg, "budget_exceeded"),
        BudgetVerdict::Downgrade => true,
        BudgetVerdict::Allow => false,
    };
    let model = model_of(&body);
    let Some(route) = resolve_route(&state, &model, &headers, &body).cloned() else {
        return error_json(400, "no route for model", "invalid_request");
    };

    if wants_stream(&body) {
        match route_streaming(
            state.clone(),
            identity,
            Surface::OpenAi,
            &route,
            &model,
            &body,
            downgrade,
        )
        .await
        {
            StreamOutcome::Body(b) => sse_response(b),
            StreamOutcome::Failed => error_json(503, "all backends failed", "proxy_error"),
        }
    } else {
        let r = route_nonstreaming(&state, &identity, "openai", &route, &body, downgrade).await;
        json_response(r.status, r.body)
    }
}

// ── Anthropic surface ───────────────────────────────────────────────────────

async fn anthropic_messages(
    State(state): State<SharedState>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let identity = resolve_identity(virtual_key(&headers).as_deref());
    let downgrade = match check_budget(&state.db, &state.config.budget, &identity, now_ms()) {
        BudgetVerdict::Refuse(msg) => return anthropic_error(402, &msg),
        BudgetVerdict::Downgrade => true,
        BudgetVerdict::Allow => false,
    };
    let mut openai_req = match translate::anthropic_to_openai(&body) {
        Ok(v) => v,
        Err(e) => return anthropic_error(400, &format!("invalid request: {e}")),
    };
    let model = openai_req
        .get("model")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let Some(route) = resolve_route(&state, &model, &headers, &body).cloned() else {
        return anthropic_error(400, "no route for model");
    };

    if wants_stream(&body) {
        openai_req["stream"] = json!(true);
        let openai_body = serde_json::to_vec(&openai_req).unwrap_or_default();
        return match route_streaming(
            state.clone(),
            identity,
            Surface::Anthropic,
            &route,
            &model,
            &openai_body,
            downgrade,
        )
        .await
        {
            StreamOutcome::Body(b) => sse_response(b),
            StreamOutcome::Failed => anthropic_error(503, "all backends failed"),
        };
    }

    let openai_body = serde_json::to_vec(&openai_req).unwrap_or_default();
    let r = route_nonstreaming(
        &state,
        &identity,
        "anthropic",
        &route,
        &openai_body,
        downgrade,
    )
    .await;
    if !(200..300).contains(&r.status) {
        return anthropic_error(r.status, "all backends failed");
    }
    let completion: Value = serde_json::from_slice(&r.body).unwrap_or(Value::Null);
    let resp = openai_completion_to_anthropic(&completion, &model);
    json_response(200, serde_json::to_vec(&resp).unwrap_or_default())
}

/// Builds an Anthropic `message` response from an OpenAI completion.
fn openai_completion_to_anthropic(completion: &Value, model: &str) -> Value {
    let blocks = translate::anthropic_content_blocks(completion);
    let finish = completion
        .get("choices")
        .and_then(Value::as_array)
        .and_then(|c| c.first())
        .and_then(|c| c.get("finish_reason"))
        .and_then(Value::as_str)
        .unwrap_or("stop");
    let usage = completion.get("usage");
    let input = usage
        .and_then(|u| u.get("prompt_tokens"))
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let output = usage
        .and_then(|u| u.get("completion_tokens"))
        .and_then(Value::as_u64)
        .unwrap_or(0);
    json!({
        "id": format!("msg_{}", now_unix()),
        "type": "message",
        "role": "assistant",
        "model": model,
        "content": blocks,
        "stop_reason": translate::map_stop_reason(finish),
        "stop_sequence": Value::Null,
        "usage": {"input_tokens": input, "output_tokens": output},
    })
}

async fn count_tokens(body: Bytes) -> Response {
    let est = transform::estimated_request_tokens(body.len());
    json_response(
        200,
        serde_json::to_vec(&json!({"input_tokens": est})).unwrap_or_default(),
    )
}

fn anthropic_error(status: u16, message: &str) -> Response {
    let body = json!({"type": "error", "error": {"type": "proxy_error", "message": message}});
    json_response(status, serde_json::to_vec(&body).unwrap_or_default())
}

// ── Introspection ───────────────────────────────────────────────────────────

async fn models(State(state): State<SharedState>) -> Response {
    let data: Vec<Value> = state
        .config
        .route_names()
        .into_iter()
        .map(|n| json!({"id": format!("model-proxy/{n}"), "object": "model", "owned_by": "model-proxy"}))
        .collect();
    json_response(
        200,
        serde_json::to_vec(&json!({"object": "list", "data": data})).unwrap_or_default(),
    )
}

async fn healthz(State(state): State<SharedState>) -> Response {
    let now = now_ms();
    let mut backends = serde_json::Map::new();
    for (ident, reason, next_probe_ms, healthy) in state.health.status(now) {
        backends.insert(
            ident,
            json!({
                "status": if healthy { "probing" } else { "cooling" },
                "reason": reason,
                "next_probe_ms": next_probe_ms,
            }),
        );
    }
    json_response(
        200,
        serde_json::to_vec(&json!({"status": "ok", "backends": backends})).unwrap_or_default(),
    )
}

async fn metrics(State(state): State<SharedState>) -> impl IntoResponse {
    let mut out = state.metrics.render();
    let now = now_ms();
    out.push_str(&format!(
        "# HELP model_proxy_uptime_seconds Daemon uptime.\n# TYPE model_proxy_uptime_seconds gauge\nmodel_proxy_uptime_seconds {}\n",
        (now - state.started_ms).max(0) / 1000
    ));
    out.push_str(
        "# HELP model_proxy_backend_exhausted Backend is cooling down.\n# TYPE model_proxy_backend_exhausted gauge\n",
    );
    for (ident, _reason, next_probe_ms, healthy) in state.health.status(now) {
        if !healthy {
            out.push_str(&format!(
                "model_proxy_backend_exhausted{{backend=\"{ident}\"}} 1\n"
            ));
            out.push_str(&format!(
                "model_proxy_backend_next_probe_seconds{{backend=\"{ident}\"}} {}\n",
                (next_probe_ms - now).max(0) / 1000
            ));
        }
    }
    (
        [(
            header::CONTENT_TYPE,
            "text/plain; version=0.0.4; charset=utf-8",
        )],
        out,
    )
}

async fn resolved(State(state): State<SharedState>) -> Response {
    json_response(
        200,
        serde_json::to_vec(&state.resolved_snapshot()).unwrap_or_default(),
    )
}

/// Receives a `[usage]` headroom snapshot from the shell (the proxy never fetches
/// quota itself). Loopback-only in practice; feeds usage-aware lane ordering.
async fn push_usage(State(state): State<SharedState>, body: Bytes) -> Response {
    match serde_json::from_slice::<crate::usage_snapshot::UsagePush>(&body) {
        Ok(push) => {
            state.set_usage_snapshot(push.providers);
            json_response(200, br#"{"status":"ok"}"#.to_vec())
        }
        Err(e) => error_json(
            400,
            &format!("invalid usage snapshot: {e}"),
            "invalid_request",
        ),
    }
}

#[derive(serde::Deserialize)]
struct StatsQuery {
    /// Rollup window in seconds (default 24h).
    since_secs: Option<i64>,
}

/// The JSON stats rollup behind `/stats` and `thegn proxy stats`.
async fn stats(State(state): State<SharedState>, Query(q): Query<StatsQuery>) -> Response {
    let now = now_ms();
    let since_ms = now - q.since_secs.unwrap_or(86_400).max(0) * 1000;
    let (rows, budgets) = match state.db.lock() {
        Ok(g) => (
            g.model_proxy_requests_since(since_ms, 10_000)
                .unwrap_or_default(),
            g.model_proxy_budget_states().unwrap_or_default(),
        ),
        Err(_) => (Vec::new(), Vec::new()),
    };
    let rollup = thegn_core::proxy::stats::rollup(&rows);
    let health: Vec<Value> = state
        .health
        .status(now)
        .into_iter()
        .map(|(ident, reason, next_probe_ms, healthy)| {
            json!({"backend": ident, "reason": reason, "next_probe_ms": next_probe_ms, "healthy": healthy})
        })
        .collect();
    let body = json!({
        "uptime_secs": (now - state.started_ms).max(0) / 1000,
        "since_ms": since_ms,
        "stats": rollup,
        "budgets": budgets,
        "health": health,
        "resolved": state.resolved_snapshot(),
    });
    json_response(200, serde_json::to_vec(&body).unwrap_or_default())
}
