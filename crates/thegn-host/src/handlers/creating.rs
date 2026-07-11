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

/// Open the just-submitted worktree's tab optimistically, on the loop, using
/// the best-known name in `progress.branch` — so a slow (remote) host bring-up
/// doesn't leave the sidebar empty until the off-loop worker reaches
/// [`crate::wizard::CreateEvent::TabOpened`]. Name derivation is pure (no
/// git/DB on the loop): `repo_slug` is already resolved at wizard open, and
/// `branch_tab`/`worktree_path` are slugify+join. The worker's authoritative
/// `TabOpened` later reconciles the name via [`reconcile_name`].
#[allow(clippy::too_many_arguments)]
pub(crate) fn open_optimistic(
    session: &mut Session,
    model: &mut FrameModel,
    sb: &mut SidebarState,
    loading_state: &mut LoadingState,
    creating_tabs: &mut HashSet<Key>,
    progress: &CreationProgress,
    wizard: &crate::wizard::NewWorktreeWizard,
    cfg: &thegn_core::config::Config,
) {
    let tab = thegn_core::repo::branch_tab(&wizard.repo_slug, &progress.branch);
    let path = thegn_core::worktree::worktree_path(wizard.root(), &progress.branch, cfg)
        .to_string_lossy()
        .into_owned();
    let jump = cfg.session.focus_on_create;
    open_tab(
        session,
        model,
        sb,
        loading_state,
        creating_tabs,
        Some(progress),
        tab,
        path,
        jump,
    );
}

/// Reconcile an optimistically-opened placeholder group (opened on the loop at
/// Submit, before the worker had settled the name) with the worker's
/// authoritative tab name/path from [`crate::wizard::CreateEvent::TabOpened`].
///
/// The Submit path opens the tab under the best-known branch name; for a
/// human-typed name the worker may dedupe it (a `-N` suffix) or move the
/// worktree, landing on a different name/path. When that happens we rename the
/// single existing placeholder **in place** — group `name` + `path`, its
/// `creating_tabs`/`loading_state` key, and its `sb.creating` marker — rather
/// than opening a second group (which would reorder the sidebar and steal
/// focus). Returns `true` if it renamed (the caller then skips [`open_tab`]),
/// `false` when the name already matches or there is no single placeholder to
/// adopt (leaving [`open_tab`]'s own idempotency to handle it).
pub(crate) fn reconcile_name(
    session: &mut Session,
    sb: &mut SidebarState,
    loading_state: &mut LoadingState,
    creating_tabs: &mut HashSet<Key>,
    tab: &str,
    path: &str,
) -> bool {
    // Already the right name: open_tab's idempotency guard will handle it.
    if creating_tabs.contains(&(tab.to_string(), 0)) {
        return false;
    }
    // Only adopt when there is exactly one placeholder in flight — the one the
    // Submit path opened. Anything else is ambiguous; fall back to open_tab.
    if creating_tabs.len() != 1 {
        return false;
    }
    let (old_name, _) = creating_tabs.iter().next().cloned().expect("len == 1");
    let Some(g) = session.worktrees.iter_mut().find(|g| g.name == old_name) else {
        return false;
    };
    g.name = tab.to_string();
    g.path = path.to_string();
    // Move the creation markers onto the authoritative name.
    creating_tabs.remove(&(old_name.clone(), 0));
    creating_tabs.insert((tab.to_string(), 0));
    if let Some(steps) = loading_state.remove(&(old_name.clone(), 0)) {
        loading_state.insert((tab.to_string(), 0), steps);
    }
    sb.creating.remove(&old_name);
    sb.creating.insert(tab.to_string());
    true
}

/// Handle the worker's authoritative [`crate::wizard::CreateEvent::TabOpened`]:
/// reconcile the placeholder the Submit path opened (renaming it in place when
/// the worker settled on a different name), or open the tab fresh when there was
/// no placeholder (the worker-only path, e.g. fork/issue dispatch).
#[allow(clippy::too_many_arguments)]
pub(crate) fn open_or_reconcile(
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
    if reconcile_name(session, sb, loading_state, creating_tabs, &tab, &path) {
        refresh_tab_model(model, session, sb);
    } else {
        open_tab(
            session,
            model,
            sb,
            loading_state,
            creating_tabs,
            progress,
            tab,
            path,
            focus,
        );
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    /// A placeholder opened at Submit under `old`; the worker later settles on a
    /// deduped `new` name. `reconcile_name` renames it in place (one group, all
    /// markers moved) instead of opening a second group.
    #[test]
    fn reconcile_renames_placeholder_in_place() {
        let mut session = Session::default();
        session.add_group(WorktreeGroup::new(
            "repo/sz-feature".to_string(),
            GroupKind::Branch,
            "/wt/sz-feature".to_string(),
        ));
        let mut sb = SidebarState::default();
        sb.creating.insert("repo/sz-feature".to_string());
        let mut loading: LoadingState = HashMap::new();
        loading.insert(("repo/sz-feature".to_string(), 0), Vec::new());
        let mut creating_tabs: HashSet<Key> = HashSet::new();
        creating_tabs.insert(("repo/sz-feature".to_string(), 0));

        let renamed = reconcile_name(
            &mut session,
            &mut sb,
            &mut loading,
            &mut creating_tabs,
            "repo/sz-feature-1",
            "/wt/sz-feature-1",
        );

        assert!(renamed, "a name mismatch must reconcile");
        assert_eq!(session.worktrees.len(), 1, "no duplicate group");
        assert_eq!(session.worktrees[0].name, "repo/sz-feature-1");
        assert_eq!(session.worktrees[0].path, "/wt/sz-feature-1");
        assert!(creating_tabs.contains(&("repo/sz-feature-1".to_string(), 0)));
        assert!(!creating_tabs.contains(&("repo/sz-feature".to_string(), 0)));
        assert!(loading.contains_key(&("repo/sz-feature-1".to_string(), 0)));
        assert!(sb.creating.contains("repo/sz-feature-1"));
        assert!(!sb.creating.contains("repo/sz-feature"));
    }

    /// When the worker's name already matches the placeholder, reconcile is a
    /// no-op and defers to `open_tab`'s own idempotency.
    #[test]
    fn reconcile_noop_when_name_matches() {
        let mut session = Session::default();
        session.add_group(WorktreeGroup::new(
            "repo/sz-feature".to_string(),
            GroupKind::Branch,
            "/wt/sz-feature".to_string(),
        ));
        let mut sb = SidebarState::default();
        sb.creating.insert("repo/sz-feature".to_string());
        let mut loading: LoadingState = HashMap::new();
        let mut creating_tabs: HashSet<Key> = HashSet::new();
        creating_tabs.insert(("repo/sz-feature".to_string(), 0));

        let renamed = reconcile_name(
            &mut session,
            &mut sb,
            &mut loading,
            &mut creating_tabs,
            "repo/sz-feature",
            "/wt/sz-feature",
        );
        assert!(!renamed, "matching name reconciles to a no-op");
        assert_eq!(session.worktrees.len(), 1);
    }

    /// With no placeholder in flight (worker-only path, no optimistic open),
    /// reconcile is a no-op so the caller falls through to `open_tab`.
    #[test]
    fn reconcile_noop_without_placeholder() {
        let mut session = Session::default();
        let mut sb = SidebarState::default();
        let mut loading: LoadingState = HashMap::new();
        let mut creating_tabs: HashSet<Key> = HashSet::new();

        let renamed = reconcile_name(
            &mut session,
            &mut sb,
            &mut loading,
            &mut creating_tabs,
            "repo/sz-feature",
            "/wt/sz-feature",
        );
        assert!(!renamed);
        assert!(session.worktrees.is_empty());
    }
}
