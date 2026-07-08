//! Loop-side handling of worktree creation as a first-class tab: instead of a
//! modal progress overlay, the new worktree opens its own tab the moment the
//! worker settles the final name/path ([`crate::wizard::CreateEvent::TabOpened`]),
//! so the shared per-tab loading splash renders "where the terminal will be"
//! while the (sandbox ensure / register / launch-spec) tail finishes off-thread.
//! A sidebar loading dot marks the row. On success the pane attaches over the
//! splash; on failure the speculative tab is removed.

use std::collections::{HashMap, HashSet};

use crate::chrome::{FrameModel, LoadStep};
use crate::run::{SidebarState, refresh_tab_model};
use crate::session::{GroupKind, Session, WorktreeGroup};
use crate::wizard::CreationProgress;

/// A `loading_state`/`creating_tabs` key: `(group_name, tab_index)`.
type Key = (String, usize);
type LoadingState = HashMap<Key, Vec<LoadStep>>;

/// Open the just-created worktree's tab so its center renders the loading
/// splash. `focus` jumps to it (the default) — otherwise it is created in the
/// background (only the sidebar loading dot signals it). Idempotent on the tab
/// name. The tab is registered in `creating_tabs` so the materialize path
/// leaves it alone until the worker completes.
#[allow(clippy::too_many_arguments)]
pub(crate) fn open_tab(
    session: &mut Session,
    model: &mut FrameModel,
    sb: &mut SidebarState,
    loading_state: &mut LoadingState,
    creating_tabs: &mut HashSet<Key>,
    progress: Option<&CreationProgress>,
    tab: String,
    path: String,
    focus: bool,
) {
    if session.worktrees.iter().any(|g| g.name == tab) {
        return;
    }
    let prev = session.active;
    session.add_group(WorktreeGroup::new(tab.clone(), GroupKind::Branch, path));
    // `add_group` focuses the new group; the background mode undoes that so the
    // user stays put while only the sidebar dot marks the build.
    if !focus {
        session.switch_to(prev);
    }
    let key = (tab.clone(), 0);
    creating_tabs.insert(key.clone());
    if let Some(p) = progress {
        loading_state.insert(key, p.to_load_steps());
    }
    sb.creating.insert(tab);
    refresh_tab_model(model, session, sb);
}

/// Mirror the latest accumulated creation steps into the open tab's splash
/// (called on each `Step`/`Preflight`). Returns whether anything was updated —
/// a no-op until [`open_tab`] has run, so pre-`TabOpened` steps don't render.
pub(crate) fn sync_steps(
    progress: &CreationProgress,
    loading_state: &mut LoadingState,
    creating_tabs: &HashSet<Key>,
) -> bool {
    if creating_tabs.is_empty() {
        return false;
    }
    let steps = progress.to_load_steps();
    for key in creating_tabs {
        loading_state.insert(key.clone(), steps.clone());
    }
    true
}

/// On `Done`: ensure the tab exists (legacy path where it wasn't opened early),
/// retire the creation markers, and report whether the finished tab is the
/// active one — so the caller only pulls keyboard focus to it when the user is
/// actually looking at it (default jump-to-create, not navigated away).
pub(crate) fn adopt(
    session: &mut Session,
    sb: &mut SidebarState,
    loading_state: &mut LoadingState,
    creating_tabs: &mut HashSet<Key>,
    tab: &str,
    path: &str,
) -> bool {
    if !creating_tabs.contains(&(tab.to_string(), 0)) {
        session.add_group(WorktreeGroup::new(
            tab.to_string(),
            GroupKind::Branch,
            path.to_string(),
        ));
    }
    settle(sb, loading_state, creating_tabs, tab);
    session.active_group().is_some_and(|g| g.name == tab)
}

/// The worktree finished: retire the "creating" markers but keep the tab (the
/// caller attaches its pane, or lets the materialize path back a sprite).
pub(crate) fn settle(
    sb: &mut SidebarState,
    loading_state: &mut LoadingState,
    creating_tabs: &mut HashSet<Key>,
    tab: &str,
) {
    let key = (tab.to_string(), 0);
    creating_tabs.remove(&key);
    loading_state.remove(&key);
    sb.creating.remove(tab);
}

/// Creation failed (or was superseded): drop any speculative tab, its splash,
/// and its sidebar dot. The worker has already removed the git worktree. Safe
/// when no tab opened yet (a preflight/checkout failure) — `creating_tabs` is
/// empty and only the caller's status message is shown.
pub(crate) fn abort(
    session: &mut Session,
    model: &mut FrameModel,
    sb: &mut SidebarState,
    loading_state: &mut LoadingState,
    creating_tabs: &mut HashSet<Key>,
) {
    let keys: Vec<Key> = creating_tabs.drain().collect();
    for (name, _) in keys {
        loading_state.remove(&(name.clone(), 0));
        sb.creating.remove(&name);
        if let Some(i) = session.worktrees.iter().position(|g| g.name == name) {
            session.worktrees.remove(i);
            // Keep `active` pointing at the same group it did before the
            // removal shifted indices (land one earlier if it *was* the tab).
            if session.active > i || session.active >= session.worktrees.len() {
                session.active = session.active.saturating_sub(1);
            }
        }
    }
    refresh_tab_model(model, session, sb);
}
