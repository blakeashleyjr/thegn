//! Sidebar reorder: move worktrees/workspaces up or down one slot and persist
//! the new `position` order. Extracted from `run.rs` (pinned by the file-size
//! ratchet).
//!
//! Two entry points share the same primitives:
//! - **Ctrl+Alt+↑/↓** (`move_active_worktree` / `move_selected_workspace`) move
//!   a single item — the active worktree, or the workspace under the cursor.
//! - **Shift+↑/↓** (`reorder_selection`) move the whole multi-select: every
//!   marked row of the cursor row's kind (or the cursor row alone when nothing
//!   is marked), one slot, as a block.
//!
//! Motion always walks the *on-screen* order so it matches what the user sees;
//! same-workspace worktrees are contiguous there. A move under a computed sort
//! first flips the workspace back to Manual so the move is visible and sticks.

use std::collections::HashSet;

use thegn_core::store::WorkspaceStore;

use crate::chrome::FrameModel;
use crate::run::{
    SidebarState, sidebar_worktree_order, visible_index_of_active, visible_index_of_workspace,
};

/// The visible-row index of the row with this `pin_key`, if present. The cursor
/// travels with the item it moved by re-resolving this after the rebuild.
fn visible_index_of_pin_key(model: &FrameModel, key: &str) -> Option<usize> {
    model
        .sidebar_rows
        .iter()
        .filter(|r| r.visible)
        .position(|r| r.pin_key == key)
}

impl SidebarState {
    /// Move the active worktree one slot within its workspace (Ctrl+Alt+↑/↓),
    /// keeping the highlight on the moved (still active) group.
    pub(crate) fn move_active_worktree(
        &mut self,
        model: &mut FrameModel,
        session: &mut crate::session::Session,
        up: bool,
    ) -> bool {
        let gi = session.active;
        if self.move_worktree_group(model, session, gi, up) {
            // Keep the highlight on the worktree that just moved (now the active
            // group), the way workspace reorders already do.
            self.cursor = visible_index_of_active(model);
            self.sync(model);
            true
        } else {
            false
        }
    }

    /// Move worktree group `gi` one slot within its workspace, swapping it with
    /// the adjacent *same-workspace* branch sibling in both the live session
    /// order and the persisted registry `position`. `home` is a fixed top
    /// anchor: a worktree can't move above it, and home itself never moves.
    /// Rebuilds the tree; the caller places the cursor. Returns whether it moved.
    pub(crate) fn move_worktree_group(
        &mut self,
        model: &mut FrameModel,
        session: &mut crate::session::Session,
        gi: usize,
        up: bool,
    ) -> bool {
        use crate::session::GroupKind;
        let a = gi;
        // Home never moves.
        if session.worktrees.get(a).map(|g| g.kind) == Some(GroupKind::Home) {
            return false;
        }
        // Walk the on-screen order so the motion matches what the user sees.
        let order = sidebar_worktree_order(model);
        let Some(p) = order.iter().position(|&g| g == a) else {
            return false;
        };
        let neighbor = if up {
            p.checked_sub(1)
        } else {
            (p + 1 < order.len()).then_some(p + 1)
        };
        let Some(np) = neighbor else { return false };
        let b = order[np];
        // Stay within the same workspace, and never cross above home.
        let slug = |gi: usize| {
            session
                .worktrees
                .get(gi)
                .and_then(|g| crate::sidebar::split_tab(&g.name).map(|(s, _)| s))
        };
        if slug(a) != slug(b) {
            return false;
        }
        if session.worktrees.get(b).map(|g| g.kind) == Some(GroupKind::Home) {
            return false;
        }

        // Persist the new order: swap the durable `position` of the two paths…
        if let Ok(db) = thegn_core::db::Db::open() {
            let (pa, pb) = (
                session.worktrees[a].path.clone(),
                session.worktrees[b].path.clone(),
            );
            // best-effort: the DB is a cache — a failed persist only loses the
            // order across restart; the live session swap below still applies
            let _ = db.swap_worktree_positions(&pa, &pb);
        }
        // …and the live session order, preserving which group stays active
        // across the vec swap (its index moves with it).
        let was_active = session.active;
        session.worktrees.swap(a, b);
        if was_active == a {
            session.active = b;
        } else if was_active == b {
            session.active = a;
        }

        // A manual move only makes sense under Manual order; flip + persist if a
        // computed sort was active so the move is visible and survives restart.
        if self.view.sort != crate::sidebar::SortMode::Manual {
            self.view.sort = crate::sidebar::SortMode::Manual;
            self.persist("sort_mode", self.view.sort.as_str());
        }
        self.rebuild(model, session);
        true
    }

    /// Reorder the workspace under the sidebar cursor one slot (Ctrl+Alt+↑/↓),
    /// keeping the cursor on the moved workspace's header.
    pub(crate) fn move_selected_workspace(
        &mut self,
        model: &mut FrameModel,
        session: &crate::session::Session,
        up: bool,
    ) -> bool {
        let Some(slug) = self.selected_row(model).map(|r| r.workspace_slug.clone()) else {
            return false;
        };
        if self.move_workspace_by_slug(model, session, &slug, up) {
            if let Some(idx) = visible_index_of_workspace(model, &slug) {
                self.cursor = idx;
                self.sync(model);
            }
            true
        } else {
            false
        }
    }

    /// Move the workspace with this `slug` one slot, swapping its persisted
    /// `position` with the adjacent DB-backed workspace and mirroring the swap
    /// into `model.sidebar_workspaces` so it shows at once. Live-only workspaces
    /// (no DB row, hence no position) are skipped. Rebuilds; the caller places
    /// the cursor. Returns whether it moved.
    pub(crate) fn move_workspace_by_slug(
        &mut self,
        model: &mut FrameModel,
        session: &crate::session::Session,
        slug: &str,
        up: bool,
    ) -> bool {
        // The reorderable (DB-backed) workspaces in display order, as
        // (full-vec index, repo_path). Empty repo_path = live-only, no position.
        let order: Vec<(usize, String)> = model
            .sidebar_workspaces
            .iter()
            .enumerate()
            .filter(|(_, (_, _, _, repo))| !repo.is_empty())
            .map(|(i, (_, _, _, repo))| (i, repo.clone()))
            .collect();
        // Locate the workspace within that order by slug.
        let Some(p) = model
            .sidebar_workspaces
            .iter()
            .position(|(s, _, _, _)| s == slug)
            .and_then(|fi| order.iter().position(|(i, _)| *i == fi))
        else {
            return false;
        };
        let neighbor = if up {
            p.checked_sub(1)
        } else {
            (p + 1 < order.len()).then_some(p + 1)
        };
        let Some(np) = neighbor else { return false };
        let (ia, repo_a) = order[p].clone();
        let (ib, repo_b) = order[np].clone();

        if let Ok(db) = thegn_core::db::Db::open() {
            // best-effort: same cache rule as the worktree swap above — the
            // live model swap below is the user-visible move
            let _ = db.swap_workspace_positions(&repo_a, &repo_b);
        }
        model.sidebar_workspaces.swap(ia, ib);
        self.rebuild(model, session);
        true
    }

    /// Reorder the current selection (Shift+↑/↓) one slot. The selection is
    /// homogeneous by the **cursor row's kind**: worktrees or workspaces. Marks
    /// of the other kind are ignored; with nothing marked, the cursor row moves
    /// alone (matching the single-item Ctrl+Alt behaviour). A worktree selection
    /// spanning >1 workspace is refused (worktrees only reorder within their
    /// own workspace). Returns whether anything moved.
    pub(crate) fn reorder_selection(
        &mut self,
        model: &mut FrameModel,
        session: &mut crate::session::Session,
        up: bool,
    ) -> bool {
        use crate::sidebar::{RowKind, RowTarget};
        let Some(cursor_row) = self.selected_row(model) else {
            return false;
        };
        let cursor_kind = cursor_row.kind;
        let cursor_key = cursor_row.pin_key.clone();

        match cursor_kind {
            RowKind::Worktree => {
                // Selected worktree groups (marked rows of this kind, else the
                // cursor's group), captured as stable group *names* since a swap
                // shifts vec indices.
                let mut sel_groups: Vec<usize> = model
                    .sidebar_rows
                    .iter()
                    .filter(|r| {
                        r.visible && r.kind == RowKind::Worktree && self.marked.contains(&r.pin_key)
                    })
                    .filter_map(|r| match r.tab_target {
                        Some(RowTarget::Tab(g, _)) => Some(g),
                        _ => None,
                    })
                    .collect();
                if sel_groups.is_empty()
                    && let Some(RowTarget::Tab(g, _)) = self.cursor_target(model)
                {
                    sel_groups.push(g);
                }
                let sel_names: HashSet<String> = sel_groups
                    .iter()
                    .filter_map(|&g| session.worktrees.get(g).map(|x| x.name.clone()))
                    .collect();
                if sel_names.is_empty() {
                    return false;
                }
                // Worktrees only reorder within their own workspace.
                let slugs: HashSet<String> = sel_names
                    .iter()
                    .filter_map(|n| crate::sidebar::split_tab(n).map(|(s, _)| s))
                    .collect();
                if slugs.len() > 1 {
                    model.status = "Can't move a selection across workspaces".into();
                    return false;
                }
                // Process in display order — top-first for up, bottom-first for
                // down — so a block moves as a unit and two selected neighbours
                // never swap with each other.
                let mut ordered: Vec<String> = sidebar_worktree_order(model)
                    .iter()
                    .filter_map(|&g| session.worktrees.get(g).map(|x| x.name.clone()))
                    .filter(|n| sel_names.contains(n))
                    .collect();
                if !up {
                    ordered.reverse();
                }
                let mut moved = false;
                for name in &ordered {
                    let order = sidebar_worktree_order(model);
                    let Some(gi) = session.worktrees.iter().position(|g| &g.name == name) else {
                        continue;
                    };
                    let Some(p) = order.iter().position(|&g| g == gi) else {
                        continue;
                    };
                    let neighbor = if up {
                        p.checked_sub(1)
                    } else {
                        (p + 1 < order.len()).then_some(p + 1)
                    };
                    // Don't swap two selected items past each other.
                    if let Some(np) = neighbor
                        && let Some(nb_name) =
                            session.worktrees.get(order[np]).map(|g| g.name.clone())
                        && sel_names.contains(&nb_name)
                    {
                        continue;
                    }
                    if self.move_worktree_group(model, session, gi, up) {
                        moved = true;
                    }
                }
                if moved {
                    self.cursor =
                        visible_index_of_pin_key(model, &cursor_key).unwrap_or(self.cursor);
                    self.sync(model);
                }
                moved
            }
            RowKind::Workspace => {
                // Selected workspace slugs (marked headers, else the cursor's).
                let mut sel_slugs: HashSet<String> = model
                    .sidebar_rows
                    .iter()
                    .filter(|r| {
                        r.visible
                            && r.kind == RowKind::Workspace
                            && self.marked.contains(&r.pin_key)
                    })
                    .map(|r| r.workspace_slug.clone())
                    .collect();
                if sel_slugs.is_empty()
                    && let Some(row) = self.selected_row(model)
                {
                    sel_slugs.insert(row.workspace_slug.clone());
                }
                if sel_slugs.is_empty() {
                    return false;
                }
                let display_order = |model: &FrameModel| -> Vec<String> {
                    model
                        .sidebar_rows
                        .iter()
                        .filter(|r| r.visible && r.kind == RowKind::Workspace)
                        .map(|r| r.workspace_slug.clone())
                        .collect::<Vec<_>>()
                };
                let mut ordered: Vec<String> = display_order(model)
                    .into_iter()
                    .filter(|s| sel_slugs.contains(s))
                    .collect();
                if !up {
                    ordered.reverse();
                }
                let mut moved = false;
                for slug in &ordered {
                    let disp = display_order(model);
                    let Some(p) = disp.iter().position(|s| s == slug) else {
                        continue;
                    };
                    let neighbor = if up {
                        p.checked_sub(1)
                    } else {
                        (p + 1 < disp.len()).then_some(p + 1)
                    };
                    if let Some(np) = neighbor
                        && sel_slugs.contains(&disp[np])
                    {
                        continue;
                    }
                    if self.move_workspace_by_slug(model, session, slug, up) {
                        moved = true;
                    }
                }
                if moved {
                    self.cursor =
                        visible_index_of_pin_key(model, &cursor_key).unwrap_or(self.cursor);
                    self.sync(model);
                }
                moved
            }
            _ => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hydrate::build_initial_model;
    use crate::run::{SidebarOutcome, now_secs};
    use crate::session::{GroupKind, Session, WorktreeGroup};
    use crate::sidebar::SortMode;
    use crate::testenv::ENV_LOCK;
    use termwiz::input::{KeyCode, Modifiers};

    /// Isolate the user DB: the move helpers open it to persist `position`
    /// swaps. The swap no-ops on the throwaway `/tmp` paths — the tests assert
    /// the in-memory reorder, which is the user-visible part.
    struct DbGuard {
        home: std::path::PathBuf,
        _lock: std::sync::MutexGuard<'static, ()>,
    }
    impl DbGuard {
        fn new(tag: &str) -> Self {
            let lock = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
            let home = std::env::temp_dir().join(format!(
                "thegn-reorder-{tag}-{}-{}",
                std::process::id(),
                now_secs()
            ));
            // SAFETY: guarded by ENV_LOCK; cleared on drop.
            unsafe { std::env::set_var("XDG_STATE_HOME", &home) };
            Self { home, _lock: lock }
        }
    }
    impl Drop for DbGuard {
        fn drop(&mut self) {
            // SAFETY: still under ENV_LOCK for this guard's lifetime.
            unsafe { std::env::remove_var("XDG_STATE_HOME") };
            let _ = std::fs::remove_dir_all(&self.home);
        }
    }

    /// A single-workspace session: the first label is home, the rest branches.
    fn app_session(labels: &[&str]) -> Session {
        let worktrees = labels
            .iter()
            .enumerate()
            .map(|(i, l)| {
                let kind = if i == 0 {
                    GroupKind::Home
                } else {
                    GroupKind::Branch
                };
                WorktreeGroup::new(format!("app/{l}"), kind, format!("/tmp/app-{l}"))
            })
            .collect();
        Session {
            id: "s1".into(),
            worktrees,
            active: 0,
        }
    }

    fn one_ws_model(session: &Session) -> FrameModel {
        let mut m = build_initial_model(session, None);
        m.sidebar_workspaces = vec![("app".into(), "app".into(), "repo".into(), String::new())];
        m
    }

    fn focused(model: &mut FrameModel, session: &Session) -> SidebarState {
        let mut sb = SidebarState {
            focused: true,
            ..Default::default()
        };
        sb.rebuild(model, session);
        sb
    }

    fn key_of(model: &FrameModel, label: &str) -> String {
        model
            .sidebar_rows
            .iter()
            .find(|r| r.label == label)
            .map(|r| r.pin_key.clone())
            .expect("row present")
    }
    fn vidx(model: &FrameModel, label: &str) -> usize {
        model
            .sidebar_rows
            .iter()
            .filter(|r| r.visible)
            .position(|r| r.label == label)
            .expect("row visible")
    }
    fn names(session: &Session) -> Vec<String> {
        session.worktrees.iter().map(|g| g.name.clone()).collect()
    }

    #[test]
    fn marks_survive_rebuild_across_collapse_and_sort() {
        let session = app_session(&["home", "alpha", "beta"]);
        let mut model = one_ws_model(&session);
        let mut sb = focused(&mut model, &session);
        sb.marked.insert(key_of(&model, "alpha"));
        sb.sync(&mut model);
        assert!(model.sidebar_marked.contains(&vidx(&model, "alpha")));

        // Collapse the workspace: alpha's row is hidden but still emitted, so the
        // identity mark is retained (not pruned).
        sb.view.collapsed.insert("app".into());
        sb.rebuild(&mut model, &session);
        assert!(sb.marked.contains("app/alpha"));

        // Expand again: the mark re-projects onto alpha's current visible index.
        sb.view.collapsed.remove("app");
        sb.rebuild(&mut model, &session);
        assert!(model.sidebar_marked.contains(&vidx(&model, "alpha")));

        // A sort change reshuffles indices; the identity mark still lands right.
        sb.view.sort = SortMode::Name;
        sb.rebuild(&mut model, &session);
        assert!(model.sidebar_marked.contains(&vidx(&model, "alpha")));
    }

    #[test]
    fn stale_mark_pruned_when_row_removed() {
        let session = app_session(&["home", "alpha", "beta"]);
        let mut model = one_ws_model(&session);
        let mut sb = focused(&mut model, &session);
        sb.marked.insert(key_of(&model, "alpha"));
        sb.marked.insert(key_of(&model, "beta"));
        sb.sync(&mut model);

        // Rebuild against a session that no longer has beta.
        let session2 = app_session(&["home", "alpha"]);
        sb.rebuild(&mut model, &session2);
        assert!(sb.marked.contains("app/alpha"));
        assert!(!sb.marked.contains("app/beta"), "gone row's mark is pruned");
    }

    #[test]
    fn space_marks_workspace_header_without_collapsing() {
        let session = app_session(&["home", "alpha"]);
        let mut model = one_ws_model(&session);
        let mut sb = focused(&mut model, &session);
        sb.cursor = vidx(&model, "app"); // the workspace header row
        let was_collapsed = sb.view.collapsed.contains("app");

        sb.handle_key(&KeyCode::Char(' '), Modifiers::NONE, &mut model, &session);
        assert!(sb.marked.contains("app"), "workspace header is now marked");
        assert_eq!(
            sb.view.collapsed.contains("app"),
            was_collapsed,
            "Space marks, it no longer collapses the header"
        );
        assert!(model.sidebar_marked.contains(&vidx(&model, "app")));
    }

    #[test]
    fn shift_arrow_returns_reorder_outcome() {
        let session = app_session(&["home", "alpha"]);
        let mut model = one_ws_model(&session);
        let mut sb = focused(&mut model, &session);
        let out = sb.handle_key(&KeyCode::UpArrow, Modifiers::SHIFT, &mut model, &session);
        assert!(matches!(out, SidebarOutcome::ReorderSelection { up: true }));
        let out = sb.handle_key(&KeyCode::DownArrow, Modifiers::SHIFT, &mut model, &session);
        assert!(matches!(
            out,
            SidebarOutcome::ReorderSelection { up: false }
        ));
    }

    #[test]
    fn reorder_moves_marked_worktree_block_as_a_unit() {
        let _db = DbGuard::new("block");
        let mut session = app_session(&["home", "a", "b", "c"]);
        let mut model = one_ws_model(&session);
        let mut sb = focused(&mut model, &session);
        sb.cursor = vidx(&model, "b");
        sb.marked.insert(key_of(&model, "b"));
        sb.marked.insert(key_of(&model, "c"));
        sb.sync(&mut model);

        assert!(sb.reorder_selection(&mut model, &mut session, true));
        assert_eq!(
            names(&session),
            vec!["app/home", "app/b", "app/c", "app/a"],
            "the {{b,c}} block moved up one slot, 'a' fell through"
        );
        // Both stay marked and the cursor rides with the item it was on.
        assert!(sb.marked.contains("app/b") && sb.marked.contains("app/c"));
        assert_eq!(sb.cursor, vidx(&model, "b"));
    }

    #[test]
    fn reorder_block_against_home_anchors_without_leapfrog() {
        let _db = DbGuard::new("anchor");
        let mut session = app_session(&["home", "a", "b"]);
        let mut model = one_ws_model(&session);
        let mut sb = focused(&mut model, &session);
        sb.cursor = vidx(&model, "a");
        sb.marked.insert(key_of(&model, "a"));
        sb.marked.insert(key_of(&model, "b"));
        sb.sync(&mut model);

        // The block is already flush against home: nothing moves, and the two
        // selected rows must not swap past each other.
        assert!(!sb.reorder_selection(&mut model, &mut session, true));
        assert_eq!(names(&session), vec!["app/home", "app/a", "app/b"]);
    }

    #[test]
    fn reorder_single_cursor_item_with_no_marks() {
        let _db = DbGuard::new("single");
        let mut session = app_session(&["home", "a", "b"]);
        let mut model = one_ws_model(&session);
        let mut sb = focused(&mut model, &session);
        sb.cursor = vidx(&model, "a");

        // Nothing marked → move the cursor's worktree down one slot.
        assert!(sb.reorder_selection(&mut model, &mut session, false));
        assert_eq!(names(&session), vec!["app/home", "app/b", "app/a"]);
        assert_eq!(sb.cursor, vidx(&model, "a"));
    }

    #[test]
    fn reorder_refuses_worktrees_across_workspaces() {
        let session_owned = Session {
            id: "s1".into(),
            worktrees: vec![
                WorktreeGroup::new("app/home", GroupKind::Home, "/tmp/app"),
                WorktreeGroup::new("app/a", GroupKind::Branch, "/tmp/app-a"),
                WorktreeGroup::new("lib/home", GroupKind::Home, "/tmp/lib"),
                WorktreeGroup::new("lib/x", GroupKind::Branch, "/tmp/lib-x"),
            ],
            active: 0,
        };
        let mut session = session_owned;
        let mut model = build_initial_model(&session, None);
        model.sidebar_workspaces = vec![
            ("app".into(), "app".into(), "repo".into(), String::new()),
            ("lib".into(), "lib".into(), "repo".into(), String::new()),
        ];
        let mut sb = focused(&mut model, &session);
        sb.cursor = vidx(&model, "a");
        sb.marked.insert(key_of(&model, "a")); // app/a
        sb.marked.insert(key_of(&model, "x")); // lib/x
        sb.sync(&mut model);

        assert!(!sb.reorder_selection(&mut model, &mut session, true));
        assert!(model.status.contains("across workspaces"));
        assert_eq!(
            names(&session),
            vec!["app/home", "app/a", "lib/home", "lib/x"],
            "nothing moved"
        );
    }

    #[test]
    fn reorder_marked_workspace_block() {
        let _db = DbGuard::new("ws");
        // DB-backed workspaces (non-empty repo_path) are the reorderable ones.
        let session = app_session(&["home"]);
        let mut model = build_initial_model(&session, None);
        model.sidebar_workspaces = vec![
            ("app".into(), "app".into(), "repo".into(), "/tmp/app".into()),
            ("lib".into(), "lib".into(), "repo".into(), "/tmp/lib".into()),
            ("zed".into(), "zed".into(), "repo".into(), "/tmp/zed".into()),
        ];
        let mut session = session;
        let mut sb = focused(&mut model, &session);
        sb.cursor = vidx(&model, "app"); // a workspace header ⇒ workspace kind
        sb.marked.insert("app".into());
        sb.marked.insert("lib".into());
        sb.sync(&mut model);

        assert!(sb.reorder_selection(&mut model, &mut session, false));
        let order: Vec<String> = model
            .sidebar_workspaces
            .iter()
            .map(|(s, _, _, _)| s.clone())
            .collect();
        assert_eq!(
            order,
            vec!["zed", "app", "lib"],
            "the app+lib block moved down"
        );
    }
}
