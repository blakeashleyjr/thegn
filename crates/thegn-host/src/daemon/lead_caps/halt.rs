//! `automations.halt` (b155 6f): the operator's kill switch. Sets the pause
//! flag, kills every live session carrying an automation origin or opened by
//! a Lead-confined caller (unless `keep_sessions`), cancels in-flight
//! executor validations, and reports what it stopped. Admin scope, HTTP + CLI
//! only; `thegn_core::lead_confine` refuses it for a Lead.
//!
//! Kills go through the daemon's own `kill` (a `SessionMsg::Kill` to the
//! session actor), so every child is reaped by its actor as on any other
//! kill. The Lead tab itself is the owner's session (no automation origin, no
//! Lead opener) and stays open.
//!
//! A headless Lead turn (`lead.wake`'s single-flight runner) has no external
//! cancel hook yet: it is bounded by its own timeout, and while paused every
//! spawn it asks for is refused. The reply says so.

use std::collections::BTreeSet;

use super::{JsonResult, decode, encode};
use crate::daemon::service::DaemonService;
use thegn_core::automation_control::HaltRequest;
use thegn_svc::control::{ControlApi, ControlError};

/// The note in every reply about a headless Lead turn (no cancel hook).
pub(crate) const HEADLESS_NOTE: &str = "a headless Lead turn, if one is running, may still finish (bounded by its timeout; its spawns are refused while paused)";

/// The reply of `automations.halt`.
#[derive(Debug, PartialEq, Eq, serde::Serialize)]
struct HaltReply {
    paused: bool,
    /// Session ids killed, sorted.
    killed: Vec<String>,
    /// In-flight executor validations cancelled.
    validations_cancelled: usize,
    /// Sessions skipped because of `keep_sessions`.
    kept_sessions: bool,
    headless_lead_turn: &'static str,
}

/// One live session as the kill switch sees it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct LiveSession {
    pub id: String,
    /// Opened by a Lead-confined `sessions.open` (`SessionEntry::lead_opener`).
    pub lead_opened: bool,
}

/// Which live sessions halt kills: those with an automation origin and those
/// a Lead opened. Pure; sorted and de-duplicated. An origin with no live
/// session (it already exited) is not a target.
pub(crate) fn halt_targets(live: &[LiveSession], origins: &BTreeSet<String>) -> Vec<String> {
    live.iter()
        .filter(|s| s.lead_opened || origins.contains(&s.id))
        .map(|s| s.id.clone())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect()
}

/// Answer `automations.halt` (`{keep_sessions}`).
pub(crate) async fn halt(svc: &DaemonService, req: serde_json::Value) -> JsonResult {
    let HaltRequest { keep_sessions } = decode(req)?;
    // The flag first: from here on the runtime, executor and approval
    // effects refuse, so nothing new starts while we kill. A failed write
    // still lets the kill run (the operator asked to stop things), then the
    // error is returned.
    let paused = svc
        .with_db(|db| thegn_core::automation_control::set_paused(db, true))
        .await;

    let mut killed = Vec::new();
    if !keep_sessions {
        let live: Vec<LiveSession> = svc
            .sessions
            .lock()
            .await
            .iter()
            .map(|(id, entry)| LiveSession {
                id: id.clone(),
                lead_opened: entry.lead_opener.is_some(),
            })
            .collect();
        let origins = crate::automation_runtime::origin_session_ids();
        for id in halt_targets(&live, &origins) {
            match ControlApi::kill(svc, &id).await {
                Ok(()) => killed.push(id),
                // Exited between the snapshot and the kill: nothing to stop.
                Err(ControlError::NotFound(_)) => {}
                Err(error) => tracing::warn!(
                    target: "thegn::automation",
                    session = %id,
                    %error,
                    "automations.halt could not kill a session"
                ),
            }
        }
    }
    let validations_cancelled = crate::daemon::stage_executor::cancel_all();

    tracing::warn!(
        target: "thegn::automation",
        killed = killed.len(),
        validations_cancelled,
        keep_sessions,
        "automations halted"
    );
    paused?;
    encode(&HaltReply {
        paused: true,
        killed,
        validations_cancelled,
        kept_sessions: keep_sessions,
        headless_lead_turn: HEADLESS_NOTE,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn live(id: &str, lead_opened: bool) -> LiveSession {
        LiveSession {
            id: id.into(),
            lead_opened,
        }
    }

    #[test]
    fn halt_targets_are_automation_origins_and_lead_opened_live_sessions() {
        let sessions = [
            live("user-shell", false),
            live("lead-tab", false),
            live("auto-1", false),
            live("lead-worker", true),
            live("auto-and-lead", true),
        ];
        let origins: BTreeSet<String> = ["auto-1", "auto-and-lead", "gone-already"]
            .into_iter()
            .map(String::from)
            .collect();
        assert_eq!(
            halt_targets(&sessions, &origins),
            vec!["auto-1", "auto-and-lead", "lead-worker"]
        );
    }

    #[test]
    fn halt_targets_empty_inputs_kill_nothing() {
        assert!(halt_targets(&[], &BTreeSet::new()).is_empty());
        assert!(halt_targets(&[live("user", false)], &BTreeSet::new()).is_empty());
        let origins: BTreeSet<String> = ["dead".to_string()].into();
        assert!(halt_targets(&[], &origins).is_empty());
    }

    #[test]
    fn halt_reply_shape() {
        let reply = encode(&HaltReply {
            paused: true,
            killed: vec!["a".into()],
            validations_cancelled: 2,
            kept_sessions: false,
            headless_lead_turn: HEADLESS_NOTE,
        })
        .unwrap();
        assert_eq!(reply["paused"], true);
        assert_eq!(reply["killed"], serde_json::json!(["a"]));
        assert_eq!(reply["validations_cancelled"], 2);
        assert!(
            reply["headless_lead_turn"]
                .as_str()
                .unwrap()
                .contains("may still finish")
        );
    }

    #[test]
    fn halt_rejects_unknown_fields() {
        assert!(decode::<HaltRequest>(serde_json::json!({"keep": true})).is_err());
        let req: HaltRequest = decode(serde_json::json!({"keep_sessions": true})).unwrap();
        assert!(req.keep_sessions);
    }
}
