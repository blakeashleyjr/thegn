//! Loop-side handling of worktree-group delete requests (`Action::CloseWorktree`
//! and `SidebarOutcome::DeleteGroups`). Extracted from `run.rs` (file-size
//! ratchet) and, critically, adds the uncommitted-changes safety net: a dirty
//! target forces the warning confirm menu even when `confirm_delete` is off, so
//! a delete-from-disk of unsaved work is never silent.
//!
//! Runs ON the loop and stays I/O-free: dirtiness is a cached in-memory lookup
//! into `sidebar_status.git` (populated off-thread by the hydration pass) — this
//! MUST NOT run blocking git on the event loop.

use crate::compositor::Rect;
use crate::menu::{self, MenuOverlay};

pub(crate) struct DeleteCtx<'a> {
    pub session: &'a mut crate::session::Session,
    pub panes: &'a mut crate::panes::Panes,
    pub model: &'a mut crate::chrome::FrameModel,
    pub sb: &'a mut crate::run::SidebarState,
    pub drawer: &'a mut Option<u32>,
    pub drawer_pool: &'a mut crate::run::DrawerPool,
    pub drawer_home: &'a mut Option<std::path::PathBuf>,
    pub active_menu: &'a mut Option<MenuOverlay>,
    pub pending: &'a mut Option<Vec<usize>>,
    pub need_relayout: &'a mut bool,
    pub waker: &'a termwiz::terminal::TerminalWaker,
    pub cfg: &'a superzej_core::config::Config,
    pub center: Rect,
    pub confirm_delete: bool,
}

/// Resolve a delete request into either a confirm menu (stashing the pending
/// targets for the menu's Pick handler) or an immediate disk-removal + UI
/// refresh. A target is "dirty" when its cached `GitGlyphs.dirty` is set; ANY
/// dirty target forces the warning menu regardless of `confirm_delete`. Sets
/// `model.status` on every path.
pub(crate) fn request_group_delete(mut cx: DeleteCtx<'_>, raw_targets: Vec<usize>) {
    let (targets, skipped_home) = crate::run::deletable_group_targets(cx.session, raw_targets);
    if targets.is_empty() {
        cx.model.status = if skipped_home > 0 {
            "Root workspace cannot be deleted".into()
        } else {
            "No worktree selected".into()
        };
        return;
    }

    // Names for the menu body + the dirty subset, from the cached sidebar
    // status (keyed by worktree path). A missing entry (not yet hydrated) is
    // best-effort treated as clean.
    let mut names = Vec::with_capacity(targets.len());
    let mut dirty_names = Vec::new();
    for &gi in &targets {
        if let Some(g) = cx.session.worktrees.get(gi) {
            names.push(g.name.clone());
            let is_dirty = cx
                .model
                .sidebar_status
                .git
                .get(&g.path)
                .map(|gl| gl.dirty)
                .unwrap_or(false);
            if is_dirty {
                dirty_names.push(g.name.clone());
            }
        }
    }
    let any_dirty = !dirty_names.is_empty();

    // Dirty ALWAYS confirms (safety net); clean confirms only when configured.
    if any_dirty || cx.confirm_delete {
        *cx.active_menu = Some(if any_dirty {
            menu::delete_worktree_menu_dirty(dirty_names.len(), &dirty_names.join(", "))
        } else {
            menu::delete_worktree_menu(names.len(), &names.join(", "))
        });
        *cx.pending = Some(targets);
        return;
    }

    // Clean + confirm disabled: remove from disk now, then refresh the UI.
    perform_delete(&mut cx, targets);
}

/// Delete `targets` from disk (keep_files = false) and rebuild sidebar/drawer
/// state, preserving focus across index shifts by stable group name.
/// `delete_groups` sorts targets descending internally, so no pre-sort here.
fn perform_delete(cx: &mut DeleteCtx<'_>, targets: Vec<usize>) {
    // Remember the active group's name AND workspace slug up front: `delete_groups`
    // removes groups and leaves `session.active` pointing at whatever slid into the
    // deleted slot — which may be a Terminal.
    let active_name = cx.session.active_group().map(|g| g.name.clone());
    let active_slug = active_name
        .as_deref()
        .and_then(|n| crate::sidebar::split_tab(n).map(|(s, _)| s));

    cx.model.status =
        crate::run::delete_groups(cx.session, cx.panes, targets, false, Some(cx.waker.clone()));

    // Restore focus: prefer the still-living active group (it survived the delete);
    // if it was itself deleted, land on the home worktree of its workspace rather
    // than on whichever terminal happened to slide into the active slot.
    let target = active_name
        .as_deref()
        .and_then(|name| cx.session.worktrees.iter().position(|g| g.name == name))
        .or_else(|| landing_for_slug(cx.session, active_slug.as_deref()));
    if let Some(idx) = target {
        cx.session.switch_to(idx);
    }

    cx.sb.marked.clear();
    crate::run::refresh_tab_model(cx.model, cx.session, cx.sb);
    cx.sb.focus_active_row(cx.model);
    *cx.need_relayout = true;
    crate::run::sync_drawer_persistence(
        cx.session,
        cx.panes,
        cx.drawer,
        cx.drawer_pool,
        cx.drawer_home,
        cx.cfg,
        cx.center,
    );
}

/// Pick a non-terminal group to focus after the active worktree was deleted.
/// Priority: (1) the home worktree of `slug`, (2) any non-terminal worktree of
/// `slug`, (3) the first non-terminal group anywhere. A workspace's home is
/// never deletable (`delete_groups` skips `GroupKind::Home`), so #1 resolves
/// whenever `slug` is known — this keeps focus in the workspace, never on the
/// Terminals section.
fn landing_for_slug(session: &crate::session::Session, slug: Option<&str>) -> Option<usize> {
    use crate::session::GroupKind;
    let slug_of = |name: &str| crate::sidebar::split_tab(name).map(|(s, _)| s);
    if let Some(slug) = slug {
        // Home of the same workspace first.
        if let Some(i) = session
            .worktrees
            .iter()
            .position(|g| g.kind == GroupKind::Home && slug_of(&g.name).as_deref() == Some(slug))
        {
            return Some(i);
        }
        // Any surviving non-terminal worktree of the same workspace.
        if let Some(i) = session.worktrees.iter().position(|g| {
            g.kind != GroupKind::Terminal && slug_of(&g.name).as_deref() == Some(slug)
        }) {
            return Some(i);
        }
    }
    // Fall back to the first non-terminal group anywhere.
    session
        .worktrees
        .iter()
        .position(|g| g.kind != GroupKind::Terminal)
}

#[cfg(test)]
mod tests {
    use super::landing_for_slug;
    use crate::session::{GroupKind, Session, WorktreeGroup};

    fn group(name: &str, kind: GroupKind) -> WorktreeGroup {
        WorktreeGroup::new(name.to_string(), kind, String::new())
    }

    fn session_with(groups: Vec<WorktreeGroup>) -> Session {
        let mut s = Session::default();
        for g in groups {
            s.add_group(g);
        }
        s.active = 0;
        s
    }

    #[test]
    fn lands_on_workspace_home_not_terminal() {
        // A terminal sits right after the branch worktree being deleted; the
        // landing must be the workspace's home, never the terminal.
        let s = session_with(vec![
            group("app/home", GroupKind::Home),
            group("term", GroupKind::Terminal),
        ]);
        assert_eq!(landing_for_slug(&s, Some("app")), Some(0));
    }

    #[test]
    fn prefers_home_over_sibling_branch() {
        let s = session_with(vec![
            group("app/other", GroupKind::Branch),
            group("app/home", GroupKind::Home),
            group("term", GroupKind::Terminal),
        ]);
        assert_eq!(landing_for_slug(&s, Some("app")), Some(1));
    }

    #[test]
    fn falls_back_to_first_non_terminal_when_slug_unknown() {
        let s = session_with(vec![
            group("term", GroupKind::Terminal),
            group("app/home", GroupKind::Home),
        ]);
        assert_eq!(landing_for_slug(&s, None), Some(1));
    }

    #[test]
    fn cross_workspace_home_not_chosen_over_own_branch() {
        // No home for slug "app" survives, so its own surviving branch wins over
        // another workspace's home.
        let s = session_with(vec![
            group("other/home", GroupKind::Home),
            group("app/feat", GroupKind::Branch),
            group("term", GroupKind::Terminal),
        ]);
        assert_eq!(landing_for_slug(&s, Some("app")), Some(1));
    }
}
