//! The daemon's headless producer owner (THE-2208).
//!
//! One tokio task per armed producer kind. Each tick it acquires or renews
//! that kind's lease (`producer_leases`, holder `daemon`, TTL three cadences,
//! at least 60 s), runs the kind's blocking `tick` in `spawn_blocking`, and
//! sleeps the cadence. A kind is armed by `thegn_core::producer_arm::armed`:
//! `[daemon] enabled` and `headless_producers` on AND something needs it.
//!
//! Inert by default: with `headless_producers = false`, or nothing that needs
//! a producer, `install` returns at once (no task, no timer, no lease write).
//!
//! Stub: the owner is filled by b161 lane 5; until then nothing arms.
#![allow(dead_code, reason = "filled by b161 lane 5")]

use std::sync::Arc;

use thegn_core::config::Config;
use thegn_core::producer_lease::ProducerKind;

use crate::daemon::service::DaemonService;

pub(crate) mod pr;
pub(crate) mod queues;
pub(crate) mod tracker;

/// What one producer tick sees.
pub(crate) struct TickCtx {
    pub cfg: Arc<Config>,
    pub now_ms: i64,
}

/// What one producer tick did.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum TickOutcome {
    /// The tick ran; `work` items were produced.
    Done { work: usize },
    /// The tick did nothing, for this reason.
    Skipped(&'static str),
}

/// Arm every producer kind that is needed. Stub: arms nothing.
pub(crate) fn install(_svc: Arc<DaemonService>, _cfg: Arc<Config>) {}

/// Re-evaluate arming after a rule edit. Stub: nothing is armed.
pub(crate) fn rearm() {}

/// True while any producer kind is armed (keeps the daemon from idle-exit).
pub(crate) fn armed_any() -> bool {
    false
}

/// Each producer kind and whether it is armed, for `thegn doctor`.
pub(crate) fn status() -> Vec<(ProducerKind, bool /*armed*/)> {
    Vec::new()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_stub_owner_arms_nothing() {
        rearm();
        assert!(!armed_any());
        assert!(status().is_empty());
    }
}
