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
            // best-effort, and NOT an error: `try_acquire` returns a bool, not a
            // Result. We already slept out the reservation this backend handed
            // us, and it is the last one — so the request proceeds either way.
            // The value is discarded because a lost race with a concurrent
            // acquire changes nothing about that decision.
            let _ = state // best-effort: value deliberately discarded (see above)
                .limiter
                .try_acquire(&ident, backend.rate, Instant::now());
        }
        if !is_last && state.inflight.at_cap(&ident, backend.inflight_cap) {
            state.metrics.inc_fallthrough(&ident, "loadshed_inflight");
            continue;
        }

        let backend_body = apply_transforms(backend, parsed, body);
        state.inflight.enter(&ident);
        let attempt = state
            .upstreams
            .for_backend(backend)
            .dispatch(&state.client, backend, &backend_body)
            .await;
        state.inflight.leave(&ident);

        let resp = match attempt {
            Ok(r) => r,
            Err(e) => {
                tracing::warn!(backend = %ident, error = %e, "backend request error");
                state.metrics.inc_backend_attempt(&ident, "network_error");
                state.metrics.inc_fallthrough(&ident, "network_error");
                continue;
            }
        };

        let (kind, reason) = classify_response(resp.status, &resp.body);
        match kind {
            FailKind::Serve => {
                state.health.record_success(&ident, &backend.model);
                state.metrics.inc_backend_attempt(&ident, "ok");
                state.metrics.inc_request(&route.name, &ident, "ok");
                let duration_ms = started.elapsed().as_millis() as i64;
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
            // best-effort, and NOT an error: `try_acquire` returns a bool, not a
            // Result. We already slept out the reservation this backend handed
            // us, and it is the last one — so the request proceeds either way.
            // The value is discarded because a lost race with a concurrent
            // acquire changes nothing about that decision.
            let _ = state // best-effort: value deliberately discarded (see above)
                .limiter
                .try_acquire(&ident, backend.rate, Instant::now());
        }

        let backend_body = apply_transforms(backend, &parsed, body);

        if backend.is_anthropic() {
            // Anthropic-surface backend in streaming mode: buffer then synthesize.
            match state
                .upstreams
                .for_backend(backend)
                .dispatch(&state.client, backend, &backend_body)
                .await
            {
                Ok(resp) => {
                    let (kind, reason) = classify_response(resp.status, &resp.body);
                    match kind {
                        FailKind::Serve => {
                            state.health.record_success(&ident, &backend.model);
                            let duration_ms = started.elapsed().as_millis() as i64;
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
                Err(e) => {
                    tracing::warn!(backend = %ident, error = %e, "anthropic stream backend error");
                    state.metrics.inc_fallthrough(&ident, "network_error");
                }
            }
            continue;
        }

        // OpenAI-surface backend: open the stream and relay it live.
        let resp = match state
            .upstreams
            .for_backend(backend)
            .open_stream(&state.client, backend, &backend_body)
            .await
        {
            Ok(r) if r.status().is_success() => r,
            Ok(r) => {
                let status = r.status().as_u16();
                let headers = r.headers().clone();
                let bytes = r.bytes().await.map(|b| b.to_vec()).unwrap_or_default();
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
            Err(e) => {
                tracing::warn!(backend = %ident, error = %e, "stream backend error");
                state.metrics.inc_fallthrough(&ident, "network_error");
                continue;
            }
        };
        let upstream_cost = header_cost(resp.headers());

        let commit = match surface {
            Surface::OpenAi => match relay::peek(resp, OpenAiSink::default(), cfg).await {
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
                    Some(relay::spawn_relay(prefix_out, rest, sink, cfg, fin))
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
                match relay::peek(resp, sink, cfg).await {
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
                        Some(relay::spawn_relay(prefix_out, rest, sink, cfg, fin))
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
            return Some(body);
        }
    }
    None
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
    let ttfb_ms = started.elapsed().as_millis() as i64;
    move |stats: RelayStats| {
        let usage = stats.usage;
        let duration_ms = started.elapsed().as_millis() as i64;
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
        duration_ms: started.elapsed().as_millis() as i64,
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
    use thegn_core::proxy::creds::{CredPool, KeyStrategy};
    use thegn_core::proxy::ratelimit::RatePolicy;

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
}
