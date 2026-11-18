//! Approvals in the panel (b155 6a): a pinned group inside the existing
//! Notifications section (not a new `Section`). `y` approves, `n` rejects,
//! each behind the panel confirm; the panel calls `approvals.resolve` over
//! the UI's local control connection, off-loop. Without a daemon it says
//! "approvals need the daemon".

use std::time::Instant;

use termwiz::terminal::TerminalWaker;
use thegn_core::approval_request::{Decision, parse_source_ref};
use thegn_core::notification::{Notification, Priority};
use tokio::sync::mpsc::UnboundedSender;

use crate::chrome::FrameModel;
use crate::hydrate::RefreshKind;
use crate::panel::confirm::TwoPress;

/// The approver label the panel records.
const APPROVER: &str = "panel";

/// The `approvals.resolve` body (pure).
pub(crate) fn resolve_body(
    issue_id: &str,
    stage: &str,
    commit_sha: &str,
    decision: Decision,
) -> serde_json::Value {
    serde_json::json!({
        "issue_id": issue_id,
        "stage": stage,
        "commit_sha": commit_sha,
        "decision": match decision {
            Decision::Approve => "approve",
            Decision::Reject => "reject",
        },
        "approver": APPROVER,
    })
}

/// The confirm prompt, built from the trusted description (pure).
pub(crate) fn confirm_prompt(description: &str, decision: Decision) -> String {
    let verb = match decision {
        Decision::Approve => "Approve",
        Decision::Reject => "Reject",
    };
    let key = match decision {
        Decision::Approve => 'y',
        Decision::Reject => 'n',
    };
    format!("{verb}: {description}? press {key} again within 3 s")
}

/// `y` / `n` on an approval row: the first press arms the confirm (showing the
/// trusted description), the second within the window resolves off-loop.
pub(crate) fn request_resolve(
    model: &mut FrameModel,
    guard: &mut TwoPress,
    notif: &Notification,
    decision: Decision,
    tx: &UnboundedSender<RefreshKind>,
    waker: &TerminalWaker,
) {
    let Some((issue, stage, sha)) = parse_source_ref(&notif.source_ref) else {
        model.status = "not an approval request".into();
        return;
    };
    let description = model
        .panel
        .approval_descriptions
        .get(&notif.source_ref)
        .cloned()
        .unwrap_or_else(|| "unverified request".into());
    let key = format!("approval:{decision:?}:{}", notif.source_ref);
    if !guard.confirm(&key, Instant::now()) {
        model.status = confirm_prompt(&description, decision);
        return;
    }
    let body = resolve_body(&issue, &stage, &sha, decision);
    let (tx, waker) = (tx.clone(), waker.clone());
    let handle = tokio::runtime::Handle::try_current().ok();
    let admission = crate::sched::spawn_bg(move || {
        let Some(handle) = handle else { return };
        let outcome = handle.block_on(async {
            let client = crate::automation_executor::client().await?;
            client
                .call_raw("POST", "/v1/approvals/resolve", Some(body))
                .await
        });
        let message = match outcome {
            Ok(_) => match decision {
                Decision::Approve => format!("approved: {description}"),
                Decision::Reject => format!("rejected: {description}"),
            },
            Err(error) => {
                tracing::debug!(%error, "approval resolve failed");
                if format!("{error:#}").contains("no live daemon") {
                    "approvals need the daemon".to_string()
                } else {
                    format!("approval not recorded: {error:#}")
                }
            }
        };
        // best-effort: consumer gone
        if let Err(error) = tx.send(RefreshKind::Toast {
            message,
            priority: Priority::Notice,
        }) {
            tracing::debug!(%error, "approval refresh send failed");
        }
        // best-effort: consumer gone
        if let Err(error) = tx.send(RefreshKind::Model) {
            tracing::debug!(%error, "approval refresh send failed");
        }
        if let Err(error) = waker.wake() {
            tracing::debug!(%error, "could not wake after approval resolve");
        }
    });
    if admission != crate::sched::Admission::Admitted {
        model.status = "approvals busy; try again".into();
    } else {
        model.status = "resolving approval…".into();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_body_names_the_triple_decision_and_panel_approver() {
        let b = resolve_body("linear:THE-1", "code", "abcdef1", Decision::Approve);
        assert_eq!(b["issue_id"], "linear:THE-1");
        assert_eq!(b["stage"], "code");
        assert_eq!(b["commit_sha"], "abcdef1");
        assert_eq!(b["decision"], "approve");
        assert_eq!(b["approver"], "panel");
        assert_eq!(
            resolve_body("a:b", "s", "abcdef1", Decision::Reject)["decision"],
            "reject"
        );
    }

    #[test]
    fn confirm_prompt_quotes_the_description() {
        let p = confirm_prompt("land THE-1 after stage code", Decision::Approve);
        assert!(
            p.starts_with("Approve: land THE-1 after stage code?"),
            "{p}"
        );
    }
}
