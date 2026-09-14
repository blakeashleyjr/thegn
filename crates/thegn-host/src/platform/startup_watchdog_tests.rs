//! Watchdog fixtures, including an owned POSIX clean-shell spawn.

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
    // Fields drop in declaration order: owned panes terminate before their
    // worktree and state directory are removed.
    fixture: tempfile::TempDir,
}

impl Harness {
    /// One group, one tab, whose sole leaf is a live, byte-blank pane.
    fn new() -> Self {
        let fixture = tempfile::tempdir().unwrap();
        let worktree = fixture.path().join("worktree");
        std::fs::create_dir(&worktree).unwrap();
        let mut cfg = thegn_core::config::Config::default();
        cfg.sandbox.enabled = false;
        cfg.sandbox.backend = thegn_core::config::SandboxBackend::None;
        cfg.placement.enabled = false;
        cfg.daemon.enabled = false;
        cfg.env.clear();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            // Preserve the production clean-shell command while bypassing
            // the outer login shell's personal startup files.
            let shell = fixture.path().join("fixture-shell");
            std::fs::write(
                &shell,
                "#!/bin/sh\n[ \"$#\" -eq 2 ] && [ \"$1\" = -lc ] || exit 97\nexec /bin/sh -c \"$2\"\n",
            )
            .unwrap();
            std::fs::set_permissions(&shell, std::fs::Permissions::from_mode(0o700)).unwrap();
            std::fs::write(fixture.path().join("empty-shell-env"), "").unwrap();
        }
        let (tx, rx) = tokio::sync::mpsc::channel::<PaneEvent>(16);
        let (ctrl_tx, ctrl_rx) = tokio::sync::mpsc::channel::<ExecControl>(8);
        let mut session = Session {
            id: "s1".into(),
            worktrees: vec![WorktreeGroup::new(
                "app/home",
                GroupKind::Home,
                worktree.to_str().unwrap(),
            )],
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
            cfg,
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
            fixture,
        }
    }

    fn tick(&mut self) {
        // The real resolver may open the DB and inspect local config. Keep
        // every such path under the retained fixture, with the existing
        // crate-wide environment lock; never alter HOME.
        let state = self.fixture.path().join("state");
        let config = self.fixture.path().join("config");
        let runtime = self.fixture.path().join("runtime");
        let vars = vec![
            ("XDG_STATE_HOME", state.to_str().unwrap()),
            ("XDG_CONFIG_HOME", config.to_str().unwrap()),
            ("THEGN_DIR", runtime.to_str().unwrap()),
        ];
        #[cfg(unix)]
        let (shell, shell_env) = (
            self.fixture.path().join("fixture-shell"),
            self.fixture.path().join("empty-shell-env"),
        );
        #[cfg(unix)]
        let vars = {
            let mut vars = vars;
            vars.extend([
                ("SHELL", shell.to_str().unwrap()),
                ("ENV", shell_env.to_str().unwrap()),
                ("BASH_ENV", shell_env.to_str().unwrap()),
            ]);
            vars
        };
        let _env = crate::testenv::EnvVarGuard::set(&vars);
        assert_eq!(
            crate::run::group_cwd(&self.session.worktrees[0]).as_deref(),
            Some(self.fixture.path().join("worktree").as_path()),
            "the real spawn must never fall back to the ambient repository"
        );
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
#[cfg(unix)] // The real clean-shell script and owned shell adapter are POSIX.
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
    assert!(
        h.panes.table[&ids[0]].provider_session().is_none(),
        "the fixture spawns only an owned local PTY"
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

    // Terminate the owned PTY before deleting its cwd; cleanup failure is an
    // assertion failure, never a silently successful fixture with residue.
    h.panes.table.clear();
    h.fixture
        .close()
        .expect("remove the owned watchdog fixture");
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
