//! The `[model_proxy]` config family — the resurrected model proxy's opt-in,
//! TOML-native provider registry and route/tier table.
//!
//! Kept in a sibling module (rather than the god-file `config.rs`), which
//! re-exports everything here. **Nothing runs unless `enabled = true`.** A stale
//! pre-alpha `[llm_proxy]` section is *not* this section — it keeps its
//! tolerated-and-warned behavior and never enables the resurrected proxy.
//!
//! This is the config surface: providers/routes/aliases/budget as schema'd,
//! validated, layered thegn config. The daemon (`thegn-proxy`) resolves this
//! into its runtime routing model; `api_key` SecretRefs travel by reference and
//! are resolved inside the proxy process, never here.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::config::{config_enum, config_warn};
use crate::secretref::{BareAs, SecretRef};

config_enum! {
    /// Which wire protocol a `[[model_proxy.providers]]` entry speaks. `anthropic`
    /// (`/v1/messages`) and `openai` (`/v1/chat/completions`, covering
    /// openai-compatible local runtimes like Ollama/vLLM) are implemented; the
    /// rest are reserved until their adapters land.
    pub enum ModelProviderKind : "model provider" {
        Anthropic = "anthropic",
        Openai    = "openai" | "openai-compat" | "oai",
        // Reserved until a native adapter lands (openai-compat gateways can
        // often be reached today via kind = "openai" against their base URL).
        Gemini    = "gemini" | "google" reserved,
        Bedrock   = "bedrock" reserved,
        Vertex    = "vertex" reserved,
    } default = Openai;
}

config_enum! {
    /// How a route's backend lanes are ordered per request. `cost_aware` is the
    /// successor of the pre-alpha `speculative` (accepted as an alias).
    pub enum RoutingStrategy : "routing strategy" {
        Sequential   = "sequential",
        LoadBalanced = "load_balanced" | "loadbalanced" | "load-balanced",
        CostAware    = "cost_aware" | "cost-aware" | "speculative",
    } default = Sequential;
}

config_enum! {
    /// What a per-scope budget does when its ceiling is crossed.
    pub enum BudgetBreach : "budget on_breach" {
        Warn      = "warn",
        Refuse    = "refuse",
        Downgrade = "downgrade",
    } default = Warn;
}

/// A `[[model_proxy.providers]]` entry — one upstream, declared once. Both config
/// validation and runtime client construction derive from this single registry.
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize, schemars::JsonSchema)]
#[serde(default)]
pub struct ProviderEntry {
    /// Stable provider id referenced by `[[model_proxy.routes]].backends`.
    pub name: String,
    /// Wire protocol (implemented or reserved).
    pub kind: ModelProviderKind,
    /// Upstream base URL, e.g. `https://api.anthropic.com` or an Ollama endpoint.
    pub base_url: String,
    /// API key as a SecretRef (`env:VAR` / `file:PATH`). A raw literal is
    /// rejected by `thegn config validate` and refuses proxy start.
    pub api_key: String,
    /// Optional multi-key lanes (each its own SecretRef), spread into rate- and
    /// health-isolated lanes sharing one rotation pool.
    pub api_keys: Vec<String>,
    /// Lane rotation strategy: `roundrobin` | `failover` | `random` | `weighted`.
    pub key_strategy: String,
    /// Per-lane weights aligned to the resolved key order (weighted strategy).
    pub key_weights: Vec<u32>,
    /// Requests per minute for this provider's shared-quota identity.
    pub rpm: f64,
    /// Token-bucket burst.
    pub burst: f64,
    /// In-flight concurrency cap per identity (0 = unlimited).
    pub inflight_cap: u32,
    /// Known context window in tokens (0 = unknown, never skipped).
    pub context_limit: usize,
    /// Price per 1M input tokens (USD). Non-zero (and not `subscription`) marks
    /// the provider cost-bearing.
    pub input_usd_per_mtok: f64,
    /// Price per 1M output tokens (USD).
    pub output_usd_per_mtok: f64,
    /// Flat-rate subscription/OAuth lane — accounts every request at $0 marginal.
    pub subscription: bool,
    /// Per-provider default request-body params merged for keys the caller left
    /// unset (caller values always win).
    pub defaults: BTreeMap<String, serde_json::Value>,
}

impl Default for ProviderEntry {
    fn default() -> Self {
        ProviderEntry {
            name: String::new(),
            kind: ModelProviderKind::Openai,
            base_url: String::new(),
            api_key: String::new(),
            api_keys: Vec::new(),
            key_strategy: String::new(),
            key_weights: Vec::new(),
            rpm: 60.0,
            burst: 5.0,
            inflight_cap: 0,
            context_limit: 0,
            input_usd_per_mtok: 0.0,
            output_usd_per_mtok: 0.0,
            subscription: false,
            defaults: BTreeMap::new(),
        }
    }
}

impl ProviderEntry {
    /// Whether this provider is priced per-token (has a rate and isn't flat-rate).
    pub fn is_cost_bearing(&self) -> bool {
        !self.subscription && (self.input_usd_per_mtok > 0.0 || self.output_usd_per_mtok > 0.0)
    }

    /// The SecretRefs this provider declares (single `api_key` plus any
    /// `api_keys`), in order, skipping empty entries.
    pub fn secret_refs(&self) -> Vec<SecretRef> {
        let mut out = Vec::new();
        for raw in std::iter::once(&self.api_key).chain(self.api_keys.iter()) {
            if raw.trim().is_empty() {
                continue;
            }
            out.push(SecretRef::parse(raw, BareAs::Literal));
        }
        out
    }
}

/// One (provider, model) lane inside a route.
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize, schemars::JsonSchema)]
#[serde(default)]
pub struct RouteBackend {
    /// Provider name (must match a `[[model_proxy.providers]]` entry).
    pub provider: String,
    /// Upstream model id to send.
    pub model: String,
}

impl Default for RouteBackend {
    fn default() -> Self {
        RouteBackend {
            provider: String::new(),
            model: String::new(),
        }
    }
}

/// A `[[model_proxy.routes]]` entry — a named tier: an ordered priority list of
/// backends. The client selects it by `model-proxy/<name>` or an alias.
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize, schemars::JsonSchema)]
#[serde(default)]
pub struct RouteEntry {
    /// Tier name; client model id `model-proxy/<name>`.
    pub name: String,
    /// Prioritized backend lanes.
    pub backends: Vec<RouteBackend>,
    /// Per-route strategy override; unset ⇒ the global `routing`.
    pub strategy: Option<RoutingStrategy>,
    /// Borrow other routes' lanes as a final tier when this route is exhausted.
    pub last_resort: bool,
    /// Upper bound of estimated prompt tokens the `auto` classifier will send to
    /// this tier (`None` = unbounded catch-all).
    pub auto_max_tokens: Option<usize>,
}

impl Default for RouteEntry {
    fn default() -> Self {
        RouteEntry {
            name: String::new(),
            backends: Vec::new(),
            strategy: None,
            last_resort: false,
            auto_max_tokens: None,
        }
    }
}

/// A single scope's budget ceiling.
#[derive(Debug, Clone, PartialEq, Default, Deserialize, Serialize, schemars::JsonSchema)]
#[serde(default)]
pub struct BudgetLimit {
    /// Token ceiling for the rolling window (None = no token cap).
    pub tokens: Option<i64>,
    /// USD ceiling for the rolling window (None = no cost cap).
    pub cost_usd: Option<f64>,
}

/// `[model_proxy.budget]` — per-scope spend ceilings over a rolling window.
/// Absent/disabled ⇒ accounting only (spend is still recorded, never enforced).
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize, schemars::JsonSchema)]
#[serde(default)]
pub struct BudgetConfig {
    /// Turn enforcement on. When false, budgets are accounting-only.
    pub enabled: bool,
    /// What a breach does: warn (default, never blocks) | refuse | downgrade.
    pub on_breach: BudgetBreach,
    /// Rolling-window length in seconds (0 = cumulative, never rolls over).
    pub window_secs: u64,
    /// Per-scope ceilings, keyed by scope (`global`, `agent:<n>`,
    /// `worktree:<path>`, `workspace:<repo>`, `zone:<n>`).
    pub scopes: BTreeMap<String, BudgetLimit>,
}

impl Default for BudgetConfig {
    fn default() -> Self {
        BudgetConfig {
            enabled: false,
            on_breach: BudgetBreach::Warn,
            window_secs: 0,
            scopes: BTreeMap::new(),
        }
    }
}

impl BudgetConfig {
    /// The rolling-window length in millis (0 = cumulative).
    pub fn window_len_ms(&self) -> i64 {
        (self.window_secs as i64).saturating_mul(1000)
    }
}

/// The `[model_proxy]` section.
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize, schemars::JsonSchema)]
#[serde(default)]
pub struct ModelProxyConfig {
    /// Opt-in switch. Nothing runs, spawns, writes, or renders when false.
    pub enabled: bool,
    /// Listen address. Loopback default; a non-loopback bind draws a warning.
    pub listen: String,
    /// Global lane-ordering strategy (per-route `strategy` overrides it).
    pub routing: RoutingStrategy,
    /// Factor `[usage]` account headroom into lane ordering.
    pub usage_aware: bool,
    /// Enable the cross-route last-resort tier globally.
    pub last_resort: bool,
    /// Streaming first-byte (peek/commit) budget, seconds.
    pub first_byte_timeout_secs: u64,
    /// Committed-stream idle watchdog, seconds.
    pub idle_timeout_secs: u64,
    /// Streaming keep-alive cadence, seconds.
    pub heartbeat_secs: u64,
    /// The provider registry.
    pub providers: Vec<ProviderEntry>,
    /// The route/tier table.
    pub routes: Vec<RouteEntry>,
    /// Extra client model ids → route names (e.g. `"gpt-5" = "standard"`). The
    /// reserved value `"@auto"` selects the deterministic tier classifier.
    pub aliases: BTreeMap<String, String>,
    /// Optional per-scope budgets.
    pub budget: BudgetConfig,
}

impl Default for ModelProxyConfig {
    fn default() -> Self {
        ModelProxyConfig {
            enabled: false,
            listen: "127.0.0.1:8383".to_string(),
            routing: RoutingStrategy::Sequential,
            usage_aware: false,
            last_resort: false,
            first_byte_timeout_secs: 45,
            idle_timeout_secs: 120,
            heartbeat_secs: 10,
            providers: Vec::new(),
            routes: Vec::new(),
            aliases: BTreeMap::new(),
            budget: BudgetConfig::default(),
        }
    }
}

/// The reserved alias value that selects the deterministic `auto` tier classifier.
pub const AUTO_ALIAS: &str = "@auto";

impl ModelProxyConfig {
    /// Whether `listen` binds a loopback (or unspecified-but-local) address.
    pub fn listen_is_loopback(&self) -> bool {
        match self.listen.parse::<std::net::SocketAddr>() {
            Ok(addr) => addr.ip().is_loopback(),
            // Unparseable is caught by `validate`; treat as non-loopback here so
            // the advisory still fires.
            Err(_) => false,
        }
    }

    /// Hard validation errors (each fails `thegn config validate`).
    pub fn validate(&self) -> Vec<String> {
        let mut errs = Vec::new();
        if !self.enabled {
            // A disabled section is never validated further — it changes nothing.
            return errs;
        }
        if self.listen.parse::<std::net::SocketAddr>().is_err() {
            errs.push(format!(
                "[model_proxy] listen = {:?} is not a valid host:port",
                self.listen
            ));
        }
        // SecretRef-only keys: a raw literal is refused, naming the provider.
        for p in &self.providers {
            for raw in std::iter::once(&p.api_key).chain(p.api_keys.iter()) {
                let raw = raw.trim();
                if raw.is_empty() {
                    continue;
                }
                if SecretRef::parse(raw, BareAs::Literal).is_literal() {
                    errs.push(format!(
                        "[[model_proxy.providers]] name = {:?}: api_key must be a secret \
                         reference (env:VAR or file:PATH), not a raw literal",
                        p.name
                    ));
                }
            }
        }
        // Every route backend must reference a declared provider.
        let known: std::collections::HashSet<&str> =
            self.providers.iter().map(|p| p.name.as_str()).collect();
        for r in &self.routes {
            for b in &r.backends {
                if !known.contains(b.provider.as_str()) {
                    errs.push(format!(
                        "[[model_proxy.routes]] name = {:?}: backend references unknown \
                         provider {:?}",
                        r.name, b.provider
                    ));
                }
            }
        }
        // Aliases must target a real route (or the reserved @auto).
        let route_names: std::collections::HashSet<&str> =
            self.routes.iter().map(|r| r.name.as_str()).collect();
        for (alias, target) in &self.aliases {
            if target != AUTO_ALIAS && !route_names.contains(target.as_str()) {
                errs.push(format!(
                    "[model_proxy.aliases] {alias:?} → {target:?} names no route (or @auto)"
                ));
            }
        }
        errs
    }

    /// Soft advisories (surfaced by `thegn doctor` and at startup, never fatal).
    pub fn warnings(&self) -> Vec<String> {
        let mut out = Vec::new();
        if self.enabled && !self.listen_is_loopback() {
            out.push(format!(
                "[model_proxy] listen = {:?} is not loopback — the endpoint meters real \
                 spend and holds no auth beyond attribution keys; expose it deliberately",
                self.listen
            ));
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn provider(name: &str, key: &str) -> ProviderEntry {
        ProviderEntry {
            name: name.into(),
            base_url: "https://api.example.com".into(),
            api_key: key.into(),
            ..Default::default()
        }
    }

    #[test]
    fn kinds_and_strategies_parse() {
        assert_eq!(
            ModelProviderKind::from_str_validated("anthropic"),
            Ok(ModelProviderKind::Anthropic)
        );
        assert_eq!(
            ModelProviderKind::from_str_validated("oai"),
            Ok(ModelProviderKind::Openai)
        );
        assert!(ModelProviderKind::from_str_validated("gemini").is_err()); // reserved
        use crate::seam::Kind;
        assert!(ModelProviderKind::Gemini.is_reserved());
        assert!(!ModelProviderKind::Anthropic.is_reserved());

        assert_eq!(
            RoutingStrategy::from_str_validated("speculative"),
            Ok(RoutingStrategy::CostAware)
        );
        assert_eq!(RoutingStrategy::default(), RoutingStrategy::Sequential);
        assert_eq!(BudgetBreach::default(), BudgetBreach::Warn);
    }

    #[test]
    fn defaults_are_disabled_loopback() {
        let c = ModelProxyConfig::default();
        assert!(!c.enabled);
        assert_eq!(c.listen, "127.0.0.1:8383");
        assert!(c.listen_is_loopback());
        assert!(c.validate().is_empty());
        assert!(c.warnings().is_empty());
    }

    #[test]
    fn parses_full_section_from_toml() {
        let toml = r#"
            enabled = true
            listen = "127.0.0.1:9000"
            routing = "cost_aware"
            usage_aware = true

            [[providers]]
            name = "anthropic"
            kind = "anthropic"
            base_url = "https://api.anthropic.com"
            api_key = "env:ANTHROPIC_API_KEY"
            input_usd_per_mtok = 3.0
            output_usd_per_mtok = 15.0

            [[providers]]
            name = "ollama"
            kind = "openai"
            base_url = "http://127.0.0.1:11434/v1"
            subscription = true

            [[routes]]
            name = "standard"
            auto_max_tokens = 64000
            backends = [
              { provider = "anthropic", model = "claude-sonnet-4-5" },
              { provider = "ollama", model = "qwen3:32b" },
            ]

            [aliases]
            "gpt-5" = "standard"
            "auto" = "@auto"

            [budget]
            enabled = true
            on_breach = "refuse"
            window_secs = 86400
            [budget.scopes."agent:reviewer"]
            cost_usd = 5.0
        "#;
        let c: ModelProxyConfig = toml::from_str(toml).unwrap();
        assert!(c.enabled);
        assert_eq!(c.routing, RoutingStrategy::CostAware);
        assert!(c.usage_aware);
        assert_eq!(c.providers.len(), 2);
        assert_eq!(c.providers[0].kind, ModelProviderKind::Anthropic);
        assert!(c.providers[0].is_cost_bearing());
        assert!(!c.providers[1].is_cost_bearing()); // subscription
        assert_eq!(c.routes[0].backends.len(), 2);
        assert_eq!(c.routes[0].auto_max_tokens, Some(64000));
        assert_eq!(c.aliases.get("gpt-5").map(String::as_str), Some("standard"));
        assert_eq!(c.aliases.get("auto").map(String::as_str), Some(AUTO_ALIAS));
        assert!(c.budget.enabled);
        assert_eq!(c.budget.on_breach, BudgetBreach::Refuse);
        assert_eq!(c.budget.window_len_ms(), 86_400_000);
        assert_eq!(c.validate(), Vec::<String>::new());
    }

    #[test]
    fn raw_literal_key_is_rejected() {
        let mut c = ModelProxyConfig::default();
        c.enabled = true;
        c.providers.push(provider("anthropic", "sk-live-abc123"));
        let errs = c.validate();
        assert!(
            errs.iter()
                .any(|e| e.contains("anthropic") && e.contains("secret reference")),
            "{errs:?}"
        );
    }

    #[test]
    fn secret_ref_forms_pass() {
        let mut c = ModelProxyConfig::default();
        c.enabled = true;
        c.providers.push(provider("a", "env:A_KEY"));
        c.providers.push(provider("b", "file:/etc/keys/b"));
        let mut multi = provider("c", "env:C1");
        multi.api_keys = vec!["env:C2".into(), "file:/k/c3".into()];
        c.providers.push(multi);
        assert!(c.validate().is_empty(), "{:?}", c.validate());
        assert_eq!(c.providers[2].secret_refs().len(), 3);
    }

    #[test]
    fn unknown_provider_and_alias_are_errors() {
        let mut c = ModelProxyConfig::default();
        c.enabled = true;
        c.providers.push(provider("anthropic", "env:K"));
        c.routes.push(RouteEntry {
            name: "standard".into(),
            backends: vec![RouteBackend {
                provider: "nope".into(),
                model: "m".into(),
            }],
            ..Default::default()
        });
        c.aliases.insert("x".into(), "missing".into());
        let errs = c.validate();
        assert!(
            errs.iter().any(|e| e.contains("unknown provider")),
            "{errs:?}"
        );
        assert!(
            errs.iter().any(|e| e.contains("names no route")),
            "{errs:?}"
        );
    }

    #[test]
    fn non_loopback_listen_warns_not_errors() {
        let mut c = ModelProxyConfig::default();
        c.enabled = true;
        c.listen = "0.0.0.0:8383".into();
        assert!(c.validate().is_empty()); // valid host:port, no hard error
        assert_eq!(c.warnings().len(), 1);
        assert!(c.warnings()[0].contains("not loopback"));
    }

    #[test]
    fn bad_listen_is_an_error() {
        let mut c = ModelProxyConfig::default();
        c.enabled = true;
        c.listen = "not-an-addr".into();
        assert!(c.validate().iter().any(|e| e.contains("valid host:port")));
    }

    #[test]
    fn disabled_section_skips_validation() {
        let mut c = ModelProxyConfig::default();
        c.listen = "garbage".into();
        c.providers.push(provider("p", "raw-literal"));
        // Disabled ⇒ no errors regardless of content.
        assert!(c.validate().is_empty());
    }
}
