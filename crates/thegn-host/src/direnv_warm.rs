//! Compatibility wrappers for the removed host-side `direnv` warm path.
//!
//! The old public helper names remain so lifecycle/materialize callers do not
//! need a flag-day API change. They are deliberately inert: no host process,
//! thread, approval, cache write, or repository filesystem probe occurs.

use crate::agent::LaunchSpec;
use thegn_core::config::Config;

/// Resolve a launch spec through the normal policy path. The historical name
/// no longer implies that a repository environment is warmed or approved.
pub(crate) fn launch_spec_synced_with(
    cfg: &Config,
    worktree: &str,
    branch: Option<&str>,
    choice: &str,
    extras: crate::agent::LaunchExtras<'_>,
) -> anyhow::Result<LaunchSpec> {
    let daemon_persistent = crate::handlers::startup::daemon_active(cfg);
    crate::agent::launch_spec_full(cfg, worktree, branch, choice, daemon_persistent, extras)
}
