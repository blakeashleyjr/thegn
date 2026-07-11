//! Host-side `direnv` cache-warm glue for pane launch.
//!
//! A worktree pane runs as a login shell, so the user's `direnv hook` fires
//! *inside* the sandbox. With `nix-direnv`'s `use flake`, a cold cache makes the
//! in-sandbox direnv rebuild the devShell against the read-only `/nix/store` and
//! fail, falling back to the previous environment. The fix is to warm the cache
//! **on the host** (writable store + daemon) so the in-pane direnv replays it
//! read-only. See [`thegn_core::direnv`].
//!
//! Extracted from `agent.rs` (at its god-file ratchet ceiling): this is the
//! thin config→action mapping plus the off-loop synchronous variant used by the
//! pane-materialize path.

use std::path::Path;
use thegn_core::config::Config;
use thegn_core::direnv;

use crate::agent::{LaunchSpec, launch_spec_with_key};

/// Map `[sandbox] warm_direnv` to a host-side `direnv` cache warm for
/// `worktree`. Off-loop and self-gating (`direnv::warm` is a no-op without a
/// cold flake-backed `.envrc`); no-op when warming is disabled.
pub(crate) fn warm_direnv(cfg: &Config, worktree: &Path) {
    if let Some(allow) = direnv::warm_now_plan(cfg.sandbox.warm_direnv) {
        direnv::warm(worktree, allow);
    }
}

/// Bounded, SYNCHRONOUS variant of [`warm_direnv`] for the off-loop
/// pane-materialize path. Warms `worktree`'s `direnv` cache in-line (up to
/// [`direnv::WARM_NOW_TIMEOUT`]) so the first launch of a cold flake worktree
/// replays a warm cache instead of falling back. Returns whether the cache is
/// warm now. **Never call on the event loop.**
pub(crate) fn warm_direnv_now(cfg: &Config, worktree: &Path) -> bool {
    let Some(allow) = direnv::warm_now_plan(cfg.sandbox.warm_direnv) else {
        return false; // warming disabled — leave the pane on today's fallback
    };
    // Surface the wait: a cold build can take seconds, and this is off-loop.
    if direnv::needs_warm(worktree) {
        thegn_core::msg::info("warming devShell cache (direnv)…");
    }
    let warmed = direnv::warm_now(worktree, allow, direnv::WARM_NOW_TIMEOUT);
    if !warmed {
        tracing::debug!(
            target: "thegn::direnv",
            worktree = %worktree.display(),
            "direnv warm did not finish in time; pane may fall back this launch"
        );
    }
    warmed
}

/// Warm `worktree`'s `direnv` cache for a pane launch: bounded-synchronous when
/// `sync` (off-loop materialize path — the first launch of a cold worktree gets
/// a warm cache instead of falling back), else the async background kick.
pub(crate) fn warm_for_launch(cfg: &Config, worktree: &Path, sync: bool) {
    if sync {
        warm_direnv_now(cfg, worktree);
    } else {
        warm_direnv(cfg, worktree);
    }
}

/// Like [`crate::agent::launch_spec`] but performs a BOUNDED SYNCHRONOUS
/// `direnv` warm before composing the spec, so a cold flake worktree's
/// in-sandbox direnv replays a warm cache on the *first* launch instead of
/// failing on the read-only `/nix/store`. **ONLY for guaranteed-off-loop
/// callers** (the spawn_blocking pane-materialize path) — the warm blocks for
/// seconds and must never run on the event loop.
pub(crate) fn launch_spec_synced(
    cfg: &Config,
    worktree: &str,
    branch: Option<&str>,
    choice: &str,
) -> anyhow::Result<LaunchSpec> {
    launch_spec_with_key(cfg, worktree, branch, choice, None, true)
}
