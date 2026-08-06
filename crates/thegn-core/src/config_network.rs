//! `[network]` — offline-detection policy. Kept out of the ratcheted `config.rs`
//! (like `config_remote`); the resolved mode + thresholds are installed into the
//! process-global [`crate::connectivity`] holder on load (`install`), which the
//! many `connectivity::current()` readers consult without carrying a `Config`.

use crate::config::{config_enum, config_warn};
use crate::connectivity::{self, Connectivity};
use serde::{Deserialize, Serialize};

config_enum! {
    /// `[network] mode` — how offline is decided.
    ///
    /// - `Auto` (default): passive detection. Consecutive network failures flip
    ///   the app to offline (pausing remote refreshes + remote MCP acquisition);
    ///   any success restores it.
    /// - `Online`: force-treat the network as available — never auto-disable
    ///   (opt out of detection).
    /// - `Offline`: force offline (airplane mode) — skip all remote refreshes and
    ///   network-acquired MCPs regardless of reachability.
    pub enum NetworkMode: "network mode" {
        Auto = "auto",
        Online = "online" | "on",
        Offline = "offline" | "off",
    } default = Auto;
}

impl NetworkMode {
    /// The forced-state this mode installs into the connectivity holder
    /// (`None` = auto / machine-driven).
    pub fn forced(self) -> Option<Connectivity> {
        match self {
            NetworkMode::Auto => None,
            NetworkMode::Online => Some(Connectivity::Online),
            NetworkMode::Offline => Some(Connectivity::Offline),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(default)]
pub struct NetworkConfig {
    /// Offline-detection mode.
    pub mode: NetworkMode,
    /// Consecutive transient network failures before the app concludes it's
    /// offline (`auto` mode only). Clamped to at least 1.
    pub offline_after_failures: u32,
    /// While offline, re-probe for connectivity at most this often (seconds).
    /// Clamped to at least 1.
    pub recovery_probe_secs: u64,
}

impl Default for NetworkConfig {
    fn default() -> Self {
        NetworkConfig {
            mode: NetworkMode::Auto,
            offline_after_failures: connectivity::OFFLINE_AFTER,
            recovery_probe_secs: connectivity::RECOVERY_PROBE_EVERY_MS / 1000,
        }
    }
}

impl NetworkConfig {
    /// Install the resolved policy into the process-global connectivity holder
    /// (called from `Config::post_process` on every load).
    pub fn install(&self) {
        connectivity::install_thresholds(
            self.offline_after_failures.max(1),
            self.recovery_probe_secs.max(1).saturating_mul(1000),
        );
        connectivity::install_forced(self.mode.forced());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mode_parse_default_alias() {
        assert_eq!(NetworkMode::default(), NetworkMode::Auto);
        assert_eq!(
            NetworkMode::from_str_validated("offline").unwrap(),
            NetworkMode::Offline
        );
        for s in ["online", "on"] {
            assert_eq!(
                NetworkMode::from_str_validated(s).unwrap(),
                NetworkMode::Online
            );
        }
        for s in ["offline", "off"] {
            assert_eq!(
                NetworkMode::from_str_validated(s).unwrap(),
                NetworkMode::Offline
            );
        }
        assert_eq!(NetworkMode::Auto.as_str(), "auto");
        assert!(NetworkMode::from_str_validated("bogus").is_err());
    }

    #[test]
    fn mode_maps_to_forced_state() {
        assert_eq!(NetworkMode::Auto.forced(), None);
        assert_eq!(NetworkMode::Online.forced(), Some(Connectivity::Online));
        assert_eq!(NetworkMode::Offline.forced(), Some(Connectivity::Offline));
    }

    #[test]
    fn defaults_mirror_holder_constants() {
        let nc = NetworkConfig::default();
        assert_eq!(nc.mode, NetworkMode::Auto);
        assert_eq!(nc.offline_after_failures, connectivity::OFFLINE_AFTER);
        assert_eq!(
            nc.recovery_probe_secs,
            connectivity::RECOVERY_PROBE_EVERY_MS / 1000
        );
    }

    #[test]
    fn toml_roundtrip() {
        let toml = r#"
            mode = "offline"
            offline_after_failures = 5
            recovery_probe_secs = 15
        "#;
        let nc: NetworkConfig = toml::from_str(toml).unwrap();
        assert_eq!(nc.mode, NetworkMode::Offline);
        assert_eq!(nc.offline_after_failures, 5);
        assert_eq!(nc.recovery_probe_secs, 15);
    }

    #[test]
    fn install_applies_without_panicking() {
        // Exercises the install glue (install_thresholds + install_forced) for
        // coverage. We deliberately do NOT assert on the process-global
        // `current()`: parallel config tests also call `install` (via
        // `post_process`), so the shared atomics would race. The pure mapping is
        // asserted in `mode_maps_to_forced_state`.
        NetworkConfig {
            mode: NetworkMode::Offline,
            offline_after_failures: 5,
            recovery_probe_secs: 15,
        }
        .install();
        NetworkConfig::default().install(); // restore auto (best-effort cleanup)
    }
}
