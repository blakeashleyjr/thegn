//! The `[host_discovery]` config family — find remote-host candidates from a
//! mesh VPN the local machine already belongs to, and promote them to
//! `[host.<name>]` targets. Kept in a sibling module (not the god-file
//! `config.rs`); `config.rs` re-exports everything here.
//!
//! Discovery is on-demand only (a CLI verb / wizard / palette action) — there
//! is deliberately no polling knob, so the 0%-idle contract is structural. The
//! seam and its subprocess live in `thegn_svc::host_discovery`; the pure parser
//! is `thegn_core::tailnet`.

use serde::{Deserialize, Serialize};

use crate::config::{config_enum, config_warn};

config_enum! {
    /// `[host_discovery] kind` — the discovery backend. `tailnet` (the local
    /// tailscale client, control-plane-agnostic so it also serves headscale) is
    /// implemented. `mdns` (zeroconf/Bonjour) and `consul` are reserved: the
    /// config accepts the name and `config validate --strict` rejects it with
    /// the standard reserved message until a build implements the seam kind.
    pub enum HostDiscoveryKind: "host_discovery kind" {
        Tailnet = "tailnet" | "tailscale" | "headscale",
        Mdns = "mdns" | "zeroconf" | "bonjour" reserved,
        Consul = "consul" reserved,
    } default = Tailnet;
}

/// `[host_discovery]` — inbound host discovery. Only the selected `kind`'s
/// sub-table is consulted; discovery still runs only on explicit user action.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize, schemars::JsonSchema)]
#[serde(default)]
pub struct HostDiscoveryConfig {
    /// Which discovery backend to use. `tailnet` (default); `mdns`/`consul`
    /// reserved.
    pub kind: HostDiscoveryKind,
    /// `[host_discovery.tailnet]` — the local tailscale client backend.
    pub tailnet: TailnetDiscoveryConfig,
}

impl Default for HostDiscoveryConfig {
    fn default() -> Self {
        HostDiscoveryConfig {
            kind: HostDiscoveryKind::Tailnet,
            tailnet: TailnetDiscoveryConfig::default(),
        }
    }
}

/// `[host_discovery.tailnet]` — enumerate peer devices from the LOCAL tailscale
/// client (`tailscale status --json`) as remote-host candidates. Reads nothing
/// but the client's own view of the tailnet; stores no credential. Works
/// unchanged against a headscale `login_server` (the client is control-plane
/// agnostic).
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize, schemars::JsonSchema)]
#[serde(default)]
pub struct TailnetDiscoveryConfig {
    /// Offer tailnet discovery at all. On by default (it is read-only and runs
    /// only on explicit action); set `false` to hide the verb/wizard step.
    pub enabled: bool,
    /// List only online peers (default). `false` includes offline devices.
    pub online_only: bool,
    /// Keep only peers carrying a tag matching this glob (`*` wildcard), e.g.
    /// `"tag:server"` or `"tag:prod-*"`. Empty = no tag filter.
    pub tag_filter: String,
    /// The `tailscale` binary to invoke (name on PATH, or an absolute path).
    pub tailscale_bin: String,
}

impl Default for TailnetDiscoveryConfig {
    fn default() -> Self {
        TailnetDiscoveryConfig {
            enabled: true,
            online_only: true,
            tag_filter: String::new(),
            tailscale_bin: "tailscale".into(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::seam::Kind;

    #[test]
    fn defaults_are_zero_config_and_read_only() {
        let c = HostDiscoveryConfig::default();
        assert_eq!(c.kind, HostDiscoveryKind::Tailnet);
        assert!(c.tailnet.enabled);
        assert!(c.tailnet.online_only);
        assert!(c.tailnet.tag_filter.is_empty());
        assert_eq!(c.tailnet.tailscale_bin, "tailscale");
    }

    #[test]
    fn parses_a_full_tailnet_table() {
        let c: HostDiscoveryConfig = toml::from_str(
            r#"
            kind = "tailnet"
            [tailnet]
            enabled = true
            online_only = false
            tag_filter = "tag:server"
            tailscale_bin = "/usr/bin/tailscale"
            "#,
        )
        .unwrap();
        assert_eq!(c.kind, HostDiscoveryKind::Tailnet);
        assert!(!c.tailnet.online_only);
        assert_eq!(c.tailnet.tag_filter, "tag:server");
        assert_eq!(c.tailnet.tailscale_bin, "/usr/bin/tailscale");
    }

    #[test]
    fn headscale_is_the_same_kind() {
        // The control plane is not a separate kind: `headscale` is an alias for
        // `tailnet` (the local client is control-plane agnostic).
        assert_eq!(
            HostDiscoveryKind::from_str_validated("headscale").unwrap(),
            HostDiscoveryKind::Tailnet
        );
    }

    #[test]
    fn reserved_kinds_are_rejected_with_the_standard_message() {
        for name in ["mdns", "zeroconf", "consul"] {
            let err = HostDiscoveryKind::from_str_validated(name).unwrap_err();
            assert!(err.contains("reserved"), "{name}: {err}");
        }
        // The Kind trait agrees with the config macro's reserved markers.
        assert!(HostDiscoveryKind::Mdns.is_reserved());
        assert!(HostDiscoveryKind::Consul.is_reserved());
        assert!(!HostDiscoveryKind::Tailnet.is_reserved());
        let implemented: Vec<_> = HostDiscoveryKind::implemented().collect();
        assert_eq!(implemented, vec![HostDiscoveryKind::Tailnet]);
    }

    #[test]
    fn unknown_kind_reports_the_valid_set() {
        let err = HostDiscoveryKind::from_str_validated("wireguard").unwrap_err();
        assert!(err.contains("host_discovery kind"));
        assert!(err.contains("tailnet"));
    }
}
