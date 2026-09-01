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
/// (git, panes, sidebar, sessions, panel, CLI merge/land/integrate, sandbox
/// backends, …) is unconditionally [`Stable`](Stability::Stable) and is not
/// enumerated.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Feature {
    /// Remote worktrees over SSH/mosh (`[sandbox.remote]`). The ssh-CLI path
    /// works but the surface is young.
    Remote,
    /// Cloud execution providers (Fly / DO / VPS / Machine0 / Daytona) and the
    /// managed-sprite pool (`[host.*]`).
    Providers,
    /// The Observe dashboards + fleet-view app tab (`[observe]`).
    Observe,
    /// The multi-host placement / host-as-resource engine (`[placement]`).
    Placement,
    /// Multi-tier project-management trackers — Linear / Jira / Kaneo
    /// (`[issues]`). GitHub PR/issue *viewing* is stable and not gated here.
    Trackers,
    /// Experimental command-backed voice mode (`[voice]`).
    Voice,
}

impl Feature {
    /// Every gated feature, for exhaustive iteration (e.g. the `doctor` table
    /// and the config clamp). Order is stable and display-friendly.
    pub const ALL: [Feature; 6] = [
        Feature::Remote,
        Feature::Providers,
        Feature::Observe,
        Feature::Placement,
        Feature::Trackers,
        Feature::Voice,
    ];

    /// The feature's stable identifier (matches the docs / doctor output).
    pub const fn id(self) -> &'static str {
        match self {
            Feature::Remote => "remote",
            Feature::Providers => "providers",
            Feature::Observe => "observe",
            Feature::Placement => "placement",
            Feature::Trackers => "trackers",
            Feature::Voice => "voice",
        }
    }

    /// The feature's release maturity. Changing this line is how a subsystem
    /// graduates from dev-only to shippable.
    pub const fn stability(self) -> Stability {
        match self {
            // The confirmed experimental set for the pre-alpha.
            Feature::Remote
            | Feature::Providers
            | Feature::Observe
            | Feature::Placement
            | Feature::Trackers
            | Feature::Voice => Stability::Experimental,
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
        for f in Feature::ALL {
            assert_eq!(
                f.stability(),
                Stability::Experimental,
                "{f} should be experimental in the pre-alpha set"
            );
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
        assert_eq!(Feature::ALL.len(), 6);
        let mut ids: Vec<&str> = Feature::ALL.iter().map(|f| f.id()).collect();
        ids.sort_unstable();
        ids.dedup();
        assert_eq!(ids.len(), Feature::ALL.len(), "feature ids must be unique");
    }

    #[test]
    fn stable_channel_denies_all_gated_features() {
        // A stable build allows nothing in the gated set; a dev build allows all.
        let denied = Feature::ALL
            .iter()
            .filter(|f| !f.allowed_in(Channel::Stable))
            .count();
        assert_eq!(denied, Feature::ALL.len());
        let allowed = Feature::ALL
            .iter()
            .filter(|f| f.allowed_in(Channel::Dev))
            .count();
        assert_eq!(allowed, Feature::ALL.len());
    }
}
