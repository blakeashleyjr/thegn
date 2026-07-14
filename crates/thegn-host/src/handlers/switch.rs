//! The tab/worktree switch **fast path**, extracted from the ratchet-pinned
//! `run.rs`.
//!
//! A pure switch moves only the session's active pointer (`session.active` /
//! a group's `active_tab`); no row *data* changes and — because no
//! [`crate::sidebar::SortMode`] reads the active pointer — row *order* cannot
//! change either. So instead of the full `refresh_tab_model` (whose
//! `sidebar::build_rows` walks every workspace × worktree with per-row
//! allocations, the only O(N) work on the switch keystroke),
//! [`refresh_tab_model_switch`] patches the existing rows in place with one
//! zero-allocation pass ([`retarget_active`]).
//!
//! # Contract
//!
//! The light path is legal **iff the only session mutation since the last full
//! `refresh_tab_model` is the active pointer** (which group / which tab is
//! focused). Its callers are exactly:
//!
//! - `Action::NextTab` / `Action::PrevTab` ([`cycle_tab`])
//! - `Action::NextWorktree` / `Action::PrevWorktree` ([`cycle_worktree`])
//! - `activate_row_target`'s pure-focus arms (`RowTarget::Tab`, and the
//!   terminal sentinel when the group is already resident)
//!
//! Everything else — workspace switch, terminal materialize, hydration
//! arrival, worktree create/delete, filter/collapse/sort/reorder — keeps
//! calling the full `refresh_tab_model`. The locked
//! `retarget_matches_full_rebuild_*` tests below pin patch ≡ rebuild for a
//! pure pointer move across every sort mode.

use crate::chrome::FrameModel;
use crate::compositor::Rect;
use crate::panes::Panes;
use crate::run::{
    DrawerPool, SidebarState, persist_active_focus, sidebar_worktree_order,
    sync_drawer_persistence, visible_index_of_active,
};
use crate::session::Session;

/// Light sibling of `run::refresh_tab_model` for pure active-pointer moves:
/// same tab-strip/model fields, but
/// - **no filesystem stat** — `container_name` needs only the path *string*;
///   a stale/deleted dir is re-verified by the next off-loop hydration (which
///   also owns the cwd fallback the stat-based path had), and
/// - **no workspace-list merge** — the workspace set cannot change on an
///   in-workspace switch, and
/// - [`retarget_active`] instead of `SidebarState::rebuild`/`build_rows`.
pub(crate) fn refresh_tab_model_switch(
    model: &mut FrameModel,
    session: &Session,
    sb: &mut SidebarState,
) {
    // Escape hatch + A/B lever: `THEGN_SWITCH_FULL_REBUILD=1` routes every
    // switch through the full rebuild — for measuring the fast path's win on
    // one binary (`just perf-flood`), and as a runtime fallback should a
    // stale-row bug surface in the field.
    static FORCE_FULL: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    if *FORCE_FULL
        .get_or_init(|| std::env::var_os("THEGN_SWITCH_FULL_REBUILD").is_some_and(|v| v == "1"))
    {
        return crate::run::refresh_tab_model(model, session, sb);
    }
    let _g = crate::perf::measure(crate::perf::Subsys::Switch);
    let (worktree, tabs, active_tab) = crate::hydrate::tab_strip(session);
    model.worktree = worktree;
    model.tabs = tabs;
    model.active_tab = active_tab;
    model.active_container_name = thegn_core::sandbox::container_name(
        session
            .active_group()
            .map(|g| g.path.as_str())
            .unwrap_or(""),
    );
    // No glyph re-seed here: this fast path only fires for in-workspace pointer
    // moves, whose worktrees' glyphs are already resident in `sidebar_status`.
    // Cross-workspace switches (which need the re-seed) route through the full
    // `refresh_tab_model`, keeping this path pure/zero-alloc.
    retarget_active(sb, model, session);
}

/// The full sidebar/model rebuild for a switch (extracted from the ratchet-pinned
/// `run.rs`; re-exported as `crate::run::refresh_tab_model`). Unlike the light
/// [`refresh_tab_model_switch`], this re-derives the workspace list and rebuilds
/// every row — used whenever more than the active pointer changed (workspace
/// switch, worktree create/delete, hydration arrival, filter/sort).
pub(crate) fn refresh_tab_model(model: &mut FrameModel, session: &Session, sb: &mut SidebarState) {
    let _g = crate::perf::measure(crate::perf::Subsys::Switch);
    let (worktree, tabs, active_tab) = crate::hydrate::tab_strip(session);
    let active_path = crate::hydrate::active_tab_path(session);
    model.worktree = worktree;
    model.tabs = tabs;
    model.active_tab = active_tab;
    model.active_container_name =
        thegn_core::sandbox::container_name(&active_path.to_string_lossy());
    // The workspace list can change when worktrees are added/closed or the
    // workspace switches: keep the DB-backed entries (refreshed by the next
    // hydration), re-derive the live fallbacks from the current session, and
    // drop stale fallbacks — replace semantics, never append-only (appending
    // duplicated workspaces whose live prefix didn't match their DB slug).
    let prev = std::mem::take(&mut model.sidebar_workspaces);
    model.sidebar_workspaces =
        crate::hydrate::merge_workspace_lists(prev, crate::hydrate::workspace_list(session, None));
    // Overlay the incoming workspace's last-known-good git glyphs from the
    // persistent cache so dirty-dots / ahead-behind arrows persist instantly
    // across a switch instead of blanking until the async hydration lands (the
    // stale map still holds the outgoing workspace's paths). In-memory only.
    crate::glyph_refresh::seed_from_global_cache(
        &mut model.sidebar_status.git,
        session
            .worktrees
            .iter()
            .filter(|g| !g.path.is_empty())
            .map(|g| g.path.clone()),
    );
    sb.rebuild(model, session);
}

/// Patch the active highlight onto the existing sidebar rows — the
/// zero-allocation replacement for a full `build_rows` when only the active
/// pointer moved. Mirrors exactly what a rebuild would change:
///
/// - every row with a live `RowTarget::Tab(gi, _)` target is active iff
///   `gi == session.active` (worktree rows compare group index; terminal rows'
///   name-match in `build_rows` is equivalent because group names are unique);
/// - a Worktree row's `Tab(_, ti)` tracks its group's `active_tab` chip
///   (Terminal rows pin tab 0, as `build_rows` does);
///
/// then replays `SidebarState::rebuild`'s tail: cursor-follows-active while
/// the sidebar is unfocused, clamp, and mirror into the model.
pub(crate) fn retarget_active(sb: &mut SidebarState, model: &mut FrameModel, session: &Session) {
    for row in &mut model.sidebar_rows {
        if let Some(crate::sidebar::RowTarget::Tab(gi, ref mut ti)) = row.tab_target {
            row.active = gi == session.active;
            if row.kind == crate::sidebar::RowKind::Worktree
                && let Some(g) = session.worktrees.get(gi)
            {
                *ti = g.active_tab;
            }
        }
    }
    if !sb.focused {
        sb.cursor = visible_index_of_active(model);
    }
    let visible = SidebarState::visible_len(model);
    if visible == 0 {
        sb.cursor = 0;
    } else if sb.cursor >= visible {
        sb.cursor = visible - 1;
    }
    sb.sync(model);
}

/// `Action::NextTab` / `Action::PrevTab`: rotate the active group's tab and run
/// the light refresh + drawer/persist tail. The caller stamps `switch_at`,
/// lands focus on the center zone, and sets `need_relayout`.
#[allow(clippy::too_many_arguments)]
pub(crate) fn cycle_tab(
    next: bool,
    session: &mut Session,
    model: &mut FrameModel,
    sb: &mut SidebarState,
    panes: &mut Panes,
    drawer: &mut Option<u32>,
    pool: &mut DrawerPool,
    home: &mut Option<std::path::PathBuf>,
    cfg: &thegn_core::config::Config,
    center: Rect,
) {
    if next {
        session.next_tab();
    } else {
        session.prev_tab();
    }
    finish_switch(session, model, sb, panes, drawer, pool, home, cfg, center);
}

/// `Action::NextWorktree` / `Action::PrevWorktree` (non-terminal arm): step to
/// the neighboring worktree in the sidebar's DISPLAY order, confined to the
/// active worktree's workspace so wrapping never crosses into another
/// workspace. A single-worktree workspace (or an active group filtered away)
/// is a no-op — never a fallback to session order. Returns the new active
/// group index (the caller's `region_last_w` bookmark).
#[allow(clippy::too_many_arguments)]
pub(crate) fn cycle_worktree(
    next: bool,
    session: &mut Session,
    model: &mut FrameModel,
    sb: &mut SidebarState,
    panes: &mut Panes,
    drawer: &mut Option<u32>,
    pool: &mut DrawerPool,
    home: &mut Option<std::path::PathBuf>,
    cfg: &thegn_core::config::Config,
    center: Rect,
) -> usize {
    let active_slug = session
        .worktrees
        .get(session.active)
        .and_then(|g| crate::sidebar::split_tab(&g.name).map(|(s, _)| s));
    let order: Vec<usize> = sidebar_worktree_order(model)
        .into_iter()
        .filter(|&g| {
            session
                .worktrees
                .get(g)
                .and_then(|w| crate::sidebar::split_tab(&w.name).map(|(s, _)| s))
                == active_slug
        })
        .collect();
    let pos = order.iter().position(|&g| g == session.active);
    if let (n, Some(p)) = (order.len(), pos)
        && n > 1
    {
        let step = if next { (p + 1) % n } else { (p + n - 1) % n };
        session.switch_to(order[step]);
    }
    finish_switch(session, model, sb, panes, drawer, pool, home, cfg, center);
    session.active
}

/// The shared switch tail: light model refresh, drawer sync, and the cheap
/// off-loop active-pointer persist.
#[allow(clippy::too_many_arguments)]
fn finish_switch(
    session: &mut Session,
    model: &mut FrameModel,
    sb: &mut SidebarState,
    panes: &mut Panes,
    drawer: &mut Option<u32>,
    pool: &mut DrawerPool,
    home: &mut Option<std::path::PathBuf>,
    cfg: &thegn_core::config::Config,
    center: Rect,
) {
    refresh_tab_model_switch(model, session, sb);
    sync_drawer_persistence(session, panes, drawer, pool, home, cfg, center);
    persist_active_focus(session);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::{GroupKind, WorktreeGroup};

    fn session_two_workspaces() -> Session {
        Session {
            id: "/tmp/app".into(),
            worktrees: vec![
                WorktreeGroup::new("app/home", GroupKind::Home, "/tmp/app"),
                WorktreeGroup::new("app/feat", GroupKind::Branch, "/tmp/app-feat"),
                WorktreeGroup::new("app/fix", GroupKind::Branch, "/tmp/app-fix"),
                WorktreeGroup::new("lib/home", GroupKind::Home, "/tmp/lib"),
            ],
            active: 0,
        }
    }

    /// Baseline (model, sb) pair whose rows were built by the FULL path for
    /// `session`, under `sort` (+ optional attention ranks).
    fn built(
        session: &Session,
        sort: crate::sidebar::SortMode,
        ranks: &[(&str, u32)],
    ) -> (FrameModel, SidebarState) {
        let mut model = crate::hydrate::build_initial_model(session, None);
        let mut sb = SidebarState::default();
        sb.view.sort = sort;
        for (p, r) in ranks {
            model
                .sidebar_status
                .attention_ranks
                .insert((*p).to_string(), *r);
        }
        crate::run::refresh_tab_model(&mut model, session, &mut sb);
        (model, sb)
    }

    fn rows_debug(model: &FrameModel) -> Vec<String> {
        model
            .sidebar_rows
            .iter()
            .map(|r| format!("{r:?}"))
            .collect()
    }

    /// The locked invariant: for a pure active-pointer move, the light patch
    /// produces row-for-row exactly what a full rebuild would — across every
    /// sort mode, so a future order-depends-on-active sort can't silently
    /// invalidate the fast path.
    #[test]
    fn retarget_matches_full_rebuild_for_pure_pointer_move() {
        for sort in [
            crate::sidebar::SortMode::Manual,
            crate::sidebar::SortMode::Name,
            crate::sidebar::SortMode::Recent,
            crate::sidebar::SortMode::Attention,
        ] {
            let ranks = [
                ("/tmp/app-fix", 0u32),
                ("/tmp/app", 1),
                ("/tmp/app-feat", 2),
            ];
            let mut session = session_two_workspaces();
            let (mut light_model, mut light_sb) = built(&session, sort, &ranks);
            let (mut full_model, mut full_sb) = built(&session, sort, &ranks);

            // Move the pointer (worktree switch), then patch vs rebuild.
            session.switch_to(2);
            refresh_tab_model_switch(&mut light_model, &session, &mut light_sb);
            crate::run::refresh_tab_model(&mut full_model, &session, &mut full_sb);

            assert_eq!(
                rows_debug(&light_model),
                rows_debug(&full_model),
                "patch ≡ rebuild violated under {sort:?}"
            );
            assert_eq!(light_sb.cursor, full_sb.cursor, "cursor under {sort:?}");
            assert_eq!(light_model.worktree, full_model.worktree);
            assert_eq!(light_model.tabs, full_model.tabs);
            assert_eq!(light_model.active_tab, full_model.active_tab);
        }
    }

    /// Tab switches within a group must also patch ≡ rebuild (the Worktree
    /// row's `Tab(_, ti)` chip index tracks the group's `active_tab`).
    #[test]
    fn retarget_matches_full_rebuild_for_tab_move() {
        let mut session = session_two_workspaces();
        session.worktrees[0].tabs.push(crate::session::Tab {
            title: "second".into(),
            center: crate::center::CenterTree::Leaf(99),
            focused_pane: 99,
            pane_cwds: Default::default(),
            pane_cmds: Default::default(),
            pane_sessions: Default::default(),
            pane_scrollback: Default::default(),
        });
        let (mut light_model, mut light_sb) =
            built(&session, crate::sidebar::SortMode::Manual, &[]);
        let (mut full_model, mut full_sb) = built(&session, crate::sidebar::SortMode::Manual, &[]);

        session.next_tab();
        refresh_tab_model_switch(&mut light_model, &session, &mut light_sb);
        crate::run::refresh_tab_model(&mut full_model, &session, &mut full_sb);

        assert_eq!(rows_debug(&light_model), rows_debug(&full_model));
        assert_eq!(light_model.active_tab, full_model.active_tab);
        assert_eq!(light_model.tabs, full_model.tabs);
    }

    /// A focused sidebar keeps the user's cursor; an unfocused one follows the
    /// active row — same as rebuild's tail.
    #[test]
    fn retarget_cursor_follows_active_only_when_unfocused() {
        let mut session = session_two_workspaces();
        let (mut model, mut sb) = built(&session, crate::sidebar::SortMode::Manual, &[]);

        session.switch_to(2);
        sb.focused = true;
        sb.cursor = 0;
        retarget_active(&mut sb, &mut model, &session);
        assert_eq!(sb.cursor, 0, "focused sidebar keeps the user's cursor");

        sb.focused = false;
        retarget_active(&mut sb, &mut model, &session);
        assert_eq!(
            sb.cursor,
            visible_index_of_active(&model),
            "unfocused sidebar follows the active row"
        );
    }
}
