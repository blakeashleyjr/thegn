//! The PTY pane registry + spawn/layout helpers: `Panes` owns every live
//! `PtyPane` keyed by id, materializes a tab's `CenterTree` leaves on focus,
//! pre-warms neighbor tabs, replaces dead sole center panes, and resolves the
//! shell/tool argv new panes run.

use anyhow::Result;
use tokio::sync::mpsc as tokio_mpsc;

use termwiz::terminal::TerminalWaker;

use crate::compositor::Rect;
use crate::pane::{PaneEvent, PtyPane};

/// The shell argv used for new panes. Non-login interactive shells are the
/// default because login startup files are expensive and can trigger user
/// autostart logic inside the compositor. Set `SUPERZEJ_LOGIN_SHELL=1` to opt
/// back into login-shell semantics.
fn shell_argv_from(shell: &str, login: bool) -> Vec<String> {
    if login {
        return vec![shell.into(), "-l".into()];
    }
    let name = std::path::Path::new(shell)
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("");
    match name {
        // Common POSIX-ish shells support `-i` for interactive non-login mode.
        "bash" | "dash" | "fish" | "ksh" | "mksh" | "sh" | "zsh" => {
            vec![shell.into(), "-i".into()]
        }
        _ => vec![shell.into()],
    }
}

fn path_is_executable_file(path: &std::path::Path) -> bool {
    path.is_file()
}

fn command_on_path(name: &str) -> Option<String> {
    let path_var = std::env::var_os("PATH")?;
    std::env::split_paths(&path_var)
        .map(|dir| dir.join(name))
        .find(|candidate| path_is_executable_file(candidate))
        .map(|candidate| candidate.to_string_lossy().into_owned())
}

fn resolve_pane_shell(shell_env: Option<String>) -> String {
    if let Some(shell) = shell_env {
        let trimmed = shell.trim();
        if !trimmed.is_empty() && path_is_executable_file(std::path::Path::new(trimmed)) {
            return trimmed.to_string();
        }
    }

    for name in ["zsh", "bash", "fish", "sh"] {
        if let Some(shell) = command_on_path(name) {
            return shell;
        }
    }

    for shell in [
        "/etc/profiles/per-user/blake/bin/zsh",
        "/run/current-system/sw/bin/zsh",
        "/bin/zsh",
        "/run/current-system/sw/bin/bash",
        "/bin/bash",
        "/run/current-system/sw/bin/sh",
        "/bin/sh",
    ] {
        if path_is_executable_file(std::path::Path::new(shell)) {
            return shell.to_string();
        }
    }

    "/bin/sh".into()
}

fn pane_shell_argv() -> Vec<String> {
    let shell = resolve_pane_shell(std::env::var("SHELL").ok());
    let login = std::env::var("SUPERZEJ_LOGIN_SHELL")
        .map(|v| matches!(v.as_str(), "1" | "true" | "TRUE" | "yes" | "YES"))
        .unwrap_or(false);
    shell_argv_from(&shell, login)
}

pub(crate) fn tool_drawer_argv(command: &str) -> Vec<String> {
    vec![
        superzej_core::util::shell(),
        "-lc".into(),
        format!("exec {}", command.trim()),
    ]
}

/// Env for spawning yazi: an isolated `YAZI_CONFIG_HOME` so the user's own
/// `~/.config/yazi` (often written for a different yazi version — schema
/// breakage shows as TOML errors on every launch) can't break the drawer.
/// `[drawer] config_home`: `""` = a private superzej dir seeded once from the
/// bundled config, `"system"` = the user's own config, else an explicit path.
pub(crate) fn yazi_env(cfg: &superzej_core::config::Config) -> Vec<(String, String)> {
    let home = cfg.drawer.config_home.trim();
    let dir = match home {
        "system" => return Vec::new(),
        "" => {
            let dir = superzej_core::util::superzej_dir().join("yazi");
            if let Err(e) = seed_yazi_config(&dir) {
                tracing::warn!(target: "szhost", error = %e, "yazi config seed failed");
                return Vec::new();
            }
            dir
        }
        path => std::path::PathBuf::from(superzej_core::util::expand_tilde(path)),
    };
    vec![(
        "YAZI_CONFIG_HOME".into(),
        dir.to_string_lossy().into_owned(),
    )]
}

/// Write the bundled yazi config files into `dir` (only the missing ones, so
/// user tweaks to the private copy survive).
fn seed_yazi_config(dir: &std::path::Path) -> std::io::Result<()> {
    std::fs::create_dir_all(dir)?;
    for (name, body) in [
        ("yazi.toml", include_str!("../../../config/yazi/yazi.toml")),
        (
            "theme.toml",
            include_str!("../../../config/yazi/theme.toml"),
        ),
        (
            "keymap.toml",
            include_str!("../../../config/yazi/keymap.toml"),
        ),
    ] {
        let p = dir.join(name);
        if !p.exists() {
            std::fs::write(p, body)?;
        }
    }
    Ok(())
}

/// The global pane registry. A tab's panes are identified by the real ids in its
/// `CenterTree`; this just owns the live `PtyPane`s keyed by id.
pub(crate) struct Panes {
    pub(crate) table: std::collections::HashMap<u32, PtyPane>,
    next_id: u32,
    tx: tokio_mpsc::Sender<PaneEvent>,
    /// Pulsed by reader threads after each send so the main loop's blocking
    /// `poll_input(None)` wakes to drain PTY output. `None` in unit tests that
    /// construct `Panes` without a live terminal.
    waker: Option<TerminalWaker>,
    /// Wall-clock time each pane was spawned. Used by the crash debounce: if a
    /// pane exits within the threshold it's counted as a crash even if it wrote
    /// output (bwrap prints an error message before dying).
    spawn_times: std::collections::HashMap<u32, std::time::Instant>,
}

impl Panes {
    #[cfg(test)]
    pub(crate) fn new(tx: tokio_mpsc::Sender<PaneEvent>) -> Self {
        Self {
            table: std::collections::HashMap::new(),
            next_id: 1,
            tx,
            waker: None,
            spawn_times: std::collections::HashMap::new(),
        }
    }

    pub(crate) fn with_waker(tx: tokio_mpsc::Sender<PaneEvent>, waker: TerminalWaker) -> Self {
        Self {
            table: std::collections::HashMap::new(),
            next_id: 1,
            tx,
            waker: Some(waker),
            spawn_times: std::collections::HashMap::new(),
        }
    }

    /// Spawn one shell pane in `cwd`, sized to `center`; returns its id.
    pub(crate) fn spawn(&mut self, cwd: Option<&std::path::Path>, center: Rect) -> Result<u32> {
        let argv = pane_shell_argv();
        self.spawn_argv(&argv, cwd, center)
    }

    /// Spawn a specific argv in `cwd`, sized to `center`; returns its id.
    pub(crate) fn spawn_argv(
        &mut self,
        argv: &[String],
        cwd: Option<&std::path::Path>,
        center: Rect,
    ) -> Result<u32> {
        self.spawn_argv_env(argv, cwd, &[], center)
    }

    /// As [`Panes::spawn_argv`], but injects `env` into the child — used for
    /// agent panes that expect `SUPERZEJ_WORKTREE`/`SUPERZEJ_BRANCH` and for
    /// per-program env on pinned programs.
    pub(crate) fn spawn_argv_env(
        &mut self,
        argv: &[String],
        cwd: Option<&std::path::Path>,
        env: &[(String, String)],
        center: Rect,
    ) -> Result<u32> {
        let id = self.next_id;
        self.next_id += 1;
        let pane = PtyPane::spawn_with_env(
            id,
            argv,
            cwd,
            env,
            center.rows.max(1) as u16,
            center.cols.max(1) as u16,
            self.tx.clone(),
            self.waker.clone(),
        )?;
        self.table.insert(id, pane);
        self.spawn_times.insert(id, std::time::Instant::now());
        Ok(id)
    }

    /// How long the pane has been alive. `None` if the id is unknown.
    /// Used by the crash debounce: exits within 5s of spawn are fast-crashes.
    pub(crate) fn pane_age(&self, id: u32) -> Option<std::time::Duration> {
        self.spawn_times.get(&id).map(|t| t.elapsed())
    }

    /// Remove the spawn-time entry for a pane that has exited.
    pub(crate) fn forget_spawn_time(&mut self, id: u32) {
        self.spawn_times.remove(&id);
    }

    /// The leaves of `tab.center` not yet backed by live panes — the targets
    /// a spec-resolution pass must cover before [`Self::materialize_with_specs`].
    pub(crate) fn missing_leaves(&self, tab: &crate::session::Tab) -> Vec<u32> {
        tab.center
            .pane_ids()
            .into_iter()
            .filter(|id| !self.table.contains_key(id))
            .collect()
    }

    /// Finish materialization with pre-resolved launch specs: spawn a pane per
    /// missing leaf and remap the tree's leaf ids + focused id onto them. Spec
    /// resolution (sandbox ensure, DB lookups — potentially SLOW: a wedged
    /// podman pulls an image) happens off-thread via the loop's spec channel;
    /// this half is openpty+exec only, safe on the loop. `worktree` is the
    /// owning group's dir (tabs spawn their shells there).
    pub(crate) fn materialize_with_specs(
        &mut self,
        tab: &mut crate::session::Tab,
        worktree: &str,
        specs: &[(u32, crate::agent::LaunchSpec)],
        center: Rect,
    ) -> Result<()> {
        let cwd = (!worktree.is_empty() && std::path::Path::new(worktree).is_dir())
            .then(|| std::path::PathBuf::from(worktree))
            .or_else(|| std::env::current_dir().ok())
            .or_else(|| std::env::var("HOME").ok().map(std::path::PathBuf::from));

        let spawn_t0 = std::time::Instant::now();
        let mut map = std::collections::HashMap::new();
        for (old, spec) in specs {
            if self.table.contains_key(old) || map.contains_key(old) {
                continue; // raced a direct spawn; keep the live pane
            }
            match self.spawn_argv_env(
                &spec.argv,
                spec.cwd.as_deref().or(cwd.as_deref()),
                &spec.env,
                center,
            ) {
                Ok(fresh) => {
                    map.insert(*old, fresh);
                }
                Err(e) => {
                    let _ = std::fs::write(
                        "/tmp/szhost-spawn-err.log",
                        format!("Materialize spawn failed: {e:?}"),
                    );
                    return Err(e);
                }
            }
        }
        tracing::info!(
            target: "szhost::startup",
            spawn_ms = spawn_t0.elapsed().as_millis() as u64,
            panes = map.len(),
            "pty panes spawned"
        );
        let old_focus = tab.focused_pane;
        tab.center
            .remap(&mut |old| map.get(&old).copied().unwrap_or(old));
        tab.focused_pane = map
            .get(&old_focus)
            .copied()
            .or_else(|| tab.center.pane_ids().first().copied())
            .unwrap_or(0);
        Ok(())
    }
}

/// Tab indices to pre-warm: the `radius` neighbors on each side of `active`,
/// clamped to `[0, len)` and excluding `active` itself. Pure for unit testing.
fn prewarm_targets(active: usize, len: usize, radius: usize) -> Vec<usize> {
    if len == 0 || radius == 0 {
        return Vec::new();
    }
    let lo = active.saturating_sub(radius);
    let hi = (active + radius).min(len.saturating_sub(1));
    (lo..=hi).filter(|&i| i != active).collect()
}

/// Radius for pre-warming: immediate neighbors only, so we never fork a child
/// per tab on a large session.
const PREWARM_RADIUS: usize = 1;

/// The (worktree, tab, missing leaf ids) triples a pre-warm pass should
/// resolve specs for: the tabs adjacent to the active one (within the active
/// worktree) and the neighboring worktrees' active tabs, so first focus of a
/// neighbor is instant. Pure enumeration — the caller requests launch specs
/// off-thread (sandbox ensure can block) and finishes the spawns when they
/// land, exactly like the lazy materialize path.
pub(crate) fn prewarm_requests(
    panes: &Panes,
    session: &mut crate::session::Session,
) -> Vec<(String, usize, Vec<u32>)> {
    let mut out = Vec::new();
    if session.worktrees.is_empty() {
        return out;
    }
    // Sibling tabs within the active worktree.
    let g = &session.worktrees[session.active];
    for ti in prewarm_targets(g.active_tab, g.tabs.len(), PREWARM_RADIUS) {
        let missing = panes.missing_leaves(&g.tabs[ti]);
        if !missing.is_empty() {
            out.push((g.path.clone(), ti, missing));
        }
    }
    // Neighboring worktrees: their remembered active tab.
    for gi in prewarm_targets(session.active, session.worktrees.len(), PREWARM_RADIUS) {
        let g = &mut session.worktrees[gi];
        let at = g.active_tab.min(g.tabs.len().saturating_sub(1));
        g.active_tab = at;
        if let Some(tab) = g.tabs.get(at) {
            let missing = panes.missing_leaves(tab);
            if !missing.is_empty() {
                out.push((g.path.clone(), at, missing));
            }
        }
    }
    out
}

/// Resize each pane in `tree` to its CONTENT rect within `center` (inside the
/// 1-cell border ring the framed layout reserves — the PTY must agree with
/// what `compose_pane` paints).
pub(crate) fn relayout(panes: &mut Panes, tree: &crate::center::CenterTree, center: Rect) {
    for (id, _, content) in tree.layout_framed(center) {
        // A degenerate rect means the center is hidden behind a full-screen
        // panel / zoomed zone (`compute_full`/`compute_chrome` collapse it to
        // the mandatory single column). Resizing the PTY to ~1 col makes vt100
        // reflow the shell to one column and spill its content into scrollback,
        // which a later grow-back can't restore — the pane comes back blank.
        // Leave hidden panes at their real size; relayout runs again with a
        // real rect when the panel retracts.
        if content.cols <= 1 || content.rows <= 1 {
            continue;
        }
        if let Some(p) = panes.table.get_mut(&id) {
            let _ = p.resize(content.rows.max(1) as u16, content.cols.max(1) as u16);
        }
    }
}

/// Resize every live strip pane to the rect the supervisor apportions it
/// (minus the 1-row header).
pub(crate) fn relayout_strip(
    panes: &mut Panes,
    supervisor: &crate::pins::PinSupervisor,
    strip: Rect,
) {
    for (id, rect) in supervisor.strip_layout(strip) {
        if let Some(p) = panes.table.get_mut(&id) {
            let body_rows = rect.rows.saturating_sub(1).max(1);
            let _ = p.resize(body_rows as u16, rect.cols.max(1) as u16);
        }
    }
}

/// Replace an externally-dead sole center pane with a fresh shell pane without
/// closing the workspace tab. Explicit close-pane/close-worktree actions remove
/// panes from the session before their process exits, so this only handles
/// unexpected PTY child exits (killed shell, missing old child, etc.).
pub(crate) fn replace_single_dead_center_pane(
    tab: &mut crate::session::Tab,
    dead_id: u32,
    fresh_id: u32,
) -> bool {
    let ids = tab.center.pane_ids();
    if ids.as_slice() != [dead_id] {
        return false;
    }

    tab.center = crate::center::CenterTree::Leaf(fresh_id);
    tab.focused_pane = fresh_id;
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::layout;
    use crate::session::{GroupKind, Session, WorktreeGroup};

    fn one_tab_session() -> Session {
        Session {
            id: "s1".into(),
            worktrees: vec![WorktreeGroup::new("app/home", GroupKind::Home, "/tmp/app")],
            active: 0,
        }
    }

    #[test]
    fn prewarm_targets_returns_clamped_neighbors_excluding_active() {
        // Middle of a list: both neighbors.
        assert_eq!(prewarm_targets(2, 5, 1), vec![1, 3]);
        // First tab: only the right neighbor.
        assert_eq!(prewarm_targets(0, 5, 1), vec![1]);
        // Last tab: only the left neighbor.
        assert_eq!(prewarm_targets(4, 5, 1), vec![3]);
        // Single tab: nothing to pre-warm.
        assert_eq!(prewarm_targets(0, 1, 1), Vec::<usize>::new());
        // Empty / zero-radius: nothing.
        assert_eq!(prewarm_targets(0, 0, 1), Vec::<usize>::new());
        assert_eq!(prewarm_targets(2, 5, 0), Vec::<usize>::new());
        // Wider radius clamps at the ends.
        assert_eq!(prewarm_targets(1, 5, 2), vec![0, 2, 3]);
    }

    #[test]
    fn relayout_skips_panes_hidden_behind_a_fullscreen_panel() {
        // SAFETY: single-threaded test setup.
        unsafe { std::env::set_var("SHELL", "/bin/sh") };
        let mut session = one_tab_session();
        let chrome = layout::compute(160, 40, true, true);
        let (tx, _rx) = tokio_mpsc::channel::<PaneEvent>(1024);
        let mut panes = Panes::new(tx);
        let path = session.worktrees[0].path.clone();
        let mut cfg = superzej_core::config::Config::default();
        cfg.sandbox.enabled = false;
        let specs: Vec<(u32, crate::agent::LaunchSpec)> = panes
            .missing_leaves(&session.worktrees[0].tabs[0])
            .into_iter()
            .map(|id| {
                (
                    id,
                    crate::agent::launch_spec(&cfg, &path, None, "shell").unwrap(),
                )
            })
            .collect();
        panes
            .materialize_with_specs(
                &mut session.worktrees[0].tabs[0],
                &path,
                &specs,
                chrome.center,
            )
            .unwrap();

        let tree = session.worktrees[0].tabs[0].center.clone();
        let id = match &tree {
            crate::center::CenterTree::Leaf(id) => *id,
            _ => panic!("expected a single leaf pane"),
        };

        // A real rect sizes the pane.
        relayout(
            &mut panes,
            &tree,
            Rect {
                x: 0,
                y: 0,
                cols: 120,
                rows: 30,
            },
        );
        let real = panes.table.get(&id).unwrap().size();
        assert!(
            real.0 > 1 && real.1 > 1,
            "pane sized to a real rect: {real:?}"
        );

        // A degenerate rect (center hidden behind a full-screen panel) must NOT
        // resize the pane — that 1-col reflow destroys the shell's content.
        relayout(
            &mut panes,
            &tree,
            Rect {
                x: 0,
                y: 0,
                cols: 1,
                rows: 30,
            },
        );
        assert_eq!(
            panes.table.get(&id).unwrap().size(),
            real,
            "a hidden (1-col) pane keeps its real size"
        );

        // Retracting back to a real rect resizes it again.
        relayout(
            &mut panes,
            &tree,
            Rect {
                x: 0,
                y: 0,
                cols: 80,
                rows: 20,
            },
        );
        assert_ne!(
            panes.table.get(&id).unwrap().size(),
            real,
            "a real rect resizes the pane"
        );
    }

    #[test]
    fn toggle_drawer_spawns_and_closes_drawer_pane() {
        // The test spawns a drawer, which reads SHELL. Force it to something that exists.
        // SAFETY: single-threaded test setup.
        unsafe { std::env::set_var("SHELL", "/bin/sh") };
        let mut session = one_tab_session();
        let chrome = layout::compute(160, 40, true, true);

        let (tx, _rx) = tokio_mpsc::channel::<PaneEvent>(1024);
        let mut panes = Panes::new(tx);
        let path = session.worktrees[0].path.clone();
        let mut cfg = superzej_core::config::Config::default();
        cfg.sandbox.enabled = false;
        // Two-phase materialize: resolve a shell launch spec per missing leaf
        // (the loop does this off-thread), then spawn + remap on the tree.
        let specs: Vec<(u32, crate::agent::LaunchSpec)> = panes
            .missing_leaves(&session.worktrees[0].tabs[0])
            .into_iter()
            .map(|id| {
                (
                    id,
                    crate::agent::launch_spec(&cfg, &path, None, "shell").unwrap(),
                )
            })
            .collect();
        panes
            .materialize_with_specs(
                &mut session.worktrees[0].tabs[0],
                &path,
                &specs,
                chrome.center,
            )
            .unwrap();

        let mut drawer: Option<u32> = None;
        let mut dirty = false;

        let simulate_toggle = |drawer: &mut Option<u32>, panes: &mut Panes, dirty: &mut bool| {
            if drawer.is_some() {
                if let Some(id) = drawer.take() {
                    panes.table.remove(&id);
                }
            } else {
                let p = panes.spawn(None, chrome.center).ok();
                if let Some(id) = p {
                    *drawer = Some(id);
                }
            }
            *dirty = true;
        };

        // Initially no drawer
        assert!(drawer.is_none());
        assert_eq!(panes.table.len(), 1); // just the materialized center pane

        // Toggle ON
        simulate_toggle(&mut drawer, &mut panes, &mut dirty);
        assert!(drawer.is_some());
        assert_eq!(panes.table.len(), 2);
        assert!(dirty);

        // Toggle OFF
        simulate_toggle(&mut drawer, &mut panes, &mut dirty);
        assert!(drawer.is_none());
        assert_eq!(panes.table.len(), 1);
    }

    #[test]
    fn shell_argv_defaults_to_interactive_non_login() {
        assert_eq!(
            shell_argv_from("/run/current-system/sw/bin/fish", false),
            vec![
                "/run/current-system/sw/bin/fish".to_string(),
                "-i".to_string()
            ]
        );
        assert_eq!(
            shell_argv_from("/bin/zsh", false),
            vec!["/bin/zsh".to_string(), "-i".to_string()]
        );
        assert_eq!(
            shell_argv_from("/opt/custom-shell", false),
            vec!["/opt/custom-shell".to_string()]
        );
    }

    #[test]
    fn external_sole_center_pane_exit_is_replaced_with_fresh_shell_pane() {
        let mut tab = crate::session::Tab::new("1");
        tab.center = crate::center::CenterTree::Leaf(7);
        tab.focused_pane = 7;

        assert!(replace_single_dead_center_pane(&mut tab, 7, 42));
        assert_eq!(tab.center.pane_ids(), vec![42]);
        assert_eq!(tab.focused_pane, 42);
    }

    #[test]
    fn pane_shell_resolution_falls_back_when_shell_env_points_to_missing_binary() {
        let shell = resolve_pane_shell(Some("/definitely/missing/superzej-shell".into()));

        assert_ne!(shell, "/definitely/missing/superzej-shell");
        assert!(
            std::path::Path::new(&shell).is_file(),
            "fallback shell should exist on disk: {shell}"
        );
    }

    #[test]
    fn shell_argv_honors_login_override() {
        assert_eq!(
            shell_argv_from("/bin/bash", true),
            vec!["/bin/bash".to_string(), "-l".to_string()]
        );
    }

    #[test]
    fn tool_drawer_argv_runs_configured_command_inside_shell() {
        let argv = tool_drawer_argv("${EDITOR:-vi} .");
        assert_eq!(argv[1], "-lc");
        assert_eq!(argv[2], "exec ${EDITOR:-vi} .");
    }

    #[test]
    fn spawn_records_time_and_forget_clears_it() {
        // SAFETY: single-threaded test.
        unsafe { std::env::set_var("SHELL", "/bin/sh") };
        let (tx, _rx) = tokio_mpsc::channel::<PaneEvent>(1024);
        let mut panes = Panes::new(tx);
        let chrome = layout::compute(80, 24, false, false);

        let id = panes.spawn(None, chrome.center).expect("spawn");

        // pane_age returns Some(duration) right after spawn.
        let age = panes.pane_age(id);
        assert!(age.is_some(), "pane_age should be Some after spawn");
        assert!(
            age.unwrap() < std::time::Duration::from_secs(1),
            "age should be near-zero just after spawn"
        );

        // forget_spawn_time removes the entry.
        panes.forget_spawn_time(id);
        assert!(
            panes.pane_age(id).is_none(),
            "pane_age should be None after forget_spawn_time"
        );
    }

    #[test]
    fn pane_age_unknown_id_returns_none() {
        let (tx, _rx) = tokio_mpsc::channel::<PaneEvent>(1024);
        let panes = Panes::new(tx);
        assert!(panes.pane_age(999).is_none());
    }
}
