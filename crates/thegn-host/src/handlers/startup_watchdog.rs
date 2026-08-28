//! The startup-shell watchdog, in two candidate sets. The splash-armed one:
//! while a tab's loading splash is up (it clears on first PTY output), a shell
//! alive past its deadline with nothing on screen is treated as a hung login
//! shell — usually one whose rc files hang/error in a provisioned env — and
//! swapped ONCE for a clean rc-free shell (the user's config left untouched).
//! The degraded one (THE-84): a daemon pane whose session FELL BACK to a fresh
//! one (`SessionFallback` — warm reattach miss or reconnect-ladder reopen) is
//! exactly the same hung-login-shell shape, with no splash to arm the first
//! set, so its degrade moment arms the deadline instead. A remote tab earns one
//! deadline extension first in both sets: the machine0 readiness gate can
//! legitimately spend a full window bringing a cold/parked VM back before the
//! shell prompts. Extracted from the event loop (`run.rs`) so the god-file
//! stays capped; the fire/extend policy is the pure `crate::loading` helpers.

use std::collections::{HashMap, HashSet};

use crate::loading::{
    active_watchdog_deadline, effective_watchdog_deadline, is_shell_wait, watchdog_deadline,
    watchdog_should_extend,
};

type TabKey = (usize, usize);
type LoadKey = (String, usize);

/// Remote tabs reuse the splash fire's remote variant verbatim: a cold VM
/// bring-up is the likeliest reason a remote fresh shell sat silent through
/// even the extended window.
const REMOTE_DEGRADED_SWAP_STATUS: &str = "Remote shell produced no output within the extended \
deadline — fell back to a plain shell. The VM may be slow to bring up, or your login \
shell's startup files hang/error in this environment; your config was left untouched.";

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
    /// Degrade moments (`SessionFallback`) per pane — the second candidate set
    /// (see `tick_degraded`). Exited panes prune in the drain's exits loop;
    /// panes that printed anything drop their entry lazily
    /// (`pty_drain::prune_output_degraded`).
    pub degraded_at: &'a mut HashMap<u32, std::time::Instant>,
    pub center_dormant: &'a mut bool,
    pub need_relayout: &'a mut bool,
    pub dirty: &'a mut bool,
}

/// One watchdog check per loop iteration, over BOTH candidate sets: the
/// splash-armed tab (the final `shell` step of a provisioning splash, after
/// clone / nix / devShell finished — never during, where silence is expected)
/// and the degraded panes (`SessionFallback`), which have no splash to arm the
/// first set. Each fires at most once per pane/tab.
pub(crate) fn tick(ctx: &mut StartupWatchdogCtx<'_>) {
    if *ctx.center_dormant {
        return;
    }
    // The splash-armed set is scoped to the shell-wait splash shape; its early
    // returns must not starve the degraded set, which is why the body lives in
    // its own fn and this call is the `is_shell_wait` gate.
    if is_shell_wait(&ctx.model.load_steps) {
        tick_splash(ctx);
    }
    tick_degraded(ctx);
}

/// The splash-armed set (behavior unchanged): one check for the ACTIVE tab,
/// whose sole leaf pane is the candidate. Arms ONLY in the shell-attach wait.
fn tick_splash(ctx: &mut StartupWatchdogCtx<'_>) {
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
        // "Alive past its deadline with NOTHING ON SCREEN" is the whole premise
        // — so make the blank screen a precondition, not an assumption inherited
        // from `load_steps`. The splash state is keyed by tab INDEX, and a tab
        // close shifts those keys, so a surviving tab could inherit a shell-wait
        // splash it never asked for; without this check the watchdog would then
        // kill that tab's healthy, long-lived pane and hand the user a clean
        // rc-free shell in its place. A pane that has printed anything is by
        // definition not a hung login shell.
        ctx.panes
            .table
            .get(pid)
            .is_some_and(|p| p.history_tail(1).trim().is_empty())
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

/// The degraded set: for each pane whose session FELL BACK to a fresh one
/// (recorded in `degraded_at`), a byte-blank screen past the same watchdog
/// deadline — per the pane's TAB remoteness, extend-once for remote — means
/// the fresh shell hung the way the splash-armed set catches a hung login
/// shell. Swap it ONCE for a clean rc-free shell; a pane that printed
/// anything is not blank (the drain drops those entries lazily, and exits
/// prune their own).
fn tick_degraded(ctx: &mut StartupWatchdogCtx<'_>) {
    if ctx.degraded_at.is_empty() {
        return;
    }
    // Collect the due fires in one immutable pass (tab scan + deadline reads)
    // so the mutable fire below never overlaps them. A pane whose tab can't be
    // resolved (corner/drawer panes never degrade here, but be safe) is skipped.
    let mut due: Vec<(u32, usize, usize, std::time::Duration, bool)> = Vec::new();
    for (pid, t0) in ctx.degraded_at.iter() {
        // "Nothing ON SCREEN" is the same precondition the splash-armed set
        // uses: a pane that has printed anything is by definition not a hung
        // login shell.
        if !ctx
            .panes
            .table
            .get(pid)
            .is_some_and(|p| p.history_tail(1).trim().is_empty())
        {
            continue;
        }
        let Some((gi, ti)) = ctx
            .session
            .worktrees
            .iter()
            .enumerate()
            .find_map(|(gi, g)| {
                g.tabs
                    .iter()
                    .position(|t| t.center.pane_ids().contains(pid))
                    .map(|ti| (gi, ti))
            })
        else {
            continue;
        };
        // The pane's TAB remoteness drives the deadline (missing ⇒ the safe
        // long window) — identical policy to `active_watchdog_deadline`.
        let name = ctx.session.worktrees[gi].name.clone();
        let remote = ctx
            .loading_remote
            .get(&(name.clone(), ti))
            .copied()
            .unwrap_or(true);
        let base = active_watchdog_deadline(ctx.loading_remote, &(name, ti));
        let extended = ctx.shell_watchdog_extended.contains(&(gi, ti));
        let deadline = effective_watchdog_deadline(base, remote, extended);
        if t0.elapsed() > deadline {
            due.push((*pid, gi, ti, deadline, remote));
        }
    }
    for (pid, gi, ti, deadline, remote) in due {
        if watchdog_should_extend(remote, ctx.shell_watchdog_extended.contains(&(gi, ti))) {
            // First expiry on a remote tab: extend once instead of stripping
            // the user's shell (same policy as the splash-armed set). The
            // entry STAYS — the doubled window keeps guarding the pane.
            ctx.shell_watchdog_extended.insert((gi, ti));
            tracing::info!(
                target: "thegn::startup",
                pane = pid,
                secs = deadline.as_secs(),
                "degraded daemon session watchdog expiry on a remote tab: extending the \
                 deadline once before falling back (VM bring-up can run long)"
            );
            continue;
        }
        // Fire ONCE per pane: drop the entry first so no path below can refire
        // on the next tick.
        ctx.degraded_at.remove(&pid);
        // Sole leaf only — mirror the splash-armed set's single-pane
        // conservatism; a fan-out leaf's swap is out of scope. The entry is
        // already dropped, so this logs once and never refires.
        let sole = ctx.session.worktrees[gi]
            .tabs
            .get(ti)
            .is_some_and(|t| t.center.pane_ids().as_slice() == [pid]);
        if !sole {
            tracing::warn!(
                target: "thegn::startup",
                pane = pid,
                "degraded daemon session never produced output, but its pane is not its \
                 leaf's sole pane — no single-pane swap available"
            );
            continue;
        }
        let (session_id, program) = ctx
            .panes
            .table
            .get(&pid)
            .map(|p| {
                (
                    p.provider_session().map(|ps| ps.session),
                    p.program().to_string(),
                )
            })
            .unwrap_or((None, String::new()));
        tracing::warn!(
            target: "thegn::startup",
            pane = pid, ?session_id, program = %program, secs = deadline.as_secs(),
            "degraded daemon session produced no output within the deadline — swapping \
             in a clean rc-free shell"
        );
        let cwd = crate::run::group_cwd(&ctx.session.worktrees[gi])
            .or_else(|| std::env::var("HOME").ok().map(std::path::PathBuf::from));
        let k = (ctx.session.worktrees[gi].name.clone(), ti);
        match crate::run::spawn_clean_shell_pane(ctx.panes, ctx.cfg, cwd.as_deref(), ctx.center) {
            Ok(fresh) => {
                // Drop the degraded pane and swap the clean shell into its leaf.
                ctx.panes.table.remove(&pid);
                ctx.panes.forget_spawn_time(pid);
                if let Some(tab) = ctx.session.tab_mut(gi, ti) {
                    crate::panes::replace_single_dead_center_pane(tab, pid, fresh);
                }
                // The tab's shell story ended with the degrade — a lingering
                // shell-wait splash entry would re-arm the splash-armed set
                // against the FRESH pane. (`load_steps` re-derives next
                // iteration from `loading_state`.)
                ctx.loading_state.remove(&k);
                ctx.loading_remote.remove(&k);
                ctx.model.status = if remote {
                    REMOTE_DEGRADED_SWAP_STATUS.into()
                } else {
                    "Session died with the daemon and the fresh shell never produced output — \
                     swapped in a clean shell. `thegn doctor bundle` captures diagnostics."
                        .into()
                };
                *ctx.need_relayout = true;
            }
            Err(e) => {
                ctx.loading_state.remove(&k);
                ctx.loading_remote.remove(&k);
                *ctx.center_dormant = true;
                ctx.model.status = format!(
                    "Degraded session's fresh shell produced no output and the clean-shell \
                     fallback failed: {e}"
                );
            }
        }
        *ctx.dirty = true;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pane::PaneEvent;
    use crate::session::{GroupKind, Session, WorktreeGroup};
    use thegn_svc::provider::ExecControl;

    const PANE: u32 = 9;

    /// A stamp `secs` in the past. `Instant`'s epoch is unspecified (boot time
    /// on Linux), so a machine up for less than `secs` cannot represent this —
    /// the expect names the constraint instead of failing obscurely.
    fn stamp_ago(secs: u64) -> std::time::Instant {
        std::time::Instant::now()
            .checked_sub(std::time::Duration::from_secs(secs))
            .expect("test machine uptime must exceed the watchdog window")
    }

    struct Harness {
        panes: crate::panes::Panes,
        session: Session,
        model: crate::chrome::FrameModel,
        cfg: thegn_core::config::Config,
        loading_state: crate::loading::track::LoadingTracker,
        loading_remote: HashMap<LoadKey, bool>,
        fired: HashSet<TabKey>,
        extended: HashSet<TabKey>,
        degraded_at: HashMap<u32, std::time::Instant>,
        center_dormant: bool,
        dirty: bool,
        need_relayout: bool,
        _rx: tokio::sync::mpsc::Receiver<PaneEvent>,
        _ctrl_rx: tokio::sync::mpsc::Receiver<ExecControl>,
    }

    impl Harness {
        /// One group, one tab, whose sole leaf is a live, byte-blank pane.
        fn new() -> Self {
            let (tx, rx) = tokio::sync::mpsc::channel::<PaneEvent>(16);
            let (ctrl_tx, ctrl_rx) = tokio::sync::mpsc::channel::<ExecControl>(8);
            let mut session = Session {
                id: "s1".into(),
                worktrees: vec![WorktreeGroup::new("app/home", GroupKind::Home, "/tmp/app")],
                active: 0,
            };
            session.worktrees[0].tabs[0].center = crate::center::CenterTree::Leaf(PANE);
            let mut panes = crate::panes::Panes::new(tx);
            panes
                .table
                .insert(PANE, crate::pane::PtyPane::test_stream(ctrl_tx, 24, 80));
            Self {
                panes,
                session,
                model: crate::chrome::FrameModel::default(),
                cfg: thegn_core::config::Config::default(),
                loading_state: crate::loading::track::LoadingTracker::default(),
                loading_remote: HashMap::new(),
                fired: HashSet::new(),
                extended: HashSet::new(),
                degraded_at: HashMap::new(),
                center_dormant: false,
                dirty: false,
                need_relayout: false,
                _rx: rx,
                _ctrl_rx: ctrl_rx,
            }
        }

        fn tick(&mut self) {
            // The swap spawns a real clean shell; its spec resolution opens the
            // loop's DB, so isolate the state home like every other test that
            // reaches it (see `testenv`).
            let state = std::env::temp_dir().join(format!(
                "thegn-the84-watchdog-{}-{:?}",
                std::process::id(),
                std::thread::current().id()
            ));
            let _env = crate::testenv::EnvVarGuard::set(&[(
                "XDG_STATE_HOME",
                state.to_str().expect("utf-8 temp path"),
            )]);
            let mut dormant = self.center_dormant;
            let mut dirty = self.dirty;
            let mut need_relayout = self.need_relayout;
            {
                let mut ctx = StartupWatchdogCtx {
                    panes: &mut self.panes,
                    session: &mut self.session,
                    model: &mut self.model,
                    cfg: &self.cfg,
                    center: crate::compositor::Rect {
                        x: 0,
                        y: 0,
                        cols: 80,
                        rows: 24,
                    },
                    loading_state: &mut self.loading_state,
                    loading_remote: &mut self.loading_remote,
                    shell_watchdog_fired: &mut self.fired,
                    shell_watchdog_extended: &mut self.extended,
                    degraded_at: &mut self.degraded_at,
                    center_dormant: &mut dormant,
                    need_relayout: &mut need_relayout,
                    dirty: &mut dirty,
                };
                tick(&mut ctx);
            }
            self.center_dormant = dormant;
            self.dirty = dirty;
            self.need_relayout = need_relayout;
        }
    }

    /// A degraded pane, byte-blank, past the local deadline: the swap fires
    /// once (entry removed, leaf handed a fresh pane, status names the
    /// respawn, frame marked dirty + relayout) and a second tick is a no-op.
    #[test]
    fn degraded_blank_pane_past_the_local_deadline_swaps_once() {
        let mut h = Harness::new();
        h.loading_remote.insert(("app/home".into(), 0), false);
        h.degraded_at.insert(PANE, stamp_ago(9));

        h.tick();
        assert!(h.dirty && h.need_relayout, "the swap repaints");
        assert!(!h.degraded_at.contains_key(&PANE), "fired once: entry gone");
        let ids = h.session.worktrees[0].tabs[0].center.pane_ids();
        assert_eq!(ids.len(), 1, "the leaf is sole again");
        assert_ne!(ids[0], PANE, "the degraded pane left the leaf");
        assert!(
            h.panes.table.contains_key(&ids[0]),
            "the fresh clean shell is live"
        );
        assert!(!h.panes.table.contains_key(&PANE));
        assert!(
            h.model
                .status
                .contains("Session died with the daemon and the fresh shell never produced output"),
            "the status names the respawn: {:?}",
            h.model.status
        );

        // Once per pane: a second tick does nothing further.
        let (status, ids) = (h.model.status.clone(), ids.clone());
        h.dirty = false;
        h.need_relayout = false;
        h.tick();
        assert!(!h.dirty && !h.need_relayout, "no second fire");
        assert_eq!(h.model.status, status);
        assert_eq!(h.session.worktrees[0].tabs[0].center.pane_ids(), ids);
    }

    /// A degraded pane that produced output is not blank: no swap, and the
    /// drain's lazy sweep drops its entry.
    #[test]
    fn degraded_pane_that_produced_output_is_not_swapped_and_drops_its_entry() {
        let mut h = Harness::new();
        h.loading_remote.insert(("app/home".into(), 0), false);
        h.degraded_at.insert(PANE, stamp_ago(9));
        // A completed history line — a bare `feed` without a newline only
        // fills `history_partial`, which `history_tail` does not read.
        h.panes.table.get_mut(&PANE).unwrap().feed(b"prompt$ \n");

        h.tick();
        let ids = h.session.worktrees[0].tabs[0].center.pane_ids();
        assert_eq!(ids.as_slice(), [PANE], "no swap: the pane is not blank");
        assert_eq!(h.model.status, "", "no status change");
        assert!(
            h.degraded_at.contains_key(&PANE),
            "tick leaves the entry; the drain owns the lazy drop"
        );

        crate::pty_drain::prune_output_degraded(&mut h.degraded_at, &h.panes);
        assert!(
            !h.degraded_at.contains_key(&PANE),
            "output dropped the entry"
        );
    }

    /// A degraded REMOTE tab earns the one-time extension before any swap: the
    /// first expiry only latches the extension (entry kept, no swap), and the
    /// doubled window keeps holding the same stamp (no fire).
    #[test]
    fn degraded_remote_tab_extends_once_before_any_swap() {
        let mut h = Harness::new();
        h.loading_remote.insert(("app/home".into(), 0), true);
        let stamp = stamp_ago(310);
        h.degraded_at.insert(PANE, stamp);

        h.tick();
        assert!(
            h.extended.contains(&(0, 0)),
            "the extension latched for the tab"
        );
        assert!(
            h.degraded_at.contains_key(&PANE),
            "extension, not a fire: the entry stays"
        );
        let ids = h.session.worktrees[0].tabs[0].center.pane_ids();
        assert_eq!(ids.as_slice(), [PANE], "no swap yet");
        assert_eq!(h.model.status, "");

        // Same stamp, but the deadline is now the DOUBLED remote window.
        h.degraded_at.insert(PANE, stamp);
        h.tick();
        assert!(
            h.degraded_at.contains_key(&PANE),
            "still inside the doubled window: no fire"
        );
        assert_eq!(h.session.worktrees[0].tabs[0].center.pane_ids(), ids);
    }

    /// A degraded pane in a tab whose remoteness is unknown gets the safe long
    /// (remote) window — same policy as `active_watchdog_deadline` — so a
    /// just-degraded pane inside the local window never fires.
    #[test]
    fn degraded_pane_with_unknown_remoteness_gets_the_safe_long_window() {
        let mut h = Harness::new();
        // 310s ago: past the LOCAL 8s window, inside the REMOTE 300s one.
        h.degraded_at.insert(PANE, stamp_ago(310));

        h.tick();
        assert!(h.degraded_at.contains_key(&PANE), "no premature fire");
        assert!(h.extended.contains(&(0, 0)), "the extension still latches");
        assert_eq!(h.session.worktrees[0].tabs[0].center.pane_ids(), [PANE]);
    }

    /// A healthy resumed session (degrade + output within the window) produces
    /// no splash, no status, no swap — and the drain's sweep clears the entry.
    #[test]
    fn healthy_resumed_session_is_untouched() {
        let mut h = Harness::new();
        h.loading_remote.insert(("app/home".into(), 0), false);
        h.degraded_at.insert(PANE, stamp_ago(1));

        h.tick();
        assert!(h.degraded_at.contains_key(&PANE));
        assert_eq!(h.session.worktrees[0].tabs[0].center.pane_ids(), [PANE]);
        assert_eq!(h.model.status, "");
        assert!(!h.dirty && !h.need_relayout);
    }
}
