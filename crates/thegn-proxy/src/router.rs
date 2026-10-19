//! The cascade router: tries a route's backends in priority order, skipping
//! exhausted/over-context/saturated lanes, classifying each response, cooling
//! down genuine availability failures, and falling through soft failures without
//! a cooldown. Restored from the pre-alpha proxy, adapted to the `[model_proxy]`
//! registry, the upstream seam, the fresh accounting tables, cost-aware ordering,
//! optional usage-aware ordering, and metadata-only audit rows.

use std::collections::HashSet;
use std::sync::Arc;
use std::time::Instant;

use axum::body::Body;
use futures::StreamExt;
use serde_json::Value;
use thegn_core::config_model_proxy::RoutingStrategy;
use thegn_core::proxy::classify::{FailKind, classify_response};
use thegn_core::proxy::cost::{PriceTable, Usage, cost_usd};
use thegn_core::proxy::creds::provider_base;
use thegn_core::proxy::transform;
use thegn_core::proxy::usage_order::usage_order;
use thegn_core::store::{ModelProxyRequestRow, ModelProxyStore};
use thegn_core::usage::{DEFAULT_CRIT_PERCENT, DEFAULT_WARN_PERCENT};

use crate::anthropic_stream::AnthropicSink;
use crate::budget::Identity;
use crate::headers::{header_cost, retry_after_ms};
use crate::model::{Backend, ProxyConfig, Route};
use crate::relay::{self, OpenAiSink, Peek, RelayStats, parse_openai_usage};
use crate::reset::parse_reset_from_body;
use crate::shared::{now_ms, now_unix};
use crate::state::{AppState, SharedState};
use crate::upstream::{DispatchDeadlines, DispatchTimeout};

/// The client-facing wire surface a streaming request arrived on.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Surface {
    OpenAi,
    Anthropic,
}

fn dispatch_timeout_reason(error: &anyhow::Error) -> Option<&'static str> {
    error
        .downcast_ref::<DispatchTimeout>()
        .map(|timeout| match timeout {
            DispatchTimeout::FirstByte => "first_byte_timeout",
            DispatchTimeout::Idle => "idle_timeout",
        })
}

fn is_connect_error(error: &anyhow::Error) -> bool {
    error.chain().any(|cause| {
        cause
            .downcast_ref::<reqwest::Error>()
            .is_some_and(|request| request.is_connect())
    })
}

fn is_connect_timeout(error: &anyhow::Error) -> bool {
    error.chain().any(|cause| {
        cause
            .downcast_ref::<reqwest::Error>()
            .is_some_and(|request| request.is_connect() && request.is_timeout())
    })
}

fn should_refund_connect_failure(is_connect: bool, took_token: bool) -> bool {
    is_connect && took_token
}

fn refund_connect_failure(state: &SharedState, ident: &str, is_connect: bool, took_token: bool) {
    if should_refund_connect_failure(is_connect, took_token) {
        state.limiter.refund(ident, Instant::now());
    }
}

fn classify_dispatch_error(
    state: &SharedState,
    ident: &str,
    error: &anyhow::Error,
    took_token: bool,
) -> &'static str {
    if let Some(reason) = dispatch_timeout_reason(error) {
        return reason;
    }
    refund_connect_failure(state, ident, is_connect_error(error), took_token);
    if is_connect_timeout(error) {
        "connect_timeout"
    } else {
        "network_error"
    }
}

async fn read_error_body(response: reqwest::Response, cap: usize) -> anyhow::Result<Vec<u8>> {
    let mut stream = response.bytes_stream();
    let mut body = Vec::new();
    while body.len() < cap {
        match stream.next().await {
            Some(Ok(chunk)) => {
                let remaining = cap - body.len();
                body.extend_from_slice(&chunk[..chunk.len().min(remaining)]);
            }
            Some(Err(error)) => return Err(error.into()),
            None => break,
        }
    }
    Ok(body)
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

/// Returns the order in which a route's backend lanes should be attempted this
/// request, composing slot ordering (route strategy) with within-slot key
/// ordering (the multi-key pool). `force_cost` overrides the strategy with
/// cheapest-first (used by budget downgrade).
fn ordered_priority(
    route: &Route,
    rand_start: usize,
    prices: &PriceTable,
    force_cost: bool,
) -> Vec<usize> {
    let lanes = &route.priority;

    // 1. Partition into slots, each already key-ordered.
    let mut slots: Vec<Vec<usize>> = Vec::new();
    let mut i = 0;
    while i < lanes.len() {
        match &lanes[i].pool {
            Some(pool) => {
                let mut k = 1;
                while i + k < lanes.len()
                    && lanes[i + k]
                        .pool
                        .as_ref()
                        .is_some_and(|p| Arc::ptr_eq(p, pool))
                {
                    k += 1;
                }
                slots.push(
                    pool.order(k, rand_start)
                        .into_iter()
                        .map(|off| i + off)
                        .collect(),
                );
                i += k;
            }
            None => {
                slots.push(vec![i]);
                i += 1;
            }
        }
    }

    // 2. Order the slots by the route strategy (or cost, when forced).
    let strategy = if force_cost {
        RoutingStrategy::CostAware
    } else {
        route.strategy
    };
    let slot_order: Vec<usize> = match strategy {
        RoutingStrategy::Sequential => (0..slots.len()).collect(),
        RoutingStrategy::LoadBalanced => match &route.order_pool {
            Some(pool) => pool.order(slots.len(), rand_start),
            None => (0..slots.len()).collect(),
        },
        RoutingStrategy::CostAware => {
            // Cheapest backend first (stable, so equal-cost ties keep natural
            // order). Subscription/free lanes price to 0 → tried first.
            let mut idx: Vec<usize> = (0..slots.len()).collect();
            idx.sort_by(|&a, &b| {
                slot_cost(prices, &lanes[slots[a][0]])
                    .total_cmp(&slot_cost(prices, &lanes[slots[b][0]]))
            });
            idx
        }
    };

    // 3. Flatten slots (in their chosen order) back into a lane-index list.
    slot_order
        .into_iter()
        .flat_map(|s| slots[s].clone())
        .collect()
}

/// A representative per-request cost for a lane, ordering `CostAware` slots.
fn slot_cost(prices: &PriceTable, lane: &Backend) -> f64 {
    let nominal = Usage {
        prompt_tokens: 1_000_000,
        completion_tokens: 1_000_000,
        ..Default::default()
    };
    cost_usd(prices, &lane.name, &lane.model, nominal, None).0
}

/// A per-request seed for the `Random` lane strategy.
fn rand_start() -> usize {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos() as usize)
        .unwrap_or(0)
}

/// The ordered lane list for one attempt pass over `route`, with upstream-account
/// pinning and (optionally) usage-aware headroom ordering applied.
fn attempt_lanes(
    state: &AppState,
    route: &Route,
    identity: &Identity,
    force_cost: bool,
) -> Vec<Backend> {
    let order = ordered_priority(route, rand_start(), &state.config.price_table, force_cost);
    let mut lanes: Vec<Backend> = order
        .into_iter()
        .map(|i| route.priority[i].clone())
        .collect();

    // Usage-aware ordering: deprioritize/skip lanes whose account is near its
    // window cap (never all — degrades to plain order when every lane is spent).
    if state.config.usage_aware {
        let used: Vec<Option<f32>> = lanes
            .iter()
            .map(|b| state.provider_used_percent(provider_base(&b.name)))
            .collect();
        let survivors = usage_order(&used, DEFAULT_WARN_PERCENT, DEFAULT_CRIT_PERCENT);
        lanes = survivors.into_iter().map(|i| lanes[i].clone()).collect();
    }

    // Upstream-account pinning: the bound provider's lanes lead (stable).
    if let Some(up) = identity.upstream.as_deref() {
        lanes.sort_by_key(|b| provider_base(&b.name) != up);
    }
    lanes
}

/// The deduped union of every OTHER route's backends (skipping tried identities),
/// for the last-resort tier.
fn last_resort_lanes(
    config: &ProxyConfig,
    route_name: &str,
    tried: &HashSet<String>,
) -> Vec<Backend> {
    let mut seen: HashSet<String> = tried.clone();
    let mut out = Vec::new();
    for r in config.routes.iter().filter(|r| r.name != route_name) {
        for b in &r.priority {
            if seen.insert(b.identity()) {
                out.push(b.clone());
            }
        }
    }
    out
}

/// The result of routing a non-streaming request.
pub struct RouteResult {
    pub status: u16,
    pub body: Vec<u8>,
    pub served_by: String,
}

/// Routes a non-streaming OpenAI request through `route`. `downgrade` forces
/// cheapest-first ordering (budget breach on a `downgrade`-mode scope).
pub async fn route_nonstreaming(
    state: &AppState,
    identity: &Identity,
    protocol: &str,
    route: &Route,
    body: &[u8],
    downgrade: bool,
) -> RouteResult {
    let started = Instant::now();
    let parsed: Value = serde_json::from_slice(body).unwrap_or(Value::Null);
    let est_tokens = transform::estimated_request_tokens(body.len());
    let mut tried: HashSet<String> = HashSet::new();

    let lanes = attempt_lanes(state, route, identity, downgrade);
    if let Some(r) = try_chain(
        state, identity, protocol, route, &lanes, &parsed, body, est_tokens, started, &mut tried,
    )
    .await
    {
        return r;
    }
    if state.config.last_resort {
        let fallback = last_resort_lanes(&state.config, &route.name, &tried);
        if !fallback.is_empty() {
            tracing::info!(route = %route.name, lanes = fallback.len(), "last-resort tier engaged");
            if let Some(r) = try_chain(
                state, identity, protocol, route, &fallback, &parsed, body, est_tokens, started,
                &mut tried,
            )
            .await
            {
                return r;
            }
        }
    }

    state.metrics.inc_request(&route.name, "none", "all_failed");
    audit_failure(state, identity, protocol, route, started);
    RouteResult {
        status: 503,
        body: br#"{"error":{"message":"all backends failed","type":"proxy_error"}}"#.to_vec(),
        served_by: "none".to_string(),
    }
}

/// One ordered pass over `lanes` for a non-streaming request.
#[allow(clippy::too_many_arguments)]
async fn try_chain(
    state: &AppState,
    identity: &Identity,
    protocol: &str,
    route: &Route,
    lanes: &[Backend],
    parsed: &Value,
    body: &[u8],
    est_tokens: usize,
    started: Instant,
    tried: &mut HashSet<String>,
) -> Option<RouteResult> {
    let n = lanes.len();
    for (pos, backend) in lanes.iter().enumerate() {
        let attempt_started = Instant::now();
        let is_last = pos + 1 == n;
        let now = now_ms();
        let ident = backend.identity();
        tried.insert(ident.clone());

        if state.health.is_exhausted(&ident, &backend.model, now) {
            state.metrics.inc_fallthrough(&ident, "skipped_exhausted");
            continue;
        }
        if transform::exceeds_context_limit(backend.context_limit, est_tokens) {
            state.metrics.inc_fallthrough(&ident, "skipped_context");
            continue;
        }

        let Some(_permit) = state.inflight.try_acquire(&ident, backend.inflight_cap) else {
            state.metrics.inc_fallthrough(&ident, "loadshed_inflight");
            continue;
        };

        let mut took_token = state
            .limiter
            .try_acquire(&ident, backend.rate, Instant::now());
        if !took_token {
            if !is_last {
                state.metrics.inc_fallthrough(&ident, "loadshed");
                continue;
            }
            let wait = state.limiter.reserve(&ident, backend.rate, Instant::now());
            if wait
                > state
                    .deadlines
                    .buffered_total
                    .saturating_sub(attempt_started.elapsed())
            {
                state
                    .metrics
                    .inc_fallthrough(&ident, "rate_wait_exceeds_deadline");
                continue;
            }
            tokio::time::sleep(wait).await;
            // The reservation can be lost to a concurrent acquire; as the last
            // backend, proceed after waiting either way.
            took_token = state
                .limiter
                .try_acquire(&ident, backend.rate, Instant::now());
            if !took_token {
                tracing::debug!(backend = %ident, "rate reservation lost to a concurrent acquire; proceeding as last lane");
            }
        }
        let backend_body = apply_transforms(backend, parsed, body);
        let attempt = tokio::time::timeout(
            state
                .deadlines
                .buffered_total
                .saturating_sub(attempt_started.elapsed()),
            state.upstreams.for_backend(backend).dispatch(
                &state.client,
                backend,
                &backend_body,
                DispatchDeadlines {
                    first_byte: state.config.relay.first_byte,
                    idle: state.config.relay.idle,
                },
            ),
        )
        .await;

        let resp = match attempt {
            Ok(Ok(r)) => r,
            Ok(Err(e)) => {
                tracing::warn!(backend = %ident, error = %e, "backend request error");
                let reason = classify_dispatch_error(state, &ident, &e, took_token);
                state.metrics.inc_backend_attempt(&ident, reason);
                state.metrics.inc_fallthrough(&ident, reason);
                continue;
            }
            Err(_) => {
                tracing::warn!(backend = %ident, "backend request exceeded total deadline");
                state.metrics.inc_backend_attempt(&ident, "total_timeout");
                state.metrics.inc_fallthrough(&ident, "total_timeout");
                continue;
            }
        };

        let (kind, reason) = classify_response(resp.status, &resp.body);
        match kind {
            FailKind::Serve => {
                state.health.record_success(&ident, &backend.model);
                state.metrics.inc_backend_attempt(&ident, "ok");
                state.metrics.inc_request(&route.name, &ident, "ok");
                let duration_ms =
                    thegn_core::time_policy::saturating_i64(started.elapsed().as_millis());
                state.metrics.observe_duration(duration_ms);
                finalize_success(
                    state,
                    identity,
                    protocol,
                    route,
                    backend,
                    &resp.body,
                    header_cost(&resp.headers),
                    duration_ms,
                    None,
                );
                return Some(RouteResult {
                    status: resp.status,
                    body: resp.body,
                    served_by: ident,
                });
            }
            FailKind::Exhausted => {
                let until = parse_reset_from_body(&resp.body, now)
                    .or_else(|| retry_after_ms(&resp.headers, now));
                state
                    .health
                    .mark_exhausted(&ident, &backend.model, &reason, until, now);
                state.metrics.inc_backend_attempt(&ident, "exhausted");
                state.metrics.inc_fallthrough(&ident, "exhausted");
            }
            FailKind::Soft => {
                state.metrics.inc_backend_attempt(&ident, "soft_fail");
                state.metrics.inc_fallthrough(&ident, "soft_fail");
            }
        }
    }
    None
}

/// The result of routing a streaming request.
pub enum StreamOutcome {
    Body(Body),
    Failed,
}

/// Routes a streaming request and returns the client SSE body.
pub async fn route_streaming(
    state: SharedState,
    identity: Identity,
    surface: Surface,
    route: &Route,
    client_model: &str,
    body: &[u8],
    downgrade: bool,
) -> StreamOutcome {
    let started = Instant::now();
    let mut tried: HashSet<String> = HashSet::new();
    let lanes = attempt_lanes(&state, route, &identity, downgrade);
    if let Some(b) = try_stream_chain(
        &state,
        &identity,
        surface,
        route,
        &lanes,
        client_model,
        body,
        started,
        &mut tried,
    )
    .await
    {
        return StreamOutcome::Body(b);
    }
    if state.config.last_resort {
        let fallback = last_resort_lanes(&state.config, &route.name, &tried);
        if !fallback.is_empty() {
            tracing::info!(route = %route.name, lanes = fallback.len(), "last-resort tier engaged (stream)");
            if let Some(b) = try_stream_chain(
                &state,
                &identity,
                surface,
                route,
                &fallback,
                client_model,
                body,
                started,
                &mut tried,
            )
            .await
            {
                return StreamOutcome::Body(b);
            }
        }
    }
    state.metrics.inc_request(&route.name, "none", "all_failed");
    StreamOutcome::Failed
}

/// One ordered pass over `lanes` for a streaming request.
#[allow(clippy::too_many_arguments)]
async fn try_stream_chain(
    state: &SharedState,
    identity: &Identity,
    surface: Surface,
    route: &Route,
    lanes: &[Backend],
    client_model: &str,
    body: &[u8],
    started: Instant,
    tried: &mut HashSet<String>,
) -> Option<Body> {
    let parsed: Value = serde_json::from_slice(body).unwrap_or(Value::Null);
    let est_tokens = transform::estimated_request_tokens(body.len());
    let cfg = state.config.relay;
    let n = lanes.len();

    for (pos, backend) in lanes.iter().enumerate() {
        let attempt_started = Instant::now();
        let is_last = pos + 1 == n;
        let now = now_ms();
        let ident = backend.identity();
        tried.insert(ident.clone());
        if state.health.is_exhausted(&ident, &backend.model, now) {
            state.metrics.inc_fallthrough(&ident, "skipped_exhausted");
            continue;
        }
        if transform::exceeds_context_limit(backend.context_limit, est_tokens) {
            state.metrics.inc_fallthrough(&ident, "skipped_context");
            continue;
        }
        let Some(permit) = state.inflight.try_acquire(&ident, backend.inflight_cap) else {
            state.metrics.inc_fallthrough(&ident, "loadshed_inflight");
            continue;
        };

        let mut took_token = state
            .limiter
            .try_acquire(&ident, backend.rate, Instant::now());
        if !took_token {
            if !is_last {
                state.metrics.inc_fallthrough(&ident, "loadshed");
                continue;
            }
            let wait = state.limiter.reserve(&ident, backend.rate, Instant::now());
            if wait > cfg.first_byte.saturating_sub(attempt_started.elapsed()) {
                state
                    .metrics
                    .inc_fallthrough(&ident, "rate_wait_exceeds_deadline");
                continue;
            }
            tokio::time::sleep(wait).await;
            // The reservation can be lost to a concurrent acquire; as the last
            // backend, proceed after waiting either way.
            took_token = state
                .limiter
                .try_acquire(&ident, backend.rate, Instant::now());
            if !took_token {
                tracing::debug!(backend = %ident, "rate reservation lost to a concurrent acquire; proceeding as last lane");
            }
        }

        let backend_body = apply_transforms(backend, &parsed, body);

        if backend.is_anthropic() {
            // Anthropic-surface backend in streaming mode: buffer then synthesize.
            let attempt = tokio::time::timeout(
                state
                    .deadlines
                    .buffered_total
                    .saturating_sub(attempt_started.elapsed()),
                state.upstreams.for_backend(backend).dispatch(
                    &state.client,
                    backend,
                    &backend_body,
                    DispatchDeadlines {
                        first_byte: cfg.first_byte,
                        idle: cfg.idle,
                    },
                ),
            )
            .await;
            match attempt {
                Ok(Ok(resp)) => {
                    let (kind, reason) = classify_response(resp.status, &resp.body);
                    match kind {
                        FailKind::Serve => {
                            state.health.record_success(&ident, &backend.model);
                            let duration_ms = thegn_core::time_policy::saturating_i64(
                                started.elapsed().as_millis(),
                            );
                            state.metrics.observe_duration(duration_ms);
                            finalize_success(
                                state,
                                identity,
                                surface.protocol(),
                                route,
                                backend,
                                &resp.body,
                                header_cost(&resp.headers),
                                duration_ms,
                                None,
                            );
                            state.metrics.inc_request(&route.name, &ident, "ok");
                            let sse = match surface {
                                Surface::OpenAi => {
                                    thegn_core::proxy::translate::openai_completion_to_stream(
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
                            drop(permit);
                            return sse.map(Body::from);
                        }
                        FailKind::Exhausted => {
                            let until = parse_reset_from_body(&resp.body, now)
                                .or_else(|| retry_after_ms(&resp.headers, now));
                            state.health.mark_exhausted(
                                &ident,
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
                Ok(Err(e)) => {
                    tracing::warn!(backend = %ident, error = %e, "anthropic stream backend error");
                    let reason = classify_dispatch_error(state, &ident, &e, took_token);
                    state.metrics.inc_backend_attempt(&ident, reason);
                    state.metrics.inc_fallthrough(&ident, reason);
                }
                Err(_) => {
                    state.metrics.inc_backend_attempt(&ident, "total_timeout");
                    state.metrics.inc_fallthrough(&ident, "total_timeout");
                }
            }
            continue;
        }

        // OpenAI-surface backend: open the stream and relay it live.
        let first_byte_deadline =
            tokio::time::Instant::now() + cfg.first_byte.saturating_sub(attempt_started.elapsed());
        let opened = tokio::time::timeout_at(
            first_byte_deadline,
            state
                .upstreams
                .for_backend(backend)
                .open_stream(&state.client, backend, &backend_body),
        )
        .await;
        let resp = match opened {
            Ok(Ok(r)) if r.status().is_success() => r,
            Ok(Ok(r)) => {
                let status = r.status().as_u16();
                let headers = r.headers().clone();
                let (bytes, read_error) =
                    match tokio::time::timeout(cfg.idle, read_error_body(r, 64 * 1024)).await {
                        Ok(Ok(bytes)) => (bytes, None),
                        Ok(Err(error)) => (Vec::new(), Some(("network_error", error))),
                        Err(_) => (
                            Vec::new(),
                            Some(("idle_timeout", anyhow::anyhow!("error body idle timeout"))),
                        ),
                    };
                if let Some((read_reason, error)) = read_error {
                    tracing::debug!(backend = %ident, %error, "upstream error body read failed");
                    state.metrics.inc_backend_attempt(&ident, read_reason);
                    state.metrics.inc_fallthrough(&ident, read_reason);
                }
                let (kind, reason) = classify_response(status, &bytes);
                if kind == FailKind::Exhausted {
                    let until = parse_reset_from_body(&bytes, now)
                        .or_else(|| retry_after_ms(&headers, now));
                    state
                        .health
                        .mark_exhausted(&ident, &backend.model, &reason, until, now);
                    state.metrics.inc_fallthrough(&ident, "exhausted");
                } else {
                    state.metrics.inc_fallthrough(&ident, "soft_fail");
                }
                continue;
            }
            Ok(Err(e)) => {
                tracing::warn!(backend = %ident, error = %e, "stream backend error");
                let reason = classify_dispatch_error(state, &ident, &e, took_token);
                state.metrics.inc_backend_attempt(&ident, reason);
                state.metrics.inc_fallthrough(&ident, reason);
                continue;
            }
            Err(_) => {
                state
                    .metrics
                    .inc_backend_attempt(&ident, "first_byte_timeout");
                state.metrics.inc_fallthrough(&ident, "first_byte_timeout");
                continue;
            }
        };
        let upstream_cost = header_cost(resp.headers());
        let peek_cfg = relay::RelayConfig {
            first_byte: first_byte_deadline.saturating_duration_since(tokio::time::Instant::now()),
            ..cfg
        };

        let commit = match surface {
            Surface::OpenAi => match relay::peek(resp, OpenAiSink::default(), peek_cfg).await {
                Peek::Commit {
                    prefix_out,
                    rest,
                    sink,
                } => {
                    let fin = finalize_closure(
                        state,
                        identity,
                        surface.protocol(),
                        &route.name,
                        backend,
                        &ident,
                        upstream_cost,
                        started,
                    );
                    Some(relay::spawn_relay(
                        prefix_out,
                        rest,
                        sink,
                        cfg,
                        state.deadlines.stream_total,
                        fin,
                    ))
                }
                other => {
                    note_stream_fallthrough(state, backend, &ident, &other);
                    None
                }
            },
            Surface::Anthropic => {
                let sink = AnthropicSink::new(
                    format!("msg_{}", now_unix()),
                    client_model,
                    est_tokens as u64,
                );
                match relay::peek(resp, sink, peek_cfg).await {
                    Peek::Commit {
                        prefix_out,
                        rest,
                        sink,
                    } => {
                        let fin = finalize_closure(
                            state,
                            identity,
                            surface.protocol(),
                            &route.name,
                            backend,
                            &ident,
                            upstream_cost,
                            started,
                        );
                        Some(relay::spawn_relay(
                            prefix_out,
                            rest,
                            sink,
                            cfg,
                            state.deadlines.stream_total,
                            fin,
                        ))
                    }
                    other => {
                        note_stream_fallthrough(state, backend, &ident, &other);
                        None
                    }
                }
            }
        };

        if let Some(body) = commit {
            state.health.record_success(&ident, &backend.model);
            state.set_resolved(&route.name, &ident);
            state.metrics.inc_request(&route.name, &ident, "ok_stream");
            return Some(hold_inflight_permit(body, permit));
        }
    }
    None
}

/// Keeps a backend-attempt permit alive until the client finishes or drops its
/// response body, including when a streamed response is cancelled.
fn hold_inflight_permit(body: Body, permit: thegn_core::proxy::ratelimit::InflightPermit) -> Body {
    let stream = futures::stream::unfold(
        (body.into_data_stream(), permit),
        |(mut body, permit)| async move { body.next().await.map(|chunk| (chunk, (body, permit))) },
    );
    Body::from_stream(stream)
}

/// Records the right health/metrics signal for a pre-commit stream that did not
/// yield usable output.
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
                ident,
                &backend.model,
                "stream empty completion",
                base,
                now,
            );
            state.metrics.inc_fallthrough(ident, "empty");
        }
        Peek::TimedOut => {
            state.health.mark_soft_cooldown(
                ident,
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
#[allow(clippy::too_many_arguments)]
fn finalize_closure(
    state: &SharedState,
    identity: &Identity,
    protocol: &'static str,
    route_name: &str,
    backend: &Backend,
    ident: &str,
    upstream_cost: Option<f64>,
    started: Instant,
) -> impl FnOnce(RelayStats) + Send + 'static {
    let state = state.clone();
    let identity = identity.clone();
    let route_name = route_name.to_string();
    let bname = backend.name.clone();
    let bmodel = backend.model.clone();
    let ident = ident.to_string();
    let ttfb_ms = thegn_core::time_policy::saturating_i64(started.elapsed().as_millis());
    move |stats: RelayStats| {
        let usage = stats.usage;
        let duration_ms = thegn_core::time_policy::saturating_i64(started.elapsed().as_millis());
        let (cost, source) = cost_usd(
            &state.config.price_table,
            &bname,
            &bmodel,
            usage,
            upstream_cost,
        );
        state
            .metrics
            .add_tokens(&ident, "prompt", usage.prompt_tokens);
        state
            .metrics
            .add_tokens(&ident, "completion", usage.completion_tokens);
        state.metrics.add_cost(&ident, source.as_str(), cost);
        state.metrics.observe_duration(duration_ms);
        crate::budget::record_spend(
            &state.db,
            &state.config.budget,
            &identity,
            usage.total() as i64,
            cost,
            now_ms(),
        );
        let row = build_row(
            &identity,
            protocol,
            &route_name,
            &bname,
            &bmodel,
            usage,
            cost,
            source.as_str(),
            "ok_stream",
            duration_ms,
            Some(ttfb_ms),
        );
        put_audit_row(&state, &row);
    }
}

/// Synthesizes an Anthropic SSE event stream from a buffered OpenAI completion.
fn synthesize_anthropic_sse(completion: &[u8], client_model: &str, input_est: u64) -> Vec<u8> {
    use crate::relay::StreamSink;
    let Some(openai_sse) = thegn_core::proxy::translate::openai_completion_to_stream(
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

/// Applies the per-backend body transforms (model rewrite, min max_tokens,
/// injected defaults) and re-serializes. Falls back to the original bytes on any
/// parse failure. (Token-reduction/compression is out of scope for the
/// resurrection.)
fn apply_transforms(backend: &Backend, parsed: &Value, original: &[u8]) -> Vec<u8> {
    let mut body = parsed.clone();
    if body.is_object() {
        if !backend.model.is_empty() {
            body["model"] = Value::String(backend.model.clone());
        }
        transform::ensure_max_tokens(&mut body);
        transform::apply_backend_defaults(&mut body, &backend.defaults);
        serde_json::to_vec(&body).unwrap_or_else(|_| original.to_vec())
    } else {
        original.to_vec()
    }
}

/// Builds a metadata-only audit row (no message content).
#[allow(clippy::too_many_arguments)]
fn build_row(
    identity: &Identity,
    protocol: &str,
    route_name: &str,
    backend_name: &str,
    backend_model: &str,
    usage: Usage,
    cost: f64,
    cost_source: &str,
    outcome: &str,
    duration_ms: i64,
    ttfb_ms: Option<i64>,
) -> ModelProxyRequestRow {
    ModelProxyRequestRow {
        ts_ms: now_ms(),
        protocol: protocol.to_string(),
        route: route_name.to_string(),
        agent: identity.agent(),
        worktree: identity.worktree(),
        workspace: identity.workspace_label(),
        client_model: format!("model-proxy/{route_name}"),
        backend: backend_name.to_string(),
        backend_model: backend_model.to_string(),
        input_tokens: usage.prompt_tokens as i64,
        output_tokens: usage.completion_tokens as i64,
        cache_read_tokens: usage.cache_read_tokens as i64,
        cache_creation_tokens: usage.cache_creation_tokens as i64,
        cost_usd: cost,
        cost_source: cost_source.to_string(),
        outcome: outcome.to_string(),
        error_code: None,
        duration_ms,
        ttfb_ms,
    }
}

/// Extracts usage, computes cost, attributes spend, and writes the audit row for
/// a served non-streaming response.
#[allow(clippy::too_many_arguments)]
fn finalize_success(
    state: &AppState,
    identity: &Identity,
    protocol: &str,
    route: &Route,
    backend: &Backend,
    body: &[u8],
    upstream_cost: Option<f64>,
    duration_ms: i64,
    ttfb_ms: Option<i64>,
) {
    let usage = parse_usage(body);
    let (cost, source) = cost_usd(
        &state.config.price_table,
        &backend.name,
        &backend.model,
        usage,
        upstream_cost,
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

    crate::budget::record_spend(
        &state.db,
        &state.config.budget,
        identity,
        usage.total() as i64,
        cost,
        now_ms(),
    );

    let row = build_row(
        identity,
        protocol,
        &route.name,
        &backend.name,
        &backend.model,
        usage,
        cost,
        source.as_str(),
        "ok",
        duration_ms,
        ttfb_ms,
    );
    put_audit_row(state, &row);
    state.set_resolved(&route.name, &backend.identity());
}

fn audit_failure(
    state: &AppState,
    identity: &Identity,
    protocol: &str,
    route: &Route,
    started: Instant,
) {
    let row = ModelProxyRequestRow {
        ts_ms: now_ms(),
        protocol: protocol.to_string(),
        route: route.name.clone(),
        agent: identity.agent(),
        worktree: identity.worktree(),
        workspace: identity.workspace_label(),
        client_model: format!("model-proxy/{}", route.name),
        backend: "none".to_string(),
        outcome: "all_failed".to_string(),
        error_code: Some("503".to_string()),
        duration_ms: thegn_core::time_policy::saturating_i64(started.elapsed().as_millis()),
        ..Default::default()
    };
    put_audit_row(state, &row);
}

/// Persist one audit row. These rows ARE the spend/usage record: `thegn proxy`'s
/// rollup reads them back, so a dropped row silently under-reports usage — never
/// swallow the failure, even though it must not fail the request that produced it.
fn put_audit_row(state: &AppState, row: &ModelProxyRequestRow) {
    let db = match state.db.lock() {
        Ok(db) => db,
        Err(e) => {
            tracing::warn!(
                target: "thegn::proxy",
                route = %row.route,
                outcome = %row.outcome,
                error = %e,
                "model-proxy audit row dropped: db lock poisoned (rollup under-reports)"
            );
            return;
        }
    };
    if let Err(e) = db.put_model_proxy_request(row) {
        tracing::warn!(
            target: "thegn::proxy",
            route = %row.route,
            backend = %row.backend,
            outcome = %row.outcome,
            error = %e,
            "model-proxy audit row not written (rollup under-reports)"
        );
    }
}

/// Reads usage (incl. cache tokens) from an OpenAI response body.
fn parse_usage(body: &[u8]) -> Usage {
    let v: Value = match serde_json::from_slice(body) {
        Ok(v) => v,
        Err(_) => return Usage::default(),
    };
    match v.get("usage") {
        Some(u) if !u.is_null() => parse_openai_usage(u),
        _ => Usage::default(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;
    use std::time::Duration;
    use thegn_core::db::Db;
    use thegn_core::proxy::creds::{CredPool, KeyStrategy};
    use thegn_core::proxy::ratelimit::RatePolicy;
    use tokio::io::AsyncWriteExt;

    fn proxy_state(relay: crate::relay::RelayConfig) -> SharedState {
        proxy_state_with_deadlines(relay, crate::state::UpstreamDeadlines::default())
    }

    fn proxy_state_with_deadlines(
        relay: crate::relay::RelayConfig,
        deadlines: crate::state::UpstreamDeadlines,
    ) -> SharedState {
        let config = ProxyConfig {
            listen: "127.0.0.1:0".parse().unwrap(),
            routes: Vec::new(),
            relay,
            aliases: Default::default(),
            last_resort: false,
            usage_aware: false,
            auto_tiers: Vec::new(),
            price_table: PriceTable::new(),
            budget: crate::model::BudgetSettings::default(),
        };
        let db = Arc::new(Mutex::new(Db::open_memory().unwrap()));
        crate::state::AppState::with_deadlines(config, db, 0, deadlines)
    }

    fn streaming_route(backend: Backend) -> Route {
        route_with(vec![backend], RoutingStrategy::Sequential)
    }

    async fn mock_stream_server(header_delay: Duration, send_event: bool) -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            tokio::time::sleep(header_delay).await;
            socket
                .write_all(
                    b"HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ntransfer-encoding: chunked\r\n\r\n",
                )
                .await
                .unwrap();
            if send_event {
                let event = b"data: {\"choices\":[{\"delta\":{\"content\":\"x\"}}]}\n\n";
                socket
                    .write_all(format!("{:x}\r\n", event.len()).as_bytes())
                    .await
                    .unwrap();
                socket.write_all(event).await.unwrap();
                socket.write_all(b"\r\n").await.unwrap();
            }
            tokio::time::sleep(Duration::from_secs(1)).await;
        });
        format!("http://{addr}")
    }

    async fn trickling_stream_server(interval: Duration) -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            socket.write_all(b"HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ntransfer-encoding: chunked\r\n\r\n").await.unwrap();
            let event = b"data: {\"choices\":[{\"delta\":{\"content\":\"x\"}}]}\n\n";
            socket
                .write_all(format!("{:x}\r\n", event.len()).as_bytes())
                .await
                .unwrap();
            socket.write_all(event).await.unwrap();
            socket.write_all(b"\r\n").await.unwrap();
            loop {
                tokio::time::sleep(interval).await;
                if socket.write_all(b"8\r\n: ping\n\n\r\n").await.is_err() {
                    break;
                }
            }
        });
        format!("http://{addr}")
    }

    async fn stalled_header_server(delay: Duration) -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (_socket, _) = listener.accept().await.unwrap();
            tokio::time::sleep(delay).await;
        });
        format!("http://{addr}")
    }

    async fn completion_server() -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let body = br#"{"choices":[{"message":{"content":"ok"}}]}"#;
            let response = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                body.len()
            );
            socket.write_all(response.as_bytes()).await.unwrap();
            socket.write_all(body).await.unwrap();
            tokio::time::sleep(Duration::from_secs(1)).await;
        });
        format!("http://{addr}")
    }

    async fn buffered_idle_server(delay: Duration) -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            socket.write_all(b"HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ntransfer-encoding: chunked\r\n\r\n5\r\n{\"x\":\r\n").await.unwrap();
            tokio::time::sleep(delay).await;
        });
        format!("http://{addr}")
    }

    async fn buffered_trickle_server(interval: Duration) -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            socket.write_all(b"HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ntransfer-encoding: chunked\r\n\r\n").await.unwrap();
            loop {
                tokio::time::sleep(interval).await;
                if socket.write_all(b"1\r\n \r\n").await.is_err() {
                    break;
                }
            }
        });
        format!("http://{addr}")
    }

    async fn trickling_error_body_server(interval: Duration) -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            socket.write_all(b"HTTP/1.1 429 Too Many Requests\r\ncontent-type: application/json\r\nretry-after: 30\r\ntransfer-encoding: chunked\r\n\r\n1\r\nx\r\n").await.unwrap();
            loop {
                tokio::time::sleep(interval).await;
                if socket.write_all(b"1\r\nx\r\n").await.is_err() {
                    break;
                }
            }
        });
        format!("http://{addr}")
    }

    fn lane(name: &str, key_id: &str, pool: Option<Arc<CredPool>>) -> Backend {
        Backend {
            name: name.into(),
            key_id: key_id.into(),
            base_url: "http://x".into(),
            model: "m".into(),
            api_key: "k".into(),
            wire: crate::model::Wire::OpenAi,
            context_limit: 0,
            defaults: serde_json::Map::new(),
            rate: RatePolicy {
                rpm: 60.0,
                burst: 5.0,
            },
            inflight_cap: 0,
            pool,
        }
    }

    fn lane_model(name: &str, model: &str) -> Backend {
        let mut b = lane(name, "", None);
        b.model = model.into();
        b
    }

    fn route_with(priority: Vec<Backend>, strategy: RoutingStrategy) -> Route {
        Route {
            name: "standard".into(),
            priority,
            strategy,
            order_pool: (strategy == RoutingStrategy::LoadBalanced)
                .then(|| Arc::new(CredPool::new(KeyStrategy::RoundRobin, vec![]))),
            last_resort: false,
        }
    }

    fn prices() -> PriceTable {
        let mut t = PriceTable::new();
        t.add_cost_bearing("openrouter");
        t.set_price(
            "openrouter:pro",
            thegn_core::proxy::cost::PricePoint {
                input_usd_per_mtok: 1.0,
                output_usd_per_mtok: 1.0,
            },
        );
        t
    }

    #[test]
    fn sequential_keeps_natural_order() {
        let route = route_with(
            vec![lane("a", "", None), lane("b", "", None)],
            RoutingStrategy::Sequential,
        );
        assert_eq!(ordered_priority(&route, 0, &prices(), false), vec![0, 1]);
    }

    #[test]
    fn cost_aware_orders_cheapest_first() {
        // openrouter:pro is paid; codex is subscription ($0) → tried first.
        let route = route_with(
            vec![lane_model("openrouter", "pro"), lane_model("codex", "gpt")],
            RoutingStrategy::CostAware,
        );
        assert_eq!(ordered_priority(&route, 0, &prices(), false), vec![1, 0]);
    }

    #[test]
    fn force_cost_overrides_sequential() {
        let route = route_with(
            vec![lane_model("openrouter", "pro"), lane_model("codex", "gpt")],
            RoutingStrategy::Sequential,
        );
        // Sequential normally keeps [0,1]; forced cost puts the free lane first.
        assert_eq!(ordered_priority(&route, 0, &prices(), false), vec![0, 1]);
        assert_eq!(ordered_priority(&route, 0, &prices(), true), vec![1, 0]);
    }

    #[test]
    fn pool_group_round_robins() {
        let pool = Arc::new(CredPool::new(KeyStrategy::RoundRobin, vec![]));
        let route = route_with(
            vec![
                lane("p", "#0", Some(pool.clone())),
                lane("p", "#1", Some(pool.clone())),
            ],
            RoutingStrategy::Sequential,
        );
        assert_eq!(ordered_priority(&route, 0, &prices(), false), vec![0, 1]);
        assert_eq!(ordered_priority(&route, 0, &prices(), false), vec![1, 0]);
    }

    #[test]
    fn last_resort_unions_and_dedups() {
        let cfg = ProxyConfig {
            listen: "127.0.0.1:0".parse().unwrap(),
            routes: vec![
                route_with(vec![lane("a", "", None)], RoutingStrategy::Sequential),
                Route {
                    name: "fast".into(),
                    priority: vec![lane("b", "", None), lane("a", "", None)],
                    strategy: RoutingStrategy::Sequential,
                    order_pool: None,
                    last_resort: false,
                },
                Route {
                    name: "free".into(),
                    priority: vec![lane("c", "", None), lane("b", "", None)],
                    strategy: RoutingStrategy::Sequential,
                    order_pool: None,
                    last_resort: false,
                },
            ],
            relay: crate::relay::RelayConfig::default(),
            aliases: Default::default(),
            last_resort: true,
            usage_aware: false,
            auto_tiers: Vec::new(),
            price_table: prices(),
            budget: crate::model::BudgetSettings::default(),
        };
        let tried: HashSet<String> = ["a".to_string()].into();
        let lanes = last_resort_lanes(&cfg, "standard", &tried);
        let names: Vec<&str> = lanes.iter().map(|b| b.name.as_str()).collect();
        assert_eq!(names, vec!["b", "c"]);
    }

    #[test]
    fn parse_usage_reads_cache_tokens() {
        let u = parse_usage(
            br#"{"usage":{"prompt_tokens":10,"completion_tokens":5,"cache_read_tokens":40}}"#,
        );
        assert_eq!(u.prompt_tokens, 10);
        assert_eq!(u.cache_read_tokens, 40);
    }

    #[tokio::test]
    async fn route_streaming_rejects_a_saturated_sole_backend() {
        let backend = lane("only", "", None);
        let ident = backend.identity();
        let rate = backend.rate;
        let route = streaming_route(backend);
        let state = proxy_state(crate::relay::RelayConfig::default());
        let held = state.inflight.try_acquire(&ident, 1).unwrap();

        let outcome = route_streaming(
            state.clone(),
            Identity::global(),
            Surface::OpenAi,
            &route,
            "m",
            br#"{"model":"m","stream":true}"#,
            false,
        )
        .await;

        assert!(matches!(outcome, StreamOutcome::Failed));
        assert_eq!(state.inflight.get(&ident), 1);
        assert!(state.limiter.try_acquire(&ident, rate, Instant::now()));
        drop(held);
        assert_eq!(state.inflight.get(&ident), 0);
    }

    #[tokio::test]
    async fn dropped_stream_body_releases_backend_inflight_permit() {
        let url = trickling_stream_server(Duration::from_millis(20)).await;
        let mut backend = lane("stream", "", None);
        backend.base_url = url;
        backend.inflight_cap = 1;
        let ident = backend.identity();
        let route = streaming_route(backend);
        let state = proxy_state(crate::relay::RelayConfig {
            first_byte: Duration::from_millis(500),
            idle: Duration::from_secs(2),
            heartbeat: Duration::from_millis(100),
        });

        let StreamOutcome::Body(body) = route_streaming(
            state.clone(),
            Identity::global(),
            Surface::OpenAi,
            &route,
            "m",
            br#"{"model":"m","stream":true}"#,
            false,
        )
        .await
        else {
            panic!("mock upstream should commit a streamed body");
        };
        assert_eq!(state.inflight.get(&ident), 1);
        drop(body);
        assert_eq!(state.inflight.get(&ident), 0);
    }

    #[tokio::test]
    async fn committed_stream_total_deadline_ends_body_and_releases_permit() {
        let url = trickling_stream_server(Duration::from_millis(20)).await;
        let mut backend = lane("stream-total", "", None);
        backend.base_url = url;
        backend.inflight_cap = 1;
        let ident = backend.identity();
        let route = streaming_route(backend);
        let state = proxy_state_with_deadlines(
            crate::relay::RelayConfig {
                first_byte: Duration::from_millis(400),
                idle: Duration::from_secs(2),
                heartbeat: Duration::from_millis(20),
            },
            crate::state::UpstreamDeadlines {
                stream_total: Duration::from_millis(100),
                ..Default::default()
            },
        );
        let StreamOutcome::Body(body) = route_streaming(
            state.clone(),
            Identity::global(),
            Surface::OpenAi,
            &route,
            "m",
            br#"{"model":"m","stream":true}"#,
            false,
        )
        .await
        else {
            panic!("mock stream should commit")
        };
        let chunks = tokio::time::timeout(Duration::from_millis(500), async {
            body.into_data_stream().collect::<Vec<_>>().await
        })
        .await
        .unwrap();
        assert!(!chunks.is_empty());
        assert_eq!(state.inflight.get(&ident), 0);
    }

    #[tokio::test(start_paused = true)]
    async fn rate_wait_longer_than_stream_first_byte_budget_is_skipped() {
        let mut backend = lane("rate-wait", "", None);
        backend.rate = RatePolicy {
            rpm: 60.0,
            burst: 1.0,
        };
        let ident = backend.identity();
        let route = streaming_route(backend.clone());
        let state = proxy_state(crate::relay::RelayConfig {
            first_byte: Duration::from_millis(20),
            idle: Duration::from_secs(1),
            heartbeat: Duration::from_millis(10),
        });
        assert!(
            state
                .limiter
                .try_acquire(&ident, backend.rate, Instant::now())
        );
        let start = tokio::time::Instant::now();
        let outcome = route_streaming(
            state.clone(),
            Identity::global(),
            Surface::OpenAi,
            &route,
            "m",
            br#"{"model":"m","stream":true}"#,
            false,
        )
        .await;
        assert!(matches!(outcome, StreamOutcome::Failed));
        assert!(start.elapsed() < Duration::from_millis(500));
        assert!(
            state
                .metrics
                .render()
                .contains("reason=\"rate_wait_exceeds_deadline\"")
        );
    }

    #[tokio::test]
    async fn cancelling_during_bounded_rate_wait_releases_inflight_permit() {
        let mut backend = lane("rate-wait-cancel", "", None);
        backend.rate = RatePolicy {
            rpm: 60.0,
            burst: 1.0,
        };
        let ident = backend.identity();
        let route = streaming_route(backend.clone());
        let state = proxy_state(crate::relay::RelayConfig {
            first_byte: Duration::from_secs(2),
            idle: Duration::from_secs(1),
            heartbeat: Duration::from_millis(10),
        });
        assert!(
            state
                .limiter
                .try_acquire(&ident, backend.rate, Instant::now())
        );
        let mut future = Box::pin(route_streaming(
            state.clone(),
            Identity::global(),
            Surface::OpenAi,
            &route,
            "m",
            br#"{"model":"m","stream":true}"#,
            false,
        ));
        tokio::select! {
            _ = &mut future => panic!("rate wait should still be pending"),
            _ = tokio::time::sleep(Duration::from_millis(30)) => {}
        }
        assert_eq!(state.inflight.get(&ident), 1);
        drop(future);
        assert_eq!(state.inflight.get(&ident), 0);
    }

    #[tokio::test]
    async fn stream_without_headers_falls_through_within_first_byte_budget() {
        let budget = Duration::from_millis(60);
        let url = stalled_header_server(Duration::from_millis(200)).await;
        let mut backend = lane("stalled", "", None);
        backend.base_url = url;
        let route = streaming_route(backend);
        let state = proxy_state(crate::relay::RelayConfig {
            first_byte: budget,
            idle: Duration::from_millis(100),
            heartbeat: Duration::from_millis(10),
        });
        let started = tokio::time::Instant::now();

        let outcome = route_streaming(
            state.clone(),
            Identity::global(),
            Surface::OpenAi,
            &route,
            "m",
            br#"{"model":"m","stream":true}"#,
            false,
        )
        .await;

        assert!(matches!(outcome, StreamOutcome::Failed));
        assert!(started.elapsed() < Duration::from_millis(120));
        assert!(
            state
                .metrics
                .render()
                .contains("reason=\"first_byte_timeout\"")
        );
    }

    #[tokio::test]
    async fn stream_open_and_peek_share_first_byte_budget() {
        let budget = Duration::from_millis(120);
        let url = mock_stream_server(Duration::from_millis(80), false).await;
        let mut backend = lane("stalled", "", None);
        backend.base_url = url;
        let route = streaming_route(backend);
        let state = proxy_state(crate::relay::RelayConfig {
            first_byte: budget,
            idle: Duration::from_millis(100),
            heartbeat: Duration::from_millis(10),
        });
        let started = tokio::time::Instant::now();

        let outcome = route_streaming(
            state,
            Identity::global(),
            Surface::OpenAi,
            &route,
            "m",
            br#"{"model":"m","stream":true}"#,
            false,
        )
        .await;

        assert!(matches!(outcome, StreamOutcome::Failed));
        assert!(started.elapsed() < Duration::from_millis(180));
    }

    #[tokio::test]
    async fn buffered_total_deadline_falls_through_and_releases_permit() {
        let stalled_url = stalled_header_server(Duration::from_millis(500)).await;
        let success_url = completion_server().await;
        let mut stalled = lane("stalled", "", None);
        stalled.base_url = stalled_url;
        stalled.inflight_cap = 1;
        let stalled_identity = stalled.identity();
        let mut success = lane("success", "", None);
        success.base_url = success_url;
        let route = route_with(vec![stalled, success.clone()], RoutingStrategy::Sequential);
        let state = proxy_state_with_deadlines(
            crate::relay::RelayConfig::default(),
            crate::state::UpstreamDeadlines {
                connect: Duration::from_secs(1),
                buffered_total: Duration::from_millis(60),
                ..Default::default()
            },
        );
        let started = tokio::time::Instant::now();

        let result = route_nonstreaming(
            &state,
            &Identity::global(),
            "openai",
            &route,
            br#"{"model":"m","messages":[]}"#,
            false,
        )
        .await;

        assert_eq!(result.served_by, success.identity());
        assert!(started.elapsed() < Duration::from_millis(300));
        assert_eq!(state.inflight.get(&stalled_identity), 0);
        assert!(state.metrics.render().contains("reason=\"total_timeout\""));
    }

    #[tokio::test]
    async fn buffered_connect_deadline_falls_through() {
        let socket = tokio::net::TcpSocket::new_v4().unwrap();
        socket.bind("127.0.0.1:0".parse().unwrap()).unwrap();
        let listener = socket.listen(1).unwrap();
        let addr = listener.local_addr().unwrap();
        let std_listener = listener.into_std().unwrap();
        let mut queued_connections = Vec::new();
        for _ in 0..8 {
            match std::net::TcpStream::connect_timeout(&addr, Duration::from_millis(100)) {
                Ok(connection) => queued_connections.push(connection),
                Err(_) => break,
            }
        }

        let success_url = completion_server().await;
        let mut stalled = lane("connect-stalled", "", None);
        stalled.base_url = format!("http://{addr}");
        stalled.rate = RatePolicy {
            rpm: 60.0,
            burst: 1.0,
        };
        let stalled_rate = stalled.rate;
        let stalled_identity = stalled.identity();
        let mut success = lane("success", "", None);
        success.base_url = success_url;
        let route = route_with(vec![stalled, success.clone()], RoutingStrategy::Sequential);
        let state = proxy_state_with_deadlines(
            crate::relay::RelayConfig::default(),
            crate::state::UpstreamDeadlines {
                connect: Duration::from_millis(50),
                buffered_total: Duration::from_secs(5),
                ..Default::default()
            },
        );
        let started = tokio::time::Instant::now();

        let result = route_nonstreaming(
            &state,
            &Identity::global(),
            "openai",
            &route,
            br#"{"model":"m","messages":[]}"#,
            false,
        )
        .await;

        assert_eq!(result.served_by, success.identity());
        assert!(started.elapsed() < Duration::from_secs(1));
        assert!(
            state
                .metrics
                .render()
                .contains("reason=\"connect_timeout\"")
        );
        assert!(
            state
                .limiter
                .try_acquire(&stalled_identity, stalled_rate, Instant::now())
        );
        drop(std_listener);
        drop(queued_connections);
    }

    #[tokio::test]
    async fn buffered_first_byte_and_idle_deadlines_fall_through() {
        let fallback_url = completion_server().await;
        let mut first = lane("first-byte", "", None);
        first.base_url = stalled_header_server(Duration::from_secs(1)).await;
        first.rate = RatePolicy {
            rpm: 60.0,
            burst: 1.0,
        };
        let first_rate = first.rate;
        let first_id = first.identity();
        let mut fallback = lane("first-byte-fallback", "", None);
        fallback.base_url = fallback_url.clone();
        let state = proxy_state(crate::relay::RelayConfig {
            first_byte: Duration::from_millis(50),
            idle: Duration::from_millis(300),
            heartbeat: Duration::from_millis(10),
        });
        let route = route_with(vec![first, fallback.clone()], RoutingStrategy::Sequential);
        let result = route_nonstreaming(
            &state,
            &Identity::global(),
            "openai",
            &route,
            br#"{"model":"m","messages":[]}"#,
            false,
        )
        .await;
        assert_eq!(result.served_by, fallback.identity());
        assert_eq!(state.inflight.get(&first_id), 0);
        assert!(
            !state
                .limiter
                .try_acquire(&first_id, first_rate, Instant::now())
        );
        assert!(
            state
                .metrics
                .render()
                .contains("reason=\"first_byte_timeout\"")
        );

        let mut idle = lane("idle", "", None);
        idle.base_url = buffered_idle_server(Duration::from_secs(1)).await;
        let idle_id = idle.identity();
        let mut fallback = lane("idle-fallback", "", None);
        fallback.base_url = completion_server().await;
        let state = proxy_state(crate::relay::RelayConfig {
            first_byte: Duration::from_millis(300),
            idle: Duration::from_millis(50),
            heartbeat: Duration::from_millis(10),
        });
        let route = route_with(vec![idle, fallback.clone()], RoutingStrategy::Sequential);
        let result = route_nonstreaming(
            &state,
            &Identity::global(),
            "openai",
            &route,
            br#"{"model":"m","messages":[]}"#,
            false,
        )
        .await;
        assert_eq!(result.served_by, fallback.identity());
        assert_eq!(state.inflight.get(&idle_id), 0);
        assert!(state.metrics.render().contains("reason=\"idle_timeout\""));
    }

    #[tokio::test]
    async fn buffered_total_deadline_stops_a_trickling_body() {
        let mut trickle = lane("trickle", "", None);
        trickle.base_url = buffered_trickle_server(Duration::from_millis(20)).await;
        let trickle_id = trickle.identity();
        let mut fallback = lane("trickle-fallback", "", None);
        fallback.base_url = completion_server().await;
        let route = route_with(vec![trickle, fallback.clone()], RoutingStrategy::Sequential);
        let state = proxy_state_with_deadlines(
            crate::relay::RelayConfig {
                first_byte: Duration::from_millis(300),
                idle: Duration::from_millis(200),
                heartbeat: Duration::from_millis(10),
            },
            crate::state::UpstreamDeadlines {
                buffered_total: Duration::from_millis(100),
                ..Default::default()
            },
        );
        let result = route_nonstreaming(
            &state,
            &Identity::global(),
            "openai",
            &route,
            br#"{"model":"m","messages":[]}"#,
            false,
        )
        .await;
        assert_eq!(result.served_by, fallback.identity());
        assert_eq!(state.inflight.get(&trickle_id), 0);
        assert!(state.metrics.render().contains("reason=\"total_timeout\""));
    }

    #[tokio::test]
    async fn stream_error_body_idle_timeout_falls_through() {
        let mut backend = lane("error-body", "", None);
        backend.base_url = trickling_error_body_server(Duration::from_millis(25)).await;
        let ident = backend.identity();
        let model = backend.model.clone();
        let route = streaming_route(backend);
        let state = proxy_state(crate::relay::RelayConfig {
            first_byte: Duration::from_millis(300),
            idle: Duration::from_millis(50),
            heartbeat: Duration::from_millis(10),
        });
        let start = tokio::time::Instant::now();
        let outcome = tokio::time::timeout(
            Duration::from_millis(300),
            route_streaming(
                state.clone(),
                Identity::global(),
                Surface::OpenAi,
                &route,
                "m",
                br#"{"model":"m","stream":true}"#,
                false,
            ),
        )
        .await
        .expect("the total error-body read deadline should fall through");
        assert!(matches!(outcome, StreamOutcome::Failed));
        assert!(start.elapsed() < Duration::from_millis(300));
        assert!(state.metrics.render().contains("reason=\"idle_timeout\""));
        assert!(state.health.is_exhausted(&ident, &model, now_ms()));
    }

    #[tokio::test]
    async fn anthropic_stream_fallback_obeys_buffered_first_byte_deadline() {
        let mut backend = lane("anthropic-stalled", "", None);
        backend.wire = crate::model::Wire::Anthropic;
        backend.base_url = stalled_header_server(Duration::from_secs(1)).await;
        let route = streaming_route(backend);
        let state = proxy_state(crate::relay::RelayConfig {
            first_byte: Duration::from_millis(50),
            idle: Duration::from_millis(200),
            heartbeat: Duration::from_millis(10),
        });
        let start = tokio::time::Instant::now();
        let outcome = route_streaming(
            state.clone(),
            Identity::global(),
            Surface::OpenAi,
            &route,
            "m",
            br#"{"model":"m","stream":true}"#,
            false,
        )
        .await;
        assert!(matches!(outcome, StreamOutcome::Failed));
        assert!(start.elapsed() < Duration::from_millis(250));
        assert!(
            state
                .metrics
                .render()
                .contains("reason=\"first_byte_timeout\"")
        );
    }

    #[tokio::test]
    async fn connect_failure_refunds_rate_token_and_cancellation_releases_permit() {
        let socket = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = socket.local_addr().unwrap();
        drop(socket);
        let mut backend = lane("refundable", "", None);
        backend.base_url = format!("http://{addr}");
        backend.rate = RatePolicy {
            rpm: 60.0,
            burst: 1.0,
        };
        let ident = backend.identity();
        let route = route_with(vec![backend.clone()], RoutingStrategy::Sequential);
        let state = proxy_state(crate::relay::RelayConfig::default());
        let _response = route_nonstreaming(
            &state,
            &Identity::global(),
            "openai",
            &route,
            br#"{"model":"m","messages":[]}"#,
            false,
        )
        .await;
        assert!(
            state
                .limiter
                .try_acquire(&ident, backend.rate, Instant::now())
        );

        let url = stalled_header_server(Duration::from_secs(1)).await;
        let mut stalled = lane("cancel", "", None);
        stalled.base_url = url;
        let cancel_id = stalled.identity();
        let route = route_with(vec![stalled], RoutingStrategy::Sequential);
        let mut future = Box::pin(route_nonstreaming(
            &state,
            &Identity::global(),
            "openai",
            &route,
            br#"{"model":"m","messages":[]}"#,
            false,
        ));
        tokio::select! {
            _ = &mut future => panic!("stalled upstream unexpectedly completed"),
            _ = tokio::time::sleep(Duration::from_millis(50)) => {}
        }
        drop(future);
        assert_eq!(state.inflight.get(&cancel_id), 0);
    }

    #[test]
    fn a_connect_failure_without_a_token_does_not_refund_one() {
        let mut backend = lane("unfunded", "", None);
        backend.rate = RatePolicy {
            rpm: 60.0,
            burst: 1.0,
        };
        let ident = backend.identity();
        let state = proxy_state(crate::relay::RelayConfig::default());
        assert!(
            state
                .limiter
                .try_acquire(&ident, backend.rate, Instant::now())
        );

        refund_connect_failure(&state, &ident, true, false);

        assert!(
            !state
                .limiter
                .try_acquire(&ident, backend.rate, Instant::now())
        );
    }
}
