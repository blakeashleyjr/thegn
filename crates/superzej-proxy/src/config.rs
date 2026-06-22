//! Runtime configuration for the standalone daemon.
//!
//! Milestone 1 loads routes from a JSON document (file via `SZPROXY_CONFIG`, or
//! inline via `SZPROXY_ROUTES_JSON`) so the daemon is usable on its own for
//! validation. Stage E folds this into superzej's layered `[llm_proxy]` config
//! (`config_enum!`-based) and maps the legacy `MODEL_PROXY_*` env knobs; the
//! JSON shape here is the same data those layers will produce.

use std::net::SocketAddr;
use std::time::Duration;

use anyhow::{Context, Result};
use regex::Regex;
use serde::Deserialize;
use serde_json::{Map, Value};
use superzej_core::config::RoutingStrategy;
use superzej_core::proxy::compress::{Level, Limits};
use superzej_core::proxy::creds::{CredPool, KeyStrategy};
use superzej_core::proxy::ratelimit::RatePolicy;
use superzej_core::proxy::transform::CompressPolicy;

use crate::model::{Backend, ProxyConfig, Route};
use crate::relay::RelayConfig;

#[derive(Debug, Deserialize)]
struct ConfigDoc {
    #[serde(default)]
    routes: Vec<RouteDoc>,
    /// Optional token-reduction settings (group W).
    #[serde(default)]
    compression: Option<CompressionDoc>,
}

#[derive(Debug, Default, Deserialize)]
struct CompressionDoc {
    /// "off" | "conservative" | "balanced" | "aggressive". Env overrides this.
    #[serde(default)]
    level: Option<String>,
    #[serde(default)]
    bypass_tools: Vec<String>,
    #[serde(default)]
    only_tools: Option<Vec<String>>,
    #[serde(default)]
    filters: Vec<FilterDoc>,
    #[serde(default)]
    max_block_chars: Option<usize>,
    #[serde(default)]
    keep_head: Option<usize>,
    #[serde(default)]
    keep_tail: Option<usize>,
}

#[derive(Debug, Deserialize)]
struct FilterDoc {
    pattern: String,
    replacement: String,
}

#[derive(Debug, Deserialize)]
struct RouteDoc {
    name: String,
    #[serde(default)]
    backends: Vec<BackendDoc>,
    /// `sequential` | `load_balanced` | `speculative`. Falls back to the global
    /// `SZPROXY_ROUTING` (then `sequential`).
    #[serde(default)]
    strategy: Option<String>,
}

#[derive(Debug, Deserialize)]
struct BackendDoc {
    name: String,
    base_url: String,
    model: String,
    /// Inline key (discouraged); prefer `api_key_env`.
    #[serde(default)]
    api_key: Option<String>,
    /// Env var to read the key from.
    #[serde(default)]
    api_key_env: Option<String>,
    /// Multiple keys for one provider (preferred form): each env var is read to a
    /// key, expanded into its own rate-limited/health-tracked lane (U 280).
    #[serde(default)]
    api_key_envs: Vec<String>,
    /// Inline multi-key list (discouraged; prefer `api_key_envs`).
    #[serde(default)]
    api_keys: Vec<String>,
    /// Lane order strategy: `roundrobin` (default) | `failover` | `random` | `weighted`.
    #[serde(default)]
    key_strategy: Option<String>,
    /// Per-key weights aligned to the resolved key order (weighted strategy).
    #[serde(default)]
    key_weights: Vec<u32>,
    #[serde(default)]
    anthropic: bool,
    #[serde(default)]
    context_limit: usize,
    #[serde(default)]
    defaults: Map<String, Value>,
    #[serde(default)]
    rpm: Option<f64>,
    #[serde(default)]
    burst: Option<f64>,
    #[serde(default)]
    inflight_cap: u32,
}

fn default_listen() -> SocketAddr {
    "127.0.0.1:8383".parse().unwrap()
}

/// Reads `SZPROXY_LISTEN` (or `META_ROUTER_PORT`), the routes document, and the
/// streaming relay tunables.
pub fn from_env() -> Result<ProxyConfig> {
    let listen = resolve_listen()?;
    let doc = load_doc()?;
    let compression = resolve_compression(doc.as_ref().and_then(|d| d.compression.as_ref()), true);
    let routes = doc
        .map(|d| build_routes(d, global_routing_from_env()))
        .transpose()?
        .unwrap_or_default();
    Ok(ProxyConfig {
        listen,
        routes,
        relay: relay_from_env(),
        compression,
    })
}

/// Builds the token-reduction policy. The effective level is: env
/// `SZPROXY_COMPRESS_LEVEL` (when `read_env`) → routes-doc `level` → `Off`; an
/// explicit `SZPROXY_COMPRESS=0` forces it off. Bypass list, allow-list, custom
/// filters, and truncation limits come from the routes doc.
fn resolve_compression(doc: Option<&CompressionDoc>, read_env: bool) -> CompressPolicy {
    // Resolve level.
    let mut level = doc
        .and_then(|d| d.level.as_deref())
        .map(Level::parse)
        .unwrap_or(Level::Off);
    if read_env {
        if let Ok(v) = std::env::var("SZPROXY_COMPRESS_LEVEL")
            .or_else(|_| std::env::var("MODEL_PROXY_COMPRESS_LEVEL"))
        {
            level = Level::parse(&v);
        }
        match std::env::var("SZPROXY_COMPRESS").ok().as_deref() {
            Some("0") | Some("false") | Some("off") | Some("no") => level = Level::Off,
            Some(_)
                if doc.and_then(|d| d.level.as_deref()).is_none()
                    && std::env::var("SZPROXY_COMPRESS_LEVEL").is_err() =>
            {
                // SZPROXY_COMPRESS=1 with no explicit level → default conservative.
                level = Level::Conservative;
            }
            _ => {}
        }
    }
    let limits = Limits {
        max_block_chars: doc
            .and_then(|d| d.max_block_chars)
            .unwrap_or(Limits::default().max_block_chars),
        keep_head: doc
            .and_then(|d| d.keep_head)
            .unwrap_or(Limits::default().keep_head),
        keep_tail: doc
            .and_then(|d| d.keep_tail)
            .unwrap_or(Limits::default().keep_tail),
    };
    let filters = doc
        .map(|d| {
            d.filters
                .iter()
                .filter_map(|f| {
                    Regex::new(&f.pattern)
                        .ok()
                        .map(|re| (re, f.replacement.clone()))
                })
                .collect()
        })
        .unwrap_or_default();
    CompressPolicy {
        level,
        limits,
        bypass_tools: doc.map(|d| d.bypass_tools.clone()).unwrap_or_default(),
        only_tools: doc.and_then(|d| d.only_tools.clone()),
        filters,
    }
}

/// Resolves the relay tunables from env, honoring the Go-compatible
/// `MODEL_PROXY_*` names (and `SZPROXY_*` aliases) with the Go defaults.
fn relay_from_env() -> RelayConfig {
    let secs = |keys: &[&str], default: u64| -> Duration {
        for k in keys {
            if let Ok(v) = std::env::var(k)
                && let Ok(n) = v.trim().parse::<u64>()
            {
                return Duration::from_secs(n);
            }
        }
        Duration::from_secs(default)
    };
    RelayConfig {
        first_byte: secs(
            &[
                "SZPROXY_FIRST_BYTE_TIMEOUT",
                "MODEL_PROXY_FIRST_BYTE_TIMEOUT",
            ],
            45,
        ),
        idle: secs(
            &[
                "SZPROXY_STREAM_IDLE_TIMEOUT",
                "MODEL_PROXY_STREAM_IDLE_TIMEOUT",
            ],
            120,
        ),
        heartbeat: secs(
            &[
                "SZPROXY_STREAM_HEARTBEAT_INTERVAL",
                "MODEL_PROXY_STREAM_HEARTBEAT_INTERVAL",
            ],
            10,
        ),
    }
}

fn resolve_listen() -> Result<SocketAddr> {
    if let Ok(v) = std::env::var("SZPROXY_LISTEN") {
        return v
            .parse()
            .with_context(|| format!("parse SZPROXY_LISTEN={v}"));
    }
    if let Ok(p) = std::env::var("META_ROUTER_PORT") {
        let port: u16 = p
            .parse()
            .with_context(|| format!("parse META_ROUTER_PORT={p}"))?;
        return Ok(SocketAddr::from(([127, 0, 0, 1], port)));
    }
    Ok(default_listen())
}

fn load_doc() -> Result<Option<ConfigDoc>> {
    if let Ok(path) = std::env::var("SZPROXY_CONFIG") {
        let raw = std::fs::read_to_string(&path).with_context(|| format!("read {path}"))?;
        return Ok(Some(parse_doc(&raw)?));
    }
    if let Ok(inline) = std::env::var("SZPROXY_ROUTES_JSON") {
        return Ok(Some(parse_doc(&inline)?));
    }
    Ok(None)
}

/// Parses a routes document (JSON), public for tests and config validation.
pub fn parse_config(raw: &str) -> Result<ProxyConfig> {
    let doc = parse_doc(raw)?;
    let compression = resolve_compression(doc.compression.as_ref(), false);
    // parse_config is env-free (tests/validation): per-route `strategy` still
    // applies; the global default is sequential.
    Ok(ProxyConfig {
        listen: default_listen(),
        routes: build_routes(doc, RoutingStrategy::default())?,
        relay: RelayConfig::default(),
        compression,
    })
}

fn parse_doc(raw: &str) -> Result<ConfigDoc> {
    serde_json::from_str(raw).context("parse proxy config JSON")
}

fn build_routes(doc: ConfigDoc, global_routing: RoutingStrategy) -> Result<Vec<Route>> {
    let mut routes = Vec::new();
    for r in doc.routes {
        let mut priority = Vec::new();
        for b in r.backends {
            expand_backend(b, &mut priority);
        }
        // Per-route strategy → global default → sequential.
        let strategy = r
            .strategy
            .as_deref()
            .and_then(|s| RoutingStrategy::from_str_validated(s).ok())
            .unwrap_or(global_routing);
        // LoadBalanced needs a persistent round-robin cursor over the slots.
        let order_pool = (strategy == RoutingStrategy::LoadBalanced)
            .then(|| std::sync::Arc::new(CredPool::new(KeyStrategy::RoundRobin, vec![])));
        routes.push(Route {
            name: r.name,
            priority,
            strategy,
            order_pool,
        });
    }
    Ok(routes)
}

/// The global default routing strategy from `SZPROXY_ROUTING` (then sequential).
fn global_routing_from_env() -> RoutingStrategy {
    std::env::var("SZPROXY_ROUTING")
        .ok()
        .and_then(|s| RoutingStrategy::from_str_validated(&s).ok())
        .unwrap_or_default()
}

/// Resolves a backend's API keys and pushes one lane per key onto `priority`.
/// 0/1 key → a single lane (`pool: None`, no `key_id`); N keys → N contiguous
/// lanes sharing one [`CredPool`], each with `key_id = "#i"` so health and
/// rate-limit identities (`name + key_id`) isolate per key.
fn expand_backend(b: BackendDoc, priority: &mut Vec<Backend>) {
    let keys = resolve_keys(&b);
    let rpm = b.rpm.unwrap_or(60.0);
    let burst = b.burst.unwrap_or(5.0);
    let rate = RatePolicy { rpm, burst };
    let mk = |key_id: String, api_key: String, pool: Option<std::sync::Arc<CredPool>>| Backend {
        name: b.name.clone(),
        key_id,
        base_url: b.base_url.clone(),
        model: b.model.clone(),
        api_key,
        anthropic: b.anthropic,
        context_limit: b.context_limit,
        defaults: b.defaults.clone(),
        rate,
        inflight_cap: b.inflight_cap,
        pool,
    };

    match keys.len() {
        0 => priority.push(mk(String::new(), String::new(), None)),
        1 => priority.push(mk(String::new(), keys.into_iter().next().unwrap(), None)),
        _ => {
            let strategy = KeyStrategy::parse(b.key_strategy.as_deref().unwrap_or(""));
            let pool = std::sync::Arc::new(CredPool::new(strategy, b.key_weights.clone()));
            for (i, key) in keys.into_iter().enumerate() {
                priority.push(mk(format!("#{i}"), key, Some(pool.clone())));
            }
        }
    }
}

/// Resolves a backend's key list (inline list → env list → legacy single
/// key/env), deduping while preserving order. Empty entries are dropped.
fn resolve_keys(b: &BackendDoc) -> Vec<String> {
    let mut raw: Vec<String> = Vec::new();
    if !b.api_keys.is_empty() {
        raw.extend(b.api_keys.iter().cloned());
    } else if !b.api_key_envs.is_empty() {
        raw.extend(
            b.api_key_envs
                .iter()
                .map(|e| std::env::var(e).unwrap_or_default()),
        );
    } else if let Some(k) = &b.api_key {
        raw.push(k.clone());
    } else if let Some(env) = &b.api_key_env {
        raw.push(std::env::var(env).unwrap_or_default());
    }
    let mut seen = std::collections::HashSet::new();
    raw.into_iter()
        .map(|k| k.trim().to_string())
        .filter(|k| !k.is_empty() && seen.insert(k.clone()))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_routes_and_defaults() {
        let cfg = parse_config(
            r#"{"routes":[{"name":"standard","backends":[
                {"name":"openrouter","base_url":"https://x/api/v1","model":"ds-pro","api_key":"k","rpm":30,"burst":3}
            ]}]}"#,
        )
        .unwrap();
        assert_eq!(cfg.routes.len(), 1);
        let r = cfg.lookup_route("model-proxy/standard").unwrap();
        assert_eq!(r.priority[0].name, "openrouter");
        assert_eq!(r.priority[0].rate.rpm, 30.0);
        assert_eq!(r.priority[0].rate.burst, 3.0);
    }

    #[test]
    fn empty_doc_is_ok() {
        let cfg = parse_config(r#"{"routes":[]}"#).unwrap();
        assert!(cfg.routes.is_empty());
    }

    #[test]
    fn lookup_route_defaults_to_first() {
        let cfg = parse_config(r#"{"routes":[{"name":"standard","backends":[]}]}"#).unwrap();
        assert_eq!(cfg.lookup_route("unknown-model").unwrap().name, "standard");
    }

    #[test]
    fn multi_key_backend_expands_into_lanes() {
        let cfg = parse_config(
            r#"{"routes":[{"name":"standard","backends":[
                {"name":"minimax","base_url":"https://x","model":"m",
                 "api_keys":["k0","k1","k2"],"key_strategy":"failover"}
            ]}]}"#,
        )
        .unwrap();
        let lanes = &cfg.routes[0].priority;
        assert_eq!(lanes.len(), 3);
        assert_eq!(lanes[0].key_id, "#0");
        assert_eq!(lanes[1].key_id, "#1");
        assert_eq!(lanes[2].key_id, "#2");
        assert_eq!(lanes[0].api_key, "k0");
        assert_eq!(lanes[2].api_key, "k2");
        // Identities isolate per key.
        assert_eq!(lanes[0].identity(), "minimax#0");
        assert_eq!(lanes[1].identity(), "minimax#1");
        // All three share one pool.
        let p0 = lanes[0].pool.as_ref().unwrap();
        assert!(std::sync::Arc::ptr_eq(p0, lanes[1].pool.as_ref().unwrap()));
        assert!(std::sync::Arc::ptr_eq(p0, lanes[2].pool.as_ref().unwrap()));
    }

    #[test]
    fn single_key_stays_one_lane() {
        let cfg = parse_config(
            r#"{"routes":[{"name":"standard","backends":[
                {"name":"openrouter","base_url":"https://x","model":"m","api_key":"k"}
            ]}]}"#,
        )
        .unwrap();
        let lanes = &cfg.routes[0].priority;
        assert_eq!(lanes.len(), 1);
        assert_eq!(lanes[0].key_id, "");
        assert!(lanes[0].pool.is_none());
    }

    #[test]
    fn duplicate_and_blank_keys_are_dropped() {
        let cfg = parse_config(
            r#"{"routes":[{"name":"standard","backends":[
                {"name":"p","base_url":"https://x","model":"m","api_keys":["k0","k0"," ","k1"]}
            ]}]}"#,
        )
        .unwrap();
        let lanes = &cfg.routes[0].priority;
        // k0 deduped, blank dropped → k0, k1.
        assert_eq!(lanes.len(), 2);
        assert_eq!(lanes[0].api_key, "k0");
        assert_eq!(lanes[1].api_key, "k1");
    }

    #[test]
    fn route_strategy_defaults_to_sequential() {
        let cfg = parse_config(r#"{"routes":[{"name":"standard","backends":[]}]}"#).unwrap();
        assert_eq!(cfg.routes[0].strategy, RoutingStrategy::Sequential);
        assert!(cfg.routes[0].order_pool.is_none());
    }

    #[test]
    fn per_route_load_balanced_gets_order_pool() {
        let cfg = parse_config(
            r#"{"routes":[{"name":"standard","strategy":"load_balanced","backends":[]}]}"#,
        )
        .unwrap();
        assert_eq!(cfg.routes[0].strategy, RoutingStrategy::LoadBalanced);
        assert!(cfg.routes[0].order_pool.is_some());
    }

    #[test]
    fn per_route_speculative_has_no_pool() {
        let cfg = parse_config(
            r#"{"routes":[{"name":"standard","strategy":"speculative","backends":[]}]}"#,
        )
        .unwrap();
        assert_eq!(cfg.routes[0].strategy, RoutingStrategy::Speculative);
        assert!(cfg.routes[0].order_pool.is_none());
    }

    // ── Env-bootstrap tests ──────────────────────────────────────────────────
    // Process env is global, so serialize and restore around each test.
    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn with_env<F: FnOnce()>(vars: &[(&str, Option<&str>)], f: F) {
        let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let saved: Vec<(String, Option<String>)> = vars
            .iter()
            .map(|(k, _)| (k.to_string(), std::env::var(k).ok()))
            .collect();
        // SAFETY: serialized by ENV_LOCK; restored below.
        unsafe {
            for (k, v) in vars {
                match v {
                    Some(val) => std::env::set_var(k, val),
                    None => std::env::remove_var(k),
                }
            }
        }
        f();
        unsafe {
            for (k, old) in saved {
                match old {
                    Some(val) => std::env::set_var(&k, val),
                    None => std::env::remove_var(&k),
                }
            }
        }
    }

    #[test]
    fn relay_tunables_from_env() {
        with_env(
            &[
                ("SZPROXY_FIRST_BYTE_TIMEOUT", Some("5")),
                ("SZPROXY_STREAM_IDLE_TIMEOUT", Some("6")),
                ("SZPROXY_STREAM_HEARTBEAT_INTERVAL", Some("7")),
            ],
            || {
                let r = relay_from_env();
                assert_eq!(r.first_byte, Duration::from_secs(5));
                assert_eq!(r.idle, Duration::from_secs(6));
                assert_eq!(r.heartbeat, Duration::from_secs(7));
            },
        );
        with_env(
            &[
                ("SZPROXY_FIRST_BYTE_TIMEOUT", None),
                ("SZPROXY_STREAM_IDLE_TIMEOUT", None),
                ("SZPROXY_STREAM_HEARTBEAT_INTERVAL", None),
                ("MODEL_PROXY_FIRST_BYTE_TIMEOUT", None),
                ("MODEL_PROXY_STREAM_IDLE_TIMEOUT", None),
                ("MODEL_PROXY_STREAM_HEARTBEAT_INTERVAL", None),
            ],
            || {
                let r = relay_from_env();
                assert_eq!(r.first_byte, Duration::from_secs(45)); // Go defaults
                assert_eq!(r.idle, Duration::from_secs(120));
                assert_eq!(r.heartbeat, Duration::from_secs(10));
            },
        );
    }

    #[test]
    fn global_routing_from_env_parses_and_defaults() {
        with_env(&[("SZPROXY_ROUTING", Some("load_balanced"))], || {
            assert_eq!(global_routing_from_env(), RoutingStrategy::LoadBalanced);
        });
        with_env(&[("SZPROXY_ROUTING", Some("nonsense"))], || {
            // config_enum infallible-ish: from_str_validated rejects → default.
            assert_eq!(global_routing_from_env(), RoutingStrategy::Sequential);
        });
        with_env(&[("SZPROXY_ROUTING", None)], || {
            assert_eq!(global_routing_from_env(), RoutingStrategy::Sequential);
        });
    }

    #[test]
    fn resolve_listen_precedence() {
        with_env(&[("SZPROXY_LISTEN", Some("127.0.0.1:9999"))], || {
            assert_eq!(resolve_listen().unwrap(), "127.0.0.1:9999".parse().unwrap());
        });
        with_env(
            &[("SZPROXY_LISTEN", None), ("META_ROUTER_PORT", Some("8080"))],
            || {
                assert_eq!(resolve_listen().unwrap(), "127.0.0.1:8080".parse().unwrap());
            },
        );
        with_env(
            &[("SZPROXY_LISTEN", None), ("META_ROUTER_PORT", None)],
            || {
                assert_eq!(resolve_listen().unwrap(), default_listen());
            },
        );
    }

    #[test]
    fn compression_level_from_env() {
        let doc = CompressionDoc::default();
        with_env(
            &[
                ("SZPROXY_COMPRESS", Some("1")),
                ("SZPROXY_COMPRESS_LEVEL", None),
                ("MODEL_PROXY_COMPRESS_LEVEL", None),
            ],
            || {
                // SZPROXY_COMPRESS=1 with no explicit level → conservative.
                assert_eq!(
                    resolve_compression(Some(&doc), true).level,
                    Level::Conservative
                );
            },
        );
        with_env(
            &[
                ("SZPROXY_COMPRESS", Some("1")),
                ("SZPROXY_COMPRESS_LEVEL", Some("balanced")),
            ],
            || {
                assert_eq!(resolve_compression(Some(&doc), true).level, Level::Balanced);
            },
        );
        with_env(
            &[
                ("SZPROXY_COMPRESS", Some("0")),
                ("SZPROXY_COMPRESS_LEVEL", Some("aggressive")),
            ],
            || {
                // Explicit off wins.
                assert_eq!(resolve_compression(Some(&doc), true).level, Level::Off);
            },
        );
    }

    #[test]
    fn from_env_reads_inline_routes_and_global_strategy() {
        with_env(
            &[
                ("SZPROXY_CONFIG", None),
                (
                    "SZPROXY_ROUTES_JSON",
                    Some(r#"{"routes":[{"name":"standard","backends":[]}]}"#),
                ),
                ("SZPROXY_ROUTING", Some("load_balanced")),
                ("SZPROXY_LISTEN", None),
                ("META_ROUTER_PORT", None),
            ],
            || {
                let cfg = from_env().unwrap();
                assert_eq!(cfg.routes.len(), 1);
                assert_eq!(cfg.routes[0].name, "standard");
                // Global SZPROXY_ROUTING applies when the route omits its own.
                assert_eq!(cfg.routes[0].strategy, RoutingStrategy::LoadBalanced);
                assert_eq!(cfg.listen, default_listen());
            },
        );
    }
}
