//! Pure targets and small decisions for panel Enter actions.

use thegn_core::notification::Notification;

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum PrRow<'a> {
    Thread(usize),
    OpenPr(&'a thegn_core::forge::model::PrHeader),
}

pub(crate) fn pr_row_at(
    data: &crate::panel::PanelData,
    deep: bool,
    cursor: usize,
) -> Option<PrRow<'_>> {
    let thread_count = if data.pr.is_some() {
        crate::panel::sections::visible_threads(data, deep).count()
    } else {
        0
    };
    if cursor < thread_count {
        return Some(PrRow::Thread(cursor));
    }
    let cap = if deep { 8 } else { 4 };
    data.open_prs
        .iter()
        .filter(|p| p.head_ref != data.branch)
        .take(cap)
        .nth(cursor - thread_count)
        .map(PrRow::OpenPr)
}

/// Resolve the repository containing an active worktree using hydrated data.
pub(crate) fn active_repo_path<'a>(
    active_path: &str,
    worktrees: &'a [crate::sidebar::DbWorktree],
) -> Option<&'a str> {
    worktrees
        .iter()
        .find(|worktree| worktree.path == active_path)
        .map(|worktree| worktree.repo_path.as_str())
}

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

    #[test]
    fn active_repo_path_uses_the_active_worktree_path() {
        let worktree = crate::sidebar::DbWorktree {
            slug: "repo".into(),
            branch: "feature".into(),
            repo_path: "/repo".into(),
            tab_name: "repo/feature".into(),
            path: "/repo/.worktrees/feature".into(),
            folder_id: None,
            sandbox_backend: None,
            env_name: None,
            env_degraded: false,
        };
        assert_eq!(
            active_repo_path("/repo/.worktrees/feature", std::slice::from_ref(&worktree),),
            Some("/repo")
        );
        assert_eq!(active_repo_path("/missing", &[worktree]), None);
    }
}
