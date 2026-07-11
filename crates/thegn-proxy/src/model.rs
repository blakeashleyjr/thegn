//! Runtime routing model: backends, routes, and the resolved config the daemon
//! serves. The pure decision logic lives in `thegn_core::proxy`; these types
//! are the I/O-layer counterparts of the Go `Backend` / `Route` structs.

use std::net::SocketAddr;
use std::sync::Arc;

use serde_json::{Map, Value};
use thegn_core::proxy::creds::CredPool;
use thegn_core::proxy::ratelimit::RatePolicy;

/// One upstream lane. Backends sharing a `name` + `key_id` share a rate-limit
/// and health identity (so a multi-key provider's keys cool independently).
#[derive(Clone)]
pub struct Backend {
    /// Provider name (groups same-account models for rate limiting).
    pub name: String,
    /// Per-key suffix (`"#0"`, `"#1"`, …) or empty for single-key/no-key lanes.
    pub key_id: String,
    /// Upstream base URL, e.g. `https://openrouter.ai/api/v1`.
    pub base_url: String,
    /// Model id to send upstream (may differ from the client's requested model).
    pub model: String,
    /// API key sent to the upstream (empty for OAuth sidecars / keyless).
    pub api_key: String,
    /// Whether the upstream speaks the Anthropic `/v1/messages` surface (so the
    /// proxy translates OpenAI⇄Anthropic around it).
    pub anthropic: bool,
    /// Known context window in tokens; 0 means unknown (never skipped).
    pub context_limit: usize,
    /// Per-backend default body params injected for keys the caller didn't set.
    pub defaults: Map<String, Value>,
    /// Resolved rate policy for this lane's identity.
    pub rate: RatePolicy,
    /// In-flight concurrency cap (0 = unlimited).
    pub inflight_cap: u32,
    /// Shared rotation pool when this lane is one of several keys for a provider.
    pub pool: Option<Arc<CredPool>>,
}

impl Backend {
    /// Health + rate-limit identity: name plus per-key suffix.
    pub fn identity(&self) -> String {
        format!("{}{}", self.name, self.key_id)
    }
}

impl std::fmt::Debug for Backend {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Never print api_key.
        f.debug_struct("Backend")
            .field("name", &self.name)
            .field("key_id", &self.key_id)
            .field("model", &self.model)
            .field("anthropic", &self.anthropic)
            .finish()
    }
}

/// A named priority chain of backends (e.g. `standard`, `fast`, `free`).
#[derive(Clone)]
pub struct Route {
    pub name: String,
    pub priority: Vec<Backend>,
    /// How backend *slots* are ordered per request (the M4 key-level pool order
    /// applies within each slot).
    pub strategy: thegn_core::config::RoutingStrategy,
    /// Round-robin cursor over slots; `Some` only for `LoadBalanced`.
    pub order_pool: Option<std::sync::Arc<thegn_core::proxy::creds::CredPool>>,
}

/// The resolved proxy configuration the daemon serves.
#[derive(Clone)]
pub struct ProxyConfig {
    pub listen: SocketAddr,
    pub routes: Vec<Route>,
    /// Streaming relay tunables (TTFB / idle / heartbeat).
    pub relay: crate::relay::RelayConfig,
    /// In-flight token-reduction policy (group W).
    pub compression: thegn_core::proxy::transform::CompressPolicy,
}

impl ProxyConfig {
    /// Resolves a client-requested model to a route. A `model-proxy/<name>` or
    /// bare `<name>` matching a route name selects it; otherwise the first route
    /// is the default. Mirrors the Go `lookupRoute` intent.
    pub fn lookup_route(&self, model: &str) -> Option<&Route> {
        let name = model.strip_prefix("model-proxy/").unwrap_or(model);
        self.routes
            .iter()
            .find(|r| r.name == name)
            .or_else(|| self.routes.first())
    }

    /// All route names, for `/v1/models`.
    pub fn route_names(&self) -> Vec<String> {
        self.routes.iter().map(|r| r.name.clone()).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use thegn_core::proxy::ratelimit::RatePolicy;

    fn backend(name: &str, key_id: &str) -> Backend {
        Backend {
            name: name.into(),
            key_id: key_id.into(),
            base_url: "http://x".into(),
            model: "m".into(),
            api_key: "super-secret-key".into(),
            anthropic: false,
            context_limit: 0,
            defaults: serde_json::Map::new(),
            rate: RatePolicy {
                rpm: 60.0,
                burst: 5.0,
            },
            inflight_cap: 0,
            pool: None,
        }
    }

    #[test]
    fn identity_combines_name_and_key_id() {
        assert_eq!(backend("openrouter", "#1").identity(), "openrouter#1");
        assert_eq!(backend("codex", "").identity(), "codex");
    }

    #[test]
    fn debug_never_leaks_api_key() {
        let dbg = format!("{:?}", backend("p", "#0"));
        assert!(dbg.contains("\"p\""));
        assert!(dbg.contains("#0"));
        assert!(
            !dbg.contains("super-secret-key"),
            "api_key leaked into Debug: {dbg}"
        );
    }

    #[test]
    fn lookup_route_strips_prefix_and_defaults_to_first() {
        let cfg = ProxyConfig {
            listen: "127.0.0.1:0".parse().unwrap(),
            routes: vec![
                Route {
                    name: "standard".into(),
                    priority: vec![],
                    strategy: Default::default(),
                    order_pool: None,
                },
                Route {
                    name: "fast".into(),
                    priority: vec![],
                    strategy: Default::default(),
                    order_pool: None,
                },
            ],
            relay: crate::relay::RelayConfig::default(),
            compression: thegn_core::proxy::transform::CompressPolicy::off(),
        };
        assert_eq!(cfg.lookup_route("model-proxy/fast").unwrap().name, "fast");
        assert_eq!(
            cfg.lookup_route("anything-unknown").unwrap().name,
            "standard"
        );
        assert_eq!(cfg.route_names(), vec!["standard", "fast"]);
    }
}
