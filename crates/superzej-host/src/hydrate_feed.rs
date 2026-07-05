//! Notification-feed population for model hydration (extracted from the
//! size-capped `hydrate.rs`). Runs on the hydration worker, never the loop.

use superzej_core::db::Db;
use superzej_core::store::NotificationStore;

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
