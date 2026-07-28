//! The startup-shell watchdog. While a tab's loading splash is up (it clears on
//! first PTY output), a shell alive past its deadline with nothing on screen is
//! treated as a hung login shell — usually one whose rc files hang/error in a
//! provisioned env — and swapped ONCE for a clean rc-free shell (the user's
//! config left untouched). A remote tab earns one deadline extension first: the
//! machine0 readiness gate can legitimately spend a full window bringing a
//! cold/parked VM back before the shell prompts, and that silent wait counts
//! against `pane_age`. Extracted from the event loop (`run.rs`) so the god-file
//! stays capped; the fire/extend policy is the pure `crate::loading` helpers.

use std::collections::{HashMap, HashSet};

use crate::loading::{
    active_watchdog_deadline, effective_watchdog_deadline, is_shell_wait, watchdog_deadline,
    watchdog_should_extend,
};

type TabKey = (usize, usize);
type LoadKey = (String, usize);

/// The event-loop locals the watchdog tick reads/mutates, borrowed for one call.
pub(crate) struct StartupWatchdogCtx<'a> {
    pub panes: &'a mut crate::panes::Panes,
    pub session: &'a mut crate::session::Session,
    pub model: &'a mut crate::chrome::FrameModel,
    pub cfg: &'a thegn_core::config::Config,
    pub center: crate::compositor::Rect,
    pub loading_state: &'a mut crate::loading::track::LoadingTracker,
    pub loading_remote: &'a mut HashMap<LoadKey, bool>,
    pub shell_watchdog_fired: &'a mut HashSet<TabKey>,
    pub shell_watchdog_extended: &'a mut HashSet<TabKey>,
    pub center_dormant: &'a mut bool,
    pub need_relayout: &'a mut bool,
    pub dirty: &'a mut bool,
}

/// One watchdog check for the active tab. Arms ONLY in the shell-attach wait (the
/// final `shell` step, after provisioning finished) — never during clone / nix /
/// devShell / cold-resume, where silence is expected. Fires at most once per tab.
pub(crate) fn tick(ctx: &mut StartupWatchdogCtx<'_>) {
    if *ctx.center_dormant || !is_shell_wait(&ctx.model.load_steps) {
        return;
    }
    let gi = ctx.session.active;
    let ti = ctx
        .session
        .worktrees
        .get(gi)
        .map(|g| g.active_tab)
        .unwrap_or(0);
    // Bind the tab name first so the remoteness/deadline reads don't nest a
    // `session` borrow inside a `loading_remote` one.
    let name = ctx.session.worktrees.get(gi).map(|g| g.name.clone());
    // A provider/remote worktree gets the long deadline (a silent devShell build
    // / cold-VM resume is not a hang); local keeps the snappy default. Remoteness
    // is the PER-TAB `loading_remote` bool; missing ⇒ the safe long window (never
    // premature-drop an unknown/slow pane). See `active_watchdog_deadline`.
    let remote = name
        .as_ref()
        .map(|n| {
            ctx.loading_remote
                .get(&(n.clone(), ti))
                .copied()
                .unwrap_or(true)
        })
        .unwrap_or(true);
    let base = name
        .as_ref()
        .map(|n| active_watchdog_deadline(ctx.loading_remote, &(n.clone(), ti)))
        .unwrap_or_else(|| watchdog_deadline(true));
    // A remote tab that already spent its one extension must clear a doubled
    // window before the fallback (see `shell_watchdog_extended`).
    let extended = ctx.shell_watchdog_extended.contains(&(gi, ti));
    let watchdog = effective_watchdog_deadline(base, remote, extended);
    // Candidate = the tab's sole leaf pane (compute off `session` first so its
    // borrow ends before the `panes` age check below).
    let candidate = (!ctx.shell_watchdog_fired.contains(&(gi, ti)))
        .then(|| ctx.session.worktrees.get(gi).and_then(|g| g.tabs.get(ti)))
        .flatten()
        .map(|t| t.center.pane_ids())
        .filter(|ids| ids.len() == 1)
        .and_then(|ids| ids.first().copied());
    let Some(pid) = candidate.filter(|pid| {
        ctx.panes.table.contains_key(pid)
            && ctx.panes.pane_age(*pid).is_some_and(|age| age > watchdog)
    }) else {
        return;
    };

    if watchdog_should_extend(remote, extended) {
        // First expiry on a remote tab: extend once instead of stripping the
        // user's shell for a rc-free one — a genuinely hung remote shell still
        // falls back on the doubled window.
        ctx.shell_watchdog_extended.insert((gi, ti));
        tracing::info!(
            target: "thegn::startup",
            pane = pid,
            secs = watchdog.as_secs(),
            "remote startup-shell watchdog expiry: extending the deadline once \
             before falling back (VM bring-up can run long)"
        );
        return;
    }

    ctx.shell_watchdog_fired.insert((gi, ti));
    tracing::warn!(
        target: "thegn::startup",
        pane = pid,
        secs = watchdog.as_secs(),
        "startup-shell watchdog fired: no PTY output within deadline — falling \
         back to a clean rc-free shell"
    );
    let cwd = crate::run::group_cwd(&ctx.session.worktrees[gi])
        .or_else(|| std::env::var("HOME").ok().map(std::path::PathBuf::from));
    let k = (ctx.session.worktrees[gi].name.clone(), ti);
    match crate::run::spawn_clean_shell_pane(ctx.panes, ctx.cfg, cwd.as_deref(), ctx.center) {
        Ok(fresh) => {
            // Drop the hung pane and swap the clean shell into its leaf.
            ctx.panes.table.remove(&pid);
            ctx.panes.forget_spawn_time(pid);
            if let Some(tab) = ctx.session.tab_mut(gi, ti) {
                crate::panes::replace_single_dead_center_pane(tab, pid, fresh);
            }
            ctx.loading_state.remove(&k);
            ctx.loading_remote.remove(&k);
            ctx.model.load_steps.clear();
            ctx.model.status = if remote {
                "Remote shell produced no output within the extended deadline — \
                 fell back to a plain shell. The VM may be slow to bring up, or \
                 your login shell's startup files hang/error in this environment; \
                 your config was left untouched."
                    .into()
            } else {
                "Shell produced no output — fell back to a plain shell. Your login \
                 shell's startup files likely hang or error in this environment \
                 (e.g. dotfiles referencing host-only paths); your config was left \
                 untouched."
                    .into()
            };
            *ctx.need_relayout = true;
        }
        Err(e) => {
            ctx.loading_state.remove(&k);
            ctx.loading_remote.remove(&k);
            ctx.model.load_steps.clear();
            *ctx.center_dormant = true;
            ctx.model.status =
                format!("Shell produced no output and the plain-shell fallback failed: {e}");
        }
    }
    *ctx.dirty = true;
}
