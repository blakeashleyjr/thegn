//! The programme's one egress vocabulary (resolutions B11), stubbed by
//! WS00-P1-03 so every workstream compiles against the same names.
//!
//! Submodules and their owners:
//! - [`plan`]: the egress plan a sandbox launch carries (`SandboxSpec.egress`);
//!   owned by WS04-P2-08 after WS00.
//!
//! - [`pattern`]: the one host-pattern grammar (`x`, `*.x`, `=x`, `*`, each with
//!   an optional `:port` suffix), shared by `network_allow`, `network_block`, the
//!   zone/clamp, the proxy, providers and credential hosts; created by
//!   WS01-P2-01, extended to the full grammar by WS04-P1-01.
//!
//! Later items add their own submodules here: WS01-P2-01 added `pattern` (host
//! patterns, extended to the full grammar by WS04-P1-01); WS04-P1-07 added `policy` (the frozen policy
//! data shape, presets to follow) and `wire` (the control wire, the lease spec
//! and the audit record v1); WS04 adds `preset` and `resolve`. The reporting enum
//! [`EgressKind`](crate::capabilities::EgressKind) stays in `capabilities`; its
//! semantics are owned by WS01-P1-02.
//!
//! - [`preset_names`]: the THE-791 network preset names (alignment T6); added
//!   by WS00-P1-14, owned by WS04-P1-05.
//!
//! Apart from the pure `pattern` matcher, nothing here carries behaviour yet:
//! no I/O.

pub mod pattern;
pub mod plan;
pub mod policy;
pub mod wire;

/// The built-in network preset names of THE-791 (alignment T6). They replace
/// `minimal@1`, `trusted@1` and `full@1` everywhere. Stub constants added by
/// WS00-P1-14; owned by WS04-P1-05 (preset definitions), consumed by
/// WS11-P4-01 and WS11-P6-05.
///
/// The config keys stay WS04's: `offline` is `network = "none"`; the others are
/// `network = "allowlist"` with `preset = <name>`, satisfied only by a
/// packet-enforced datapath.
pub mod preset_names {
    /// No network at all (`network = "none"`).
    pub const OFFLINE: &str = "offline";
    /// Offline plus the launching agent's API and auth endpoints.
    pub const AGENT: &str = "agent@1";
    /// The development preset (registries, forges, toolchain mirrors).
    pub const DEVELOPMENT: &str = "development@1";
    /// Every host through the proxy; the address guard and audit still apply.
    pub const UNRESTRICTED: &str = "unrestricted@1";
    /// Prefix of a user preset: `custom:<name>` names `[egress_preset.<name>]`.
    pub const CUSTOM_PREFIX: &str = "custom:";

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn preset_names_are_versioned() {
            for name in [AGENT, DEVELOPMENT, UNRESTRICTED] {
                let (base, version) = name.split_once('@').expect("versioned preset");
                assert!(!base.is_empty());
                assert_eq!(version, "1", "{name}");
            }
            assert!(
                !OFFLINE.contains('@'),
                "offline is a mode, not a versioned preset"
            );
            assert_eq!(CUSTOM_PREFIX, "custom:");
            for retired in ["minimal@1", "trusted@1", "full@1"] {
                assert!(![OFFLINE, AGENT, DEVELOPMENT, UNRESTRICTED].contains(&retired));
            }
            // The T2 class defaults name these presets.
            for (kind, defaults) in crate::launch_class::CLASS_DEFAULTS {
                assert!(
                    [OFFLINE, AGENT].contains(&defaults.network),
                    "{kind:?} names an unknown preset {}",
                    defaults.network
                );
            }
        }
    }
}
