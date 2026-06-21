//! The cascade router: tries a route's backends in priority order, skipping
//! exhausted/over-context/saturated lanes, classifying each response, cooling
//! down genuine availability failures, and falling through soft failures
//! without a cooldown. Port of `routeRequest`/`attemptBackend` (non-streaming),
//! plus spend attribution + audit logging (group V).

use std::time::Instant;

use axum::body::Body;
use serde_json::Value;
use superzej_core::db::ProxyRequestRow;
use superzej_core::proxy::classify::{FailKind, classify_response};
use superzej_core::proxy::cost::{Usage, cost_usd};
use superzej_core::proxy::transform;

use crate::anthropic_stream::AnthropicSink;
use crate::budget::Identity;
use crate::model::{Backend, Route};
use crate::relay::{self, OpenAiSink, Peek, RelayStats};
use crate::reset::parse_reset_from_body;
use crate::shared::{now_ms, now_unix};
use crate::state::{AppState, SharedState};
use crate::upstream;

/// The client-facing wire surface a streaming request arrived on.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Surface {
    OpenAi,
    Anthropic,
}

impl Surface {
    /// Audit-row protocol label.
    pub fn protocol(self) -> &'static str {
        match self {
            Surface::OpenAi => "openai",
            Surface::Anthropic => "anthropic",
        }
    }
}

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

/// The result of routing a streaming request: a committed client body or a
/// total failure.
pub enum StreamOutcome {
    Body(Body),
    Failed,
}

/// Routes a streaming request and returns the client SSE body. OpenAI-surface
/// backends are relayed live through [`crate::relay`] (TTFB peek, empty-completion
/// fall-through, heartbeat, idle watchdog, usage reconciliation); Anthropic-surface
/// backends fall back to a buffered call re-streamed as synthesized SSE. `surface`
/// is the *client's* wire protocol — an OpenAI backend behind an Anthropic client
/// is translated chunk-by-chunk by [`AnthropicSink`]. `client_model` is the model
/// the client requested (used to label synthesized Anthropic events).
pub async fn route_streaming(
    state: SharedState,
    identity: Identity,
    surface: Surface,
    route: &Route,
    client_model: &str,
    body: &[u8],
) -> StreamOutcome {
    let parsed: Value = serde_json::from_slice(body).unwrap_or(Value::Null);
    let est_tokens = transform::estimated_request_tokens(body.len());
    let cfg = state.relay_config;
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
            // Rare: an Anthropic-surface backend in streaming mode — buffer then
            // synthesize SSE for the client's surface.
            match upstream::call_backend(&state.client, backend, &backend_body).await {
                Ok(resp) => {
                    let (kind, reason) = classify_response(resp.status, &resp.body);
                    match kind {
                        FailKind::Serve => {
                            state.health.record_success(&backend.name, &backend.model);
                            finalize_success(
                                &state,
                                &identity,
                                surface.protocol(),
                                route,
                                backend,
                                &resp.body,
                            );
                            state.metrics.inc_request(&route.name, &ident, "ok");
                            let sse = match surface {
                                Surface::OpenAi => {
                                    superzej_core::proxy::bridge::openai_completion_to_stream(
                                        &resp.body,
                                        now_unix(),
                                        "chatcmpl-proxy",
                                    )
                                }
                                Surface::Anthropic => Some(synthesize_anthropic_sse(
                                    &resp.body,
                                    client_model,
                                    est_tokens as u64,
                                )),
                            };
                            return match sse {
                                Some(bytes) => StreamOutcome::Body(Body::from(bytes)),
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

        // OpenAI-surface backend: open the stream and relay it live.
        let resp = match upstream::open_openai_stream(&state.client, backend, &backend_body).await {
            Ok(r) if r.status().is_success() => r,
            Ok(r) => {
                let status = r.status().as_u16();
                let bytes = r.bytes().await.map(|b| b.to_vec()).unwrap_or_default();
                let (kind, reason) = classify_response(status, &bytes);
                if kind == FailKind::Exhausted {
                    let until = parse_reset_from_body(&bytes, now);
                    state
                        .health
                        .mark_exhausted(&backend.name, &backend.model, &reason, until, now);
                    state.metrics.inc_fallthrough(&ident, "exhausted");
                } else {
                    state.metrics.inc_fallthrough(&ident, "soft_fail");
                }
                continue;
            }
            Err(e) => {
                tracing::warn!(backend = %ident, error = %e, "stream backend error");
                state.metrics.inc_fallthrough(&ident, "network_error");
                continue;
            }
        };

        // Peek before committing: only a stream with usable output is returned to
        // the client; an empty/timed-out stream soft-cools the backend and falls
        // through. The peek is generic over the sink chosen by the client surface.
        let commit = match surface {
            Surface::OpenAi => match relay::peek(resp, OpenAiSink::default(), cfg).await {
                Peek::Commit {
                    prefix_out,
                    rest,
                    sink,
                } => {
                    let fin = finalize_closure(
                        &state,
                        &identity,
                        surface.protocol(),
                        &route.name,
                        backend,
                        &ident,
                    );
                    Some(relay::spawn_relay(prefix_out, rest, sink, cfg, fin))
                }
                other => {
                    note_stream_fallthrough(&state, backend, &ident, &other);
                    None
                }
            },
            Surface::Anthropic => {
                let sink = AnthropicSink::new(
                    format!("msg_{}", now_unix()),
                    client_model,
                    est_tokens as u64,
                );
                match relay::peek(resp, sink, cfg).await {
                    Peek::Commit {
                        prefix_out,
                        rest,
                        sink,
                    } => {
                        let fin = finalize_closure(
                            &state,
                            &identity,
                            surface.protocol(),
                            &route.name,
                            backend,
                            &ident,
                        );
                        Some(relay::spawn_relay(prefix_out, rest, sink, cfg, fin))
                    }
                    other => {
                        note_stream_fallthrough(&state, backend, &ident, &other);
                        None
                    }
                }
            }
        };

        if let Some(body) = commit {
            state.health.record_success(&backend.name, &backend.model);
            state.set_resolved(&route.name, &ident);
            state.metrics.inc_request(&route.name, &ident, "ok_stream");
            return StreamOutcome::Body(body);
        }
    }

    state.metrics.inc_request(&route.name, "none", "all_failed");
    StreamOutcome::Failed
}

/// Records the right health/metrics signal for a pre-commit stream that did not
/// yield usable output. Empty/timeout park the backend briefly (soft cooldown);
/// a transport error just falls through.
fn note_stream_fallthrough<S: relay::StreamSink>(
    state: &AppState,
    backend: &Backend,
    ident: &str,
    peek: &Peek<S>,
) {
    let now = now_ms();
    let base = std::time::Duration::from_millis(100);
    match peek {
        Peek::Empty => {
            state.health.mark_soft_cooldown(
                &backend.name,
                &backend.model,
                "stream empty completion",
                base,
                now,
            );
            state.metrics.inc_fallthrough(ident, "empty");
        }
        Peek::TimedOut => {
            state.health.mark_soft_cooldown(
                &backend.name,
                &backend.model,
                "stream first byte timeout",
                base,
                now,
            );
            state.metrics.inc_fallthrough(ident, "ttfb");
        }
        Peek::Errored(e) => {
            tracing::warn!(backend = %ident, error = %e, "stream pre-commit error");
            state.metrics.inc_fallthrough(ident, "network_error");
        }
        Peek::Commit { .. } => {}
    }
}

/// Builds the finalize callback the relay task runs once a committed stream
/// completes: reconcile usage → cost → spend → audit row + metrics.
fn finalize_closure(
    state: &SharedState,
    identity: &Identity,
    protocol: &'static str,
    route_name: &str,
    backend: &Backend,
    ident: &str,
) -> impl FnOnce(RelayStats) + Send + 'static {
    let state = state.clone();
    let identity = identity.clone();
    let route_name = route_name.to_string();
    let bname = backend.name.clone();
    let bmodel = backend.model.clone();
    let ident = ident.to_string();
    move |stats: RelayStats| {
        let usage = stats.usage;
        let (cost, source) = cost_usd(&state.price_table, &bname, &bmodel, usage, None);
        state
            .metrics
            .add_tokens(&ident, "prompt", usage.prompt_tokens);
        state
            .metrics
            .add_tokens(&ident, "completion", usage.completion_tokens);
        state.metrics.add_cost(&ident, source.as_str(), cost);
        crate::budget::record_spend(&state.db, &identity, usage.total() as i64, cost);
        let row = ProxyRequestRow {
            ts_ms: now_ms(),
            protocol: protocol.to_string(),
            route: route_name.clone(),
            virtual_key: identity.virtual_key.clone(),
            agent: identity.agent(),
            worktree: identity.worktree(),
            workspace: None,
            client_model: format!("model-proxy/{route_name}"),
            backend: bname.clone(),
            backend_model: bmodel.clone(),
            input_tokens: usage.prompt_tokens as i64,
            output_tokens: usage.completion_tokens as i64,
            cost_usd: cost,
            cost_source: source.as_str().to_string(),
            outcome: "ok_stream".to_string(),
            error_code: None,
        };
        if let Ok(db) = state.db.lock() {
            let _ = db.put_proxy_request(&row);
        }
    }
}

/// Synthesizes an Anthropic SSE event stream from a buffered OpenAI completion by
/// feeding a one-shot OpenAI SSE rendering through the incremental translator.
fn synthesize_anthropic_sse(completion: &[u8], client_model: &str, input_est: u64) -> Vec<u8> {
    use crate::relay::StreamSink;
    let Some(openai_sse) = superzej_core::proxy::bridge::openai_completion_to_stream(
        completion,
        now_unix(),
        "chatcmpl-proxy",
    ) else {
        return Vec::new();
    };
    let mut sink = AnthropicSink::new(format!("msg_{}", now_unix()), client_model, input_est);
    let mut out = Vec::new();
    for line in openai_sse.split_inclusive(|&b| b == b'\n') {
        out.extend_from_slice(&sink.process(line));
    }
    out.extend_from_slice(&sink.finish());
    out
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
