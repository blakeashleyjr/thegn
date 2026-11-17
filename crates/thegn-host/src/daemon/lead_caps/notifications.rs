//! `notifications.list` / `notifications.ack` (THE-2172).

use super::{JsonResult, decode, encode, feed};
use crate::daemon::service::DaemonService;
use thegn_core::db::Db;
use thegn_core::notification_query::{
    AckTarget, NotificationsAckRequest, NotificationsListRequest, ack_target, filter,
};
use thegn_core::store::NotificationStore;
use thegn_svc::control::ControlError;

fn list_sync(db: &Db, req: &NotificationsListRequest) -> anyhow::Result<serde_json::Value> {
    let rows = db.get_all_notifications((req.effective_limit() * 4).max(500))?;
    let unread_total = rows.iter().filter(|n| !n.read).count();
    let notifications = filter(rows, req);
    Ok(serde_json::json!({ "notifications": notifications, "unread_total": unread_total }))
}

/// Apply an ack; returns the number of rows that were unread beforehand
/// (best effort).
fn ack_sync(db: &Db, target: &AckTarget) -> anyhow::Result<usize> {
    let unread = db.get_unread_notifications()?;
    let acked = match target {
        AckTarget::One(id) => {
            db.mark_notification_read(*id)?;
            unread.iter().filter(|n| n.id == *id).count()
        }
        AckTarget::All => {
            db.mark_all_notifications_read()?;
            unread.len()
        }
        AckTarget::Worktree(w) => {
            db.mark_notifications_read_for_worktree(w)?;
            unread.iter().filter(|n| n.worktree_path == *w).count()
        }
    };
    Ok(acked)
}

/// Answer `notifications.list`.
pub(crate) async fn list(svc: &DaemonService, req: serde_json::Value) -> JsonResult {
    let req: NotificationsListRequest = decode(req)?;
    req.validate().map_err(ControlError::InvalidArgument)?;
    let out = svc.with_db(move |db| list_sync(db, &req)).await?;
    Ok(out)
}

/// Answer `notifications.ack`; exactly one of `id`, `all`, `worktree`.
pub(crate) async fn ack(svc: &DaemonService, req: serde_json::Value) -> JsonResult {
    let req: NotificationsAckRequest = decode(req)?;
    let target = ack_target(&req).map_err(ControlError::InvalidArgument)?;
    let t = target.clone();
    let acked = svc.with_db(move |db| ack_sync(db, &t)).await?;
    if let AckTarget::One(id) = target {
        feed::notification(&svc.events, id);
    }
    encode(&serde_json::json!({ "acked": acked }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn list_and_ack_against_an_isolated_db() {
        let db = Db::open_memory().unwrap();
        let a = db.put_notification("agent_done", "i", "m1", "/a").unwrap();
        let _b = db.put_notification("test_failed", "i", "m2", "/b").unwrap();
        let req = NotificationsListRequest {
            unread: Some(true),
            ..Default::default()
        };
        let out = list_sync(&db, &req).unwrap();
        assert_eq!(out["unread_total"], 2);
        assert_eq!(out["notifications"].as_array().unwrap().len(), 2);

        assert_eq!(ack_sync(&db, &AckTarget::One(a)).unwrap(), 1);
        assert_eq!(ack_sync(&db, &AckTarget::Worktree("/b".into())).unwrap(), 1);
        let out = list_sync(&db, &req).unwrap();
        assert_eq!(out["unread_total"], 0);
        assert!(out["notifications"].as_array().unwrap().is_empty());
        assert_eq!(ack_sync(&db, &AckTarget::All).unwrap(), 0);
    }
}
