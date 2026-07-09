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
///
/// Focus landing (when the active worktree is itself deleted): move to the
/// **next** worktree within the same workspace in sidebar-visual order — or the
/// **previous** one when the deleted worktree was last — mirroring
/// `NextWorktree` navigation. `landing_for_slug` (home-first, then global) is
/// only the last-resort fallback for edge cases where that neighbor can't be
/// resolved (collapsed workspace, terminal-active, cross-workspace).
fn perform_delete(cx: &mut DeleteCtx<'_>, targets: Vec<usize>) {
    // Remember the active group's name AND workspace slug up front: `delete_groups`
    // removes groups and leaves `session.active` pointing at whatever slid into the
    // deleted slot — which may be a Terminal.
    let active_name = cx.session.active_group().map(|g| g.name.clone());
    let active_slug = active_name
        .as_deref()
        .and_then(|n| crate::sidebar::split_tab(n).map(|(s, _)| s));

    // Compute the neighbor to land on BEFORE deleting (the pre-delete
    // `sidebar_rows` still reflect the on-screen order; `refresh_tab_model` runs
    // later). Confine to the active workspace's slug so we never cross into
    // another workspace, exactly like the `NextWorktree` handler. Capture the
    // neighbor as a stable name because `delete_groups` shifts indices.
    let neighbor_name = {
        let order: Vec<usize> = crate::run::sidebar_worktree_order(cx.model)
            .into_iter()
            .filter(|&g| {
                cx.session
                    .worktrees
                    .get(g)
                    .and_then(|w| crate::sidebar::split_tab(&w.name).map(|(s, _)| s))
                    .as_deref()
                    == active_slug.as_deref()
            })
            .collect();
        let deleted: std::collections::HashSet<usize> = targets.iter().copied().collect();
        order
            .iter()
            .position(|&g| g == cx.session.active)
            .and_then(|pos| next_or_prev(&order, pos, &deleted))
            .and_then(|gi| cx.session.worktrees.get(gi).map(|g| g.name.clone()))
    };

    cx.model.status =
        crate::run::delete_groups(cx.session, cx.panes, targets, false, Some(cx.waker.clone()));

    // Restore focus: (a) keep the still-living active group (it survived the
    // delete); (b) if it was deleted, land on the next/prev worktree in the same
    // workspace (sidebar-visual order); (c) fall back to the workspace home, then
    // the first non-terminal group anywhere.
    let target = active_name
        .as_deref()
        .and_then(|name| cx.session.worktrees.iter().position(|g| g.name == name))
        .or_else(|| {
            neighbor_name
                .as_deref()
                .and_then(|name| cx.session.worktrees.iter().position(|g| g.name == name))
        })
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

/// Given worktree group indices in sidebar-visual order (`order`), the position
/// of the active worktree within that order (`pos`), and the set of indices
/// being deleted, pick the neighbor to land on: the nearest surviving worktree
/// AFTER the active one, else the nearest surviving worktree BEFORE it. Returns
/// the pre-delete group index, or None if no neighbor survives. Pure so the
/// next/prev landing rule is unit-tested without a `FrameModel`.
fn next_or_prev(
    order: &[usize],
    pos: usize,
    deleted: &std::collections::HashSet<usize>,
) -> Option<usize> {
    let survives = |g: &&usize| !deleted.contains(g);
    // Forward: the next worktree; else backward: the previous one (deleted last).
    order[pos + 1..]
        .iter()
        .find(survives)
        .or_else(|| order[..pos].iter().rev().find(survives))
        .copied()
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
    use super::{landing_for_slug, next_or_prev};
    use crate::session::{GroupKind, Session, WorktreeGroup};
    use std::collections::HashSet;

    fn deleted(items: &[usize]) -> HashSet<usize> {
        items.iter().copied().collect()
    }

    #[test]
    fn next_or_prev_lands_on_next_when_middle_deleted() {
        // Visual order [10, 20, 30]; active 20 (pos 1) deleted → next is 30.
        let order = [10, 20, 30];
        assert_eq!(next_or_prev(&order, 1, &deleted(&[20])), Some(30));
    }

    #[test]
    fn next_or_prev_lands_on_previous_when_last_deleted() {
        // Active is last (pos 2); nothing after it → previous survivor 20.
        let order = [10, 20, 30];
        assert_eq!(next_or_prev(&order, 2, &deleted(&[30])), Some(20));
    }

    #[test]
    fn next_or_prev_skips_a_run_of_deletions() {
        // Active 10 (pos 0) plus 20 and 30 all deleted → first survivor after is 40.
        let order = [10, 20, 30, 40];
        assert_eq!(next_or_prev(&order, 0, &deleted(&[10, 20, 30])), Some(40));
    }

    #[test]
    fn next_or_prev_finds_home_at_index_zero_backward() {
        // Only survivor is the home worktree at the top; active last is deleted.
        let order = [1, 2];
        assert_eq!(next_or_prev(&order, 1, &deleted(&[2])), Some(1));
    }

    #[test]
    fn next_or_prev_none_when_nothing_survives() {
        // The whole workspace order is being deleted → no neighbor.
        let order = [1, 2, 3];
        assert_eq!(next_or_prev(&order, 1, &deleted(&[1, 2, 3])), None);
    }

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
