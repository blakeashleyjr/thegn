//! Host-side release-channel holder.
//!
//! [`thegn_core::channel`] defines the pure registry (which features are
//! experimental, which channels allow them); this is the host's process-global
//! cell holding the *resolved* [`Channel`] the running binary operates in. It
//! follows the same sanctioned pattern as [`crate::caps`] — an atomic written
//! once at startup and read lock-free everywhere (masthead, palette, panel,
//! CLI guards).
//!
//! Resolution order (highest first):
//! 1. `THEGN_CHANNEL` env (`stable` / `dev` / `experimental`) — lets a stable
//!    binary be opened in dev mode for testing and vice-versa;
//! 2. the compiled-in default: the `dev` Cargo feature ⇒ [`Channel::Dev`],
//!    otherwise [`Channel::Stable`].

use std::sync::atomic::{AtomicU8, Ordering};

use thegn_core::channel::{Channel, Feature};
use thegn_core::config::EnvSource;

// 0 = Stable (also the safe pre-install default), 1 = Dev.
static CHANNEL: AtomicU8 = AtomicU8::new(0);
// One bit per feature in `Feature::ALL`. Clamp results are cumulative for the
// lifetime of this process because later config refreshes must not erase the
// explanation for the config generation already in use.
static CLAMPED: AtomicU8 = AtomicU8::new(0);

const fn feature_bit(feature: Feature) -> u8 {
    match feature {
        Feature::Remote => 1 << 0,
        Feature::Providers => 1 << 1,
        Feature::Observe => 1 << 2,
        Feature::Placement => 1 << 3,
        Feature::Trackers => 1 << 4,
        Feature::Voice => 1 << 5,
    }
}

fn record_into(cell: &AtomicU8, features: &[Feature]) {
    let bits = features
        .iter()
        .fold(0, |bits, feature| bits | feature_bit(*feature));
    cell.fetch_or(bits, Ordering::Relaxed);
}

/// Record the features neutralised by a config clamp. The process-wide set is
/// cumulative so a daemon refresh cannot hide a clamp observed at startup.
pub fn record_clamped(features: &[Feature]) {
    record_into(&CLAMPED, features);
}

/// Whether this process has clamped `feature` in any loaded config generation.
pub fn clamped(feature: Feature) -> bool {
    CLAMPED.load(Ordering::Relaxed) & feature_bit(feature) != 0
}

/// Clamp a config and retain the result for later refusal/config diagnostics.
pub fn clamp_and_record(cfg: &mut thegn_core::config::Config, channel: Channel) -> Vec<Feature> {
    let features = cfg.clamp_to_channel(channel);
    record_clamped(&features);
    features
}

/// Message used by tracker-dependent CLI doors when the config was neutralised
/// by the release-channel policy. Kept pure for focused command tests.
pub fn tracker_unconfigured_message(trackers_clamped: bool) -> &'static str {
    if trackers_clamped {
        "configured issue tracker is experimental and disabled on the stable channel; set THEGN_CHANNEL=dev to enable it"
    } else {
        "no issue tracker configured (set [issues] providers/accounts)"
    }
}

const fn to_u8(c: Channel) -> u8 {
    match c {
        Channel::Stable => 0,
        Channel::Dev => 1,
    }
}

const fn from_u8(v: u8) -> Channel {
    match v {
        1 => Channel::Dev,
        _ => Channel::Stable,
    }
}

/// The channel this build defaults to when nothing overrides it: `Dev` iff the
/// `dev` Cargo feature is compiled in, else `Stable`.
pub const fn default_channel() -> Channel {
    if cfg!(feature = "dev") {
        Channel::Dev
    } else {
        Channel::Stable
    }
}

/// Resolve the channel from an environment source, falling back to
/// [`default_channel`]. Pure over `env` so it is unit-testable.
pub fn resolve(env: &dyn EnvSource) -> Channel {
    env.get("THEGN_CHANNEL")
        .and_then(|v| Channel::parse(&v))
        .unwrap_or_else(default_channel)
}

/// Resolve from the real process environment and install the result. Returns
/// the resolved channel. Call once at startup, before config clamping.
pub fn resolve_and_install() -> Channel {
    let ch = resolve(&thegn_core::config::ProcessEnv);
    install(ch);
    ch
}

/// Install the resolved channel into the process-global holder.
pub fn install(channel: Channel) {
    CHANNEL.store(to_u8(channel), Ordering::Relaxed);
}

/// Startup helper: resolve + install the channel, clamp `cfg`, and return a
/// one-line note (for `model.status`) when the stable channel disabled any
/// experimental subsystem the config asked for. Also logs the clamp to the
/// startup waterfall. Keeps the compositor's startup path (run.rs) to one call.
pub fn apply_startup_channel(cfg: &mut thegn_core::config::Config) -> Option<String> {
    let channel = resolve_and_install();
    let clamped = clamp_and_record(cfg, channel);
    if clamped.is_empty() {
        return None;
    }
    let feats = clamped
        .iter()
        .map(|f| f.id())
        .collect::<Vec<_>>()
        .join(", ");
    tracing::warn!(
        target: "thegn::startup",
        channel = channel.as_str(),
        clamped = %feats,
        "release channel disabled configured experimental features ({feats}); set THEGN_CHANNEL=dev to enable"
    );
    Some(format!(
        "stable channel: disabled {feats} (set THEGN_CHANNEL=dev to enable)"
    ))
}

/// The channel currently in effect (lock-free read).
pub fn current() -> Channel {
    from_u8(CHANNEL.load(Ordering::Relaxed))
}

/// Whether `feature` is allowed in the current channel — the call UI/CLI sites
/// use to decide whether to surface an experimental affordance.
#[cfg_attr(not(test), allow(dead_code))]
pub fn allows(feature: Feature) -> bool {
    feature.allowed_in(current())
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Env(Option<&'static str>);
    impl EnvSource for Env {
        fn get(&self, key: &str) -> Option<String> {
            if key == "THEGN_CHANNEL" {
                self.0.map(str::to_string)
            } else {
                None
            }
        }
    }

    #[test]
    fn env_overrides_default() {
        assert_eq!(resolve(&Env(Some("dev"))), Channel::Dev);
        assert_eq!(resolve(&Env(Some("stable"))), Channel::Stable);
        assert_eq!(resolve(&Env(Some("experimental"))), Channel::Dev);
    }

    #[test]
    fn unset_or_bad_env_falls_back_to_default() {
        assert_eq!(resolve(&Env(None)), default_channel());
        assert_eq!(resolve(&Env(Some("garbage"))), default_channel());
    }

    #[test]
    fn install_roundtrips() {
        install(Channel::Dev);
        assert_eq!(current(), Channel::Dev);
        assert!(allows(Feature::Remote));
        install(Channel::Stable);
        assert_eq!(current(), Channel::Stable);
        assert!(!allows(Feature::Remote));
    }

    #[test]
    fn clamp_outcome_records_linear_but_not_github_trackers() {
        use thegn_core::config_issues::IssueProviderKind as K;
        let observed = AtomicU8::new(0);
        let mut linear = thegn_core::config::Config::default();
        linear.issues.provider = K::Linear;
        let removed = linear.clamp_to_channel(Channel::Stable);
        record_into(&observed, &removed);
        assert_ne!(
            observed.load(Ordering::Relaxed) & feature_bit(Feature::Trackers),
            0
        );

        let observed_github = AtomicU8::new(0);
        let mut github = thegn_core::config::Config::default();
        github.issues.provider = K::Github;
        let removed = github.clamp_to_channel(Channel::Stable);
        record_into(&observed_github, &removed);
        assert_eq!(
            observed_github.load(Ordering::Relaxed) & feature_bit(Feature::Trackers),
            0
        );
    }

    #[test]
    fn tracker_refusal_explains_a_channel_clamp_without_changing_empty_config_advice() {
        let clamped = tracker_unconfigured_message(true);
        assert!(clamped.contains("stable channel"));
        assert!(clamped.contains("THEGN_CHANNEL=dev"));
        assert!(!clamped.contains("set [issues] providers/accounts"));
        assert_eq!(
            tracker_unconfigured_message(false),
            "no issue tracker configured (set [issues] providers/accounts)"
        );
    }
}
