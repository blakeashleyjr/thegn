//! The daemon's tracker producer (THE-2208): polls the issue tracker for
//! `issue_matched` rules and runs autopilot pickup while the daemon holds the
//! tracker lease. Blocking; the owner runs it in `spawn_blocking`.
//!
//! Stub: filled by b161 lane 6.
#![allow(dead_code, reason = "filled by b161 lane 6")]

use super::{TickCtx, TickOutcome};

/// One tracker tick. Blocking. Stub: does nothing.
pub(crate) fn tick(_ctx: &TickCtx) -> TickOutcome {
    TickOutcome::Skipped("not implemented")
}
