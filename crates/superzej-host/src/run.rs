//! The interactive loop: own the outer terminal, run one shell pane inside the
//! chrome cross, render it, route input. Fully event-driven — the loop blocks on
//! `poll_input(None)` (zero idle wakeups) and every off-thread producer (PTY
//! readers, model/PR hydration, config + worktree fs-watchers, the refresh
//! ticker) pulses the termwiz `TerminalWaker` after sending on its tokio channel,
//! which returns `InputEvent::Wake` so the loop drains its channels and repaints.
//! Frames are painted via `BufferedTerminal::draw_from_screen` + `flush`, which
//! diffs against the prior frame and emits only changed cells (no clear-and-redraw
//! → no flashing).

use anyhow::{Context, Result};
use notify::{Event, RecursiveMode, Watcher, recommended_watcher};
use std::path::Path;

use tokio::sync::mpsc as tokio_mpsc;
use tokio::task;

use termwiz::caps::Capabilities;
use termwiz::input::{InputEvent, KeyCode, Modifiers};
use termwiz::surface::{Change, Position, Surface};
use termwiz::terminal::buffered::BufferedTerminal;
use termwiz::terminal::{Terminal, TerminalWaker, new_terminal};

use crate::chrome::{FrameModel, render_tab};
use crate::compositor::Rect;
use crate::hydrate::{
    RefreshKind, active_tab_path, build_initial_model, load_or_seed_session, retarget_diff_watcher,
    spawn_model_hydration, spawn_pr_cache_refresh, spawn_refresh_ticker, workspace_list,
};
use crate::input::key_bytes;
use crate::layout;
use crate::palette::{build_agent_palette, build_palette, build_sandbox_palette};
use crate::pane::PaneEvent;
use crate::panes::{
    Panes, prewarm_neighbors, relayout, relayout_strip, replace_single_dead_center_pane,
    tool_drawer_argv,
};

pub fn now_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// The focused workspace's repo root + slug for per-workspace keybind layering.
/// `session.id` is the workspace's repo path; the active tab's worktree is the
/// repo-root overlay source. Both are `None` when no workspace is resolvable.
fn workspace_context(
    session: &crate::session::Session,
) -> (Option<std::path::PathBuf>, Option<String>) {
    if session.id.is_empty() {
        return (None, None);
    }
    let root = std::path::PathBuf::from(&session.id);
    let slug = superzej_core::repo::repo_slug(&root);
    let root = root.is_dir().then_some(root);
    (root, Some(slug))
}

/// Rebuild the host keymap for the session's focused workspace (profile +
/// global + per-workspace + repo-root layers).
fn rebuild_keymap(
    cfg: &superzej_core::config::Config,
    session: &crate::session::Session,
) -> crate::keymap::KeyMap {
    let (root, slug) = workspace_context(session);
    crate::keymap::default_keymap_for(cfg, root.as_deref(), slug.as_deref())
}

/// A one-line status summary if the resolved keymap has chord conflicts, else
/// `None`. Non-fatal: drives the launch/reload warning banner.
fn keybind_conflict_summary(cfg: &superzej_core::config::Config) -> Option<String> {
    let cols = superzej_core::keymap::detect_collisions(&superzej_core::keymap::effective(cfg));
    if cols.is_empty() {
        return None;
    }
    for c in &cols {
        superzej_core::msg::warn(&format!("keybind conflict: {c:?}"));
    }
    Some(format!(
        "\u{26a0} {} keybind conflict(s) — run `sj keys validate`",
        cols.len()
    ))
}

fn apply_mode_status(model: &mut FrameModel, mode: crate::keymap::Mode) {
    // The bottom bar carries the contextual keybind hints; the status slot
    // only flags a non-default input mode.
    model.status = match mode {
        crate::keymap::Mode::Normal => String::new(),
        m => format!("{} mode", m.as_str()),
    };
}

/// The bottom bar's contextual keybind hints: what works right now, given the
/// focused zone (and the panel's view when it owns the keyboard).
fn context_hints(
    focus: &crate::focus::FocusState,
    panel_ui: &crate::panel::PanelUi,
    cfg: &superzej_core::config::Config,
) -> String {
    let chord = |id: &str| -> Option<String> { crate::keymap::chord_hint_for(cfg, id) };
    let hint = |label: &str, id: &str| chord(id).map(|c| format!("{c} {label}"));
    if focus.locked {
        return hint("unlock", "toggle-key-lock").unwrap_or_else(|| "Ctrl-g unlock".into());
    }
    match focus.zone {
        crate::focus::Zone::Center => [
            hint("pane", "focus-left")
                .or_else(|| chord("focus-right").map(|c| format!("{c} pane"))),
            hint("tab", "prev-tab").or_else(|| chord("next-tab").map(|c| format!("{c} tab"))),
            hint("worktree", "prev-worktree")
                .or_else(|| chord("next-worktree").map(|c| format!("{c} worktree"))),
            hint("close tab", "close-tab"),
            hint("smart split", "new-pane"),
            hint("split↓", "split-down"),
            hint("split→", "split-right"),
            hint("zoom", "zoom"),
            hint("menu", "palette"),
        ]
        .into_iter()
        .flatten()
        .collect::<Vec<_>>()
        .join(" · "),
        crate::focus::Zone::Sidebar => [
            Some("↑↓ move".into()),
            Some("Enter open".into()),
            Some("Space mark".into()),
            Some("m menu".into()),
            hint("tab", "prev-tab").or_else(|| chord("next-tab").map(|c| format!("{c} tab"))),
            Some("Esc back".into()),
        ]
        .into_iter()
        .flatten()
        .collect::<Vec<_>>()
        .join(" · "),
        crate::focus::Zone::Panel => {
            let nav = [
                hint("pane", "focus-left")
                    .or_else(|| chord("focus-right").map(|c| format!("{c} pane"))),
                hint("tab", "prev-tab").or_else(|| chord("next-tab").map(|c| format!("{c} tab"))),
                Some("Esc back".into()),
            ]
            .into_iter()
            .flatten()
            .collect::<Vec<_>>()
            .join(" · ");
            let panel = crate::chrome::panel_help_hint(panel_ui.tab, panel_ui.diff_view);
            if nav.is_empty() {
                panel.into()
            } else {
                format!("{nav} · {panel}")
            }
        }
    }
}

pub async fn main(cli: crate::Cli) -> Result<()> {
    let start = std::time::Instant::now();
    // File-sink logging is opt-in via `SUPERZEJ_LOG` (an env-filter string).
    // When unset no subscriber is installed at all, so every tracing callsite
    // collapses to one atomic load — instrumentation is free in the idle case.
    if std::env::var_os("SUPERZEJ_LOG").is_some() {
        superzej_core::log::init(
            superzej_core::log::Role::Host,
            &superzej_core::config::LogConfig {
                file: true,
                ..Default::default()
            },
        );
    }

    // While the compositor owns the screen, any stray write to stderr (e.g.
    // `msg::warn`'s eprintln fallback when no log subscriber is installed)
    // scrolls the alt screen and corrupts the damage-tracked frame — ghost
    // tabbars / doubled panel headers. Redirect fd 2 to a file for the whole
    // session; the guard restores it on exit so post-exit errors still print.
    let _stderr_guard = redirect_stderr_to_logfile();

    let caps = Capabilities::new_from_env().context("term capabilities")?;
    let mut term = new_terminal(caps).context("open terminal")?;
    term.set_raw_mode().context("raw mode")?;
    term.enter_alternate_screen().context("alt screen")?;
    let size = term.get_screen_size().context("screen size")?;
    let (rows, cols) = (size.rows, size.cols);

    // Kitty keyboard protocol, "disambiguate escape codes": Ctrl+h/j/k/l then
    // arrive as CSI-u sequences (termwiz decodes fixterms) instead of legacy
    // control bytes that collide with Backspace/Enter. Terminals without the
    // protocol ignore the sequence and those chords degrade to passthrough —
    // Ctrl+arrows carry the focus moves everywhere.
    //
    // Also enable SGR mouse reporting (1002 = button + drag, 1006 = SGR
    // encoding): clicks focus panes/rows and drags build a per-pane selection
    // that auto-copies (OSC 52) on release, zellij-style.
    {
        use std::io::Write as _;
        let mut out = std::io::stdout();
        let _ = out
            .write_all(b"\x1b[>1u\x1b[?1002h\x1b[?1006h")
            .and_then(|_| out.flush());
    }

    let mut buf = BufferedTerminal::new(term).context("buffered terminal")?;

    // Grab the waker after `BufferedTerminal` takes ownership of the terminal.
    // Every off-thread producer pulses this so the loop's blocking
    // `poll_input(None)` returns to drain its channel — the loop is fully
    // event-driven (zero idle wakeups) rather than polled on a 16ms tick.
    let waker = buf.terminal().waker();
    tracing::info!(
        target: "szhost::startup",
        since_start_ms = start.elapsed().as_millis() as u64,
        "terminal ready (raw mode + alt screen + buffer)"
    );

    let cwd = std::env::current_dir().unwrap_or_else(|_| ".".into());
    let (session, seeded) = load_or_seed_session(&cwd);
    tracing::info!(
        target: "szhost::startup",
        since_start_ms = start.elapsed().as_millis() as u64,
        worktrees = session.worktrees.len(),
        "session loaded"
    );

    let cfg = superzej_core::config::Config::load_layered(
        &superzej_core::config::ProcessEnv,
        &cli.overrides,
        cli.config.clone(),
    );
    tracing::info!(
        target: "szhost::startup",
        since_start_ms = start.elapsed().as_millis() as u64,
        "config loaded"
    );
    let keymap = rebuild_keymap(&cfg, &session);
    let mode = crate::keymap::startup_mode(&cfg);
    // Resolve the configurable chrome palette ([theme] / [theme.colors]) before
    // the first frame; the config fs-watch re-resolves it live.
    crate::chrome::set_palette(cfg.palette());
    crate::center::PANE_HPAD.store(
        cfg.theme.pane_padding as usize,
        std::sync::atomic::Ordering::Relaxed,
    );
    let mut model = build_initial_model(&session);
    model.accent = cfg.accent_rgb();
    apply_mode_status(&mut model, mode);
    // Surface keybind conflicts at launch (non-fatal — the shell always opens).
    if let Some(summary) = keybind_conflict_summary(&cfg) {
        model.status = summary;
    }
    model.bars = cfg.bars.clone();
    model.stats_icons = cfg.stats.clone();
    let (model_tx, model_rx) = tokio_mpsc::unbounded_channel::<(u64, FrameModel)>();
    spawn_model_hydration(model_tx.clone(), 0, session.clone(), Some(waker.clone()));

    // Config reload events ride a tokio channel so the loop drains them on wake;
    // the notify watcher thread `send`s + pulses the waker.
    let (config_tx, config_rx) =
        tokio_mpsc::unbounded_channel::<Result<superzej_core::config::Config, String>>();

    let config_path = superzej_core::config::Config::path();
    let config_waker = waker.clone();
    std::thread::spawn(move || {
        if let Some(parent) = config_path.parent() {
            let mut last_send = std::time::Instant::now();
            let overrides_clone = cli.overrides.clone();
            let config_clone = cli.config.clone();
            if let Ok(mut watcher) = recommended_watcher(move |res: notify::Result<Event>| {
                if let Ok(ev) = res
                    && matches!(
                        ev.kind,
                        notify::EventKind::Modify(_)
                            | notify::EventKind::Create(_)
                            | notify::EventKind::Remove(_)
                    )
                    && last_send.elapsed() > std::time::Duration::from_millis(500)
                {
                    let new_cfg_res = superzej_core::config::Config::try_load_layered(
                        &superzej_core::config::ProcessEnv,
                        &overrides_clone,
                        config_clone.clone(),
                    );
                    if config_tx.send(new_cfg_res).is_ok() {
                        let _ = config_waker.wake();
                    }
                    last_send = std::time::Instant::now();
                }
            }) {
                let _ = watcher.watch(parent, RecursiveMode::NonRecursive);
                loop {
                    std::thread::sleep(std::time::Duration::MAX);
                }
            }
        }
    });

    // Low-frequency safety-net refresh: fs-watching the active worktree drives
    // prompt diff updates, but a periodic tick still rehydrates non-fs state
    // (branch moves, PR cache) and bounds staleness. The loop owns the actual
    // refresh; this thread just pulses a tick + waker on the interval.
    let (refresh_tx, refresh_rx) = tokio_mpsc::unbounded_channel::<RefreshKind>();
    let (stats_tx, stats_rx) = tokio_mpsc::unbounded_channel::<crate::stats::StatsSnapshot>();
    // The stats cadence is user-cyclable at runtime (click the top-right
    // stats block); the ticker thread reads it per tick.
    let stats_interval_ms = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(
        (cfg.stats.refresh_secs.max(0.5) * 1000.0) as u64,
    ));
    spawn_refresh_ticker(
        refresh_tx.clone(),
        stats_tx,
        stats_interval_ms.clone(),
        waker.clone(),
    );

    let result = event_loop(
        &mut buf,
        session,
        seeded,
        model,
        model_tx,
        model_rx,
        rows,
        cols,
        keymap,
        mode,
        config_rx,
        refresh_tx,
        refresh_rx,
        stats_rx,
        stats_interval_ms,
        waker,
        start,
    )
    .await;

    {
        use std::io::Write as _;
        let mut out = std::io::stdout();
        let _ = out
            .write_all(b"\x1b[?1006l\x1b[?1002l\x1b[<u")
            .and_then(|_| out.flush());
    }
    let _ = buf.terminal().exit_alternate_screen();
    let _ = buf.terminal().set_cooked_mode();
    result
}

/// Redirect process stderr to `$XDG_STATE_HOME/superzej/logs/szhost-stderr.log`
/// for the compositor's lifetime. Returns a guard whose `Drop` restores the
/// original fd. `None` (no redirect) if any step fails — never blocks startup.
fn redirect_stderr_to_logfile() -> Option<StderrGuard> {
    use std::os::unix::io::AsRawFd;
    let dir = superzej_core::util::xdg_state_home().join("superzej/logs");
    std::fs::create_dir_all(&dir).ok()?;
    let file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(dir.join("szhost-stderr.log"))
        .ok()?;
    // SAFETY: dup/dup2 on live fds; the guard restores fd 2 before close.
    unsafe {
        let saved = libc::dup(2);
        if saved < 0 {
            return None;
        }
        if libc::dup2(file.as_raw_fd(), 2) < 0 {
            libc::close(saved);
            return None;
        }
        Some(StderrGuard { saved })
    }
}

struct StderrGuard {
    saved: i32,
}

impl Drop for StderrGuard {
    fn drop(&mut self) {
        // SAFETY: `saved` is the dup of the original stderr taken at startup.
        unsafe {
            libc::dup2(self.saved, 2);
            libc::close(self.saved);
        }
    }
}

/// Compute the chrome cross with the strip reserved iff the supervisor wants it
/// shown and has live strip panes, at the runtime sidebar width. Single place so
/// every recompute agrees.
#[allow(clippy::too_many_arguments)]
fn compute_chrome(
    cols: usize,
    rows: usize,
    want_sidebar: bool,
    want_panel: bool,
    panel_forced: bool,
    panel_expanded: bool,
    sidebar_cols: usize,
    zoom: Option<crate::focus::Zone>,
    supervisor: &crate::pins::PinSupervisor,
) -> layout::ChromeLayout {
    use crate::focus::Zone;
    let strip = supervisor.strip_visible() && supervisor.has_strip_panes();
    match zoom {
        // Center zoom: full-width center (chrome columns suppressed); the
        // focused pane alone renders into it (see the render block).
        Some(Zone::Center) => layout::compute_full(
            cols,
            rows,
            false,
            false,
            false,
            false,
            sidebar_cols,
            false,
            0.0,
        ),
        // Sidebar / panel zoom: the zone takes (nearly) the whole width; a
        // 1-col center keeps the pane math alive.
        Some(Zone::Sidebar) => {
            let mut l = layout::compute_full(
                cols,
                rows,
                true,
                false,
                false,
                false,
                sidebar_cols,
                false,
                0.0,
            );
            let w = cols.saturating_sub(2).max(1);
            if let Some(sb) = l.sidebar.as_mut() {
                sb.cols = w;
            }
            l.sep_left = Some(w);
            for r in [&mut l.center_tabs, &mut l.center] {
                r.x = (w + 1).min(cols.saturating_sub(1));
                r.cols = 1;
            }
            l.strip = None;
            l
        }
        Some(Zone::Panel) => {
            let mut l = layout::compute_full(
                cols,
                rows,
                false,
                true,
                true,
                true,
                sidebar_cols,
                false,
                0.0,
            );
            let w = cols.saturating_sub(2).max(1);
            if let Some(pn) = l.panel.as_mut() {
                pn.x = cols - w;
                pn.cols = w;
            }
            l.sep_right = Some((cols - w).saturating_sub(1));
            for r in [&mut l.center_tabs, &mut l.center] {
                r.x = 0;
                r.cols = 1;
            }
            l.strip = None;
            l
        }
        None => layout::compute_full(
            cols,
            rows,
            want_sidebar,
            want_panel,
            panel_forced,
            panel_expanded,
            sidebar_cols,
            strip,
            supervisor.strip_ratio(),
        ),
    }
}

/// Working directory for a pin: explicit `cwd`, else the active tab's worktree,
/// else `$HOME` / cwd.
fn pin_cwd(
    pin: &superzej_core::config::Pin,
    active_dir: Option<std::path::PathBuf>,
) -> std::path::PathBuf {
    if let Some(c) = &pin.cwd {
        let expanded = superzej_core::util::expand_tilde(c);
        return std::path::PathBuf::from(expanded);
    }
    active_dir
        .or_else(|| std::env::current_dir().ok())
        .or_else(|| std::env::var("HOME").ok().map(std::path::PathBuf::from))
        .unwrap_or_else(|| std::path::PathBuf::from("/"))
}

/// Persist the supervisor's live pin set to `session_state.pin_state` (best
/// effort; pin persistence never blocks the loop).
fn persist_pin_state(supervisor: &crate::pins::PinSupervisor, session_id: &str) {
    if session_id.is_empty() {
        return;
    }
    if let Ok(db) = superzej_core::db::Db::open() {
        let _ = db.set_pin_state(session_id, &supervisor.to_json(), now_secs());
    }
}

/// Launch-or-focus the pin at 1-based `index` for the current workspace. Returns
/// an optional status line. Singleton pins that are already live are a no-op
/// (the strip/float already shows them). Strip/float/layout pins spawn a pane;
/// tab pins are out of scope for the strip path (handled via the tab system).
fn summon_pin(
    index: usize,
    cfg: &superzej_core::config::Config,
    session: &crate::session::Session,
    panes: &mut Panes,
    supervisor: &mut crate::pins::PinSupervisor,
    center: Rect,
) -> Option<String> {
    let ws = (!session.id.is_empty()).then_some(session.id.as_str());
    let resolved = crate::pins::PinSupervisor::resolve(cfg, ws);
    let pin = resolved.get(index.checked_sub(1)?)?;
    if pin.singleton && supervisor.live_instance(&pin.name).is_some() {
        return Some(format!("Pin '{}' already running", pin.display_label()));
    }
    let active_dir = active_cwd(session);
    let pin = (*pin).clone();
    match spawn_pin(&pin, panes, supervisor, active_dir, center) {
        Some(_) => Some(format!("Launched pin '{}'", pin.display_label())),
        None => Some(format!("Pin '{}' failed to launch", pin.display_label())),
    }
}

/// Spawn a pin's program into a pane and register it with the supervisor.
/// Sized to the strip body for strip pins, else the center. Returns the pane id.
fn spawn_pin(
    pin: &superzej_core::config::Pin,
    panes: &mut Panes,
    supervisor: &mut crate::pins::PinSupervisor,
    active_dir: Option<std::path::PathBuf>,
    center: Rect,
) -> Option<u32> {
    let argv = crate::pins::PinSupervisor::argv(pin);
    let env: Vec<(String, String)> = crate::pins::PinSupervisor::spawn_env(pin)
        .into_iter()
        .collect();
    let cwd = pin_cwd(pin, active_dir);
    match panes.spawn_argv_env(&argv, Some(&cwd), &env, center) {
        Ok(id) => {
            supervisor.attach(pin, id);
            Some(id)
        }
        Err(_) => None,
    }
}

fn refresh_tab_model(
    model: &mut FrameModel,
    session: &crate::session::Session,
    sb: &mut SidebarState,
) {
    let (worktree, tabs, active_tab) = crate::hydrate::tab_strip(session);
    model.worktree = worktree;
    model.tabs = tabs;
    model.active_tab = active_tab;
    // The workspace list can change when worktrees are added/closed or the
    // workspace switches: keep the DB-backed entries (refreshed by the next
    // hydration), re-derive the live fallbacks from the current session, and
    // drop stale fallbacks — replace semantics, never append-only (appending
    // duplicated workspaces whose live prefix didn't match their DB slug).
    let prev = std::mem::take(&mut model.sidebar_workspaces);
    model.sidebar_workspaces =
        crate::hydrate::merge_workspace_lists(prev, workspace_list(session, None));
    sb.rebuild(model, session);
}

/// Interaction + persisted view state for the workspace tree (items 16–27).
/// The single source of truth the event loop mutates; [`SidebarState::rebuild`]
/// derives `FrameModel`'s sidebar fields from it plus the model's data carriers.
#[derive(Default)]
struct SidebarState {
    view: crate::sidebar::ViewState,
    focused: bool,
    /// Cursor over the *visible* rows.
    cursor: usize,
    filtering: bool,
    /// Marked visible-row indices for bulk actions (item 26).
    marked: std::collections::HashSet<usize>,
    /// Open context menu, if any (item 27).
    menu: Option<crate::chrome::RowMenu>,
    /// Adjustable bar width in columns (item 25); `None` = layout default.
    width: Option<usize>,
}

impl SidebarState {
    /// Load persisted collapse/sort/pins/width from `ui_state` for this session.
    fn load(&mut self, db: &superzej_core::db::Db, scope: &str) {
        for (key, value) in db.ui_state_in_scope(scope).unwrap_or_default() {
            if let Some(slug) = key.strip_prefix("collapse:") {
                if value == "1" {
                    self.view.collapsed.insert(slug.to_string());
                }
            } else if let Some(slug) = key.strip_prefix("pin:") {
                if value == "1" && !self.view.pins.contains(&slug.to_string()) {
                    self.view.pins.push(slug.to_string());
                }
            } else if key == "sort_mode" {
                self.view.sort = crate::sidebar::SortMode::from_str(&value);
            } else if key == "sidebar_cols" {
                self.width = value.parse().ok();
            }
        }
    }

    /// The currently-selected visible row, if any.
    fn selected_row<'a>(&self, model: &'a FrameModel) -> Option<&'a crate::sidebar::SidebarRow> {
        model
            .sidebar_rows
            .iter()
            .filter(|r| r.visible)
            .nth(self.cursor)
    }

    /// Number of currently-visible rows.
    fn visible_len(model: &FrameModel) -> usize {
        model.sidebar_rows.iter().filter(|r| r.visible).count()
    }

    /// Rederive `model.sidebar_rows` from its data carriers + this view state,
    /// then mirror interaction fields into the model for the renderer.
    fn rebuild(&mut self, model: &mut FrameModel, session: &crate::session::Session) {
        model.sidebar_rows = crate::sidebar::build_rows(
            session,
            &model.sidebar_workspaces,
            &self.view,
            &model.sidebar_status,
            &model.sidebar_db_worktrees,
        );
        let visible = Self::visible_len(model);
        // While unfocused, track the active worktree so opening the sidebar
        // lands on the current tab; once focused, keep the user's cursor.
        if !self.focused {
            self.cursor = visible_index_of_active(model);
        }
        if visible == 0 {
            self.cursor = 0;
        } else if self.cursor >= visible {
            self.cursor = visible - 1;
        }
        self.sync(model);
    }

    /// Copy interaction state into the model fields the renderer reads.
    fn sync(&self, model: &mut FrameModel) {
        model.sidebar_selected = self.cursor;
        model.sidebar_focused = self.focused;
        model.sidebar_filter = self.view.filter.clone();
        model.sidebar_filtering = self.filtering;
        model.sidebar_sort = self.view.sort;
        model.sidebar_marked = self.marked.clone();
        model.sidebar_menu = self.menu.clone();
    }
}

/// What the event loop should do after a sidebar key was handled.
enum SidebarOutcome {
    /// Key wasn't for the sidebar; let normal dispatch handle it.
    NotHandled,
    /// Handled; just redraw.
    Redraw,
    /// Leave sidebar focus (return input to the pane).
    Defocus,
    /// Activate this `(worktree group, tab)` target.
    Activate(crate::sidebar::RowTarget),
    /// The layout changed (bar width); recompute chrome.
    Relayout,
    /// Close the worktree groups at these session indices (bulk action).
    CloseGroups(Vec<usize>),
    /// DELETE these worktree groups from disk (`git worktree remove`) and
    /// close them — destructive; the loop may interpose a confirmation.
    DeleteGroups(Vec<usize>),
}

impl SidebarState {
    /// Persist a single `ui_state` key for this session's scope.
    fn persist(&self, session_id: &str, key: &str, value: &str) {
        if let Ok(db) = superzej_core::db::Db::open() {
            let _ = db.set_ui_state(session_id, key, value);
        }
    }

    /// What the cursor row activates, if anything.
    fn cursor_target(&self, model: &FrameModel) -> Option<crate::sidebar::RowTarget> {
        self.selected_row(model).and_then(|r| r.tab_target.clone())
    }

    /// Build the context-menu entries for the cursor row (item 27).
    fn menu_for_cursor(
        &self,
        model: &FrameModel,
        session: &crate::session::Session,
    ) -> Option<crate::chrome::RowMenu> {
        use crate::sidebar::RowKind;
        let row = self.selected_row(model)?;
        let mut entries = Vec::new();
        if row.tab_target.is_some() {
            entries.push(("open", "Open"));
        }
        if row.kind == RowKind::Workspace {
            entries.push(("toggle", "Collapse/expand"));
        }
        entries.push(("pin", "Pin / unpin"));
        if row.kind == RowKind::Worktree {
            let is_home = matches!(
                row.tab_target,
                Some(crate::sidebar::RowTarget::Tab(gi, _))
                    if session.worktrees.get(gi).map(|g| g.kind) == Some(crate::session::GroupKind::Home)
            );
            if !is_home {
                entries.push(("close", "Close worktree"));
                entries.push(("delete", "Delete worktree (disk)"));
            }
        }
        Some(crate::chrome::RowMenu {
            anchor: self.cursor,
            entries: entries
                .into_iter()
                .map(|(id, label)| crate::chrome::RowMenuEntry {
                    id: id.into(),
                    label: label.into(),
                })
                .collect(),
            cursor: 0,
        })
    }

    /// Handle a key while the sidebar owns focus. Mutates view/interaction
    /// state, rebuilds rows, and returns what the loop must do.
    fn handle_key(
        &mut self,
        key: &KeyCode,
        mods: Modifiers,
        model: &mut FrameModel,
        session: &crate::session::Session,
    ) -> SidebarOutcome {
        // Filter input sub-mode captures text (item 21).
        if self.filtering {
            match key {
                KeyCode::Escape => {
                    self.filtering = false;
                    self.view.filter.clear();
                }
                KeyCode::Enter => self.filtering = false,
                KeyCode::Backspace => {
                    self.view.filter.pop();
                }
                KeyCode::Char(c) if !mods.contains(Modifiers::CTRL) => {
                    self.view.filter.push(*c);
                }
                _ => return SidebarOutcome::Redraw,
            }
            self.cursor = 0;
            self.rebuild(model, session);
            return SidebarOutcome::Redraw;
        }

        // Open context menu captures navigation (item 27).
        if let Some(menu) = &mut self.menu {
            match key {
                KeyCode::Escape => {
                    self.menu = None;
                }
                KeyCode::UpArrow | KeyCode::Char('k') => {
                    menu.cursor = menu.cursor.saturating_sub(1);
                }
                KeyCode::DownArrow | KeyCode::Char('j') => {
                    if menu.cursor + 1 < menu.entries.len() {
                        menu.cursor += 1;
                    }
                }
                KeyCode::Enter => {
                    let id = menu.entries.get(menu.cursor).map(|e| e.id.clone());
                    self.menu = None;
                    if let Some(id) = id {
                        return self.run_menu_action(&id, model, session);
                    }
                }
                _ => {}
            }
            self.sync(model);
            return SidebarOutcome::Redraw;
        }

        let visible = Self::visible_len(model);
        match key {
            KeyCode::Escape => return SidebarOutcome::Defocus,
            KeyCode::Char('q') => return SidebarOutcome::Defocus,
            KeyCode::DownArrow | KeyCode::Char('j') => {
                if visible > 0 {
                    self.cursor = (self.cursor + 1).min(visible - 1);
                }
            }
            KeyCode::UpArrow | KeyCode::Char('k') => {
                self.cursor = self.cursor.saturating_sub(1);
            }
            KeyCode::Enter => {
                // On a workspace row, Enter toggles collapse; elsewhere opens.
                if let Some(row) = self.selected_row(model) {
                    if row.kind == crate::sidebar::RowKind::Workspace {
                        return self.toggle_collapse(model, session);
                    }
                    if let Some(t) = row.tab_target.clone() {
                        return SidebarOutcome::Activate(t);
                    }
                }
            }
            KeyCode::Char('l') | KeyCode::RightArrow => {
                // Expand a collapsed workspace.
                if let Some(row) = self.selected_row(model)
                    && row.kind == crate::sidebar::RowKind::Workspace
                    && row.collapsed
                {
                    return self.toggle_collapse(model, session);
                }
            }
            KeyCode::Char('h') | KeyCode::LeftArrow => {
                // Collapse an expanded workspace.
                if let Some(row) = self.selected_row(model)
                    && row.kind == crate::sidebar::RowKind::Workspace
                    && !row.collapsed
                {
                    return self.toggle_collapse(model, session);
                }
            }
            KeyCode::Char('/') => {
                self.filtering = true;
                self.sync(model);
            }
            KeyCode::Char('s') => {
                self.view.sort = self.view.sort.next();
                self.persist(&session.id, "sort_mode", self.view.sort.as_str());
                self.rebuild(model, session);
            }
            KeyCode::Char('p') => return self.toggle_pin(model, session),
            KeyCode::Char(' ') => {
                // Multi-select toggle (item 26); on workspace rows, collapse.
                if let Some(row) = self.selected_row(model)
                    && row.kind == crate::sidebar::RowKind::Workspace
                {
                    return self.toggle_collapse(model, session);
                }
                if self.marked.contains(&self.cursor) {
                    self.marked.remove(&self.cursor);
                } else {
                    self.marked.insert(self.cursor);
                }
                self.sync(model);
            }
            KeyCode::Char('m') => {
                self.menu = self.menu_for_cursor(model, session);
                self.sync(model);
            }
            KeyCode::Char('X') => {
                // Bulk close: every marked worktree, else the cursor row.
                let targets = self.action_targets(model);
                if !targets.is_empty() {
                    return SidebarOutcome::CloseGroups(targets);
                }
            }
            KeyCode::Char('D') => {
                // Bulk DELETE from disk: marked worktrees, else the cursor row.
                let targets = self.action_targets(model);
                if !targets.is_empty() {
                    return SidebarOutcome::DeleteGroups(targets);
                }
            }
            KeyCode::Char('<') | KeyCode::Char(',') => {
                return self.adjust_width(-2, session);
            }
            KeyCode::Char('>') | KeyCode::Char('.') => {
                return self.adjust_width(2, session);
            }
            _ => return SidebarOutcome::NotHandled,
        }
        self.sync(model);
        SidebarOutcome::Redraw
    }

    /// The groups a bulk action applies to: every marked row's group, or the
    /// cursor row's group when nothing is marked.
    fn action_targets(&self, model: &FrameModel) -> Vec<usize> {
        let marked = self.marked_group_targets(model);
        if !marked.is_empty() {
            return marked;
        }
        match self.cursor_target(model) {
            Some(crate::sidebar::RowTarget::Tab(g, _)) => vec![g],
            _ => Vec::new(),
        }
    }

    /// Marked rows resolved to worktree-group indices (close acts per group).
    fn marked_group_targets(&self, model: &FrameModel) -> Vec<usize> {
        let visible: Vec<&crate::sidebar::SidebarRow> =
            model.sidebar_rows.iter().filter(|r| r.visible).collect();
        let mut targets: Vec<usize> = self
            .marked
            .iter()
            .filter_map(
                |&i| match visible.get(i).and_then(|r| r.tab_target.clone()) {
                    Some(crate::sidebar::RowTarget::Tab(g, _)) => Some(g),
                    _ => None,
                },
            )
            .collect();
        targets.sort_unstable();
        targets.dedup();
        targets
    }

    fn toggle_collapse(
        &mut self,
        model: &mut FrameModel,
        session: &crate::session::Session,
    ) -> SidebarOutcome {
        if let Some(row) = self.selected_row(model) {
            let slug = row.workspace_slug.clone();
            let now_collapsed = if self.view.collapsed.contains(&slug) {
                self.view.collapsed.remove(&slug);
                false
            } else {
                self.view.collapsed.insert(slug.clone());
                true
            };
            self.persist(
                &session.id,
                &format!("collapse:{slug}"),
                if now_collapsed { "1" } else { "0" },
            );
            self.rebuild(model, session);
        }
        SidebarOutcome::Redraw
    }

    fn toggle_pin(
        &mut self,
        model: &mut FrameModel,
        session: &crate::session::Session,
    ) -> SidebarOutcome {
        // Bulk: every marked row's pin key, else the cursor row's.
        let visible: Vec<&crate::sidebar::SidebarRow> =
            model.sidebar_rows.iter().filter(|r| r.visible).collect();
        let mut keys: Vec<String> = self
            .marked
            .iter()
            .filter_map(|&i| visible.get(i).map(|r| r.pin_key.clone()))
            .collect();
        if keys.is_empty()
            && let Some(row) = self.selected_row(model)
        {
            keys.push(row.pin_key.clone());
        }
        for key in keys {
            if let Some(pos) = self.view.pins.iter().position(|k| *k == key) {
                self.view.pins.remove(pos);
                self.persist(&session.id, &format!("pin:{key}"), "0");
            } else {
                self.view.pins.push(key.clone());
                self.persist(&session.id, &format!("pin:{key}"), "1");
            }
        }
        self.rebuild(model, session);
        SidebarOutcome::Redraw
    }

    fn adjust_width(&mut self, delta: i32, session: &crate::session::Session) -> SidebarOutcome {
        let cur = self.width.unwrap_or(crate::layout::SIDEBAR_COLS) as i32;
        let next = (cur + delta).clamp(
            crate::layout::SIDEBAR_MIN_WIDTH as i32,
            crate::layout::SIDEBAR_MAX_WIDTH as i32,
        ) as usize;
        self.width = Some(next);
        self.persist(&session.id, "sidebar_cols", &next.to_string());
        SidebarOutcome::Relayout
    }

    fn run_menu_action(
        &mut self,
        id: &str,
        model: &mut FrameModel,
        session: &crate::session::Session,
    ) -> SidebarOutcome {
        match id {
            "open" => {
                if let Some(t) = self.cursor_target(model) {
                    return SidebarOutcome::Activate(t);
                }
            }
            "toggle" => return self.toggle_collapse(model, session),
            "pin" => return self.toggle_pin(model, session),
            "close" => {
                let targets = self.action_targets(model);
                if !targets.is_empty() {
                    return SidebarOutcome::CloseGroups(targets);
                }
            }
            "delete" => {
                let targets = self.action_targets(model);
                if !targets.is_empty() {
                    return SidebarOutcome::DeleteGroups(targets);
                }
            }
            _ => {}
        }
        SidebarOutcome::Redraw
    }
}

/// Activate a sidebar row target: focus a live `(group, tab)` in the session,
/// or switch to another workspace (landing on its named worktree group when
/// that group exists in the target's persisted layout).
#[allow(clippy::too_many_arguments)]
fn activate_row_target(
    target: crate::sidebar::RowTarget,
    session: &mut crate::session::Session,
    model: &mut FrameModel,
    sb: &mut SidebarState,
    panes: &mut Panes,
    drawer: &mut Option<u32>,
    pool: &mut DrawerPool,
    home: &mut Option<std::path::PathBuf>,
    cfg: &superzej_core::config::Config,
    center: Rect,
) {
    match target {
        crate::sidebar::RowTarget::Tab(gi, ti) => {
            if gi >= session.worktrees.len() {
                return;
            }
            session.switch_to_tab(gi, ti);
        }
        crate::sidebar::RowTarget::Workspace { repo_path, group } => {
            let Ok(db) = superzej_core::db::Db::open() else {
                return;
            };
            // Collect the OUTGOING workspace's pane ids before the trees are
            // replaced; reap them only on success. Without this, the new
            // workspace's persisted trees can reference ids that still belong
            // to live panes of the old workspace (e.g. an editor pane bleeds
            // across the switch).
            let outgoing = session_pane_ids(session);
            let landed = group
                .as_deref()
                .map(|name| {
                    switch_to_workspace_tab(session, &db, &repo_path, name).unwrap_or(false)
                })
                .unwrap_or(false);
            if !landed && session.switch_to_workspace(&repo_path, &db).is_err() {
                return;
            }
            for id in outgoing {
                panes.table.remove(&id);
            }
            if let Some(id) = drawer.take() {
                panes.table.remove(&id);
            }
            for id in pool.drain_ids() {
                panes.table.remove(&id);
            }
        }
    }
    refresh_tab_model(model, session, sb);
    sync_drawer_persistence(session, panes, drawer, pool, home, cfg, center);
}

/// Worktree group indices in the order the sidebar DISPLAYS them (home-first
/// name sort, pins, filter). Alt+↑/↓ steps through this, not the session's
/// internal order — otherwise switching "skips around" relative to the tree.
fn sidebar_worktree_order(model: &FrameModel) -> Vec<usize> {
    model
        .sidebar_rows
        .iter()
        .filter(|r| r.visible && r.kind == crate::sidebar::RowKind::Worktree)
        .filter_map(|r| match r.tab_target {
            Some(crate::sidebar::RowTarget::Tab(g, _)) => Some(g),
            _ => None,
        })
        .collect()
}

/// The visible-row index of the active row, or 0.
fn visible_index_of_active(model: &FrameModel) -> usize {
    model
        .sidebar_rows
        .iter()
        .filter(|r| r.visible)
        .position(|r| r.active)
        .unwrap_or(0)
}

fn switch_to_workspace_tab(
    session: &mut crate::session::Session,
    db: &superzej_core::db::Db,
    repo_path: &str,
    group_name: &str,
) -> Result<bool> {
    session.switch_to_workspace(repo_path, db)?;
    let Some(idx) = session.worktrees.iter().position(|g| g.name == group_name) else {
        return Ok(false);
    };
    session.switch_to(idx);
    session.persist(db, &session.id, now_secs())?;
    Ok(true)
}

/// A worktree group's working directory, falling back to the process cwd.
fn group_cwd(g: &crate::session::WorktreeGroup) -> Option<std::path::PathBuf> {
    (!g.path.is_empty() && std::path::Path::new(&g.path).is_dir())
        .then(|| std::path::PathBuf::from(&g.path))
        .or_else(|| std::env::current_dir().ok())
}

/// The active worktree's working directory.
fn active_cwd(session: &crate::session::Session) -> Option<std::path::PathBuf> {
    session.active_group().and_then(group_cwd)
}

/// DELETE these worktree groups from disk (`git worktree remove`, branch
/// kept) and close them. Home groups are never deletable. Returns the status
/// line summarizing what happened.
fn deletable_group_targets(
    session: &crate::session::Session,
    targets: Vec<usize>,
) -> (Vec<usize>, usize) {
    let mut kept = Vec::new();
    let mut skipped = 0;
    for gi in targets {
        if session
            .worktrees
            .get(gi)
            .map(|g| g.kind == crate::session::GroupKind::Home)
            .unwrap_or(false)
        {
            skipped += 1;
        } else {
            kept.push(gi);
        }
    }
    (kept, skipped)
}

fn delete_groups(
    session: &mut crate::session::Session,
    panes: &mut Panes,
    mut targets: Vec<usize>,
) -> String {
    targets.sort_unstable_by(|a, b| b.cmp(a));
    targets.dedup();
    let (mut deleted, mut skipped) = (0usize, 0usize);
    for gi in targets {
        if gi >= session.worktrees.len() {
            continue;
        }
        if session.worktrees[gi].kind == crate::session::GroupKind::Home {
            skipped += 1;
            continue;
        }
        let path = session.worktrees[gi].path.clone();
        if !path.is_empty() {
            if let Some(root) = superzej_core::repo::main_worktree(Path::new(&path)) {
                superzej_core::worktree::remove(&root, Path::new(&path), "", false);
            }
            if let Ok(db) = superzej_core::db::Db::open() {
                let _ = db.del_worktree(&path);
            }
        }
        for tab in &session.worktrees[gi].tabs {
            for id in tab.center.pane_ids() {
                panes.table.remove(&id);
            }
        }
        session.switch_to(gi);
        session.close_active_group();
        deleted += 1;
    }
    let mut status = format!("Deleted {deleted} worktree(s) from disk");
    if skipped > 0 {
        status.push_str(" (home checkout skipped)");
    }
    status
}

/// Remove group `gi` from the session (its dir vanished from disk — deleted
/// externally); returns its pane ids for the caller to reap. Lands the user
/// on the workspace's home group. Pure w.r.t. disk/DB so it's unit-testable;
/// the caller handles the registry row + persist.
fn prune_vanished_group(session: &mut crate::session::Session, gi: usize) -> Vec<u32> {
    if gi >= session.worktrees.len() {
        return Vec::new();
    }
    let ids: Vec<u32> = session.worktrees[gi]
        .tabs
        .iter()
        .flat_map(|t| t.center.pane_ids())
        .collect();
    session.switch_to(gi);
    session.close_active_group();
    if let Some(hi) = session
        .worktrees
        .iter()
        .position(|g| g.kind == crate::session::GroupKind::Home)
    {
        session.switch_to(hi);
    }
    ids
}

/// Every pane id referenced by the session's tab trees. Pins and the drawer
/// live outside the trees, so they are naturally excluded.
fn session_pane_ids(session: &crate::session::Session) -> Vec<u32> {
    session
        .worktrees
        .iter()
        .flat_map(|g| g.tabs.iter())
        .flat_map(|t| t.center.pane_ids())
        .collect()
}

/// Highlighted single-file diff vs HEAD — the same range the panel's file list
/// is built from, so drilling in always matches the row. One fast,
/// user-triggered subprocess (same as the worktree-creation actions); failures
/// yield an empty body rather than an error.
fn fetch_file_diff(worktree: &std::path::Path, path: &str) -> String {
    let loc = superzej_core::remote::GitLoc::for_worktree(worktree);
    loc.git_command(&["diff", "--no-color", "HEAD", "--", path])
        .output()
        .map(|o| {
            superzej_core::diff_highlight::highlight_diff(&String::from_utf8_lossy(&o.stdout), path)
        })
        .unwrap_or_default()
}

/// Tracked + untracked (non-ignored) files for the Files tab, repo-relative.
/// One fast, user-triggered subprocess (same class as [`fetch_file_diff`]).
fn fetch_worktree_files(worktree: &std::path::Path) -> Vec<String> {
    let loc = superzej_core::remote::GitLoc::for_worktree(worktree);
    loc.git_command(&["ls-files", "--cached", "--others", "--exclude-standard"])
        .output()
        .map(|o| {
            String::from_utf8_lossy(&o.stdout)
                .lines()
                .map(|s| s.to_string())
                .collect()
        })
        .unwrap_or_default()
}

/// The bat invocation for the in-panel preview: colored, numbered, unpaged,
/// unwrapped (our renderer soft-wraps). `override_cmd` is `[tools] bat`'s
/// first token when configured.
fn bat_preview_argv(override_cmd: Option<&str>, abs: &std::path::Path) -> Vec<String> {
    let bin = override_cmd
        .and_then(|c| c.split_whitespace().next())
        .unwrap_or("bat")
        .to_string();
    vec![
        bin,
        "--color=always".into(),
        "--style=numbers,changes".into(),
        "--paging=never".into(),
        "--wrap=never".into(),
        "--".into(),
        abs.to_string_lossy().into_owned(),
    ]
}

/// Read + highlight a file for the Files tab's in-panel preview: real `bat`
/// output when available (the user's pager, faithfully), syntect fallback
/// otherwise. Size-capped and binary-safe; errors degrade to a message line.
fn fetch_file_preview(worktree: &std::path::Path, path: &str, bat: Option<&str>) -> String {
    const MAX_BYTES: usize = 1_000_000;
    let abs = worktree.join(path);
    let bytes = match std::fs::read(&abs) {
        Ok(b) if b.len() > MAX_BYTES => {
            return format!("{path}: too large to preview ({} KiB)", b.len() / 1024);
        }
        Ok(b) => b,
        Err(e) => return format!("{path}: {e}"),
    };
    let Ok(text) = String::from_utf8(bytes) else {
        return format!("{path}: binary file");
    };
    let argv = bat_preview_argv(bat, &abs);
    if let Ok(out) = std::process::Command::new(&argv[0])
        .args(&argv[1..])
        .current_dir(worktree)
        .output()
        && out.status.success()
        && !out.stdout.is_empty()
    {
        return String::from_utf8_lossy(&out.stdout).into_owned();
    }
    superzej_core::diff_highlight::highlight_file(&text, path)
}

/// Cache key for a drilled document (`'d'` diff / `'p'` preview).
fn doc_key(kind: char, worktree: &std::path::Path, path: &str) -> String {
    format!("{kind}:{}:{path}", worktree.display())
}

fn fetch_doc(kind: char, worktree: &std::path::Path, path: &str, bat: Option<&str>) -> String {
    if kind == 'd' {
        fetch_file_diff(worktree, path)
    } else {
        fetch_file_preview(worktree, path, bat)
    }
}

/// Preload neighbor documents off-thread into the doc cache so Shift+J/K
/// lands instantly; pulses the waker so the loop banks the results.
fn spawn_doc_preload(
    tx: tokio_mpsc::UnboundedSender<(String, String)>,
    waker: TerminalWaker,
    worktree: std::path::PathBuf,
    kind: char,
    paths: Vec<String>,
    bat: Option<String>,
) {
    tokio::task::spawn_blocking(move || {
        for path in paths {
            let key = doc_key(kind, &worktree, &path);
            let doc = fetch_doc(kind, &worktree, &path, bat.as_deref());
            if tx.send((key, doc)).is_err() {
                return;
            }
        }
        let _ = waker.wake();
    });
}

/// The next-two + previous-one indices around `cur` (for document preloads).
fn neighbor_indices(len: usize, cur: usize) -> Vec<usize> {
    let mut v = Vec::new();
    for i in [cur + 1, cur + 2] {
        if i < len {
            v.push(i);
        }
    }
    if let Some(p) = cur.checked_sub(1) {
        v.push(p);
    }
    v
}

/// Show the diff document for the current Diff cursor (cache-first), enter
/// the FileDiff view, and preload neighbors.
fn show_diff_doc(
    panel_ui: &mut crate::panel::PanelUi,
    files: &[crate::panel::DiffFile],
    wt: &std::path::Path,
    cache: &mut std::collections::HashMap<String, String>,
    tx: &tokio_mpsc::UnboundedSender<(String, String)>,
    waker: &TerminalWaker,
) {
    let Some(f) = files.get(panel_ui.diff_cursor) else {
        return;
    };
    let key = doc_key('d', wt, &f.path);
    let doc = cache
        .get(&key)
        .cloned()
        .unwrap_or_else(|| fetch_file_diff(wt, &f.path));
    cache.insert(key, doc.clone());
    panel_ui.file_diff = doc;
    panel_ui.focused_path = f.path.clone();
    panel_ui.diff_scroll = 0;
    panel_ui.diff_view = crate::panel::DiffView::FileDiff;
    let neighbors: Vec<String> = neighbor_indices(files.len(), panel_ui.diff_cursor)
        .into_iter()
        .map(|i| files[i].path.clone())
        .filter(|p| !cache.contains_key(&doc_key('d', wt, p)))
        .collect();
    if !neighbors.is_empty() {
        spawn_doc_preload(
            tx.clone(),
            waker.clone(),
            wt.to_path_buf(),
            'd',
            neighbors,
            None,
        );
    }
}

/// The visible-row positions of FILE entries (dirs skipped) in the Files tree.
fn file_row_positions(panel_ui: &crate::panel::PanelUi) -> (Vec<usize>, Vec<usize>) {
    let visible = crate::panel::visible_file_indices(&panel_ui.files, &panel_ui.files_collapsed);
    let positions: Vec<usize> = visible
        .iter()
        .enumerate()
        .filter(|(_, vi)| !panel_ui.files[**vi].is_dir)
        .map(|(pos, _)| pos)
        .collect();
    (visible, positions)
}

/// Show the preview for the Files cursor's file (cache-first) and preload
/// neighboring files in visible order.
fn show_file_preview(
    panel_ui: &mut crate::panel::PanelUi,
    wt: &std::path::Path,
    cache: &mut std::collections::HashMap<String, String>,
    tx: &tokio_mpsc::UnboundedSender<(String, String)>,
    waker: &TerminalWaker,
    bat: Option<&str>,
) {
    let (visible, positions) = file_row_positions(panel_ui);
    let Some(&vi) = visible.get(panel_ui.files_cursor) else {
        return;
    };
    let entry = panel_ui.files[vi].clone();
    if entry.is_dir {
        return;
    }
    let key = doc_key('p', wt, &entry.path);
    let doc = cache
        .get(&key)
        .cloned()
        .unwrap_or_else(|| fetch_file_preview(wt, &entry.path, bat));
    cache.insert(key, doc.clone());
    panel_ui.file_diff = doc;
    panel_ui.focused_path = entry.path;
    panel_ui.diff_scroll = 0;
    panel_ui.files_preview = true;
    let cur_fp = positions
        .iter()
        .position(|&p| p == panel_ui.files_cursor)
        .unwrap_or(0);
    let neighbors: Vec<String> = neighbor_indices(positions.len(), cur_fp)
        .into_iter()
        .map(|fp| panel_ui.files[visible[positions[fp]]].path.clone())
        .filter(|p| !cache.contains_key(&doc_key('p', wt, p)))
        .collect();
    if !neighbors.is_empty() {
        spawn_doc_preload(
            tx.clone(),
            waker.clone(),
            wt.to_path_buf(),
            'p',
            neighbors,
            bat.map(str::to_string),
        );
    }
}

/// Normalize legacy control-key encodings so configured chords match on
/// every terminal: Ctrl+Space arrives as NUL (0x00) without the kitty
/// keyboard protocol, but the chord is written "Ctrl Space".
fn normalize_key(k: termwiz::input::KeyEvent) -> termwiz::input::KeyEvent {
    if k.key == KeyCode::Char('\0') {
        return termwiz::input::KeyEvent {
            key: KeyCode::Char(' '),
            modifiers: k.modifiers | Modifiers::CTRL,
        };
    }
    k
}

/// Count immediately-available repeats of `first` (same key + modifiers);
/// the first NON-identical event is returned for requeueing. `next` yields
/// `None` when the input queue is drained. Coalescing a held key's backlog
/// into one application kills scroll inertia without dropping other events.
fn drain_key_repeats(
    first: &termwiz::input::KeyEvent,
    mut next: impl FnMut() -> Option<InputEvent>,
) -> (usize, Option<InputEvent>) {
    let mut count = 1usize;
    loop {
        match next() {
            Some(InputEvent::Key(k)) if k.key == first.key && k.modifiers == first.modifiers => {
                count += 1;
            }
            Some(other) => return (count, Some(other)),
            None => return (count, None),
        }
    }
}

/// Run the detected test command off-thread; results (parsed indicator
/// lines + summary) ride the channel back to the loop with a waker pulse.
fn spawn_test_run(
    tx: tokio_mpsc::UnboundedSender<(Vec<crate::panel::TestLine>, String)>,
    waker: TerminalWaker,
    worktree: std::path::PathBuf,
    command: String,
) {
    tokio::task::spawn_blocking(move || {
        let out = std::process::Command::new(superzej_core::util::shell())
            .arg("-lc")
            .arg(&command)
            .current_dir(&worktree)
            .output();
        let (lines, summary) = match out {
            Ok(o) => {
                let text = format!(
                    "{}\n{}",
                    String::from_utf8_lossy(&o.stdout),
                    String::from_utf8_lossy(&o.stderr)
                );
                let lines = crate::panel::parse_test_output(&text);
                let summary = if lines.is_empty() {
                    if o.status.success() {
                        "passed (no per-test output recognized)".to_string()
                    } else {
                        format!("failed ({})", o.status)
                    }
                } else {
                    crate::panel::test_summary(&lines)
                };
                (lines, summary)
            }
            Err(e) => (Vec::new(), format!("could not run: {e}")),
        };
        let _ = tx.send((lines, summary));
        let _ = waker.wake();
    });
}

/// The active tab's focused pane id (0 when no tab exists).
fn focused_pane_id(session: &crate::session::Session) -> u32 {
    session.active_tab().map(|t| t.focused_pane).unwrap_or(0)
}

/// Zellij's auto-split heuristic: split along the pane's longer visual
/// dimension (cells are ~2.4× taller than wide, hence the aspect factor) —
/// wide panes split right, tall panes split down.
fn smart_split_dir(cols: usize, rows: usize) -> crate::center::Dir {
    if cols * 10 >= rows * 24 {
        crate::center::Dir::Row
    } else {
        crate::center::Dir::Col
    }
}

/// Rows available to the panel's document/list body (below the switcher row
/// and its blank spacer).
fn panel_doc_rows(chrome: &layout::ChromeLayout) -> usize {
    chrome
        .panel
        .map(|p| {
            let (zone, _) = crate::chrome::panel_split(p);
            zone.rows.saturating_sub(2).max(1)
        })
        .unwrap_or(20)
}

/// Scroll the panel's CURRENT view by `n`, clamped at both ends — a drilled
/// document scrolls its lines (stopping at the last line), the Diff file
/// list and Files tree move their cursors (stopping at the last row).
fn scroll_panel(
    panel_ui: &mut crate::panel::PanelUi,
    model: &FrameModel,
    up: bool,
    n: usize,
    body_rows: usize,
) {
    if panel_ui.drilled() {
        let max = panel_ui.file_diff.lines().count().saturating_sub(body_rows);
        panel_ui.diff_scroll = if up {
            panel_ui.diff_scroll.saturating_sub(n)
        } else {
            (panel_ui.diff_scroll + n).min(max)
        };
        return;
    }
    match panel_ui.tab {
        crate::panel::PanelTab::Diff => {
            let max = model.panel.files.len().saturating_sub(1);
            panel_ui.diff_cursor = if up {
                panel_ui.diff_cursor.saturating_sub(n)
            } else {
                (panel_ui.diff_cursor + n).min(max)
            };
        }
        crate::panel::PanelTab::Tests => {
            let max = panel_ui.tests_lines.len().saturating_sub(body_rows);
            panel_ui.tests_scroll = if up {
                panel_ui.tests_scroll.saturating_sub(n)
            } else {
                (panel_ui.tests_scroll + n).min(max)
            };
        }
        crate::panel::PanelTab::Files => {
            let visible =
                crate::panel::visible_file_indices(&panel_ui.files, &panel_ui.files_collapsed);
            let max = visible.len().saturating_sub(1);
            panel_ui.files_cursor = if up {
                panel_ui.files_cursor.saturating_sub(n)
            } else {
                (panel_ui.files_cursor + n).min(max)
            };
            if panel_ui.files_cursor < panel_ui.files_scroll {
                panel_ui.files_scroll = panel_ui.files_cursor;
            } else if panel_ui.files_cursor >= panel_ui.files_scroll + body_rows {
                panel_ui.files_scroll = panel_ui.files_cursor + 1 - body_rows;
            }
        }
        _ => {}
    }
}

/// Background-load BOTH documents (diff + bat preview) for the hovered file
/// and the one below it, deduped against the cache and the in-flight set so
/// holding j/k never spawns a subprocess storm.
#[allow(clippy::too_many_arguments)]
fn preload_hover(
    panel_ui: &crate::panel::PanelUi,
    files: &[crate::panel::DiffFile],
    wt: &std::path::Path,
    cache: &std::collections::HashMap<String, String>,
    inflight: &mut std::collections::HashSet<String>,
    tx: &tokio_mpsc::UnboundedSender<(String, String)>,
    waker: &TerminalWaker,
    bat: Option<&str>,
) {
    // Hovered + next, in the active tab's ordering.
    let hovered: Vec<String> = match panel_ui.tab {
        crate::panel::PanelTab::Diff => {
            let c = panel_ui.diff_cursor;
            files
                .iter()
                .skip(c)
                .take(2)
                .map(|f| f.path.clone())
                .collect()
        }
        crate::panel::PanelTab::Files => {
            let (visible, positions) = file_row_positions(panel_ui);
            let start = positions
                .iter()
                .position(|&p| p >= panel_ui.files_cursor)
                .unwrap_or(0);
            positions
                .iter()
                .skip(start)
                .take(2)
                .map(|&p| panel_ui.files[visible[p]].path.clone())
                .collect()
        }
        _ => Vec::new(),
    };
    for kind in ['d', 'p'] {
        let jobs: Vec<String> = hovered
            .iter()
            .filter(|p| {
                let key = doc_key(kind, wt, p);
                !cache.contains_key(&key) && inflight.insert(key)
            })
            .cloned()
            .collect();
        if !jobs.is_empty() {
            spawn_doc_preload(
                tx.clone(),
                waker.clone(),
                wt.to_path_buf(),
                kind,
                jobs,
                bat.map(str::to_string),
            );
        }
    }
}

/// Move the Files cursor to the next/prev FILE row (skipping directories).
/// Returns false when there is nothing to move to.
fn step_file_cursor(panel_ui: &mut crate::panel::PanelUi, fwd: bool) -> bool {
    let (_, positions) = file_row_positions(panel_ui);
    if positions.is_empty() {
        return false;
    }
    let cur = positions
        .iter()
        .position(|&p| p >= panel_ui.files_cursor)
        .unwrap_or(positions.len() - 1);
    let new = if positions.get(cur) == Some(&panel_ui.files_cursor) {
        if fwd {
            (cur + 1).min(positions.len() - 1)
        } else {
            cur.saturating_sub(1)
        }
    } else {
        // Cursor sat on a dir: snap to the nearest file in the direction.
        if fwd {
            cur.min(positions.len() - 1)
        } else {
            cur.saturating_sub(1)
        }
    };
    panel_ui.files_cursor = positions[new];
    true
}

/// Spawn `command` (via the login-shell exec wrapper) into a fresh tab of the
/// active worktree and focus it.
fn open_command_tab(
    session: &mut crate::session::Session,
    panes: &mut Panes,
    command: &str,
    cwd: Option<&std::path::Path>,
    center: Rect,
) {
    let argv = tool_drawer_argv(command);
    let Ok(id) = panes.spawn_argv(&argv, cwd, center) else {
        return;
    };
    if let Some(g) = session.active_group_mut() {
        g.add_tab();
        if let Some(tab) = g.active_tab_mut() {
            tab.center = crate::center::CenterTree::Leaf(id);
            tab.focused_pane = id;
            return;
        }
    }
    panes.table.remove(&id);
}

/// Spawn `command` into a new split beside the focused center pane.
fn open_command_pane(
    session: &mut crate::session::Session,
    panes: &mut Panes,
    focused: u32,
    command: &str,
    cwd: Option<&std::path::Path>,
    center: Rect,
) {
    let argv = tool_drawer_argv(command);
    let Ok(id) = panes.spawn_argv(&argv, cwd, center) else {
        return;
    };
    if let Some(tab) = session.active_tab_mut()
        && tab.center.split(focused, crate::center::Dir::Row, id)
    {
        tab.focused_pane = id;
        return;
    }
    panes.table.remove(&id);
}

/// (Re)build the Files tab's tree for the active worktree. Top-level
/// directories start collapsed so the tab opens as a tidy accordion; cached
/// per worktree and rebuilt only when the worktree changes.
fn refresh_files_tree(panel_ui: &mut crate::panel::PanelUi, session: &crate::session::Session) {
    let wt = active_tab_path(session);
    let key = wt.to_string_lossy().into_owned();
    if panel_ui.files_worktree == key && !panel_ui.files.is_empty() {
        return;
    }
    panel_ui.files = crate::panel::build_file_tree(&fetch_worktree_files(&wt));
    panel_ui.files_collapsed = panel_ui
        .files
        .iter()
        .filter(|e| e.is_dir && e.depth == 0)
        .map(|e| e.path.clone())
        .collect();
    panel_ui.files_worktree = key;
    panel_ui.files_cursor = 0;
    panel_ui.files_scroll = 0;
}

/// Keep-alive yazi drawers, one per worktree dir: hiding STASHES the pane
/// (cursor position and yazi state survive), showing takes it back
/// instantly, and the worktree-change detector pre-warms the pool so the
/// first toggle never waits on yazi's startup.
///
/// The pool is bounded by `[drawer].pool_limit`: hidden drawers are held in
/// insertion order and the oldest is evicted (its pane torn down) once the
/// limit is exceeded, so invisible yazi instances cannot accumulate without
/// limit. `pool_limit = 0` disables pooling entirely (hiding kills the pane).
#[derive(Default)]
struct DrawerPool {
    /// `(dir-key, pane-id)` in insertion order; front is the oldest (next to evict).
    hidden: std::collections::VecDeque<(String, u32)>,
}

impl DrawerPool {
    fn key(dir: &std::path::Path) -> String {
        superzej_core::util::slugify(&dir.to_string_lossy())
    }
    /// Stash `id` for `dir`, enforcing `limit`. A `limit` of 0 tears the pane
    /// down immediately (no pool); otherwise the oldest entries beyond the
    /// limit are evicted and their panes dropped from the table.
    fn stash(&mut self, dir: &std::path::Path, id: u32, limit: usize, panes: &mut Panes) {
        if limit == 0 {
            panes.table.remove(&id);
            return;
        }
        let key = Self::key(dir);
        self.remove_key(&key, panes);
        self.hidden.push_back((key, id));
        while self.hidden.len() > limit {
            if let Some((_, evicted)) = self.hidden.pop_front() {
                panes.table.remove(&evicted);
            }
        }
    }
    fn take(&mut self, dir: &std::path::Path) -> Option<u32> {
        let key = Self::key(dir);
        let idx = self.hidden.iter().position(|(k, _)| k == &key)?;
        self.hidden.remove(idx).map(|(_, id)| id)
    }
    fn contains(&self, dir: &std::path::Path) -> bool {
        let key = Self::key(dir);
        self.hidden.iter().any(|(k, _)| k == &key)
    }
    /// Drop a pooled entry by pane id (e.g. its yazi exited on its own).
    fn remove_id(&mut self, id: u32) -> bool {
        let Some(idx) = self.hidden.iter().position(|(_, hid)| *hid == id) else {
            return false;
        };
        self.hidden.remove(idx);
        true
    }
    /// Drop the pooled entry for `key`, tearing down its pane.
    fn remove_key(&mut self, key: &str, panes: &mut Panes) {
        if let Some(idx) = self.hidden.iter().position(|(k, _)| k == key)
            && let Some((_, id)) = self.hidden.remove(idx)
        {
            panes.table.remove(&id);
        }
    }
    /// All pooled pane ids (workspace switch reaps them).
    fn drain_ids(&mut self) -> Vec<u32> {
        self.hidden.drain(..).map(|(_, id)| id).collect()
    }
}

/// Wrap a drawer yazi argv in a bounded user `systemd-run --scope` so its whole
/// process tree — including image-preview helpers such as `ueberzugpp`, which
/// can leak to tens of GB — is OOM-killed inside its own cgroup instead of
/// triggering a global OOM that takes the terminal session down. Empty limit
/// strings omit only that property. Containment is skipped when disabled, when
/// `systemd-run` is unavailable, or when the resolved sandbox already launches
/// through `systemd-run` (avoids a nested scope that would escape the bound).
fn contain_yazi_argv(
    cfg: &superzej_core::config::Config,
    cmd: Vec<String>,
    systemd_available: bool,
) -> Vec<String> {
    if !cfg.drawer.contain
        || !systemd_available
        || cmd.first().map(String::as_str) == Some("systemd-run")
    {
        return cmd;
    }
    let mut wrapped = vec![
        "systemd-run".to_string(),
        "--user".into(),
        "--scope".into(),
        "--quiet".into(),
        "--collect".into(),
    ];
    for (key, value) in [
        ("MemoryMax", cfg.drawer.memory_max.trim()),
        ("MemorySwapMax", cfg.drawer.memory_swap_max.trim()),
        ("CPUQuota", cfg.drawer.cpu_quota.trim()),
    ] {
        if !value.is_empty() {
            wrapped.push("-p".into());
            wrapped.push(format!("{key}={value}"));
        }
    }
    wrapped.push("--".into());
    wrapped.extend(cmd);
    wrapped
}

/// Spawn a fresh (hidden or visible) yazi pane for `dir`; falls back to a
/// plain shell when no yazi tool is configured.
fn spawn_worktree_shell_pane(
    panes: &mut Panes,
    cfg: &superzej_core::config::Config,
    dir: Option<&std::path::Path>,
    center: Rect,
) -> Result<u32> {
    if let Some(dir) = dir
        && dir.is_dir()
    {
        let wt = dir.to_string_lossy().into_owned();
        let spec = crate::agent::launch_spec(cfg, &wt, None, "shell");
        return panes.spawn_argv_env(
            &spec.argv,
            spec.cwd.as_deref().or(Some(dir)),
            &spec.env,
            center,
        );
    }
    panes.spawn(dir, center)
}

fn spawn_yazi_pane(
    panes: &mut Panes,
    cfg: &superzej_core::config::Config,
    dir: Option<&std::path::Path>,
    center: Rect,
) -> Option<u32> {
    if let Some(dir) = dir
        && dir.is_dir()
        && cfg.tool_command("yazi").is_some()
    {
        let wt = dir.to_string_lossy().into_owned();
        let spec = crate::agent::launch_spec(cfg, &wt, None, "yazi");
        let yenv = crate::panes::yazi_env(cfg);
        let argv = contain_yazi_argv(cfg, spec.argv, superzej_core::util::have("systemd-run"));
        return panes
            .spawn_argv_env(&argv, spec.cwd.as_deref().or(Some(dir)), &yenv, center)
            .ok();
    }
    spawn_worktree_shell_pane(panes, cfg, dir, center).ok()
}

/// Show the worktree's drawer: pooled pane if alive (instant, position
/// preserved), fresh spawn otherwise. Records the dir the pane belongs to in
/// `home` so hiding stashes it under the RIGHT key even after a switch.
#[allow(clippy::too_many_arguments)]
fn show_yazi_drawer(
    panes: &mut Panes,
    drawer: &mut Option<u32>,
    pool: &mut DrawerPool,
    home: &mut Option<std::path::PathBuf>,
    cfg: &superzej_core::config::Config,
    dir: &std::path::Path,
    center: Rect,
) {
    if drawer.is_some() {
        return;
    }
    if let Some(id) = pool.take(dir) {
        *drawer = Some(id);
        *home = Some(dir.to_path_buf());
        return;
    }
    *drawer = spawn_yazi_pane(panes, cfg, Some(dir), center);
    if drawer.is_some() {
        *home = Some(dir.to_path_buf());
    }
}

/// Hide the visible drawer, keeping its pane alive in the pool under the dir
/// it was opened for (`home`; `fallback` covers pre-tracking drawers). The
/// stash honors `[drawer].pool_limit`, evicting/tearing down older drawers.
#[allow(clippy::too_many_arguments)]
fn hide_drawer_into_pool(
    drawer: &mut Option<u32>,
    pool: &mut DrawerPool,
    home: &mut Option<std::path::PathBuf>,
    fallback: &std::path::Path,
    cfg: &superzej_core::config::Config,
    panes: &mut Panes,
) {
    if let Some(id) = drawer.take() {
        let key = home.take().unwrap_or_else(|| fallback.to_path_buf());
        pool.stash(&key, id, cfg.drawer.pool_limit, panes);
    }
}

/// A worktree awaiting its agent choice. While set, the command palette is
/// in "agent picker" mode: its selection launches the chosen agent into the
/// group named `tab` rather than dispatching a command. Escaping defaults to a
/// plain shell so the worktree is never left with no process.
struct PendingAgent {
    /// The `{repo_slug}/{branch}` group name to launch into.
    tab: String,
    worktree: String,
    branch: String,
    choosing_sandbox: bool,
}

/// A freshly-created worktree, ready to back a tab + agent launch.
struct NewWorktree {
    /// The `{repo_slug}/{branch}` tab name.
    tab: String,
    /// The branch created.
    branch: String,
    /// Absolute worktree path (local on disk; DB key for the agent launch).
    path: String,
}

/// Create a local git worktree off `repo_root`, reusing core's worktree helpers
/// (the same calls the legacy `new_worktree` command made, minus the zellij
/// tab). Records it in the DB so the sidebar/dashboard/resurrect pick it up.
/// Returns `None` (after a branded warning) when the base has no commits or the
/// `git worktree add` fails.
fn create_local_worktree(
    cfg: &superzej_core::config::Config,
    repo_root: &std::path::Path,
) -> Option<NewWorktree> {
    use superzej_core::{db::Db, repo, util, worktree};

    let base = worktree::resolve_base(repo_root, cfg);
    if util::git_out(repo_root, &["rev-parse", "--verify", "--quiet", &base]).is_none() {
        superzej_core::msg::warn(&format!(
            "'{base}' has no commits yet — make an initial commit before adding a worktree."
        ));
        return None;
    }

    let slug = repo::repo_slug(repo_root);
    let branch = worktree::branch_name(repo_root, None, cfg);
    let tab = repo::branch_tab(&slug, &branch);
    let path = worktree::worktree_path(repo_root, &branch, cfg);
    if !worktree::add(repo_root, &branch, &base, &path, cfg) {
        superzej_core::msg::warn("could not create the worktree (see the git error above).");
        return None;
    }
    let path = path.to_string_lossy().into_owned();

    if let Ok(db) = Db::open() {
        let _ = db.put_worktree(&tab, &repo_root.to_string_lossy(), &path, &branch, None);
    }
    Some(NewWorktree { tab, branch, path })
}

/// Launch `choice` into the worktree tab named `pending.tab`: compose the
/// sandbox-wrapped argv + env (via [`crate::agent::launch_spec`]), spawn it as a
/// fresh pane, and point that tab's center at the live pane so `materialize`
/// won't also spawn a plain shell. No-op (returns false) if the tab is gone.
fn launch_agent_into_tab(
    cfg: &superzej_core::config::Config,
    session: &mut crate::session::Session,
    panes: &mut Panes,
    pending: &PendingAgent,
    choice: &str,
    center: Rect,
) -> bool {
    let Some(gi) = session.worktrees.iter().position(|g| g.name == pending.tab) else {
        return false;
    };
    let spec = crate::agent::launch_spec(cfg, &pending.worktree, Some(&pending.branch), choice);
    let cwd = spec.cwd.clone();
    match panes.spawn_argv_env(&spec.argv, cwd.as_deref(), &spec.env, center) {
        Ok(id) => {
            // Reap any panes the group's active tab already had, then back it
            // with the agent pane.
            let g = &mut session.worktrees[gi];
            let ti = g.active_tab.min(g.tabs.len().saturating_sub(1));
            let Some(tab) = g.tabs.get_mut(ti) else {
                return false;
            };
            for old in tab.center.pane_ids() {
                panes.table.remove(&old);
            }
            tab.center = crate::center::CenterTree::Leaf(id);
            tab.focused_pane = id;
            true
        }
        Err(e) => {
            superzej_core::msg::warn(&format!("agent launch failed: {e}"));
            false
        }
    }
}

fn sync_drawer_persistence(
    session: &crate::session::Session,
    panes: &mut Panes,
    drawer: &mut Option<u32>,
    pool: &mut DrawerPool,
    home: &mut Option<std::path::PathBuf>,
    cfg: &superzej_core::config::Config,
    center: Rect,
) {
    let Some(dir) = active_cwd(session) else {
        return;
    };
    let key = superzej_core::util::slugify(&dir.to_string_lossy());
    let should_be_open =
        std::fs::read_to_string(superzej_core::util::superzej_dir().join("drawer").join(key))
            .map(|s| s.trim() == "true")
            .unwrap_or(false);

    // The visible drawer belongs to whichever worktree opened it; on a
    // mismatch stash it under ITS home before deciding for the new one.
    if drawer.is_some() && home.as_deref() != Some(dir.as_path()) {
        hide_drawer_into_pool(drawer, pool, home, &dir, cfg, panes);
    }
    if should_be_open && drawer.is_none() {
        show_yazi_drawer(panes, drawer, pool, home, cfg, &dir, center);
    } else if !should_be_open && drawer.is_some() {
        hide_drawer_into_pool(drawer, pool, home, &dir, cfg, panes);
    }
}

#[allow(clippy::too_many_arguments)]
async fn event_loop<T: Terminal>(
    buf: &mut BufferedTerminal<T>,
    mut session: crate::session::Session,
    seeded: bool,
    mut model: FrameModel,
    model_tx: tokio_mpsc::UnboundedSender<(u64, FrameModel)>,
    mut model_rx: tokio_mpsc::UnboundedReceiver<(u64, FrameModel)>,
    mut rows: usize,
    mut cols: usize,
    mut keymap: crate::keymap::KeyMap,
    mut mode: crate::keymap::Mode,
    mut config_rx: tokio_mpsc::UnboundedReceiver<Result<superzej_core::config::Config, String>>,
    refresh_tx: tokio_mpsc::UnboundedSender<RefreshKind>,
    mut refresh_rx: tokio_mpsc::UnboundedReceiver<RefreshKind>,
    mut stats_rx: tokio_mpsc::UnboundedReceiver<crate::stats::StatsSnapshot>,
    stats_interval_ms: std::sync::Arc<std::sync::atomic::AtomicU64>,
    waker: TerminalWaker,
    start: std::time::Instant,
) -> Result<()> {
    let mut scratch = Surface::new(cols, rows);
    // What the terminal currently shows; the render path diffs scratch
    // against this and sends only the delta (see the flush block).
    let mut front = Surface::new(cols, rows);
    // Freshly-seeded sessions launch dormant: no PTY is forked and the center
    // shows the logo splash until the first keypress / center click (dashboard
    // style). Resurrected sessions never see it. The bench guard keeps
    // `just bench` measuring launch → first SHELL frame, unchanged.
    let mut center_dormant =
        seeded && std::env::var_os("SUPERZEJ_BENCH_FIRST_FRAME_EXIT").is_none();
    tracing::debug!(target: "szhost::frame", seeded, center_dormant, "dormant init");
    // One-shot startup milestone: logged after the first diff-flush below.
    let mut first_frame_logged = false;
    let mut want_sidebar = true;
    let mut want_panel = true;
    // An explicit un-hide on a small screen overrides the panel's auto-hide
    // threshold (readable width up to nearly full screen); cleared on hide.
    let mut panel_forced = false;
    // True while a drilled-in document (single-file diff / file preview) is
    // open: the panel widens to a reading width and retracts on exit. The
    // loop-top detector keeps it in sync with `panel_ui`.
    let mut panel_expanded = false;
    // Set while the panel was popped up by Ctrl+→ at the center edge; holds
    // the (want_panel, panel_forced) pair to restore when focus leaves it.
    let mut panel_auto_revealed: Option<(bool, bool)> = None;
    // Drilled-document cache (highlighted diffs/previews) + the channel the
    // background preloader feeds it through; Shift+J/K hits are instant.
    let mut doc_cache: std::collections::HashMap<String, String> = std::collections::HashMap::new();
    let (doc_tx, mut doc_rx) = tokio_mpsc::unbounded_channel::<(String, String)>();
    // Test-run results from the background runner.
    let (tests_tx, mut tests_rx) =
        tokio_mpsc::unbounded_channel::<(Vec<crate::panel::TestLine>, String)>();
    // Keys currently being fetched by the preloader (dedupes hover storms).
    let mut doc_inflight: std::collections::HashSet<String> = std::collections::HashSet::new();
    // Sidebar interaction + persisted view state (collapse/sort/pins/width).
    let mut sb = SidebarState::default();
    if let Ok(db) = superzej_core::db::Db::open() {
        sb.load(&db, &session.id);
    }
    tracing::info!(
        target: "szhost::startup",
        since_start_ms = start.elapsed().as_millis() as u64,
        "sidebar state loaded"
    );
    let mut sidebar_cols = sb.width.unwrap_or(layout::SIDEBAR_COLS);
    // The last tab name we acked activity for (clears its "look at me" dot).
    let mut last_acked_tab: Option<String> = None;

    // The pin supervisor owns daemon panes independent of tabs/visibility.
    let mut supervisor = crate::pins::PinSupervisor::from_config(keymap.config());
    // Live theme-cycle position within `theme::PRESETS` (Ctrl+Alt+t).
    let mut theme_idx: usize = superzej_core::theme::PRESETS
        .iter()
        .position(|p| *p == keymap.config().theme.preset)
        .unwrap_or(0);
    // Fullscreen zoom: the zone that owns the whole screen, if any. Toggled
    // by Ctrl+Alt+z for the CURRENT zone; any zone change clears it.
    let mut zoom: Option<crate::focus::Zone> = None;
    let mut chrome = compute_chrome(
        cols,
        rows,
        want_sidebar,
        want_panel,
        panel_forced,
        panel_expanded,
        sidebar_cols,
        zoom,
        &supervisor,
    );
    sb.rebuild(&mut model, &session);
    let mut dirty = true;
    // One zone owns the keyboard at any time; Ctrl+g toggles the keybind lock.
    // `sb.focused` / `model.panel_focused` / `model.center_focused` mirror it.
    let mut focus = crate::focus::FocusState::default();
    // The right panel's persistent UI state (tab, cursor, drill-in view).
    let mut panel_ui = crate::panel::PanelUi::default();
    let mut prev_zone = focus.zone;
    // Mouse: SGR press/drag tracking + the live text selection — (pane id,
    // grid selection). Drags highlight within ONE pane only and auto-copy
    // (OSC 52) on release, zellij-style.
    let mut mouse_left_down = false;
    // Swallows split SGR mouse fragments termwiz mis-delivers as keys.
    let mut residue = crate::mousefilter::MouseResidueFilter::default();
    // Events read ahead of their turn by the key-repeat coalescer; consumed
    // before blocking on poll_input again.
    let mut pending_input: std::collections::VecDeque<InputEvent> =
        std::collections::VecDeque::new();
    let mut mouse_selecting = false;
    let mut mouse_sel: Option<(u32, crate::copymode::Selection)> = None;
    // A destructive delete awaiting its y/N confirmation: (question, targets).
    let mut pending_delete: Option<(String, Vec<usize>)> = None;
    // Force a full terminal repaint on the next flush when the chrome
    // GEOMETRY changed (toggles, strip, resize) — nothing from the previous
    // layout may survive. Tab/worktree switches reuse the same rects and must
    // NOT trigger this: a full repaint flashes the whole center.
    let mut full_repaint = true;
    let mut palette: Option<crate::palette::Palette> = None;
    // Cheatsheet overlay (Alt-?) and the transient which-key popup (set while a
    // multi-key prefix is pending). Both render via the shared `keyhint` module.
    let mut cheatsheet = false;
    let mut which_key: Vec<crate::keyhint::HintRow> = Vec::new();
    let mut which_key_prefix = String::new();
    // When set, the open palette is an agent picker for a just-created worktree
    // tab; its selection launches the agent rather than dispatching a command.
    let mut pending_agent: Option<PendingAgent> = None;

    let (tx, mut rx) = tokio_mpsc::channel::<PaneEvent>(1024);
    let mut panes = Panes::with_waker(tx, waker.clone());
    let mut need_relayout = true;
    let mut drawer: Option<u32> = None;
    // Hidden keep-alive yazi panes per worktree (instant drawer toggles).
    let mut drawer_pool = DrawerPool::default();
    // The dir the VISIBLE drawer was opened for (its pool key when hidden).
    let mut drawer_home: Option<std::path::PathBuf> = None;

    // Diff fs-watcher: bound to the active worktree, re-targeted on tab switch.
    // On a (debounced) change it pushes `RefreshKind::Model` into the shared
    // refresh channel + pulses the waker, so the diff panel updates on file save
    // instead of waiting for the periodic safety-net tick.
    let mut watched_worktree: Option<std::path::PathBuf> = None;
    let mut diff_watcher: Option<notify::RecommendedWatcher> = None;
    // Finished watchers arrive here from the retarget threads (see
    // `retarget_diff_watcher`); the loop adopts the one matching the
    // currently-watched worktree and drops stale ones.
    let (watcher_tx, mut watcher_rx) =
        tokio_mpsc::unbounded_channel::<(std::path::PathBuf, notify::RecommendedWatcher)>();
    retarget_diff_watcher(
        &session,
        &mut watched_worktree,
        &mut diff_watcher,
        &watcher_tx,
        &refresh_tx,
        &waker,
    );
    tracing::info!(
        target: "szhost::startup",
        since_start_ms = start.elapsed().as_millis() as u64,
        "diff watcher targeted"
    );

    sync_drawer_persistence(
        &session,
        &mut panes,
        &mut drawer,
        &mut drawer_pool,
        &mut drawer_home,
        keymap.config(),
        chrome.center,
    );
    tracing::info!(
        target: "szhost::startup",
        since_start_ms = start.elapsed().as_millis() as u64,
        "drawer synced"
    );

    let mut current_config = keymap.config().clone();
    // The workspace the keymap was last built for; when the focused workspace
    // changes we rebuild so per-workspace/repo-root keybind layers follow it.
    let mut keymap_workspace = session.id.clone();
    // The active worktree as of the last loop turn. When it changes (any switch
    // path: keymap tab-nav, palette, sidebar) we kick an immediate model + PR
    // refresh and re-target the diff watcher — so the panel is correct for the
    // new worktree right away (stale-while-revalidate) instead of up to 2s late.
    let mut last_active_worktree: Option<std::path::PathBuf> = Some(active_tab_path(&session));
    // Monotonic tag for model hydrations; intake drops results whose tag isn't
    // current (a pre-switch hydration landing post-switch). The startup spawn
    // in `run()` used 0, which this initial value accepts.
    let mut hydration_gen: u64 = 0;

    // Launch eager pins + resurrect previously-running pins for this workspace.
    {
        let ws = (!session.id.is_empty()).then(|| session.id.clone());
        let active_dir = active_cwd(&session);

        // Names to launch: eager pins ∪ persisted (previously-running) pins, in
        // config order, deduped.
        let mut to_launch: Vec<superzej_core::config::Pin> =
            crate::pins::PinSupervisor::eager_pins(&current_config, ws.as_deref())
                .into_iter()
                .cloned()
                .collect();
        if let Ok(db) = superzej_core::db::Db::open()
            && let Ok(Some(json)) = db.pin_state(&session.id)
        {
            for pp in crate::pins::PinSupervisor::parse_persisted(&json, &current_config) {
                if !to_launch.iter().any(|p| p.name == pp.name)
                    && let Some(p) = current_config.pin(&pp.name)
                {
                    to_launch.push(p.clone());
                }
            }
        }
        for pin in &to_launch {
            spawn_pin(
                pin,
                &mut panes,
                &mut supervisor,
                active_dir.clone(),
                chrome.center,
            );
        }
        if supervisor.has_strip_panes() {
            chrome = compute_chrome(
                cols,
                rows,
                want_sidebar,
                want_panel,
                panel_forced,
                panel_expanded,
                sidebar_cols,
                zoom,
                &supervisor,
            );
            need_relayout = true;
        }
        persist_pin_state(&supervisor, &session.id);
    }
    tracing::info!(
        target: "szhost::startup",
        since_start_ms = start.elapsed().as_millis() as u64,
        "pins launched, entering loop"
    );
    loop {
        if session.worktrees.is_empty() {
            return Ok(()); // last worktree closed
        }
        // Per-workspace keybinds: rebuild when the focused workspace changed.
        if session.id != keymap_workspace {
            keymap = rebuild_keymap(&current_config, &session);
            keymap.reset();
            keymap_workspace = session.id.clone();
        }
        // Leaving the panel fully resets it (drill closed, preview closed,
        // scroll rewound — cursors kept). Central, so EVERY exit path (Esc,
        // Ctrl+←, mouse, Alt+s, …) behaves identically.
        if prev_zone == crate::focus::Zone::Panel && !focus.panel() {
            panel_ui.reset_on_leave();
            // A Ctrl+→ auto-revealed panel disappears again when navigated
            // off; explicit Toggle/FocusPanel pins it (they clear the marker).
            if let Some((w, f)) = panel_auto_revealed.take() {
                want_panel = w;
                panel_forced = f;
                chrome = compute_chrome(
                    cols,
                    rows,
                    want_sidebar,
                    want_panel,
                    panel_forced,
                    panel_expanded,
                    sidebar_cols,
                    zoom,
                    &supervisor,
                );
                need_relayout = true;
                dirty = true;
            }
        }
        if prev_zone != focus.zone
            && let Some(z) = zoom
            && z != focus.zone
        {
            // Navigating to another zone un-zooms.
            zoom = None;
            chrome = compute_chrome(
                cols,
                rows,
                want_sidebar,
                want_panel,
                panel_forced,
                panel_expanded,
                sidebar_cols,
                zoom,
                &supervisor,
            );
            need_relayout = true;
            dirty = true;
        }
        prev_zone = focus.zone;
        sb.focused = focus.sidebar();

        // Detect an active-worktree change centrally so every switch path is
        // covered without per-call-site wiring.
        let current_worktree = active_tab_path(&session);
        if last_active_worktree.as_deref() != Some(current_worktree.as_path()) {
            last_active_worktree = Some(current_worktree.clone());
            // A selection anchored in the previous worktree's pane is stale.
            mouse_sel = None;
            // Immediate hydrate for the newly-focused worktree; the cached panel
            // stays on screen until the fresh model lands (never blank).
            hydration_gen += 1;
            spawn_model_hydration(
                model_tx.clone(),
                hydration_gen,
                session.clone(),
                Some(waker.clone()),
            );
            spawn_pr_cache_refresh(session.clone(), Some(waker.clone()));
            retarget_diff_watcher(
                &session,
                &mut watched_worktree,
                &mut diff_watcher,
                &watcher_tx,
                &refresh_tx,
                &waker,
            );
            // Pre-warm sibling tabs so first focus of a neighbor is instant.
            prewarm_neighbors(&mut panes, &mut session, chrome.center, keymap.config());
            // And the new worktree's hidden yazi drawer, so the first toggle
            // never waits on yazi's startup. Off by default ([drawer].prewarm)
            // so invisible yazi instances never accumulate unbidden.
            if keymap.config().drawer.prewarm
                && keymap.config().drawer.pool_limit > 0
                && drawer.is_none()
                && keymap.config().tool_command("yazi").is_some()
                && !drawer_pool.contains(&current_worktree)
                && let Some(id) = spawn_yazi_pane(
                    &mut panes,
                    keymap.config(),
                    Some(&current_worktree),
                    chrome.center,
                )
            {
                drawer_pool.stash(
                    &current_worktree,
                    id,
                    keymap.config().drawer.pool_limit,
                    &mut panes,
                );
            }
        }

        if let Ok(size) = buf.terminal().get_screen_size()
            && (size.rows != rows || size.cols != cols)
        {
            rows = size.rows;
            cols = size.cols;
            chrome = compute_chrome(
                cols,
                rows,
                want_sidebar,
                want_panel,
                panel_forced,
                panel_expanded,
                sidebar_cols,
                zoom,
                &supervisor,
            );
            need_relayout = true;
            buf.resize(cols, rows);
            // The physical screen content is untrustworthy after a resize —
            // rebuild the wire state from scratch (see the Resized arm).
            full_repaint = true;
            dirty = true;
        }

        // The active tab's panes are spawned lazily on first focus. While the
        // launch splash is up (dormant) nothing is forked — the first
        // keypress/center click clears `center_dormant` and the next loop turn
        // materializes the shell.
        if !center_dormant && first_frame_logged {
            let (path, ti) = {
                let g = &mut session.worktrees[session.active];
                let ti = g.active_tab.min(g.tabs.len().saturating_sub(1));
                g.active_tab = ti;
                (g.path.clone(), ti)
            };
            // A worktree whose dir vanished (deleted externally) must never
            // crash the loop: prune it from the session + registry, land on
            // home, and tell the user. The stat is cheap (`active_tab_path`
            // precedent); the DB remote-exemption check runs only on a miss.
            let dir_missing = !path.is_empty() && !Path::new(&path).is_dir();
            let remote = dir_missing
                && superzej_core::db::Db::open()
                    .and_then(|db| db.worktrees())
                    .map(|rows| {
                        rows.iter()
                            .any(|w| w.worktree == path && !w.location.is_empty())
                    })
                    .unwrap_or(false);
            if dir_missing && !remote && session.worktrees.len() > 1 {
                let gi = session.active;
                let ids = prune_vanished_group(&mut session, gi);
                for id in ids {
                    panes.table.remove(&id);
                }
                if let Ok(db) = superzej_core::db::Db::open() {
                    let _ = db.del_worktree(&path);
                    let _ = session.persist(&db, &session.id, now_secs());
                }
                model.status = format!("Worktree dir gone: {path} — removed from session");
                refresh_tab_model(&mut model, &session, &mut sb);
                need_relayout = true;
                dirty = true;
                // The landing group materializes on the next loop turn.
            } else if let Err(e) = panes.materialize(
                &mut session.worktrees[session.active].tabs[ti],
                &path,
                chrome.center,
                keymap.config(),
            ) {
                // Spawn failures are survivable: report, don't exit the loop.
                model.status = format!("Pane spawn failed: {e}");
                dirty = true;
            }
        }
        if need_relayout {
            let tree = if zoom == Some(crate::focus::Zone::Center) {
                crate::center::CenterTree::Leaf(focused_pane_id(&session))
            } else {
                session
                    .active_tab()
                    .map(|t| t.center.clone())
                    .unwrap_or(crate::center::CenterTree::Leaf(0))
            };
            relayout(&mut panes, &tree, chrome.center);
            if let Some(strip_rect) = chrome.strip {
                relayout_strip(&mut panes, &supervisor, strip_rect);
            }
            // Keep the tabbar chips in sync with the live pin set/health.
            let ws = (!session.id.is_empty()).then_some(session.id.as_str());
            model.pins = supervisor.chips(&current_config, ws);
            need_relayout = false;
        }
        let focused = session.active_tab().map(|t| t.focused_pane).unwrap_or(0);
        // The drain below only needs the visible pane ids; the tree itself is
        // cloned inside the render block — only on dirty frames (most wakes
        // aren't), and after the drain, so an exit-mutated tree renders fresh
        // this frame instead of one wake late.
        let visible: Vec<u32> = session
            .active_tab()
            .map(|t| t.center.pane_ids())
            .unwrap_or_default();

        // 1. Drain pending PTY output, routed by pane id. Only output from a pane
        //    visible in the active tab dirties the frame; others advance silently.
        //    The drain is budgeted so a chatty pane cannot starve rendering/input.
        let mut disconnected = false;
        let mut budget_exhausted = false;
        let mut drain_stats_chunks = 0;

        loop {
            if drain_stats_chunks >= 64 {
                budget_exhausted = true;
                break;
            }
            match rx.try_recv() {
                Ok(ev) => {
                    drain_stats_chunks += 1;
                    match ev {
                        PaneEvent::Output(id, b) => {
                            if let Some(p) = panes.table.get_mut(&id) {
                                p.feed(&b);
                                // Answer terminal queries (DA/DSR/OSC color,
                                // kitty probes) the app just sent — without a
                                // reply, programs like yazi warn or time out.
                                let resp = {
                                    let emu = p.emulator();
                                    crate::queries::query_responses(&b, emu.cursor(), emu.size())
                                };
                                if !resp.is_empty() {
                                    let _ = p.write_input(&resp);
                                }
                                // Clipboard sets (OSC 52) from inner apps go
                                // VERBATIM to the outer terminal — vim's
                                // "+y inside a pane reaches the system
                                // clipboard like in a plain terminal.
                                let fwd = crate::queries::osc_passthrough(&b);
                                if !fwd.is_empty() {
                                    use std::io::Write;
                                    let mut out = std::io::stdout();
                                    let _ = out.write_all(&fwd);
                                    let _ = out.flush();
                                }
                                if visible.contains(&id) {
                                    dirty = true;
                                }
                            }
                        }
                        PaneEvent::Exit(id) => {
                            panes.table.remove(&id);
                            // The visible yazi drawer died on its own (e.g. its
                            // contained scope hit the memory limit). Clear it,
                            // mark the worktree's drawer closed, and surface why.
                            if drawer == Some(id) {
                                drawer = None;
                                if let Some(dir) =
                                    drawer_home.take().or_else(|| active_cwd(&session))
                                {
                                    let key = superzej_core::util::slugify(&dir.to_string_lossy());
                                    let ddir = superzej_core::util::superzej_dir().join("drawer");
                                    let _ = std::fs::create_dir_all(&ddir);
                                    let _ = std::fs::write(ddir.join(key), "false");
                                }
                                model.status = "Files drawer exited; if image previews \
                                    were enabled it may have hit the drawer memory limit."
                                    .into();
                                dirty = true;
                                continue;
                            }
                            // A pooled (hidden) drawer's yazi exited; just forget it.
                            if drawer_pool.remove_id(id) {
                                dirty = true;
                                continue;
                            }
                            // Pin panes are supervised separately from tab panes: the
                            // supervisor applies the restart policy. (PTY EOF carries no
                            // exit status, so treat death as a failure for policy purposes.)
                            if let Some(inst) = supervisor.instance_of_pane(id) {
                                let name = inst.name.clone();
                                match supervisor.on_exit(id, false) {
                                    crate::pins::RestartDecision::Respawn => {
                                        let active_dir = active_cwd(&session);
                                        let pin = current_config
                                            .pins
                                            .iter()
                                            .find(|p| p.name == name)
                                            .cloned();
                                        if let Some(pin) = pin {
                                            let argv = crate::pins::PinSupervisor::argv(&pin);
                                            let env: Vec<(String, String)> =
                                                crate::pins::PinSupervisor::spawn_env(&pin)
                                                    .into_iter()
                                                    .collect();
                                            let cwd = pin_cwd(&pin, active_dir);
                                            if let Ok(fresh) = panes.spawn_argv_env(
                                                &argv,
                                                Some(&cwd),
                                                &env,
                                                chrome.center,
                                            ) {
                                                supervisor.reattach(&name, fresh);
                                            }
                                        }
                                    }
                                    crate::pins::RestartDecision::Leave => {}
                                }
                                persist_pin_state(&supervisor, &session.id);
                                need_relayout = true;
                                dirty = true;
                                continue;
                            }
                            // Find the owning (group, tab) and either drop the pane from
                            // its split or, if its only shell died, keep the tab and
                            // respawn a fresh shell. Explicit close-pane/worktree actions
                            // remove the pane from the session before the PTY exit event
                            // arrives, so this path is for external child death.
                            let owner = session
                                .iter_tabs()
                                .find(|(_, _, t)| t.center.pane_ids().contains(&id))
                                .map(|(gi, ti, t)| (gi, ti, t.center.pane_ids().len() == 1));
                            if let Some((gi, ti, sole)) = owner {
                                let is_active_tab =
                                    gi == session.active && ti == session.worktrees[gi].active_tab;
                                if sole {
                                    if is_active_tab {
                                        // Worktree dir first, then current_dir, then $HOME.
                                        let cwd = group_cwd(&session.worktrees[gi]).or_else(|| {
                                            std::env::var("HOME").ok().map(std::path::PathBuf::from)
                                        });
                                        match spawn_worktree_shell_pane(
                                            &mut panes,
                                            keymap.config(),
                                            cwd.as_deref(),
                                            chrome.center,
                                        ) {
                                            Ok(fresh) => {
                                                if let Some(tab) = session.tab_mut(gi, ti) {
                                                    replace_single_dead_center_pane(tab, id, fresh);
                                                }
                                                model.status =
                                                    "Pane exited; spawned a fresh shell".into();
                                                need_relayout = true;
                                            }
                                            Err(err) => {
                                                model.status = format!("Respawn failed: {err:#}");
                                            }
                                        }
                                    }
                                } else if let Some(tab) = session.tab_mut(gi, ti) {
                                    tab.center.remove(id);
                                    if tab.focused_pane == id
                                        && let Some(first) = tab.center.pane_ids().first()
                                    {
                                        tab.focused_pane = *first;
                                    }
                                    need_relayout = true;
                                }
                            }
                            dirty = true;
                        }
                    }
                }
                Err(tokio_mpsc::error::TryRecvError::Empty) => break,
                Err(tokio_mpsc::error::TryRecvError::Disconnected) => {
                    disconnected = true;
                    break;
                }
            }
        }
        if disconnected {
            return Ok(());
        }
        if budget_exhausted {
            dirty = true;
        }
        if session.worktrees.is_empty() {
            return Ok(());
        }

        // Hydrated models replace the whole FrameModel; re-apply the loop-owned
        // fields (stats, bars, accent, pins, hints). Stale generations are
        // dropped — a fresh hydration is always in flight for the current one.
        while let Ok((lines, summary)) = tests_rx.try_recv() {
            panel_ui.tests_lines = lines;
            panel_ui.tests_summary = summary;
            panel_ui.tests_running = false;
            panel_ui.tests_scroll = 0;
            dirty = true;
        }

        // Bank preloaded documents (capped — it's a small LRU-ish pool).
        while let Ok((key, doc)) = doc_rx.try_recv() {
            if doc_cache.len() > 64 {
                doc_cache.clear();
            }
            doc_inflight.remove(&key);
            doc_cache.insert(key, doc);
        }

        while let Ok((generation, next_model)) = model_rx.try_recv() {
            if generation != hydration_gen {
                continue;
            }
            // Fresh git data invalidates cached documents (the 2s safety tick
            // sends identical panels, so only real changes clear the cache).
            if next_model.panel != model.panel {
                doc_cache.clear();
            }
            let stats = std::mem::take(&mut model.stats);
            model = next_model;
            model.stats = stats;
            refresh_tab_model(&mut model, &session, &mut sb);
            apply_mode_status(&mut model, mode);
            model.accent = current_config.accent_rgb();
            model.bars = current_config.bars.clone();
            model.stats_icons = current_config.stats.clone();
            let ws = (!session.id.is_empty()).then_some(session.id.as_str());
            model.pins = supervisor.chips(&current_config, ws);
            dirty = true;
        }

        // Fresh stats reading from the ticker thread (top-bar widgets).
        while let Ok(snap) = stats_rx.try_recv() {
            if model.stats != snap {
                model.stats = snap;
                dirty = true;
            }
        }

        // Adopt freshly-registered diff watchers; drop stale ones (the user
        // switched worktrees again before the recursive registration finished).
        while let Ok((path, nw)) = watcher_rx.try_recv() {
            if watched_worktree.as_deref() == Some(path.as_path()) {
                diff_watcher = Some(nw);
            }
        }

        while let Ok(cfg_res) = config_rx.try_recv() {
            match cfg_res {
                Ok(new_cfg) => {
                    keymap = rebuild_keymap(&new_cfg, &session);
                    model.status = keybind_conflict_summary(&new_cfg)
                        .unwrap_or_else(|| "Config reloaded".into());
                    // Live theme reload: colors apply on the next repaint.
                    crate::chrome::set_palette(new_cfg.palette());
                    crate::center::PANE_HPAD.store(
                        new_cfg.theme.pane_padding as usize,
                        std::sync::atomic::Ordering::Relaxed,
                    );
                    model.accent = new_cfg.accent_rgb();
                    model.bars = new_cfg.bars.clone();
                    model.stats_icons = new_cfg.stats.clone();
                    current_config = new_cfg;
                    need_relayout = true;
                }
                Err(e) => {
                    model.status = format!("Config error: {e}");
                }
            }
            dirty = true;
        }

        // Refresh requests arrive event-driven (worktree fs-watch, on-switch
        // kick) and on the safety-net ticker. Coalesce all pending into at most
        // one model + one PR hydrate per wake so a burst of file saves is one
        // refresh. Both run off-thread and pulse the waker when results land.
        let mut want_model_refresh = false;
        let mut want_pr_refresh = false;
        while let Ok(kind) = refresh_rx.try_recv() {
            match kind {
                RefreshKind::Model => want_model_refresh = true,
                RefreshKind::Pr => {
                    want_pr_refresh = true;
                    want_model_refresh = true;
                }
            }
        }
        if want_model_refresh {
            hydration_gen += 1;
            spawn_model_hydration(
                model_tx.clone(),
                hydration_gen,
                session.clone(),
                Some(waker.clone()),
            );
        }
        if want_pr_refresh {
            spawn_pr_cache_refresh(session.clone(), Some(waker.clone()));
        }

        // Ack the focused worktree's activity so its "look at me" dot clears
        // once the user is actually on it. Idempotent; off-thread so the
        // file write never stalls the loop.
        if let Some(name) = session.active_group().map(|g| g.name.clone())
            && last_acked_tab.as_deref() != Some(name.as_str())
        {
            last_acked_tab = Some(name.clone());
            task::spawn_blocking(move || superzej_core::activity::ack(&name));
        }

        // Mirror the focus zone into the render model RIGHT BEFORE rendering —
        // hydrated models replace the whole FrameModel mid-iteration, and
        // mirroring earlier let one frame render with empty keyhints (the
        // bottom bar visibly flashed on every hydration).
        sb.focused = focus.sidebar();
        model.sidebar_focused = focus.sidebar();
        model.panel_focused = focus.panel();
        model.center_focused = focus.center();
        model.key_locked = focus.locked;
        model.zoomed = zoom.is_some();
        model.keyhints = context_hints(&focus, &panel_ui, keymap.config());

        // A drilled-in document (single-file diff / file preview) widens the
        // panel to a reading width; closing it retracts. Central detector so
        // every open/close path is covered without per-call-site wiring.
        if panel_ui.drilled() != panel_expanded {
            panel_expanded = panel_ui.drilled();
            chrome = compute_chrome(
                cols,
                rows,
                want_sidebar,
                want_panel,
                panel_forced,
                panel_expanded,
                sidebar_cols,
                zoom,
                &supervisor,
            );
            need_relayout = true;
            dirty = true;
        }

        // 2. Render if anything changed (diff-flush): all visible panes of the
        //    active tab + the chrome, with the hardware cursor in the focused pane.
        if dirty {
            let frame_t0 = std::time::Instant::now();
            let tree = if zoom == Some(crate::focus::Zone::Center) {
                crate::center::CenterTree::Leaf(focused)
            } else {
                session
                    .active_tab()
                    .map(|t| t.center.clone())
                    .unwrap_or(crate::center::CenterTree::Leaf(0))
            };
            // Layout changes (panel toggles/expansion, zoom) need NO physical
            // clear: `front` mirrors the wire exactly, so the diff repaints
            // precisely the changed cells — clearing here only caused a
            // visible flash. Full repaints remain for real resizes (the
            // terminal scrambles its own content) and scratch re-allocation.
            if scratch.dimensions() != (cols, rows) {
                scratch = Surface::new(cols, rows);
                full_repaint = true;
            }
            crate::chrome::clear_frame(&mut scratch);
            // Card titles: "{program} · {worktree-leaf}" from the spawn argv —
            // best-effort but cheap (no OSC-title capture yet).
            let title_leaf = model
                .worktree
                .rsplit_once('/')
                .map(|(_, l)| l.to_string())
                .unwrap_or_else(|| model.worktree.clone());
            render_tab(
                &mut scratch,
                &chrome,
                &tree,
                focused,
                &model,
                &panel_ui,
                |id| panes.table.get(&id).map(|p| p.emulator()),
                &|id| {
                    panes
                        .table
                        .get(&id)
                        .map(|p| {
                            if title_leaf.is_empty() {
                                p.program().to_string()
                            } else {
                                format!("{} \u{00b7} {}", p.program(), title_leaf)
                            }
                        })
                        .unwrap_or_default()
                },
            );
            // Mouse-selection highlight, clipped to the anchored pane.
            if let Some((sel_pane, sel)) = &mouse_sel
                && let Some((_, _, content)) = tree
                    .layout_framed(chrome.center)
                    .iter()
                    .find(|(id, _, _)| id == sel_pane)
            {
                crate::compositor::overlay_selection(
                    &mut scratch,
                    *content,
                    sel,
                    crate::chrome::col(crate::chrome::S::Panel2),
                );
            }
            if let Some(strip_rect) = chrome.strip {
                let cells: Vec<crate::chrome::StripCell> = supervisor
                    .strip_layout(strip_rect)
                    .into_iter()
                    .filter_map(|(pane, rect)| {
                        supervisor
                            .instance_of_pane(pane)
                            .map(|inst| crate::chrome::StripCell {
                                pane,
                                rect,
                                label: inst.label.clone(),
                                glyph: inst.health.glyph(),
                                focused: false,
                            })
                    })
                    .collect();
                crate::chrome::draw_strip(
                    &mut scratch,
                    strip_rect,
                    &cells,
                    model.accent_or_default(),
                    |id| panes.table.get(&id).map(|p| p.emulator()),
                );
            }
            if let Some(drawer_id) = drawer
                && let Some(p) = panes.table.get(&drawer_id)
            {
                let height = current_config
                    .drawer
                    .height
                    .parse::<usize>()
                    .unwrap_or(20)
                    .min(rows); // cfg.drawer.height equivalent
                let rect = Rect {
                    x: 0,
                    y: rows.saturating_sub(height),
                    cols,
                    rows: height,
                };
                crate::compositor::compose_pane(&mut scratch, p.emulator(), rect);
            }
            let screen = Rect {
                x: 0,
                y: 0,
                cols,
                rows,
            };
            if let Some(pal) = &palette {
                pal.render(&mut scratch, screen);
            }
            let accent = current_config.accent_rgb();
            if cheatsheet {
                let groups = crate::keyhint::cheatsheet_groups(&current_config);
                crate::keyhint::render_cheatsheet(&mut scratch, screen, &groups, &accent);
            } else if !which_key.is_empty() {
                crate::keyhint::render_which_key(
                    &mut scratch,
                    screen,
                    &which_key_prefix,
                    &which_key,
                    &accent,
                );
            }
            if let Some((question, _)) = &pending_delete {
                crate::chrome::draw_confirm(&mut scratch, screen, question);
            }
            // Flush path: diff `scratch` against our own `front` buffer and
            // render the change list directly on the terminal.
            //
            // Deliberately NOT `BufferedTerminal::flush`/`repaint`: termwiz
            // 0.23's `Surface::get_changes` falls back to `repaint_all` on the
            // first flush (seq==0) and whenever its cost heuristic trips, and
            // `repaint_all` collapses every line that merely ENDS with the
            // same trailing background into one ClearToEndOfScreen — with the
            // panel-tinted right column EVERY line ends in `panel` bg, so a
            // repaint paints row 0 and erases all other content. An explicit
            // diff has no such fallback and keeps damage-tracking exact.
            let mut wire: Vec<Change> = Vec::new();
            if full_repaint {
                // Geometry changed since the last flush: reset the baseline
                // so no cell from the previous layout survives (the
                // duplicate-tabbar / doubled-header class of corruption).
                tracing::debug!(target: "szhost::frame", "geometry_changed → full_repaint");
                front = Surface::new(cols, rows);
                let clear = Change::ClearScreen(crate::chrome::col(crate::chrome::S::Bg0));
                let seq = front.add_change(clear.clone());
                front.flush_changes_older_than(seq);
                wire.push(clear);
                full_repaint = false;
            }
            let mut pending = front.diff_screens(&scratch);
            if palette.is_none() && !cheatsheet {
                // The hardware cursor sits in the focused pane's CONTENT rect
                // (inside its frame ring). With no live focused pane (launch
                // splash), hide it so nothing blinks over the wordmark.
                let focused_rect = tree
                    .layout_framed(chrome.center)
                    .into_iter()
                    .find(|(id, _, _)| *id == focused)
                    .map(|(_, _, content)| content);
                if let (Some(rect), Some(p)) = (focused_rect, panes.table.get(&focused)) {
                    let (cur_row, cur_col) = p.emulator().cursor();
                    pending.push(Change::CursorVisibility(
                        termwiz::surface::CursorVisibility::Visible,
                    ));
                    pending.push(Change::CursorPosition {
                        x: Position::Absolute(rect.x + cur_col as usize),
                        y: Position::Absolute(rect.y + cur_row as usize),
                    });
                } else {
                    pending.push(Change::CursorVisibility(
                        termwiz::surface::CursorVisibility::Hidden,
                    ));
                }
            }
            // Keep `front` exactly in sync with what goes on the wire, and
            // trim both change logs (Surface retains them indefinitely).
            let seq = front.add_changes(pending.clone());
            front.flush_changes_older_than(seq);
            let seq = scratch.current_seqno();
            scratch.flush_changes_older_than(seq);
            wire.extend(pending);
            buf.terminal().render(&wire).context("render")?;
            buf.terminal().flush().context("terminal flush")?;
            dirty = false;
            tracing::debug!(
                target: "szhost::frame",
                render_ms = frame_t0.elapsed().as_millis() as u64,
                drain_chunks = drain_stats_chunks,
                "frame flushed"
            );
            if !first_frame_logged {
                first_frame_logged = true;
                tracing::info!(
                    target: "szhost::startup",
                    since_start_ms = start.elapsed().as_millis() as u64,
                    "first frame flushed"
                );
                // Benchmark hook (`just bench`): exit right after the first
                // real frame so hyperfine measures launch → first paint.
                // `run::main` still tears down the alt screen + raw mode.
                if std::env::var_os("SUPERZEJ_BENCH_FIRST_FRAME_EXIT").is_some() {
                    return Ok(());
                }
                // Deferred warms — AFTER the first flush so they never tax
                // launch→first-frame: syntect's lazy sets (first diff
                // drill-in) and the initial worktree's hidden yazi drawer.
                tokio::task::spawn_blocking(superzej_core::diff_highlight::warm);
                dirty = true;
                let _ = waker.wake();
                if keymap.config().drawer.prewarm
                    && keymap.config().drawer.pool_limit > 0
                    && drawer.is_none()
                    && keymap.config().tool_command("yazi").is_some()
                    && let Some(dir) = active_cwd(&session)
                    && !drawer_pool.contains(&dir)
                    && let Some(id) =
                        spawn_yazi_pane(&mut panes, keymap.config(), Some(&dir), chrome.center)
                {
                    drawer_pool.stash(&dir, id, keymap.config().drawer.pool_limit, &mut panes);
                }
            }
        }

        // 3. Block until something happens: a real terminal event, or a
        //    `waker.wake()` from any producer (PTY reader, model/PR hydration,
        //    config watcher, diff fs-watch, refresh ticker) which returns
        //    `InputEvent::Wake`. No timeout → zero idle CPU; we only wake when
        //    there is work, and render the instant it arrives.
        let polled = match pending_input.pop_front() {
            Some(ev) => Ok(Some(ev)),
            None => buf.terminal().poll_input(None),
        };
        match polled {
            Ok(Some(InputEvent::Mouse(m))) => {
                use termwiz::input::MouseButtons;
                // SGR mouse coordinates are 1-based.
                let mx = (m.x as usize).saturating_sub(1);
                let my = (m.y as usize).saturating_sub(1);
                let left = m.mouse_buttons.contains(MouseButtons::LEFT);
                let contains = |r: Rect, x: usize, y: usize| {
                    x >= r.x && x < r.x + r.cols && y >= r.y && y < r.y + r.rows
                };
                let frames = session
                    .active_tab()
                    .map(|t| t.center.layout_framed(chrome.center))
                    .unwrap_or_default();
                let hit_pane = frames
                    .iter()
                    .find(|(_, _, c)| contains(*c, mx, my))
                    .map(|(id, _, c)| (*id, *c));

                // Full terminal support: when the app inside the pane asked
                // for mouse reporting (htop, lazygit, …), forward the event
                // into the pane instead of handling it ourselves. Holding
                // Shift bypasses the app and forces host selection — the
                // convention every terminal uses.
                if let Some((id, content)) = hit_pane
                    && !m.modifiers.contains(Modifiers::SHIFT)
                {
                    let proto = panes.table.get(&id).map(|p| p.emulator().mouse_mode());
                    if let Some((mode, sgr)) = proto
                        && mode != crate::emulator::MouseMode::None
                    {
                        use crate::input::{PaneMouse, encode_mouse};
                        let col = (mx - content.x) as u16;
                        let row = (my - content.y) as u16;
                        let ev = if m.mouse_buttons.contains(MouseButtons::VERT_WHEEL) {
                            if m.mouse_buttons.contains(MouseButtons::WHEEL_POSITIVE) {
                                Some(PaneMouse::WheelUp)
                            } else {
                                Some(PaneMouse::WheelDown)
                            }
                        } else if left && !mouse_left_down {
                            // A press also focuses the pane.
                            focus.zone = crate::focus::Zone::Center;
                            if let Some(tab) = session.active_tab_mut() {
                                tab.focused_pane = id;
                            }
                            mouse_sel = None;
                            dirty = true;
                            Some(PaneMouse::Press(0))
                        } else if left && mouse_left_down {
                            Some(PaneMouse::Drag(0))
                        } else if !left && mouse_left_down {
                            Some(PaneMouse::Release(0))
                        } else {
                            Some(PaneMouse::Move)
                        };
                        if let Some(ev) = ev
                            && let Some(bytes) = encode_mouse(ev, mode, sgr, col, row)
                            && let Some(p) = panes.table.get_mut(&id)
                        {
                            let _ = p.write_input(&bytes);
                        }
                        mouse_left_down = left;
                        mouse_selecting = false;
                        continue;
                    }
                }

                // Wheel over a pane scrolls its scrollback; over the panel /
                // sidebar it scrolls THAT widget (never the terminal behind).
                if m.mouse_buttons.contains(MouseButtons::VERT_WHEEL) {
                    let up = m.mouse_buttons.contains(MouseButtons::WHEEL_POSITIVE);
                    if let Some((id, _)) = hit_pane {
                        if let Some(p) = panes.table.get_mut(&id) {
                            if up {
                                p.scroll_up(3);
                            } else {
                                p.scroll_down(3);
                            }
                            dirty = true;
                        }
                    } else if chrome.panel.is_some_and(|r| contains(r, mx, my)) {
                        scroll_panel(&mut panel_ui, &model, up, 3, panel_doc_rows(&chrome));
                        dirty = true;
                    } else if chrome.sidebar.is_some_and(|r| contains(r, mx, my)) {
                        let visible = SidebarState::visible_len(&model);
                        if up {
                            sb.cursor = sb.cursor.saturating_sub(3);
                        } else if visible > 0 {
                            sb.cursor = (sb.cursor + 3).min(visible - 1);
                        }
                        sb.sync(&mut model);
                        dirty = true;
                    }
                } else if left && !mouse_left_down {
                    // Press: focus whatever is under the cursor. A click on
                    // the dormant center dismisses the launch splash.
                    if center_dormant && contains(chrome.center, mx, my) {
                        center_dormant = false;
                        need_relayout = true;
                    }
                    mouse_sel = None;
                    if let Some((id, content)) = hit_pane {
                        // Interacting with the terminal clears selections
                        // everywhere else (sidebar marks included).
                        if !sb.marked.is_empty() {
                            sb.marked.clear();
                            sb.sync(&mut model);
                        }
                        focus.zone = crate::focus::Zone::Center;
                        if let Some(tab) = session.active_tab_mut() {
                            tab.focused_pane = id;
                        }
                        let cell = ((my - content.y) as u16, (mx - content.x) as u16);
                        mouse_sel = Some((id, crate::copymode::Selection::new(cell)));
                        mouse_selecting = true;
                    } else if contains(chrome.masthead_stats_row(), mx, my) {
                        // Click the top-right stats block to cycle its refresh
                        // rate ([stats] refresh_rates).
                        if mx >= chrome.masthead.cols / 2 {
                            let rates = &current_config.stats.refresh_rates;
                            if !rates.is_empty() {
                                use std::sync::atomic::Ordering;
                                let cur = stats_interval_ms.load(Ordering::Relaxed);
                                let idx = rates
                                    .iter()
                                    .position(|r| ((r * 1000.0) as u64) == cur)
                                    .map(|i| (i + 1) % rates.len())
                                    .unwrap_or(0);
                                let next = rates[idx].max(0.5);
                                stats_interval_ms.store((next * 1000.0) as u64, Ordering::Relaxed);
                                model.status = format!("Stats refresh: {next}s");
                            }
                        }
                    } else if contains(chrome.center_tabs, mx, my) {
                        // Click a tab chip to switch tabs within the worktree.
                        if let Some(i) =
                            crate::chrome::center_tab_hit(&model, chrome.center_tabs, mx)
                            && let Some(g) = session.active_group_mut()
                            && i < g.tabs.len()
                        {
                            g.active_tab = i;
                            focus.zone = crate::focus::Zone::Center;
                            refresh_tab_model(&mut model, &session, &mut sb);
                            need_relayout = true;
                            sync_drawer_persistence(
                                &session,
                                &mut panes,
                                &mut drawer,
                                &mut drawer_pool,
                                &mut drawer_home,
                                keymap.config(),
                                chrome.center,
                            );
                        }
                    } else if let Some(r) = chrome.sidebar.filter(|r| contains(*r, mx, my)) {
                        focus.zone = crate::focus::Zone::Sidebar;
                        sb.focused = true;
                        sb.sync(&mut model);
                        // Rows start two below the header (one blank row).
                        if my > r.y + 1 {
                            let idx = my - r.y - 2;
                            if idx < SidebarState::visible_len(&model) {
                                sb.cursor = idx;
                                if m.modifiers.contains(Modifiers::CTRL) {
                                    // Ctrl+click: toggle the multi-select mark.
                                    if !sb.marked.remove(&idx) {
                                        sb.marked.insert(idx);
                                    }
                                    sb.sync(&mut model);
                                } else {
                                    sb.sync(&mut model);
                                    if let Some(t) = sb.cursor_target(&model) {
                                        activate_row_target(
                                            t,
                                            &mut session,
                                            &mut model,
                                            &mut sb,
                                            &mut panes,
                                            &mut drawer,
                                            &mut drawer_pool,
                                            &mut drawer_home,
                                            keymap.config(),
                                            chrome.center,
                                        );
                                        need_relayout = true;
                                    }
                                }
                            }
                        }
                    } else if let Some(r) = chrome.panel.filter(|r| contains(*r, mx, my)) {
                        focus.zone = crate::focus::Zone::Panel;
                        if my == r.y {
                            // Click a DIFF|FILES|PR|CHECKS segment to switch.
                            if let Some(t) = crate::chrome::panel_tab_hit(r, mx) {
                                panel_ui.tab = t;
                                panel_ui.diff_view = crate::panel::DiffView::FileList;
                                panel_ui.files_preview = false;
                                if t == crate::panel::PanelTab::Files {
                                    refresh_files_tree(&mut panel_ui, &session);
                                }
                                if t == crate::panel::PanelTab::Tests {
                                    panel_ui.tests_cmd = crate::panel::detect_test_command(
                                        &active_tab_path(&session),
                                    );
                                }
                            }
                        } else if my > r.y + 1
                            && my
                                < crate::chrome::panel_split(r).0.y
                                    + crate::chrome::panel_split(r).0.rows
                            && panel_ui.tab == crate::panel::PanelTab::Diff
                            && panel_ui.diff_view == crate::panel::DiffView::FileList
                        {
                            // Click a file row to move the cursor (body starts
                            // below the title + blank rows; the SANDBOXES
                            // section is not clickable rows).
                            let idx = my - r.y - 2;
                            if idx < model.panel.files.len() {
                                panel_ui.diff_cursor = idx;
                            }
                        }
                    }
                    dirty = true;
                } else if left && mouse_left_down && mouse_selecting {
                    // Drag: extend the selection, clamped to the anchored pane.
                    if let Some((id, sel)) = mouse_sel.as_mut()
                        && let Some((_, _, content)) = frames.iter().find(|(fid, _, _)| fid == id)
                    {
                        let row = my.clamp(content.y, content.y + content.rows.saturating_sub(1))
                            - content.y;
                        let col = mx.clamp(content.x, content.x + content.cols.saturating_sub(1))
                            - content.x;
                        sel.cursor = (row as u16, col as u16);
                        dirty = true;
                    }
                } else if !left && mouse_left_down {
                    // Release: auto-copy a non-empty selection (zellij-style).
                    mouse_selecting = false;
                    match mouse_sel.as_ref() {
                        Some((_, sel)) if sel.anchor == sel.cursor => {
                            mouse_sel = None; // a plain click — nothing to copy
                        }
                        Some((id, sel)) => {
                            if let Some(p) = panes.table.get(id) {
                                let text = crate::copymode::extract(p.emulator(), sel);
                                if !text.trim().is_empty() {
                                    use std::io::Write;
                                    let mut out = std::io::stdout();
                                    let _ = out.write_all(&crate::copymode::osc52(&text));
                                    let _ = out.flush();
                                    model.status = format!("Copied {} chars", text.chars().count());
                                }
                            }
                        }
                        None => {}
                    }
                    dirty = true;
                }
                mouse_left_down = left;
            }
            Ok(Some(InputEvent::Key(k))) => {
                // Split mouse-report fragments masquerade as keys; drop them
                // before they reach any dispatch or pane.
                if residue.swallow(&k.key, k.modifiers) {
                    continue;
                }
                let k = normalize_key(k);
                // Any real keypress dismisses the launch splash (chrome chords
                // still dispatch below; a plain pane-bound key is swallowed
                // once — the shell materializes on the next loop turn).
                // `Wake`/`Resized` never reach here, so background hydration
                // can't dismiss it.
                if center_dormant {
                    tracing::debug!(target: "szhost::frame", key = ?k.key, "dormant dismissed by key");
                    center_dormant = false;
                    need_relayout = true;
                    dirty = true;
                }
                // Typing clears any lingering mouse selection highlight.
                if mouse_sel.take().is_some() {
                    dirty = true;
                }
                // Modal: the cheatsheet swallows all keys; Esc / Alt-? closes it.
                if cheatsheet {
                    let close = matches!(k.key, KeyCode::Escape)
                        || (k.key == KeyCode::Char('?') && k.modifiers.contains(Modifiers::ALT));
                    if close {
                        cheatsheet = false;
                        dirty = true;
                    }
                    continue;
                }
                // Modal: a pending destructive delete swallows the next key.
                if let Some((_, targets)) = pending_delete.take() {
                    if matches!(k.key, KeyCode::Char('y') | KeyCode::Char('Y')) {
                        model.status = delete_groups(&mut session, &mut panes, targets);
                        sb.marked.clear();
                        refresh_tab_model(&mut model, &session, &mut sb);
                        need_relayout = true;
                        sync_drawer_persistence(
                            &session,
                            &mut panes,
                            &mut drawer,
                            &mut drawer_pool,
                            &mut drawer_home,
                            keymap.config(),
                            chrome.center,
                        );
                    } else {
                        model.status = "Delete cancelled".into();
                    }
                    dirty = true;
                    continue;
                }
                let mut forced_palette_action: Option<crate::keymap::Action> = None;
                // Modal: when the palette is open it captures all keys.
                if let Some(p) = palette.as_mut() {
                    // Agent-picker mode: the palette is choosing what to run in a
                    // just-created worktree tab. The tab already materialized a
                    // shell, so "shell" (and Escape) keep the live pane —
                    // respawning it would needlessly reload the terminal. Only a
                    // real agent choice replaces the pane.
                    if pending_agent.is_some() {
                        match k.key {
                            KeyCode::Escape => {
                                if let Some(pending) = pending_agent.as_mut()
                                    && pending.choosing_sandbox
                                {
                                    let backend = keymap.config().sandbox.default_backend.as_str();
                                    if let Ok(db) = superzej_core::db::Db::open() {
                                        let _ = db.set_worktree_sandbox(&pending.worktree, backend);
                                    }
                                    pending.choosing_sandbox = false;
                                    palette = Some(crate::palette::Palette::new(
                                        build_agent_palette(keymap.config()),
                                    ));
                                } else {
                                    pending_agent = None;
                                    palette = None;
                                    refresh_tab_model(&mut model, &session, &mut sb);
                                    need_relayout = true;
                                }
                            }
                            KeyCode::Enter => {
                                let choice = p
                                    .selected_item()
                                    .map(|i| i.key.clone())
                                    .unwrap_or_else(|| "shell".to_string());
                                if let Some(backend) = choice.strip_prefix("sandbox:")
                                    && let Some(pending) = pending_agent.as_mut()
                                {
                                    if let Ok(db) = superzej_core::db::Db::open() {
                                        let _ = db.set_worktree_sandbox(&pending.worktree, backend);
                                    }
                                    pending.choosing_sandbox = false;
                                    palette = Some(crate::palette::Palette::new(
                                        build_agent_palette(keymap.config()),
                                    ));
                                    dirty = true;
                                    continue;
                                }
                                if let Some(pending) = pending_agent.as_ref()
                                    && choice != "shell"
                                {
                                    launch_agent_into_tab(
                                        keymap.config(),
                                        &mut session,
                                        &mut panes,
                                        pending,
                                        &choice,
                                        chrome.center,
                                    );
                                }
                                pending_agent = None;
                                palette = None;
                                refresh_tab_model(&mut model, &session, &mut sb);
                                need_relayout = true;
                            }
                            KeyCode::UpArrow => p.move_up(),
                            KeyCode::DownArrow => p.move_down(),
                            KeyCode::Backspace => p.backspace(),
                            KeyCode::Char(c) if !k.modifiers.contains(Modifiers::CTRL) => {
                                p.push_char(c)
                            }
                            _ => {}
                        }
                        dirty = true;
                        continue;
                    }
                    match k.key {
                        KeyCode::Escape => palette = None,
                        KeyCode::Enter => {
                            if let Some(item) = p.selected_item() {
                                let key = item.key.clone();
                                // Record frecency so the choice floats up next time.
                                if let Ok(db) = superzej_core::db::Db::open() {
                                    let _ = db.bump_palette_usage(&key);
                                }
                                if key == "quit" {
                                    return Ok(());
                                }
                                if let Some(payload) = key.strip_prefix("wt:") {
                                    let outgoing = session_pane_ids(&session);
                                    if let Some((repo_path, tab_name)) = payload.split_once('\t')
                                        && let Ok(db) = superzej_core::db::Db::open()
                                        && switch_to_workspace_tab(
                                            &mut session,
                                            &db,
                                            repo_path,
                                            tab_name,
                                        )
                                        .unwrap_or(false)
                                    {
                                        // Reap the outgoing workspace's panes
                                        // (persisted-id collisions otherwise).
                                        for id in outgoing {
                                            panes.table.remove(&id);
                                        }
                                        if let Some(id) = drawer.take() {
                                            panes.table.remove(&id);
                                        }
                                        for id in drawer_pool.drain_ids() {
                                            panes.table.remove(&id);
                                        }
                                        refresh_tab_model(&mut model, &session, &mut sb);
                                        need_relayout = true;
                                        sync_drawer_persistence(
                                            &session,
                                            &mut panes,
                                            &mut drawer,
                                            &mut drawer_pool,
                                            &mut drawer_home,
                                            keymap.config(),
                                            chrome.center,
                                        );
                                    }
                                } else if let Some(repo_path) = key.strip_prefix("repo:") {
                                    let outgoing = session_pane_ids(&session);
                                    if let Ok(db) = superzej_core::db::Db::open()
                                        && session.switch_to_workspace(repo_path, &db).is_ok()
                                    {
                                        for id in outgoing {
                                            panes.table.remove(&id);
                                        }
                                        if let Some(id) = drawer.take() {
                                            panes.table.remove(&id);
                                        }
                                        for id in drawer_pool.drain_ids() {
                                            panes.table.remove(&id);
                                        }
                                        refresh_tab_model(&mut model, &session, &mut sb);
                                        need_relayout = true;
                                        sync_drawer_persistence(
                                            &session,
                                            &mut panes,
                                            &mut drawer,
                                            &mut drawer_pool,
                                            &mut drawer_home,
                                            keymap.config(),
                                            chrome.center,
                                        );
                                    }
                                } else if let Some(name) = key.strip_prefix("tab:")
                                    && let Some(i) =
                                        session.worktrees.iter().position(|g| g.name == name)
                                {
                                    session.switch_to(i);
                                    refresh_tab_model(&mut model, &session, &mut sb);
                                    need_relayout = true;
                                    sync_drawer_persistence(
                                        &session,
                                        &mut panes,
                                        &mut drawer,
                                        &mut drawer_pool,
                                        &mut drawer_home,
                                        keymap.config(),
                                        chrome.center,
                                    );
                                } else if let Some(n) = key
                                    .strip_prefix("summon-pin-")
                                    .and_then(|s| s.parse::<usize>().ok())
                                {
                                    if let Some(s) = summon_pin(
                                        n,
                                        &current_config,
                                        &session,
                                        &mut panes,
                                        &mut supervisor,
                                        chrome.center,
                                    ) {
                                        model.status = s;
                                    }
                                    chrome = compute_chrome(
                                        cols,
                                        rows,
                                        want_sidebar,
                                        want_panel,
                                        panel_forced,
                                        panel_expanded,
                                        sidebar_cols,
                                        zoom,
                                        &supervisor,
                                    );
                                    need_relayout = true;
                                } else if key == "toggle-strip" {
                                    supervisor.toggle_strip();
                                    chrome = compute_chrome(
                                        cols,
                                        rows,
                                        want_sidebar,
                                        want_panel,
                                        panel_forced,
                                        panel_expanded,
                                        sidebar_cols,
                                        zoom,
                                        &supervisor,
                                    );
                                    need_relayout = true;
                                } else if let Some(action) = crate::keymap::Action::from_key(&key) {
                                    forced_palette_action = Some(action);
                                } else if let Some(idx) = keymap
                                    .custom_actions()
                                    .iter()
                                    .position(|action| action.name == key)
                                {
                                    forced_palette_action =
                                        Some(crate::keymap::Action::Custom(idx as u16));
                                }
                                // Other command keys are also reachable via their
                                // keybind; their in-palette dispatch lands with the
                                // unified action table.
                            }
                            palette = None;
                        }
                        KeyCode::UpArrow => p.move_up(),
                        KeyCode::DownArrow => p.move_down(),
                        KeyCode::Backspace => p.backspace(),
                        KeyCode::Char(c) if !k.modifiers.contains(Modifiers::CTRL) => {
                            p.push_char(c)
                        }
                        _ => {}
                    }
                    if forced_palette_action.is_none() {
                        dirty = true;
                        continue;
                    }
                }
                // The Ctrl+g keybind lock: while locked, every key except
                // Ctrl+g itself goes straight to the focused pane.
                if focus.locked {
                    if k.key == KeyCode::Char('g') && k.modifiers == Modifiers::CTRL {
                        focus.locked = false;
                        model.status = "Keybinds unlocked".into();
                        dirty = true;
                        continue;
                    }
                    let target_pane = drawer.unwrap_or(focused);
                    if let Some(p) = panes.table.get_mut(&target_pane) {
                        let app = p.emulator().application_cursor();
                        if let Some(bytes) = crate::input::key_bytes_mode(&k.key, k.modifiers, app)
                        {
                            p.write_input(&bytes)?;
                        }
                    }
                    continue;
                }
                // Sidebar zone: unmodified keys drive the tree (j/k, Enter, /,
                // …). Ctrl/Alt chords fall through to the keymap so the
                // spatial focus moves and tab switches still work from here.
                if forced_palette_action.is_none()
                    && focus.sidebar()
                    && !k.modifiers.contains(Modifiers::CTRL)
                    && !k.modifiers.contains(Modifiers::ALT)
                {
                    match sb.handle_key(&k.key, k.modifiers, &mut model, &session) {
                        SidebarOutcome::NotHandled => { /* fall through to keymap */ }
                        SidebarOutcome::Redraw => {
                            dirty = true;
                            continue;
                        }
                        SidebarOutcome::Defocus => {
                            focus.zone = crate::focus::Zone::Center;
                            sb.focused = false;
                            sb.menu = None;
                            sb.sync(&mut model);
                            dirty = true;
                            continue;
                        }
                        SidebarOutcome::Relayout => {
                            sidebar_cols = sb.width.unwrap_or(layout::SIDEBAR_COLS);
                            chrome = compute_chrome(
                                cols,
                                rows,
                                want_sidebar,
                                want_panel,
                                panel_forced,
                                panel_expanded,
                                sidebar_cols,
                                zoom,
                                &supervisor,
                            );
                            need_relayout = true;
                            dirty = true;
                            continue;
                        }
                        SidebarOutcome::Activate(target) => {
                            activate_row_target(
                                target,
                                &mut session,
                                &mut model,
                                &mut sb,
                                &mut panes,
                                &mut drawer,
                                &mut drawer_pool,
                                &mut drawer_home,
                                keymap.config(),
                                chrome.center,
                            );
                            need_relayout = true;
                            dirty = true;
                            continue;
                        }
                        SidebarOutcome::DeleteGroups(targets) => {
                            let (targets, skipped_home) =
                                deletable_group_targets(&session, targets);
                            let names: Vec<String> = targets
                                .iter()
                                .filter_map(|&g| session.worktrees.get(g))
                                .map(|g| g.name.clone())
                                .collect();
                            if targets.is_empty() {
                                model.status = if skipped_home > 0 {
                                    "Root workspace cannot be deleted".into()
                                } else {
                                    "No worktree selected".into()
                                };
                                dirty = true;
                                continue;
                            }
                            if current_config.confirm_delete {
                                pending_delete = Some((
                                    format!(
                                        "Delete {} worktree(s) from disk? ({})",
                                        names.len(),
                                        names.join(", ")
                                    ),
                                    targets,
                                ));
                            } else {
                                model.status = delete_groups(&mut session, &mut panes, targets);
                                sb.marked.clear();
                                refresh_tab_model(&mut model, &session, &mut sb);
                                need_relayout = true;
                                sync_drawer_persistence(
                                    &session,
                                    &mut panes,
                                    &mut drawer,
                                    &mut drawer_pool,
                                    &mut drawer_home,
                                    keymap.config(),
                                    chrome.center,
                                );
                            }
                            dirty = true;
                            continue;
                        }
                        SidebarOutcome::CloseGroups(targets) => {
                            let (mut targets, skipped_home) =
                                deletable_group_targets(&session, targets);
                            // Close from the highest index down so earlier
                            // indices stay valid as groups are removed.
                            targets.sort_unstable_by(|a, b| b.cmp(a));
                            for gi in targets {
                                if gi < session.worktrees.len() {
                                    for tab in &session.worktrees[gi].tabs {
                                        for id in tab.center.pane_ids() {
                                            panes.table.remove(&id);
                                        }
                                    }
                                    session.switch_to(gi);
                                    session.close_active_group();
                                }
                            }
                            if skipped_home > 0 {
                                model.status = "Root workspace cannot be closed".into();
                            }
                            sb.marked.clear();
                            refresh_tab_model(&mut model, &session, &mut sb);
                            need_relayout = true;
                            sync_drawer_persistence(
                                &session,
                                &mut panes,
                                &mut drawer,
                                &mut drawer_pool,
                                &mut drawer_home,
                                keymap.config(),
                                chrome.center,
                            );
                            dirty = true;
                            continue;
                        }
                    }
                }
                // Panel zone: unmodified keys drive the Diff/PR/Checks widgets.
                if forced_palette_action.is_none()
                    && focus.panel()
                    && !k.modifiers.contains(Modifiers::CTRL)
                    && !k.modifiers.contains(Modifiers::ALT)
                {
                    use crate::panel::{DiffView, PanelNav, PanelTab, panel_nav_key};
                    if let Some(nav) = panel_nav_key(&k.key, panel_ui.tab, panel_ui.diff_view) {
                        // Files-tab helpers: the selected (visible) entry and
                        // the drawer-open primitive shared by bat/editor/yazi.
                        let selected_file_entry = |ui: &crate::panel::PanelUi| {
                            crate::panel::visible_file_indices(&ui.files, &ui.files_collapsed)
                                .get(ui.files_cursor)
                                .map(|&vi| ui.files[vi].clone())
                        };
                        let panel_body_rows = panel_doc_rows(&chrome);
                        match nav {
                            PanelNav::SelectTab(t) => {
                                panel_ui.tab = t;
                                panel_ui.diff_view = DiffView::FileList;
                                panel_ui.files_preview = false;
                                if t == PanelTab::Files {
                                    refresh_files_tree(&mut panel_ui, &session);
                                }
                                if t == PanelTab::Tests {
                                    panel_ui.tests_cmd = crate::panel::detect_test_command(
                                        &active_tab_path(&session),
                                    );
                                }
                            }
                            PanelNav::CycleTab => {
                                panel_ui.tab = panel_ui.tab.next();
                                panel_ui.diff_view = DiffView::FileList;
                                panel_ui.files_preview = false;
                                if panel_ui.tab == PanelTab::Files {
                                    refresh_files_tree(&mut panel_ui, &session);
                                }
                                if panel_ui.tab == PanelTab::Tests {
                                    panel_ui.tests_cmd = crate::panel::detect_test_command(
                                        &active_tab_path(&session),
                                    );
                                }
                            }
                            PanelNav::Up | PanelNav::Down => {
                                // One clamped scroll step per QUEUED key:
                                // held-key repeats are drained and applied in
                                // a single render pass, so releasing the key
                                // stops the motion instantly (no backlog
                                // inertia). Documents stop at the last line,
                                // lists at the last row.
                                let up = nav == PanelNav::Up;
                                let (repeat, leftover) = drain_key_repeats(&k, || {
                                    buf.terminal()
                                        .poll_input(Some(std::time::Duration::ZERO))
                                        .ok()
                                        .flatten()
                                });
                                if let Some(ev) = leftover {
                                    pending_input.push_back(ev);
                                }
                                scroll_panel(&mut panel_ui, &model, up, repeat, panel_body_rows);
                                // Hover preload: warm the diff + bat docs for
                                // the file now under the cursor and the next
                                // one, so Enter / Shift+J land instantly.
                                if !panel_ui.drilled() {
                                    preload_hover(
                                        &panel_ui,
                                        &model.panel.files,
                                        &active_tab_path(&session),
                                        &doc_cache,
                                        &mut doc_inflight,
                                        &doc_tx,
                                        &waker,
                                        keymap.config().tool_command("bat"),
                                    );
                                }
                            }
                            PanelNav::Back => {
                                // One press: back to the terminal. The
                                // panel-leave detector performs the full reset.
                                focus.zone = crate::focus::Zone::Center;
                            }
                            PanelNav::Enter if panel_ui.tab == PanelTab::Files => {
                                if let Some(entry) = selected_file_entry(&panel_ui) {
                                    if entry.is_dir {
                                        // Accordion: toggle the folder.
                                        if !panel_ui.files_collapsed.remove(&entry.path) {
                                            panel_ui.files_collapsed.insert(entry.path);
                                        }
                                    } else {
                                        // Open the file as an in-panel
                                        // syntax-highlighted preview
                                        // (cache-first; neighbors preload).
                                        show_file_preview(
                                            &mut panel_ui,
                                            &active_tab_path(&session),
                                            &mut doc_cache,
                                            &doc_tx,
                                            &waker,
                                            keymap.config().tool_command("bat"),
                                        );
                                    }
                                }
                            }
                            PanelNav::Enter => {
                                // Drill into the selected file's diff
                                // (cache-first; neighbors preload behind it).
                                show_diff_doc(
                                    &mut panel_ui,
                                    &model.panel.files,
                                    &active_tab_path(&session),
                                    &mut doc_cache,
                                    &doc_tx,
                                    &waker,
                                );
                            }
                            PanelNav::Open | PanelNav::OpenEditorPane => {
                                // Open the selected file in the editor — `o`
                                // in a fresh center tab, `e` in a split pane
                                // (the bottom drawer is no longer used).
                                let path = if panel_ui.tab == PanelTab::Files {
                                    selected_file_entry(&panel_ui)
                                        .filter(|e| !e.is_dir)
                                        .map(|e| e.path)
                                } else {
                                    model
                                        .panel
                                        .files
                                        .get(panel_ui.diff_cursor)
                                        .map(|f| f.path.clone())
                                };
                                if let Some(path) = path {
                                    let editor = keymap
                                        .config()
                                        .tool_command("editor")
                                        .unwrap_or("${EDITOR:-vi} .")
                                        .trim();
                                    let editor = editor.strip_suffix(" .").unwrap_or(editor);
                                    let quoted = path.replace('\'', r"'\''");
                                    let cmd = format!("{editor} '{quoted}'");
                                    let cwd = active_cwd(&session);
                                    if nav == PanelNav::Open {
                                        open_command_tab(
                                            &mut session,
                                            &mut panes,
                                            &cmd,
                                            cwd.as_deref(),
                                            chrome.center,
                                        );
                                    } else {
                                        open_command_pane(
                                            &mut session,
                                            &mut panes,
                                            focused,
                                            &cmd,
                                            cwd.as_deref(),
                                            chrome.center,
                                        );
                                    }
                                    focus.zone = crate::focus::Zone::Center;
                                    refresh_tab_model(&mut model, &session, &mut sb);
                                    need_relayout = true;
                                }
                            }
                            PanelNav::NextDoc | PanelNav::PrevDoc => {
                                let fwd = nav == PanelNav::NextDoc;
                                let wt = active_tab_path(&session);
                                match panel_ui.tab {
                                    PanelTab::Diff => {
                                        let len = model.panel.files.len();
                                        if len > 0 {
                                            let cur = panel_ui.diff_cursor.min(len - 1);
                                            panel_ui.diff_cursor = if fwd {
                                                (cur + 1).min(len - 1)
                                            } else {
                                                cur.saturating_sub(1)
                                            };
                                            show_diff_doc(
                                                &mut panel_ui,
                                                &model.panel.files,
                                                &wt,
                                                &mut doc_cache,
                                                &doc_tx,
                                                &waker,
                                            );
                                        }
                                    }
                                    PanelTab::Files => {
                                        if !step_file_cursor(&mut panel_ui, fwd) {
                                            // nothing to walk to
                                        } else {
                                            if panel_ui.files_cursor < panel_ui.files_scroll {
                                                panel_ui.files_scroll = panel_ui.files_cursor;
                                            } else if panel_ui.files_cursor
                                                >= panel_ui.files_scroll + panel_body_rows
                                            {
                                                panel_ui.files_scroll =
                                                    panel_ui.files_cursor + 1 - panel_body_rows;
                                            }
                                            show_file_preview(
                                                &mut panel_ui,
                                                &wt,
                                                &mut doc_cache,
                                                &doc_tx,
                                                &waker,
                                                keymap.config().tool_command("bat"),
                                            );
                                        }
                                    }
                                    _ => {}
                                }
                            }
                            PanelNav::OpenTab | PanelNav::OpenPane => {
                                // Promote the current artifact into the
                                // center: the file's diff (Diff tab) or its
                                // bat view (Files tab), as a tab or a split.
                                let shq = |p: &str| p.replace('\'', r"'\''");
                                let command = match panel_ui.tab {
                                    PanelTab::Diff => {
                                        model.panel.files.get(panel_ui.diff_cursor).map(|f| {
                                            format!(
                                                "git -c color.ui=always diff HEAD -- '{}' \
                                                 | ${{PAGER:-less -R}}",
                                                shq(&f.path)
                                            )
                                        })
                                    }
                                    PanelTab::Files => selected_file_entry(&panel_ui)
                                        .filter(|e| !e.is_dir)
                                        .map(|e| {
                                            let pager = keymap
                                                .config()
                                                .tool_command("bat")
                                                .unwrap_or("bat --paging=always")
                                                .trim()
                                                .to_string();
                                            format!("{pager} '{}'", shq(&e.path))
                                        }),
                                    _ => None,
                                };
                                if let Some(cmd) = command {
                                    let cwd = active_cwd(&session);
                                    if nav == PanelNav::OpenTab {
                                        open_command_tab(
                                            &mut session,
                                            &mut panes,
                                            &cmd,
                                            cwd.as_deref(),
                                            chrome.center,
                                        );
                                    } else {
                                        open_command_pane(
                                            &mut session,
                                            &mut panes,
                                            focused,
                                            &cmd,
                                            cwd.as_deref(),
                                            chrome.center,
                                        );
                                    }
                                    focus.zone = crate::focus::Zone::Center;
                                    refresh_tab_model(&mut model, &session, &mut sb);
                                    need_relayout = true;
                                }
                            }
                            PanelNav::OpenExternal => {
                                // Hand the file to the system opener, detached.
                                if let Some(entry) =
                                    selected_file_entry(&panel_ui).filter(|e| !e.is_dir)
                                {
                                    let abs = active_tab_path(&session).join(&entry.path);
                                    let _ = std::process::Command::new("xdg-open")
                                        .arg(abs)
                                        .stdin(std::process::Stdio::null())
                                        .stdout(std::process::Stdio::null())
                                        .stderr(std::process::Stdio::null())
                                        .spawn();
                                    model.status = format!("Opened {} externally", entry.name);
                                }
                            }
                            PanelNav::RevealDrawer => {
                                // Yazi drawer anchored at the selection's dir.
                                if let Some(entry) = selected_file_entry(&panel_ui) {
                                    let wt = active_tab_path(&session);
                                    let dir = if entry.is_dir {
                                        wt.join(&entry.path)
                                    } else {
                                        wt.join(&entry.path)
                                            .parent()
                                            .map(|p| p.to_path_buf())
                                            .unwrap_or(wt)
                                    };
                                    if let Some(cwd) = active_cwd(&session) {
                                        hide_drawer_into_pool(
                                            &mut drawer,
                                            &mut drawer_pool,
                                            &mut drawer_home,
                                            &cwd,
                                            keymap.config(),
                                            &mut panes,
                                        );
                                    } else if let Some(id) = drawer.take() {
                                        panes.table.remove(&id);
                                    }
                                    drawer = spawn_yazi_pane(
                                        &mut panes,
                                        keymap.config(),
                                        Some(dir.as_path()),
                                        chrome.center,
                                    );
                                    drawer_home = Some(dir.clone());
                                }
                            }
                            PanelNav::Rerun if panel_ui.tab == PanelTab::Tests => {
                                if panel_ui.tests_cmd.is_none() {
                                    panel_ui.tests_cmd = crate::panel::detect_test_command(
                                        &active_tab_path(&session),
                                    );
                                }
                                if let Some((_, cmd)) = panel_ui.tests_cmd.clone()
                                    && !panel_ui.tests_running
                                {
                                    panel_ui.tests_running = true;
                                    spawn_test_run(
                                        tests_tx.clone(),
                                        waker.clone(),
                                        active_tab_path(&session),
                                        cmd,
                                    );
                                }
                            }
                            // PR-tab actions (merge/approve/create/rerun) land
                            // with the gh wiring; navigation is the zone's job.
                            _ => {}
                        }
                        dirty = true;
                        continue;
                    }
                }
                // Global/mode chords are intercepted by the keymap; everything
                // else is forwarded to the focused pane.
                let input_key = crate::sequence::Key::modified(k.key, k.modifiers);
                // The program running in the focused (or drawer) pane keys the
                // per-program overlay + remap layers.
                let focused_program = panes
                    .table
                    .get(&drawer.unwrap_or(focused))
                    .map(|p| p.program().to_string())
                    .unwrap_or_default();
                // Per-program host-action overlay intercepts before the mode
                // matcher; otherwise fall through to the normal keymap dispatch.
                let dispatch = if let Some(action) = forced_palette_action.take() {
                    keymap.reset();
                    crate::sequence::MatchResult::Matched(action)
                } else {
                    match keymap.program_action(&focused_program, &input_key) {
                        Some(action) => {
                            keymap.reset();
                            crate::sequence::MatchResult::Matched(action)
                        }
                        None => keymap.dispatch(mode, input_key.clone()),
                    }
                };
                match dispatch {
                    crate::sequence::MatchResult::Matched(action) => {
                        use crate::keymap::Action;
                        // A completed chord clears any pending which-key popup.
                        which_key.clear();
                        match action {
                            Action::Cheatsheet => {
                                cheatsheet = true;
                            }
                            Action::SwitchMode(next) => {
                                mode = next;
                                keymap.reset();
                                apply_mode_status(&mut model, mode);
                            }
                            Action::ToggleKeyLock => {
                                focus.locked = true;
                                model.status = "Keybinds locked — Ctrl+g to unlock".into();
                            }
                            Action::Custom(idx) => {
                                if let Some(ca) = keymap.custom_actions().get(idx as usize) {
                                    let mut cmd =
                                        std::process::Command::new(superzej_core::util::shell());
                                    cmd.arg("-c").arg(&ca.run);
                                    if ca.floating {
                                        let cwd = active_cwd(&session);
                                        if let Some(dir) = cwd {
                                            cmd.current_dir(dir);
                                        }
                                        let _ = cmd.spawn();
                                    } else {
                                        // A non-floating run should spawn in the current center/pane
                                        // or split, but for the spike we'll just shell out similarly
                                        // or spawn a new pane. For now, spawn floating.
                                        let _ = cmd.spawn();
                                    }
                                }
                            }
                            Action::Quit => return Ok(()),
                            Action::OpenPalette => {
                                if let Ok(db) = superzej_core::db::Db::open() {
                                    palette = Some(crate::palette::Palette::new(build_palette(
                                        &session,
                                        &db,
                                        &current_config,
                                    )));
                                }
                            }
                            Action::ToggleDrawer => {
                                if drawer.is_some() {
                                    // Reap the drawer pane
                                    if let Some(cwd) = active_cwd(&session) {
                                        // Keep-alive: hide, don't kill — the
                                        // yazi position survives reopening.
                                        hide_drawer_into_pool(
                                            &mut drawer,
                                            &mut drawer_pool,
                                            &mut drawer_home,
                                            &cwd,
                                            keymap.config(),
                                            &mut panes,
                                        );
                                        let key =
                                            superzej_core::util::slugify(&cwd.to_string_lossy());
                                        let dir =
                                            superzej_core::util::superzej_dir().join("drawer");
                                        let _ = std::fs::write(dir.join(key), "false");
                                    } else if let Some(id) = drawer.take() {
                                        panes.table.remove(&id);
                                    }
                                } else {
                                    // Show the worktree's drawer (pooled pane
                                    // when pre-warmed — instant).
                                    let cwd = active_cwd(&session);
                                    if let Some(d) = cwd.as_deref() {
                                        show_yazi_drawer(
                                            &mut panes,
                                            &mut drawer,
                                            &mut drawer_pool,
                                            &mut drawer_home,
                                            keymap.config(),
                                            d,
                                            chrome.center,
                                        );
                                    }
                                    if let Some(dir) = cwd {
                                        let key =
                                            superzej_core::util::slugify(&dir.to_string_lossy());
                                        let dir =
                                            superzej_core::util::superzej_dir().join("drawer");
                                        let _ = std::fs::create_dir_all(&dir);
                                        let _ = std::fs::write(dir.join(key), "true");
                                    }
                                }
                            }
                            Action::ToggleSidebar => {
                                want_sidebar = !want_sidebar;
                                chrome = compute_chrome(
                                    cols,
                                    rows,
                                    want_sidebar,
                                    want_panel,
                                    panel_forced,
                                    panel_expanded,
                                    sidebar_cols,
                                    zoom,
                                    &supervisor,
                                );
                                if !want_sidebar && focus.sidebar() {
                                    focus.zone = crate::focus::Zone::Center;
                                    sb.focused = false;
                                    sb.sync(&mut model);
                                }
                                need_relayout = true;
                            }
                            Action::TogglePanel => {
                                panel_auto_revealed = None;
                                // Toggle on what the user SEES: if the panel
                                // is auto-hidden by the width threshold,
                                // "toggle" means force it visible (readable
                                // width even on small screens); a visible
                                // panel hides and clears the override.
                                if chrome.panel.is_some() {
                                    want_panel = false;
                                    panel_forced = false;
                                } else {
                                    want_panel = true;
                                    panel_forced = cols < layout::PANEL_MIN_COLS;
                                }
                                chrome = compute_chrome(
                                    cols,
                                    rows,
                                    want_sidebar,
                                    want_panel,
                                    panel_forced,
                                    panel_expanded,
                                    sidebar_cols,
                                    zoom,
                                    &supervisor,
                                );
                                if !want_panel && focus.panel() {
                                    focus.zone = crate::focus::Zone::Center;
                                }
                                need_relayout = true;
                            }
                            Action::FocusSidebar => {
                                if !want_sidebar {
                                    want_sidebar = true;
                                    chrome = compute_chrome(
                                        cols,
                                        rows,
                                        want_sidebar,
                                        want_panel,
                                        panel_forced,
                                        panel_expanded,
                                        sidebar_cols,
                                        zoom,
                                        &supervisor,
                                    );
                                    need_relayout = true;
                                }
                                // Take keyboard focus, land on the active worktree.
                                focus.zone = crate::focus::Zone::Sidebar;
                                sb.focused = true;
                                sb.rebuild(&mut model, &session);
                            }
                            Action::FocusPanel => {
                                panel_auto_revealed = None;
                                if chrome.panel.is_none() {
                                    // Jumping to a hidden panel reveals it,
                                    // forcing past the width threshold.
                                    want_panel = true;
                                    panel_forced = cols < layout::PANEL_MIN_COLS;
                                    chrome = compute_chrome(
                                        cols,
                                        rows,
                                        want_sidebar,
                                        want_panel,
                                        panel_forced,
                                        panel_expanded,
                                        sidebar_cols,
                                        zoom,
                                        &supervisor,
                                    );
                                    need_relayout = true;
                                }
                                focus.zone = crate::focus::Zone::Panel;
                            }
                            Action::NextTab => {
                                session.next_tab();
                                refresh_tab_model(&mut model, &session, &mut sb);
                                need_relayout = true;
                                sync_drawer_persistence(
                                    &session,
                                    &mut panes,
                                    &mut drawer,
                                    &mut drawer_pool,
                                    &mut drawer_home,
                                    keymap.config(),
                                    chrome.center,
                                );
                            }
                            Action::PrevTab => {
                                session.prev_tab();
                                refresh_tab_model(&mut model, &session, &mut sb);
                                need_relayout = true;
                                sync_drawer_persistence(
                                    &session,
                                    &mut panes,
                                    &mut drawer,
                                    &mut drawer_pool,
                                    &mut drawer_home,
                                    keymap.config(),
                                    chrome.center,
                                );
                            }
                            Action::NextWorktree | Action::PrevWorktree => {
                                // Step in the sidebar's display order so the
                                // motion matches what the user sees.
                                let order = sidebar_worktree_order(&model);
                                let pos = order.iter().position(|&g| g == session.active);
                                match (order.len(), pos) {
                                    (n, Some(p)) if n > 1 => {
                                        let next = if action == Action::NextWorktree {
                                            (p + 1) % n
                                        } else {
                                            (p + n - 1) % n
                                        };
                                        session.switch_to(order[next]);
                                    }
                                    // No usable display order (filtered away,
                                    // hydrating): fall back to session order.
                                    _ if action == Action::NextWorktree => session.next_worktree(),
                                    _ => session.prev_worktree(),
                                }
                                refresh_tab_model(&mut model, &session, &mut sb);
                                need_relayout = true;
                                sync_drawer_persistence(
                                    &session,
                                    &mut panes,
                                    &mut drawer,
                                    &mut drawer_pool,
                                    &mut drawer_home,
                                    keymap.config(),
                                    chrome.center,
                                );
                            }
                            Action::NewPane => {
                                // Zellij-style: split the focused pane along
                                // its longer dimension.
                                let dir = session
                                    .active_tab()
                                    .and_then(|t| {
                                        t.center
                                            .layout(chrome.center)
                                            .into_iter()
                                            .find(|(id, _)| *id == focused)
                                            .map(|(_, r)| smart_split_dir(r.cols, r.rows))
                                    })
                                    .unwrap_or(crate::center::Dir::Row);
                                let cwd = active_cwd(&session);
                                match spawn_worktree_shell_pane(
                                    &mut panes,
                                    keymap.config(),
                                    cwd.as_deref(),
                                    chrome.center,
                                ) {
                                    Ok(new) => {
                                        if let Some(tab) = session.active_tab_mut() {
                                            if tab.center.split(focused, dir, new) {
                                                tab.focused_pane = new;
                                                need_relayout = true;
                                            } else {
                                                panes.table.remove(&new);
                                            }
                                        } else {
                                            panes.table.remove(&new);
                                        }
                                    }
                                    Err(e) => {
                                        model.status = format!("Pane spawn failed: {e}");
                                    }
                                }
                            }
                            Action::CycleTheme => {
                                // Live theme cycle: presets resolve through
                                // the config so [theme.colors] customizations
                                // ride along. Set `[theme] preset` to persist.
                                let presets = superzej_core::theme::PRESETS;
                                theme_idx = (theme_idx + 1) % presets.len();
                                let name = presets[theme_idx];
                                crate::chrome::set_palette(
                                    current_config.palette_with_preset(name),
                                );
                                model.status =
                                    format!("Theme: {name} (set [theme] preset to keep)");
                            }
                            Action::ToggleZoom => {
                                zoom = if zoom.is_some() {
                                    None
                                } else {
                                    Some(focus.zone)
                                };
                                model.status = if zoom.is_some() {
                                    "Zoomed — Ctrl+Alt+z to restore".into()
                                } else {
                                    String::new()
                                };
                                chrome = compute_chrome(
                                    cols,
                                    rows,
                                    want_sidebar,
                                    want_panel,
                                    panel_forced,
                                    panel_expanded,
                                    sidebar_cols,
                                    zoom,
                                    &supervisor,
                                );
                                need_relayout = true;
                            }
                            Action::SplitDown | Action::SplitRight => {
                                let dir = if action == Action::SplitDown {
                                    crate::center::Dir::Col
                                } else {
                                    crate::center::Dir::Row
                                };
                                let cwd = active_cwd(&session);
                                let new = match spawn_worktree_shell_pane(
                                    &mut panes,
                                    keymap.config(),
                                    cwd.as_deref(),
                                    chrome.center,
                                ) {
                                    Ok(id) => id,
                                    Err(e) => {
                                        // Survivable: report, don't exit the loop.
                                        model.status = format!("Pane spawn failed: {e}");
                                        dirty = true;
                                        continue;
                                    }
                                };
                                if let Some(tab) = session.active_tab_mut() {
                                    if tab.center.split(focused, dir, new) {
                                        tab.focused_pane = new;
                                        need_relayout = true;
                                    } else {
                                        // target not found (shouldn't happen); reap the pane
                                        panes.table.remove(&new);
                                    }
                                } else {
                                    panes.table.remove(&new);
                                }
                            }
                            Action::FocusLeft
                            | Action::FocusRight
                            | Action::FocusUp
                            | Action::FocusDown => {
                                use crate::center::Move;
                                use crate::focus::{FocusMove, Zone};
                                let mv = match action {
                                    Action::FocusLeft => Move::Left,
                                    Action::FocusRight => Move::Right,
                                    Action::FocusUp => Move::Up,
                                    _ => Move::Down,
                                };
                                // One spatial graph: panes first, then across
                                // the chrome seams (sidebar ← center → panel).
                                let pane_layout = session
                                    .active_tab()
                                    .map(|t| t.center.layout(chrome.center))
                                    .unwrap_or_default();
                                let ctx = crate::focus::RouteCtx {
                                    sidebar_visible: want_sidebar && chrome.sidebar.is_some(),
                                    panel_visible: want_panel && chrome.panel.is_some(),
                                    layout: &pane_layout,
                                    focused_pane: focused,
                                };
                                match crate::focus::route(focus.zone, mv, &ctx) {
                                    FocusMove::CenterPane(n) => {
                                        if let Some(tab) = session.active_tab_mut() {
                                            tab.focused_pane = n;
                                        }
                                    }
                                    FocusMove::Enter(zone) => {
                                        focus.zone = zone;
                                        if zone == Zone::Sidebar {
                                            sb.focused = true;
                                            sb.rebuild(&mut model, &session);
                                        }
                                    }
                                    FocusMove::WithinZone(delta) => {
                                        if focus.sidebar() {
                                            let visible = SidebarState::visible_len(&model);
                                            if delta < 0 {
                                                sb.cursor = sb.cursor.saturating_sub(1);
                                            } else if visible > 0 {
                                                sb.cursor = (sb.cursor + 1).min(visible - 1);
                                            }
                                            sb.sync(&mut model);
                                        } else if focus.panel() {
                                            if delta < 0 {
                                                panel_ui.diff_cursor =
                                                    panel_ui.diff_cursor.saturating_sub(1);
                                            } else {
                                                let max = model.panel.files.len().saturating_sub(1);
                                                panel_ui.diff_cursor =
                                                    (panel_ui.diff_cursor + 1).min(max);
                                            }
                                        }
                                    }
                                    FocusMove::None => {
                                        // Ctrl+→ at the center's right edge
                                        // with the panel hidden: pop it up at
                                        // its normal width and focus it; the
                                        // Panel-leave detector restores the
                                        // saved visibility when navigated off.
                                        if mv == Move::Right
                                            && focus.zone == Zone::Center
                                            && chrome.panel.is_none()
                                        {
                                            panel_auto_revealed = Some((want_panel, panel_forced));
                                            want_panel = true;
                                            panel_forced = cols < layout::PANEL_MIN_COLS;
                                            chrome = compute_chrome(
                                                cols,
                                                rows,
                                                want_sidebar,
                                                want_panel,
                                                panel_forced,
                                                panel_expanded,
                                                sidebar_cols,
                                                zoom,
                                                &supervisor,
                                            );
                                            need_relayout = true;
                                            focus.zone = Zone::Panel;
                                        }
                                    }
                                }
                            }
                            Action::NewWorkspace | Action::SwitchWorkspace => {
                                if let Ok(db) = superzej_core::db::Db::open()
                                    && let Some(target) = palette
                                        .as_ref()
                                        .and_then(|p| p.selected_item())
                                        .map(|i| i.key.clone())
                                {
                                    let repo_path =
                                        target.strip_prefix("repo:").unwrap_or(&target).to_string();
                                    let outgoing = session_pane_ids(&session);
                                    if session.switch_to_workspace(&repo_path, &db).is_ok() {
                                        for id in outgoing {
                                            panes.table.remove(&id);
                                        }
                                        if let Some(id) = drawer.take() {
                                            panes.table.remove(&id);
                                        }
                                        for id in drawer_pool.drain_ids() {
                                            panes.table.remove(&id);
                                        }
                                        refresh_tab_model(&mut model, &session, &mut sb);
                                        need_relayout = true;
                                        sync_drawer_persistence(
                                            &session,
                                            &mut panes,
                                            &mut drawer,
                                            &mut drawer_pool,
                                            &mut drawer_home,
                                            keymap.config(),
                                            chrome.center,
                                        );
                                    }
                                }
                            }
                            Action::NewWorktree => {
                                // Create a real git worktree off the active group's repo,
                                // add a `{slug}/{branch}` group for it, then open the agent
                                // picker — its selection launches into the new worktree.
                                // From the sidebar, Alt+w applies to the
                                // SELECTED workspace (e.g. WASHU), not
                                // whichever worktree happens to be active.
                                let sidebar_repo = focus
                                    .sidebar()
                                    .then(|| sb.selected_row(&model))
                                    .flatten()
                                    .map(|r| r.workspace_slug.clone())
                                    .and_then(|slug| {
                                        model
                                            .sidebar_workspaces
                                            .iter()
                                            .find(|(s, _, _, _)| *s == slug)
                                            .map(|(_, _, _, p)| p.clone())
                                    })
                                    .filter(|p| !p.is_empty());
                                let src_wt = sidebar_repo.unwrap_or_else(|| {
                                    session
                                        .active_group()
                                        .map(|g| g.path.clone())
                                        .unwrap_or_default()
                                });
                                let repo_root = (!src_wt.is_empty())
                                    .then(|| superzej_core::repo::main_worktree(Path::new(&src_wt)))
                                    .flatten()
                                    .or_else(|| {
                                        std::env::current_dir()
                                            .ok()
                                            .and_then(|c| superzej_core::repo::main_worktree(&c))
                                    });
                                if let Some(root) = repo_root {
                                    if let Some(nw) = create_local_worktree(keymap.config(), &root)
                                    {
                                        session.add_group(crate::session::WorktreeGroup::new(
                                            nw.tab.clone(),
                                            crate::session::GroupKind::Branch,
                                            nw.path.clone(),
                                        ));
                                        refresh_tab_model(&mut model, &session, &mut sb);
                                        need_relayout = true;
                                        pending_agent = Some(PendingAgent {
                                            tab: nw.tab,
                                            worktree: nw.path,
                                            branch: nw.branch,
                                            choosing_sandbox: true,
                                        });
                                        palette = Some(crate::palette::Palette::new(
                                            build_sandbox_palette(keymap.config()),
                                        ));
                                    }
                                } else {
                                    superzej_core::msg::warn(
                                        "new-worktree: not inside a git repository",
                                    );
                                }
                            }
                            Action::NewTab => {
                                // A fresh tab WITHIN the active worktree.
                                if let Some(g) = session.active_group_mut() {
                                    g.add_tab();
                                }
                                refresh_tab_model(&mut model, &session, &mut sb);
                                need_relayout = true;
                            }
                            Action::CloseTab => {
                                // Close the active tab; if it was the home group's last tab,
                                // refuse so the root workspace cannot disappear.
                                if session
                                    .active_group()
                                    .map(|g| {
                                        g.kind == crate::session::GroupKind::Home
                                            && g.tabs.len() <= 1
                                    })
                                    .unwrap_or(false)
                                {
                                    model.status = "Root workspace cannot be closed".into();
                                    dirty = true;
                                    continue;
                                }
                                match session.close_active_tab() {
                                    crate::session::CloseResult::Tab(tab) => {
                                        for id in tab.center.pane_ids() {
                                            panes.table.remove(&id);
                                        }
                                    }
                                    crate::session::CloseResult::Group(g) => {
                                        for tab in &g.tabs {
                                            for id in tab.center.pane_ids() {
                                                panes.table.remove(&id);
                                            }
                                        }
                                    }
                                    crate::session::CloseResult::Nothing => {}
                                }
                                refresh_tab_model(&mut model, &session, &mut sb);
                                need_relayout = true;
                            }
                            Action::CloseWorktree => {
                                // Close the active worktree group; never close the root/home checkout.
                                if session
                                    .active_group()
                                    .map(|g| g.kind == crate::session::GroupKind::Home)
                                    .unwrap_or(false)
                                {
                                    model.status = "Root workspace cannot be closed".into();
                                    dirty = true;
                                    continue;
                                }
                                if let Some(g) = session.active_group() {
                                    for tab in &g.tabs {
                                        for id in tab.center.pane_ids() {
                                            panes.table.remove(&id);
                                        }
                                    }
                                }
                                session.close_active_group();
                                refresh_tab_model(&mut model, &session, &mut sb);
                                need_relayout = true;
                            }
                            Action::ScrollUp | Action::ScrollDown => {
                                let half = (chrome.center.rows / 2).max(1);
                                if let Some(p) = panes.table.get_mut(&focused) {
                                    if action == Action::ScrollUp {
                                        p.scroll_up(half);
                                    } else {
                                        p.scroll_down(half);
                                    }
                                }
                            }
                            Action::CopyPane => {
                                // Copy the focused pane's visible text to the system
                                // clipboard via OSC 52 (out-of-band to the outer term).
                                if let Some(p) = panes.table.get(&focused) {
                                    let emu = p.emulator();
                                    let sel = crate::copymode::whole(emu);
                                    let text = crate::copymode::extract(emu, &sel);
                                    use std::io::Write;
                                    let mut out = std::io::stdout();
                                    let _ = out.write_all(&crate::copymode::osc52(&text));
                                    let _ = out.flush();
                                }
                            }
                            Action::Lazygit | Action::Editor | Action::Diff => {
                                // Tools open in a fresh center tab — a real
                                // working surface, not the bottom drawer.
                                let cwd = active_cwd(&session);
                                let tool_name = match action {
                                    Action::Lazygit => "lazygit",
                                    Action::Editor => "editor",
                                    Action::Diff => "diff",
                                    _ => unreachable!(),
                                };
                                if let Some(cmd_str) = keymap.config().tool_command(tool_name) {
                                    let cmd = cmd_str.to_string();
                                    open_command_tab(
                                        &mut session,
                                        &mut panes,
                                        &cmd,
                                        cwd.as_deref(),
                                        chrome.center,
                                    );
                                    focus.zone = crate::focus::Zone::Center;
                                    refresh_tab_model(&mut model, &session, &mut sb);
                                    need_relayout = true;
                                }
                            }
                            Action::Yazi => {
                                // Direct bind for yazi: always replace the visible drawer
                                // with a fresh, contained yazi pane for the active worktree
                                // (routed through spawn_yazi_pane so it inherits the same
                                // systemd-run memory/swap/CPU bound as the toggle path).
                                if let Some(id) = drawer.take() {
                                    panes.table.remove(&id);
                                    drawer_home = None;
                                }
                                let cwd = active_cwd(&session);
                                if let Some(id) = spawn_yazi_pane(
                                    &mut panes,
                                    keymap.config(),
                                    cwd.as_deref(),
                                    chrome.center,
                                ) {
                                    drawer = Some(id);
                                    drawer_home = cwd;
                                }
                            }
                            Action::SummonPin(n) => {
                                let status = summon_pin(
                                    n as usize,
                                    &current_config,
                                    &session,
                                    &mut panes,
                                    &mut supervisor,
                                    chrome.center,
                                );
                                if let Some(s) = status {
                                    model.status = s;
                                }
                                persist_pin_state(&supervisor, &session.id);
                                chrome = compute_chrome(
                                    cols,
                                    rows,
                                    want_sidebar,
                                    want_panel,
                                    panel_forced,
                                    panel_expanded,
                                    sidebar_cols,
                                    zoom,
                                    &supervisor,
                                );
                                need_relayout = true;
                            }
                            Action::ToggleStrip => {
                                supervisor.toggle_strip();
                                chrome = compute_chrome(
                                    cols,
                                    rows,
                                    want_sidebar,
                                    want_panel,
                                    panel_forced,
                                    panel_expanded,
                                    sidebar_cols,
                                    zoom,
                                    &supervisor,
                                );
                                need_relayout = true;
                            }
                            Action::GrowStrip | Action::ShrinkStrip => {
                                let delta = if action == Action::GrowStrip {
                                    0.05
                                } else {
                                    -0.05
                                };
                                supervisor.adjust_ratio(delta);
                                chrome = compute_chrome(
                                    cols,
                                    rows,
                                    want_sidebar,
                                    want_panel,
                                    panel_forced,
                                    panel_expanded,
                                    sidebar_cols,
                                    zoom,
                                    &supervisor,
                                );
                                need_relayout = true;
                            }
                            Action::PromotePin => {
                                // Promote the focused center pane into the strip. The
                                // pane keeps its process; it leaves the tab's tree.
                                let label = session
                                    .active_group()
                                    .map(|g| g.name.clone())
                                    .unwrap_or_default();
                                let removed = session
                                    .active_tab_mut()
                                    .map(|t| t.center.remove(focused))
                                    .unwrap_or(false);
                                if removed {
                                    if let Some(tab) = session.active_tab_mut()
                                        && let Some(first) = tab.center.pane_ids().first().copied()
                                    {
                                        tab.focused_pane = first;
                                    }
                                    let name = format!("promoted-{focused}");
                                    supervisor.promote(
                                        &name,
                                        &label,
                                        crate::pins::PinPlacement::Strip,
                                        focused,
                                    );
                                    model.status = format!("Promoted pane to strip: {label}");
                                    persist_pin_state(&supervisor, &session.id);
                                    chrome = compute_chrome(
                                        cols,
                                        rows,
                                        want_sidebar,
                                        want_panel,
                                        panel_forced,
                                        panel_expanded,
                                        sidebar_cols,
                                        zoom,
                                        &supervisor,
                                    );
                                    need_relayout = true;
                                } else {
                                    model.status =
                                        "Promote: can't promote the sole pane of a tab".into();
                                }
                            }
                            Action::Unpin => {
                                // Unpin the first live strip pin (or the focused one if a
                                // strip pane is focused), reaping its process.
                                let target = supervisor
                                    .instance_of_pane(focused)
                                    .map(|i| i.name.clone())
                                    .or_else(|| {
                                        supervisor
                                            .instances()
                                            .iter()
                                            .find(|i| i.pane.is_some())
                                            .map(|i| i.name.clone())
                                    });
                                if let Some(name) = target {
                                    if let Some(pane) = supervisor.unpin(&name) {
                                        panes.table.remove(&pane);
                                    }
                                    model.status = format!("Unpinned {name}");
                                    persist_pin_state(&supervisor, &session.id);
                                    chrome = compute_chrome(
                                        cols,
                                        rows,
                                        want_sidebar,
                                        want_panel,
                                        panel_forced,
                                        panel_expanded,
                                        sidebar_cols,
                                        zoom,
                                        &supervisor,
                                    );
                                    need_relayout = true;
                                } else {
                                    model.status = "Unpin: no live pin".into();
                                }
                            }
                            // New/switch worktree+workspace and tool floats: recognized
                            // and consumed; they land with the sandbox::enter_argv spawn
                            // + branch/repo picker wiring.
                            _ => {}
                        }
                        dirty = true;
                        continue;
                    }
                    crate::sequence::MatchResult::Pending => {
                        // Show the which-key popup of next-key candidates.
                        which_key_prefix = crate::keyhint::key_hint(&input_key);
                        which_key =
                            crate::keyhint::which_key_rows(&keymap.pending_continuations(mode));
                        model.status = format!("{} mode   awaiting next key…", mode.as_str());
                        dirty = true;
                        continue;
                    }
                    crate::sequence::MatchResult::None => {
                        // No match (and not pending): dismiss any which-key popup.
                        which_key.clear();
                    }
                }
                // Keys the sidebar/panel didn't claim must never leak into the
                // terminal while one of them owns the keyboard.
                if !crate::focus::forwards_to_pane(focus.zone, drawer.is_some()) {
                    keymap.reset();
                    dirty = true;
                    continue;
                }
                // Per-program key-injection remap: an unclaimed chord is rewritten
                // into the program's own keys before forwarding. Falls back to the
                // raw keystroke when no remap matches.
                let remapped: Option<Vec<u8>> = keymap
                    .program_remap(&focused_program, &input_key)
                    .map(|keys| {
                        keys.iter()
                            .filter_map(|key| key_bytes(&key.code, key.mods))
                            .flatten()
                            .collect()
                    });
                let target_pane = drawer.unwrap_or(focused);
                let app_cursor = panes
                    .table
                    .get(&target_pane)
                    .map(|p| p.emulator().application_cursor())
                    .unwrap_or(false);
                let bytes = remapped
                    .or_else(|| crate::input::key_bytes_mode(&k.key, k.modifiers, app_cursor));
                if let Some(bytes) = bytes
                    && let Some(p) = panes.table.get_mut(&target_pane)
                {
                    p.write_input(&bytes)?;
                    keymap.reset();
                    // Typing into the terminal clears selections elsewhere
                    // (sidebar multi-select marks).
                    if !sb.marked.is_empty() {
                        sb.marked.clear();
                        sb.sync(&mut model);
                        dirty = true;
                    }
                }
            }
            Ok(Some(InputEvent::Resized { rows: r, cols: c })) => {
                rows = r;
                cols = c;
                chrome = compute_chrome(
                    cols,
                    rows,
                    want_sidebar,
                    want_panel,
                    panel_forced,
                    panel_expanded,
                    sidebar_cols,
                    zoom,
                    &supervisor,
                );
                need_relayout = true;
                buf.resize(cols, rows);
                let _ = buf
                    .terminal()
                    .set_screen_size(termwiz::terminal::ScreenSize {
                        rows,
                        cols,
                        xpixel: 0,
                        ypixel: 0,
                    });
                // ANY resize scrambles the physical screen (terminals reflow/
                // clip during the transition), even when the geometry lands
                // back on the previous size — e.g. a quick 160→120→160 drag
                // coalesced into one wake leaves `chrome == last_chrome` and
                // the scratch dimensions unchanged, so without this the diff
                // against `front` would repaint nothing over the garbage.
                full_repaint = true;
                dirty = true;
            }
            Ok(Some(InputEvent::Paste(s))) => {
                if !crate::focus::forwards_to_pane(focus.zone, drawer.is_some()) {
                    model.status = "Paste ignored (terminal not focused)".into();
                    dirty = true;
                    continue;
                }
                let target_pane = drawer.unwrap_or(focused);
                if let Some(p) = panes.table.get_mut(&target_pane) {
                    // Honor bracketed paste when the app requested it, so
                    // editors don't auto-indent pasted blocks.
                    if p.emulator().bracketed_paste() {
                        p.write_input(b"\x1b[200~")?;
                        p.write_input(s.as_bytes())?;
                        p.write_input(b"\x1b[201~")?;
                    } else {
                        p.write_input(s.as_bytes())?;
                    }
                    keymap.reset();
                }
            }
            Ok(_) | Err(_) => {}
        }
    }
}

#[allow(dead_code)]
fn render_before_pty_drain(dirty: bool) -> bool {
    dirty
}

#[allow(dead_code)]
fn remap_warmed_tab_ids(tab: &mut crate::session::Tab, focus: u32, pairs: &[(u32, u32)]) -> bool {
    let leaves = tab.center.pane_ids();
    if pairs.len() != leaves.len() {
        return false;
    }
    let mut map = std::collections::HashMap::new();
    for (old, new) in pairs {
        map.insert(*old, *new);
    }
    for old in &leaves {
        if !map.contains_key(old) {
            return false;
        }
    }
    tab.center.remap(&mut |old| map[&old]);
    if let Some(&new) = map.get(&focus) {
        tab.focused_pane = new;
    } else {
        tab.focused_pane = *map.values().next().unwrap_or(&0);
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::center::CenterTree;
    use crate::hydrate::build_model;
    use crate::session::{GroupKind, Session, WorktreeGroup};

    /// Tests that set `XDG_STATE_HOME` race each other (the env is process
    /// global); serialize them. Poisoning is fine to ignore — the env is
    /// re-set by the next holder either way.
    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn one_tab_session() -> Session {
        Session {
            id: "s1".into(),
            worktrees: vec![WorktreeGroup::new("app/home", GroupKind::Home, "/tmp/app")],
            active: 0,
        }
    }

    fn two_worktree_session() -> Session {
        Session {
            id: "s1".into(),
            worktrees: vec![
                WorktreeGroup::new("app/home", GroupKind::Home, "/tmp/app"),
                WorktreeGroup::new("app/feat", GroupKind::Branch, "/tmp/app-feat"),
            ],
            active: 0,
        }
    }

    #[test]
    fn center_context_hints_include_close_tab_and_split_controls() {
        let cfg = superzej_core::config::Config::default();
        let focus = crate::focus::FocusState::default();
        let panel = crate::panel::PanelUi::default();
        let hints = context_hints(&focus, &panel, &cfg);

        assert!(hints.contains("Alt-X close tab"), "hints were {hints}");
        assert!(hints.contains("Alt-p smart split"), "hints were {hints}");
        assert!(hints.contains("Alt-n split↓"), "hints were {hints}");
        assert!(hints.contains("Alt-N split→"), "hints were {hints}");
    }

    #[test]
    fn center_context_hints_follow_keybind_overrides() {
        let mut cfg = superzej_core::config::Config::default();
        cfg.keybinds.insert("close-tab".into(), "Ctrl Alt x".into());
        let focus = crate::focus::FocusState::default();
        let panel = crate::panel::PanelUi::default();
        let hints = context_hints(&focus, &panel, &cfg);

        assert!(hints.contains("Ctrl-Alt-x close tab"), "hints were {hints}");
        assert!(!hints.contains("Alt-X close tab"), "hints were {hints}");
    }

    #[test]
    fn contain_yazi_argv_wraps_scope_with_drawer_limits() {
        let cfg = superzej_core::config::Config::default();
        let argv = contain_yazi_argv(&cfg, vec!["yazi".into()], true);

        assert_eq!(argv[0], "systemd-run");
        assert!(argv.contains(&"--user".to_string()));
        assert!(argv.contains(&"--scope".to_string()));
        assert!(argv.contains(&"--collect".to_string()));
        assert!(argv.contains(&"MemoryMax=2G".to_string()));
        assert!(argv.contains(&"MemorySwapMax=512M".to_string()));
        assert!(argv.contains(&"CPUQuota=200%".to_string()));
        // The wrapped command follows the `--` separator.
        let sep = argv.iter().position(|a| a == "--").unwrap();
        assert_eq!(&argv[sep + 1..], &["yazi".to_string()]);
    }

    #[test]
    fn contain_yazi_argv_omits_empty_limits_and_can_disable() {
        let mut cfg = superzej_core::config::Config::default();
        cfg.drawer.memory_swap_max.clear();
        cfg.drawer.cpu_quota.clear();
        let argv = contain_yazi_argv(&cfg, vec!["yazi".into()], true);
        assert_eq!(argv[0], "systemd-run");
        assert!(argv.contains(&"MemoryMax=2G".to_string()));
        assert!(!argv.iter().any(|a| a.starts_with("MemorySwapMax=")));
        assert!(!argv.iter().any(|a| a.starts_with("CPUQuota=")));

        // Disabled, missing systemd-run, or an already-wrapped sandbox argv all
        // pass the command through untouched.
        cfg.drawer.contain = false;
        assert_eq!(
            contain_yazi_argv(&cfg, vec!["yazi".into()], true),
            vec!["yazi"]
        );
        cfg.drawer.contain = true;
        assert_eq!(
            contain_yazi_argv(&cfg, vec!["yazi".into()], false),
            vec!["yazi"]
        );
        let nested = vec!["systemd-run".to_string(), "--user".into(), "--pty".into()];
        assert_eq!(contain_yazi_argv(&cfg, nested.clone(), true), nested);
    }

    #[test]
    fn drawer_pool_respects_zero_limit_and_evicts_oldest() {
        let (tx, _rx) = tokio_mpsc::channel::<PaneEvent>(1024);
        let mut panes = Panes::new(tx);
        let mut pool = DrawerPool::default();
        let a = std::path::Path::new("/tmp/a");
        let b = std::path::Path::new("/tmp/b");

        // limit 0 = no pooling; the just-hidden pane is torn down immediately.
        pool.stash(a, 1, 0, &mut panes);
        assert!(!pool.contains(a));

        // limit 1 keeps only the most recent; stashing b evicts a.
        pool.stash(a, 1, 1, &mut panes);
        assert!(pool.contains(a));
        pool.stash(b, 2, 1, &mut panes);
        assert!(!pool.contains(a));
        assert!(pool.contains(b));
        assert_eq!(pool.take(b), Some(2));
        assert!(!pool.contains(b));

        // remove_id forgets a pooled drawer whose yazi exited on its own.
        pool.stash(a, 3, 2, &mut panes);
        assert!(pool.remove_id(3));
        assert!(!pool.remove_id(3));
    }

    /// A SidebarState whose `persist` writes to a temp DB scope rather than the
    /// user DB — set via XDG_STATE_HOME guarded by the test itself is avoided;
    /// instead these tests exercise only in-memory state transitions and the
    /// rebuilt row visibility (persistence is covered by db.rs::ui_state tests).
    fn focused_state(model: &mut FrameModel, session: &Session) -> SidebarState {
        let mut sb = SidebarState {
            focused: true,
            ..Default::default()
        };
        sb.rebuild(model, session);
        sb
    }

    fn press(
        sb: &mut SidebarState,
        ch: char,
        model: &mut FrameModel,
        session: &Session,
    ) -> SidebarOutcome {
        sb.handle_key(&KeyCode::Char(ch), Modifiers::NONE, model, session)
    }

    #[test]
    fn sidebar_filter_hides_nonmatching_rows() {
        let session = two_worktree_session();
        let mut model = build_initial_model(&session);
        model.sidebar_workspaces = vec![("app".into(), "app".into(), "repo".into(), String::new())];
        let mut sb = focused_state(&mut model, &session);

        press(&mut sb, '/', &mut model, &session);
        for c in "feat".chars() {
            press(&mut sb, c, &mut model, &session);
        }
        let visible: Vec<String> = model
            .sidebar_rows
            .iter()
            .filter(|r| r.visible)
            .map(|r| r.label.clone())
            .collect();
        assert!(visible.contains(&"feat".to_string()));
        assert!(!visible.contains(&"home".to_string()));
    }

    #[test]
    fn sidebar_enter_activates_cursor_row() {
        let session = two_worktree_session();
        let mut model = build_initial_model(&session);
        model.sidebar_workspaces = vec![("app".into(), "app".into(), "repo".into(), String::new())];
        let mut sb = focused_state(&mut model, &session);
        // Rows: app(ws), home, feat. Move to feat and Enter → activate it.
        press(&mut sb, 'j', &mut model, &session);
        press(&mut sb, 'j', &mut model, &session);
        let out = sb.handle_key(&KeyCode::Enter, Modifiers::NONE, &mut model, &session);
        match out {
            SidebarOutcome::Activate(crate::sidebar::RowTarget::Tab(gi, ti)) => {
                assert_eq!(session.worktrees[gi].name, "app/feat");
                assert_eq!(ti, 0);
            }
            _ => panic!("expected Activate"),
        }
        // Digit keys are no longer a hidden quick-jump (no numbers shown).
        assert!(matches!(
            press(&mut sb, '3', &mut model, &session),
            SidebarOutcome::NotHandled
        ));
    }

    #[test]
    fn sidebar_multiselect_marks_and_bulk_close_targets_marked() {
        let session = two_worktree_session();
        let mut model = build_initial_model(&session);
        model.sidebar_workspaces = vec![("app".into(), "app".into(), "repo".into(), String::new())];
        let mut sb = focused_state(&mut model, &session);
        // Move to the home worktree row (index 1) and mark it.
        press(&mut sb, 'j', &mut model, &session);
        press(&mut sb, ' ', &mut model, &session);
        assert!(model.sidebar_marked.contains(&1));
        // Move to feat (index 2) and mark it too.
        press(&mut sb, 'j', &mut model, &session);
        press(&mut sb, ' ', &mut model, &session);
        let out = sb.handle_key(&KeyCode::Char('X'), Modifiers::NONE, &mut model, &session);
        match out {
            SidebarOutcome::CloseGroups(t) => assert_eq!(t.len(), 2),
            _ => panic!("expected CloseGroups"),
        }
    }

    #[test]
    fn sidebar_width_adjust_clamps_and_relayouts() {
        // Persisting width opens the global DB; redirect it to a temp dir so the
        let _env = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        // test never touches the user's state (mirrors the other DB tests here).
        let state_home = std::env::temp_dir().join(format!("sz-host-width-{}", std::process::id()));
        // SAFETY: test is single-threaded; sets/clears an XDG var around calls.
        unsafe { std::env::set_var("XDG_STATE_HOME", &state_home) };

        let session = one_tab_session();
        let mut model = build_initial_model(&session);
        let mut sb = focused_state(&mut model, &session);
        // Narrow past the minimum: clamps at SIDEBAR_MIN_WIDTH.
        for _ in 0..20 {
            let _ = press(&mut sb, '<', &mut model, &session);
        }
        assert_eq!(sb.width, Some(crate::layout::SIDEBAR_MIN_WIDTH));
        let out = press(&mut sb, '>', &mut model, &session);
        assert!(matches!(out, SidebarOutcome::Relayout));

        // SAFETY: test is single-threaded.
        unsafe { std::env::remove_var("XDG_STATE_HOME") };
        let _ = std::fs::remove_dir_all(&state_home);
    }

    #[test]
    fn sidebar_escape_defocuses() {
        let session = one_tab_session();
        let mut model = build_initial_model(&session);
        let mut sb = focused_state(&mut model, &session);
        let out = sb.handle_key(&KeyCode::Escape, Modifiers::NONE, &mut model, &session);
        assert!(matches!(out, SidebarOutcome::Defocus));
    }

    #[test]
    fn load_or_seed_session_recovers_tabs_from_db_when_present() {
        let _env = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let state_home = std::env::temp_dir().join(format!("test_db_{}", std::process::id()));
        let db_path = state_home.join("superzej/superzej.db");
        std::fs::create_dir_all(db_path.parent().unwrap()).unwrap();

        let db = superzej_core::db::Db::open_at(&db_path).unwrap();
        let _ = db.put_workspace("/tmp/app", "app", "repo");
        // The worktree dir must exist on disk — vanished dirs are pruned at
        // load (git is the source of truth).
        let wt_dir = state_home.join("app-feat");
        std::fs::create_dir_all(&wt_dir).unwrap();
        let wt_path = wt_dir.to_string_lossy().into_owned();
        db.put_tab_group(
            "/tmp/app",
            &superzej_core::models::TabGroupRow {
                name: "app/feat".into(),
                kind: "branch".into(),
                worktree: wt_path.clone(),
                ordinal: 0,
                active_tab: 0,
            },
        )
        .unwrap();
        db.put_group_tab(
            "/tmp/app",
            &superzej_core::models::GroupTabRow {
                group_name: "app/feat".into(),
                ordinal: 0,
                title: "1".into(),
                pane_tree: r#"{"leaf":0}"#.into(),
                focused_pane: 0,
            },
        )
        .unwrap();

        // SAFETY: test is single-threaded; sets/clears an XDG var around one call.
        unsafe { std::env::set_var("XDG_STATE_HOME", &state_home) };

        let (session, seeded) = load_or_seed_session(std::path::Path::new("/tmp/app"));

        unsafe { std::env::remove_var("XDG_STATE_HOME") };

        assert_eq!(session.worktrees.len(), 1);
        assert_eq!(session.worktrees[0].name, "app/feat");
        assert_eq!(session.id, "/tmp/app");
        assert!(!seeded, "a resurrected session is not a fresh seed");
    }

    #[test]
    fn load_or_seed_session_reports_fresh_seed() {
        let _env = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let state_home =
            std::env::temp_dir().join(format!("test_db_seed_{}_state", std::process::id()));
        let _ = std::fs::remove_dir_all(&state_home);
        std::fs::create_dir_all(state_home.join("superzej")).unwrap();

        // SAFETY: test holds ENV_LOCK; sets/clears an XDG var around one call.
        unsafe { std::env::set_var("XDG_STATE_HOME", &state_home) };
        let (session, seeded) = load_or_seed_session(std::path::Path::new("/tmp/Fresh-Repo"));
        unsafe { std::env::remove_var("XDG_STATE_HOME") };

        assert!(seeded, "an empty DB seeds a fresh home group");
        assert_eq!(session.worktrees.len(), 1);
        assert_eq!(
            session.worktrees[0].name, "fresh-repo/home",
            "seeded home group is slug-keyed"
        );
    }

    #[test]
    fn hydration_worker_loads_real_workspaces_into_sidebar() {
        let _env = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let state_home =
            std::env::temp_dir().join(format!("test_db_sidebar_{}_state", std::process::id()));
        let db_path = state_home.join("superzej/superzej.db");
        std::fs::create_dir_all(db_path.parent().unwrap()).unwrap();

        let db = superzej_core::db::Db::open_at(&db_path).unwrap();
        let _ = db.put_workspace("/tmp/repo1", "repo1", "repo");
        // Ensure some time passes so timestamps are distinctly different
        std::thread::sleep(std::time::Duration::from_millis(10));
        let _ = db.put_workspace("/tmp/repo2", "repo2", "repo");

        // SAFETY: test is single-threaded; sets/clears an XDG var around calls.
        unsafe { std::env::set_var("XDG_STATE_HOME", &state_home) };

        let (session, _) = load_or_seed_session(std::path::Path::new("/tmp/repo1"));
        let model = build_model(&session, &db);

        unsafe { std::env::remove_var("XDG_STATE_HOME") };

        let slugs: Vec<&str> = model
            .sidebar_workspaces
            .iter()
            .map(|(s, _, _, _)| s.as_str())
            .collect();
        assert!(
            slugs.contains(&"repo1"),
            "Sidebar should contain repo1, got: {slugs:?}"
        );
        assert!(
            slugs.contains(&"repo2"),
            "Sidebar should contain repo2, got: {slugs:?}"
        );
    }

    #[test]
    fn palette_worktree_switch_persists_active_tab_for_target_workspace() {
        let db_path = std::env::temp_dir().join(format!(
            "sj-host-palette-switch-{}-{}.sqlite",
            std::process::id(),
            now_secs()
        ));
        let db = superzej_core::db::Db::open_at(&db_path).unwrap();
        db.put_workspace("/tmp/repo-a", "repo-a", "repo").unwrap();
        db.put_workspace("/tmp/repo-b", "repo-b", "repo").unwrap();

        let row = |name: &str, ord: i64| superzej_core::models::TabGroupRow {
            name: name.into(),
            kind: "branch".into(),
            worktree: format!("/tmp/{name}"),
            ordinal: ord,
            active_tab: 0,
        };
        db.put_tab_group("/tmp/repo-b", &row("repo-b/home", 0))
            .unwrap();
        db.put_tab_group("/tmp/repo-b", &row("repo-b/feature-x", 1))
            .unwrap();

        let mut session = Session {
            id: "/tmp/repo-a".into(),
            worktrees: vec![WorktreeGroup::new(
                "repo-a/home",
                GroupKind::Home,
                "/tmp/repo-a",
            )],
            active: 0,
        };

        switch_to_workspace_tab(&mut session, &db, "/tmp/repo-b", "repo-b/feature-x").unwrap();

        assert_eq!(session.id, "/tmp/repo-b");
        assert_eq!(session.active_group().unwrap().name, "repo-b/feature-x");
        assert_eq!(
            db.active_tab("/tmp/repo-b").unwrap().as_deref(),
            Some("repo-b/feature-x")
        );
    }

    fn sidebar_labels(model: &FrameModel) -> Vec<String> {
        model.sidebar_rows.iter().map(|r| r.label.clone()).collect()
    }

    #[test]
    fn refresh_tab_model_updates_sidebar_tree_when_tabs_change() {
        let mut session = one_tab_session();
        let mut model = build_initial_model(&session);
        let mut sb = SidebarState::default();

        refresh_tab_model(&mut model, &session, &mut sb);
        assert!(
            sidebar_labels(&model)
                .iter()
                .any(|row| row.contains("home")),
            "sidebar should show the initial home worktree: {:?}",
            sidebar_labels(&model)
        );

        session.add_group(WorktreeGroup::new(
            "app/feature-x",
            GroupKind::Branch,
            "/tmp/app-feature-x",
        ));
        refresh_tab_model(&mut model, &session, &mut sb);

        assert_eq!(model.worktree, "app/feature-x");
        assert!(
            sidebar_labels(&model)
                .iter()
                .any(|row| row.contains("feature-x")),
            "sidebar should include newly-created worktrees immediately: {:?}",
            sidebar_labels(&model)
        );
    }

    #[test]
    fn normalize_key_maps_nul_to_ctrl_space() {
        let nul = termwiz::input::KeyEvent {
            key: KeyCode::Char('\0'),
            modifiers: Modifiers::NONE,
        };
        let n = normalize_key(nul);
        assert_eq!(n.key, KeyCode::Char(' '));
        assert!(n.modifiers.contains(Modifiers::CTRL));
        // Already-decoded Ctrl+Space (kitty CSI-u) passes through unchanged.
        let kitty = termwiz::input::KeyEvent {
            key: KeyCode::Char(' '),
            modifiers: Modifiers::CTRL,
        };
        let k = normalize_key(kitty.clone());
        assert_eq!(k.key, kitty.key);
        assert_eq!(k.modifiers, kitty.modifiers);
    }

    #[test]
    fn drain_key_repeats_coalesces_identical_keys() {
        let key = termwiz::input::KeyEvent {
            key: KeyCode::DownArrow,
            modifiers: Modifiers::NONE,
        };
        let mk = |code| {
            InputEvent::Key(termwiz::input::KeyEvent {
                key: code,
                modifiers: Modifiers::NONE,
            })
        };
        // Three identical repeats then a different key.
        let mut q: std::collections::VecDeque<InputEvent> = [
            mk(KeyCode::DownArrow),
            mk(KeyCode::DownArrow),
            mk(KeyCode::Char('x')),
        ]
        .into();
        let (n, leftover) = drain_key_repeats(&key, || q.pop_front());
        assert_eq!(n, 3);
        assert!(matches!(
            leftover,
            Some(InputEvent::Key(k)) if k.key == KeyCode::Char('x')
        ));
        // Empty queue → just the first.
        let (n, leftover) = drain_key_repeats(&key, || None);
        assert_eq!(n, 1);
        assert!(leftover.is_none());
    }

    #[test]
    fn prune_vanished_group_lands_on_home_and_returns_pane_ids() {
        let mut session = Session {
            id: "/r/app".into(),
            worktrees: vec![
                WorktreeGroup::new("app/home", GroupKind::Home, "/r/app"),
                WorktreeGroup::new("app/feat", GroupKind::Branch, "/wt/feat"),
            ],
            active: 1,
        };
        // Point the doomed group's tab at a known pane id.
        session.worktrees[1].tabs[0].center = crate::center::CenterTree::Leaf(7);

        let ids = prune_vanished_group(&mut session, 1);
        assert_eq!(ids, vec![7]);
        assert_eq!(session.worktrees.len(), 1);
        assert_eq!(session.active_group().unwrap().name, "app/home");

        // Out of range is a no-op.
        assert!(prune_vanished_group(&mut session, 9).is_empty());
        assert_eq!(session.worktrees.len(), 1);
    }

    #[test]
    fn session_pane_ids_collects_all_tab_tree_leaves() {
        let mut session = two_worktree_session();
        session.worktrees[0].tabs[0].center = crate::center::CenterTree::Leaf(3);
        session.worktrees[1].tabs[0].center = crate::center::CenterTree::Leaf(8);
        let mut ids = session_pane_ids(&session);
        ids.sort_unstable();
        assert_eq!(ids, vec![3, 8]);
    }

    #[test]
    fn workspace_switch_does_not_duplicate_sidebar_workspaces() {
        // Post-switch state: the DB-hydrated list already names both
        // workspaces; the session now holds only the switched-to workspace's
        // (slug-keyed) live home group.
        let session = Session {
            id: "/r/washu".into(),
            worktrees: vec![WorktreeGroup::new(
                "washu/home",
                GroupKind::Home,
                "/r/washu",
            )],
            active: 0,
        };
        let mut model = build_initial_model(&session);
        model.sidebar_workspaces = vec![
            (
                "superzej".into(),
                "superzej".into(),
                "repo".into(),
                "/r/superzej".into(),
            ),
            (
                "washu".into(),
                "WASHU".into(),
                "repo".into(),
                "/r/washu".into(),
            ),
        ];
        let mut sb = SidebarState::default();

        // Refresh repeatedly (every hydration intake calls this): the list
        // must stay stable — the old behavior appended a duplicate per call.
        refresh_tab_model(&mut model, &session, &mut sb);
        refresh_tab_model(&mut model, &session, &mut sb);

        let slugs: Vec<_> = model
            .sidebar_workspaces
            .iter()
            .map(|(s, _, _, _)| s.as_str())
            .collect();
        assert_eq!(slugs, vec!["superzej", "washu"]);

        let home_rows: Vec<_> = model
            .sidebar_rows
            .iter()
            .filter(|r| r.label == "home" && r.workspace_slug == "washu")
            .collect();
        assert_eq!(
            home_rows.len(),
            1,
            "exactly one home row for the live workspace: {:?}",
            sidebar_labels(&model)
        );
        assert!(home_rows[0].active, "and it is the active (live) row");
    }

    #[test]
    fn new_tab_stays_within_the_worktree_and_tabbar_scopes_to_it() {
        let mut session = two_worktree_session();
        let mut model = build_initial_model(&session);
        let mut sb = SidebarState::default();

        // A new tab in the active worktree (Alt+t): the tabbar shows ONLY this
        // worktree's chips, never other worktrees.
        session.active_group_mut().unwrap().add_tab();
        refresh_tab_model(&mut model, &session, &mut sb);

        assert_eq!(model.worktree, "app/home");
        assert_eq!(model.tabs, vec!["1".to_string(), "2".to_string()]);
        assert_eq!(model.active_tab, 1);

        // Switching worktree swaps the whole strip (tabs live WITHIN a worktree).
        session.next_worktree();
        refresh_tab_model(&mut model, &session, &mut sb);
        assert_eq!(model.worktree, "app/feat");
        assert_eq!(model.tabs, vec!["1".to_string()]);
        assert_eq!(model.active_tab, 0);

        // And switching back restores the remembered tab.
        session.prev_worktree();
        refresh_tab_model(&mut model, &session, &mut sb);
        assert_eq!(model.worktree, "app/home");
        assert_eq!(model.active_tab, 1);
    }

    #[test]
    fn tab_switch_refreshes_model_without_changing_chrome_layout() {
        let mut session = one_tab_session();
        session.add_group(WorktreeGroup::new(
            "app/feat",
            GroupKind::Branch,
            "/tmp/app-feat",
        ));
        let mut model = build_initial_model(&session);
        let mut sb = SidebarState::default();
        let chrome = layout::compute(160, 40, true, true);
        let before = chrome.clone();

        session.switch_to(1);
        refresh_tab_model(&mut model, &session, &mut sb);

        assert_eq!(model.worktree, "app/feat");
        assert_eq!(model.tabs, vec!["1".to_string()]);
        assert_eq!(
            chrome, before,
            "worktree switches must reuse the chrome snapshot"
        );
        assert_eq!(chrome.panel.unwrap().cols, layout::PANEL_COLS);
    }

    #[test]
    fn dirty_ui_frames_render_before_pty_drain() {
        assert!(render_before_pty_drain(true));
        assert!(!render_before_pty_drain(false));
    }

    #[test]
    fn warmed_tab_remap_rewrites_tree_and_focus() {
        let mut tab = crate::session::Tab::new("1");
        tab.center = CenterTree::Split {
            dir: crate::center::Dir::Row,
            children: vec![
                crate::center::Branch {
                    weight: 1.0,
                    child: CenterTree::Leaf(3),
                },
                crate::center::Branch {
                    weight: 1.0,
                    child: CenterTree::Leaf(4),
                },
            ],
        };
        tab.focused_pane = 4;

        assert!(remap_warmed_tab_ids(&mut tab, 4, &[(3, 20), (4, 21)]));

        assert_eq!(tab.center.pane_ids(), vec![20, 21]);
        assert_eq!(tab.focused_pane, 21);
    }

    #[test]
    fn warmed_tab_remap_rejects_stale_tree() {
        let mut tab = crate::session::Tab::new("1");
        tab.center = CenterTree::Leaf(99);
        tab.focused_pane = 99;
        let before = tab.clone();

        assert!(!remap_warmed_tab_ids(&mut tab, 4, &[(3, 20), (4, 21)]));
        assert_eq!(tab, before);
    }
}
