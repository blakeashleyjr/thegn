//! Release-channel capability registry — the single source of truth for which
//! subsystems are shippable in the **stable** pre-alpha and which are gated to
//! the **dev** channel.
//!
//! thegn ships as one binary; the [`Channel`] it runs in is resolved once at
//! startup (host side: `THEGN_CHANNEL` env → the `dev` Cargo feature default)
//! and installed into a lock-free holder. This module stays substrate-free and
//! pure so it can be reused everywhere and exhaustively unit-tested (it feeds
//! the core's 95% coverage gate):
//!
//! - [`Feature`] enumerates the cross-cutting subsystems that are gated.
//! - [`Feature::stability`] classifies each as [`Stability::Stable`] or
//!   [`Stability::Experimental`] — the experimental set is the one thing that
//!   changes as a subsystem graduates.
//! - [`Feature::allowed_in`] answers "may this run in that channel?"
//!   (experimental ⇒ dev-only; stable ⇒ everywhere).
//!
//! Enforcement lives at the edges — [`crate::config::Config::clamp_to_channel`]
//! neutralises disallowed toggles at config load, and the host hides the
//! matching UI/CLI surfaces — never by compiling code out (that would fight the
//! "additive / always-fallback" architecture).

use std::fmt;

/// The release channel a running thegn is operating in.
///
/// `Stable` is the default for the regular pre-alpha build; `Dev` unlocks the
/// experimental subsystems. Resolution (env / Cargo feature) is a host concern;
/// this enum is just the resolved value.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum Channel {
    /// The regular pre-alpha shell: experimental features are inert + hidden.
    #[default]
    Stable,
    /// The dev build: every feature is available.
    Dev,
}

impl Channel {
    /// The channel's canonical lowercase name (`"stable"` / `"dev"`).
    pub const fn as_str(self) -> &'static str {
        match self {
            Channel::Stable => "stable",
            Channel::Dev => "dev",
        }
    }

    /// Parse a channel name case-insensitively. Accepts `stable`/`dev` (and the
    /// common alias `experimental` for `dev`); anything else is `None` so the
    /// caller can fall back to its default rather than silently guess.
    pub fn parse(s: &str) -> Option<Channel> {
        match s.trim().to_ascii_lowercase().as_str() {
            "stable" | "release" => Some(Channel::Stable),
            "dev" | "experimental" => Some(Channel::Dev),
            _ => None,
        }
    }
}

impl fmt::Display for Channel {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A feature's release maturity.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Stability {
    /// Shippable in the regular pre-alpha.
    Stable,
    /// Dev-channel only — WIP surfaces, incomplete backends, or strictly
    /// additive AI that the shell must never hard-depend on.
    Experimental,
}

/// The cross-cutting subsystems whose availability the release channel governs.
///
/// Only subsystems that need gating appear here — the AI-free workspace shell
/// (git, panes, sidebar, sessions, panel, CLI merge/land/integrate, built-in sandbox
/// backends, …) is unconditionally [`Stable`](Stability::Stable) and is not
/// enumerated.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Feature {
    /// Remote worktrees over SSH/mosh (`[sandbox.remote]`) and Kubernetes
    /// placements (`[env.*] placement = "k8s"`). The ssh-CLI path
    /// works but the surface is young.
    Remote,
    /// Cloud execution providers (Fly / DO / VPS / Machine0 / Daytona) and the
    /// managed-sprite pool (`[host.*]`).
    Providers,
    /// The Observe dashboards + fleet-view app tab (`[observe]`).
    Observe,
    /// The multi-host placement / host-as-resource engine (`[placement]`).
    Placement,
    /// Experimental command-backed voice mode (`[voice]`).
    Voice,
    /// Resident plugin app tabs (`AppTab` contributions, plugin ids in `[apps]`).
    PluginApps,
    /// The Lead meta agent (`[lead]`): a hosted agent CLI in a global tab.
    Lead,
    /// Config-defined, shipped and recipe sandbox backends; live fork.
    /// Added by WS00-P1-01; gating owned by WS02-P4-01 (and WS07-P5-03 for fork).
    SandboxBackends,
    /// Landlock + seccomp confinement helper on bwrap/systemd/landlock.
    /// Added by WS00-P1-01; gating owned by WS03-P2-03b.
    LinuxConfine,
    /// `network = "allowlist"`, presets, approvals, brokering, external proxies.
    /// Added by WS00-P1-01; gating owned by WS04-P1-09.
    EgressProxy,
    /// macOS Seatbelt backend. Added by WS00-P1-01; gating owned by WS05-P2-06.
    Seatbelt,
    /// Windows MXC isolation backend. Added by WS00-P1-01; gating owned by
    /// WS06-P4-05.
    WinIsolation,
    /// Sandbox-backend plugins. Added by WS00-P1-01; gating owned by WS10-P8-04.
    PluginBackends,
    /// Per-agent presets applied at launch. Added by WS00-P1-01; gating owned by
    /// WS11-P1-07.
    AgentPresets,
    /// Private-copy workspaces (`workspace.mode = "private-copy"`). Added by
    /// WS00-P1-01; gating owned by WS11 (THE-791 gate 2).
    PrivateCopy,
    /// In-pane escalation requests. Added by WS00-P1-01; gating owned by WS11.
    Escalation,
}

impl Feature {
    /// Every gated feature, for exhaustive iteration (e.g. the `doctor` table
    /// and the config clamp). Order is stable and display-friendly.
    pub const ALL: [Feature; 16] = [
        Feature::Remote,
        Feature::Providers,
        Feature::Observe,
        Feature::Placement,
        Feature::Voice,
        Feature::PluginApps,
        Feature::Lead,
        Feature::SandboxBackends,
        Feature::LinuxConfine,
        Feature::EgressProxy,
        Feature::Seatbelt,
        Feature::WinIsolation,
        Feature::PluginBackends,
        Feature::AgentPresets,
        Feature::PrivateCopy,
        Feature::Escalation,
    ];

    /// The sandbox + remote programme features added together by WS00-P1-01.
    /// Each stays [`Stability::Experimental`] until its owner item graduates it;
    /// until then the clamp neutralises nothing for them and doctor reports them
    /// as not yet implemented.
    pub const PROGRAMME: [Feature; 9] = [
        Feature::SandboxBackends,
        Feature::LinuxConfine,
        Feature::EgressProxy,
        Feature::Seatbelt,
        Feature::WinIsolation,
        Feature::PluginBackends,
        Feature::AgentPresets,
        Feature::PrivateCopy,
        Feature::Escalation,
    ];

    /// The feature's stable identifier (matches the docs / doctor output).
    pub const fn id(self) -> &'static str {
        match self {
            Feature::Remote => "remote",
            Feature::Providers => "providers",
            Feature::Observe => "observe",
            Feature::Placement => "placement",
            Feature::Voice => "voice",
            Feature::PluginApps => "plugin_apps",
            Feature::Lead => "lead",
            Feature::SandboxBackends => "sandbox_backends",
            Feature::LinuxConfine => "linux_confine",
            Feature::EgressProxy => "egress_proxy",
            Feature::Seatbelt => "seatbelt",
            Feature::WinIsolation => "win_isolation",
            Feature::PluginBackends => "plugin_backends",
            Feature::AgentPresets => "agent_presets",
            Feature::PrivateCopy => "private_copy",
            Feature::Escalation => "escalation",
        }
    }

    /// The feature's release maturity. Changing this line is how a subsystem
    /// graduates from dev-only to shippable.
    pub const fn stability(self) -> Stability {
        match self {
            // The confirmed experimental set for the pre-alpha.
            Feature::Remote
            | Feature::Providers
            | Feature::Placement
            | Feature::Voice
            | Feature::Lead => Stability::Experimental,
            // The sandbox + remote programme (WS00-P1-01): experimental until each
            // owner item graduates its feature.
            Feature::SandboxBackends
            | Feature::LinuxConfine
            | Feature::EgressProxy
            | Feature::Seatbelt
            | Feature::WinIsolation
            | Feature::PluginBackends
            | Feature::AgentPresets
            | Feature::PrivateCopy
            | Feature::Escalation => Stability::Experimental,
            Feature::Observe | Feature::PluginApps => Stability::Stable,
        }
    }

    /// Whether this feature may run in `channel`. Experimental features are
    /// dev-only; stable features run everywhere.
    pub const fn allowed_in(self, channel: Channel) -> bool {
        match self.stability() {
            Stability::Stable => true,
            Stability::Experimental => matches!(channel, Channel::Dev),
        }
    }
}

impl fmt::Display for Feature {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.id())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn channel_roundtrips_and_aliases() {
        assert_eq!(Channel::default(), Channel::Stable);
        assert_eq!(Channel::parse("stable"), Some(Channel::Stable));
        assert_eq!(Channel::parse("RELEASE"), Some(Channel::Stable));
        assert_eq!(Channel::parse(" Dev "), Some(Channel::Dev));
        assert_eq!(Channel::parse("experimental"), Some(Channel::Dev));
        assert_eq!(Channel::parse("nope"), None);
        assert_eq!(Channel::Stable.as_str(), "stable");
        assert_eq!(Channel::Dev.to_string(), "dev");
    }

    #[test]
    fn experimental_features_are_dev_only() {
        for f in Feature::ALL
            .into_iter()
            .filter(|f| f.stability() == Stability::Experimental)
        {
            assert!(
                !f.allowed_in(Channel::Stable),
                "{f} must be denied in stable"
            );
            assert!(f.allowed_in(Channel::Dev), "{f} must be allowed in dev");
        }
    }

    #[test]
    fn all_is_exhaustive_and_ids_unique() {
        // ALL must cover every variant so the clamp/doctor never miss one.
        // (Compile-time exhaustiveness is enforced by `stability`'s match; this
        // guards ALL against drift.)
        assert_eq!(Feature::ALL.len(), 16);
        assert_eq!(Feature::Lead.id(), "lead");
        assert_eq!(Feature::Lead.stability(), Stability::Experimental);
        let mut ids: Vec<&str> = Feature::ALL.iter().map(|f| f.id()).collect();
        ids.sort_unstable();
        ids.dedup();
        assert_eq!(ids.len(), Feature::ALL.len(), "feature ids must be unique");
    }

    #[test]
    fn channel_availability_follows_feature_stability() {
        let denied = Feature::ALL
            .iter()
            .filter(|f| !f.allowed_in(Channel::Stable))
            .count();
        let experimental = Feature::ALL
            .iter()
            .filter(|f| f.stability() == Stability::Experimental)
            .count();
        assert_eq!(denied, experimental);
        for f in Feature::ALL {
            assert_eq!(
                f.allowed_in(Channel::Stable),
                f.stability() == Stability::Stable,
                "stable availability should follow {f}'s stability"
            );
        }
        let allowed = Feature::ALL
            .iter()
            .filter(|f| f.allowed_in(Channel::Dev))
            .count();
        assert_eq!(allowed, Feature::ALL.len());
    }

    #[test]
    fn new_programme_features_are_experimental() {
        let ids: Vec<&str> = Feature::PROGRAMME.iter().map(|f| f.id()).collect();
        assert_eq!(
            ids,
            [
                "sandbox_backends",
                "linux_confine",
                "egress_proxy",
                "seatbelt",
                "win_isolation",
                "plugin_backends",
                "agent_presets",
                "private_copy",
                "escalation",
            ]
        );
        for f in Feature::PROGRAMME {
            assert_eq!(f.stability(), Stability::Experimental, "{f}");
            assert!(
                !f.allowed_in(Channel::Stable),
                "{f} must be denied in stable"
            );
            assert!(f.allowed_in(Channel::Dev), "{f} must be allowed in dev");
            assert!(Feature::ALL.contains(&f), "{f} must be in ALL");
        }
        // The seven pre-programme feature ids and their display order are part
        // of doctor JSON compatibility. The programme appends to that surface.
        let existing_ids: Vec<&str> = Feature::ALL[..7].iter().map(|f| f.id()).collect();
        assert_eq!(
            existing_ids,
            [
                "remote",
                "providers",
                "observe",
                "placement",
                "voice",
                "plugin_apps",
                "lead",
            ]
        );
        assert_eq!(&Feature::ALL[7..], &Feature::PROGRAMME[..]);
    }
}
