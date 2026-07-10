//! Loop-side close handling: pane-close, tab-close, and the smart
//! `Action::Close` that folds them (the default `Alt x`). Extracted from
//! `run.rs` (file-size ratchet).
//!
//! Runs ON the loop and stays I/O-free — session/pane bookkeeping only. Layout
//! persistence goes through `persist_session_layout` (the DB cache, best-effort;
//! git is the source of truth).

use crate::run::{SidebarState, persist_session_layout, refresh_tab_model};

/// The loop-scope state the close helpers touch, borrowed for the call.
pub(crate) struct CloseCtx<'a> {
    pub session: &'a mut crate::session::Session,
    pub panes: &'a mut crate::panes::Panes,
    pub model: &'a mut crate::chrome::FrameModel,
    pub sb: &'a mut SidebarState,
    pub focus: &'a mut crate::focus::FocusState,
    pub need_relayout: &'a mut bool,
}

/// Close the currently focused split pane. If it's the only pane in the tab, do
/// nothing but hint toward tab-close — the last pane can't be closed without
/// closing the tab/worktree.
pub(crate) fn close_pane(cx: &mut CloseCtx<'_>) {
    let focused = cx.session.active_tab().map(|t| t.focused_pane).unwrap_or(0);
    let pane_count = cx
        .session
        .active_tab()
        .map(|t| t.center.pane_ids().len())
        .unwrap_or(0);
    if pane_count <= 1 {
        cx.model.status = "Only one pane — use Close tab to close the tab".into();
    } else {
        let removed = cx
            .session
            .active_tab_mut()
            .map(|t| t.center.remove(focused))
            .unwrap_or(false);
        if removed {
            cx.panes.table.remove(&focused);
            // Focus whatever is now the first pane.
            if let Some(tab) = cx.session.active_tab_mut()
                && let Some(first) = tab.center.pane_ids().first().copied()
            {
                tab.focused_pane = first;
            }
            *cx.need_relayout = true;
            persist_session_layout(cx.session, cx.panes);
        }
    }
    refresh_tab_model(cx.model, cx.session, cx.sb);
}

/// Close the active tab only. The final tab is the durable surface for its
/// worktree, so closing a worktree is a separate explicit action
/// (`CloseWorktree`) — `close_tab` never erases a home checkout or branch
/// worktree by accident. Returns `true` when the close was **blocked** (last
/// tab; a status was set), so the caller can re-render and skip the rest of the
/// loop iteration; `false` when a tab was actually closed.
pub(crate) fn close_tab(cx: &mut CloseCtx<'_>) -> bool {
    if cx
        .session
        .active_group()
        .map(|g| g.tabs.len() <= 1)
        .unwrap_or(false)
    {
        cx.model.status = cx
            .session
            .active_group()
            .map(|g| {
                if g.kind == crate::session::GroupKind::Home {
                    "Cannot close the last tab in the home worktree".to_string()
                } else {
                    format!(
                        "Cannot close the last tab in '{}'; use Close worktree to remove it",
                        g.name
                    )
                }
            })
            .unwrap_or_else(|| "No tab to close".into());
        return true;
    }
    match cx.session.close_active_tab() {
        crate::session::CloseResult::Tab(tab) => {
            for id in tab.center.pane_ids() {
                cx.panes.table.remove(&id);
            }
        }
        crate::session::CloseResult::Nothing => {}
    }
    persist_session_layout(cx.session, cx.panes);
    // Close always lands the user on the center terminal of whichever tab is
    // now active.
    cx.focus.zone = crate::focus::Zone::Center;
    refresh_tab_model(cx.model, cx.session, cx.sb);
    *cx.need_relayout = true;
    false
}

/// Smart "close this" (`Alt x`): close the focused pane when the active tab is
/// split, otherwise close the tab. Returns `true` when the (tab) close was
/// blocked (see [`close_tab`]).
pub(crate) fn close_smart(cx: &mut CloseCtx<'_>) -> bool {
    let pane_count = cx
        .session
        .active_tab()
        .map(|t| t.center.pane_ids().len())
        .unwrap_or(0);
    if pane_count > 1 {
        close_pane(cx);
        false
    } else {
        close_tab(cx)
    }
}
