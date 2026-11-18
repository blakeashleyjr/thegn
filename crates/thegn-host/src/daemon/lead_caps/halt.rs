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
    /// The dispatch roster could not be read, so live automation lanes with
    /// no in-memory origin may have been missed. Omitted when false.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    lanes_unreadable: bool,
}

/// One live session as the kill switch sees it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct LiveSession {
    pub id: String,
    /// Opened by a Lead-confined `sessions.open` (`SessionEntry::lead_opener`).
    pub lead_opened: bool,
}

/// Which live sessions halt kills: those with an automation origin, those a
/// Lead opened, and those running an active automation lane on the dispatch
/// roster (covers lanes the in-memory origin map lost: a transport-retry
/// relaunch or a daemon restart). Pure; sorted and de-duplicated. An origin
/// or lane with no live session (it already exited) is not a target.
pub(crate) fn halt_targets(
    live: &[LiveSession],
    origins: &BTreeSet<String>,
    lane_sessions: &BTreeSet<String>,
) -> Vec<String> {
    live.iter()
        .filter(|s| s.lead_opened || origins.contains(&s.id) || lane_sessions.contains(&s.id))
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
    let mut lanes_unreadable = false;
    if !keep_sessions {
        // Automation lanes from the roster: the origin map is in-memory
        // only. No status filter: a Lead can `dispatches.set_status` a row
        // to done while its worker still runs, and `halt_targets` keeps only
        // sessions that are live. A read error never skips the kill.
        let lane_sessions = match svc
            .with_db(|db| {
                use thegn_core::store::NotificationStore;
                let mut out = BTreeSet::new();
                for row in db.list_dispatches()? {
                    if let Some(sid) = row.session_id.clone()
                        && db.dispatch_automation(row.id)?.1.is_some()
                    {
                        out.insert(sid);
                    }
                }
                Ok(out)
            })
            .await
        {
            Ok(set) => set,
            Err(error) => {
                tracing::warn!(
                    target: "thegn::automation",
                    %error,
                    "automations.halt could not read the dispatch roster; killing by origin only"
                );
                lanes_unreadable = true;
                BTreeSet::new()
            }
        };
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
        for id in halt_targets(&live, &origins, &lane_sessions) {
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
        lanes_unreadable,
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
            live("relaunched-lane", false),
            live("lane-and-origin", false),
        ];
        let origins: BTreeSet<String> =
            ["auto-1", "auto-and-lead", "lane-and-origin", "gone-already"]
                .into_iter()
                .map(String::from)
                .collect();
        let lanes: BTreeSet<String> = ["relaunched-lane", "lane-and-origin", "lane-exited"]
            .into_iter()
            .map(String::from)
            .collect();
        assert_eq!(
            halt_targets(&sessions, &origins, &lanes),
            vec![
                "auto-1",
                "auto-and-lead",
                "lane-and-origin",
                "lead-worker",
                "relaunched-lane"
            ]
        );
    }

    #[test]
    fn halt_targets_empty_inputs_kill_nothing() {
        let none = BTreeSet::new();
        assert!(halt_targets(&[], &none, &none).is_empty());
        assert!(halt_targets(&[live("user", false)], &none, &none).is_empty());
        let origins: BTreeSet<String> = ["dead".to_string()].into();
        assert!(halt_targets(&[], &origins, &none).is_empty());
        let lanes: BTreeSet<String> = ["dead-lane".to_string()].into();
        assert!(halt_targets(&[], &none, &lanes).is_empty());
        assert!(halt_targets(&[live("user", false)], &none, &lanes).is_empty());
        // Only in the lane set: still a target.
        assert_eq!(
            halt_targets(&[live("dead-lane", false)], &none, &lanes),
            vec!["dead-lane"]
        );
    }

    #[test]
    fn halt_reply_shape() {
        let reply = encode(&HaltReply {
            paused: true,
            killed: vec!["a".into()],
            validations_cancelled: 2,
            kept_sessions: false,
            headless_lead_turn: HEADLESS_NOTE,
            lanes_unreadable: false,
        })
        .unwrap();
        assert!(reply.get("lanes_unreadable").is_none());
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
