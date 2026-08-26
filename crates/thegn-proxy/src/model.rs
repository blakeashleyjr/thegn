//! Runtime routing model: backends, routes, and the resolved config the daemon
//! serves. The pure decision logic lives in `thegn_core::proxy`; these types are
//! the I/O-layer counterparts built from the `[model_proxy]` config registry.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;

use serde_json::{Map, Value};
use thegn_core::config_model_proxy::{BudgetBreach, RoutingStrategy};
use thegn_core::proxy::cost::PriceTable;
use thegn_core::proxy::creds::CredPool;
use thegn_core::proxy::ratelimit::RatePolicy;
use thegn_core::proxy::route_select::AutoTier;

use crate::relay::RelayConfig;

/// The wire protocol a backend speaks. The upstream seam dispatches by this.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Wire {
    /// OpenAI `/v1/chat/completions` (also openai-compatible local runtimes).
    OpenAi,
    /// Anthropic `/v1/messages`.
    Anthropic,
}

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
    /// Which wire protocol this lane speaks.
    pub wire: Wire,
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
    /// Whether this lane speaks the Anthropic surface.
    pub fn is_anthropic(&self) -> bool {
        self.wire == Wire::Anthropic
    }
}

impl std::fmt::Debug for Backend {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Never print api_key.
        f.debug_struct("Backend")
            .field("name", &self.name)
            .field("key_id", &self.key_id)
            .field("model", &self.model)
            .field("wire", &self.wire)
            .finish()
    }
}

/// A named priority chain of backends (e.g. `standard`, `fast`, `heavy`).
#[derive(Clone)]
pub struct Route {
    pub name: String,
    pub priority: Vec<Backend>,
    /// How backend *slots* are ordered per request.
    pub strategy: RoutingStrategy,
    /// Round-robin cursor over slots; `Some` only for `LoadBalanced`.
    pub order_pool: Option<Arc<CredPool>>,
    /// Borrow other routes' lanes as a final tier when this route is exhausted.
    pub last_resort: bool,
}

/// Per-scope budget enforcement settings resolved from `[model_proxy.budget]`.
#[derive(Clone, Debug)]
pub struct BudgetSettings {
    pub enabled: bool,
    pub on_breach: BudgetBreach,
    /// Rolling-window length in millis (0 = cumulative, never rolls over).
    pub window_len_ms: i64,
    /// Per-scope ceilings: `(token_cap, cost_cap)`, either optional.
    pub scopes: HashMap<String, (Option<i64>, Option<f64>)>,
}

impl Default for BudgetSettings {
    fn default() -> Self {
        BudgetSettings {
            enabled: false,
            on_breach: BudgetBreach::Warn,
            window_len_ms: 0,
            scopes: HashMap::new(),
        }
    }
}

impl BudgetSettings {
    /// Whether a breach refuses (vs downgrades). Warn never blocks.
    pub fn refuses(&self) -> bool {
        self.enabled && self.on_breach == BudgetBreach::Refuse
    }
    /// Whether a breach downgrades to the cheapest lane.
    pub fn downgrades(&self) -> bool {
        self.enabled && self.on_breach == BudgetBreach::Downgrade
    }
}

/// The resolved proxy configuration the daemon serves.
#[derive(Clone)]
pub struct ProxyConfig {
    pub listen: SocketAddr,
    pub routes: Vec<Route>,
    /// Streaming relay tunables (TTFB / idle / heartbeat).
    pub relay: RelayConfig,
    /// Model/tier aliasing: extra client model ids that select a route, checked
    /// before route names. The reserved value `@auto` selects the classifier.
    pub aliases: HashMap<String, String>,
    /// When a route's whole chain fails, try the deduped union of every OTHER
    /// route's backends as a last resort (skipping identities already tried).
    pub last_resort: bool,
    /// Factor `[usage]` account headroom into lane ordering.
    pub usage_aware: bool,
    /// Tiers the deterministic `auto` classifier may pick from.
    pub auto_tiers: Vec<AutoTier>,
    /// Cost estimation table (built from the provider registry).
    pub price_table: PriceTable,
    /// Budget enforcement settings.
    pub budget: BudgetSettings,
}

/// The reserved alias value that selects the deterministic `auto` classifier.
pub const AUTO_TARGET: &str = "@auto";

impl ProxyConfig {
    /// Resolves a client-requested model to a route: alias map first (exact
    /// client id, then with the `model-proxy/` prefix stripped), then a
    /// `model-proxy/<name>`/bare `<name>` route-name match, then the first route
    /// as the default. Returns `None` only for an empty route table. When the
    /// resolved alias target is `@auto`, `select` picks a tier from features.
    pub fn lookup_route(&self, model: &str) -> Option<&Route> {
        let name = model.strip_prefix("model-proxy/").unwrap_or(model);
        let target = self
            .aliases
            .get(model)
            .or_else(|| self.aliases.get(name))
            .map(String::as_str)
            .unwrap_or(name);
        self.routes
            .iter()
            .find(|r| r.name == target)
            .or_else(|| self.routes.first())
    }

    /// Whether the client's requested model resolves (via alias) to `@auto`.
    pub fn is_auto(&self, model: &str) -> bool {
        let name = model.strip_prefix("model-proxy/").unwrap_or(model);
        self.aliases
            .get(model)
            .or_else(|| self.aliases.get(name))
            .map(String::as_str)
            == Some(AUTO_TARGET)
    }

    /// All route names, for `/v1/models`.
    pub fn route_names(&self) -> Vec<String> {
        self.routes.iter().map(|r| r.name.clone()).collect()
    }
}
