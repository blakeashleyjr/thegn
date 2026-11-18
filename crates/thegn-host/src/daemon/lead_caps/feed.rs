//! The opt-in Lead event feed (THE-2172): `EventFrame::Notice` frames for
//! notifications, dispatch status changes and automation runs, delivered only
//! to subscribers whose `FeedFilter.kinds` names them.
//!
//! Only daemon-written rows produce frames today; UI-process producers move
//! in a later phase.

use std::sync::{Arc, OnceLock};
use thegn_core::control_wire::{EventFrame, NoticeKind};

/// The daemon's event broadcast sender.
pub(crate) type EventsTx = tokio::sync::broadcast::Sender<Arc<EventFrame>>;

static GLOBAL: OnceLock<EventsTx> = OnceLock::new();

/// Install the process-global sender [`automation_run`] publishes on. Called
/// once by the daemon at startup; never in the UI process.
pub(crate) fn install_global(events: EventsTx) {
    if GLOBAL.set(events).is_err() {
        tracing::debug!(target: "thegn::lead", "feed sender already installed; keeping the first");
    }
}

fn send(events: &EventsTx, kind: NoticeKind, json: serde_json::Value) {
    if events
        .send(Arc::new(EventFrame::Notice {
            kind,
            json: json.to_string(),
        }))
        .is_err()
    {
        tracing::trace!(target: "thegn::lead", "notice dropped: no subscriber");
    }
}

/// Publish a `notification` Notice for row `id` (details via `notifications.list`).
pub(crate) fn notification(events: &EventsTx, id: i64) {
    send(
        events,
        NoticeKind::Notification,
        serde_json::json!({ "id": id }),
    );
}

/// Publish a `dispatch` Notice for dispatch row `id`.
pub(crate) fn dispatch_status(events: &EventsTx, id: i64) {
    send(
        events,
        NoticeKind::Dispatch,
        serde_json::json!({ "id": id }),
    );
}

/// Publish an `automation_run` Notice through the global sender; a no-op when
/// none is installed (the UI process).
#[allow(dead_code, reason = "called by the automation runtime (b152 lane 5)")]
pub(crate) fn automation_run(rule_id: &str, run_id: i64, outcome: &str) {
    if let Some(events) = GLOBAL.get() {
        send(
            events,
            NoticeKind::AutomationRun,
            serde_json::json!({ "rule": rule_id, "run_id": run_id, "outcome": outcome }),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn automation_run_without_global_is_a_noop() {
        // The global is never installed in unit tests of this module.
        automation_run("r", 1, "ok");
    }

    #[test]
    fn notification_and_dispatch_publish_id_only_notices() {
        let (tx, mut rx) = tokio::sync::broadcast::channel(4);
        notification(&tx, 7);
        dispatch_status(&tx, 9);
        let a = rx.try_recv().unwrap();
        let b = rx.try_recv().unwrap();
        assert_eq!(
            *a,
            EventFrame::Notice {
                kind: NoticeKind::Notification,
                json: r#"{"id":7}"#.into()
            }
        );
        assert_eq!(
            *b,
            EventFrame::Notice {
                kind: NoticeKind::Dispatch,
                json: r#"{"id":9}"#.into()
            }
        );
    }

    #[test]
    fn send_without_subscribers_does_not_panic() {
        let (tx, rx) = tokio::sync::broadcast::channel(1);
        drop(rx);
        notification(&tx, 1);
    }
}
