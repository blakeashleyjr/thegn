//! axum HTTP surface. Endpoints mirror the Go proxy: the OpenAI
//! `/v1/chat/completions` (streaming + non-streaming), the Anthropic
//! `/v1/messages` (+ `count_tokens`), plus `/v1/models`, `/health`, `/metrics`,
//! and `/resolved`.

use axum::{
    Router,
    body::{Body, Bytes},
    extract::State,
    http::{HeaderMap, StatusCode, header},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use serde_json::{Value, json};
use superzej_core::proxy::bridge;

use crate::budget::{BudgetVerdict, check_budget, resolve_identity};
use crate::router::{StreamOutcome, Surface, route_nonstreaming, route_streaming};
use crate::shared::now_unix;
use crate::state::SharedState;

/// Builds the axum router with all endpoints bound to `state`.
pub fn app(state: SharedState) -> Router {
    Router::new()
        .route("/v1/chat/completions", post(chat_completions))
        .route("/v1/messages", post(anthropic_messages))
        .route("/v1/messages/count_tokens", post(count_tokens))
        .route("/v1/models", get(models))
        .route("/health", get(health))
        .route("/metrics", get(metrics))
        .route("/resolved", get(resolved))
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
    let identity = resolve_identity(&state.db, virtual_key(&headers).as_deref());
    if let BudgetVerdict::Refuse(msg) = check_budget(&state.db, &identity, state.refuse_on_breach) {
        return error_json(402, &msg, "budget_exceeded");
    }
    let Some(route) = state.config.lookup_route(&model_of(&body)).cloned() else {
        return error_json(400, "no route for model", "invalid_request");
    };

    if wants_stream(&body) {
        let model = model_of(&body);
        match route_streaming(
            state.clone(),
            identity,
            Surface::OpenAi,
            &route,
            &model,
            &body,
        )
        .await
        {
            StreamOutcome::Body(b) => sse_response(b),
            StreamOutcome::Failed => error_json(503, "all backends failed", "proxy_error"),
        }
    } else {
        let r = route_nonstreaming(&state, &identity, "openai", &route, &body).await;
        json_response(r.status, r.body)
    }
}

// ── Anthropic surface ─────────────────────────────────────────────────────────
//
// Milestone 1 supports non-streaming `/v1/messages` by translating the request
// to OpenAI, routing it, and translating the completion back to an Anthropic
// message. Streaming on the Anthropic surface (and the native Claude-Max
// passthrough) are tracked follow-ups.

async fn anthropic_messages(
    State(state): State<SharedState>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let identity = resolve_identity(&state.db, virtual_key(&headers).as_deref());
    if let BudgetVerdict::Refuse(msg) = check_budget(&state.db, &identity, state.refuse_on_breach) {
        return anthropic_error(402, &msg);
    }
    let openai_req = match bridge::anthropic_to_openai(&body) {
        Ok(v) => v,
        Err(e) => return anthropic_error(400, &format!("invalid request: {e}")),
    };
    let model = openai_req
        .get("model")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let Some(route) = state.config.lookup_route(&model).cloned() else {
        return anthropic_error(400, "no route for model");
    };
    let openai_body = serde_json::to_vec(&openai_req).unwrap_or_default();

    // Streaming: translate the upstream OpenAI SSE into live Anthropic events.
    if wants_stream(&body) {
        return match route_streaming(
            state.clone(),
            identity,
            Surface::Anthropic,
            &route,
            &model,
            &openai_body,
        )
        .await
        {
            StreamOutcome::Body(b) => sse_response(b),
            StreamOutcome::Failed => anthropic_error(503, "all backends failed"),
        };
    }

    let r = route_nonstreaming(&state, &identity, "anthropic", &route, &openai_body).await;
    if !(200..300).contains(&r.status) {
        return anthropic_error(r.status, "all backends failed");
    }
    let completion: Value = serde_json::from_slice(&r.body).unwrap_or(Value::Null);
    let resp = openai_completion_to_anthropic(&completion, &model);
    json_response(200, serde_json::to_vec(&resp).unwrap_or_default())
}

/// Builds an Anthropic `message` response from an OpenAI completion.
fn openai_completion_to_anthropic(completion: &Value, model: &str) -> Value {
    let blocks = bridge::anthropic_content_blocks(completion);
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
        "stop_reason": bridge::map_stop_reason(finish),
        "stop_sequence": Value::Null,
        "usage": {"input_tokens": input, "output_tokens": output},
    })
}

async fn count_tokens(body: Bytes) -> Response {
    // Rough estimate (chars/4), matching the proxy's upper-bound heuristic.
    let est = superzej_core::proxy::transform::estimated_request_tokens(body.len());
    json_response(
        200,
        serde_json::to_vec(&json!({"input_tokens": est})).unwrap_or_default(),
    )
}

fn anthropic_error(status: u16, message: &str) -> Response {
    let body = json!({"type": "error", "error": {"type": "proxy_error", "message": message}});
    json_response(status, serde_json::to_vec(&body).unwrap_or_default())
}

// ── Introspection ─────────────────────────────────────────────────────────────

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

async fn health() -> Response {
    json_response(
        200,
        serde_json::to_vec(&json!({"status": "ok"})).unwrap_or_default(),
    )
}

async fn metrics(State(state): State<SharedState>) -> impl IntoResponse {
    (
        [(
            header::CONTENT_TYPE,
            "text/plain; version=0.0.4; charset=utf-8",
        )],
        state.metrics.render(),
    )
}

async fn resolved(State(state): State<SharedState>) -> Response {
    json_response(
        200,
        serde_json::to_vec(&state.resolved_snapshot()).unwrap_or_default(),
    )
}
