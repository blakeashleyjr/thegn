//! Resolves the runtime [`ProxyConfig`] the daemon serves.
//!
//! The host hands the daemon its `[model_proxy]` section as a serialized
//! [`ModelProxyConfig`] via the `THEGN_MODEL_PROXY_CONFIG` env var (a temp file
//! path). SecretRefs travel by reference — the host never resolves keys — so this
//! module resolves `env:`/`file:` refs *inside the proxy process*, holds the
//! material in memory only, and never logs or persists it.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use serde_json::{Map, Value};
use thegn_core::config_model_proxy::{
    ModelProxyConfig, ProviderEntry, RouteEntry, RoutingStrategy,
};
use thegn_core::proxy::cost::{PricePoint, PriceTable};
use thegn_core::proxy::creds::{CredPool, KeyStrategy};
use thegn_core::proxy::ratelimit::RatePolicy;
use thegn_core::proxy::route_select::AutoTier;
use thegn_core::seam::Kind;

use crate::model::{Backend, BudgetSettings, ProxyConfig, Route, Wire};
use crate::relay::RelayConfig;

/// Env var carrying the path to the serialized `[model_proxy]` config.
pub const CONFIG_ENV: &str = "THEGN_MODEL_PROXY_CONFIG";

/// Loads and resolves the proxy config from the host-provided file.
pub fn from_env() -> Result<ProxyConfig> {
    let path = std::env::var(CONFIG_ENV)
        .with_context(|| format!("{CONFIG_ENV} must point at the resolved model-proxy config"))?;
    let raw = std::fs::read_to_string(&path).with_context(|| format!("read {path}"))?;
    let cfg: ModelProxyConfig = serde_json::from_str(&raw).context("parse model-proxy config")?;
    build(&cfg)
}

/// Builds the runtime routing model from a `[model_proxy]` section, resolving
/// SecretRefs and expanding multi-key providers into lanes.
pub fn build(cfg: &ModelProxyConfig) -> Result<ProxyConfig> {
    let listen: SocketAddr = cfg
        .listen
        .parse()
        .with_context(|| format!("parse listen {:?}", cfg.listen))?;

    // Provider registry, keyed by name. Reserved kinds are excluded from routing.
    let providers: HashMap<&str, &ProviderEntry> = cfg
        .providers
        .iter()
        .filter(|p| !p.kind.is_reserved())
        .map(|p| (p.name.as_str(), p))
        .collect();

    let mut price_table = PriceTable::new();
    let mut routes = Vec::new();
    for r in &cfg.routes {
        routes.push(build_route(r, cfg.routing, &providers, &mut price_table));
    }

    let auto_tiers: Vec<AutoTier> = cfg
        .routes
        .iter()
        .map(|r| AutoTier {
            name: r.name.clone(),
            max_tokens: r.auto_max_tokens,
        })
        .collect();

    let budget = BudgetSettings {
        enabled: cfg.budget.enabled,
        on_breach: cfg.budget.on_breach,
        window_len_ms: cfg.budget.window_len_ms(),
        scopes: cfg
            .budget
            .scopes
            .iter()
            .map(|(k, v)| (k.clone(), (v.tokens, v.cost_usd)))
            .collect(),
    };

    let last_resort = cfg.last_resort || cfg.routes.iter().any(|r| r.last_resort);

    Ok(ProxyConfig {
        listen,
        routes,
        relay: RelayConfig {
            first_byte: Duration::from_secs(cfg.first_byte_timeout_secs),
            idle: Duration::from_secs(cfg.idle_timeout_secs),
            heartbeat: Duration::from_secs(cfg.heartbeat_secs),
        },
        aliases: cfg
            .aliases
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect(),
        last_resort,
        usage_aware: cfg.usage_aware,
        auto_tiers,
        price_table,
        budget,
    })
}

/// Builds one route: expands each (provider, model) backend into lanes, drops
/// backends whose provider is unknown/reserved, and records pricing.
fn build_route(
    r: &RouteEntry,
    global_routing: RoutingStrategy,
    providers: &HashMap<&str, &ProviderEntry>,
    price_table: &mut PriceTable,
) -> Route {
    let strategy = r.strategy.unwrap_or(global_routing);
    let mut priority = Vec::new();
    for b in &r.backends {
        let Some(p) = providers.get(b.provider.as_str()) else {
            // Unknown or reserved provider — the lane is dropped (validated at
            // config time; here we fail safe by simply not routing to it).
            continue;
        };
        if p.is_cost_bearing() {
            price_table.add_cost_bearing(&p.name);
            price_table.set_price(
                format!("{}:{}", p.name, b.model),
                PricePoint {
                    input_usd_per_mtok: p.input_usd_per_mtok,
                    output_usd_per_mtok: p.output_usd_per_mtok,
                },
            );
        }
        expand_backend(p, &b.model, &mut priority);
    }
    let order_pool = (strategy == RoutingStrategy::LoadBalanced)
        .then(|| Arc::new(CredPool::new(KeyStrategy::RoundRobin, vec![])));
    Route {
        name: r.name.clone(),
        priority,
        strategy,
        order_pool,
        last_resort: r.last_resort,
    }
}

/// Resolves a provider's keys and pushes one lane per key. 0/1 key → one lane
/// (`pool: None`); N keys → N contiguous lanes sharing one [`CredPool`].
fn expand_backend(p: &ProviderEntry, model: &str, priority: &mut Vec<Backend>) {
    let keys = resolve_keys(p);
    let wire = match p.kind {
        thegn_core::config_model_proxy::ModelProviderKind::Anthropic => Wire::Anthropic,
        _ => Wire::OpenAi, // openai and openai-compat (reserved kinds already dropped)
    };
    let rate = RatePolicy {
        rpm: p.rpm,
        burst: p.burst,
    };
    let defaults: Map<String, Value> = p
        .defaults
        .iter()
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect();
    let mk = |key_id: String, api_key: String, pool: Option<Arc<CredPool>>| Backend {
        name: p.name.clone(),
        key_id,
        base_url: p.base_url.clone(),
        model: model.to_string(),
        api_key,
        wire,
        context_limit: p.context_limit,
        defaults: defaults.clone(),
        rate,
        inflight_cap: p.inflight_cap,
        pool,
    };

    match keys.len() {
        0 => priority.push(mk(String::new(), String::new(), None)),
        1 => priority.push(mk(String::new(), keys.into_iter().next().unwrap(), None)),
        _ => {
            let strategy = KeyStrategy::parse(&p.key_strategy);
            let pool = Arc::new(CredPool::new(strategy, p.key_weights.clone()));
            for (i, key) in keys.into_iter().enumerate() {
                priority.push(mk(format!("#{i}"), key, Some(pool.clone())));
            }
        }
    }
}

/// Resolves a provider's SecretRefs into key material, deduping while preserving
/// order and dropping unresolvable/empty entries. `subscription`/keyless
/// providers legitimately resolve to zero keys.
fn resolve_keys(p: &ProviderEntry) -> Vec<String> {
    let mut seen = std::collections::HashSet::new();
    let mut out = Vec::new();
    for raw in std::iter::once(&p.api_key).chain(p.api_keys.iter()) {
        let Some(key) = resolve_ref(raw) else {
            continue;
        };
        if seen.insert(key.clone()) {
            out.push(key);
        }
    }
    out
}

/// Resolves one SecretRef string (`env:VAR` / `file:PATH`) to its value. A raw
/// literal is rejected at config-validate time and ignored here; keyring refs
/// are not resolvable from the daemon process.
pub fn resolve_ref(raw: &str) -> Option<String> {
    let raw = raw.trim();
    if let Some(var) = raw.strip_prefix("env:") {
        std::env::var(var.trim()).ok().filter(|v| !v.is_empty())
    } else if let Some(path) = raw.strip_prefix("file:") {
        std::fs::read_to_string(path.trim())
            .ok()
            .map(|s| s.trim().to_string())
            .filter(|v| !v.is_empty())
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use thegn_core::config_model_proxy::{ProviderEntry, RouteBackend};

    fn base_cfg() -> ModelProxyConfig {
        let mut c = ModelProxyConfig::default();
        c.enabled = true;
        c
    }

    #[test]
    fn builds_single_lane_openai_route() {
        // SAFETY: serialized within this test; no other test reads BUILD_K1.
        unsafe { std::env::set_var("BUILD_K1", "secret-value") };
        let mut c = base_cfg();
        c.providers.push(ProviderEntry {
            name: "openrouter".into(),
            kind: thegn_core::config_model_proxy::ModelProviderKind::Openai,
            base_url: "https://openrouter.ai/api/v1".into(),
            api_key: "env:BUILD_K1".into(),
            input_usd_per_mtok: 0.27,
            output_usd_per_mtok: 1.1,
            ..Default::default()
        });
        c.routes.push(RouteEntry {
            name: "standard".into(),
            backends: vec![RouteBackend {
                provider: "openrouter".into(),
                model: "deepseek/v4".into(),
            }],
            ..Default::default()
        });
        let pc = build(&c).unwrap();
        assert_eq!(pc.routes.len(), 1);
        let lanes = &pc.routes[0].priority;
        assert_eq!(lanes.len(), 1);
        assert_eq!(lanes[0].name, "openrouter");
        assert_eq!(lanes[0].api_key, "secret-value");
        assert_eq!(lanes[0].wire, Wire::OpenAi);
        assert_eq!(pc.auto_tiers.len(), 1);
        unsafe { std::env::remove_var("BUILD_K1") };
    }

    #[test]
    fn reserved_provider_is_excluded() {
        let mut c = base_cfg();
        c.providers.push(ProviderEntry {
            name: "g".into(),
            kind: thegn_core::config_model_proxy::ModelProviderKind::Gemini, // reserved
            base_url: "https://x".into(),
            ..Default::default()
        });
        c.routes.push(RouteEntry {
            name: "standard".into(),
            backends: vec![RouteBackend {
                provider: "g".into(),
                model: "gemini-2".into(),
            }],
            ..Default::default()
        });
        let pc = build(&c).unwrap();
        // The reserved provider's lane is dropped.
        assert!(pc.routes[0].priority.is_empty());
    }

    #[test]
    fn subscription_provider_needs_no_key() {
        let mut c = base_cfg();
        c.providers.push(ProviderEntry {
            name: "ollama".into(),
            kind: thegn_core::config_model_proxy::ModelProviderKind::Openai,
            base_url: "http://127.0.0.1:11434/v1".into(),
            subscription: true,
            ..Default::default()
        });
        c.routes.push(RouteEntry {
            name: "local".into(),
            backends: vec![RouteBackend {
                provider: "ollama".into(),
                model: "qwen3".into(),
            }],
            ..Default::default()
        });
        let pc = build(&c).unwrap();
        assert_eq!(pc.routes[0].priority.len(), 1);
        assert!(pc.routes[0].priority[0].api_key.is_empty());
    }

    #[test]
    fn resolve_ref_env_and_file() {
        unsafe { std::env::set_var("RR_ENV", "v") };
        assert_eq!(resolve_ref("env:RR_ENV").as_deref(), Some("v"));
        assert_eq!(resolve_ref("env:MISSING_VAR_XYZ"), None);
        assert_eq!(resolve_ref("sk-literal"), None); // raw literal ignored
        unsafe { std::env::remove_var("RR_ENV") };
    }
}
