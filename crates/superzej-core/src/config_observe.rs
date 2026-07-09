//! `[observe]` — the embedded observability app tab ("Observe").
//!
//! Kept in its own module (not `config.rs`) to hold that god-file at its ratchet
//! ceiling. `config.rs` only re-exports these types, adds one `Config` field, and
//! one `Default` line — the same pattern as [`crate::config_placement`].
//!
//! The app is a Grafana-style in-terminal dashboard: with no external config it
//! shows a built-in **host-metrics** dashboard (CPU/mem/load, sampled locally);
//! set `prometheus`/`loki` endpoints + a `dashboard_path` to point it at external
//! sources. The tab is **off by default** (AI-free shell stays lean; this is
//! additive) — set `enabled = true` to show it in the app-tab strip.

use serde::{Deserialize, Serialize};

/// `[observe]` — the embedded observability app tab.
#[derive(Debug, Clone, Deserialize, Serialize, schemars::JsonSchema)]
#[serde(default)]
pub struct ObserveConfig {
    /// Show the "Observe" tab in the top-level app-tab strip. Off by default.
    pub enabled: bool,
    /// Panel auto-refresh cadence, in seconds.
    pub refresh_interval_secs: u64,
    /// Dashboard source: empty ⇒ the built-in host-metrics dashboard; otherwise a
    /// path to a dashboard TOML (tilde-expanded), loaded via `gtui_core::dashboard`.
    /// A parse/read error falls back to the built-in dashboard (never fails the tab).
    pub dashboard_path: String,
    pub prometheus: PrometheusSourceConfig,
    pub loki: LokiSourceConfig,
}

impl Default for ObserveConfig {
    fn default() -> Self {
        ObserveConfig {
            enabled: false,
            refresh_interval_secs: 15,
            dashboard_path: String::new(),
            prometheus: PrometheusSourceConfig::default(),
            loki: LokiSourceConfig::default(),
        }
    }
}

/// `[observe.prometheus]` — a Prometheus HTTP endpoint for dashboard panels.
#[derive(Debug, Clone, Deserialize, Serialize, schemars::JsonSchema)]
#[serde(default)]
pub struct PrometheusSourceConfig {
    /// Base URL (e.g. `http://localhost:9090`). Empty ⇒ Prometheus panels report
    /// "not configured" rather than erroring.
    pub base_url: String,
    /// Bearer token; accepts the `env:VAR` / `file:PATH` indirection used elsewhere.
    pub token: String,
}

impl Default for PrometheusSourceConfig {
    fn default() -> Self {
        PrometheusSourceConfig {
            base_url: String::new(),
            token: "env:PROMETHEUS_TOKEN".into(),
        }
    }
}

/// `[observe.loki]` — a Loki HTTP endpoint for log panels.
#[derive(Debug, Clone, Deserialize, Serialize, schemars::JsonSchema)]
#[serde(default)]
pub struct LokiSourceConfig {
    /// Base URL (e.g. `http://localhost:3100`). Empty ⇒ Loki panels report
    /// "not configured".
    pub base_url: String,
    /// Bearer token; accepts the `env:VAR` / `file:PATH` indirection.
    pub token: String,
}

impl Default for LokiSourceConfig {
    fn default() -> Self {
        LokiSourceConfig {
            base_url: String::new(),
            token: "env:LOKI_TOKEN".into(),
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::config::Config;

    #[test]
    fn observe_defaults_are_disabled() {
        let o = super::ObserveConfig::default();
        assert!(!o.enabled);
        assert_eq!(o.refresh_interval_secs, 15);
        assert!(o.dashboard_path.is_empty());
        assert!(o.prometheus.base_url.is_empty());
        assert_eq!(o.prometheus.token, "env:PROMETHEUS_TOKEN");
        assert_eq!(o.loki.token, "env:LOKI_TOKEN");
    }

    #[test]
    fn observe_full_table_parses() {
        let cfg: Config = toml::from_str(
            r#"
[observe]
enabled = true
refresh_interval_secs = 30
dashboard_path = "~/dash.toml"

[observe.prometheus]
base_url = "http://localhost:9090"
token = "env:PROMETHEUS_TOKEN"

[observe.loki]
base_url = "http://localhost:3100"
"#,
        )
        .unwrap();
        assert!(cfg.observe.enabled);
        assert_eq!(cfg.observe.refresh_interval_secs, 30);
        assert_eq!(cfg.observe.dashboard_path, "~/dash.toml");
        assert_eq!(cfg.observe.prometheus.base_url, "http://localhost:9090");
        assert_eq!(cfg.observe.loki.base_url, "http://localhost:3100");
    }

    #[test]
    fn observe_absent_uses_defaults() {
        let cfg: Config = toml::from_str("").unwrap();
        assert!(!cfg.observe.enabled);
    }
}
