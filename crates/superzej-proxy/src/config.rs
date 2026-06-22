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
use superzej_core::proxy::compress::{Level, Limits};
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
    let routes = doc.map(build_routes).transpose()?.unwrap_or_default();
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
    Ok(ProxyConfig {
        listen: default_listen(),
        routes: build_routes(doc)?,
        relay: RelayConfig::default(),
        compression,
    })
}

fn parse_doc(raw: &str) -> Result<ConfigDoc> {
    serde_json::from_str(raw).context("parse proxy config JSON")
}

fn build_routes(doc: ConfigDoc) -> Result<Vec<Route>> {
    let mut routes = Vec::new();
    for r in doc.routes {
        let mut priority = Vec::new();
        for b in r.backends {
            let api_key = match (&b.api_key, &b.api_key_env) {
                (Some(k), _) => k.clone(),
                (None, Some(env)) => std::env::var(env).unwrap_or_default(),
                (None, None) => String::new(),
            };
            let rpm = b.rpm.unwrap_or(60.0);
            let burst = b.burst.unwrap_or(5.0);
            priority.push(Backend {
                name: b.name,
                key_id: String::new(),
                base_url: b.base_url,
                model: b.model,
                api_key,
                anthropic: b.anthropic,
                context_limit: b.context_limit,
                defaults: b.defaults,
                rate: RatePolicy { rpm, burst },
                inflight_cap: b.inflight_cap,
                pool: None,
            });
        }
        routes.push(Route {
            name: r.name,
            priority,
        });
    }
    Ok(routes)
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
}
