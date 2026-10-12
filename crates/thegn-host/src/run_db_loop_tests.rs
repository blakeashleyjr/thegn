use super::*;
use crate::session::{GroupKind, Session, WorktreeGroup};
use std::collections::HashSet;

#[test]
fn vanished_path_fence_keeps_groups_that_are_not_being_created() {
    let session = Session {
        id: "session".into(),
        worktrees: vec![
            WorktreeGroup::new("app/feat", GroupKind::Branch, "/repo/feat"),
            WorktreeGroup::new("app/other", GroupKind::Branch, "/repo/other"),
        ],
        active: 0,
    };
    let creating = HashSet::from([("app/feat".to_string(), 0)]);
    let paths = vec!["/repo/feat".to_string(), "/repo/other".to_string()];

    assert_eq!(
        fence_vanished_paths(&session, &creating, &paths),
        vec!["/repo/other".to_string()]
    );
}
