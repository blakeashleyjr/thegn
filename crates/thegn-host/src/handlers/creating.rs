//! Loop-side handling of worktree creation as a first-class tab: instead of a
//! modal progress overlay, the new worktree opens its own tab the moment the
//! worker settles the final name/path ([`crate::wizard::CreateEvent::TabOpened`]),
//! so the shared per-tab loading splash renders "where the terminal will be"
//! while the (sandbox ensure / register / launch-spec) tail finishes off-thread.
//! A sidebar loading dot marks the row. On success the pane attaches over the
//! splash; on failure the speculative tab is removed.

use std::collections::{HashMap, HashSet};

use crate::chrome::FrameModel;
use crate::run::{SidebarState, refresh_tab_model};
use crate::session::{GroupKind, Session, WorktreeGroup};
use crate::wizard::CreationProgress;

/// A `loading_state`/`creating_tabs` key: `(group_name, tab_index)`.
type Key = (String, usize);
type LoadingState = crate::loading::track::LoadingTracker;
/// `generation -> the tab key its splash lives under`. Populated when a
/// creation's tab (or optimistic placeholder) opens, so later `Step`/abort
/// events route to the right tab even when several creations are in flight.
pub(crate) type GenTab = HashMap<u64, Key>;

/// All worktree creations that have not yet reached `Done`/`Failed`.
///
/// The per-tab splash state (`loading_state`/`creating_tabs`/`sb.creating`) is
/// already multi-instance keyed by tab, so it needs no change; `InFlight`
/// carries the remaining previously-single-valued state, now keyed by the
/// creation's generation. This is what makes creation concurrent: a slow remote
/// sandbox bring-up keeps its entry here while a fresh wizard opens with its own
/// generation, instead of a single `Option` gating everything.
#[derive(Default)]
pub(crate) struct InFlight {
    /// Per-creation progress rows, seeded at wizard open / preset start. Seeds
    /// the tab's splash the moment the tab opens and keeps accumulating the
    /// SandboxPrep/Register/... rows until `Done`/`Failed`/cancel removes it.
    pub progress: HashMap<u64, CreationProgress>,
    /// Generation of the modal wizard currently on screen (Cancel/Submit/
    /// PrepChosen target it). `None` whenever no wizard form is open — including
    /// after Submit, once the creation has committed to the background.
    pub wizard_gen: Option<u64>,
    /// `generation -> settled tab key`, populated when the tab (or its optimistic
    /// placeholder) opens.
    pub gen_tab: GenTab,
}

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
    gen_tab: &mut GenTab,
    progress: Option<&CreationProgress>,
    generation: u64,
    tab: String,
    path: String,
    focus: bool,
) {
    if session.worktrees.iter().any(|g| g.name == tab) {
        // The name is already taken. Only adopt it for this generation when the
        // existing group is ITSELF mid-creation (idempotent re-open, or a
        // same-name collision with another in-flight creation) — then later
        // Step/abort events route to it safely. If it is a settled LIVE tab,
        // adopting it would let reconcile_name hijack (rename/repoint) it or
        // abort_gen delete it — corrupting the user's open tab. In that case do
        // NOT map the generation: leave the placeholder unopened and let the
        // worker's authoritative TabOpened open a fresh tab under its deduped,
        // non-colliding name.
        if creating_tabs.contains(&(tab.clone(), 0)) {
            gen_tab.insert(generation, (tab, 0));
        }
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
    gen_tab.insert(generation, key.clone());
    if let Some(p) = progress {
        loading_state.set(key, p.to_load_steps());
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
    gen_tab: &mut GenTab,
    progress: &CreationProgress,
    generation: u64,
    wizard: &crate::wizard::NewWorktreeWizard,
    cfg: &thegn_core::config::Config,
    env: &str,
) {
    let tab = thegn_core::repo::branch_tab(&wizard.repo_slug, &progress.branch);
    let path = thegn_core::worktree::worktree_path(wizard.root(), &progress.branch, cfg)
        .to_string_lossy()
        .into_owned();
    let jump = cfg.session.focus_on_create;
    // Record the wizard's host pick against the optimistic key BEFORE the worker's
    // `TabOpened` (which re-asserts it in `open_or_reconcile`) — otherwise the
    // splash context resolves `effective_env == None` (the DB row isn't written
    // until the worker's Register step) and falls through the config chain to a
    // provider `[sandbox] default_env`, briefly showing e.g. `machine0` for a
    // local pick. `rename` carries this entry across any name-dedupe reconcile.
    let key = (tab.clone(), 0);
    open_tab(
        session,
        model,
        sb,
        loading_state,
        creating_tabs,
        gen_tab,
        Some(progress),
        generation,
        tab,
        path,
        jump,
    );
    loading_state.set_env(key, env);
}

/// Reconcile an optimistically-opened placeholder group (opened on the loop at
/// Submit, before the worker had settled the name) with the worker's
/// authoritative tab name/path from [`crate::wizard::CreateEvent::TabOpened`].
///
/// The Submit path opens the tab under the best-known branch name; for a
/// human-typed name the worker may dedupe it (a `-N` suffix) or move the
/// worktree, landing on a different name/path. This generation's placeholder is
/// found via `gen_tab` (so several creations can be in flight at once), and
/// renamed **in place** — group `name` + `path`, its `creating_tabs`/
/// `loading_state` key, its `sb.creating` marker, and its `gen_tab` entry —
/// rather than opening a second group (which would reorder the sidebar and steal
/// focus). Returns `true` when this generation owns a placeholder (renamed, or
/// already correct — the caller then skips [`open_tab`]), `false` when there is
/// none (worker-only fork/preset paths, leaving the caller to [`open_tab`]).
#[allow(clippy::too_many_arguments)]
pub(crate) fn reconcile_name(
    session: &mut Session,
    sb: &mut SidebarState,
    loading_state: &mut LoadingState,
    creating_tabs: &mut HashSet<Key>,
    gen_tab: &mut GenTab,
    generation: u64,
    tab: &str,
    path: &str,
) -> bool {
    // Find THIS generation's placeholder (opened optimistically at Submit).
    // No entry → this creation never opened a placeholder (worker-only paths
    // like fork/preset): defer to `open_tab`.
    let Some((old_name, _)) = gen_tab.get(&generation).cloned() else {
        return false;
    };
    // Guard: only reconcile a placeholder this creation OWNS. If the mapped name
    // is not in `creating_tabs`, `gen_tab` points at a settled LIVE tab (never
    // opened as a placeholder) — renaming/repointing it would hijack the user's
    // open tab. Drop the stale mapping and defer to a fresh `open_tab` (which
    // will use the worker's deduped, non-colliding name).
    if !creating_tabs.contains(&(old_name.clone(), 0)) {
        gen_tab.remove(&generation);
        return false;
    }
    if old_name == tab {
        // Placeholder already carries the authoritative name; just settle the
        // (authoritative) path over the optimistically-derived one.
        if let Some(g) = session.worktrees.iter_mut().find(|g| g.name == tab) {
            g.path = path.to_string();
        }
        return true;
    }
    let Some(g) = session.worktrees.iter_mut().find(|g| g.name == old_name) else {
        // Placeholder vanished (e.g. superseded); fall through to a fresh open.
        gen_tab.remove(&generation);
        return false;
    };
    g.name = tab.to_string();
    g.path = path.to_string();
    // Move the creation markers onto the authoritative name.
    let old_key = (old_name.clone(), 0);
    let new_key = (tab.to_string(), 0);
    creating_tabs.remove(&old_key);
    creating_tabs.insert(new_key.clone());
    loading_state.rename(&old_key, new_key.clone());
    sb.creating.remove(&old_name);
    sb.creating.insert(tab.to_string());
    gen_tab.insert(generation, new_key);
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
    gen_tab: &mut GenTab,
    progress: Option<&CreationProgress>,
    generation: u64,
    tab: String,
    path: String,
    env: String,
    focus: bool,
) {
    // The settled splash key is always `(tab, 0)` (both branches below), so
    // record the wizard's host choice against it — the splash context resolves
    // this pick instead of the (not-yet-written) DB env. See `set_env`.
    let key = (tab.clone(), 0);
    if reconcile_name(
        session,
        sb,
        loading_state,
        creating_tabs,
        gen_tab,
        generation,
        &tab,
        &path,
    ) {
        refresh_tab_model(model, session, sb);
    } else {
        open_tab(
            session,
            model,
            sb,
            loading_state,
            creating_tabs,
            gen_tab,
            progress,
            generation,
            tab,
            path,
            focus,
        );
    }
    loading_state.set_env(key, &env);
}

/// Mirror the latest accumulated creation steps into the open tab's splash
/// (called on each `Step`/`Preflight`). Returns whether anything was updated —
/// a no-op until [`open_tab`] has run, so pre-`TabOpened` steps don't render.
pub(crate) fn sync_steps(
    progress: &CreationProgress,
    loading_state: &mut LoadingState,
    generation: u64,
    gen_tab: &GenTab,
) -> bool {
    // No tab yet for this generation (pre-`TabOpened`): nothing to mirror. This
    // also keeps a Step for creation A from stomping creation B's splash.
    let Some(key) = gen_tab.get(&generation) else {
        return false;
    };
    loading_state.set(key.clone(), progress.to_load_steps());
    true
}

/// On `Done`: ensure the tab exists (legacy path where it wasn't opened early),
/// retire the creation markers, and report whether the finished tab is the
/// active one — so the caller only pulls keyboard focus to it when the user is
/// actually looking at it (default jump-to-create, not navigated away).
#[allow(clippy::too_many_arguments)]
pub(crate) fn adopt(
    session: &mut Session,
    sb: &mut SidebarState,
    loading_state: &mut LoadingState,
    creating_tabs: &mut HashSet<Key>,
    gen_tab: &mut GenTab,
    generation: u64,
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
    gen_tab.remove(&generation);
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

/// Creation for `generation` failed (or was cancelled): drop only THIS
/// creation's speculative tab, its splash, and its sidebar dot — concurrent
/// creations keep theirs. The worker has already removed the git worktree. Safe
/// when no tab opened yet (a preflight/checkout failure) — `gen_tab` has no
/// entry and only the caller's status message is shown.
pub(crate) fn abort_gen(
    session: &mut Session,
    model: &mut FrameModel,
    sb: &mut SidebarState,
    loading_state: &mut LoadingState,
    creating_tabs: &mut HashSet<Key>,
    gen_tab: &mut GenTab,
    generation: u64,
) {
    let Some((name, idx)) = gen_tab.remove(&generation) else {
        return;
    };
    let key = (name.clone(), idx);
    // Only tear down a group this creation actually OWNS. If the name is not in
    // `creating_tabs`, `gen_tab` was pointed at a settled live tab (or the
    // placeholder was already adopted/renamed away) — removing it would delete
    // the user's open tab. Retire only this generation's markers in that case.
    let owns = creating_tabs.remove(&key);
    loading_state.remove(&key);
    sb.creating.remove(&name);
    if !owns {
        refresh_tab_model(model, session, sb);
        return;
    }
    if let Some(i) = session.worktrees.iter().position(|g| g.name == name) {
        session.worktrees.remove(i);
        // Keep `active` pointing at the same group it did before the removal
        // shifted indices (land one earlier if it *was* the tab).
        if session.active > i || session.active >= session.worktrees.len() {
            session.active = session.active.saturating_sub(1);
        }
    }
    refresh_tab_model(model, session, sb);
}

/// `CreateEvent::Preflight`: adopt the collision-free name suggestion. Only the
/// on-screen wizard's own generation may rewrite the form's name field — a
/// background straggler must not. Returns whether a repaint is needed. Split out
/// of the event loop to keep the ratchet-pinned `run.rs` from growing.
pub(crate) fn on_preflight(
    inflight: &mut InFlight,
    wizard_ui: &mut Option<crate::wizard::NewWorktreeWizard>,
    generation: u64,
    suggested: &str,
) -> bool {
    if !inflight.progress.contains_key(&generation) {
        return false;
    }
    if inflight.wizard_gen == Some(generation)
        && let Some(w) = wizard_ui.as_mut()
    {
        w.apply_name_suggestion(suggested);
    }
    if let Some(cp) = inflight.progress.get_mut(&generation) {
        cp.branch = suggested.to_string();
    }
    true
}

/// `WizardOutcome::Submit`: forward the decision to the worker, optimistically
/// open the tab (so a slow remote bring-up doesn't leave the sidebar empty),
/// then close the form and release the modal — the creation is now committed to
/// the background and a new wizard can open immediately (concurrent creation).
/// Returns whether a relayout is needed (an optimistic tab was opened).
#[allow(clippy::too_many_arguments)]
pub(crate) fn on_submit(
    session: &mut Session,
    model: &mut FrameModel,
    sb: &mut SidebarState,
    loading_state: &mut LoadingState,
    creating_tabs: &mut HashSet<Key>,
    inflight: &mut InFlight,
    wizard_ui: &mut Option<crate::wizard::NewWorktreeWizard>,
    wizard_cmd_tx: &mut Option<std::sync::mpsc::Sender<crate::wizard::WizardCmd>>,
    choices: crate::wizard::WizardChoices,
    cfg: &thegn_core::config::Config,
) -> bool {
    let wizard_gen = inflight.wizard_gen;
    if let crate::wizard::NameChoice::Human(tail) = &choices.name
        && let Some(cp) = wizard_gen.and_then(|g| inflight.progress.get_mut(&g))
    {
        cp.branch = format!("{}{}", cfg.branch_prefix, tail);
    }
    // Capture the host pick before `choices` is moved into the Submit command, so
    // the optimistic splash below can reflect it immediately (see `open_optimistic`).
    let env = choices.env.clone();
    if let Some(tx) = wizard_cmd_tx.take() {
        let _ = tx.send(crate::wizard::WizardCmd::Submit(choices));
    }
    let mut relayout = false;
    if let (Some(w), Some(g)) = (wizard_ui.as_ref(), wizard_gen)
        && let Some(cp) = inflight.progress.get(&g)
    {
        open_optimistic(
            session,
            model,
            sb,
            loading_state,
            creating_tabs,
            &mut inflight.gen_tab,
            cp,
            g,
            w,
            cfg,
            &env,
        );
        relayout = true;
    }
    *wizard_ui = None;
    inflight.wizard_gen = None;
    relayout
}

/// `CreateEvent::TabOpened`: reconcile the placeholder the Submit path opened
/// (renaming it in place to the authoritative name), or open the tab fresh when
/// there was none (fork/preset paths). Returns whether it was a live creation —
/// the caller then relayouts + repaints.
#[allow(clippy::too_many_arguments)]
pub(crate) fn on_tab_opened(
    session: &mut Session,
    model: &mut FrameModel,
    sb: &mut SidebarState,
    loading_state: &mut LoadingState,
    creating_tabs: &mut HashSet<Key>,
    inflight: &mut InFlight,
    generation: u64,
    tab: String,
    path: String,
    env: String,
    focus: bool,
) -> bool {
    if !inflight.progress.contains_key(&generation) {
        return false;
    }
    open_or_reconcile(
        session,
        model,
        sb,
        loading_state,
        creating_tabs,
        &mut inflight.gen_tab,
        inflight.progress.get(&generation),
        generation,
        tab,
        path,
        env,
        focus,
    );
    true
}

/// `CreateEvent::Step`: accumulate the row and mirror this generation's tab
/// splash (never a sibling's). Returns whether a repaint is needed.
pub(crate) fn on_step(
    inflight: &mut InFlight,
    loading_state: &mut LoadingState,
    generation: u64,
    step: crate::wizard::CreateStep,
    state: crate::wizard::StepState,
    detail: Option<String>,
) -> bool {
    let Some(cp) = inflight.progress.get_mut(&generation) else {
        return false;
    };
    cp.apply(step, state, detail);
    sync_steps(cp, loading_state, generation, &inflight.gen_tab)
}

/// `CreateEvent::Failed`: drop only this creation's tab + progress, clearing the
/// modal wizard only if it owns this generation (a committed background failure
/// must not disturb a freshly-opened wizard). Returns whether it was a live
/// creation — the caller then surfaces the error + repaints.
#[allow(clippy::too_many_arguments)]
pub(crate) fn on_failed(
    session: &mut Session,
    model: &mut FrameModel,
    sb: &mut SidebarState,
    loading_state: &mut LoadingState,
    creating_tabs: &mut HashSet<Key>,
    inflight: &mut InFlight,
    wizard_ui: &mut Option<crate::wizard::NewWorktreeWizard>,
    wizard_cmd_tx: &mut Option<std::sync::mpsc::Sender<crate::wizard::WizardCmd>>,
    generation: u64,
) -> bool {
    if inflight.progress.remove(&generation).is_none() {
        return false;
    }
    if inflight.wizard_gen == Some(generation) {
        *wizard_ui = None;
        *wizard_cmd_tx = None;
        inflight.wizard_gen = None;
    }
    abort_gen(
        session,
        model,
        sb,
        loading_state,
        creating_tabs,
        &mut inflight.gen_tab,
        generation,
    );
    true
}

/// `CreateEvent::Halted`: a RECOVERABLE env bring-up failure. Unlike [`on_failed`],
/// the git worktree + its registry row survive — so retire this creation's
/// progress + "creating" markers but KEEP the tab, and focus it so the ask/halt
/// modal the caller raises (and its retry / run-on-host choices, which act on the
/// active worktree) target it. Returns the kept tab's `(group, tab)` key so the
/// caller can mark it needs-bring-up (`materialize_failed`), or `None` when this
/// generation had no live tab (nothing to keep).
#[allow(clippy::too_many_arguments)]
pub(crate) fn on_halted(
    session: &mut Session,
    model: &mut FrameModel,
    sb: &mut SidebarState,
    loading_state: &mut LoadingState,
    creating_tabs: &mut HashSet<Key>,
    inflight: &mut InFlight,
    wizard_ui: &mut Option<crate::wizard::NewWorktreeWizard>,
    wizard_cmd_tx: &mut Option<std::sync::mpsc::Sender<crate::wizard::WizardCmd>>,
    generation: u64,
) -> Option<Key> {
    inflight.progress.remove(&generation)?;
    if inflight.wizard_gen == Some(generation) {
        *wizard_ui = None;
        *wizard_cmd_tx = None;
        inflight.wizard_gen = None;
    }
    let (name, idx) = inflight.gen_tab.remove(&generation)?;
    let key = (name.clone(), idx);
    // Retire the "creating" spinner/splash but leave the group + its tab standing.
    creating_tabs.remove(&key);
    loading_state.remove(&key);
    sb.creating.remove(&name);
    // Focus the kept worktree so the ask/halt modal + its retry/run-on-host act
    // on it (they key off the active worktree).
    if let Some(i) = session.worktrees.iter().position(|g| g.name == name) {
        session.active = i;
    }
    refresh_tab_model(model, session, sb);
    Some(key)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a placeholder group + its per-tab markers for `gen`, as
    /// `open_optimistic`/`open_tab` would.
    #[allow(clippy::too_many_arguments)]
    fn seed_placeholder(
        session: &mut Session,
        sb: &mut SidebarState,
        loading: &mut LoadingState,
        creating_tabs: &mut HashSet<Key>,
        gen_tab: &mut GenTab,
        generation: u64,
        name: &str,
        path: &str,
    ) {
        session.add_group(WorktreeGroup::new(
            name.to_string(),
            GroupKind::Branch,
            path.to_string(),
        ));
        sb.creating.insert(name.to_string());
        loading.set((name.to_string(), 0), Vec::new());
        creating_tabs.insert((name.to_string(), 0));
        gen_tab.insert(generation, (name.to_string(), 0));
    }

    /// A placeholder opened at Submit under `old`; the worker later settles on a
    /// deduped `new` name. `reconcile_name` renames it in place (one group, all
    /// markers moved) instead of opening a second group.
    #[test]
    fn reconcile_renames_placeholder_in_place() {
        let mut session = Session::default();
        let mut sb = SidebarState::default();
        let mut loading = LoadingState::default();
        let mut creating_tabs: HashSet<Key> = HashSet::new();
        let mut gen_tab: GenTab = HashMap::new();
        seed_placeholder(
            &mut session,
            &mut sb,
            &mut loading,
            &mut creating_tabs,
            &mut gen_tab,
            7,
            "repo/sz-feature",
            "/wt/sz-feature",
        );

        let renamed = reconcile_name(
            &mut session,
            &mut sb,
            &mut loading,
            &mut creating_tabs,
            &mut gen_tab,
            7,
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
        assert_eq!(
            gen_tab.get(&7),
            Some(&("repo/sz-feature-1".to_string(), 0)),
            "gen->tab mapping follows the rename"
        );
    }

    /// When the worker's name already matches the placeholder, reconcile still
    /// owns it (returns true) and settles the authoritative path.
    #[test]
    fn reconcile_settles_when_name_matches() {
        let mut session = Session::default();
        let mut sb = SidebarState::default();
        let mut loading = LoadingState::default();
        let mut creating_tabs: HashSet<Key> = HashSet::new();
        let mut gen_tab: GenTab = HashMap::new();
        seed_placeholder(
            &mut session,
            &mut sb,
            &mut loading,
            &mut creating_tabs,
            &mut gen_tab,
            3,
            "repo/sz-feature",
            "/wt/optimistic",
        );

        let handled = reconcile_name(
            &mut session,
            &mut sb,
            &mut loading,
            &mut creating_tabs,
            &mut gen_tab,
            3,
            "repo/sz-feature",
            "/wt/authoritative",
        );
        assert!(
            handled,
            "the placeholder for this gen is owned by reconcile"
        );
        assert_eq!(session.worktrees.len(), 1);
        assert_eq!(
            session.worktrees[0].path, "/wt/authoritative",
            "path settles to the worker's authoritative value"
        );
    }

    /// With no placeholder for this generation (worker-only path, no optimistic
    /// open), reconcile is a no-op so the caller falls through to `open_tab`.
    #[test]
    fn reconcile_noop_without_placeholder() {
        let mut session = Session::default();
        let mut sb = SidebarState::default();
        let mut loading = LoadingState::default();
        let mut creating_tabs: HashSet<Key> = HashSet::new();
        let mut gen_tab: GenTab = HashMap::new();

        let renamed = reconcile_name(
            &mut session,
            &mut sb,
            &mut loading,
            &mut creating_tabs,
            &mut gen_tab,
            9,
            "repo/sz-feature",
            "/wt/sz-feature",
        );
        assert!(!renamed);
        assert!(session.worktrees.is_empty());
    }

    /// Two placeholders in flight: reconcile adopts the one belonging to the
    /// given generation, not "the single placeholder" (which no longer exists).
    #[test]
    fn reconcile_adopts_by_generation_when_two_in_flight() {
        let mut session = Session::default();
        let mut sb = SidebarState::default();
        let mut loading = LoadingState::default();
        let mut creating_tabs: HashSet<Key> = HashSet::new();
        let mut gen_tab: GenTab = HashMap::new();
        seed_placeholder(
            &mut session,
            &mut sb,
            &mut loading,
            &mut creating_tabs,
            &mut gen_tab,
            1,
            "repo/a",
            "/wt/a",
        );
        seed_placeholder(
            &mut session,
            &mut sb,
            &mut loading,
            &mut creating_tabs,
            &mut gen_tab,
            2,
            "repo/b",
            "/wt/b",
        );

        let renamed = reconcile_name(
            &mut session,
            &mut sb,
            &mut loading,
            &mut creating_tabs,
            &mut gen_tab,
            2,
            "repo/b-1",
            "/wt/b-1",
        );
        assert!(renamed);
        assert_eq!(gen_tab.get(&2), Some(&("repo/b-1".to_string(), 0)));
        assert_eq!(
            gen_tab.get(&1),
            Some(&("repo/a".to_string(), 0)),
            "the other in-flight creation is untouched"
        );
        assert!(session.worktrees.iter().any(|g| g.name == "repo/a"));
        assert!(session.worktrees.iter().any(|g| g.name == "repo/b-1"));
    }

    /// `sync_steps` writes only the given generation's tab — a Step for
    /// creation A must not stomp creation B's splash.
    #[test]
    fn sync_steps_is_isolated_per_generation() {
        let mut loading = LoadingState::default();
        let mut gen_tab: GenTab = HashMap::new();
        gen_tab.insert(1, ("repo/a".to_string(), 0));
        gen_tab.insert(2, ("repo/b".to_string(), 0));
        loading.set(("repo/b".to_string(), 0), Vec::new());

        let progress = CreationProgress::new("repo/a".to_string());
        let wrote = sync_steps(&progress, &mut loading, 1, &gen_tab);
        assert!(wrote);
        assert!(
            loading.contains_key(&("repo/a".to_string(), 0)),
            "gen 1's splash updated"
        );
        assert!(
            loading.get(&("repo/b".to_string(), 0)).unwrap().is_empty(),
            "gen 2's splash untouched"
        );

        // No tab yet for a generation → nothing mirrored.
        assert!(!sync_steps(&progress, &mut loading, 99, &gen_tab));
    }

    /// An optimistic open whose derived name COLLIDES with a settled LIVE tab
    /// (not mid-creation) must not adopt it: `gen_tab` stays unmapped and the
    /// live group is left untouched, so the worker's deduped `TabOpened` opens a
    /// fresh tab instead of hijacking the user's open one.
    #[test]
    fn open_tab_does_not_adopt_a_live_colliding_tab() {
        let mut session = Session::default();
        let mut model = FrameModel::default();
        let mut sb = SidebarState::default();
        let mut loading = LoadingState::default();
        let mut creating_tabs: HashSet<Key> = HashSet::new();
        let mut gen_tab: GenTab = HashMap::new();
        // A LIVE tab already owns the name (NOT in creating_tabs).
        session.add_group(WorktreeGroup::new(
            "repo/tg-fix-login".to_string(),
            GroupKind::Branch,
            "/wt/live".to_string(),
        ));

        open_tab(
            &mut session,
            &mut model,
            &mut sb,
            &mut loading,
            &mut creating_tabs,
            &mut gen_tab,
            None,
            5,
            "repo/tg-fix-login".to_string(),
            "/wt/optimistic".to_string(),
            true,
        );

        assert!(
            !gen_tab.contains_key(&5),
            "the generation must not be mapped onto the live tab",
        );
        assert!(!creating_tabs.contains(&("repo/tg-fix-login".to_string(), 0)));
        assert_eq!(session.worktrees.len(), 1, "no duplicate group");
        assert_eq!(
            session.worktrees[0].path, "/wt/live",
            "the live tab's path is untouched",
        );
    }

    /// After the collision above, a `Failed`/abort for that generation must NOT
    /// delete the live tab (gen_tab has no entry → no-op).
    #[test]
    fn abort_gen_spares_a_live_colliding_tab() {
        let mut session = Session::default();
        let mut model = FrameModel::default();
        let mut sb = SidebarState::default();
        let mut loading = LoadingState::default();
        let mut creating_tabs: HashSet<Key> = HashSet::new();
        let mut gen_tab: GenTab = HashMap::new();
        session.add_group(WorktreeGroup::new(
            "repo/tg-fix-login".to_string(),
            GroupKind::Branch,
            "/wt/live".to_string(),
        ));
        // Collision path left gen_tab unmapped.
        open_tab(
            &mut session,
            &mut model,
            &mut sb,
            &mut loading,
            &mut creating_tabs,
            &mut gen_tab,
            None,
            5,
            "repo/tg-fix-login".to_string(),
            "/wt/optimistic".to_string(),
            true,
        );

        abort_gen(
            &mut session,
            &mut model,
            &mut sb,
            &mut loading,
            &mut creating_tabs,
            &mut gen_tab,
            5,
        );

        assert!(
            session
                .worktrees
                .iter()
                .any(|g| g.name == "repo/tg-fix-login"),
            "the user's live tab survives a colliding creation's failure",
        );
    }

    /// Defense-in-depth: if `gen_tab` somehow points at a name NOT in
    /// `creating_tabs` (a settled live tab), `abort_gen` retires its own markers
    /// but must NOT remove the group from the session.
    #[test]
    fn abort_gen_does_not_remove_a_non_creating_group() {
        let mut session = Session::default();
        let mut model = FrameModel::default();
        let mut sb = SidebarState::default();
        let mut loading = LoadingState::default();
        let mut creating_tabs: HashSet<Key> = HashSet::new();
        let mut gen_tab: GenTab = HashMap::new();
        session.add_group(WorktreeGroup::new(
            "repo/live".to_string(),
            GroupKind::Branch,
            "/wt/live".to_string(),
        ));
        // Stale mapping onto the live tab, with NO creating_tabs entry.
        gen_tab.insert(9, ("repo/live".to_string(), 0));

        abort_gen(
            &mut session,
            &mut model,
            &mut sb,
            &mut loading,
            &mut creating_tabs,
            &mut gen_tab,
            9,
        );

        assert!(
            session.worktrees.iter().any(|g| g.name == "repo/live"),
            "a non-creating group must never be torn down by abort_gen",
        );
        assert!(!gen_tab.contains_key(&9), "the stale mapping is cleared");
    }

    /// Defense-in-depth: `reconcile_name` refuses to rename a group whose name
    /// isn't in `creating_tabs` (a settled live tab), returning false so the
    /// caller opens a fresh tab instead of hijacking the live one.
    #[test]
    fn reconcile_refuses_a_non_creating_group() {
        let mut session = Session::default();
        let mut sb = SidebarState::default();
        let mut loading = LoadingState::default();
        let mut creating_tabs: HashSet<Key> = HashSet::new();
        let mut gen_tab: GenTab = HashMap::new();
        session.add_group(WorktreeGroup::new(
            "repo/live".to_string(),
            GroupKind::Branch,
            "/wt/live".to_string(),
        ));
        gen_tab.insert(4, ("repo/live".to_string(), 0));

        let handled = reconcile_name(
            &mut session,
            &mut sb,
            &mut loading,
            &mut creating_tabs,
            &mut gen_tab,
            4,
            "repo/tg-fix-login-1",
            "/wt/new",
        );

        assert!(!handled, "must not adopt a non-creating group");
        assert_eq!(
            session.worktrees[0].name, "repo/live",
            "the live tab is not renamed",
        );
        assert_eq!(session.worktrees[0].path, "/wt/live");
        assert!(!gen_tab.contains_key(&4), "the stale mapping is cleared");
    }

    /// `abort_gen` removes only the failed generation's tab, leaving a
    /// concurrent creation's group intact.
    #[test]
    fn abort_gen_removes_only_its_tab() {
        let mut session = Session::default();
        let mut model = FrameModel::default();
        let mut sb = SidebarState::default();
        let mut loading = LoadingState::default();
        let mut creating_tabs: HashSet<Key> = HashSet::new();
        let mut gen_tab: GenTab = HashMap::new();
        seed_placeholder(
            &mut session,
            &mut sb,
            &mut loading,
            &mut creating_tabs,
            &mut gen_tab,
            1,
            "repo/a",
            "/wt/a",
        );
        seed_placeholder(
            &mut session,
            &mut sb,
            &mut loading,
            &mut creating_tabs,
            &mut gen_tab,
            2,
            "repo/b",
            "/wt/b",
        );

        abort_gen(
            &mut session,
            &mut model,
            &mut sb,
            &mut loading,
            &mut creating_tabs,
            &mut gen_tab,
            1,
        );

        assert!(!session.worktrees.iter().any(|g| g.name == "repo/a"));
        assert!(
            session.worktrees.iter().any(|g| g.name == "repo/b"),
            "the concurrent creation survives the sibling's failure"
        );
        assert!(!gen_tab.contains_key(&1));
        assert_eq!(gen_tab.get(&2), Some(&("repo/b".to_string(), 0)));
        assert!(sb.creating.contains("repo/b"));
        assert!(!sb.creating.contains("repo/a"));

        // No tab for the generation (early preflight failure) → no-op.
        abort_gen(
            &mut session,
            &mut model,
            &mut sb,
            &mut loading,
            &mut creating_tabs,
            &mut gen_tab,
            42,
        );
        assert!(session.worktrees.iter().any(|g| g.name == "repo/b"));
    }

    /// The optimistic placeholder opened at Submit records the wizard's host pick
    /// against its `(tab, 0)` key, so the splash context resolves that env instead
    /// of falling through to `[sandbox] default_env` (a provider) while the DB
    /// `worktrees` row doesn't yet exist — the "splash briefly shows machine0 for a
    /// local pick" regression.
    #[test]
    fn open_optimistic_records_the_chosen_env() {
        let mut session = Session::default();
        let mut model = FrameModel::default();
        let mut sb = SidebarState::default();
        let mut loading = LoadingState::default();
        let mut creating_tabs: HashSet<Key> = HashSet::new();
        let mut gen_tab: GenTab = HashMap::new();

        // A config whose default env is a provider: without the recorded pick, the
        // splash would resolve THIS key to that provider.
        let mut cfg = thegn_core::config::Config::default();
        cfg.sandbox.default_env = "machine0".into();
        let root = std::env::temp_dir();
        let w = crate::wizard::NewWorktreeWizard::new(root.clone(), &cfg);
        let progress = CreationProgress::new("tg-lively-orbit".to_string());
        let key = (
            thegn_core::repo::branch_tab(&w.repo_slug, &progress.branch),
            0,
        );

        open_optimistic(
            &mut session,
            &mut model,
            &mut sb,
            &mut loading,
            &mut creating_tabs,
            &mut gen_tab,
            &progress,
            5,
            &w,
            &cfg,
            "host",
        );

        assert_eq!(
            loading.env_for(&key),
            Some("host"),
            "the wizard's local host pick is recorded on the optimistic splash key, \
             so the context never falls through to the provider default_env",
        );
    }

    /// A recoverable env halt during create KEEPS the worktree: `on_halted`
    /// retires the "creating" markers + progress but leaves the group standing,
    /// focuses it, and returns its key so the loop can mark it needs-bring-up.
    /// (Contrast `on_failed`, whose `abort_gen` deletes the group.)
    #[test]
    fn halted_keeps_worktree_and_returns_key() {
        let mut session = Session::default();
        let mut model = FrameModel::default();
        let mut sb = SidebarState::default();
        let mut loading = LoadingState::default();
        let mut creating_tabs: HashSet<Key> = HashSet::new();
        let mut inflight = InFlight::default();
        let mut wizard_ui = None;
        let mut wizard_cmd_tx = None;

        // A second, settled worktree so focus landing on the halted one is proven.
        session.add_group(WorktreeGroup::new(
            "repo/other".to_string(),
            GroupKind::Branch,
            "/wt/other".to_string(),
        ));
        seed_placeholder(
            &mut session,
            &mut sb,
            &mut loading,
            &mut creating_tabs,
            &mut inflight.gen_tab,
            9,
            "repo/sz-machine0",
            "/wt/sz-machine0",
        );
        inflight
            .progress
            .insert(9, CreationProgress::new("repo/sz-machine0".to_string()));

        let key = on_halted(
            &mut session,
            &mut model,
            &mut sb,
            &mut loading,
            &mut creating_tabs,
            &mut inflight,
            &mut wizard_ui,
            &mut wizard_cmd_tx,
            9,
        );

        assert_eq!(
            key,
            Some(("repo/sz-machine0".to_string(), 0)),
            "returns the kept tab's key"
        );
        assert!(
            session
                .worktrees
                .iter()
                .any(|g| g.name == "repo/sz-machine0"),
            "the halted worktree is KEPT, not deleted"
        );
        assert!(!creating_tabs.contains(&("repo/sz-machine0".to_string(), 0)));
        assert!(!loading.contains_key(&("repo/sz-machine0".to_string(), 0)));
        assert!(!sb.creating.contains("repo/sz-machine0"));
        assert!(inflight.progress.get(&9).is_none(), "progress retired");
        assert_eq!(
            session
                .worktrees
                .get(session.active)
                .map(|g| g.name.as_str()),
            Some("repo/sz-machine0"),
            "focus lands on the halted worktree so the ask/halt modal targets it",
        );
    }

    /// A stale straggler with no in-flight progress for the generation is a
    /// no-op — nothing to keep, nothing returned.
    #[test]
    fn halted_noop_without_progress() {
        let mut session = Session::default();
        let mut model = FrameModel::default();
        let mut sb = SidebarState::default();
        let mut loading = LoadingState::default();
        let mut creating_tabs: HashSet<Key> = HashSet::new();
        let mut inflight = InFlight::default();
        let mut wizard_ui = None;
        let mut wizard_cmd_tx = None;

        let key = on_halted(
            &mut session,
            &mut model,
            &mut sb,
            &mut loading,
            &mut creating_tabs,
            &mut inflight,
            &mut wizard_ui,
            &mut wizard_cmd_tx,
            42,
        );
        assert_eq!(key, None);
    }
}
