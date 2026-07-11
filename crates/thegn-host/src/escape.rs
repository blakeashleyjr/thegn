//! Esc-to-center chrome collapse.
//!
//! When the keyboard leaves a chrome zone (sidebar, panel, a bar, the drawer)
//! back to the center terminal, a single Esc should land you at work with the
//! chrome at rest: the right panel back at its default width and the bottom
//! file drawer closed. That behaviour is opt-in via `[panel] collapse_on_escape`
//! (default on) and lives here so the event loop only calls a one-liner.

use crate::drawer_state::{DrawerPool, hide_drawer_into_pool, set_flag};
use crate::run::active_cwd;

/// Close the focused bottom drawer: stash its pane into the pool and persist the
/// "closed" flag so it stays down across restarts (falling back to a plain
/// remove when the cwd is unknown). Shared by the Esc/q drawer-dismiss path and
/// [`escape_to_center`].
pub(crate) fn close_drawer_to_pool(
    drawer: &mut Option<u32>,
    drawer_pool: &mut DrawerPool,
    drawer_home: &mut Option<std::path::PathBuf>,
    session: &crate::session::Session,
    panes: &mut crate::panes::Panes,
    cfg: &thegn_core::config::Config,
) {
    if let Some(cwd) = active_cwd(session) {
        hide_drawer_into_pool(drawer, drawer_pool, drawer_home, &cwd, cfg, panes);
        set_flag(&cwd, false);
    } else if let Some(id) = drawer.take() {
        panes.table.remove(&id);
    }
}

/// Hand keyboard focus back to the center terminal. When `[panel]
/// collapse_on_escape` is set (the default), also snap the right panel back to
/// its Normal width and close the bottom drawer. Returns `true` when the caller
/// must relayout (panel width shrank and/or the drawer closed).
#[must_use]
#[allow(clippy::too_many_arguments)]
pub(crate) fn escape_to_center(
    focus: &mut crate::focus::FocusState,
    panel_ui: &mut crate::panel::PanelUi,
    drawer: &mut Option<u32>,
    drawer_pool: &mut DrawerPool,
    drawer_home: &mut Option<std::path::PathBuf>,
    session: &crate::session::Session,
    panes: &mut crate::panes::Panes,
    cfg: &thegn_core::config::Config,
) -> bool {
    focus.zone = crate::focus::Zone::Center;
    if !cfg.panel.collapse_on_escape {
        return false;
    }
    let mut relayout = false;
    if panel_ui.width != crate::layout::PanelWidth::Normal {
        panel_ui.width = crate::layout::PanelWidth::Normal;
        relayout = true;
    }
    if drawer.is_some() {
        close_drawer_to_pool(drawer, drawer_pool, drawer_home, session, panes, cfg);
        relayout = true;
    }
    relayout
}
