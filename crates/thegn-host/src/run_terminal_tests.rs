use super::{terminal_launch_is_current, terminal_worktree_path};
use crate::session::{Session, WorktreeGroup};
use std::path::Path;

#[test]
fn terminal_launch_requires_matching_generation_and_worktree() {
    let path = Path::new("/worktrees/feature");
    assert!(!terminal_launch_is_current(4, path, 5, path,));
    assert!(!terminal_launch_is_current(
        4,
        path,
        4,
        Path::new("/worktrees/other"),
    ));
    assert!(terminal_launch_is_current(4, path, 4, path));
}

#[test]
fn terminal_worktree_capture_keeps_nonexistent_selected_path() {
    let path = std::env::temp_dir().join(format!(
        "thegn-terminal-remote-placeholder-{}",
        std::process::id()
    ));
    assert!(!path.is_dir());
    let mut session = Session::default();
    session.worktrees.push(WorktreeGroup::new(
        "remote",
        crate::session::GroupKind::Branch,
        path.to_string_lossy().into_owned(),
    ));

    assert_eq!(terminal_worktree_path(&session).unwrap(), path);
}

#[test]
fn terminal_worktree_capture_refuses_missing_or_empty_selection() {
    assert!(terminal_worktree_path(&Session::default()).is_err());

    let mut session = Session::default();
    session.worktrees.push(WorktreeGroup::new(
        "terminal",
        crate::session::GroupKind::Terminal,
        "",
    ));
    assert!(terminal_worktree_path(&session).is_err());
}
