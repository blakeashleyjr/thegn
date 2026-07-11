//! Sidebar row activation: focus a live tab or switch workspace. Extracted
//! from `run.rs` (pinned by the file-size ratchet); shared by Enter/click on a
//! sidebar row, the Alt+↑/↓ ring, and the jump-to-attention action.

use crate::chrome::FrameModel;
use crate::compositor::Rect;
use crate::panes::Panes;
use crate::run::{
    DrawerPool, SidebarState, WorkspacePool, persist_active_focus, persist_session_layout,
    refresh_tab_model, switch_workspace, sync_drawer_persistence,
};

/// Activate a sidebar row target: focus a live `(group, tab)` in the session,
/// or switch to another workspace (landing on its named worktree group when
/// that group exists in the target's persisted layout).
#[allow(clippy::too_many_arguments)]
pub(crate) fn activate_row_target(
    target: crate::sidebar::RowTarget,
    session: &mut crate::session::Session,
    model: &mut FrameModel,
    sb: &mut SidebarState,
    panes: &mut Panes,
    drawer: &mut Option<u32>,
    pool: &mut DrawerPool,
    home: &mut Option<std::path::PathBuf>,
    workspace_pool: &mut WorkspacePool,
    cfg: &thegn_core::config::Config,
    center: Rect,
    need_relayout: &mut bool,
    clear_on_next_frame: &mut bool,
) -> bool {
    // Set when this activation switched to a different workspace, so the caller
    // can kick an immediate model hydration (the new workspace's worktree paths
    // aren't in `model.sidebar_status` yet — without this the git glyphs blank
    // out until the ~1s refresh ticker fires).
    let mut workspace_switched = false;
    // Set when this activation adds a *new* group to the session (the lazy
    // terminal-materialize arm below). Only then does the layout structurally
    // change and warrant the heavyweight `persist_session_layout`; a plain
    // tab/worktree activation is a pure focus move and persists cheaply.
    let mut structural = false;
    // Set on the pure-focus arms (only the active pointer moved): the model
    // refresh below can then take the light patch path instead of a full
    // sidebar rebuild — see `handlers::switch` for the contract.
    let mut pure_focus = false;
    match target {
        crate::sidebar::RowTarget::Tab(gi, ti) => {
            if gi >= session.worktrees.len() {
                return false;
            }
            session.switch_to_tab(gi, ti);
            pure_focus = true;
        }
        // A terminal row uses the sentinel repo_path "terminal" when its group
        // isn't resident in the session yet (a terminal declared in the global
        // `terminals` table — e.g. the auto-provisioned default — that this
        // session has never opened). Switch to the existing group if present,
        // else materialize a fresh Terminal group; its pane spawns lazily via
        // the materialize path, which resolves the connection by name.
        crate::sidebar::RowTarget::Workspace { repo_path, group } if repo_path == "terminal" => {
            let Some(name) = group else {
                return false;
            };
            // NOT `pure_focus`: this row carried a Workspace fallback target
            // (the group wasn't resident at the last rebuild), so the light
            // patch — which only retargets `RowTarget::Tab` rows — couldn't
            // move the active highlight onto it. Rebuild.
            if let Some(gi) = session.worktrees.iter().position(|w| w.name == name) {
                session.switch_to_tab(gi, 0);
            } else {
                let placeholder = panes.reserve_ids(1);
                session.worktrees.push(crate::session::WorktreeGroup {
                    name,
                    kind: crate::session::GroupKind::Terminal,
                    path: String::new(),
                    tabs: vec![crate::session::Tab {
                        title: "main".to_string(),
                        center: crate::center::CenterTree::Leaf(placeholder),
                        focused_pane: placeholder,
                        pane_cwds: Default::default(),
                        pane_cmds: Default::default(),
                        pane_sessions: Default::default(),
                        pane_scrollback: Default::default(),
                    }],
                    active_tab: 0,
                });
                session.active = session.worktrees.len() - 1;
                *need_relayout = true;
                structural = true;
            }
        }
        crate::sidebar::RowTarget::Workspace { repo_path, group } => {
            let Ok(db) = thegn_core::db::Db::open() else {
                return false;
            };
            // Park the outgoing workspace's panes (kept alive) and restore the
            // target's — no reaping, so an editor/server keeps running across
            // the switch. `switch_workspace` handles the id-aliasing that the
            // old reap guarded against by remapping cold-resurrected ids.
            if !switch_workspace(
                &repo_path,
                group.as_deref(),
                session,
                panes,
                workspace_pool,
                &db,
                need_relayout,
                clear_on_next_frame,
            ) {
                return false;
            }
            workspace_switched = true;
        }
    }
    // When activating a tab via the sidebar, focus the leftmost visible pane
    // if the tab has more than one pane open.
    if let Some(tab) = session.active_tab_mut() {
        let layout = tab.center.layout(center);
        if layout.len() > 1
            && let Some((id, _)) = layout.iter().min_by_key(|(_, r)| r.x)
        {
            tab.focused_pane = *id;
        }
    }
    if pure_focus {
        crate::handlers::switch::refresh_tab_model_switch(model, session, sb);
    } else {
        refresh_tab_model(model, session, sb);
    }
    sync_drawer_persistence(session, panes, drawer, pool, home, cfg, center);
    // Persist the new active worktree/tab so it survives a non-graceful exit.
    // Only a structural change (the terminal-materialize arm, which pushes a
    // new group) needs the full layout rewrite; the Workspace arm already
    // persisted its full layout inside `switch_workspace`, and the in-workspace
    // Tab arm is a pure focus move — both just need the cheap, off-loop
    // active-pointer write so sidebar activation never blocks a frame.
    if structural {
        persist_session_layout(session, panes);
    } else {
        persist_active_focus(session);
    }
    workspace_switched
}
