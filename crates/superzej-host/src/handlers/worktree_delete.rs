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
    let active_name = cx.session.active_group().map(|g| g.name.clone());

    cx.model.status =
        crate::run::delete_groups(cx.session, cx.panes, targets, false, Some(cx.waker.clone()));

    if let Some(name) = active_name
        && let Some(idx) = cx.session.worktrees.iter().position(|g| g.name == name)
    {
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
