//! Sprites lifetime reaper (opt-in, `[env.<n>.provider] lifetime_reap = true`),
//! on the hydration cadence beside `vercel_reaper`: destroys sprites this host
//! can prove it owns once older than `max_lifetime_secs`, never a `running`
//! one. Inert unless an env opts in (an early-return config scan: no I/O and
//! no thread at idle).
//!
//! b162 spine stub: lane 3 fills in the pass.

use thegn_core::config::Config;

/// One throttled reaper pass. Returns at once when no env opts in.
pub fn tick(cfg: &Config) {
    if reaper_enabled(cfg) {
        tracing::debug!(target: "thegn::provider", "sprites lifetime reaper: no pass in this build");
    }
}

/// Whether any env opts into the background lifetime reaper (pure config scan).
pub(crate) fn reaper_enabled(_cfg: &Config) -> bool {
    false
}
