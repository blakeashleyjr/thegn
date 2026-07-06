//! Notification-feed population for model hydration (extracted from the
//! size-capped `hydrate.rs`). Runs on the hydration worker, never the loop.

use superzej_core::db::Db;
use superzej_core::notification::Notification;
use superzej_core::store::NotificationStore;

/// The ERROR count carried by the most recent existing `log:szhost`
/// notification, parsed from its `"{n} error(s) in szhost.log"` message.
/// `None` when there is no such notification (or its message doesn't lead with
/// an integer). `notifs` is newest-first (see [`get_all_notifications`]), so the
/// first match is the newest.
///
/// [`get_all_notifications`]: superzej_core::store::NotificationStore::get_all_notifications
pub(crate) fn last_logged_error_count(notifs: &[Notification]) -> Option<usize> {
    notifs
        .iter()
        .find(|n| n.source_ref == "log:szhost")
        .and_then(|n| n.message.split_whitespace().next())
        .and_then(|tok| tok.parse::<usize>().ok())
}

/// Emit a `log_error` notification only when the ERROR count has *grown* beyond
/// what was last notified — i.e. genuinely new errors appeared. A persistent,
/// unchanged set of errors emits nothing, so a previously-read notification
/// stays read (the append-only log otherwise re-lit the badge forever).
///
/// Option ordering makes `None < Some(n)`, so the first-ever error still fires.
///
/// When the live log has *no* errors (rotated/truncated away), mark any lingering
/// unread `log:szhost` row read so the badge clears — otherwise a stale row stays
/// clickable and drills into an error-free log ("no matching log lines").
pub(crate) fn maybe_emit_log_error(db: &Db, notifs: &[Notification], error_count: usize) {
    if error_count == 0 {
        for id in stale_log_notifs_to_clear(notifs, error_count) {
            // best-effort: DB is a cache; a failed mark just re-attempts next refresh.
            let _ = db.mark_notification_read(id);
        }
        return;
    }
    if Some(error_count) > last_logged_error_count(notifs) {
        let msg = format!(
            "{} error{} in szhost.log",
            error_count,
            if error_count == 1 { "" } else { "s" }
        );
        let _ = db.put_notification("log_error", "log:szhost", &msg, "");
    }
}

/// The `log:szhost` notification ids to mark read when the live log carries no
/// errors — a rotated/truncated log otherwise leaves a stale, unread badge that
/// drills into an error-free log. Returns nothing while errors remain.
fn stale_log_notifs_to_clear(notifs: &[Notification], error_count: usize) -> Vec<i64> {
    if error_count > 0 {
        return Vec::new();
    }
    notifs
        .iter()
        .filter(|n| n.source_ref == "log:szhost" && !n.read)
        .map(|n| n.id)
        .collect()
}

/// Full notification list for the inbox panel; badge counts are derived from
/// it by effective priority (Info kinds never count; the red flag is
/// Alert-only). Scoped to this repo's own worktrees by default (host-global
/// notifications, with an empty `worktree_path`, always show); the System-tab
/// "all" toggle reveals every worktree's — so a sibling repo's error doesn't
/// leak here or light the badge.
pub(crate) fn populate_notifications(
    db: &Db,
    repo_root: &std::path::Path,
    app_cfg: &superzej_core::config::Config,
    panel: &mut crate::panel::PanelData,
) {
    if let Ok(mut notifications) = db.get_all_notifications(50) {
        use superzej_core::notification::Priority;
        if !crate::panel::scope::system_all() {
            let repo_paths = crate::hydrate::repo_worktree_paths(db, repo_root);
            notifications
                .retain(|n| n.worktree_path.is_empty() || repo_paths.contains(&n.worktree_path));
        }
        let unread = notifications.iter().filter(|n| !n.read);
        let (mut alert, mut counted) = (0usize, 0usize);
        for n in unread {
            match app_cfg.notifications.priority_of(n.kind) {
                Priority::Alert => {
                    alert += 1;
                    counted += 1;
                }
                Priority::Notice => counted += 1,
                Priority::Info => {}
            }
        }
        panel.alert_notifications = alert;
        panel.unread_notifications = counted;
        panel.notifications = notifications;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use superzej_core::notification::NotificationKind;

    fn log_notif(created_at_ms: i64, message: &str) -> Notification {
        Notification {
            id: 0,
            kind: NotificationKind::LogError,
            source_ref: "log:szhost".into(),
            message: message.into(),
            created_at_ms,
            read: false,
            worktree_path: String::new(),
        }
    }

    #[test]
    fn last_logged_error_count_parses_newest_first() {
        // `get_all_notifications` returns newest-first; the first `log:szhost`
        // match is authoritative.
        let notifs = vec![
            log_notif(200, "5 errors in szhost.log"),
            log_notif(100, "2 errors in szhost.log"),
        ];
        assert_eq!(last_logged_error_count(&notifs), Some(5));
    }

    #[test]
    fn last_logged_error_count_handles_singular_and_mixed() {
        // Singular "1 error" parses, and unrelated kinds are skipped.
        let notifs = vec![
            Notification {
                kind: NotificationKind::Assigned,
                source_ref: "linear:ABC-1".into(),
                message: "42 whatever".into(),
                ..log_notif(300, "")
            },
            log_notif(200, "1 error in szhost.log"),
        ];
        assert_eq!(last_logged_error_count(&notifs), Some(1));
    }

    #[test]
    fn last_logged_error_count_none_without_log_notif() {
        let notifs = vec![Notification {
            kind: NotificationKind::Assigned,
            source_ref: "linear:ABC-1".into(),
            message: "3 blah".into(),
            ..log_notif(100, "")
        }];
        assert_eq!(last_logged_error_count(&notifs), None);
    }

    // The emit decision (the actual bug): mirrors `maybe_emit_log_error`'s gate
    // without a DB, so the reset regression is locked without a real Db.
    fn would_emit(notifs: &[Notification], error_count: usize) -> bool {
        error_count > 0 && Some(error_count) > last_logged_error_count(notifs)
    }

    #[test]
    fn emits_first_error_when_no_prior_notification() {
        assert!(would_emit(&[], 3));
    }

    #[test]
    fn does_not_re_emit_when_count_unchanged() {
        // The regression: same errors, no change → stay read, emit nothing.
        let notifs = vec![log_notif(100, "3 errors in szhost.log")];
        assert!(!would_emit(&notifs, 3));
    }

    #[test]
    fn emits_when_count_grows() {
        let notifs = vec![log_notif(100, "3 errors in szhost.log")];
        assert!(would_emit(&notifs, 4));
    }

    #[test]
    fn never_emits_with_zero_errors() {
        assert!(!would_emit(&[], 0));
        let notifs = vec![log_notif(100, "3 errors in szhost.log")];
        assert!(!would_emit(&notifs, 0));
    }

    #[test]
    fn clears_stale_unread_log_badge_when_log_has_no_errors() {
        // Rotated/truncated log → 0 errors: the lingering unread row is cleared.
        let notifs = vec![Notification {
            id: 7,
            ..log_notif(100, "3 errors in szhost.log")
        }];
        assert_eq!(stale_log_notifs_to_clear(&notifs, 0), vec![7]);
    }

    #[test]
    fn does_not_clear_while_errors_remain_or_already_read() {
        let unread = vec![Notification {
            id: 7,
            ..log_notif(100, "3 errors in szhost.log")
        }];
        // Errors still present → leave the badge alone.
        assert!(stale_log_notifs_to_clear(&unread, 3).is_empty());
        // Already-read rows aren't re-touched.
        let read = vec![Notification {
            id: 7,
            read: true,
            ..log_notif(100, "3 errors in szhost.log")
        }];
        assert!(stale_log_notifs_to_clear(&read, 0).is_empty());
    }
}
