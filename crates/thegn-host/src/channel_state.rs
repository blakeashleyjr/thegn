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
    let clamped = cfg.clamp_to_channel(channel);
    if clamped.is_empty() {
        return None;
    }
    let feats = clamped
        .iter()
        .map(|f| f.id())
        .collect::<Vec<_>>()
        .join(", ");
    tracing::info!(
        target: "thegn::startup",
        channel = channel.as_str(),
        clamped = %feats,
        "stable channel: experimental features disabled ({feats}) — run the dev build to enable"
    );
    Some(format!(
        "stable channel: disabled {feats} (use the dev build to enable)"
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
}
