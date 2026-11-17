//! The opt-in Lead event feed (THE-2172): `EventFrame::Notice` frames for
//! notifications, dispatch status changes and automation runs, delivered only
//! to subscribers whose `FeedFilter.kinds` names them.
//!
//! Stub created by the b152 spine (lane 1); filled by b152 lane 4. Every body
//! is a no-op until then.

/// The daemon's event broadcast sender.
pub(crate) type EventsTx =
    tokio::sync::broadcast::Sender<std::sync::Arc<thegn_core::control_wire::EventFrame>>;

/// Install the process-global sender [`automation_run`] publishes on. Called
/// once by the daemon at startup; never in the UI process.
pub(crate) fn install_global(_events: EventsTx) {}

/// Publish a `notification` Notice for row `id`.
pub(crate) fn notification(_events: &EventsTx, _id: i64) {}

/// Publish a `dispatch` Notice for dispatch row `id`.
pub(crate) fn dispatch_status(_events: &EventsTx, _id: i64) {}

/// Publish an `automation_run` Notice through the global sender; a no-op when
/// none is installed (the UI process).
#[allow(dead_code, reason = "filled by b152 lane 4; called by lane 5")]
pub(crate) fn automation_run(_rule_id: &str, _run_id: i64, _outcome: &str) {}
