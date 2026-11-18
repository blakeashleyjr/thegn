//! The daemon's inbox producer (b163, THE-2214): polls each enabled
//! `[[inbox.accounts]]` entry of an implemented kind, diffs against the
//! account's seen-set (`inbox_account_state`), writes `inbox_item`
//! notification rows and submits `inbox_item` facts for new ids after the
//! baseline. Blocking; the owner runs it in `spawn_blocking`.
//!
//! Stub (filled by b163 lane 5): fetches nothing and writes nothing.

use super::{TickCtx, TickOutcome};

/// One inbox tick. Stub: skips.
#[allow(dead_code, reason = "filled by b163 lane 5")]
pub(crate) fn tick(_ctx: &TickCtx) -> TickOutcome {
    TickOutcome::Skipped("not implemented")
}
