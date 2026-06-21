//! Runtime configuration for the standalone daemon.
//!
//! Milestone 1 loads routes from a JSON document (file via `SZPROXY_CONFIG`, or
//! inline via `SZPROXY_ROUTES_JSON`) so the daemon is usable on its own for
//! validation. Stage E folds this into superzej's layered `[llm_proxy]` config
//! (`config_enum!`-based) and maps the legacy `MODEL_PROXY_*` env knobs; the
//! JSON shape here is the same data those layers will produce.

use std::net::SocketAddr;

use anyhow::{Context, Result};
use serde::Deserialize;
use serde_json::{Map, Value};
use superzej_core::proxy::ratelimit::RatePolicy;

use crate::model::{Backend, ProxyConfig, Route};

#[derive(Debug, Deserialize)]
struct ConfigDoc {
    #[serde(default)]
    routes: Vec<RouteDoc>,
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

/// Reads `SZPROXY_LISTEN` (or `META_ROUTER_PORT`) and the routes document.
pub fn from_env() -> Result<ProxyConfig> {
    let listen = resolve_listen()?;
    let doc = load_doc()?;
    let routes = doc.map(build_routes).transpose()?.unwrap_or_default();
    Ok(ProxyConfig { listen, routes })
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
    Ok(ProxyConfig {
        listen: default_listen(),
        routes: build_routes(doc)?,
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
