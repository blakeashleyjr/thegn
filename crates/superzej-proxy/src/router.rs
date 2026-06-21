//! The cascade router: tries a route's backends in priority order, skipping
//! exhausted/over-context/saturated lanes, classifying each response, cooling
//! down genuine availability failures, and falling through soft failures
//! without a cooldown. Port of `routeRequest`/`attemptBackend` (non-streaming),
//! plus spend attribution + audit logging (group V).

use std::time::Instant;

use serde_json::Value;
use superzej_core::db::ProxyRequestRow;
use superzej_core::proxy::classify::{FailKind, classify_response};
use superzej_core::proxy::cost::{Usage, cost_usd};
use superzej_core::proxy::transform;

use crate::budget::Identity;
use crate::model::{Backend, Route};
use crate::reset::parse_reset_from_body;
use crate::shared::now_ms;
use crate::state::AppState;
use crate::upstream;

/// The result of routing a non-streaming request.
pub struct RouteResult {
    pub status: u16,
    pub body: Vec<u8>,
    /// Identity string of the backend that served it, or `none`.
    pub served_by: String,
}

/// Routes a non-streaming OpenAI request through `route`. `identity` is the
/// resolved caller (for spend attribution); `protocol` is the client-facing
/// surface (`openai`/`anthropic`) for audit rows.
pub async fn route_nonstreaming(
    state: &AppState,
    identity: &Identity,
    protocol: &str,
    route: &Route,
    body: &[u8],
) -> RouteResult {
    let parsed: Value = serde_json::from_slice(body).unwrap_or(Value::Null);
    let has_tools = transform::request_has_tools(&parsed);
    let est_tokens = transform::estimated_request_tokens(body.len());

    let n = route.priority.len();
    for (i, backend) in route.priority.iter().enumerate() {
        let is_last = i + 1 == n;
        let now = now_ms();

        // Skip cooled-down backends.
        if state
            .health
            .is_exhausted(&backend.name, &backend.model, now)
        {
            state
                .metrics
                .inc_fallthrough(&backend.identity(), "skipped_exhausted");
            continue;
        }
        // Skip backends whose context window can't fit the request.
        if transform::exceeds_context_limit(backend.context_limit, est_tokens) {
            state
                .metrics
                .inc_fallthrough(&backend.identity(), "skipped_context");
            continue;
        }
        // Skip tool requests for backends with no tool support flagged via a 0
        // context-limit sentinel is not modelled in M1; tool routing is honored
        // by config ordering. (has_tools kept for the audit/metrics story.)
        let _ = has_tools;

        // Rate-limit admission: a non-tail backend sheds to the next lane when
        // its identity is saturated; the tail backend waits instead of shedding
        // so the whole chain queues on the cheapest lane rather than 503-ing.
        let ident = backend.identity();
        if !state
            .limiter
            .try_acquire(&ident, backend.rate, Instant::now())
        {
            if !is_last {
                state.metrics.inc_fallthrough(&ident, "loadshed");
                continue;
            }
            let wait = state.limiter.reserve(&ident, backend.rate, Instant::now());
            tokio::time::sleep(wait).await;
            let _ = state
                .limiter
                .try_acquire(&ident, backend.rate, Instant::now());
        }
        // In-flight concurrency cap (secondary load signal).
        if !is_last && state.inflight.at_cap(&ident, backend.inflight_cap) {
            state.metrics.inc_fallthrough(&ident, "loadshed_inflight");
            continue;
        }

        let backend_body = apply_transforms(backend, &parsed, body);
        state.inflight.enter(&ident);
        let attempt = upstream::call_backend(&state.client, backend, &backend_body).await;
        state.inflight.leave(&ident);

        let resp = match attempt {
            Ok(r) => r,
            Err(e) => {
                // Transient network error: treat as a soft failure (no cooldown)
                // and fall through. Mirrors the Go network-retry-then-fallthrough.
                tracing::warn!(backend = %ident, error = %e, "backend request error");
                state.metrics.inc_backend_attempt(&ident, "network_error");
                state.metrics.inc_fallthrough(&ident, "network_error");
                continue;
            }
        };

        let (kind, reason) = classify_response(resp.status, &resp.body);
        match kind {
            FailKind::Serve => {
                state.health.record_success(&backend.name, &backend.model);
                state.metrics.inc_backend_attempt(&ident, "ok");
                state.metrics.inc_request(&route.name, &ident, "ok");
                finalize_success(state, identity, protocol, route, backend, &resp.body);
                return RouteResult {
                    status: resp.status,
                    body: resp.body,
                    served_by: ident,
                };
            }
            FailKind::Exhausted => {
                let until = parse_reset_from_body(&resp.body, now);
                state
                    .health
                    .mark_exhausted(&backend.name, &backend.model, &reason, until, now);
                state.metrics.inc_backend_attempt(&ident, "exhausted");
                state.metrics.inc_fallthrough(&ident, "exhausted");
            }
            FailKind::Soft => {
                state.metrics.inc_backend_attempt(&ident, "soft_fail");
                state.metrics.inc_fallthrough(&ident, "soft_fail");
            }
        }
    }

    // Whole chain failed.
    state.metrics.inc_request(&route.name, "none", "all_failed");
    audit_failure(state, identity, protocol, route);
    RouteResult {
        status: 503,
        body: br#"{"error":{"message":"all backends failed","type":"proxy_error"}}"#.to_vec(),
        served_by: "none".to_string(),
    }
}

/// The result of routing a streaming request.
pub enum StreamOutcome {
    /// A live upstream response to relay byte-for-byte (true passthrough).
    Passthrough(reqwest::Response),
    /// SSE bytes synthesized from a buffered completion (Anthropic-surface
    /// backends, which the proxy reaches non-streaming then re-streams).
    Synthesized(Vec<u8>),
    /// The whole chain failed.
    Failed,
}

/// Routes a streaming request. OpenAI-surface backends are relayed live; an
/// Anthropic-surface backend is called buffered and re-streamed as synthesized
/// SSE. The eligibility/skip/classification logic mirrors the non-streaming
/// path. Milestone-1 note: usage/cost for a live passthrough stream is not yet
/// reconciled (the final usage chunk isn't parsed back), so those rows log zero
/// — the 15s heartbeat / idle-timeout relay is a tracked follow-up.
pub async fn route_streaming(
    state: &AppState,
    identity: &Identity,
    protocol: &str,
    route: &Route,
    body: &[u8],
) -> StreamOutcome {
    let parsed: Value = serde_json::from_slice(body).unwrap_or(Value::Null);
    let est_tokens = transform::estimated_request_tokens(body.len());
    let n = route.priority.len();

    for (i, backend) in route.priority.iter().enumerate() {
        let is_last = i + 1 == n;
        let now = now_ms();
        if state
            .health
            .is_exhausted(&backend.name, &backend.model, now)
        {
            state
                .metrics
                .inc_fallthrough(&backend.identity(), "skipped_exhausted");
            continue;
        }
        if transform::exceeds_context_limit(backend.context_limit, est_tokens) {
            state
                .metrics
                .inc_fallthrough(&backend.identity(), "skipped_context");
            continue;
        }
        let ident = backend.identity();
        if !state
            .limiter
            .try_acquire(&ident, backend.rate, Instant::now())
        {
            if !is_last {
                state.metrics.inc_fallthrough(&ident, "loadshed");
                continue;
            }
            let wait = state.limiter.reserve(&ident, backend.rate, Instant::now());
            tokio::time::sleep(wait).await;
            let _ = state
                .limiter
                .try_acquire(&ident, backend.rate, Instant::now());
        }

        let backend_body = apply_transforms(backend, &parsed, body);

        if backend.anthropic {
            // Buffered call → synthesize SSE.
            match upstream::call_backend(&state.client, backend, &backend_body).await {
                Ok(resp) => {
                    let (kind, reason) = classify_response(resp.status, &resp.body);
                    match kind {
                        FailKind::Serve => {
                            state.health.record_success(&backend.name, &backend.model);
                            finalize_success(state, identity, protocol, route, backend, &resp.body);
                            state.metrics.inc_request(&route.name, &ident, "ok");
                            let sse = superzej_core::proxy::bridge::openai_completion_to_stream(
                                &resp.body,
                                crate::shared::now_unix(),
                                "chatcmpl-proxy",
                            );
                            return match sse {
                                Some(bytes) => StreamOutcome::Synthesized(bytes),
                                None => StreamOutcome::Failed,
                            };
                        }
                        FailKind::Exhausted => {
                            let until = parse_reset_from_body(&resp.body, now);
                            state.health.mark_exhausted(
                                &backend.name,
                                &backend.model,
                                &reason,
                                until,
                                now,
                            );
                            state.metrics.inc_fallthrough(&ident, "exhausted");
                        }
                        FailKind::Soft => state.metrics.inc_fallthrough(&ident, "soft_fail"),
                    }
                }
                Err(e) => {
                    tracing::warn!(backend = %ident, error = %e, "anthropic stream backend error");
                    state.metrics.inc_fallthrough(&ident, "network_error");
                }
            }
            continue;
        }

        // OpenAI-surface: relay live.
        match upstream::open_openai_stream(&state.client, backend, &backend_body).await {
            Ok(resp) if resp.status().is_success() => {
                state.health.record_success(&backend.name, &backend.model);
                state.metrics.inc_request(&route.name, &ident, "ok_stream");
                audit_stream_open(state, identity, protocol, route, backend);
                state.set_resolved(&route.name, &ident);
                return StreamOutcome::Passthrough(resp);
            }
            Ok(resp) => {
                let status = resp.status().as_u16();
                let bytes = resp.bytes().await.map(|b| b.to_vec()).unwrap_or_default();
                let (kind, reason) = classify_response(status, &bytes);
                match kind {
                    FailKind::Exhausted => {
                        let until = parse_reset_from_body(&bytes, now);
                        state.health.mark_exhausted(
                            &backend.name,
                            &backend.model,
                            &reason,
                            until,
                            now,
                        );
                        state.metrics.inc_fallthrough(&ident, "exhausted");
                    }
                    _ => state.metrics.inc_fallthrough(&ident, "soft_fail"),
                }
            }
            Err(e) => {
                tracing::warn!(backend = %ident, error = %e, "stream backend error");
                state.metrics.inc_fallthrough(&ident, "network_error");
            }
        }
    }

    state.metrics.inc_request(&route.name, "none", "all_failed");
    StreamOutcome::Failed
}

/// Audit row for a passthrough stream we committed to (usage reconciled later).
fn audit_stream_open(
    state: &AppState,
    identity: &Identity,
    protocol: &str,
    route: &Route,
    backend: &Backend,
) {
    let row = ProxyRequestRow {
        ts_ms: now_ms(),
        protocol: protocol.to_string(),
        route: route.name.clone(),
        virtual_key: identity.virtual_key.clone(),
        agent: identity.agent(),
        worktree: identity.worktree(),
        client_model: format!("model-proxy/{}", route.name),
        backend: backend.name.clone(),
        backend_model: backend.model.clone(),
        outcome: "ok_stream".to_string(),
        ..Default::default()
    };
    if let Ok(db) = state.db.lock() {
        let _ = db.put_proxy_request(&row);
    }
}

/// Applies the per-backend body transforms (min max_tokens, injected defaults)
/// and re-serializes. Falls back to the original bytes on any parse failure.
fn apply_transforms(backend: &Backend, parsed: &Value, original: &[u8]) -> Vec<u8> {
    let mut body = parsed.clone();
    if body.is_object() {
        transform::ensure_max_tokens(&mut body);
        transform::apply_backend_defaults(&mut body, &backend.defaults);
        serde_json::to_vec(&body).unwrap_or_else(|_| original.to_vec())
    } else {
        original.to_vec()
    }
}

/// Extracts usage, computes cost, attributes spend, and writes the audit row for
/// a served response.
fn finalize_success(
    state: &AppState,
    identity: &Identity,
    protocol: &str,
    route: &Route,
    backend: &Backend,
    body: &[u8],
) {
    let usage = parse_usage(body);
    let (cost, source) = cost_usd(
        &state.price_table,
        &backend.name,
        &backend.model,
        usage,
        None,
    );
    state
        .metrics
        .add_tokens(&backend.identity(), "prompt", usage.prompt_tokens);
    state
        .metrics
        .add_tokens(&backend.identity(), "completion", usage.completion_tokens);
    state
        .metrics
        .add_cost(&backend.identity(), source.as_str(), cost);

    let total = usage.total() as i64;
    crate::budget::record_spend(&state.db, identity, total, cost);

    let row = ProxyRequestRow {
        ts_ms: now_ms(),
        protocol: protocol.to_string(),
        route: route.name.clone(),
        virtual_key: identity.virtual_key.clone(),
        agent: identity.agent(),
        worktree: identity.worktree(),
        workspace: None,
        client_model: format!("model-proxy/{}", route.name),
        backend: backend.name.clone(),
        backend_model: backend.model.clone(),
        input_tokens: usage.prompt_tokens as i64,
        output_tokens: usage.completion_tokens as i64,
        cost_usd: cost,
        cost_source: source.as_str().to_string(),
        outcome: "ok".to_string(),
        error_code: None,
    };
    if let Ok(db) = state.db.lock() {
        let _ = db.put_proxy_request(&row);
    }
    state.set_resolved(&route.name, &backend.identity());
}

fn audit_failure(state: &AppState, identity: &Identity, protocol: &str, route: &Route) {
    let row = ProxyRequestRow {
        ts_ms: now_ms(),
        protocol: protocol.to_string(),
        route: route.name.clone(),
        virtual_key: identity.virtual_key.clone(),
        agent: identity.agent(),
        worktree: identity.worktree(),
        client_model: format!("model-proxy/{}", route.name),
        backend: "none".to_string(),
        outcome: "all_failed".to_string(),
        error_code: Some("503".to_string()),
        ..Default::default()
    };
    if let Ok(db) = state.db.lock() {
        let _ = db.put_proxy_request(&row);
    }
}

/// Reads `usage.{prompt_tokens,completion_tokens}` from an OpenAI response body.
fn parse_usage(body: &[u8]) -> Usage {
    let v: Value = match serde_json::from_slice(body) {
        Ok(v) => v,
        Err(_) => return Usage::default(),
    };
    let u = v.get("usage");
    Usage {
        prompt_tokens: u
            .and_then(|u| u.get("prompt_tokens"))
            .and_then(Value::as_u64)
            .unwrap_or(0),
        completion_tokens: u
            .and_then(|u| u.get("completion_tokens"))
            .and_then(Value::as_u64)
            .unwrap_or(0),
    }
}
