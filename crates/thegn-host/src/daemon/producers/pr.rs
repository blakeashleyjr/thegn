//! The daemon's PR poll and mentions producers (THE-2208). Blocking; the
//! owner runs them in `spawn_blocking`.
//!
//! Stub: filled by b161 lane 7.
#![allow(dead_code, reason = "filled by b161 lane 7")]

use super::{TickCtx, TickOutcome};

/// One PR poll tick (checks, review requests, landings). Stub: does nothing.
pub(crate) fn tick_pr(_ctx: &TickCtx) -> TickOutcome {
    TickOutcome::Skipped("not implemented")
}

/// One mentions tick. Stub: does nothing.
pub(crate) fn tick_mentions(_ctx: &TickCtx) -> TickOutcome {
    TickOutcome::Skipped("not implemented")
}
