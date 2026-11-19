//! The daemon's PR queue producer and autopilot pickup (THE-2208).
//!
//! Stub: filled by b161 lane 8. Until then the PR queue tick does nothing and
//! `daemon_pickup` picks nothing up.
#![allow(dead_code, reason = "filled by b161 lane 8")]

use std::path::Path;

use thegn_core::config::Config;

use super::{TickCtx, TickOutcome};

/// One PR queue tick. Stub: does nothing.
pub(crate) fn tick_pr_queue(_ctx: &TickCtx) -> TickOutcome {
    TickOutcome::Skipped("not implemented")
}

/// One autopilot pickup the tracker tick hands over.
pub(crate) struct PickupRequest<'a> {
    pub repo_root: &'a Path,
    pub cwd: &'a Path,
    pub account: &'a str,
    pub provider: &'a str,
    pub issues: &'a [thegn_core::issue::Issue],
    pub from_assignee_me: bool,
}

/// Autopilot pickup inside the daemon. Stub: picks nothing up.
pub(crate) fn daemon_pickup(_cfg: &Config, _req: &PickupRequest<'_>) {}
