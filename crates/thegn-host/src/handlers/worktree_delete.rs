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

/// Map target indices to their groups' stable names (skipping any out of range),
/// for stashing across a confirm modal.
pub(crate) fn group_names_for(session: &crate::session::Session, targets: &[usize]) -> Vec<String> {
    targets
        .iter()
        .filter_map(|&i| session.worktrees.get(i).map(|g| g.name.clone()))
        .collect()
}

/// Re-resolve stashed group names to their *current* indices. Names that no
/// longer exist (reaped while the modal was open) are dropped, so a confirm can
/// only ever act on groups that still exist under the name the user saw.
pub(crate) fn resolve_group_indices(
    session: &crate::session::Session,
    names: &[String],
) -> Vec<usize> {
    names
        .iter()
        .filter_map(|name| session.worktrees.iter().position(|g| &g.name == name))
        .collect()
}

pub(crate) struct DeleteCtx<'a> {
    pub session: &'a mut crate::session::Session,
    pub panes: &'a mut crate::panes::Panes,
    pub model: &'a mut crate::chrome::FrameModel,
    pub sb: &'a mut crate::run::SidebarState,
    pub drawer_runtime: &'a mut crate::run::DrawerRuntime,
    pub active_menu: &'a mut Option<MenuOverlay>,
    /// Targets stashed across the confirm modal as **stable group names**, not
    /// indices: a background reap/prune can shift `session.worktrees` indices
    /// while the modal is open, so an index captured at open time would resolve
    /// to a *different* worktree on confirm (and delete the wrong files).
    /// Re-resolved to current indices via [`resolve_group_indices`] on confirm.
    pub pending: &'a mut Option<Vec<String>>,
    pub need_relayout: &'a mut bool,
    pub waker: &'a termwiz::terminal::TerminalWaker,
    pub cfg: &'a thegn_core::config::Config,
    pub center: Rect,
    pub confirm_delete: bool,
}

/// Resolve the sidebar's `d` into the close-or-delete chooser: a
/// *disambiguation* modal (close = safe default / delete = danger arm), so it
/// ALWAYS opens regardless of `confirm_delete`. The dirty-tree variant
/// preserves the safety net verbatim — it shouts the dirty names and
/// pre-selects Cancel. Pending targets are stashed for the Pick handler
/// (`ConfirmCloseWorktrees` / `ConfirmDeleteWorktrees`).
pub(crate) fn request_close_or_delete(mut cx: DeleteCtx<'_>, raw_targets: Vec<usize>) {
    let (targets, skipped_home) = crate::run::deletable_group_targets(cx.session, raw_targets);
    if targets.is_empty() {
        cx.model.status = if skipped_home > 0 {
            "The home worktree can't be closed or deleted".into()
        } else {
            "No worktree selected".into()
        };
        return;
    }
    let (names, dirty_names) = target_names(&mut cx, &targets);
    *cx.active_menu = Some(if dirty_names.is_empty() {
        menu::close_or_delete_menu(names.len(), &names.join(", "))
    } else {
        menu::close_or_delete_menu_dirty(dirty_names.len(), &dirty_names.join(", "))
    });
    *cx.pending = Some(group_names_for(cx.session, &targets));
}

/// Close (forget) the worktree groups at `targets` — the `[c]` arm of the
/// chooser and the menu's "Close": branch + files stay on disk, the groups
/// leave the session and the DB registry. Restores focus to the pre-close
/// active group when it survives. (The former inline `CloseGroups` body from
/// `run.rs`, shared by the direct outcome and the chooser's Pick handler.)
pub(crate) fn perform_close(cx: &mut DeleteCtx<'_>, targets: Vec<usize>) {
    let (mut targets, skipped_home) = crate::run::deletable_group_targets(cx.session, targets);
    if targets.is_empty() {
        cx.model.status = if skipped_home > 0 {
            "The home worktree can't be closed".into()
        } else {
            "No worktree selected".into()
        };
        return;
    }

    // Capture the groups and worktree paths being closed BEFORE the loop shifts indices, so
    // we can optimistically drop their merge-queue rows from the in-memory model
    // (the authoritative DB delete happens in `forget_worktree_group`; this keeps
    // the MQ badge correct on the same frame instead of next hydration tick).
    let removed_paths: Vec<String> = targets
        .iter()
        .filter_map(|&gi| cx.session.worktrees.get(gi).map(|g| g.path.clone()))
        .filter(|p| !p.is_empty())
        .collect();

    let removed_groups: Vec<crate::session::WorktreeGroup> = targets
        .iter()
        .filter_map(|&gi| cx.session.worktrees.get(gi).cloned())
        .collect();
    let session_id = cx.session.id.clone();

    // Closing a group keeps its checkout, so the destroy worker cannot own
    // the session boundary. Release any live session latch before removing
    // the group; otherwise reopening it in this process suppresses the next
    // session_start and session_end never runs for an explicit close.
    let mut session_end_errors = Vec::new();
    for group in &removed_groups {
        if !group.path.is_empty()
            && let Err(error) = crate::worktree_lifecycle::session_end_once(
                cx.cfg,
                std::path::Path::new(&group.path),
                Some(cx.waker.clone()),
            )
        {
            session_end_errors.push(format!("{}: {error}", group.path));
        }
    }

    // Cache pruning is a worker operation. Close from the highest index down so
    // earlier in-memory indices stay valid without opening SQLite on the loop.
    crate::db_task::persist(move |db| {
        for group in &removed_groups {
            crate::run::forget_worktree_group(db, &session_id, group, true);
        }
    });

    // Plan the landing against the rows still on screen, then close from the
    // highest index down so earlier indices stay valid.
    let removed: Vec<&str> = removed_paths.iter().map(String::as_str).collect();
    let landing = Landing::plan(cx.model, cx.session, cx.sb, &removed);
    targets.sort_unstable_by(|a, b| b.cmp(a));
    let mut active_removed = false;
    for gi in targets {
        active_removed |= remove_group(cx.session, cx.panes, gi);
    }
    if active_removed {
        landing.land(cx.session);
    }

    cx.model
        .panel
        .merge_queue
        .retain(|r| !removed_paths.contains(&r.worktree));

    if skipped_home > 0 {
        cx.model.status = "The home worktree can't be closed".into();
    }
    crate::run::persist_session_layout_cached(cx.session);
    cx.sb.marked.clear();
    crate::run::refresh_tab_model(cx.model, cx.session, cx.sb);
    if !session_end_errors.is_empty() {
        cx.model.status = format!(
            "Closed worktree; session_end failed to start: {}",
            session_end_errors.join("; ")
        );
    }
    landing.place_cursor(cx.model, cx.sb);
    *cx.need_relayout = true;
    if let Some(dir) = crate::run::active_cwd(cx.session) {
        cx.drawer_runtime
            .reconcile(cx.cfg, &dir, cx.panes, cx.center);
    }
}

/// Names for the menu body + the dirty subset, from the cached sidebar status
/// (keyed by worktree path). A missing entry (not yet hydrated) is best-effort
/// treated as clean.
fn target_names(cx: &mut DeleteCtx<'_>, targets: &[usize]) -> (Vec<String>, Vec<String>) {
    let mut names = Vec::with_capacity(targets.len());
    let mut dirty_names = Vec::new();
    for &gi in targets {
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
    (names, dirty_names)
}

/// Resolve a delete request into either a confirm menu (stashing the pending
/// targets for the menu's Pick handler) or an immediate disk-removal + UI
/// refresh. A target is "dirty" when its cached `GitGlyphs.dirty` is set; ANY
/// dirty target forces the warning menu regardless of `confirm_delete`. Sets
/// `model.status` on every path.
pub(crate) fn request_group_delete(mut cx: DeleteCtx<'_>, raw_targets: Vec<usize>) {
    // A standalone terminal isn't a git worktree: the delete path below only
    // touches the `worktrees` table (a terminal's `path` is empty), so it would
    // leave the `terminals` registry row + its sidebar entry behind. Route a
    // terminal target to the terminal-close confirm — the `sidebar-close-terminal`
    // menu handler calls `close_terminal`, which deletes the DB row and drops it
    // from the model, exactly like the sidebar `d` flow. Alt-X on a terminal
    // reaches here with the active group as the sole target.
    if let [gi] = raw_targets[..]
        && cx.session.worktrees.get(gi).map(|g| g.kind) == Some(crate::session::GroupKind::Terminal)
    {
        let name = cx.session.worktrees[gi].name.clone();
        *cx.active_menu = Some(menu::confirm_menu(
            format!("Close terminal '{name}'?"),
            "the shell process ends; scrollback is lost",
            "sidebar-close-terminal",
            name,
            true,
        ));
        return;
    }
    let (targets, skipped_home) = crate::run::deletable_group_targets(cx.session, raw_targets);
    if targets.is_empty() {
        cx.model.status = if skipped_home > 0 {
            "Root workspace cannot be deleted".into()
        } else {
            "No worktree selected".into()
        };
        return;
    }

    let (names, dirty_names) = target_names(&mut cx, &targets);
    let any_dirty = !dirty_names.is_empty();

    // Dirty ALWAYS confirms (safety net); clean confirms only when configured.
    if any_dirty || cx.confirm_delete {
        *cx.active_menu = Some(if any_dirty {
            menu::delete_worktree_menu_dirty(dirty_names.len(), &dirty_names.join(", "))
        } else {
            menu::delete_worktree_menu(names.len(), &names.join(", "))
        });
        *cx.pending = Some(group_names_for(cx.session, &targets));
        return;
    }

    // Clean + confirm disabled: remove from disk now, then refresh the UI.
    perform_delete(&mut cx, targets);
}

/// Delete `targets` from disk (keep_files = false).
fn perform_delete(cx: &mut DeleteCtx<'_>, targets: Vec<usize>) {
    confirm_delete_worktrees(cx, targets, false, false);
}

/// The `ConfirmDeleteWorktrees` menu-Pick path from `run.rs`: same body as
/// `perform_delete` but honoring the chooser's `keep_files` (Close-keeps-files
/// vs delete-from-disk). Exposed so the loop's confirm arm delegates here
/// instead of re-inlining `delete_groups` + refresh.
///
/// This only *schedules* the teardown: the groups stay in the session until the
/// destroy worker reports success, so neither focus nor the sidebar cursor
/// moves here. The landing happens when the group actually leaves —
/// `worktree_lifecycle::apply_completions` plans a [`Landing`] against the rows
/// on screen at that moment.
pub(crate) fn confirm_delete_worktrees(
    cx: &mut DeleteCtx<'_>,
    targets: Vec<usize>,
    keep_files: bool,
    force: bool,
) {
    cx.model.status = crate::run::delete_groups_with_mode(
        cx.session,
        cx.panes,
        targets,
        keep_files,
        crate::worktree_lifecycle::mode_for_user(force, false),
        Some(cx.waker.clone()),
    );
    cx.sb.marked.clear();
    crate::run::refresh_tab_model(cx.model, cx.session, cx.sb);
    *cx.need_relayout = true;
}

/// Where focus goes when worktree rows leave the tree, planned against the
/// PRE-removal sidebar rows — the order the user was looking at.
///
/// The removal primitive (`switch_to` + `close_active_group`) lands the active
/// pointer on whatever slid into the freed *session* slot, and session order is
/// not display order (sort, pins, folders, home-first), so on screen that read
/// as a jump to a random row — often the home row at the very top — and
/// `focus_active_row` then dragged a focused cursor there too. Instead:
///
/// - a surviving active worktree stays active ([`remove_group`] re-pins it by
///   name), whichever worktree was removed;
/// - a removed active worktree hands focus to its **next** visible worktree in
///   the same workspace, else the **previous** one — the `NextWorktree` rule —
///   with [`landing_for_slug`] as the last resort (row not visible, no live
///   neighbour);
/// - a focused cursor that sat on a removed row moves to that row's neighbour
///   by the same rule; anywhere else, `SidebarState::rebuild`'s identity
///   re-anchor keeps it on its row. An unfocused cursor follows the active row
///   as it always does.
#[derive(Debug, Default)]
pub(crate) struct Landing {
    /// Group name of the active row's surviving live neighbour.
    active: Option<String>,
    /// The active worktree's workspace slug, for the fallback.
    slug: Option<String>,
    /// `pin_key` of the row a focused cursor on a removed row moves to.
    cursor: Option<String>,
}

impl Landing {
    /// Plan against `model.sidebar_rows` as currently built for `session`,
    /// before any of the worktrees at `removed` (paths) leave it.
    pub(crate) fn plan(
        model: &crate::chrome::FrameModel,
        session: &crate::session::Session,
        sb: &crate::run::SidebarState,
        removed: &[&str],
    ) -> Self {
        use crate::sidebar::{RowKind, RowTarget, SidebarRow};
        let rows: Vec<&SidebarRow> = model.sidebar_rows.iter().filter(|r| r.visible).collect();
        let gone = |r: &SidebarRow| {
            r.worktree_path
                .as_deref()
                .is_some_and(|p| removed.contains(&p))
        };
        // The nearest surviving worktree row of `at`'s workspace that passes
        // `keep`: after `at` first, else before it.
        let neighbour = |at: usize, keep: &dyn Fn(&SidebarRow) -> bool| {
            let slug = &rows[at].workspace_slug;
            let order: Vec<usize> = (0..rows.len())
                .filter(|&i| {
                    i == at
                        || (rows[i].kind == RowKind::Worktree
                            && &rows[i].workspace_slug == slug
                            && keep(rows[i]))
                })
                .collect();
            let deleted: std::collections::HashSet<usize> =
                order.iter().copied().filter(|&i| gone(rows[i])).collect();
            let pos = order.iter().position(|&i| i == at)?;
            next_or_prev(&order, pos, &deleted)
        };

        let active_group = session.active_group();
        let slug = active_group.and_then(|g| crate::sidebar::split_tab(&g.name).map(|(s, _)| s));
        // Resolved by path, not by the row's group index: a batch of worker
        // completions removes several groups against one plan, and the rows'
        // `Tab(gi, _)` indices go stale after the first.
        let active = active_group
            .filter(|g| !g.path.is_empty() && removed.contains(&g.path.as_str()))
            .and_then(|g| {
                rows.iter().position(|r| {
                    r.kind == RowKind::Worktree && r.worktree_path.as_deref() == Some(&g.path)
                })
            })
            .and_then(|at| neighbour(at, &|r| matches!(r.tab_target, Some(RowTarget::Tab(..)))))
            .and_then(|i| rows[i].worktree_path.as_deref())
            .and_then(|p| session.worktrees.iter().find(|g| g.path == p))
            .map(|g| g.name.clone());
        let cursor = Some(sb.cursor)
            .filter(|&c| sb.focused && rows.get(c).is_some_and(|r| gone(r)))
            .and_then(|c| neighbour(c, &|_| true))
            .map(|i| rows[i].pin_key.clone())
            .filter(|k| !k.is_empty());
        Self {
            active,
            slug,
            cursor,
        }
    }

    /// Re-point `session.active` after the active worktree was removed.
    pub(crate) fn land(&self, session: &mut crate::session::Session) {
        let target = self
            .active
            .as_deref()
            .and_then(|name| session.worktrees.iter().position(|g| g.name == name))
            .or_else(|| landing_for_slug(session, self.slug.as_deref()));
        if let Some(idx) = target {
            session.switch_to(idx);
        }
    }

    /// Seat a focused cursor that sat on a removed row on the planned
    /// neighbour. Call after the rows are rebuilt.
    pub(crate) fn place_cursor(
        &self,
        model: &mut crate::chrome::FrameModel,
        sb: &mut crate::run::SidebarState,
    ) {
        if !sb.focused {
            return;
        }
        if let Some(key) = &self.cursor
            && let Some(idx) = model
                .sidebar_rows
                .iter()
                .filter(|r| r.visible)
                .position(|r| &r.pin_key == key)
        {
            sb.cursor = idx;
            sb.sync(model);
        }
    }
}

/// Remove group `gi` from the session, reaping its panes, WITHOUT moving focus
/// off a surviving active group. Returns whether the removed group was the
/// active one — the caller's cue to [`Landing::land`].
pub(crate) fn remove_group(
    session: &mut crate::session::Session,
    panes: &mut crate::panes::Panes,
    gi: usize,
) -> bool {
    let Some(group) = session.worktrees.get(gi) else {
        return false;
    };
    for tab in &group.tabs {
        for id in tab.center.pane_ids() {
            panes.table.remove(&id);
        }
    }
    // Rendering and Landing::plan use active_group(), which clamps stale
    // indices to the last group. Compare against that same effective index.
    let was_active = gi == session.active.min(session.worktrees.len() - 1);
    let prior = session.active_group().map(|g| g.name.clone());
    session.switch_to(gi);
    session.close_active_group();
    if !was_active
        && let Some(name) = prior
        && let Some(idx) = session.worktrees.iter().position(|g| g.name == name)
    {
        session.switch_to(idx);
    }
    was_active
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
    use super::{group_names_for, landing_for_slug, next_or_prev, resolve_group_indices};
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
    fn pending_targets_track_the_group_by_name_across_an_index_shift() {
        // User selects "b" (index 1) for delete; the confirm modal stashes NAMES.
        let s = session_with(vec![
            group("a", GroupKind::Branch),
            group("b", GroupKind::Branch),
            group("c", GroupKind::Branch),
        ]);
        let names = group_names_for(&s, &[1]);
        assert_eq!(names, vec!["b".to_string()]);

        // While the modal is open a background reap removes "a" (index 0), so
        // "b" is now at index 0. Re-resolving by name must yield the NEW index of
        // "b", not the stale index 1 (which now points at "c").
        let mut shifted = session_with(vec![
            group("b", GroupKind::Branch),
            group("c", GroupKind::Branch),
        ]);
        shifted.active = 0;
        assert_eq!(resolve_group_indices(&shifted, &names), vec![0]);

        // A name that vanished entirely resolves to nothing (never the wrong row).
        assert!(resolve_group_indices(&shifted, &["gone".to_string()]).is_empty());
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
