//! Pure targets and small decisions for panel Enter actions.

use thegn_core::notification::Notification;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum NotificationTarget {
    Jump(crate::sidebar::RowTarget),
    Browser(String),
    Nothing,
}

pub(crate) fn notification_target(
    row: &Notification,
    groups: &[(String, usize)],
    db: &[crate::sidebar::DbWorktree],
) -> NotificationTarget {
    if !row.worktree_path.is_empty()
        && let Some(target) = crate::nav::worktree_row_target(&row.worktree_path, groups, db)
    {
        return NotificationTarget::Jump(target);
    }
    if row.source_ref.starts_with("http://") || row.source_ref.starts_with("https://") {
        NotificationTarget::Browser(row.source_ref.clone())
    } else {
        NotificationTarget::Nothing
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use thegn_core::notification::NotificationKind;

    fn row(path: &str, source: &str) -> Notification {
        Notification {
            id: 1,
            kind: NotificationKind::Mentioned,
            source_ref: source.into(),
            message: "msg".into(),
            worktree_path: path.into(),
            created_at_ms: 0,
            read: false,
        }
    }

    #[test]
    fn notification_enter_jumps_to_its_worktree() {
        assert_eq!(
            notification_target(&row("/open", "issue:1"), &[("/open".into(), 3)], &[]),
            NotificationTarget::Jump(crate::sidebar::RowTarget::Tab(0, 3))
        );
    }

    #[test]
    fn notification_enter_falls_back_to_a_url_source() {
        assert_eq!(
            notification_target(&row("", "https://example.com/1"), &[], &[]),
            NotificationTarget::Browser("https://example.com/1".into())
        );
    }

    #[test]
    fn notification_enter_without_a_place_is_nothing() {
        assert_eq!(
            notification_target(&row("", "issue:1"), &[], &[]),
            NotificationTarget::Nothing
        );
    }
}
