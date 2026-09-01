//! Esc-to-center chrome collapse.
//!
//! When the keyboard leaves a chrome zone (sidebar, panel, a bar, the drawer)
//! back to the center terminal, a single Esc should land you at work with the
//! chrome at rest: the right panel back at its default width and the bottom
//! file drawer closed. That behaviour is opt-in via `[panel] collapse_on_escape`
//! (default on) and lives here so the event loop only calls a one-liner.

use crate::drawer_state::DrawerRuntime;
use crate::run::active_cwd;

/// Close the focused bottom drawer: stash its pane into the pool and persist the
/// "closed" flag so it stays down across restarts (falling back to a plain
/// remove when the cwd is unknown). Shared by the Esc/q drawer-dismiss path and
/// [`escape_to_center`].
pub(crate) fn close_drawer_to_pool(
    drawer_runtime: &mut DrawerRuntime,
    session: &crate::session::Session,
    panes: &mut crate::panes::Panes,
    cfg: &thegn_core::config::Config,
    center: crate::compositor::Rect,
) {
    if let Some(cwd) = active_cwd(session) {
        drawer_runtime.close_visible(cfg, &cwd, panes, center);
    } else if let Some(visible) = drawer_runtime.visible.take() {
        panes.table.remove(&visible.pane_id);
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
    drawer_runtime: &mut DrawerRuntime,
    session: &crate::session::Session,
    panes: &mut crate::panes::Panes,
    cfg: &thegn_core::config::Config,
    center: crate::compositor::Rect,
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
    if drawer_runtime.visible.is_some() {
        close_drawer_to_pool(drawer_runtime, session, panes, cfg, center);
        relayout = true;
    }
    relayout
}
