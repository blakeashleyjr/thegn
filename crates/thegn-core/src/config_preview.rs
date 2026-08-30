//! `[preview]` — frontend preview discovery and bounded-fetch policy.
//!
//! This module owns the schema and normalization rules; `config.rs` contains
//! only the wiring needed to install it in the layered configuration.

use crate::config::config_warn;
use serde::{Deserialize, Serialize};

/// Smallest useful preview fetch timeout.
pub const MIN_FETCH_TIMEOUT_MS: u64 = 100;
/// Largest preview fetch timeout accepted from any configuration layer.
pub const MAX_FETCH_TIMEOUT_MS: u64 = 30_000;
/// Largest response body the preview fetcher may retain (16 MiB).
pub const MAX_PREVIEW_BODY_BYTES: usize = 16 * 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(default)]
pub struct PreviewConfig {
    /// Enable discovery and preview surfaces. This never launches a dev server.
    pub enabled: bool,
    /// Explicit candidate ports, ahead of pane/package-script hints.
    pub ports: Vec<u16>,
    /// Wall-clock limit for one bounded HTTP fetch.
    pub fetch_timeout_ms: u64,
    /// Maximum response bytes retained by one fetch.
    pub max_body_bytes: usize,
    /// Permit non-loopback fetch targets. Off by default (the SSRF boundary).
    pub allow_external_urls: bool,
}

impl Default for PreviewConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            ports: Vec::new(),
            fetch_timeout_ms: 3_000,
            max_body_bytes: 1_048_576,
            allow_external_urls: false,
        }
    }
}

impl PreviewConfig {
    /// Clamp resource limits and canonicalize explicit port candidates.
    pub(crate) fn normalize(&mut self) {
        self.fetch_timeout_ms = self
            .fetch_timeout_ms
            .clamp(MIN_FETCH_TIMEOUT_MS, MAX_FETCH_TIMEOUT_MS);
        self.max_body_bytes = self.max_body_bytes.clamp(1, MAX_PREVIEW_BODY_BYTES);
        if self.ports.contains(&0) {
            config_warn("preview.ports: port 0 is not a usable target; ignoring");
        }
        self.ports.retain(|port| *port != 0);
        self.ports.sort_unstable();
        self.ports.dedup();
    }
}

/// All-optional `[preview]` layer used by environment and CLI overlays.
#[derive(Debug, Default, Clone, schemars::JsonSchema)]
pub struct PreviewOverlay {
    pub enabled: Option<bool>,
    pub ports: Option<Vec<u16>>,
    pub fetch_timeout_ms: Option<u64>,
    pub max_body_bytes: Option<usize>,
    pub allow_external_urls: Option<bool>,
}

impl PreviewOverlay {
    pub(crate) fn apply(self, base: &mut PreviewConfig) {
        if let Some(value) = self.enabled {
            base.enabled = value;
        }
        if let Some(value) = self.ports {
            base.ports = value;
        }
        if let Some(value) = self.fetch_timeout_ms {
            base.fetch_timeout_ms = value;
        }
        if let Some(value) = self.max_body_bytes {
            base.max_body_bytes = value;
        }
        if let Some(value) = self.allow_external_urls {
            base.allow_external_urls = value;
        }
    }
}

/// Parse the comma-separated `THEGN_PREVIEW_PORTS` form atomically.
///
/// One malformed value rejects the whole environment layer so a file-provided
/// allowlist is not silently replaced by a partial list.
pub(crate) fn parse_ports_env(raw: &str) -> Result<Vec<u16>, String> {
    let mut ports = Vec::new();
    for value in raw
        .split(',')
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        let port = value
            .parse::<u16>()
            .map_err(|_| format!("invalid port {value:?}; expected 1..=65535"))?;
        if port == 0 {
            return Err("invalid port 0; expected 1..=65535".into());
        }
        ports.push(port);
    }
    ports.sort_unstable();
    ports.dedup();
    Ok(ports)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preview_defaults_are_bounded_and_local_only() {
        let config = PreviewConfig::default();
        assert!(config.enabled);
        assert!(config.ports.is_empty());
        assert_eq!(config.fetch_timeout_ms, 3_000);
        assert_eq!(config.max_body_bytes, 1_048_576);
        assert!(!config.allow_external_urls);
    }

    #[test]
    fn preview_toml_schema_round_trips() {
        let config: PreviewConfig = toml::from_str(
            r#"
                enabled = false
                ports = [5173, 3000]
                fetch_timeout_ms = 900
                max_body_bytes = 4096
                allow_external_urls = true
            "#,
        )
        .unwrap();
        assert!(!config.enabled);
        assert_eq!(config.ports, vec![5173, 3000]);
        assert_eq!(config.fetch_timeout_ms, 900);
        assert_eq!(config.max_body_bytes, 4096);
        assert!(config.allow_external_urls);
        assert!(
            toml::to_string(&config)
                .unwrap()
                .contains("fetch_timeout_ms = 900")
        );
    }

    #[test]
    fn preview_policy_clamps_and_canonicalizes() {
        let mut low = PreviewConfig {
            ports: vec![5173, 0, 3000, 5173],
            fetch_timeout_ms: 1,
            max_body_bytes: 0,
            ..PreviewConfig::default()
        };
        low.normalize();
        assert_eq!(low.ports, vec![3000, 5173]);
        assert_eq!(low.fetch_timeout_ms, MIN_FETCH_TIMEOUT_MS);
        assert_eq!(low.max_body_bytes, 1);

        let mut high = PreviewConfig {
            fetch_timeout_ms: u64::MAX,
            max_body_bytes: usize::MAX,
            ..PreviewConfig::default()
        };
        high.normalize();
        assert_eq!(high.fetch_timeout_ms, MAX_FETCH_TIMEOUT_MS);
        assert_eq!(high.max_body_bytes, MAX_PREVIEW_BODY_BYTES);
    }

    #[test]
    fn preview_port_env_is_atomic_and_deduplicated() {
        assert_eq!(
            parse_ports_env("5173, 3000,5173").unwrap(),
            vec![3000, 5173]
        );
        assert_eq!(parse_ports_env("  ").unwrap(), Vec::<u16>::new());
        for malformed in ["0", "65536", "3000,nope"] {
            assert!(parse_ports_env(malformed).is_err(), "{malformed}");
        }
    }

    #[test]
    fn preview_overlay_changes_only_present_values() {
        let mut config = PreviewConfig::default();
        PreviewOverlay {
            enabled: Some(false),
            ports: Some(vec![8080]),
            fetch_timeout_ms: Some(750),
            max_body_bytes: Some(2048),
            allow_external_urls: Some(true),
        }
        .apply(&mut config);
        assert_eq!(
            config,
            PreviewConfig {
                enabled: false,
                ports: vec![8080],
                fetch_timeout_ms: 750,
                max_body_bytes: 2048,
                allow_external_urls: true,
            }
        );
    }
}
