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
use crate::gitmut::{GitOp, GitOpResult};
use crate::hydrate::{
    RefreshKind, active_tab_path, build_initial_model, load_or_seed_session, retarget_diff_watcher,
    spawn_model_hydration, spawn_pr_cache_refresh, spawn_refresh_ticker, workspace_list,
};
use crate::input::key_bytes;
use crate::layout;
use crate::menu::{self, MenuChoice, MenuOverlay};
use crate::palette::build_palette;
use crate::pane::PaneEvent;
use crate::panel::gitui::{self, GitFlow, GitMsg, GitView, StagePane};
use crate::panes::{
    Panes, prewarm_requests, relayout, relayout_strip, replace_single_dead_center_pane,
    tool_drawer_argv,
};
use crate::wizard;

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

/// Resolve `[theme] undercurl` to a capability: "on"/"off" are explicit, and
/// "auto" sniffs $TERM/$TERM_PROGRAM/$VTE_VERSION.
fn resolve_undercurl(cfg: &superzej_core::config::Config) -> bool {
    use superzej_core::config::UndercurlMode;
    match cfg.theme.undercurl {
        UndercurlMode::On => true,
        UndercurlMode::Off => false,
        UndercurlMode::Auto => crate::wire::detect_undercurl(),
    }
}

fn apply_mode_status(model: &mut FrameModel, mode: crate::keymap::Mode) {
    // The bottom bar carries the contextual keybind hints; the status slot
    // only flags a non-default input mode. The mode chip always shows.
    model.status = match mode {
        crate::keymap::Mode::Normal => String::new(),
        m => format!("{} mode", m.as_str()),
    };
    model.mode_chip = match mode {
        crate::keymap::Mode::Normal => "N",
        crate::keymap::Mode::VimNormal => "V",
        crate::keymap::Mode::VimInsert => "I",
        crate::keymap::Mode::Emacs => "E",
    }
    .into();
}

/// The bottom bar's contextual keybind hints — (chord, label) pairs the
/// statusbar renders as key chips + dim labels: what works right now, given
/// the focused zone (and the panel's view when it owns the keyboard).
fn context_hints(
    focus: &crate::focus::FocusState,
    panel_ui: &crate::panel::PanelUi,
    cfg: &superzej_core::config::Config,
) -> Vec<(String, String)> {
    let chord = |id: &str| -> Option<String> { crate::keymap::chord_hint_for(cfg, id) };
    let hint = |label: &str, id: &str| chord(id).map(|c| (c, label.to_string()));
    let pair = |c: &str, label: &str| Some((c.to_string(), label.to_string()));
    if focus.locked {
        return vec![
            hint("unlock", "toggle-key-lock").unwrap_or_else(|| ("Ctrl-g".into(), "unlock".into())),
        ];
    }
    match focus.zone {
        crate::focus::Zone::Center => [
            hint("pane", "focus-left").or_else(|| chord("focus-right").map(|c| (c, "pane".into()))),
            hint("tab", "prev-tab").or_else(|| chord("next-tab").map(|c| (c, "tab".into()))),
            hint("worktree", "prev-worktree")
                .or_else(|| chord("next-worktree").map(|c| (c, "worktree".into()))),
            hint("close tab", "close-tab"),
            hint("smart split", "new-pane"),
            hint("split↓", "split-down"),
            hint("split→", "split-right"),
            hint("zoom", "zoom"),
            hint("menu", "palette"),
        ]
        .into_iter()
        .flatten()
        .collect(),
        crate::focus::Zone::Sidebar => [
            pair("↑↓", "move"),
            pair("Enter", "open"),
            pair("Space", "mark"),
            pair("m", "menu"),
            hint("tab", "prev-tab").or_else(|| chord("next-tab").map(|c| (c, "tab".into()))),
            pair("Esc", "back"),
        ]
        .into_iter()
        .flatten()
        .collect(),
        crate::focus::Zone::Panel => {
            let mut hints: Vec<(String, String)> = [
                hint("pane", "focus-left")
                    .or_else(|| chord("focus-right").map(|c| (c, "pane".into()))),
                hint("tab", "prev-tab").or_else(|| chord("next-tab").map(|c| (c, "tab".into()))),
                pair("Esc", "back"),
            ]
            .into_iter()
            .flatten()
            .collect();
            hints.extend(crate::chrome::panel_help_pairs(panel_ui));
            hints
        }
        crate::focus::Zone::Masthead => vec![
            hint("back", "focus-down").unwrap_or_else(|| ("Ctrl-Down".into(), "back".into())),
            ("Esc".into(), "back".into()),
        ],
        crate::focus::Zone::Statusbar => vec![
            hint("back", "focus-up").unwrap_or_else(|| ("Ctrl-Up".into(), "back".into())),
            ("Esc".into(), "back".into()),
        ],
    }
}

/// Fetch the git section's heat/velocity/log payload off the loop (skipped
/// while the per-worktree cache is warm). The result rides `tx` + a waker
/// pulse; the body shows "loading…" until it lands.
fn kick_git_docs_fetch(
    generation: u64,
    session: &crate::session::Session,
    panel_ui: &crate::panel::PanelUi,
    tx: &tokio_mpsc::UnboundedSender<(u64, crate::panel::docs::DocsPayload)>,
    waker: &TerminalWaker,
) {
    use crate::panel::docs::{DocsPayload, GitDocs};
    if panel_ui.docs.git.is_some() {
        return;
    }
    const WEEKS: usize = 36;
    let wt = active_tab_path(session);
    let tx = tx.clone();
    let wk = waker.clone();
    task::spawn_blocking(move || {
        use superzej_svc::git::GitBackend;
        let loc = superzej_core::remote::GitLoc::for_worktree(&wt);
        let git = superzej_svc::git::CliGit;
        let now = superzej_core::util::now();
        let epochs = git.commit_times(&loc, WEEKS).unwrap_or_default();
        let data = GitDocs {
            heat: superzej_core::gitviz::heat_grid(&epochs, now, WEEKS),
            weekly: superzej_core::gitviz::weekly_counts(&epochs, now, WEEKS),
            log: git.log_graph(&loc, 40).unwrap_or_default(),
            total: epochs.len() as u32,
            head_branch: git.current_branch(&loc).unwrap_or_default(),
        };
        if tx.send((generation, DocsPayload::Git(data))).is_ok() {
            let _ = wk.wake();
        }
    });
}

/// Fetch the selected change's parsed side-by-side diff off the loop —
/// refetched on every (re-)entry into the changes full view, the working
/// tree moves constantly. Targets the panel's selected change, else the
/// first; with a clean tree the doc is synthesized here (the body renders
/// the empty state immediately — nothing to fetch).
fn kick_diff_doc_fetch(
    generation: u64,
    session: &crate::session::Session,
    panel_ui: &mut crate::panel::PanelUi,
    model: &FrameModel,
    tx: &tokio_mpsc::UnboundedSender<(u64, crate::panel::docs::DocsPayload)>,
    waker: &TerminalWaker,
) {
    use crate::panel::docs::{DiffDoc, DocsPayload};
    panel_ui.docs.diff = None;
    let path = panel_ui
        .chg_sel
        .and_then(|i| model.panel.changes.get(i))
        .or_else(|| model.panel.changes.first())
        .map(|c| c.path.clone());
    let Some(path) = path else {
        panel_ui.docs.diff = Some(DiffDoc {
            path: String::new(),
            file: superzej_core::diff_sbs::SbsFile::default(),
        });
        return;
    };
    let wt = active_tab_path(session);
    let tx = tx.clone();
    let wk = waker.clone();
    task::spawn_blocking(move || {
        let loc = superzej_core::remote::GitLoc::for_worktree(&wt);
        let text = loc
            .git_command(&["diff", "--no-color", "HEAD", "--", &path])
            .output()
            .ok()
            .filter(|o| o.status.success())
            .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
            .unwrap_or_default();
        let doc = DiffDoc {
            path,
            file: superzej_core::diff_sbs::parse_unified(&text),
        };
        if tx.send((generation, DocsPayload::Diff(doc))).is_ok() {
            let _ = wk.wake();
        }
    });
}

/// Kick whatever document fetch the panel's (section, width) state needs.
/// The single entry point for every transition (section open, width cycle,
/// worktree switch) so data wiring can't drift per site.
fn sync_panel_docs(
    panel_ui: &mut crate::panel::PanelUi,
    model: &FrameModel,
    session: &crate::session::Session,
    generation: u64,
    tx: &tokio_mpsc::UnboundedSender<(u64, crate::panel::docs::DocsPayload)>,
    waker: &TerminalWaker,
) {
    use crate::panel::Section;
    if panel_ui.open == Section::Git && panel_ui.width.is_expanded() {
        kick_git_docs_fetch(generation, session, panel_ui, tx, waker);
    }
    if panel_ui.open == Section::Changes && panel_ui.width == layout::PanelWidth::Full {
        kick_diff_doc_fetch(generation, session, panel_ui, model, tx, waker);
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
    crate::seg::set_undercurl_supported(resolve_undercurl(&cfg));
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
    spawn_model_hydration(
        model_tx.clone(),
        0,
        session.clone(),
        Some(waker.clone()),
        crate::hydrate::HydrateHints::default(),
    );

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
    let (metrics_tx, metrics_rx) = tokio_mpsc::unbounded_channel::<crate::metrics::MetricsState>();
    crate::metrics::spawn_metrics_supervisor(cfg.metrics.clone(), metrics_tx, waker.clone());
    // The stats cadence is user-cyclable at runtime (click the top-right
    // stats block); the ticker thread reads it per tick.
    let stats_interval_ms = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(
        (cfg.stats.refresh_secs.max(0.5) * 1000.0) as u64,
    ));
    // Set while the telemetry overlay is open: the ticker samples stats at
    // its 500ms half-tick instead of the user-cycled rate.
    let stats_live = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    spawn_refresh_ticker(
        refresh_tx.clone(),
        stats_tx,
        stats_interval_ms.clone(),
        stats_live.clone(),
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
        metrics_rx,
        stats_interval_ms,
        stats_live,
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
    panel_width: layout::PanelWidth,
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
            layout::PanelWidth::Normal,
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
                layout::PanelWidth::Normal,
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
                layout::PanelWidth::Full,
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
        // The bars are single rows — zooming them makes no sense; fall back
        // to the normal layout (zoom is never set to a bar zone; this arm
        // exists for exhaustiveness).
        Some(Zone::Masthead) | Some(Zone::Statusbar) | None => layout::compute_full(
            cols,
            rows,
            want_sidebar,
            want_panel,
            panel_forced,
            panel_width,
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
    let active_path = crate::hydrate::active_tab_path(session);
    model.worktree = worktree;
    model.tabs = tabs;
    model.active_tab = active_tab;
    model.active_container_name =
        superzej_core::sandbox::container_name(&active_path.to_string_lossy());
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

    fn focus_active_row(&mut self, model: &mut FrameModel) {
        self.cursor = visible_index_of_active(model);
        self.sync(model);
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

    /// Move the active worktree one slot within its workspace (Shift+Alt+↑/↓).
    /// Swaps it with the adjacent *same-workspace* branch sibling in both the
    /// live session order and the persisted registry `position`, so the new
    /// order survives restart. `home` is a fixed top anchor: a worktree can't
    /// move above it, and home itself never moves. A move while a computed sort
    /// is active first flips the workspace back to Manual so the move is visible
    /// and sticks. Returns whether anything moved.
    fn move_active_worktree(
        &mut self,
        model: &mut FrameModel,
        session: &mut crate::session::Session,
        up: bool,
    ) -> bool {
        use crate::session::GroupKind;
        let a = session.active;
        // Home never moves.
        if session.worktrees.get(a).map(|g| g.kind) == Some(GroupKind::Home) {
            return false;
        }
        // Walk the on-screen order so the motion matches what the user sees;
        // same-workspace worktrees are contiguous there (one block per repo).
        let order = sidebar_worktree_order(model);
        let Some(p) = order.iter().position(|&g| g == a) else {
            return false;
        };
        let neighbor = if up {
            p.checked_sub(1)
        } else {
            (p + 1 < order.len()).then_some(p + 1)
        };
        let Some(np) = neighbor else { return false };
        let b = order[np];
        // Stay within the same workspace, and never cross above home.
        let slug = |gi: usize| {
            session
                .worktrees
                .get(gi)
                .and_then(|g| crate::sidebar::split_tab(&g.name).map(|(s, _)| s))
        };
        if slug(a) != slug(b) {
            return false;
        }
        if session.worktrees.get(b).map(|g| g.kind) == Some(GroupKind::Home) {
            return false;
        }

        // Persist the new order: swap the durable `position` of the two paths…
        if let Ok(db) = superzej_core::db::Db::open() {
            let (pa, pb) = (
                session.worktrees[a].path.clone(),
                session.worktrees[b].path.clone(),
            );
            let _ = db.swap_worktree_positions(&pa, &pb);
        }
        // …and the live session order, keeping the moved group active.
        session.worktrees.swap(a, b);
        session.active = b;

        // A manual move only makes sense under Manual order; flip + persist if a
        // computed sort was active so the move is visible and survives restart.
        if self.view.sort != crate::sidebar::SortMode::Manual {
            self.view.sort = crate::sidebar::SortMode::Manual;
            self.persist(&session.id, "sort_mode", self.view.sort.as_str());
        }
        self.rebuild(model, session);
        true
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
                key if crate::input::is_escape_key(key) => {
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
                key if crate::input::is_escape_key(key) => {
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
            key if crate::input::is_escape_key(key) => return SidebarOutcome::Defocus,
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

/// The panel's current geometry for content building, falling back to the
/// resting width and a tall default when the panel rect is hidden.
fn panel_geom(chrome: &layout::ChromeLayout) -> (usize, usize) {
    chrome
        .panel
        .map(|r| (r.cols, r.rows))
        .unwrap_or((layout::PANEL_COLS, 40))
}

/// The accordion immediately after `from` in the live order (no wrap) — the
/// cursor flows into it when Down is pressed at the bottom of `from`. Every
/// accordion is visited, including those with no actionable rows (you land on
/// the header and the next Down flows onward). None at the last accordion.
fn next_section_in_order(
    from: crate::panel::Section,
    ui: &crate::panel::PanelUi,
) -> Option<crate::panel::Section> {
    let idx = ui.order.iter().position(|&s| s == from)?;
    ui.order.get(idx + 1).copied()
}

/// The accordion immediately before `from` in the live order (no wrap); the
/// cursor flows into its LAST item when Up is pressed at the top of `from`.
fn prev_section_in_order(
    from: crate::panel::Section,
    ui: &crate::panel::PanelUi,
) -> Option<crate::panel::Section> {
    let idx = ui.order.iter().position(|&s| s == from)?;
    idx.checked_sub(1).and_then(|i| ui.order.get(i).copied())
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

fn forget_worktree_group(
    db: &superzej_core::db::Db,
    session_id: &str,
    group: &crate::session::WorktreeGroup,
) {
    if !group.path.is_empty() {
        let _ = db.del_worktree(&group.path);
    }
    let _ = db.del_worktree_for_tab(session_id, &group.name);
    let _ = db.delete_tab_group(session_id, &group.name);
}

fn delete_groups(
    session: &mut crate::session::Session,
    panes: &mut Panes,
    mut targets: Vec<usize>,
) -> String {
    targets.sort_unstable_by(|a, b| b.cmp(a));
    targets.dedup();
    let db = superzej_core::db::Db::open().ok();
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
            // git is the source of truth, but `git worktree remove` leaves the
            // dir behind if it ever fails (locked, detached, prune races); a
            // lingering dir is re-adopted on the next launch and looks like a
            // failed delete. Make sure the directory is actually gone.
            let _ = std::fs::remove_dir_all(&path);
        }
        if let Some(db) = &db {
            forget_worktree_group(db, &session.id, &session.worktrees[gi]);
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
    // Persist the trimmed layout: without this the closed groups survive in the
    // `tab_groups` table and `Session::resurrect` brings the "deleted" worktrees
    // back on the next launch.
    if deleted > 0
        && let Some(db) = &db
    {
        let _ = session.persist(db, &session.id, now_secs());
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

/// The editor invocation for a worktree-relative `path`, with the universal
/// `+N` line jump when a location is known. Shared by every panel open path
/// (changed files, review threads, failing tests).
fn editor_open_command(
    cfg: &superzej_core::config::Config,
    path: &str,
    line: Option<usize>,
) -> String {
    let editor = cfg
        .tool_command("editor")
        .unwrap_or("${EDITOR:-vi} .")
        .trim();
    let editor = editor.strip_suffix(" .").unwrap_or(editor);
    let quoted = path.replace('\'', r"'\''");
    match line {
        Some(l) => format!("{editor} +{l} '{quoted}'"),
        None => format!("{editor} '{quoted}'"),
    }
}

/// Parse a `path:line` failure location; bare messages yield `None`.
fn parse_file_line(at: &str) -> Option<(String, usize)> {
    let (path, line) = at.rsplit_once(':')?;
    let line: usize = line.trim().parse().ok()?;
    (!path.is_empty()).then(|| (path.to_string(), line))
}

/// The cursor-th FILE row of the files section's changed-files mini tree —
/// mirrors `panel::sections::files` display order (dirs synthesized but not
/// actionable), so the row-mode cursor and Enter always agree.
fn changed_file_at(model: &FrameModel, cursor: usize) -> Option<String> {
    let paths: Vec<String> = model.panel.changes.iter().map(|c| c.path.clone()).collect();
    crate::panel::build_file_tree(&paths)
        .into_iter()
        .filter(|e| !e.is_dir)
        .nth(cursor)
        .map(|e| e.path)
}

/// Persist the accordion's open section + wide mode (mirrors the sidebar's
/// inline `ui_state` writes — single-row upserts on a WAL handle, sub-ms).
fn persist_panel_state(panel_ui: &crate::panel::PanelUi) {
    if let Ok(db) = superzej_core::db::Db::open() {
        let _ = db.set_ui_state("panel", "open", panel_ui.open.as_key());
        let _ = db.set_ui_state("panel", "width", panel_ui.width.as_key());
    }
}

/// The docs-fetch wiring a panel transition needs: generation + channel +
/// the model the diff fetch targets. Bundled so `open_panel_section` /
/// `toggle_panel_expand` call sites stay readable.
struct PanelDocsWiring<'a> {
    model: &'a FrameModel,
    generation: u64,
    tx: &'a tokio_mpsc::UnboundedSender<(u64, crate::panel::docs::DocsPayload)>,
}

/// Open accordion section `s`: reset row-mode state, persist the choice,
/// kick a rehydrate so the model carries the section's deep data (git log,
/// file count) — the cached panel stays on screen until the fresh one lands —
/// and start whatever document fetch the new (section, width) state needs.
fn open_panel_section(
    s: crate::panel::Section,
    panel_ui: &mut crate::panel::PanelUi,
    hydration_gen: &mut u64,
    model_tx: &tokio_mpsc::UnboundedSender<(u64, FrameModel)>,
    session: &crate::session::Session,
    waker: &TerminalWaker,
    docs: PanelDocsWiring<'_>,
) {
    panel_ui.open = s;
    // Land at the top of the new section's items (row_mode tracks panel focus,
    // not the section, so a focused panel keeps walking rows across switches).
    panel_ui.cursor = 0;
    panel_ui.chg_sel = None;
    panel_ui.scroll = 0;
    panel_ui.diff_hunk = 0;
    persist_panel_state(panel_ui);
    *hydration_gen += 1;
    spawn_model_hydration(
        model_tx.clone(),
        *hydration_gen,
        session.clone(),
        Some(waker.clone()),
        crate::hydrate::HydrateHints {
            open: panel_ui.open,
            expanded: panel_ui.width.is_expanded(),
        },
    );
    sync_panel_docs(
        panel_ui,
        docs.model,
        session,
        docs.generation,
        docs.tx,
        waker,
    );
}

/// Cycle the accordion's view (`e`): persist + rehydrate (section bodies
/// change with the width) and start any document fetch the wider view needs.
/// The pre-render expansion detector picks up the new state and recomputes
/// the chrome.
fn toggle_panel_expand(
    panel_ui: &mut crate::panel::PanelUi,
    hydration_gen: &mut u64,
    model_tx: &tokio_mpsc::UnboundedSender<(u64, FrameModel)>,
    session: &crate::session::Session,
    waker: &TerminalWaker,
    docs: PanelDocsWiring<'_>,
) {
    // Cycle the panel width Normal → Half → Full → Normal; every section
    // renders a distinct body per width.
    panel_ui.width = panel_ui.width.cycle();
    panel_ui.scroll = 0;
    panel_ui.diff_hunk = 0;
    persist_panel_state(panel_ui);
    *hydration_gen += 1;
    spawn_model_hydration(
        model_tx.clone(),
        *hydration_gen,
        session.clone(),
        Some(waker.clone()),
        crate::hydrate::HydrateHints {
            open: panel_ui.open,
            expanded: panel_ui.width.is_expanded(),
        },
    );
    sync_panel_docs(
        panel_ui,
        docs.model,
        session,
        docs.generation,
        docs.tx,
        waker,
    );
}

/// Fetch one changed file's inline hunk preview off the loop, deduped against
/// the banked previews and the in-flight set; the result rides `hunk_tx` back
/// with a waker pulse.
#[allow(clippy::too_many_arguments)]
fn spawn_hunk_fetch(
    path: &str,
    session: &crate::session::Session,
    panel_ui: &crate::panel::PanelUi,
    hunk_inflight: &mut std::collections::HashSet<String>,
    hunk_tx: &tokio_mpsc::UnboundedSender<(u64, String, Vec<superzej_svc::git::Hunk>)>,
    waker: &TerminalWaker,
    generation: u64,
) {
    if panel_ui.hunks.contains_key(path) || !hunk_inflight.insert(path.to_string()) {
        return;
    }
    let tx = hunk_tx.clone();
    let waker = waker.clone();
    let wt = active_tab_path(session);
    let path = path.to_string();
    tokio::task::spawn_blocking(move || {
        use superzej_svc::git::GitBackend;
        let loc = superzej_core::remote::GitLoc::for_worktree(&wt);
        let hunks = superzej_svc::git::CliGit
            .diff_hunks(&loc, "HEAD", &path, 16)
            .unwrap_or_default();
        if tx.send((generation, path, hunks)).is_ok() {
            let _ = waker.wake();
        }
    });
}

/// Toggle the changes-section selection onto row `i` (re-selecting dismisses
/// the preview), kicking the background hunk fetch for newly-selected paths.
#[allow(clippy::too_many_arguments)]
fn toggle_change_selection(
    i: usize,
    panel_ui: &mut crate::panel::PanelUi,
    model: &FrameModel,
    session: &crate::session::Session,
    hunk_inflight: &mut std::collections::HashSet<String>,
    hunk_tx: &tokio_mpsc::UnboundedSender<(u64, String, Vec<superzej_svc::git::Hunk>)>,
    waker: &TerminalWaker,
    generation: u64,
) {
    if panel_ui.chg_sel == Some(i) {
        panel_ui.chg_sel = None;
        return;
    }
    panel_ui.chg_sel = Some(i);
    // Untracked rows have no diff: the preview renders a static note.
    if let Some(row) = model
        .panel
        .changes
        .get(i)
        .filter(|c| c.stage != crate::panel::Stage::Untracked)
    {
        spawn_hunk_fetch(
            &row.path,
            session,
            panel_ui,
            hunk_inflight,
            hunk_tx,
            waker,
            generation,
        );
    }
}

/// Kick a `gh` PR action off the loop; a PR-cache + model refresh follows so
/// the git section reflects the outcome. Failures are logged (the next
/// refresh's `pr_note` carries the visible state) rather than blocking.
fn spawn_pr_action<F>(
    session: &crate::session::Session,
    refresh_tx: &tokio_mpsc::UnboundedSender<RefreshKind>,
    waker: &TerminalWaker,
    label: &'static str,
    action: F,
) where
    F: FnOnce(&superzej_core::remote::GitLoc) -> Result<(), superzej_core::github::GhError>
        + Send
        + 'static,
{
    let wt = active_tab_path(session);
    let tx = refresh_tx.clone();
    let waker = waker.clone();
    tokio::task::spawn_blocking(move || {
        let loc = superzej_core::remote::GitLoc::for_worktree(&wt);
        if let Err(e) = action(&loc) {
            superzej_core::msg::warn(&format!(
                "{label} failed: {}",
                superzej_core::github::describe(&e)
            ));
        }
        // A PR refresh implies a model refresh in the loop's intake.
        if tx.send(RefreshKind::Pr).is_ok() {
            let _ = waker.wake();
        }
    });
}

// ---------------------------------------------------------------------------
// The git mutation pipeline: every lazygit-style write flows through ONE
// runner — `enqueue_git_op` rejects while one is in flight, runs the op on
// `spawn_blocking`, and the result rides `gitop_tx` back with a waker pulse.
// `handle_git_msg` turns the pure `GitMsg` intents from `gitui::git_key`
// into ops / state changes; `dispatch_menu_choice` does the same for the
// option/confirm menus. Both are plain functions so the event-loop match
// stays readable.
// ---------------------------------------------------------------------------

/// Which flow a SUCCESSFUL op closes (computed at dispatch — results only
/// carry the op's label).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FlowEnd {
    None,
    Rebase,
    Bisect,
    Patch,
}

fn flow_end_of(op: &GitOp) -> FlowEnd {
    match op {
        GitOp::RebaseAbort
        | GitOp::RebaseContinue
        | GitOp::RebaseSkip
        | GitOp::RebaseInteractive { .. }
        | GitOp::RebaseBranch { .. }
        | GitOp::RebaseOnto { .. }
        | GitOp::Squash { .. }
        | GitOp::Fixup { .. }
        | GitOp::Drop { .. }
        | GitOp::MoveCommit { .. }
        | GitOp::AmendOldCommit { .. } => FlowEnd::Rebase,
        GitOp::BisectReset => FlowEnd::Bisect,
        GitOp::PatchApply { .. }
        | GitOp::PatchRemoveFromCommit { .. }
        | GitOp::PatchSplit { .. }
        | GitOp::PatchToIndex { .. } => FlowEnd::Patch,
        _ => FlowEnd::None,
    }
}

/// One finished mutation, tagged with everything the intake needs.
struct GitOpDone {
    generation: u64,
    label: &'static str,
    touches_remote: bool,
    flow_end: FlowEnd,
    clear_clipboard: bool,
    result: GitOpResult,
}

/// Run `op` off the loop (one at a time — a request while busy is rejected
/// with a status note, lazygit-style; queueing compound git ops invites
/// disaster). The result lands on `tx` with a waker pulse.
fn enqueue_git_op(
    op: GitOp,
    git: &mut gitui::GitUi,
    status: &mut String,
    session: &crate::session::Session,
    override_gpg: bool,
    tx: &tokio_mpsc::UnboundedSender<GitOpDone>,
    waker: &TerminalWaker,
) {
    if let Some(p) = &git.pending {
        *status = format!("git busy: {}", p.label);
        return;
    }
    let label = op.label();
    let touches_remote = op.touches_remote();
    let flow_end = flow_end_of(&op);
    let clear_clipboard = matches!(op, GitOp::CherryPick { .. });
    git.pending = Some(gitui::PendingOp {
        label: label.to_string(),
    });
    *status = format!("{label}…");
    let generation = git.op_gen;
    let wt = active_tab_path(session);
    let tx = tx.clone();
    let wk = waker.clone();
    tokio::task::spawn_blocking(move || {
        let loc = superzej_core::remote::GitLoc::for_worktree(&wt);
        let result = crate::gitmut::execute(op, &loc, override_gpg);
        if tx
            .send(GitOpDone {
                generation,
                label,
                touches_remote,
                flow_end,
                clear_clipboard,
                result,
            })
            .is_ok()
        {
            let _ = wk.wake();
        }
    });
}

/// A fetched git document for the line-cursor views.
enum GitDoc {
    Stage(gitui::StageDocState),
    CommitFiles(Vec<(String, u32, u32)>),
    Patch(gitui::StageDocState),
    /// A paused rebase's live state (`None` when the pause vanished before
    /// the read landed).
    Rebase(Option<superzej_svc::git::RebaseStatus>),
}

/// Fetch the staging view's diff (unstaged|staged per pane) off the loop and
/// flatten it; generation-tagged like the hunk fetches.
fn spawn_stage_doc_fetch(
    generation: u64,
    session: &crate::session::Session,
    path: String,
    pane: StagePane,
    tx: &tokio_mpsc::UnboundedSender<(u64, GitDoc)>,
    waker: &TerminalWaker,
) {
    let wt = active_tab_path(session);
    let tx = tx.clone();
    let wk = waker.clone();
    tokio::task::spawn_blocking(move || {
        use superzej_svc::git::GitBackend;
        let loc = superzej_core::remote::GitLoc::for_worktree(&wt);
        let git = superzej_svc::git::CliGit;
        let diff = match pane {
            StagePane::Unstaged => git.unstaged_diff(&loc, &path),
            StagePane::Staged => git.staged_diff(&loc, &path),
        }
        .unwrap_or_default();
        let doc = crate::panel::staging::build(&path, &diff);
        let state = gitui::StageDocState {
            path,
            pane,
            doc,
            diff,
        };
        if tx.send((generation, GitDoc::Stage(state))).is_ok() {
            let _ = wk.wake();
        }
    });
}

/// Fetch a paused rebase's live state off the loop, so the TODO editor
/// always works on `rebase-merge/git-rebase-todo` as it actually is (never
/// the stale pre-rebase plan, never blind to external edits).
fn spawn_rebase_status_fetch(
    generation: u64,
    session: &crate::session::Session,
    tx: &tokio_mpsc::UnboundedSender<(u64, GitDoc)>,
    waker: &TerminalWaker,
) {
    let wt = active_tab_path(session);
    let tx = tx.clone();
    let wk = waker.clone();
    tokio::task::spawn_blocking(move || {
        use superzej_svc::git::RebaseOps;
        let loc = superzej_core::remote::GitLoc::for_worktree(&wt);
        let status = superzej_svc::git::CliGit.rebase_status(&loc).ok().flatten();
        if tx.send((generation, GitDoc::Rebase(status))).is_ok() {
            let _ = wk.wake();
        }
    });
}

/// Fetch a drilled commit's file list (numstat) off the loop.
fn spawn_commit_files_fetch(
    generation: u64,
    session: &crate::session::Session,
    sha: String,
    tx: &tokio_mpsc::UnboundedSender<(u64, GitDoc)>,
    waker: &TerminalWaker,
) {
    let wt = active_tab_path(session);
    let tx = tx.clone();
    let wk = waker.clone();
    tokio::task::spawn_blocking(move || {
        use superzej_core::patch::LineKind;
        use superzej_svc::git::GitBackend;
        let loc = superzej_core::remote::GitLoc::for_worktree(&wt);
        let git = superzej_svc::git::CliGit;
        // `sha^..sha` covers ordinary commits; a root commit has no parent,
        // so fall back to counting the commit diff's own +/- lines.
        let files: Vec<(String, u32, u32)> = match git.diff_refs(&loc, &format!("{sha}^"), &sha) {
            Ok(v) if !v.is_empty() => v
                .into_iter()
                .map(|d| (d.path, d.added, d.deleted))
                .collect(),
            _ => git
                .commit_diff(&loc, &sha, None)
                .map(|d| {
                    superzej_core::patch::parse_patch(&d)
                        .into_iter()
                        .map(|f| {
                            let (a, del) = f.hunks.iter().flat_map(|h| &h.lines).fold(
                                (0u32, 0u32),
                                |(a, d), l| match l.kind {
                                    LineKind::Add => (a + 1, d),
                                    LineKind::Del => (a, d + 1),
                                    _ => (a, d),
                                },
                            );
                            (f.new_path.clone(), a, del)
                        })
                        .collect()
                })
                .unwrap_or_default(),
        };
        if tx.send((generation, GitDoc::CommitFiles(files))).is_ok() {
            let _ = wk.wake();
        }
    });
}

/// Fetch one file of a drilled commit's diff (the patch-building doc).
fn spawn_patch_doc_fetch(
    generation: u64,
    session: &crate::session::Session,
    sha: String,
    path: String,
    tx: &tokio_mpsc::UnboundedSender<(u64, GitDoc)>,
    waker: &TerminalWaker,
) {
    let wt = active_tab_path(session);
    let tx = tx.clone();
    let wk = waker.clone();
    tokio::task::spawn_blocking(move || {
        use superzej_svc::git::GitBackend;
        let loc = superzej_core::remote::GitLoc::for_worktree(&wt);
        let diff = superzej_svc::git::CliGit
            .commit_diff(&loc, &sha, Some(&path))
            .unwrap_or_default();
        let doc = crate::panel::staging::build(&path, &diff);
        let state = gitui::StageDocState {
            path,
            pane: StagePane::Unstaged,
            doc,
            diff,
        };
        if tx.send((generation, GitDoc::Patch(state))).is_ok() {
            let _ = wk.wake();
        }
    });
}

/// The read-only wiring `handle_git_msg` / `dispatch_menu_choice` need.
struct GitWires<'a> {
    session: &'a crate::session::Session,
    cfg: &'a superzej_core::config::Config,
    op_tx: &'a tokio_mpsc::UnboundedSender<GitOpDone>,
    doc_tx: &'a tokio_mpsc::UnboundedSender<(u64, GitDoc)>,
    waker: &'a TerminalWaker,
}

/// The loop-owned overlay slots the git layer drives.
struct GitOverlays<'a> {
    menu: &'a mut Option<MenuOverlay>,
    input: &'a mut Option<(menu::InputOverlay, GitInputKind)>,
    /// A destructive op awaiting its `[y]` (the menu carries tag "git-op").
    confirm_op: &'a mut Option<GitOp>,
    /// `cfg.git_commands` indices behind the open custom-commands menu.
    custom_cmds: &'a mut Vec<usize>,
}

/// What a submitted git input overlay means.
enum GitInputKind {
    Commit,
    Reword {
        sha: String,
    },
    StashPush,
    PatchSplit {
        sha: String,
        patch: String,
    },
    BranchCreate,
    BranchRename {
        old: String,
    },
    /// One prompt of a custom command; `remaining` are `(key, title)` pairs.
    CustomPrompt {
        cmd: usize,
        key: String,
        collected: std::collections::BTreeMap<String, String>,
        remaining: Vec<(String, String)>,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HostInputKind {
    NewWorkspace,
}

fn begin_new_workspace_prompt(
    host_input: &mut Option<(menu::InputOverlay, HostInputKind)>,
    model: &mut FrameModel,
) {
    *host_input = Some((
        menu::InputOverlay::new("new workspace — path or URL", ""),
        HostInputKind::NewWorkspace,
    ));
    model.status = "Create workspace: enter path or URL (Esc cancels)".into();
}

fn looks_like_git_url(input: &str) -> bool {
    input.starts_with("http://")
        || input.starts_with("https://")
        || input.starts_with("ssh://")
        || input.starts_with("git://")
        || input.starts_with("git@")
}

fn workspace_repo_name_from_url(input: &str) -> String {
    let trimmed = input.trim_end_matches('/');
    let tail = trimmed.rsplit(['/', ':']).next().unwrap_or(trimmed);
    let name = tail.strip_suffix(".git").unwrap_or(tail);
    let slug = superzej_core::util::slugify(name);
    if slug.is_empty() {
        "workspace".into()
    } else {
        name.to_string()
    }
}

#[cfg(test)]
fn create_workspace_from_input(
    input: &str,
    session: &mut crate::session::Session,
    db: &superzej_core::db::Db,
) -> Result<std::path::PathBuf> {
    create_workspace_from_input_with_config(
        input,
        session,
        db,
        &superzej_core::config::Config::default(),
    )
}

fn create_workspace_from_input_with_config(
    input: &str,
    session: &mut crate::session::Session,
    db: &superzej_core::db::Db,
    cfg: &superzej_core::config::Config,
) -> Result<std::path::PathBuf> {
    let input = input.trim();
    anyhow::ensure!(!input.is_empty(), "no workspace path or URL given");

    let root = if looks_like_git_url(input) {
        let repo_name = workspace_repo_name_from_url(input);
        let dest = std::path::PathBuf::from(superzej_core::util::expand_tilde(&cfg.workspaces_dir))
            .join(repo_name);
        if !dest.exists() {
            if let Some(parent) = dest.parent() {
                std::fs::create_dir_all(parent)
                    .with_context(|| format!("create {}", parent.display()))?;
            }
            let status = std::process::Command::new("git")
                .arg("clone")
                .arg(input)
                .arg(&dest)
                .status()
                .with_context(|| format!("git clone {input} {}", dest.display()))?;
            anyhow::ensure!(status.success(), "git clone failed for {input}");
        }
        std::fs::canonicalize(&dest).unwrap_or(dest)
    } else {
        let expanded = superzej_core::util::expand_tilde(input);
        let path = std::path::PathBuf::from(expanded);
        let path = if path.is_absolute() {
            path
        } else {
            std::env::current_dir()?.join(path)
        };
        anyhow::ensure!(path.is_dir(), "path does not exist: {}", path.display());
        let canonical = std::fs::canonicalize(&path).unwrap_or(path);
        superzej_core::repo::main_worktree(&canonical).unwrap_or(canonical)
    };

    let root_s = root.to_string_lossy().into_owned();
    let name = root
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "workspace".into());
    let kind = if superzej_core::repo::main_worktree(&root).is_some() {
        "repo"
    } else {
        "dir"
    };
    db.put_workspace(&root_s, &name, kind)?;
    db.touch_repo(&root_s, &name)?;
    session.switch_to_workspace(&root_s, db)?;
    Ok(root)
}

/// A follow-up only the loop body can perform (it owns session/panes).
#[must_use]
enum GitAfter {
    None,
    /// Run the `Action::NewWorktree` flow.
    NewWorktree,
    /// Spawn this command into a fresh center tab (custom-command
    /// `output = "terminal"`).
    Terminal(String),
}

fn short_sha(sha: &str) -> String {
    sha.chars().take(7).collect()
}

fn sel_change(ui: &crate::panel::PanelUi, model: &FrameModel) -> Option<crate::panel::ChangeRow> {
    gitui::source_at(&ui.git, GitView::Files, &model.panel)
        .and_then(|i| model.panel.changes.get(i).cloned())
}

fn sel_commit(ui: &crate::panel::PanelUi, model: &FrameModel) -> Option<crate::panel::CommitRow> {
    gitui::source_at(&ui.git, GitView::Commits, &model.panel)
        .and_then(|i| model.panel.commits.get(i).cloned())
}

fn sel_branch(ui: &crate::panel::PanelUi, model: &FrameModel) -> Option<crate::panel::BranchRow> {
    gitui::source_at(&ui.git, GitView::Branches, &model.panel)
        .and_then(|i| model.panel.branches.get(i).cloned())
}

fn sel_stash(ui: &crate::panel::PanelUi, model: &FrameModel) -> Option<crate::panel::StashRow> {
    gitui::source_at(&ui.git, GitView::Stash, &model.panel)
        .and_then(|i| model.panel.stashes.get(i).cloned())
}

/// The line-cursor views' active document.
fn active_line_doc(git: &gitui::GitUi) -> Option<&gitui::StageDocState> {
    match git.focus {
        GitView::Staging => git.stage_doc.as_ref(),
        GitView::PatchBuilding => git.patch_doc.as_ref(),
        _ => None,
    }
}

/// Stash `op` behind a yes/no confirm menu (tag "git-op").
fn confirm_git_op(
    ov: &mut GitOverlays<'_>,
    title: &str,
    body: impl Into<String>,
    danger: bool,
    op: GitOp,
) {
    *ov.confirm_op = Some(op);
    *ov.menu = Some(menu::confirm_menu(
        title,
        body,
        "git-op",
        String::new(),
        danger,
    ));
}

/// The custom-commands `context` label a git view filters on.
fn custom_context_label(view: GitView) -> &'static str {
    match view {
        GitView::Files | GitView::Staging => "files",
        GitView::Branches => "branches",
        GitView::Commits | GitView::CommitFiles | GitView::PatchBuilding | GitView::RebaseTodo => {
            "commits"
        }
        GitView::Stash => "stash",
    }
}

/// The template context for custom commands, built from the CURRENT
/// selection (expansion happens at pick/submit time, lazygit-style).
fn git_template_ctx(
    panel_ui: &crate::panel::PanelUi,
    model: &FrameModel,
    session: &crate::session::Session,
    prompts: std::collections::BTreeMap<String, String>,
) -> superzej_core::custom_cmd::TemplateCtx {
    use superzej_core::custom_cmd::{BranchVars, CommitVars, StashVars, TemplateCtx};
    TemplateCtx {
        selected_commit: sel_commit(panel_ui, model).map(|c| CommitVars {
            sha: c.sha,
            short: c.short,
            subject: c.subject,
            author: c.author,
        }),
        selected_branch: sel_branch(panel_ui, model).map(|b| BranchVars {
            name: b.name,
            upstream: b.upstream,
        }),
        checked_out_branch: Some(BranchVars {
            name: model.panel.branch.clone(),
            upstream: None,
        }),
        selected_file: sel_change(panel_ui, model).map(|c| c.path),
        selected_stash: sel_stash(panel_ui, model).map(|s| StashVars {
            index: s.index,
            message: s.message,
        }),
        worktree_path: Some(active_tab_path(session).to_string_lossy().into_owned()),
        prompt_responses: prompts,
    }
}

/// Expand + run custom command `idx` with the collected prompt responses.
fn run_custom_command(
    idx: usize,
    prompts: std::collections::BTreeMap<String, String>,
    panel_ui: &mut crate::panel::PanelUi,
    model: &mut FrameModel,
    wires: &GitWires<'_>,
) -> GitAfter {
    use superzej_core::config::GitCmdOutput;
    let Some(cmd) = wires.cfg.git_commands.get(idx) else {
        return GitAfter::None;
    };
    let ctx = git_template_ctx(panel_ui, model, wires.session, prompts);
    match superzej_core::custom_cmd::expand(&cmd.command, &ctx) {
        Ok(line) => match cmd.output {
            GitCmdOutput::Terminal => GitAfter::Terminal(line),
            out => {
                enqueue_git_op(
                    GitOp::Custom {
                        command: line,
                        capture: out == GitCmdOutput::Popup,
                    },
                    &mut panel_ui.git,
                    &mut model.status,
                    wires.session,
                    wires.cfg.git.override_gpg,
                    wires.op_tx,
                    wires.waker,
                );
                GitAfter::None
            }
        },
        Err(e) => {
            model.status = e.to_string();
            GitAfter::None
        }
    }
}

/// Render the custom patch from the marked lines across every fetched patch
/// doc. `reverse=true` for the removal flows (remove/split/move-to-index) —
/// see `svc::git::patch`; forward for plain applies.
fn render_marked_patch(git: &gitui::GitUi, reverse: bool) -> Option<String> {
    let GitFlow::Patch(p) = &git.flow else {
        return None;
    };
    let mut out = String::new();
    for (path, marks) in &p.marks {
        if marks.is_empty() {
            continue;
        }
        let Some(docst) = git.patch_docs.get(path) else {
            continue;
        };
        let files = superzej_core::patch::parse_patch(&docst.diff);
        let Some(file) = files.first() else {
            continue;
        };
        let sel = crate::panel::staging::to_selection(&docst.doc, marks.iter().copied());
        if let Some(piece) = superzej_core::patch::transform(file, &sel, reverse) {
            out.push_str(&piece);
        }
    }
    (!out.is_empty()).then_some(out)
}

/// Open a PR url in the browser, detached (no `gh` needed).
fn open_url_detached(url: &str) {
    let _ = std::process::Command::new("xdg-open")
        .arg(url)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn();
}

/// Turn one decoded [`GitMsg`] into ops / state changes. Navigation mutates
/// [`gitui::GitUi`] directly; effects flow through the mutation runner (with
/// destructive ones parked behind a confirm menu first).
#[allow(clippy::too_many_lines)]
fn handle_git_msg(
    msg: GitMsg,
    panel_ui: &mut crate::panel::PanelUi,
    model: &mut FrameModel,
    wires: &GitWires<'_>,
    ov: &mut GitOverlays<'_>,
) -> GitAfter {
    let enqueue = |panel_ui: &mut crate::panel::PanelUi, model: &mut FrameModel, op: GitOp| {
        enqueue_git_op(
            op,
            &mut panel_ui.git,
            &mut model.status,
            wires.session,
            wires.cfg.git.override_gpg,
            wires.op_tx,
            wires.waker,
        );
    };
    match msg {
        // ---- navigation -------------------------------------------------
        GitMsg::CursorDown | GitMsg::CursorUp => {
            let down = msg == GitMsg::CursorDown;
            match panel_ui.git.focus {
                GitView::RebaseTodo => {
                    if let GitFlow::Rebase(r) = &mut panel_ui.git.flow {
                        let max = r.todos.len().saturating_sub(1);
                        r.cursor = if down {
                            (r.cursor + 1).min(max)
                        } else {
                            r.cursor.saturating_sub(1)
                        };
                    }
                }
                GitView::Staging | GitView::PatchBuilding => {
                    let cur = panel_ui.git.staging.as_ref().map_or(0, |s| s.cursor);
                    let next = active_line_doc(&panel_ui.git)
                        .map(|d| gitui::step_cursor(&d.doc, cur, down));
                    if let (Some(next), Some(s)) = (next, panel_ui.git.staging.as_mut()) {
                        s.cursor = next;
                    }
                }
                _ => {}
            }
        }
        GitMsg::ToggleRangeMode => match panel_ui.git.focus {
            GitView::Staging | GitView::PatchBuilding => {
                if let Some(s) = panel_ui.git.staging.as_mut() {
                    s.anchor = match s.anchor {
                        Some(_) => None,
                        None => Some(s.cursor),
                    };
                }
            }
            v => {
                panel_ui.git.sel_anchor = match panel_ui.git.sel_anchor {
                    Some(_) => None,
                    None => Some(panel_ui.git.cur.get(v)),
                };
            }
        },
        GitMsg::TogglePane => {
            if panel_ui.git.focus == GitView::Staging {
                let kicked = panel_ui.git.staging.as_mut().map(|s| {
                    s.pane = match s.pane {
                        StagePane::Unstaged => StagePane::Staged,
                        StagePane::Staged => StagePane::Unstaged,
                    };
                    s.cursor = 0;
                    s.anchor = None;
                    (s.path.clone(), s.pane)
                });
                if let Some((path, pane)) = kicked {
                    spawn_stage_doc_fetch(
                        panel_ui.git.op_gen,
                        wires.session,
                        path,
                        pane,
                        wires.doc_tx,
                        wires.waker,
                    );
                }
            }
        }
        GitMsg::NextHunk | GitMsg::PrevHunk => {
            let cur = panel_ui.git.staging.as_ref().map_or(0, |s| s.cursor);
            let next = active_line_doc(&panel_ui.git).and_then(|d| {
                if msg == GitMsg::NextHunk {
                    crate::panel::staging::next_hunk(&d.doc, cur)
                } else {
                    crate::panel::staging::prev_hunk(&d.doc, cur)
                }
            });
            if let (Some(next), Some(s)) = (next, panel_ui.git.staging.as_mut()) {
                s.cursor = next;
                s.anchor = None;
            }
        }
        GitMsg::Drill => match panel_ui.git.focus {
            GitView::Files => {
                if let Some(row) = sel_change(panel_ui, model) {
                    // An untracked file has no diff to line-stage: record it
                    // with intent-to-add first (`git add -N`), which turns
                    // its whole content into stageable Add lines.
                    if row.stage == crate::panel::Stage::Untracked {
                        enqueue(
                            panel_ui,
                            model,
                            GitOp::IntentToAdd {
                                path: row.path.clone(),
                            },
                        );
                    }
                    let mut s = gitui::StagingUi::new(&row.path);
                    if row.stage == crate::panel::Stage::Staged {
                        s.pane = StagePane::Staged;
                    }
                    let pane = s.pane;
                    panel_ui.git.stage_doc = None;
                    panel_ui.git.staging = Some(s);
                    panel_ui.git.focus = GitView::Staging;
                    spawn_stage_doc_fetch(
                        panel_ui.git.op_gen,
                        wires.session,
                        row.path,
                        pane,
                        wires.doc_tx,
                        wires.waker,
                    );
                }
            }
            GitView::Commits => {
                if let Some(c) = sel_commit(panel_ui, model) {
                    panel_ui.git.drilled_commit = Some(c.sha.clone());
                    panel_ui.git.commit_files.clear();
                    panel_ui.git.cur.commit_files = 0;
                    panel_ui.cursor = 0;
                    panel_ui.git.focus = GitView::CommitFiles;
                    spawn_commit_files_fetch(
                        panel_ui.git.op_gen,
                        wires.session,
                        c.sha,
                        wires.doc_tx,
                        wires.waker,
                    );
                }
            }
            GitView::CommitFiles => {
                let file = panel_ui
                    .git
                    .commit_files
                    .get(panel_ui.git.cur.commit_files)
                    .map(|(p, _, _)| p.clone());
                if let (Some(path), Some(sha)) = (file, panel_ui.git.drilled_commit.clone()) {
                    match &mut panel_ui.git.flow {
                        GitFlow::Patch(p) => p.path = path.clone(),
                        f => {
                            *f = GitFlow::Patch(gitui::PatchUi {
                                commit: sha.clone(),
                                path: path.clone(),
                                marks: Default::default(),
                            })
                        }
                    }
                    panel_ui.git.patch_doc = None;
                    panel_ui.git.staging = Some(gitui::StagingUi::new(&path));
                    panel_ui.git.focus = GitView::PatchBuilding;
                    spawn_patch_doc_fetch(
                        panel_ui.git.op_gen,
                        wires.session,
                        sha,
                        path,
                        wires.doc_tx,
                        wires.waker,
                    );
                }
            }
            GitView::Branches => {
                model.status = "branch log renders from the commits list".into();
            }
            GitView::Stash => {
                model.status = "stash diff renders in the main region".into();
            }
            _ => {}
        },
        GitMsg::Back => {
            let git = &mut panel_ui.git;
            if git.filter.is_some() {
                git.filter = None;
            } else if git.staging.as_ref().is_some_and(|s| s.anchor.is_some()) {
                if let Some(s) = git.staging.as_mut() {
                    s.anchor = None;
                }
            } else if git.sel_anchor.is_some() {
                git.sel_anchor = None;
            } else {
                match git.focus {
                    GitView::Staging => {
                        git.staging = None;
                        git.stage_doc = None;
                        git.focus = panel_ui.open.home_view().unwrap_or(GitView::Files);
                    }
                    GitView::PatchBuilding => {
                        git.staging = None;
                        git.focus = GitView::CommitFiles;
                    }
                    GitView::CommitFiles => {
                        git.drilled_commit = None;
                        git.commit_files.clear();
                        git.focus = GitView::Commits;
                    }
                    GitView::RebaseTodo => {
                        if !matches!(&git.flow, GitFlow::Rebase(r) if r.running) {
                            git.flow = GitFlow::None;
                        }
                        git.focus = GitView::Commits;
                    }
                    _ => {}
                }
            }
        }
        // ---- staging / patch building ------------------------------------
        GitMsg::StageLines => match panel_ui.git.focus {
            GitView::Staging => {
                let op = panel_ui
                    .git
                    .staging
                    .as_ref()
                    .zip(panel_ui.git.stage_doc.as_ref())
                    .map(|(s, d)| {
                        let indices = gitui::sel_pairs(&d.doc, s.selection());
                        (indices, d.path.clone(), d.diff.clone(), d.pane)
                    });
                match op {
                    Some((indices, _, _, _)) if indices.is_empty() => {
                        model.status = "nothing stageable selected".into();
                    }
                    Some((indices, path, diff, pane)) => {
                        if let Some(s) = panel_ui.git.staging.as_mut() {
                            s.anchor = None;
                        }
                        let target = match pane {
                            StagePane::Unstaged => crate::gitmut::StageTarget::Unstaged,
                            StagePane::Staged => crate::gitmut::StageTarget::Staged,
                        };
                        enqueue(
                            panel_ui,
                            model,
                            GitOp::StageLines {
                                path,
                                diff,
                                indices,
                                target,
                            },
                        );
                    }
                    None => {}
                }
            }
            GitView::PatchBuilding => {
                // Pure: toggle marks for the selected range.
                let sel = panel_ui.git.staging.as_ref().map(|s| s.selection());
                let toggles: Vec<usize> = sel
                    .and_then(|sel| {
                        panel_ui.git.patch_doc.as_ref().map(|d| {
                            sel.filter(|&i| crate::panel::staging::selectable(&d.doc, i))
                                .collect()
                        })
                    })
                    .unwrap_or_default();
                let path = panel_ui.git.staging.as_ref().map(|s| s.path.clone());
                if let (Some(path), GitFlow::Patch(p)) = (path, &mut panel_ui.git.flow) {
                    let marks = p.marks.entry(path).or_default();
                    for i in toggles {
                        if !marks.remove(&i) {
                            marks.insert(i);
                        }
                    }
                    model.status = format!("{} line(s) in patch", p.marked());
                }
                if let Some(s) = panel_ui.git.staging.as_mut() {
                    s.anchor = None;
                }
            }
            _ => {}
        },
        GitMsg::SelectHunk => {
            let cur = panel_ui.git.staging.as_ref().map_or(0, |s| s.cursor);
            let range = active_line_doc(&panel_ui.git).and_then(|d| {
                let r = crate::panel::staging::hunk_range(&d.doc, cur)?;
                // Rest the cursor on the hunk's last CURSORABLE line.
                let end = (*r.start()..=*r.end())
                    .rev()
                    .find(|&i| crate::panel::staging::cursorable(&d.doc, i))?;
                Some((*r.start(), end))
            });
            if let (Some((start, end)), Some(s)) = (range, panel_ui.git.staging.as_mut()) {
                s.anchor = Some(start);
                s.cursor = end;
            }
        }
        GitMsg::DiscardLines => {
            if panel_ui.git.focus == GitView::Staging {
                let op = panel_ui
                    .git
                    .staging
                    .as_ref()
                    .zip(panel_ui.git.stage_doc.as_ref())
                    .map(|(s, d)| {
                        (
                            gitui::sel_pairs(&d.doc, s.selection()),
                            d.path.clone(),
                            d.diff.clone(),
                        )
                    });
                if let Some((indices, path, diff)) = op {
                    if indices.is_empty() {
                        model.status = "nothing selected to discard".into();
                    } else {
                        confirm_git_op(
                            ov,
                            "discard lines?",
                            format!(
                                "discards {} line(s) of {path} — unrecoverable",
                                indices.len()
                            ),
                            true,
                            GitOp::DiscardLines {
                                path,
                                diff,
                                indices,
                            },
                        );
                    }
                }
            }
        }
        GitMsg::StageAll => {
            // Toggle, lazygit-style: when everything is already staged the
            // same key empties the index instead.
            let all_staged = !model.panel.changes.is_empty()
                && model
                    .panel
                    .changes
                    .iter()
                    .all(|c| c.stage == crate::panel::Stage::Staged);
            if all_staged {
                confirm_git_op(
                    ov,
                    "unstage all?",
                    "unstages every change",
                    false,
                    GitOp::UnstageAll,
                );
            } else {
                confirm_git_op(
                    ov,
                    "stage all?",
                    "stages every change",
                    false,
                    GitOp::StageAll,
                );
            }
        }
        // ---- files ---------------------------------------------------------
        GitMsg::StageToggleFile => match panel_ui.git.focus {
            GitView::RebaseTodo => {
                if let GitFlow::Rebase(r) = &mut panel_ui.git.flow {
                    gitui::todo_toggle_at(&mut r.todos, r.cursor);
                }
            }
            _ => {
                if let Some(row) = sel_change(panel_ui, model) {
                    enqueue(
                        panel_ui,
                        model,
                        GitOp::StageFile {
                            path: row.path,
                            unstage: row.stage == crate::panel::Stage::Staged,
                        },
                    );
                }
            }
        },
        GitMsg::DiscardFile => {
            if let Some(row) = sel_change(panel_ui, model) {
                confirm_git_op(
                    ov,
                    "discard?",
                    format!("discards {} — unrecoverable", row.path),
                    true,
                    GitOp::DiscardFile {
                        path: row.path,
                        untracked: row.stage == crate::panel::Stage::Untracked,
                    },
                );
            }
        }
        // ---- commits ---------------------------------------------------------
        GitMsg::Commit => {
            *ov.input = Some((
                menu::InputOverlay::new("commit message", ""),
                GitInputKind::Commit,
            ));
        }
        GitMsg::Reword => {
            if let Some(c) = sel_commit(panel_ui, model) {
                *ov.input = Some((
                    menu::InputOverlay::new(format!("reword {}", c.short), c.subject),
                    GitInputKind::Reword { sha: c.sha },
                ));
            }
        }
        GitMsg::Squash | GitMsg::Fixup => {
            if let Some((oldest, targets)) = gitui::commit_selection(&panel_ui.git, &model.panel) {
                panel_ui.git.sel_anchor = None;
                let op = if msg == GitMsg::Squash {
                    GitOp::Squash { oldest, targets }
                } else {
                    GitOp::Fixup { oldest, targets }
                };
                enqueue(panel_ui, model, op);
            }
        }
        GitMsg::Drop => {
            if let Some((oldest, targets)) = gitui::commit_selection(&panel_ui.git, &model.panel) {
                panel_ui.git.sel_anchor = None;
                confirm_git_op(
                    ov,
                    "drop?",
                    format!("drops {} commit(s) from history", targets.len()),
                    true,
                    GitOp::Drop { oldest, targets },
                );
            }
        }
        GitMsg::Edit => {
            if let Some(c) = sel_commit(panel_ui, model) {
                panel_ui.git.flow = GitFlow::Rebase(gitui::RebaseUi {
                    running: true,
                    ..Default::default()
                });
                enqueue(panel_ui, model, GitOp::EditStop { sha: c.sha });
            }
        }
        GitMsg::MoveUp | GitMsg::MoveDown => {
            let up = msg == GitMsg::MoveUp;
            match panel_ui.git.focus {
                GitView::RebaseTodo => {
                    if let GitFlow::Rebase(r) = &mut panel_ui.git.flow {
                        r.cursor = gitui::todo_move(&mut r.todos, r.cursor, up);
                    }
                }
                _ => {
                    if let Some(c) = sel_commit(panel_ui, model) {
                        enqueue(panel_ui, model, GitOp::MoveCommit { sha: c.sha, up });
                    }
                }
            }
        }
        GitMsg::AmendStaged => {
            if let Some(c) = sel_commit(panel_ui, model) {
                // Amending HEAD needs no rebase — a plain `--amend` is
                // cheaper and conflict-free.
                let is_head = model.panel.commits.first().map(|h| &h.sha) == Some(&c.sha);
                let op = if is_head {
                    GitOp::AmendHead
                } else {
                    GitOp::AmendOldCommit { target: c.sha }
                };
                confirm_git_op(
                    ov,
                    "amend?",
                    format!("amends staged changes into {}", c.short),
                    false,
                    op,
                );
            }
        }
        GitMsg::Revert => {
            if let Some(c) = sel_commit(panel_ui, model) {
                confirm_git_op(
                    ov,
                    "revert?",
                    format!("reverts {} with a new inverse commit", c.short),
                    false,
                    GitOp::Revert { sha: c.sha },
                );
            }
        }
        GitMsg::CopyCommits => {
            let shas: Vec<String> = gitui::selected_sources(&panel_ui.git, &model.panel)
                .into_iter()
                .filter_map(|i| model.panel.commits.get(i).map(|c| c.sha.clone()))
                .collect();
            for sha in shas {
                if !panel_ui.git.clipboard.contains(&sha) {
                    panel_ui.git.clipboard.push(sha);
                }
            }
            panel_ui.git.sel_anchor = None;
            model.status = format!("{} copied", panel_ui.git.clipboard.len());
        }
        GitMsg::PasteCommits => {
            if panel_ui.git.clipboard.is_empty() {
                model.status = "nothing copied".into();
            } else {
                // The clipboard holds newest-first; the executor wants
                // oldest-first.
                let shas: Vec<String> = panel_ui.git.clipboard.iter().rev().cloned().collect();
                confirm_git_op(
                    ov,
                    "cherry-pick?",
                    format!("cherry-picks {} commit(s) onto HEAD", shas.len()),
                    false,
                    GitOp::CherryPick { shas },
                );
            }
        }
        GitMsg::EnterInteractive => {
            // A rebase already in progress owns the editor — `i` jumps to
            // it rather than clobbering the live flow with a fresh plan.
            if matches!(&panel_ui.git.flow, GitFlow::Rebase(r) if r.running) {
                panel_ui.git.focus = GitView::RebaseTodo;
            } else if let Some((base, todos)) = gitui::todo_from_commits(&model.panel.commits) {
                panel_ui.git.flow = GitFlow::Rebase(gitui::RebaseUi {
                    base,
                    todos,
                    ..Default::default()
                });
                panel_ui.git.focus = GitView::RebaseTodo;
            } else {
                model.status = "no commits loaded".into();
            }
        }
        GitMsg::ConfirmRebase => {
            let op = match &mut panel_ui.git.flow {
                GitFlow::Rebase(r) if !r.running => {
                    r.running = true;
                    Some(GitOp::RebaseInteractive {
                        base: r.base.clone(),
                        todo: r.todos.clone(),
                    })
                }
                // A PAUSED rebase: confirm rewrites the still-pending
                // entries in place (reorder/retag/drop mid-flight). The op
                // carries the as-read baseline and the backend refuses to
                // clobber a todo that changed on disk since; an unsynced
                // editor (live read still out) can't rewrite at all.
                GitFlow::Rebase(r) if r.running && r.todos_synced => {
                    Some(GitOp::RewritePendingTodo {
                        todo: r.todos.clone(),
                        baseline: r.baseline.clone(),
                    })
                }
                GitFlow::Rebase(_) => {
                    model.status = "loading live rebase todo — retry in a moment".into();
                    None
                }
                _ => None,
            };
            if let Some(op) = op {
                enqueue(panel_ui, model, op);
            }
        }
        GitMsg::TodoSetAction(action) => {
            if let GitFlow::Rebase(r) = &mut panel_ui.git.flow {
                gitui::todo_retag_at(&mut r.todos, r.cursor, action);
            }
        }
        GitMsg::MarkBase => {
            if let Some(c) = sel_commit(panel_ui, model) {
                panel_ui.git.mark_base = match panel_ui.git.mark_base.take() {
                    Some(m) if m == c.sha => None,
                    _ => {
                        model.status = format!("rebase base marked: {}", c.short);
                        Some(c.sha)
                    }
                };
            }
        }
        GitMsg::ToggleDiffMark => {
            if let Some(c) = sel_commit(panel_ui, model) {
                if matches!(&panel_ui.git.flow, GitFlow::Diffing(m) if *m == c.sha) {
                    *ov.menu = Some(menu::diff_menu(&c.short));
                } else {
                    panel_ui.git.diff_mark = Some(c.sha.clone());
                    panel_ui.git.flow = GitFlow::Diffing(c.sha);
                    model.status = format!("diffing against {}", c.short);
                }
            }
        }
        GitMsg::CheckoutSel => match panel_ui.git.focus {
            GitView::Branches => {
                if let Some(b) = sel_branch(panel_ui, model) {
                    if b.is_head {
                        model.status = "already checked out".into();
                    } else {
                        enqueue(panel_ui, model, GitOp::Checkout { refname: b.name });
                    }
                }
            }
            _ => {
                if let Some(c) = sel_commit(panel_ui, model) {
                    confirm_git_op(
                        ov,
                        "checkout commit?",
                        format!("checks out {} (detached HEAD)", c.short),
                        false,
                        GitOp::Checkout { refname: c.sha },
                    );
                }
            }
        },
        GitMsg::ResetMenu => {
            let (sha, short) = match panel_ui.git.focus {
                GitView::Files => ("HEAD".to_string(), "HEAD".to_string()),
                _ => match sel_commit(panel_ui, model) {
                    Some(c) => (c.sha, c.short),
                    None => return GitAfter::None,
                },
            };
            *ov.menu = Some(menu::reset_menu(&sha, &short));
        }
        GitMsg::OpenMenu(kind) => match kind {
            gitui::MenuKind::Rebase => {
                // The continue family is chosen by the live conflict banner:
                // `m` during a cherry-pick/merge conflict drives THOSE
                // sequencers, not the rebase one.
                let banner = model.panel.merge.as_ref().map(|m| m.label.clone());
                *ov.menu = Some(match banner.as_deref() {
                    Some("CHERRY-PICK") => menu::cherry_conflict_menu(),
                    Some("MERGING") => menu::merge_conflict_menu(),
                    Some("REVERTING") => menu::revert_conflict_menu(),
                    other => {
                        let conflicted = matches!(&panel_ui.git.flow, GitFlow::Rebase(r) if r.conflict)
                            || other.is_some();
                        menu::rebase_menu(conflicted)
                    }
                });
            }
            gitui::MenuKind::Patch => {
                *ov.menu = Some(menu::patch_menu());
            }
            gitui::MenuKind::Bisect => {
                *ov.menu = Some(menu::bisect_menu(matches!(
                    panel_ui.git.flow,
                    GitFlow::Bisect(_)
                )));
            }
            gitui::MenuKind::BranchActions => {
                if let Some(b) = sel_branch(panel_ui, model) {
                    *ov.menu = Some(menu::branch_actions_menu(&b.name, b.is_head));
                }
            }
            gitui::MenuKind::CustomCommands => {
                let ctx_label = custom_context_label(panel_ui.git.focus);
                let mut seen = std::collections::HashSet::new();
                let mut pairs: Vec<(char, String)> = Vec::new();
                ov.custom_cmds.clear();
                for (i, c) in wires.cfg.git_commands.iter().enumerate() {
                    if c.context != "global" && c.context != ctx_label {
                        continue;
                    }
                    let key = c.key.chars().next().unwrap_or('?');
                    if !seen.insert(key.to_ascii_lowercase()) {
                        continue; // duplicate hotkey: first one wins
                    }
                    ov.custom_cmds.push(i);
                    pairs.push((
                        key,
                        c.description.clone().unwrap_or_else(|| c.command.clone()),
                    ));
                }
                if pairs.is_empty() {
                    model.status = "no custom commands for this view ([[git_commands]])".into();
                } else {
                    *ov.menu = Some(menu::custom_commands_menu(&pairs));
                }
            }
        },
        // ---- branches ---------------------------------------------------
        GitMsg::Pull => enqueue(panel_ui, model, GitOp::Pull),
        GitMsg::Push => enqueue(
            panel_ui,
            model,
            GitOp::Push {
                force: superzej_svc::git::ForceMode::None,
            },
        ),
        GitMsg::FastForward => {
            if let Some(b) = sel_branch(panel_ui, model) {
                enqueue(
                    panel_ui,
                    model,
                    GitOp::FastForward {
                        branch: b.name,
                        current: b.is_head,
                    },
                );
            }
        }
        GitMsg::RebaseOntoSel => {
            if let Some(b) = sel_branch(panel_ui, model) {
                if b.is_head {
                    model.status = "cannot rebase a branch onto itself".into();
                } else if let Some(marked_base) = panel_ui.git.mark_base.clone() {
                    confirm_git_op(
                        ov,
                        "rebase onto?",
                        format!(
                            "rebases commits after {} onto {}",
                            short_sha(&marked_base),
                            b.name
                        ),
                        false,
                        GitOp::RebaseOnto {
                            target: b.name,
                            marked_base,
                        },
                    );
                } else {
                    enqueue(panel_ui, model, GitOp::RebaseBranch { branch: b.name });
                }
            }
        }
        GitMsg::DeleteSel => {
            if let Some(b) = sel_branch(panel_ui, model) {
                if b.is_head {
                    model.status = "cannot delete the checked-out branch".into();
                } else {
                    confirm_git_op(
                        ov,
                        "delete branch?",
                        format!("deletes {}", b.name),
                        true,
                        GitOp::DeleteBranch {
                            name: b.name,
                            force: false,
                        },
                    );
                }
            }
        }
        GitMsg::CreateWorktree => return GitAfter::NewWorktree,
        GitMsg::OpenPrInBrowser => {
            match sel_branch(panel_ui, model).and_then(|b| b.pr.map(|p| p.url)) {
                Some(url) => {
                    open_url_detached(&url);
                    model.status = "opened PR in the browser".into();
                }
                None => model.status = "no PR for this branch".into(),
            }
        }
        // ---- stash --------------------------------------------------------
        GitMsg::StashPush => {
            *ov.input = Some((
                menu::InputOverlay::new("stash message", ""),
                GitInputKind::StashPush,
            ));
        }
        GitMsg::StashApply => {
            if let Some(s) = sel_stash(panel_ui, model) {
                enqueue(panel_ui, model, GitOp::StashApply { index: s.index });
            }
        }
        GitMsg::StashPop => {
            if let Some(s) = sel_stash(panel_ui, model) {
                enqueue(panel_ui, model, GitOp::StashPop { index: s.index });
            }
        }
        GitMsg::StashDrop => {
            if let Some(s) = sel_stash(panel_ui, model) {
                confirm_git_op(
                    ov,
                    "drop stash?",
                    format!("drops stash@{{{}}}: {}", s.index, s.message),
                    true,
                    GitOp::StashDrop { index: s.index },
                );
            }
        }
        // ---- global-ish ---------------------------------------------------
        GitMsg::Undo => enqueue(panel_ui, model, GitOp::UndoPlan { redo: false }),
        GitMsg::Redo => enqueue(panel_ui, model, GitOp::UndoPlan { redo: true }),
        GitMsg::Cheatsheet => {
            let view = panel_ui.git.focus;
            let pairs: Vec<(String, String)> = gitui::context_keys(view)
                .iter()
                .map(|ck| (ck.chord.to_string(), ck.label.to_string()))
                .collect();
            *ov.menu = Some(menu::keybinds_menu(view.label(), &pairs));
        }
        GitMsg::FilterStart => {
            let view = panel_ui.git.focus;
            if gitui::list_labels(view, &model.panel).is_some() {
                panel_ui.git.filter = Some(gitui::ListFilter {
                    view,
                    query: String::new(),
                    editing: true,
                    map: gitui::display_map(&panel_ui.git, view, &model.panel),
                });
            } else {
                model.status = "filter is not available in this view".into();
            }
        }
    }
    GitAfter::None
}

/// Resolve a picked menu choice into ops / state changes (the exhaustive
/// counterpart of the constructors in `crate::menu`).
#[allow(clippy::too_many_lines)]
fn dispatch_menu_choice(
    choice: MenuChoice,
    panel_ui: &mut crate::panel::PanelUi,
    model: &mut FrameModel,
    wires: &GitWires<'_>,
    ov: &mut GitOverlays<'_>,
    pending_undo: &mut Option<(superzej_core::reflog::UndoPlan, bool)>,
) -> GitAfter {
    use superzej_svc::git::{ForceMode, ResetMode};
    let enqueue = |panel_ui: &mut crate::panel::PanelUi, model: &mut FrameModel, op: GitOp| {
        enqueue_git_op(
            op,
            &mut panel_ui.git,
            &mut model.status,
            wires.session,
            wires.cfg.git.override_gpg,
            wires.op_tx,
            wires.waker,
        );
    };
    match choice {
        MenuChoice::RebaseContinue => enqueue(panel_ui, model, GitOp::RebaseContinue),
        MenuChoice::RebaseAbort => enqueue(panel_ui, model, GitOp::RebaseAbort),
        MenuChoice::RebaseSkip => enqueue(panel_ui, model, GitOp::RebaseSkip),
        MenuChoice::ResetSoft(sha) => enqueue(
            panel_ui,
            model,
            GitOp::ResetTo {
                sha,
                mode: ResetMode::Soft,
            },
        ),
        MenuChoice::ResetMixed(sha) => enqueue(
            panel_ui,
            model,
            GitOp::ResetTo {
                sha,
                mode: ResetMode::Mixed,
            },
        ),
        MenuChoice::ResetHard(sha) => enqueue(
            panel_ui,
            model,
            GitOp::ResetTo {
                sha,
                mode: ResetMode::Hard,
            },
        ),
        MenuChoice::Nuke => enqueue(panel_ui, model, GitOp::Nuke),
        MenuChoice::PatchApply | MenuChoice::PatchApplyReverse => {
            match render_marked_patch(&panel_ui.git, false) {
                Some(patch) => enqueue(
                    panel_ui,
                    model,
                    GitOp::PatchApply {
                        patch,
                        reverse: choice == MenuChoice::PatchApplyReverse,
                    },
                ),
                None => model.status = "no lines marked".into(),
            }
        }
        MenuChoice::PatchToIndex => {
            let sha = match &panel_ui.git.flow {
                GitFlow::Patch(p) => Some(p.commit.clone()),
                _ => None,
            };
            match (sha, render_marked_patch(&panel_ui.git, true)) {
                (Some(sha), Some(patch)) => {
                    enqueue(panel_ui, model, GitOp::PatchToIndex { sha, patch })
                }
                _ => model.status = "no lines marked".into(),
            }
        }
        MenuChoice::PatchNewCommit => {
            let sha = match &panel_ui.git.flow {
                GitFlow::Patch(p) => Some(p.commit.clone()),
                _ => None,
            };
            match (sha, render_marked_patch(&panel_ui.git, true)) {
                (Some(sha), Some(patch)) => {
                    *ov.input = Some((
                        menu::InputOverlay::new("new commit message", ""),
                        GitInputKind::PatchSplit { sha, patch },
                    ));
                }
                _ => model.status = "no lines marked".into(),
            }
        }
        MenuChoice::PatchRemoveFromCommit => {
            let sha = match &panel_ui.git.flow {
                GitFlow::Patch(p) => Some(p.commit.clone()),
                _ => None,
            };
            match (sha, render_marked_patch(&panel_ui.git, true)) {
                (Some(sha), Some(patch)) => {
                    enqueue(panel_ui, model, GitOp::PatchRemoveFromCommit { sha, patch })
                }
                _ => model.status = "no lines marked".into(),
            }
        }
        MenuChoice::PatchReset => {
            if let GitFlow::Patch(p) = &mut panel_ui.git.flow {
                p.marks.clear();
                model.status = "patch reset".into();
            }
        }
        MenuChoice::DiffSwap => {
            model.status = "diff sides swap is not wired yet".into();
        }
        MenuChoice::DiffExit => {
            panel_ui.git.flow = GitFlow::None;
            panel_ui.git.diff_mark = None;
            model.status = "diff mode off".into();
        }
        MenuChoice::BisectStart => {
            panel_ui.git.flow = GitFlow::Bisect(gitui::BisectUi {
                bad_term: "bad".into(),
                good_term: "good".into(),
                ..Default::default()
            });
            enqueue(
                panel_ui,
                model,
                GitOp::BisectStart {
                    bad: None,
                    good: None,
                },
            );
        }
        MenuChoice::BisectMarkGood => enqueue(
            panel_ui,
            model,
            GitOp::BisectMark {
                term: "good".into(),
            },
        ),
        MenuChoice::BisectMarkBad => {
            enqueue(panel_ui, model, GitOp::BisectMark { term: "bad".into() })
        }
        MenuChoice::BisectSkip => enqueue(panel_ui, model, GitOp::BisectSkip),
        MenuChoice::BisectReset => enqueue(panel_ui, model, GitOp::BisectReset),
        MenuChoice::BranchDelete { name, force } => {
            enqueue(panel_ui, model, GitOp::DeleteBranch { name, force })
        }
        MenuChoice::BranchForcePush => enqueue(
            panel_ui,
            model,
            GitOp::Push {
                force: ForceMode::WithLease,
            },
        ),
        MenuChoice::BranchPush => enqueue(
            panel_ui,
            model,
            GitOp::Push {
                force: ForceMode::None,
            },
        ),
        MenuChoice::BranchPull => enqueue(panel_ui, model, GitOp::Pull),
        MenuChoice::BranchSetUpstream(name) => {
            let q = superzej_core::util::sh_quote(&name);
            enqueue(
                panel_ui,
                model,
                GitOp::Custom {
                    command: format!("git branch --set-upstream-to=origin/{q} {q}"),
                    capture: false,
                },
            );
        }
        MenuChoice::BranchRename(name) => {
            *ov.input = Some((
                menu::InputOverlay::new(format!("rename {name}"), name.clone()),
                GitInputKind::BranchRename { old: name },
            ));
        }
        MenuChoice::BranchMerge(name) => enqueue(panel_ui, model, GitOp::Merge { branch: name }),
        MenuChoice::BranchCreate => {
            *ov.input = Some((
                menu::InputOverlay::new("new branch name", ""),
                GitInputKind::BranchCreate,
            ));
        }
        MenuChoice::CherryContinue => enqueue(panel_ui, model, GitOp::CherryContinue),
        MenuChoice::CherryAbort => enqueue(panel_ui, model, GitOp::CherryAbort),
        MenuChoice::CherrySkip => enqueue(panel_ui, model, GitOp::CherrySkip),
        MenuChoice::RevertContinue => enqueue(panel_ui, model, GitOp::RevertContinue),
        MenuChoice::RevertAbort => enqueue(panel_ui, model, GitOp::RevertAbort),
        MenuChoice::BranchFetch => enqueue(panel_ui, model, GitOp::Fetch),
        MenuChoice::MergeContinue => enqueue(panel_ui, model, GitOp::MergeContinue),
        MenuChoice::MergeAbort => enqueue(panel_ui, model, GitOp::MergeAbort),
        MenuChoice::CustomCommand(i) => {
            if let Some(&idx) = ov.custom_cmds.get(i) {
                let prompts: Vec<(String, String)> = wires
                    .cfg
                    .git_commands
                    .get(idx)
                    .map(|c| {
                        c.prompts
                            .iter()
                            .map(|p| {
                                (
                                    p.key.clone(),
                                    p.title.clone().unwrap_or_else(|| p.key.clone()),
                                )
                            })
                            .collect()
                    })
                    .unwrap_or_default();
                match prompts.split_first() {
                    None => {
                        return run_custom_command(idx, Default::default(), panel_ui, model, wires);
                    }
                    Some(((key, title), rest)) => {
                        *ov.input = Some((
                            menu::InputOverlay::new(title.clone(), ""),
                            GitInputKind::CustomPrompt {
                                cmd: idx,
                                key: key.clone(),
                                collected: Default::default(),
                                remaining: rest.to_vec(),
                            },
                        ));
                    }
                }
            }
        }
        MenuChoice::ConfirmUndo | MenuChoice::ConfirmRedo => {
            if let Some((plan, _redo)) = pending_undo.take() {
                // Always autostash: the plan may hard-reset over a dirty
                // tree, and parking the dirt is strictly safer than failing.
                enqueue(
                    panel_ui,
                    model,
                    GitOp::UndoApply {
                        plan,
                        autostash: true,
                    },
                );
            }
        }
        MenuChoice::Confirm { tag: "git-op", .. } => {
            if let Some(op) = ov.confirm_op.take() {
                enqueue(panel_ui, model, op);
            }
        }
        MenuChoice::Confirm { .. } | MenuChoice::Dismiss => {
            *ov.confirm_op = None;
        }
    }
    GitAfter::None
}

/// Resolve a submitted git input overlay into its op (or the next prompt of
/// a custom-command chain).
fn handle_git_input_submit(
    kind: GitInputKind,
    text: String,
    panel_ui: &mut crate::panel::PanelUi,
    model: &mut FrameModel,
    wires: &GitWires<'_>,
    ov: &mut GitOverlays<'_>,
) -> GitAfter {
    let enqueue = |panel_ui: &mut crate::panel::PanelUi, model: &mut FrameModel, op: GitOp| {
        enqueue_git_op(
            op,
            &mut panel_ui.git,
            &mut model.status,
            wires.session,
            wires.cfg.git.override_gpg,
            wires.op_tx,
            wires.waker,
        );
    };
    let trimmed = text.trim().to_string();
    let require = |model: &mut FrameModel| {
        if trimmed.is_empty() {
            model.status = "empty input — cancelled".into();
            true
        } else {
            false
        }
    };
    match kind {
        GitInputKind::Commit => {
            if !require(model) {
                enqueue(panel_ui, model, GitOp::Commit { message: text });
            }
        }
        GitInputKind::Reword { sha } => {
            if !require(model) {
                enqueue(panel_ui, model, GitOp::Reword { sha, message: text });
            }
        }
        GitInputKind::StashPush => {
            if !require(model) {
                enqueue(panel_ui, model, GitOp::StashPush { message: text });
            }
        }
        GitInputKind::PatchSplit { sha, patch } => {
            if !require(model) {
                enqueue(
                    panel_ui,
                    model,
                    GitOp::PatchSplit {
                        sha,
                        patch,
                        message: text,
                    },
                );
            }
        }
        GitInputKind::BranchCreate => {
            if !require(model) {
                enqueue(
                    panel_ui,
                    model,
                    GitOp::CreateBranch {
                        name: trimmed,
                        base: "HEAD".into(),
                    },
                );
            }
        }
        GitInputKind::BranchRename { old } => {
            if !require(model) {
                let from = superzej_core::util::sh_quote(&old);
                let to = superzej_core::util::sh_quote(&trimmed);
                enqueue(
                    panel_ui,
                    model,
                    GitOp::Custom {
                        command: format!("git branch -m {from} {to}"),
                        capture: false,
                    },
                );
            }
        }
        GitInputKind::CustomPrompt {
            cmd,
            key,
            mut collected,
            mut remaining,
        } => {
            collected.insert(key, text);
            if remaining.is_empty() {
                return run_custom_command(cmd, collected, panel_ui, model, wires);
            }
            let (next_key, title) = remaining.remove(0);
            *ov.input = Some((
                menu::InputOverlay::new(title, ""),
                GitInputKind::CustomPrompt {
                    cmd,
                    key: next_key,
                    collected,
                    remaining,
                },
            ));
        }
    }
    GitAfter::None
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

/// Generic repeat-drainer: starting from a count of 1 (the caller already
/// consumed `first`), keep pulling from `next` as long as `is_repeat` returns
/// `true` for the incoming event. Returns `(total_count, leftover)` where
/// `leftover` is the first non-matching event to push back, or `None` if the
/// queue was drained.
///
/// Both `drain_key_repeats` and `drain_wheel_ticks` are thin wrappers around
/// this so the coalescing logic lives in one place.
fn drain_event_repeats(
    mut is_repeat: impl FnMut(&InputEvent) -> bool,
    mut next: impl FnMut() -> Option<InputEvent>,
) -> (usize, Option<InputEvent>) {
    let mut count = 1usize;
    loop {
        match next() {
            Some(ev) if is_repeat(&ev) => count += 1,
            Some(other) => return (count, Some(other)),
            None => return (count, None),
        }
    }
}

/// Count immediately-available repeats of `first` (same key + modifiers);
/// the first NON-identical event is returned for requeueing. `next` yields
/// `None` when the input queue is drained. Coalescing a held key's backlog
/// into one application kills scroll inertia without dropping other events.
fn drain_key_repeats(
    first: &termwiz::input::KeyEvent,
    next: impl FnMut() -> Option<InputEvent>,
) -> (usize, Option<InputEvent>) {
    drain_event_repeats(
        |ev| {
            matches!(
                ev,
                InputEvent::Key(k) if k.key == first.key && k.modifiers == first.modifiers
            )
        },
        next,
    )
}

/// Drain immediately-available wheel events that match `up` direction.
/// Returns `(tick_count, leftover)` — the opposite-direction wheel or any
/// non-wheel event is returned as leftover so the caller can requeueit.
///
/// Using `Duration::ZERO` for the `next` poll drains everything already
/// queued even when the OS delivers events one-per-`poll_input` return,
/// because the spin loop keeps pulling until the kernel returns nothing.
fn drain_wheel_ticks(
    up: bool,
    next: impl FnMut() -> Option<InputEvent>,
) -> (usize, Option<InputEvent>) {
    use termwiz::input::MouseButtons;
    drain_event_repeats(
        move |ev| {
            matches!(
                ev,
                InputEvent::Mouse(m)
                    if m.mouse_buttons.contains(MouseButtons::VERT_WHEEL)
                        && m.mouse_buttons.contains(MouseButtons::WHEEL_POSITIVE) == up
            )
        },
        next,
    )
}

/// Run the detected test command off-thread; results (parsed indicator
/// lines + summary) ride the channel back to the loop with a waker pulse.
/// Run a test task off the loop, capped + single-flight (see `crate::task`),
/// delivering a `TaskOutcome` and pulsing the waker. A global semaphore bounds
/// concurrent jobs.
#[allow(clippy::too_many_arguments)]
fn spawn_test_run_task(
    tx: tokio_mpsc::UnboundedSender<crate::task::TaskOutcome>,
    waker: TerminalWaker,
    worktree: std::path::PathBuf,
    generation: u64,
    task_spec: crate::panel::TestTask,
    limits: superzej_core::config::LimitsConfig,
    sem: std::sync::Arc<tokio::sync::Semaphore>,
) {
    tokio::spawn(async move {
        let _permit = sem.acquire_owned().await;
        if let Ok(outcome) = tokio::task::spawn_blocking(move || {
            crate::task::run_task(worktree, generation, task_spec, &limits)
        })
        .await
        {
            let _ = tx.send(outcome);
            let _ = waker.wake();
        }
    });
}

/// Lazily discover a task's test targets off the loop (capped, single-flight).
#[allow(clippy::too_many_arguments)]
fn spawn_test_discovery(
    tx: tokio_mpsc::UnboundedSender<crate::task::DiscoveryOutcome>,
    waker: TerminalWaker,
    worktree: std::path::PathBuf,
    generation: u64,
    task_spec: crate::panel::TestTask,
    limits: superzej_core::config::LimitsConfig,
    sem: std::sync::Arc<tokio::sync::Semaphore>,
) {
    tokio::spawn(async move {
        let _permit = sem.acquire_owned().await;
        if let Ok(result) = tokio::task::spawn_blocking(move || {
            crate::task::discover_tests(worktree, generation, task_spec, &limits)
        })
        .await
        {
            let _ = tx.send(result);
            let _ = waker.wake();
        }
    });
}

/// Load cached test state for a worktree and detect its task if none is known.
/// Reading only — never spawns a run (no auto-run).
fn sync_tests_for_worktree(
    ui: &mut crate::panel::PanelUi,
    worktree: &std::path::Path,
    cfg: &superzej_core::config::Config,
) {
    let key = worktree.to_string_lossy();
    if let Some(cache) = superzej_core::db::Db::open()
        .ok()
        .and_then(|db| db.get_test_cache(&key).ok().flatten())
        .and_then(|(json, _)| serde_json::from_str::<crate::panel::TestCache>(&json).ok())
    {
        ui.tests.apply_cache(cache);
    }
    if ui.tests.task.is_none() {
        ui.tests.task = crate::task::detect_test_task(worktree, cfg);
    }
}

fn persist_tests_for_worktree(ui: &crate::panel::PanelUi, worktree: &str) {
    if let Ok(json) = serde_json::to_string(&ui.tests.to_cache())
        && let Ok(db) = superzej_core::db::Db::open()
    {
        let _ = db.put_test_cache(worktree, &json);
    }
}

fn test_shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

/// Resolve a (possibly worktree-relative) failure path to an absolute path the
/// editor can open.
fn resolve_loc_path(worktree: &std::path::Path, path: &str) -> String {
    let p = std::path::Path::new(path);
    if p.is_absolute() {
        path.to_string()
    } else {
        worktree.join(path).to_string_lossy().into_owned()
    }
}

/// Where to open the selected test: its captured `file:line` if present, else
/// located by name with ripgrep/grep so *any* test opens (not just failures).
fn resolve_open_target(
    ui: &crate::panel::PanelUi,
    worktree: &std::path::Path,
) -> Option<(String, usize)> {
    if let Some(node) = ui.tests.selected_node() {
        if let Some(loc) = &node.location {
            return Some((resolve_loc_path(worktree, &loc.path), loc.line));
        }
        return locate_test_in_repo(worktree, &node.id)
            .map(|(rel, line)| (resolve_loc_path(worktree, &rel), line));
    }
    None
}

fn on_path(bin: &str) -> bool {
    std::env::var_os("PATH")
        .map(|p| std::env::split_paths(&p).any(|d| d.join(bin).is_file()))
        .unwrap_or(false)
}

/// Best-effort search for a test definition by name (ripgrep, then grep).
fn locate_test_in_repo(worktree: &std::path::Path, test_id: &str) -> Option<(String, usize)> {
    let rg = on_path("rg");
    for pat in crate::panel::locate_regexes(test_id) {
        let out = if rg {
            std::process::Command::new("rg")
                .args([
                    "--no-heading",
                    "--line-number",
                    "--max-count",
                    "1",
                    "-e",
                    &pat,
                    ".",
                ])
                .current_dir(worktree)
                .output()
        } else {
            std::process::Command::new("grep")
                .args(["-rnE", "--max-count=1", &pat, "."])
                .current_dir(worktree)
                .output()
        };
        let Ok(out) = out else { continue };
        let text = String::from_utf8_lossy(&out.stdout);
        if let Some(hit) = text.lines().next() {
            let mut it = hit.trim_start_matches("./").splitn(3, ':');
            if let (Some(path), Some(line)) = (it.next(), it.next())
                && let Ok(n) = line.parse::<usize>()
            {
                return Some((path.to_string(), n));
            }
        }
    }
    None
}

/// Which slice of the suite a tests-section run key targets.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TestRun {
    /// `r`: the selected test (whole suite when none is selected).
    Selected,
    /// `f`: the first failing test (falls back to the selection).
    Failed,
    /// `R`: everything.
    All,
}

/// For `Selected`/`Failed`, narrow the base task command to the selected/failed
/// test where the runner supports it; otherwise run the whole suite.
fn test_task_for_run(
    ui: &crate::panel::PanelUi,
    kind: TestRun,
    base: crate::panel::TestTask,
) -> crate::panel::TestTask {
    use crate::panel::{TestNodeKind, TestState};
    let selected = ui
        .tests
        .selected_node()
        .filter(|n| n.kind == TestNodeKind::Test);
    let failed = ui
        .tests
        .nodes
        .iter()
        .find(|n| n.kind == TestNodeKind::Test && n.state == TestState::Fail);
    let target = match kind {
        TestRun::Selected => selected,
        TestRun::Failed => failed.or(selected),
        TestRun::All => None,
    };
    let Some(target) = target else {
        return base;
    };
    let mut task = base;
    match task.matcher.as_str() {
        "cargo-test" | "nextest" => {
            task.command = format!("{} {}", task.command, test_shell_quote(&target.id))
        }
        "go-test" => {
            task.command = format!("{} -run {}", task.command, test_shell_quote(&target.id))
        }
        "pytest" | "swift" => {
            task.command = format!("{} {}", task.command, test_shell_quote(&target.id))
        }
        "vitest" | "jest" | "javascript" => {
            task.command = format!("{} {}", task.command, test_shell_quote(&target.id))
        }
        "nix-flake" => {
            let attr = format!("checks.{}", target.id.replace("::", "."));
            task.command = format!("nix build -L {}", test_shell_quote(&format!(".#{attr}")));
        }
        _ => {}
    }
    task.name = format!("{} ({})", task.name, target.label);
    task
}

/// Entering the Tests tab: ensure cached state is loaded and kick a lazy,
/// capped discovery if we haven't discovered targets yet. Never runs tests.
#[allow(clippy::too_many_arguments)]
fn maybe_discover_tests(
    ui: &mut crate::panel::PanelUi,
    session: &crate::session::Session,
    generation: &mut u64,
    tx: tokio_mpsc::UnboundedSender<crate::task::DiscoveryOutcome>,
    waker: TerminalWaker,
    cfg: &superzej_core::config::Config,
    sem: std::sync::Arc<tokio::sync::Semaphore>,
) {
    let wt = active_tab_path(session);
    sync_tests_for_worktree(ui, &wt, cfg);
    // Skip discovery when a prior run/discovery already covered this manifest
    // state: a fresh fingerprint match means nothing relevant changed, so reuse
    // the cache and spawn no subprocess at all.
    let current_fp = crate::task::manifest_fingerprint(&wt);
    let fp_changed = ui.tests.fingerprint != current_fp;
    if ui.tests.task.is_some() && !ui.tests.discovering && (!ui.tests.discovered || fp_changed) {
        *generation += 1;
        ui.tests.discovering = true;
        // Record the fingerprint up front so we don't re-trigger while the
        // discovery we just launched is still in flight.
        ui.tests.fingerprint = current_fp;
        if let Some(task) = ui.tests.task.clone() {
            spawn_test_discovery(tx, waker, wt, *generation, task, cfg.limits.clone(), sem);
        }
    }
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
        let spec = crate::agent::launch_spec(cfg, &wt, None, "shell")?;
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
        let spec = crate::agent::launch_spec(cfg, &wt, None, "yazi").ok()?;
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

// ── Search helpers ────────────────────────────────────────────────────────────

/// Build the list of `(pane_id, label, &HistoryBuffer)` sources for the search
/// engine. The `scope` determines which panes from `session` are included;
/// only live panes present in `panes` contribute (closed panes are skipped).
///
/// For `Pane(id)` scope we return at most one entry. For broader scopes we
/// walk the session tree in display order.
fn build_search_sources<'a>(
    scope: superzej_core::search::SearchScope,
    session: &crate::session::Session,
    panes: &'a Panes,
    focused: u32,
) -> Vec<(u32, String, &'a superzej_core::history::HistoryBuffer)> {
    use superzej_core::search::SearchScope;
    let mut out = Vec::new();

    match scope {
        SearchScope::Pane(id) => {
            if let Some(p) = panes.table.get(&id) {
                out.push((id, "pane".to_string(), &p.history));
            }
        }
        SearchScope::Tab => {
            // Collect panes in the active tab only.
            if let Some(tab) = session.active_tab() {
                for pid in tab.center.pane_ids() {
                    if let Some(p) = panes.table.get(&pid) {
                        let label = if pid == focused {
                            "focused pane".to_string()
                        } else {
                            format!("pane {pid}")
                        };
                        out.push((pid, label, &p.history));
                    }
                }
            }
        }
        SearchScope::Worktree => {
            // All tabs in the active worktree.
            if let Some(g) = session.active_group() {
                for (ti, tab) in g.tabs.iter().enumerate() {
                    let tab_label = format!("tab {}", ti + 1);
                    for pid in tab.center.pane_ids() {
                        if let Some(p) = panes.table.get(&pid) {
                            out.push((pid, tab_label.clone(), &p.history));
                        }
                    }
                }
            }
        }
        SearchScope::Workspace => {
            // All worktrees in the active session.
            for g in &session.worktrees {
                let wt_name = g.path.rsplit('/').next().unwrap_or(&g.name).to_string();
                for (ti, tab) in g.tabs.iter().enumerate() {
                    let label = format!("tab {} · {}", ti + 1, wt_name);
                    for pid in tab.center.pane_ids() {
                        if let Some(p) = panes.table.get(&pid) {
                            out.push((pid, label.clone(), &p.history));
                        }
                    }
                }
            }
        }
        SearchScope::Profile => {
            // Same as Workspace for now (profile == session in the current model;
            // when profiles land this will walk across sessions).
            for g in &session.worktrees {
                let wt_name = g.path.rsplit('/').next().unwrap_or(&g.name).to_string();
                for (ti, tab) in g.tabs.iter().enumerate() {
                    let label = format!("tab {} · {}", ti + 1, wt_name);
                    for pid in tab.center.pane_ids() {
                        if let Some(p) = panes.table.get(&pid) {
                            out.push((pid, label.clone(), &p.history));
                        }
                    }
                }
            }
        }
    }
    out
}

/// Compute the overlay rect: centered horizontally, upper third of the center.
fn search_overlay_rect(center: Rect) -> Rect {
    center // run.rs passes center directly; open_layer handles sizing
}

/// Jump to the matched line: switch tab if needed, scroll the pane so the
/// matched line is visible, focus the center terminal.
fn apply_search_jump(
    m: superzej_core::search::SearchMatch,
    session: &mut crate::session::Session,
    panes: &mut Panes,
    focus: &mut crate::focus::FocusState,
    model: &mut crate::chrome::FrameModel,
    sb: &mut SidebarState,
    need_relayout: &mut bool,
) {
    use crate::focus::Zone;

    // Find which tab owns this pane and switch to it if not already active.
    let mut switched = false;
    'outer: for (gi, g) in session.worktrees.iter().enumerate() {
        for (ti, tab) in g.tabs.iter().enumerate() {
            if tab.center.pane_ids().contains(&m.pane_id) {
                if gi != session.active {
                    session.switch_to(gi);
                    switched = true;
                }
                if gi == session.active {
                    let g = &mut session.worktrees[gi];
                    if g.active_tab != ti {
                        g.active_tab = ti;
                        switched = true;
                    }
                }
                break 'outer;
            }
        }
    }
    if switched {
        refresh_tab_model(model, session, sb);
        *need_relayout = true;
    }

    // Focus the center terminal.
    focus.zone = Zone::Center;
    if let Some(tab) = session.active_tab_mut() {
        tab.focused_pane = m.pane_id;
    }

    // Scroll the pane to bring the matched line into view.
    // `line_idx` is the 0-based index in the history ring (oldest surviving = 0).
    // The history ring may have evicted older lines; the distance from the tail
    // is `history.len() - 1 - line_idx` lines up from the live tail.
    if let Some(p) = panes.table.get_mut(&m.pane_id) {
        let buf_len = p.history.len();
        if buf_len > 0 && m.line_idx < buf_len {
            let lines_from_tail = buf_len - 1 - m.line_idx;
            // Reset first so we're always measuring from the tail.
            // Scroll to tail first (huge down scroll clamps to 0).
            p.scroll_down(usize::MAX / 2);
            if lines_from_tail > 0 {
                p.scroll_up(lines_from_tail);
            }
        }
    }
}

fn drawer_cancel_key(key: &KeyCode, modifiers: Modifiers) -> bool {
    if modifiers.contains(Modifiers::CTRL) || modifiers.contains(Modifiers::ALT) {
        return false;
    }
    crate::input::is_escape_key(key) || matches!(key, KeyCode::Char('q') | KeyCode::Char('Q'))
}

fn palette_cancel_key(
    palette: &crate::palette::Palette,
    key: &KeyCode,
    modifiers: Modifiers,
) -> bool {
    if crate::input::is_escape_key(key) {
        return true;
    }
    if modifiers.contains(Modifiers::CTRL)
        && matches!(
            key,
            KeyCode::Char('c') | KeyCode::Char('C') | KeyCode::Char('g') | KeyCode::Char('G')
        )
    {
        return true;
    }
    if modifiers.contains(Modifiers::CTRL) || modifiers.contains(Modifiers::ALT) {
        return false;
    }
    palette.query().is_empty()
        && matches!(key, KeyCode::Char('q') | KeyCode::Char('Q'))
        && palette
            .matches()
            .iter()
            .all(|item| item.key.starts_with("font:"))
}

fn persist_session_layout(session: &crate::session::Session) {
    if let Ok(db) = superzej_core::db::Db::open() {
        let _ = session.persist(&db, &session.id, now_secs());
    }
}

/// Attach a freshly-created worktree's agent pane: spawn the pre-resolved
/// launch spec (openpty+exec — fast, the blocking sandbox/compose work already
/// ran on the wizard worker) into the tab named `tab_name` and point that
/// tab's center at the live pane so `materialize` won't also spawn a plain
/// shell. No-op (returns false) if the tab is gone.
fn attach_agent_pane(
    session: &mut crate::session::Session,
    panes: &mut Panes,
    tab_name: &str,
    spec: &crate::agent::LaunchSpec,
    center: Rect,
) -> bool {
    let Some(gi) = session.worktrees.iter().position(|g| g.name == tab_name) else {
        return false;
    };
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
    mut metrics_rx: tokio_mpsc::UnboundedReceiver<crate::metrics::MetricsState>,
    stats_interval_ms: std::sync::Arc<std::sync::atomic::AtomicU64>,
    stats_live: std::sync::Arc<std::sync::atomic::AtomicBool>,
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
    // The accordion's width state (cycled by `e`: Normal → Half → Full). The
    // pre-render detector keeps this in sync with `panel_ui.width` so every
    // toggle path triggers a chrome recompute.
    let mut panel_width = layout::PanelWidth::Normal;
    // Set while the panel was popped up by Ctrl+→ at the center edge; holds
    // the (want_panel, panel_forced) pair to restore when focus leaves it.
    let mut panel_auto_revealed: Option<(bool, bool)> = None;
    // Inline hunk previews arrive from background `git diff` fetches tagged
    // with the hydration generation at request time; `hunk_inflight` dedupes
    // re-selects while a fetch is still out.
    let (hunk_tx, mut hunk_rx) =
        tokio_mpsc::unbounded_channel::<(u64, String, Vec<superzej_svc::git::Hunk>)>();
    // The git mutation runner + the line-cursor document fetches (staging
    // diff, drilled-commit files, patch doc). Results are tagged with
    // `panel_ui.git.op_gen` so a worktree switch kills strays on arrival.
    let (gitop_tx, mut gitop_rx) = tokio_mpsc::unbounded_channel::<GitOpDone>();
    let (gitdoc_tx, mut gitdoc_rx) = tokio_mpsc::unbounded_channel::<(u64, GitDoc)>();
    // The open git option/confirm menu and text-input overlay — held like
    // the palette (Option, keys first, painted last).
    let mut active_menu: Option<MenuOverlay> = None;
    let mut git_input: Option<(menu::InputOverlay, GitInputKind)> = None;
    let mut host_input: Option<(menu::InputOverlay, HostInputKind)> = None;
    // A live rebase_status read is out (dedupes the safety-net re-kicks).
    let mut rebase_sync_inflight = false;
    // A computed undo/redo plan awaiting its confirm pick.
    let mut pending_undo: Option<(superzej_core::reflog::UndoPlan, bool)> = None;
    // A destructive git op parked behind the open confirm menu.
    let mut pending_confirm_op: Option<GitOp> = None;
    // `cfg.git_commands` indices behind the open custom-commands menu rows.
    let mut custom_menu_cmds: Vec<usize> = Vec::new();
    // Pane launch specs resolved off-thread: `launch_spec` walks the sandbox
    // chain (podman ensure can pull an image — seconds to minutes on a cold
    // or wedged runtime) and MUST NOT run on the loop. The loop requests
    // specs for a (worktree, tab)'s missing leaves, keeps the splash up, and
    // finishes the spawn (openpty+exec, fast) when they land. Stale results
    // (worktree/tab changed mid-flight) are dropped on arrival.
    type SpecBatch = (
        String,
        usize,
        std::result::Result<Vec<(u32, crate::agent::LaunchSpec)>, String>,
    );
    let (spec_tx, mut spec_rx) = tokio_mpsc::unbounded_channel::<SpecBatch>();
    let mut materialize_inflight: Option<(String, usize)> = None;
    // The new-worktree wizard (Alt+w) + its creation pipeline. The worker
    // speculatively creates the worktree under the pregenerated name while
    // the wizard is open; `wizard_cmd_tx` carries the wizard's decisions to
    // it, `create_rx` carries progress events back. One creation at a time;
    // `create_gen` kills a cancelled run's stragglers on arrival.
    let (create_tx, mut create_rx) = tokio_mpsc::unbounded_channel::<wizard::CreateEvent>();
    let mut wizard_ui: Option<wizard::NewWorktreeWizard> = None;
    let mut wizard_cmd_tx: Option<std::sync::mpsc::Sender<wizard::WizardCmd>> = None;
    let mut creating: Option<wizard::CreationProgress> = None;
    let mut create_gen: u64 = 0;
    // Test-explorer results from the background runner/discoverer (capped,
    // single-flight). Two channels: run outcomes and discovery outcomes.
    let (test_run_tx, mut test_run_rx) =
        tokio_mpsc::unbounded_channel::<crate::task::TaskOutcome>();
    let (test_discovery_tx, mut test_discovery_rx) =
        tokio_mpsc::unbounded_channel::<crate::task::DiscoveryOutcome>();
    let mut test_generation: u64 = 0;
    let mut loaded_tests_worktree = String::new();
    // Bounds concurrent test/discovery jobs across worktrees so explicit runs
    // can't collectively pin the machine. superzej never auto-runs tests.
    let test_sem = std::sync::Arc::new(tokio::sync::Semaphore::new(
        keymap.config().limits.test_max_parallel.max(1),
    ));
    // Paths whose hunk fetch is still in flight (dedupes select storms).
    let mut hunk_inflight: std::collections::HashSet<String> = std::collections::HashSet::new();
    // Sidebar interaction + persisted view state (collapse/sort/pins/width),
    // and the right panel's persisted accordion state (open section + wide
    // mode survive restarts; row mode is intentionally transient).
    let mut sb = SidebarState::default();
    let mut panel_ui = crate::panel::PanelUi::default();
    if let Ok(db) = superzej_core::db::Db::open() {
        sb.load(&db, &session.id);
        for (key, value) in db.ui_state_in_scope("panel").unwrap_or_default() {
            match key.as_str() {
                "open" => {
                    if let Some(s) = crate::panel::Section::from_key(&value) {
                        panel_ui.open = s;
                    }
                }
                // Back-compat: older builds stored a boolean "expanded".
                "expanded" if value == "1" => {
                    panel_ui.width = layout::PanelWidth::Half;
                }
                "width" => {
                    panel_ui.width = layout::PanelWidth::from_key(&value);
                }
                _ => {}
            }
        }
    }
    // `[panel] sections` reorders/hides accordions; a persisted open section
    // the config hid snaps to the first visible one. The keys section renders
    // the cheatsheet groups cached here (refreshed on config reload).
    panel_ui.set_order(crate::panel::resolve_order(keymap.config()));
    panel_ui.docs.cfg_keys = crate::keyhint::cheatsheet_groups(keymap.config());
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
        panel_width,
        sidebar_cols,
        zoom,
        &supervisor,
    );
    sb.rebuild(&mut model, &session);
    let mut dirty = true;
    // One zone owns the keyboard at any time; Ctrl+g toggles the keybind lock.
    // `sb.focused` / `model.panel_focused` / `model.center_focused` mirror it.
    let mut focus = crate::focus::FocusState::default();
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
    // Our own Change→escape serializer (undercurl + underline-color capable);
    // `SUPERZEJ_RENDERER=termwiz` falls back to the stock renderer.
    let mut wire_renderer = crate::wire::WireRenderer::new();
    let use_termwiz_renderer = crate::wire::use_termwiz_renderer();
    let mut palette: Option<crate::palette::Palette> = None;
    // Search overlay state: lives here so it survives across loop iterations.
    let mut search: Option<crate::search::SearchOverlay> = None;
    // Panel document payloads (git calendar/log, the selected file's diff)
    // are fetched off-loop on section entry and tagged with `docs_gen`; a
    // worktree switch bumps the generation so stale results die on arrival.
    // The git payload is cached per worktree (it's a calendar — cheap to
    // keep); the diff doc refetches on every full-view entry.
    let (docs_tx, mut docs_rx) =
        tokio_mpsc::unbounded_channel::<(u64, crate::panel::docs::DocsPayload)>();
    let mut docs_gen: u64 = 0;
    // The transient which-key popup (set while a multi-key prefix is pending).
    let mut which_key: Vec<crate::keyhint::HintRow> = Vec::new();
    let mut which_key_prefix = String::new();
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
                panel_width,
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
        // Leaving the panel drops back to section mode (row walk + change
        // selection cleared — open section and cursor kept). Central, so
        // EVERY exit path (Esc, Ctrl+←, mouse, Alt+s, …) behaves identically.
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
                    panel_width,
                    sidebar_cols,
                    zoom,
                    &supervisor,
                );
                need_relayout = true;
                dirty = true;
            }
        }
        // Entering the panel lands directly on the open section's items: the
        // cursor walks rows immediately (Down/j), with Shift+Down/j hopping
        // between sections. No separate "press Enter to enter rows" step.
        if prev_zone != crate::focus::Zone::Panel && focus.panel() {
            panel_ui.row_mode = true;
            let (pc, pr) = panel_geom(&chrome);
            let max =
                crate::panel::frame::actionable_rows(&model, &panel_ui, pc, pr).saturating_sub(1);
            panel_ui.cursor = panel_ui.cursor.min(max);
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
                panel_width,
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
            // Load this worktree's cached test state (most-recent status) so the
            // Tests tab shows it instantly; discovery stays lazy (on tab open).
            panel_ui.tests = crate::panel::TestPanelState::default();
            sync_tests_for_worktree(&mut panel_ui, &current_worktree, keymap.config());
            loaded_tests_worktree = current_worktree.to_string_lossy().into_owned();
            // Immediate hydrate for the newly-focused worktree. Clear the stale
            // panel data immediately so the panel shows the new worktree's branch
            // name and empty changes rather than the previous worktree's data
            // while the background hydration is in flight.
            model.panel = crate::panel::PanelData::default();
            // The old worktree's hunk previews are meaningless here: drop them and
            // raise the acceptance cutoff so in-flight fetches die on arrival.
            hydration_gen += 1;
            panel_ui.hunks.clear();
            hunk_inflight.clear();
            materialize_inflight = None;
            panel_ui.chg_sel = None;
            panel_ui.hunks_gen = hydration_gen;
            // Git interaction state is per-worktree: cursors, flows, marks
            // and fetched docs all reset; `op_gen` bumps so in-flight op/doc
            // results die on arrival. Overlays target the old worktree.
            panel_ui.git.reset_for_worktree();
            rebase_sync_inflight = false;
            active_menu = None;
            git_input = None;
            pending_undo = None;
            pending_confirm_op = None;
            custom_menu_cmds.clear();
            // Panel documents are per-worktree: drop the caches, raise the
            // acceptance cutoff so in-flight fetches die on arrival, and
            // refetch whatever the open (section, width) still needs (the
            // body shows "loading…" until fresh data lands).
            docs_gen += 1;
            panel_ui.docs.git = None;
            panel_ui.docs.diff = None;
            panel_ui.scroll = 0;
            panel_ui.diff_hunk = 0;
            sync_panel_docs(&mut panel_ui, &model, &session, docs_gen, &docs_tx, &waker);
            spawn_model_hydration(
                model_tx.clone(),
                hydration_gen,
                session.clone(),
                Some(waker.clone()),
                crate::hydrate::HydrateHints {
                    open: panel_ui.open,
                    expanded: panel_ui.width.is_expanded(),
                },
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
            for (wt, ti, missing) in prewarm_requests(&panes, &mut session) {
                let cfg = keymap.config().clone();
                let tx = spec_tx.clone();
                let wk = waker.clone();
                task::spawn_blocking(move || {
                    let specs = crate::agent::launch_spec(&cfg, &wt, None, "shell")
                        .map(|spec| missing.into_iter().map(|id| (id, spec.clone())).collect())
                        .map_err(|e| e.to_string());
                    if tx.send((wt, ti, specs)).is_ok() {
                        let _ = wk.wake();
                    }
                });
            }
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
                panel_width,
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
            } else {
                // Two-phase materialize: request launch specs off-thread (the
                // sandbox ensure inside `launch_spec` can block on podman for
                // seconds to minutes), spawn when they land. One request per
                // (worktree, tab) at a time.
                let missing = panes.missing_leaves(&session.worktrees[session.active].tabs[ti]);
                let key = (path.clone(), ti);
                if !missing.is_empty() && materialize_inflight.as_ref() != Some(&key) {
                    materialize_inflight = Some(key);
                    let cfg = keymap.config().clone();
                    let tx = spec_tx.clone();
                    let wk = waker.clone();
                    let wt = path.clone();
                    task::spawn_blocking(move || {
                        let specs = crate::agent::launch_spec(&cfg, &wt, None, "shell")
                            .map(|spec| missing.into_iter().map(|id| (id, spec.clone())).collect())
                            .map_err(|e| e.to_string());
                        if tx.send((wt, ti, specs)).is_ok() {
                            let _ = wk.wake();
                        }
                    });
                }
            }
        }
        // The accordion's width (Normal → Half → Full) drives the chrome
        // geometry. Keep this before the relayout gate so the resized center
        // pane PTYs match the frame that is about to render; otherwise the
        // event loop can block with a stale PTY width after the panel
        // retracts.
        if panel_ui.width != panel_width {
            panel_width = panel_ui.width;
            chrome = compute_chrome(
                cols,
                rows,
                want_sidebar,
                want_panel,
                panel_forced,
                panel_width,
                sidebar_cols,
                zoom,
                &supervisor,
            );
            need_relayout = true;
            dirty = true;
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
        // Discovery results: seed newly-found tests as Unknown (merge, not
        // replace). Stale generations / other worktrees are dropped.
        while let Ok(d) = test_discovery_rx.try_recv() {
            if d.generation == test_generation && d.worktree == loaded_tests_worktree {
                panel_ui.tests.discovering = false;
                panel_ui.tests.task = Some(d.task);
                if let Some(err) = d.error {
                    panel_ui.tests.summary.error = Some(err);
                } else {
                    panel_ui.tests.merge_discovered(&d.nodes);
                    panel_ui.tests.summary.error = None;
                }
                persist_tests_for_worktree(&panel_ui, &d.worktree);
                dirty = true;
            }
        }
        // Run results: the latest run upserts each reported test's status while
        // leaving other tests' most-recent status intact.
        while let Ok(outcome) = test_run_rx.try_recv() {
            if outcome.generation == test_generation && outcome.worktree == loaded_tests_worktree {
                panel_ui.tests.running = false;
                panel_ui.tests.task = Some(outcome.task.clone());
                panel_ui
                    .tests
                    .merge_results(&crate::task::parse_task_outcome(&outcome));
                if outcome.exit_code != Some(0) && panel_ui.tests.summary.failed == 0 {
                    panel_ui.tests.summary.failed = 1;
                }
                panel_ui.tests.summary.error = outcome
                    .truncated
                    .then(|| "output truncated before parsing completed".to_string());
                panel_ui
                    .tests
                    .push_history(crate::testkit::model::TestRunRec {
                        at: superzej_core::util::now(),
                        passed: panel_ui.tests.summary.passed,
                        failed: panel_ui.tests.summary.failed,
                        skipped: panel_ui.tests.summary.skipped,
                        duration_ms: outcome.duration_ms as u64,
                        branch: model.panel.branch.clone(),
                    });
                persist_tests_for_worktree(&panel_ui, &outcome.worktree);
                model.status = format!(
                    "Tests finished in {:.1}s: {}",
                    outcome.duration_ms as f64 / 1000.0,
                    panel_ui.tests.summary.label()
                );
                dirty = true;
            }
        }

        // Bank background hunk previews. Strays from before a worktree switch
        // are dropped (`hunks_gen` records the acceptance cutoff); a fetch
        // outliving an ordinary refresh is fine — same worktree, same diff.
        while let Ok((generation, path, hunks)) = hunk_rx.try_recv() {
            hunk_inflight.remove(&path);
            if generation >= panel_ui.hunks_gen {
                panel_ui.hunks.insert(path, hunks);
                dirty = true;
            }
        }

        // Finished git mutations: clear the pending lock, surface the
        // outcome, close ended flows, and rehydrate (even failures may have
        // half-moved state). Stale generations died with their worktree.
        while let Ok(done) = gitop_rx.try_recv() {
            if done.generation != panel_ui.git.op_gen {
                continue;
            }
            panel_ui.git.pending = None;
            let mut rehydrate = true;
            match done.result {
                GitOpResult::Ok(note) => {
                    model.status = note.unwrap_or_else(|| format!("{} ✓", done.label));
                    match done.flow_end {
                        FlowEnd::Rebase => {
                            if matches!(panel_ui.git.flow, GitFlow::Rebase(_)) {
                                panel_ui.git.flow = GitFlow::None;
                                if panel_ui.git.focus == GitView::RebaseTodo {
                                    panel_ui.git.focus = GitView::Commits;
                                }
                            }
                        }
                        FlowEnd::Bisect => {
                            if matches!(panel_ui.git.flow, GitFlow::Bisect(_)) {
                                panel_ui.git.flow = GitFlow::None;
                            }
                        }
                        FlowEnd::Patch => {
                            if matches!(panel_ui.git.flow, GitFlow::Patch(_)) {
                                panel_ui.git.flow = GitFlow::None;
                                if matches!(
                                    panel_ui.git.focus,
                                    GitView::PatchBuilding | GitView::CommitFiles
                                ) {
                                    panel_ui.git.focus = GitView::Commits;
                                }
                            }
                        }
                        FlowEnd::None => {}
                    }
                    if done.clear_clipboard {
                        panel_ui.git.clipboard.clear();
                    }
                    // A landed stage/discard moves lines between panes:
                    // refetch the staging doc the cursor is parked on.
                    if let Some(s) = &panel_ui.git.staging {
                        spawn_stage_doc_fetch(
                            panel_ui.git.op_gen,
                            &session,
                            s.path.clone(),
                            s.pane,
                            &gitdoc_tx,
                            &waker,
                        );
                    }
                    // A landed live-todo rewrite changed the disk todo: the
                    // editor's baseline is stale until the re-read lands.
                    if done.label == "editing todo"
                        && let GitFlow::Rebase(r) = &mut panel_ui.git.flow
                    {
                        r.todos_synced = false;
                    }
                }
                GitOpResult::Stopped(out) => {
                    let conflict = out == superzej_svc::git::RebaseOutcome::Conflict;
                    model.status = if conflict {
                        "conflict — resolve, then m → continue".into()
                    } else {
                        "rebase paused (edit) — m → continue".into()
                    };
                    match &mut panel_ui.git.flow {
                        GitFlow::Rebase(r) => {
                            r.running = true;
                            r.conflict = conflict;
                            // The sequencer consumed entries; whatever the
                            // editor holds is no longer the live todo.
                            r.todos_synced = false;
                        }
                        flow => {
                            *flow = GitFlow::Rebase(gitui::RebaseUi {
                                running: true,
                                conflict,
                                ..Default::default()
                            });
                        }
                    }
                }
                GitOpResult::Culprit(sha) => {
                    model.status = format!("first bad commit: {}", short_sha(&sha));
                    if let GitFlow::Bisect(b) = &mut panel_ui.git.flow {
                        b.culprit = Some(sha);
                    }
                }
                GitOpResult::Plan { plan, redo } => {
                    rehydrate = false;
                    let verb = if redo { "redo" } else { "undo" };
                    match &plan {
                        superzej_core::reflog::UndoPlan::Nothing => {
                            model.status = format!("nothing to {verb}");
                        }
                        plan => {
                            let body = match plan {
                                superzej_core::reflog::UndoPlan::HardResetTo {
                                    undoing, ..
                                } => undoing.clone(),
                                superzej_core::reflog::UndoPlan::Checkout { branch, undoing } => {
                                    format!("{undoing} (back to {branch})")
                                }
                                superzej_core::reflog::UndoPlan::Nothing => unreachable!(),
                            };
                            pending_undo = Some((plan.clone(), redo));
                            active_menu = Some(menu::undo_confirm_menu(body, redo));
                        }
                    }
                }
                GitOpResult::Output(text) => {
                    if text.trim().is_empty() {
                        model.status = format!("{} ✓ (no output)", done.label);
                    } else {
                        active_menu = Some(menu::output_menu("output", &text));
                    }
                }
                GitOpResult::Err(msg) => {
                    model.status = msg;
                    // A refused rewrite (todo changed on disk) — or any
                    // other failure mid-pause — warrants a fresh read so
                    // the editor shows what's actually pending.
                    if done.label == "editing todo"
                        && let GitFlow::Rebase(r) = &mut panel_ui.git.flow
                    {
                        r.todos_synced = false;
                    }
                }
            }
            if rehydrate {
                let _ = refresh_tx.send(if done.touches_remote {
                    crate::hydrate::RefreshKind::Pr
                } else {
                    crate::hydrate::RefreshKind::Model
                });
            }
            // Safety net: a running-but-unsynced TODO editor always gets a
            // live read (deduped by the inflight flag, killed on worktree
            // switch by the generation tag).
            if matches!(&panel_ui.git.flow, GitFlow::Rebase(r) if r.running && !r.todos_synced)
                && !rebase_sync_inflight
            {
                rebase_sync_inflight = true;
                spawn_rebase_status_fetch(panel_ui.git.op_gen, &session, &gitdoc_tx, &waker);
            }
            dirty = true;
        }

        // Fetched git documents for the line-cursor views (generation-tagged
        // like the hunk previews).
        while let Ok((generation, doc)) = gitdoc_rx.try_recv() {
            if generation != panel_ui.git.op_gen {
                continue;
            }
            match doc {
                GitDoc::Stage(state) => {
                    if let Some(s) = panel_ui.git.staging.as_mut()
                        && s.path == state.path
                        && s.pane == state.pane
                    {
                        s.cursor = crate::panel::staging::clamp_cursor(&state.doc, s.cursor);
                        if s.anchor.is_some_and(|a| a >= state.doc.lines.len()) {
                            s.anchor = None;
                        }
                        panel_ui.git.stage_doc = Some(state);
                    }
                }
                GitDoc::CommitFiles(files) => {
                    panel_ui.git.commit_files = files;
                    let max = panel_ui.git.commit_files.len().saturating_sub(1);
                    let cur = panel_ui.git.cur.get(GitView::CommitFiles).min(max);
                    panel_ui.git.cur.set(GitView::CommitFiles, cur);
                }
                GitDoc::Patch(state) => {
                    panel_ui
                        .git
                        .patch_docs
                        .insert(state.path.clone(), state.clone());
                    panel_ui.git.patch_doc = Some(state);
                }
                GitDoc::Rebase(status) => {
                    rebase_sync_inflight = false;
                    if let GitFlow::Rebase(r) = &mut panel_ui.git.flow {
                        match status {
                            Some(st) if r.running => {
                                r.todos = st.remaining.clone();
                                r.baseline = st.remaining;
                                r.todos_synced = true;
                                r.done = st.done.len();
                                r.stopped_sha = st.stopped_sha;
                                r.conflict = st.paused == superzej_svc::git::PauseReason::Conflict;
                                r.cursor = r.cursor.min(r.todos.len().saturating_sub(1));
                            }
                            // The pause vanished before the read landed (a
                            // fast --continue elsewhere); hydration's banner
                            // sync clears the flow — just drop the read.
                            _ => {}
                        }
                    }
                }
            }
            dirty = true;
        }

        // Resolved launch specs: finish the deferred materialize (lazy focus
        // path and pre-warm alike). Results for a worktree/tab that vanished
        // mid-flight are dropped; `materialize_with_specs` itself skips
        // leaves that came alive some other way in the meantime.
        while let Ok((wt, ti, specs)) = spec_rx.try_recv() {
            if materialize_inflight.as_ref() == Some(&(wt.clone(), ti)) {
                materialize_inflight = None;
            }
            let Some(gi) = session.worktrees.iter().position(|g| g.path == wt) else {
                continue;
            };
            let is_active = gi == session.active && session.worktrees[gi].active_tab == ti;
            if is_active && center_dormant {
                continue; // splash still up: stay lazy
            }
            let specs = match specs {
                Ok(specs) => specs,
                Err(e) => {
                    model.status = format!("Pane launch blocked: {e}");
                    if is_active {
                        center_dormant = true;
                        dirty = true;
                    }
                    continue;
                }
            };
            let warnings = specs
                .iter()
                .filter_map(|(_, spec)| spec.warning_summary())
                .collect::<std::collections::BTreeSet<_>>()
                .into_iter()
                .collect::<Vec<_>>();
            let Some(tab) = session.worktrees[gi].tabs.get_mut(ti) else {
                continue;
            };
            if let Err(e) = panes.materialize_with_specs(tab, &wt, &specs, chrome.center) {
                // Spawn failures are survivable: report, don't exit the loop.
                model.status = format!("Pane spawn failed: {e}");
            } else if is_active && !warnings.is_empty() {
                model.status = format!("⚠ Sandbox fallback: {}", warnings.join("; "));
            }
            if is_active {
                need_relayout = true;
            }
            dirty = true;
        }

        while let Ok((generation, next_model)) = model_rx.try_recv() {
            if generation != hydration_gen {
                continue;
            }
            // Fresh git data invalidates the inline hunk previews (the 2s
            // safety tick sends identical panels, so only real changes
            // clear); the selected row's preview is refetched immediately so
            // an open preview never sticks on stale content.
            if next_model.panel != model.panel {
                panel_ui.hunks.clear();
                hunk_inflight.clear();
                if let Some(path) = panel_ui
                    .chg_sel
                    .and_then(|i| next_model.panel.changes.get(i))
                    .filter(|c| c.stage != crate::panel::Stage::Untracked)
                    .map(|c| c.path.clone())
                {
                    spawn_hunk_fetch(
                        &path,
                        &session,
                        &panel_ui,
                        &mut hunk_inflight,
                        &hunk_tx,
                        &waker,
                        hydration_gen,
                    );
                }
            }
            let stats = std::mem::take(&mut model.stats);
            let metrics = std::mem::take(&mut model.metrics);
            model = next_model;
            model.stats = stats;
            model.metrics = metrics;
            // Mirror an externally-started (or externally-finished) rebase
            // into the git flow state, so the TODO view and conflict chrome
            // track `git rebase` runs from any terminal.
            {
                let banner = model
                    .panel
                    .merge
                    .as_ref()
                    .map(|m| (m.label.as_str(), m.unresolved));
                if let Some(note) = gitui::sync_rebase_flow(&mut panel_ui.git, banner) {
                    model.status = note.to_string();
                }
                // An externally-started rebase arrives here with an empty,
                // unsynced editor: load the live pending todo for it.
                if matches!(&panel_ui.git.flow, GitFlow::Rebase(r) if r.running && !r.todos_synced)
                    && !rebase_sync_inflight
                {
                    rebase_sync_inflight = true;
                    spawn_rebase_status_fetch(panel_ui.git.op_gen, &session, &gitdoc_tx, &waker);
                }
            }
            refresh_tab_model(&mut model, &session, &mut sb);
            apply_mode_status(&mut model, mode);
            model.accent = current_config.accent_rgb();
            model.bars = current_config.bars.clone();
            model.stats_icons = current_config.stats.clone();
            let ws = (!session.id.is_empty()).then_some(session.id.as_str());
            model.pins = supervisor.chips(&current_config, ws);
            dirty = true;
        }

        // Fresh stats reading from the ticker thread (top-bar widgets + the
        // telemetry section's history). While that section is on screen,
        // every tick dirties the frame — its graphs advance even when the
        // headline numbers are unchanged.
        let telemetry_visible =
            chrome.panel.is_some() && panel_ui.open == crate::panel::Section::Telemetry;
        while let Ok(snap) = stats_rx.try_recv() {
            panel_ui.docs.telemetry.push(&snap);
            panel_ui.docs.tick = panel_ui.docs.tick.wrapping_add(1);
            if model.stats != snap || telemetry_visible {
                model.stats = snap;
                dirty = true;
            }
        }

        // Fresh metrics readings from the scrape supervisor (sidebar section).
        while let Ok(state) = metrics_rx.try_recv() {
            if model.metrics != state {
                model.metrics = state;
                dirty = true;
            }
        }

        // Panel document payloads from the on-entry fetches; stale
        // generations (pre-worktree-switch) are dropped.
        while let Ok((generation, payload)) = docs_rx.try_recv() {
            if generation != docs_gen {
                continue;
            }
            match payload {
                crate::panel::docs::DocsPayload::Git(d) => panel_ui.docs.git = Some(d),
                crate::panel::docs::DocsPayload::Diff(d) => panel_ui.docs.diff = Some(d),
            }
            dirty = true;
        }

        // Worktree-creation progress from the wizard worker; stale
        // generations (a cancelled run's stragglers) are dropped.
        while let Ok(ev) = create_rx.try_recv() {
            match ev {
                wizard::CreateEvent::Preflight {
                    generation,
                    suggested,
                } => {
                    if generation != create_gen {
                        continue;
                    }
                    if let Some(w) = wizard_ui.as_mut() {
                        w.apply_name_suggestion(&suggested);
                    }
                    if let Some(cp) = creating.as_mut() {
                        cp.branch = suggested;
                    }
                    dirty = true;
                }
                wizard::CreateEvent::Step {
                    generation,
                    step,
                    state,
                    detail,
                } => {
                    if generation != create_gen {
                        continue;
                    }
                    if let Some(cp) = creating.as_mut() {
                        cp.apply(step, state, detail);
                        if cp.revealed {
                            dirty = true;
                        }
                    }
                }
                wizard::CreateEvent::Tick { generation } => {
                    if generation != create_gen {
                        continue;
                    }
                    if let Some(cp) = creating.as_mut() {
                        cp.bump_tick();
                        if cp.revealed {
                            dirty = true;
                        }
                    }
                }
                wizard::CreateEvent::Failed {
                    generation,
                    step,
                    error,
                } => {
                    if generation != create_gen {
                        continue;
                    }
                    // The worker cleaned up and exited; surface the failure
                    // immediately (even mid-wizard — nothing left to submit).
                    wizard_ui = None;
                    wizard_cmd_tx = None;
                    if let Some(cp) = creating.as_mut() {
                        cp.revealed = true;
                        cp.stop_ticker();
                    }
                    model.status = format!("worktree creation failed ({}): {error}", step.label());
                    dirty = true;
                }
                wizard::CreateEvent::Done {
                    generation,
                    payload,
                } => {
                    if generation != create_gen {
                        continue;
                    }
                    let payload = *payload;
                    if let Some(cp) = creating.as_mut() {
                        cp.apply(
                            wizard::CreateStep::LaunchAgent,
                            wizard::StepState::Running,
                            Some(payload.agent.clone()),
                        );
                    }
                    session.add_group(crate::session::WorktreeGroup::new(
                        payload.tab.clone(),
                        crate::session::GroupKind::Branch,
                        payload.path.clone(),
                    ));
                    refresh_tab_model(&mut model, &session, &mut sb);
                    need_relayout = true;
                    // Pane spawn (openpty+exec) is the only loop-side step;
                    // on failure the tab's empty leaves fall through to the
                    // materialize path, which backs it with a plain shell.
                    if attach_agent_pane(
                        &mut session,
                        &mut panes,
                        &payload.tab,
                        &payload.spec,
                        chrome.center,
                    ) {
                        focus.zone = crate::focus::Zone::Center;
                        let backend = &payload.spec.backend;
                        model.status = match payload.spec.warning_summary() {
                            Some(warning) => format!(
                                "⚠ worktree {} ready ({backend}) — sandbox fallback: {warning}",
                                payload.branch
                            ),
                            None => format!("worktree {} ready ({backend})", payload.branch),
                        };
                    } else {
                        model.status =
                            format!("worktree {} created (agent launch failed)", payload.branch);
                    }
                    if let Some(cp) = creating.take() {
                        cp.stop_ticker();
                    }
                    wizard_cmd_tx = None;
                    // Fresh worktree + agent pane: re-hydrate so the sidebar
                    // and panel reflect it immediately.
                    hydration_gen = hydration_gen.wrapping_add(1);
                    spawn_model_hydration(
                        model_tx.clone(),
                        hydration_gen,
                        session.clone(),
                        Some(waker.clone()),
                        crate::hydrate::HydrateHints {
                            open: panel_ui.open,
                            expanded: panel_ui.width.is_expanded(),
                        },
                    );
                    dirty = true;
                }
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
                    crate::seg::set_undercurl_supported(resolve_undercurl(&new_cfg));
                    crate::center::PANE_HPAD.store(
                        new_cfg.theme.pane_padding as usize,
                        std::sync::atomic::Ordering::Relaxed,
                    );
                    model.accent = new_cfg.accent_rgb();
                    model.bars = new_cfg.bars.clone();
                    model.stats_icons = new_cfg.stats.clone();
                    // Live `[panel] sections` reload: reorder/hide accordions;
                    // the keys section's cheatsheet follows the new keymap.
                    panel_ui.set_order(crate::panel::resolve_order(&new_cfg));
                    panel_ui.docs.cfg_keys = crate::keyhint::cheatsheet_groups(&new_cfg);
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
                crate::hydrate::HydrateHints {
                    open: panel_ui.open,
                    expanded: panel_ui.width.is_expanded(),
                },
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
        model.masthead_focused = focus.masthead();
        model.statusbar_focused = focus.statusbar();
        model.key_locked = focus.locked;
        model.zoomed = zoom.is_some();
        model.keyhints = context_hints(&focus, &panel_ui, keymap.config());
        // The ticker samples at live (500ms) cadence while the telemetry
        // section is on screen, so its graphs actually move.
        stats_live.store(
            chrome.panel.is_some() && panel_ui.open == crate::panel::Section::Telemetry,
            std::sync::atomic::Ordering::Relaxed,
        );

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
            // Card titles: "{title} · {worktree-leaf}" — the OSC window title
            // the app sets, else the program name derived from the spawn argv.
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
                            // Prefer the OSC window title the app sets (zsh +
                            // starship, tmux, etc.); fall back to the program
                            // name derived from the spawn argv.
                            let name = p
                                .emulator()
                                .title()
                                .filter(|t| !t.trim().is_empty())
                                .unwrap_or_else(|| p.program().to_string());
                            if title_leaf.is_empty() {
                                name
                            } else {
                                format!("{name} \u{00b7} {title_leaf}")
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
            // Search overlay is composited above the center, below menus.
            if let Some(ref ov) = search {
                let rect = search_overlay_rect(chrome.center);
                ov.render(&mut scratch, rect);
            }
            if let Some(m) = &active_menu {
                m.render(&mut scratch, screen);
            }
            if let Some((inp, _)) = &git_input {
                inp.render(&mut scratch, screen);
            }
            if let Some((inp, _)) = &host_input {
                inp.render(&mut scratch, screen);
            }
            if let Some(w) = &wizard_ui {
                w.render(&mut scratch, screen);
            }
            if let Some(cp) = &creating
                && cp.revealed
            {
                cp.render(&mut scratch, screen);
            }
            let accent = current_config.accent_rgb();
            if !which_key.is_empty() {
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
                wire_renderer.invalidate();
                full_repaint = false;
            }
            let mut pending = front.diff_screens(&scratch);
            if palette.is_none()
                && wizard_ui.is_none()
                && !creating.as_ref().is_some_and(|cp| cp.revealed)
            {
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
            if use_termwiz_renderer {
                buf.terminal().render(&wire).context("render")?;
                buf.terminal().flush().context("terminal flush")?;
            } else {
                let mut bytes = String::new();
                wire_renderer.render(&wire, &mut bytes);
                use std::io::Write as _;
                let mut out = std::io::stdout();
                out.write_all(bytes.as_bytes()).context("render")?;
                out.flush().context("terminal flush")?;
            }
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
                        // Coalesce all same-direction wheel ticks that are
                        // already queued: one render pass for the whole
                        // gesture, never one render per tick.
                        let (ticks, leftover) = drain_wheel_ticks(up, || {
                            buf.terminal()
                                .poll_input(Some(std::time::Duration::ZERO))
                                .ok()
                                .flatten()
                        });
                        if let Some(ev) = leftover {
                            pending_input.push_back(ev);
                        }
                        // 5 rows per tick (was 3) — snappier single-tick response.
                        let delta = ticks * 5;
                        if let Some(p) = panes.table.get_mut(&id) {
                            if up {
                                p.scroll_up(delta);
                            } else {
                                p.scroll_down(delta);
                            }
                            dirty = true;
                        }
                    } else if chrome.panel.is_some_and(|r| contains(r, mx, my)) {
                        // Row mode walks the open section's rows; section mode
                        // wheels through the accordion itself.
                        if panel_ui.row_mode {
                            let (pc, pr) = panel_geom(&chrome);
                            let max =
                                crate::panel::frame::actionable_rows(&model, &panel_ui, pc, pr)
                                    .saturating_sub(1);
                            panel_ui.cursor = if up {
                                panel_ui.cursor.saturating_sub(1)
                            } else {
                                (panel_ui.cursor + 1).min(max)
                            };
                        } else {
                            let next = if up {
                                panel_ui.prev_section()
                            } else {
                                panel_ui.next_section()
                            };
                            open_panel_section(
                                next,
                                &mut panel_ui,
                                &mut hydration_gen,
                                &model_tx,
                                &session,
                                &waker,
                                PanelDocsWiring {
                                    model: &model,
                                    generation: docs_gen,
                                    tx: &docs_tx,
                                },
                            );
                        }
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
                        // Resolve the click against the same frame the
                        // renderer painted — placement can never drift.
                        let hit = crate::chrome::panel_hits(&model, &panel_ui, r)
                            .into_iter()
                            .find(|(y, _)| *y == my)
                            .map(|(_, h)| h);
                        match hit {
                            Some(crate::panel::PanelHit::OpenSection(s)) => {
                                open_panel_section(
                                    s,
                                    &mut panel_ui,
                                    &mut hydration_gen,
                                    &model_tx,
                                    &session,
                                    &waker,
                                    PanelDocsWiring {
                                        model: &model,
                                        generation: docs_gen,
                                        tx: &docs_tx,
                                    },
                                );
                                if s == crate::panel::Section::Tests {
                                    maybe_discover_tests(
                                        &mut panel_ui,
                                        &session,
                                        &mut test_generation,
                                        test_discovery_tx.clone(),
                                        waker.clone(),
                                        keymap.config(),
                                        test_sem.clone(),
                                    );
                                }
                            }
                            Some(crate::panel::PanelHit::Expand) => {
                                toggle_panel_expand(
                                    &mut panel_ui,
                                    &mut hydration_gen,
                                    &model_tx,
                                    &session,
                                    &waker,
                                    PanelDocsWiring {
                                        model: &model,
                                        generation: docs_gen,
                                        tx: &docs_tx,
                                    },
                                );
                                need_relayout = true;
                            }
                            Some(crate::panel::PanelHit::Row(sec, i)) => {
                                panel_ui.row_mode = true;
                                panel_ui.cursor = i;
                                if sec == crate::panel::Section::Changes {
                                    toggle_change_selection(
                                        i,
                                        &mut panel_ui,
                                        &model,
                                        &session,
                                        &mut hunk_inflight,
                                        &hunk_tx,
                                        &waker,
                                        hydration_gen,
                                    );
                                }
                            }
                            None => {
                                // The Full view packs the section list onto a
                                // single horizontal rail, so a row-granular hit
                                // misses it — fall back to the x+y rail test.
                                if panel_ui.width == crate::layout::PanelWidth::Full
                                    && let Some(s) =
                                        crate::chrome::panel_rail_hit(&model, &panel_ui, r, mx, my)
                                {
                                    open_panel_section(
                                        s,
                                        &mut panel_ui,
                                        &mut hydration_gen,
                                        &model_tx,
                                        &session,
                                        &waker,
                                        PanelDocsWiring {
                                            model: &model,
                                            generation: docs_gen,
                                            tx: &docs_tx,
                                        },
                                    );
                                }
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
                // Modal: a pending destructive delete swallows the next key.
                if let Some((_, targets)) = pending_delete.take() {
                    if matches!(k.key, KeyCode::Char('y') | KeyCode::Char('Y')) {
                        model.status = delete_groups(&mut session, &mut panes, targets);
                        sb.marked.clear();
                        refresh_tab_model(&mut model, &session, &mut sb);
                        sb.focus_active_row(&mut model);
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
                // Modal: the revealed worktree-creation progress overlay
                // swallows every key. Esc hides it while work continues (a
                // mid-flight checkout isn't safely interruptible and the
                // result is useful); on a failed run Esc/Enter dismisses.
                if creating.as_ref().is_some_and(|cp| cp.revealed) {
                    let failed = creating.as_ref().is_some_and(|cp| cp.failed);
                    let dismiss = crate::input::is_escape_key(&k.key)
                        || (failed && k.key == KeyCode::Enter)
                        || (matches!(k.key, KeyCode::Char('c' | 'C' | 'g' | 'G'))
                            && k.modifiers.contains(Modifiers::CTRL));
                    if dismiss {
                        if failed {
                            creating = None;
                            wizard_cmd_tx = None;
                            create_gen += 1;
                        } else if let Some(cp) = creating.as_mut() {
                            cp.revealed = false;
                            cp.stop_ticker();
                            model.status = format!("creating {} in the background…", cp.branch);
                        }
                    }
                    dirty = true;
                    continue;
                }
                // Modal: the new-worktree wizard captures all keys; its
                // decisions stream to the creation worker as they happen.
                if let Some(w) = wizard_ui.as_mut() {
                    match w.handle_key(&k.key, k.modifiers) {
                        wizard::WizardOutcome::Pending => {}
                        wizard::WizardOutcome::Cancel => {
                            if let Some(tx) = wizard_cmd_tx.take() {
                                let _ = tx.send(wizard::WizardCmd::Cancel);
                            }
                            wizard_ui = None;
                            creating = None;
                            create_gen += 1;
                            model.status = "worktree creation cancelled".into();
                        }
                        wizard::WizardOutcome::SandboxChosen(backend) => {
                            if let Some(tx) = wizard_cmd_tx.as_ref() {
                                let _ = tx.send(wizard::WizardCmd::SandboxChosen(backend));
                            }
                        }
                        wizard::WizardOutcome::Submit(choices) => {
                            if let wizard::NameChoice::Human(tail) = &choices.name
                                && let Some(cp) = creating.as_mut()
                            {
                                cp.branch = format!("{}{}", keymap.config().branch_prefix, tail);
                            }
                            if let Some(tx) = wizard_cmd_tx.as_ref() {
                                let _ = tx.send(wizard::WizardCmd::Submit(choices));
                            }
                            wizard_ui = None;
                            if let Some(cp) = creating.as_mut() {
                                cp.revealed = true;
                                wizard::spawn_ticker(
                                    cp.generation,
                                    cp.ticker_alive.clone(),
                                    create_tx.clone(),
                                    {
                                        let wk = waker.clone();
                                        move || {
                                            let _ = wk.wake();
                                        }
                                    },
                                );
                            }
                        }
                    }
                    dirty = true;
                    continue;
                }
                // Modal: host text input overlays (workspace creation, etc.)
                // capture all keys before palettes/panels so the shortcut gives
                // immediate visible feedback and a focused input target.
                if host_input.is_some() {
                    let outcome = host_input
                        .as_mut()
                        .map(|(inp, _)| inp.handle_key(&k.key, k.modifiers))
                        .unwrap_or(menu::InputOutcome::Pending);
                    match outcome {
                        menu::InputOutcome::Pending => {}
                        menu::InputOutcome::Cancel => {
                            host_input = None;
                            model.status = "workspace creation cancelled".into();
                        }
                        menu::InputOutcome::Submit(text) => {
                            if let Some((_, kind)) = host_input.take() {
                                match kind {
                                    HostInputKind::NewWorkspace => {
                                        let outgoing = session_pane_ids(&session);
                                        match superzej_core::db::Db::open()
                                            .context("open superzej db")
                                            .and_then(|db| {
                                                create_workspace_from_input_with_config(
                                                    &text,
                                                    &mut session,
                                                    &db,
                                                    &current_config,
                                                )
                                            }) {
                                            Ok(path) => {
                                                for id in outgoing {
                                                    panes.table.remove(&id);
                                                }
                                                if let Some(id) = drawer.take() {
                                                    panes.table.remove(&id);
                                                }
                                                for id in drawer_pool.drain_ids() {
                                                    panes.table.remove(&id);
                                                }
                                                focus.zone = crate::focus::Zone::Center;
                                                refresh_tab_model(&mut model, &session, &mut sb);
                                                sync_drawer_persistence(
                                                    &session,
                                                    &mut panes,
                                                    &mut drawer,
                                                    &mut drawer_pool,
                                                    &mut drawer_home,
                                                    keymap.config(),
                                                    chrome.center,
                                                );
                                                model.status = format!(
                                                    "workspace created: {}",
                                                    path.display()
                                                );
                                                need_relayout = true;
                                            }
                                            Err(e) => {
                                                model.status =
                                                    format!("workspace create failed: {e}");
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }
                    dirty = true;
                    continue;
                }
                // Modal: an open git option/confirm menu captures all keys.
                if let Some(m) = active_menu.as_mut() {
                    match m.handle_key(&k.key, k.modifiers) {
                        menu::MenuOutcome::Pending => {}
                        menu::MenuOutcome::Cancel => {
                            active_menu = None;
                            pending_confirm_op = None;
                            pending_undo = None;
                        }
                        menu::MenuOutcome::Pick(choice) => {
                            active_menu = None;
                            let wires = GitWires {
                                session: &session,
                                cfg: keymap.config(),
                                op_tx: &gitop_tx,
                                doc_tx: &gitdoc_tx,
                                waker: &waker,
                            };
                            let mut ov = GitOverlays {
                                menu: &mut active_menu,
                                input: &mut git_input,
                                confirm_op: &mut pending_confirm_op,
                                custom_cmds: &mut custom_menu_cmds,
                            };
                            match dispatch_menu_choice(
                                choice,
                                &mut panel_ui,
                                &mut model,
                                &wires,
                                &mut ov,
                                &mut pending_undo,
                            ) {
                                GitAfter::None => {}
                                GitAfter::NewWorktree => {
                                    forced_palette_action =
                                        Some(crate::keymap::Action::NewWorktree);
                                }
                                GitAfter::Terminal(cmd) => {
                                    let cwd = active_cwd(&session);
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
                        }
                    }
                    dirty = true;
                    if forced_palette_action.is_none() {
                        continue;
                    }
                }
                // Modal: a git text-input overlay (commit message, reword,
                // prompts, …) captures all keys.
                if git_input.is_some() {
                    let outcome = git_input
                        .as_mut()
                        .map(|(inp, _)| inp.handle_key(&k.key, k.modifiers))
                        .unwrap_or(menu::InputOutcome::Pending);
                    match outcome {
                        menu::InputOutcome::Pending => {}
                        menu::InputOutcome::Cancel => {
                            git_input = None;
                            model.status = "cancelled".into();
                        }
                        menu::InputOutcome::Submit(text) => {
                            if let Some((_, kind)) = git_input.take() {
                                let wires = GitWires {
                                    session: &session,
                                    cfg: keymap.config(),
                                    op_tx: &gitop_tx,
                                    doc_tx: &gitdoc_tx,
                                    waker: &waker,
                                };
                                let mut ov = GitOverlays {
                                    menu: &mut active_menu,
                                    input: &mut git_input,
                                    confirm_op: &mut pending_confirm_op,
                                    custom_cmds: &mut custom_menu_cmds,
                                };
                                match handle_git_input_submit(
                                    kind,
                                    text,
                                    &mut panel_ui,
                                    &mut model,
                                    &wires,
                                    &mut ov,
                                ) {
                                    GitAfter::None => {}
                                    GitAfter::NewWorktree => {
                                        forced_palette_action =
                                            Some(crate::keymap::Action::NewWorktree);
                                    }
                                    GitAfter::Terminal(cmd) => {
                                        let cwd = active_cwd(&session);
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
                            }
                        }
                    }
                    dirty = true;
                    if forced_palette_action.is_none() {
                        continue;
                    }
                }
                // The git filter line eats printable keys while editing.
                if focus.panel() && panel_ui.git.filter.as_ref().is_some_and(|f| f.editing) {
                    let mut consumed = true;
                    match k.key {
                        KeyCode::Enter => {
                            if let Some(f) = panel_ui.git.filter.as_mut() {
                                f.editing = false;
                            }
                        }
                        key if crate::input::is_escape_key(&key) => {
                            panel_ui.git.filter = None;
                        }
                        KeyCode::Backspace => {
                            if let Some(f) = panel_ui.git.filter.as_mut() {
                                f.query.pop();
                            }
                        }
                        KeyCode::Char(c)
                            if !k.modifiers.contains(Modifiers::CTRL)
                                && !k.modifiers.contains(Modifiers::ALT) =>
                        {
                            if let Some(f) = panel_ui.git.filter.as_mut() {
                                f.query.push(c);
                            }
                        }
                        _ => consumed = false,
                    }
                    if consumed {
                        // Recompute display space; stale cursors/anchors are
                        // the classic filtered-list bug.
                        let view = panel_ui.git.focus;
                        let map = gitui::display_map(&panel_ui.git, view, &model.panel);
                        let cur = panel_ui.git.cur.get(view).min(map.len().saturating_sub(1));
                        panel_ui.git.cur.set(view, cur);
                        panel_ui.git.sel_anchor = None;
                        if let Some(f) = panel_ui.git.filter.as_mut() {
                            f.map = map;
                        }
                        dirty = true;
                        continue;
                    }
                }
                // Modal: when the search overlay is open it captures all keys.
                if let Some(ref mut ov) = search {
                    let srcs = build_search_sources(ov.scope(), &session, &panes, focused);
                    // Convert to the borrow shape the engine expects.
                    let srcs_ref: Vec<(u32, &str, &superzej_core::history::HistoryBuffer)> = srcs
                        .iter()
                        .map(|(id, label, buf)| (*id, label.as_str(), *buf))
                        .collect();
                    match ov.handle_key(&k.key, k.modifiers, &srcs_ref) {
                        crate::search::SearchOutcome::Pending => {
                            dirty = true;
                            continue;
                        }
                        crate::search::SearchOutcome::Dismiss => {
                            search = None;
                            dirty = true;
                            continue;
                        }
                        crate::search::SearchOutcome::Jump(m) => {
                            search = None;
                            apply_search_jump(
                                m,
                                &mut session,
                                &mut panes,
                                &mut focus,
                                &mut model,
                                &mut sb,
                                &mut need_relayout,
                            );
                            dirty = true;
                            continue;
                        }
                    }
                }

                // Modal: when the palette is open it captures all keys.
                if let Some(p) = palette.as_mut() {
                    // Agent-picker mode: the palette is choosing what to run in a
                    // just-created worktree tab. The tab already materialized a
                    // shell, so "shell" (and Escape) keep the live pane —
                    // respawning it would needlessly reload the terminal. Only a
                    // real agent choice replaces the pane.
                    if palette_cancel_key(p, &k.key, k.modifiers) {
                        palette = None;
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
                                if let Some(family) = key.strip_prefix("font:") {
                                    match crate::font::apply_font_family(family) {
                                        Ok(path) => {
                                            model.status =
                                                format!("Font → {family} ({})", path.display());
                                        }
                                        Err(e) => model.status = format!("Font switch failed: {e}"),
                                    }
                                    palette = None;
                                    refresh_tab_model(&mut model, &session, &mut sb);
                                    need_relayout = true;
                                    dirty = true;
                                    continue;
                                } else if key == "quit" {
                                    return Ok(());
                                } else if let Some(payload) = key.strip_prefix("wt:") {
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
                                        panel_width,
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
                                        panel_width,
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
                if drawer.is_some() && drawer_cancel_key(&k.key, k.modifiers) {
                    if let Some(cwd) = active_cwd(&session) {
                        hide_drawer_into_pool(
                            &mut drawer,
                            &mut drawer_pool,
                            &mut drawer_home,
                            &cwd,
                            keymap.config(),
                            &mut panes,
                        );
                        let key = superzej_core::util::slugify(&cwd.to_string_lossy());
                        let dir = superzej_core::util::superzej_dir().join("drawer");
                        let _ = std::fs::create_dir_all(&dir);
                        let _ = std::fs::write(dir.join(key), "false");
                    } else if let Some(id) = drawer.take() {
                        panes.table.remove(&id);
                    }
                    dirty = true;
                    continue;
                }
                // Bar zones (masthead/statusbar): no widget interaction is
                // wired yet — Esc returns to the center; everything else is
                // swallowed (bars never forward to a pane).
                if focus.bar()
                    && !k.modifiers.contains(Modifiers::CTRL)
                    && !k.modifiers.contains(Modifiers::ALT)
                {
                    if crate::input::is_escape_key(&k.key) {
                        focus.zone = crate::focus::Zone::Center;
                    }
                    dirty = true;
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
                                panel_width,
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
                            let (mut targets, skipped_home) =
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
                                // Capture the active group name to restore focus after indices shift
                                let active_group_name =
                                    session.active_group().map(|g| g.name.clone());

                                // Sort targets descending so deletion doesn't shift upcoming indices
                                targets.sort_unstable_by(|a, b| b.cmp(a));

                                model.status = delete_groups(&mut session, &mut panes, targets);

                                // Restore focus based on stable name
                                if let Some(target_name) = active_group_name
                                    && let Some(new_idx) =
                                        session.worktrees.iter().position(|g| g.name == target_name)
                                {
                                    session.switch_to(new_idx);
                                }

                                sb.marked.clear();
                                refresh_tab_model(&mut model, &session, &mut sb);
                                sb.focus_active_row(&mut model);
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

                            // Capture the *name* of the active group before deletion,
                            // because its index will shift as groups below it are removed.
                            let active_group_name = session.active_group().map(|g| g.name.clone());

                            // Close from the highest index down so earlier
                            // indices stay valid as groups are removed.
                            let db = superzej_core::db::Db::open().ok();
                            targets.sort_unstable_by(|a, b| b.cmp(a));
                            for gi in targets {
                                if gi < session.worktrees.len() {
                                    if let Some(db) = &db {
                                        forget_worktree_group(
                                            db,
                                            &session.id,
                                            &session.worktrees[gi],
                                        );
                                    }
                                    for tab in &session.worktrees[gi].tabs {
                                        for id in tab.center.pane_ids() {
                                            panes.table.remove(&id);
                                        }
                                    }
                                    session.switch_to(gi);
                                    session.close_active_group();
                                }
                            }

                            // Restore focus to the group that was active before bulk-delete
                            // (if it survived). If it was deleted, `close_active_group()`
                            // above already applied the fallback clamping.
                            if let Some(target_name) = active_group_name
                                && let Some(new_idx) =
                                    session.worktrees.iter().position(|g| g.name == target_name)
                            {
                                session.switch_to(new_idx);
                            }

                            if skipped_home > 0 {
                                model.status = "Root workspace cannot be closed".into();
                            }
                            persist_session_layout(&session);
                            sb.marked.clear();
                            refresh_tab_model(&mut model, &session, &mut sb);
                            sb.focus_active_row(&mut model);
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
                // Panel zone, git-family contexts: the table-driven git keys
                // resolve BEFORE the accordion (space = stage-line in the
                // staging view, stage-file in the files list, …). Ctrl is
                // carved out only for the chords git explicitly claims
                // (Ctrl+j/k move commits, Ctrl+p patch menu).
                if forced_palette_action.is_none()
                    && focus.panel()
                    && !k.modifiers.contains(Modifiers::ALT)
                    && (!k.modifiers.contains(Modifiers::CTRL)
                        || gitui::git_claims_ctrl(&panel_ui, &k.key))
                    && let Some(msg) = gitui::git_key(&k.key, k.modifiers, &panel_ui)
                {
                    let wires = GitWires {
                        session: &session,
                        cfg: keymap.config(),
                        op_tx: &gitop_tx,
                        doc_tx: &gitdoc_tx,
                        waker: &waker,
                    };
                    let mut ov = GitOverlays {
                        menu: &mut active_menu,
                        input: &mut git_input,
                        confirm_op: &mut pending_confirm_op,
                        custom_cmds: &mut custom_menu_cmds,
                    };
                    match handle_git_msg(msg, &mut panel_ui, &mut model, &wires, &mut ov) {
                        GitAfter::None => {}
                        GitAfter::NewWorktree => {
                            forced_palette_action = Some(crate::keymap::Action::NewWorktree);
                        }
                        GitAfter::Terminal(cmd) => {
                            let cwd = active_cwd(&session);
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
                    dirty = true;
                    if forced_palette_action.is_none() {
                        continue;
                    }
                }
                // Panel zone: unmodified keys drive the accordion — the pure
                // key→intent map first, per-section ACTION keys resolved on
                // top of it (the loop's job, per the panel module contract).
                if forced_palette_action.is_none()
                    && focus.panel()
                    && !k.modifiers.contains(Modifiers::CTRL)
                    && !k.modifiers.contains(Modifiers::ALT)
                {
                    use crate::panel::{PanelMsg, Section, accordion_key};
                    if let Some(msg) = accordion_key(&k.key, k.modifiers, &panel_ui) {
                        // Open/next/prev share one reset+persist+rehydrate path.
                        let open_target = match msg {
                            PanelMsg::Open(s) => Some(s),
                            PanelMsg::NextSection => Some(panel_ui.next_section()),
                            PanelMsg::PrevSection => Some(panel_ui.prev_section()),
                            _ => None,
                        };
                        if let Some(s) = open_target {
                            open_panel_section(
                                s,
                                &mut panel_ui,
                                &mut hydration_gen,
                                &model_tx,
                                &session,
                                &waker,
                                PanelDocsWiring {
                                    model: &model,
                                    generation: docs_gen,
                                    tx: &docs_tx,
                                },
                            );
                            // Opening tests kicks the lazy (capped) discovery,
                            // exactly as entering the old Tests tab did.
                            if s == Section::Tests {
                                maybe_discover_tests(
                                    &mut panel_ui,
                                    &session,
                                    &mut test_generation,
                                    test_discovery_tx.clone(),
                                    waker.clone(),
                                    keymap.config(),
                                    test_sem.clone(),
                                );
                            }
                            dirty = true;
                            continue;
                        }
                        match msg {
                            // Handled above; kept to keep the match exhaustive.
                            PanelMsg::Open(_) | PanelMsg::NextSection | PanelMsg::PrevSection => {}
                            PanelMsg::LeaveRows => {
                                // Esc peels one layer: an expanded change
                                // preview first, then the whole panel zone
                                // (focus drops back to the center terminal).
                                if panel_ui.chg_sel.is_some() {
                                    panel_ui.chg_sel = None;
                                } else {
                                    focus.zone = crate::focus::Zone::Center;
                                }
                            }
                            PanelMsg::CursorDown | PanelMsg::CursorUp => {
                                // One clamped step per QUEUED key: held-key
                                // repeats are drained and applied in a single
                                // render pass (no backlog inertia).
                                let up = msg == PanelMsg::CursorUp;
                                let (repeat, leftover) = drain_key_repeats(&k, || {
                                    buf.terminal()
                                        .poll_input(Some(std::time::Duration::ZERO))
                                        .ok()
                                        .flatten()
                                });
                                if let Some(ev) = leftover {
                                    pending_input.push_back(ev);
                                }
                                // Full-view scroll documents (the side-by-side
                                // diff, the git log, the cheatsheet): j/k move
                                // the viewport, not a row cursor.
                                if panel_ui.width == layout::PanelWidth::Full
                                    && matches!(
                                        panel_ui.open,
                                        Section::Changes | Section::Git | Section::Keys
                                    )
                                {
                                    let max = match panel_ui.open {
                                        Section::Changes => panel_ui
                                            .docs
                                            .diff
                                            .as_ref()
                                            .map(|d| {
                                                crate::panel::docs::diff_flat_len(&d.file)
                                                    .saturating_sub(1)
                                            })
                                            .unwrap_or(0),
                                        Section::Git => panel_ui
                                            .docs
                                            .git
                                            .as_ref()
                                            .map(|d| d.log.len().saturating_sub(1))
                                            .unwrap_or(0),
                                        // Two balanced columns → height ≈ half
                                        // the total group lines (clamped again
                                        // at render).
                                        _ => {
                                            let lines: usize = panel_ui
                                                .docs
                                                .cfg_keys
                                                .iter()
                                                .map(|g| g.rows.len() + 2)
                                                .sum::<usize>()
                                                + 8;
                                            lines.div_ceil(2).saturating_sub(1)
                                        }
                                    };
                                    panel_ui.scroll = if up {
                                        panel_ui.scroll.saturating_sub(repeat)
                                    } else {
                                        (panel_ui.scroll + repeat).min(max)
                                    };
                                    if panel_ui.open == Section::Changes
                                        && let Some(doc) = &panel_ui.docs.diff
                                    {
                                        let starts =
                                            crate::panel::docs::diff_hunk_starts(&doc.file);
                                        panel_ui.diff_hunk = crate::panel::docs::diff_hunk_at(
                                            &starts,
                                            panel_ui.scroll,
                                        );
                                    }
                                    dirty = true;
                                    continue;
                                }
                                let geom = panel_geom(&chrome);
                                let count = crate::panel::frame::actionable_rows(
                                    &model, &panel_ui, geom.0, geom.1,
                                );
                                let max = count.saturating_sub(1);
                                if up {
                                    if panel_ui.cursor > 0 {
                                        panel_ui.cursor = panel_ui.cursor.saturating_sub(repeat);
                                    } else if let Some(s) =
                                        prev_section_in_order(panel_ui.open, &panel_ui)
                                    {
                                        // Top of the list: flow into the previous
                                        // accordion, landing on its last row (or its
                                        // header when it has no actionable rows).
                                        let last = crate::panel::frame::section_rows(
                                            s, &model, &panel_ui, geom.0, geom.1,
                                        )
                                        .saturating_sub(1);
                                        open_panel_section(
                                            s,
                                            &mut panel_ui,
                                            &mut hydration_gen,
                                            &model_tx,
                                            &session,
                                            &waker,
                                            PanelDocsWiring {
                                                model: &model,
                                                generation: docs_gen,
                                                tx: &docs_tx,
                                            },
                                        );
                                        panel_ui.cursor = last;
                                    }
                                } else if count > 0 && panel_ui.cursor < max {
                                    panel_ui.cursor = (panel_ui.cursor + repeat).min(max);
                                } else if let Some(s) =
                                    next_section_in_order(panel_ui.open, &panel_ui)
                                {
                                    // Bottom of the list (or an accordion with no
                                    // actionable rows): flow into the next accordion
                                    // at its first row (its header when empty).
                                    open_panel_section(
                                        s,
                                        &mut panel_ui,
                                        &mut hydration_gen,
                                        &model_tx,
                                        &session,
                                        &waker,
                                        PanelDocsWiring {
                                            model: &model,
                                            generation: docs_gen,
                                            tx: &docs_tx,
                                        },
                                    );
                                    panel_ui.cursor = 0;
                                }
                            }
                            PanelMsg::Select => match panel_ui.open {
                                Section::Changes => {
                                    toggle_change_selection(
                                        panel_ui.cursor,
                                        &mut panel_ui,
                                        &model,
                                        &session,
                                        &mut hunk_inflight,
                                        &hunk_tx,
                                        &waker,
                                        hydration_gen,
                                    );
                                }
                                Section::Git => {
                                    // The cursor walks the DISPLAYED review
                                    // threads — same filter/cap as the git
                                    // section's content builder.
                                    let deep = panel_ui.width.is_expanded();
                                    let visible = if deep { 4 } else { 2 };
                                    let target = model
                                        .panel
                                        .threads
                                        .iter()
                                        .filter(|t| !t.resolved || deep)
                                        .take(visible)
                                        .nth(panel_ui.cursor)
                                        .map(|t| (t.path.clone(), t.line.map(|l| l as usize)));
                                    if let Some((path, line)) = target {
                                        let cmd = editor_open_command(keymap.config(), &path, line);
                                        let cwd = active_cwd(&session);
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
                                Section::Tests => {
                                    let target = model
                                        .panel
                                        .tests
                                        .as_ref()
                                        .and_then(|t| t.failures.get(panel_ui.cursor))
                                        .and_then(|(_, at)| parse_file_line(at));
                                    if let Some((path, line)) = target {
                                        let cmd =
                                            editor_open_command(keymap.config(), &path, Some(line));
                                        let cwd = active_cwd(&session);
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
                                    } else {
                                        model.status = "No source location for this failure".into();
                                    }
                                }
                                Section::Files => {
                                    if let Some(path) = changed_file_at(&model, panel_ui.cursor) {
                                        let cmd = editor_open_command(keymap.config(), &path, None);
                                        let cwd = active_cwd(&session);
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
                                // Drill handling for the git-family lists
                                // arrives with the GitMsg dispatch layer.
                                Section::Commits | Section::Branches | Section::Stash => {}
                                Section::Debug
                                | Section::Sandbox
                                | Section::Db
                                | Section::Telemetry
                                | Section::Keys => {}
                            },
                            PanelMsg::ToggleExpand => {
                                toggle_panel_expand(
                                    &mut panel_ui,
                                    &mut hydration_gen,
                                    &model_tx,
                                    &session,
                                    &waker,
                                    PanelDocsWiring {
                                        model: &model,
                                        generation: docs_gen,
                                        tx: &docs_tx,
                                    },
                                );
                                need_relayout = true;
                            }
                            PanelMsg::StageToggle => {
                                // The highlighted change row (the cursor row
                                // when nothing is highlighted); git runs off
                                // the loop and a model refresh follows.
                                let row = panel_ui
                                    .chg_sel
                                    .or(Some(panel_ui.cursor))
                                    .and_then(|i| model.panel.changes.get(i))
                                    .map(|c| (c.path.clone(), c.stage));
                                if let Some((path, stage)) = row {
                                    let unstage = stage == crate::panel::Stage::Staged;
                                    model.status = format!(
                                        "{} {path}",
                                        if unstage { "Unstaging" } else { "Staging" }
                                    );
                                    let wt = active_tab_path(&session);
                                    let tx = refresh_tx.clone();
                                    let wk = waker.clone();
                                    tokio::task::spawn_blocking(move || {
                                        use superzej_svc::git::GitBackend;
                                        let loc = superzej_core::remote::GitLoc::for_worktree(&wt);
                                        let git = superzej_svc::git::GixGit::new();
                                        let _ = if unstage {
                                            git.unstage(&loc, &path)
                                        } else {
                                            git.stage(&loc, &path)
                                        };
                                        if tx.send(RefreshKind::Model).is_ok() {
                                            let _ = wk.wake();
                                        }
                                    });
                                }
                            }
                        }
                        dirty = true;
                        continue;
                    }
                    // Per-section ACTION keys: plain chars the accordion map
                    // left unclaimed, scoped to the open section.
                    let handled = match (panel_ui.open, k.key) {
                        // -- changes (full view): hunk snap + file hop -------
                        (Section::Changes, KeyCode::Char(c @ (']' | '[')))
                            if panel_ui.width == layout::PanelWidth::Full =>
                        {
                            if let Some(doc) = &panel_ui.docs.diff {
                                let starts = crate::panel::docs::diff_hunk_starts(&doc.file);
                                if !starts.is_empty() {
                                    panel_ui.diff_hunk = if c == ']' {
                                        (panel_ui.diff_hunk + 1).min(starts.len() - 1)
                                    } else {
                                        panel_ui.diff_hunk.saturating_sub(1)
                                    };
                                    panel_ui.scroll = starts[panel_ui.diff_hunk];
                                }
                            }
                            true
                        }
                        (Section::Changes, KeyCode::Char(c @ ('n' | 'p')))
                            if panel_ui.width == layout::PanelWidth::Full
                                && !model.panel.changes.is_empty() =>
                        {
                            // Hop the diff target to the next/previous change
                            // and refetch (the body shows "loading…").
                            let len = model.panel.changes.len();
                            let cur = panel_ui.chg_sel.unwrap_or(0);
                            panel_ui.chg_sel = Some(if c == 'n' {
                                (cur + 1) % len
                            } else {
                                (cur + len - 1) % len
                            });
                            panel_ui.scroll = 0;
                            panel_ui.diff_hunk = 0;
                            kick_diff_doc_fetch(
                                docs_gen,
                                &session,
                                &mut panel_ui,
                                &model,
                                &docs_tx,
                                &waker,
                            );
                            true
                        }
                        // -- git: copy the HEAD sha (wide views) -------------
                        (Section::Git, KeyCode::Char('y')) if panel_ui.width.is_expanded() => {
                            match panel_ui
                                .docs
                                .git
                                .as_ref()
                                .and_then(crate::panel::docs::copy_target_sha)
                            {
                                Some(sha) => {
                                    use std::io::Write;
                                    let mut out = std::io::stdout();
                                    let _ = out.write_all(&crate::copymode::osc52(&sha));
                                    let _ = out.flush();
                                    model.status = format!("Copied {sha}");
                                }
                                None => model.status = "No commit data yet".into(),
                            }
                            true
                        }
                        // -- git: PR actions via `gh`, off the loop ----------
                        (Section::Git, KeyCode::Char('M')) => {
                            match &model.panel.pr {
                                Some(pr) => {
                                    model.status = format!("Merging PR #{} (squash)…", pr.number);
                                    spawn_pr_action(
                                        &session,
                                        &refresh_tx,
                                        &waker,
                                        "pr merge",
                                        |loc| {
                                            superzej_core::github::merge_pr(
                                                loc,
                                                superzej_core::github::MergeMethod::Squash,
                                                false,
                                                false,
                                            )
                                        },
                                    );
                                }
                                None => model.status = "No pull request to merge".into(),
                            }
                            true
                        }
                        (Section::Git, KeyCode::Char('A')) => {
                            match &model.panel.pr {
                                Some(pr) => {
                                    model.status = format!("Approving PR #{}…", pr.number);
                                    spawn_pr_action(
                                        &session,
                                        &refresh_tx,
                                        &waker,
                                        "pr approve",
                                        |loc| superzej_core::github::approve_pr(loc, None),
                                    );
                                }
                                None => model.status = "No pull request to approve".into(),
                            }
                            true
                        }
                        (Section::Git, KeyCode::Char('c')) => {
                            if model.panel.pr.is_some() {
                                model.status = "A pull request already exists".into();
                            } else {
                                model.status = "Creating PR from branch commits…".into();
                                spawn_pr_action(
                                    &session,
                                    &refresh_tx,
                                    &waker,
                                    "pr create",
                                    |loc| {
                                        superzej_core::github::create_pr(
                                            loc,
                                            &superzej_core::github::CreateOpts {
                                                title: None,
                                                body: None,
                                                base: None,
                                                draft: false,
                                                web: false,
                                                fill: true,
                                            },
                                        )
                                        .map(|_| ())
                                    },
                                );
                            }
                            true
                        }
                        (Section::Git, KeyCode::Char('r')) => {
                            model.status = "Re-running failed checks…".into();
                            spawn_pr_action(
                                &session,
                                &refresh_tx,
                                &waker,
                                "pr rerun-checks",
                                |loc| superzej_core::github::rerun_failed_checks(loc).map(|_| ()),
                            );
                            true
                        }
                        (Section::Git, KeyCode::Char('o')) => {
                            // The PR in the browser, detached (no gh needed).
                            if let Some(pr) = &model.panel.pr {
                                let _ = std::process::Command::new("xdg-open")
                                    .arg(&pr.url)
                                    .stdin(std::process::Stdio::null())
                                    .stdout(std::process::Stdio::null())
                                    .stderr(std::process::Stdio::null())
                                    .spawn();
                                model.status = format!("Opened PR #{} in the browser", pr.number);
                            } else {
                                model.status = "No pull request to open".into();
                            }
                            true
                        }
                        // -- tests: run / discover (capped, single-flight) ---
                        (Section::Tests, KeyCode::Char(c @ ('r' | 'R' | 'f'))) => {
                            let kind = match c {
                                'r' => TestRun::Selected,
                                'R' => TestRun::All,
                                _ => TestRun::Failed,
                            };
                            let wt = active_tab_path(&session);
                            sync_tests_for_worktree(&mut panel_ui, &wt, keymap.config());
                            if let Some(base) = panel_ui.tests.task.clone() {
                                if !panel_ui.tests.running {
                                    let task_spec = test_task_for_run(&panel_ui, kind, base);
                                    test_generation += 1;
                                    panel_ui.tests.running = true;
                                    panel_ui.tests.summary.running = true;
                                    panel_ui.tests.summary.error = None;
                                    model.status = format!("Running tests: {}", task_spec.name);
                                    spawn_test_run_task(
                                        test_run_tx.clone(),
                                        waker.clone(),
                                        wt,
                                        test_generation,
                                        task_spec,
                                        keymap.config().limits.clone(),
                                        test_sem.clone(),
                                    );
                                }
                            } else {
                                model.status = "No test task detected".into();
                            }
                            true
                        }
                        (Section::Tests, KeyCode::Char('u')) => {
                            let wt = active_tab_path(&session);
                            sync_tests_for_worktree(&mut panel_ui, &wt, keymap.config());
                            if let Some(task) = panel_ui.tests.task.clone() {
                                test_generation += 1;
                                panel_ui.tests.discovering = true;
                                panel_ui.tests.summary.error = None;
                                // Force re-discovery and record the current
                                // fingerprint so the auto-gate stays quiet after.
                                panel_ui.tests.fingerprint = crate::task::manifest_fingerprint(&wt);
                                spawn_test_discovery(
                                    test_discovery_tx.clone(),
                                    waker.clone(),
                                    wt,
                                    test_generation,
                                    task,
                                    keymap.config().limits.clone(),
                                    test_sem.clone(),
                                );
                            } else {
                                model.status = "No test task detected".into();
                            }
                            true
                        }
                        (Section::Tests, KeyCode::Char('o')) => {
                            // Open the explorer's selected test in the editor
                            // (file:line when captured, name-located otherwise).
                            let wt = active_tab_path(&session);
                            if let Some((path, line)) = resolve_open_target(&panel_ui, &wt) {
                                let cmd = editor_open_command(keymap.config(), &path, Some(line));
                                let cwd = active_cwd(&session);
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
                            } else {
                                model.status = "Could not locate the selected test".into();
                            }
                            true
                        }
                        (Section::Tests, KeyCode::Char('b')) => {
                            // Peek the selected test in bat, in a split pane.
                            let wt = active_tab_path(&session);
                            if let Some((path, line)) = resolve_open_target(&panel_ui, &wt) {
                                let bat = keymap
                                    .config()
                                    .tool_command("bat")
                                    .unwrap_or("bat --paging=always")
                                    .to_string();
                                let cmd = format!(
                                    "{bat} --highlight-line {line} {}",
                                    test_shell_quote(&path)
                                );
                                let cwd = active_cwd(&session);
                                open_command_pane(
                                    &mut session,
                                    &mut panes,
                                    focused,
                                    &cmd,
                                    cwd.as_deref(),
                                    chrome.center,
                                );
                                focus.zone = crate::focus::Zone::Center;
                                refresh_tab_model(&mut model, &session, &mut sb);
                                need_relayout = true;
                            } else {
                                model.status = "Could not locate the selected test".into();
                            }
                            true
                        }
                        // Row mode only: section mode's `d` summons the diff
                        // layer through the accordion map above.
                        (Section::Tests, KeyCode::Char('d')) => {
                            if let (Some(node), Some(task)) =
                                (panel_ui.tests.selected_node(), &panel_ui.tests.task)
                            {
                                model.status = crate::task::dap_launch_descriptor(task, &node.id);
                            } else {
                                model.status = "Select a test to prepare a debug launch".into();
                            }
                            true
                        }
                        // -- files / changes: yazi reveal + editor open ------
                        (Section::Files, KeyCode::Char('y')) => {
                            // Yazi drawer anchored at the selection's dir.
                            let wt = active_tab_path(&session);
                            let dir = changed_file_at(&model, panel_ui.cursor)
                                .and_then(|p| wt.join(&p).parent().map(|d| d.to_path_buf()))
                                .unwrap_or_else(|| wt.clone());
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
                            drawer_home = Some(dir);
                            true
                        }
                        (Section::Files, KeyCode::Char('o'))
                        | (Section::Changes, KeyCode::Char('o')) => {
                            let path = if panel_ui.open == Section::Files {
                                changed_file_at(&model, panel_ui.cursor)
                            } else {
                                panel_ui
                                    .chg_sel
                                    .or(Some(panel_ui.cursor))
                                    .and_then(|i| model.panel.changes.get(i))
                                    .map(|c| c.path.clone())
                            };
                            if let Some(path) = path {
                                let cmd = editor_open_command(keymap.config(), &path, None);
                                let cwd = active_cwd(&session);
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
                            true
                        }
                        (Section::Files, KeyCode::Char('O'))
                        | (Section::Changes, KeyCode::Char('O')) => {
                            let path = if panel_ui.open == Section::Files {
                                changed_file_at(&model, panel_ui.cursor)
                            } else {
                                panel_ui
                                    .chg_sel
                                    .or(Some(panel_ui.cursor))
                                    .and_then(|i| model.panel.changes.get(i))
                                    .map(|c| c.path.clone())
                            };
                            if let Some(path) = path {
                                let cmd = editor_open_command(keymap.config(), &path, None);
                                let cwd = active_cwd(&session);
                                open_command_pane(
                                    &mut session,
                                    &mut panes,
                                    focused,
                                    &cmd,
                                    cwd.as_deref(),
                                    chrome.center,
                                );
                                focus.zone = crate::focus::Zone::Center;
                                refresh_tab_model(&mut model, &session, &mut sb);
                                need_relayout = true;
                            }
                            true
                        }
                        (Section::Files, KeyCode::Char('\x0F'))
                        | (Section::Changes, KeyCode::Char('\x0F')) => {
                            // Ctrl-O: Open in external editor
                            let path = if panel_ui.open == Section::Files {
                                changed_file_at(&model, panel_ui.cursor)
                            } else {
                                panel_ui
                                    .chg_sel
                                    .or(Some(panel_ui.cursor))
                                    .and_then(|i| model.panel.changes.get(i))
                                    .map(|c| c.path.clone())
                            };
                            if let Some(path) = path {
                                let wt = active_tab_path(&session);
                                let abs_path = wt.join(path);
                                let cmd = editor_open_command(
                                    keymap.config(),
                                    &abs_path.to_string_lossy(),
                                    None,
                                );
                                let _ = std::process::Command::new("sh").arg("-c").arg(cmd).spawn();
                            }
                            true
                        }
                        (Section::Files, KeyCode::Char('\r'))
                        | (Section::Changes, KeyCode::Char('\r'))
                        | (Section::Files, KeyCode::Enter)
                        | (Section::Changes, KeyCode::Enter) => {
                            // Handled by PanelMsg::Select in accordion map!
                            false
                        }
                        (Section::Files, KeyCode::Char('b'))
                        | (Section::Changes, KeyCode::Char('b')) => {
                            let path = if panel_ui.open == Section::Files {
                                changed_file_at(&model, panel_ui.cursor)
                            } else {
                                panel_ui
                                    .chg_sel
                                    .or(Some(panel_ui.cursor))
                                    .and_then(|i| model.panel.changes.get(i))
                                    .map(|c| c.path.clone())
                            };
                            if let Some(path) = path {
                                let bat = keymap
                                    .config()
                                    .tool_command("bat")
                                    .unwrap_or("bat --paging=always")
                                    .to_string();
                                let cmd = format!("{bat} {}", test_shell_quote(&path));
                                let cwd = active_cwd(&session);
                                open_command_pane(
                                    &mut session,
                                    &mut panes,
                                    focused,
                                    &cmd,
                                    cwd.as_deref(),
                                    chrome.center,
                                );
                                focus.zone = crate::focus::Zone::Center;
                                refresh_tab_model(&mut model, &session, &mut sb);
                                need_relayout = true;
                            }
                            true
                        }
                        // Esc in section mode returns to the terminal (row
                        // mode's Esc is claimed by the accordion map).
                        (_, key) if crate::input::is_escape_key(&key) => {
                            focus.zone = crate::focus::Zone::Center;
                            true
                        }
                        _ => false,
                    };
                    if handled {
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
                            Action::SwitchFont => match crate::font::font_palette_items() {
                                Ok(items) if items.is_empty() => {
                                    model.status = "No fonts found via fc-list".into();
                                }
                                Ok(items) => {
                                    palette = Some(crate::palette::Palette::new(items));
                                }
                                Err(e) => {
                                    model.status = format!("Font list failed: {e}");
                                }
                            },
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
                                    panel_width,
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
                                    panel_width,
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
                                        panel_width,
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
                                        panel_width,
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
                                // Tab switches always land focus on the center
                                // terminal — the user switched to work there.
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
                            Action::PrevTab => {
                                session.prev_tab();
                                // Tab switches always land focus on the center
                                // terminal — the user switched to work there.
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
                                // Worktree switches always land focus on the
                                // center terminal — the user switched to work there.
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
                            Action::MoveWorktreeUp | Action::MoveWorktreeDown => {
                                // Reorder the active worktree within its
                                // workspace; the move method rebuilds the tree
                                // and persists the new order itself. The active
                                // group's content is unchanged, so only a redraw
                                // is needed.
                                let up = action == Action::MoveWorktreeUp;
                                if sb.move_active_worktree(&mut model, &mut session, up) {
                                    need_relayout = true;
                                }
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
                                // Toggle: zoom the focused zone, unless already
                                // zoomed or focused on a single-row bar (can't zoom).
                                zoom = if zoom.is_none() && !focus.bar() {
                                    Some(focus.zone)
                                } else {
                                    None
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
                                    panel_width,
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
                                            // Row mode walks the open section's
                                            // rows; section mode steps the
                                            // accordion itself.
                                            if panel_ui.row_mode {
                                                let (pc, pr) = panel_geom(&chrome);
                                                let max = crate::panel::frame::actionable_rows(
                                                    &model, &panel_ui, pc, pr,
                                                )
                                                .saturating_sub(1);
                                                panel_ui.cursor = if delta < 0 {
                                                    panel_ui.cursor.saturating_sub(1)
                                                } else {
                                                    (panel_ui.cursor + 1).min(max)
                                                };
                                            } else {
                                                let next = if delta < 0 {
                                                    panel_ui.prev_section()
                                                } else {
                                                    panel_ui.next_section()
                                                };
                                                open_panel_section(
                                                    next,
                                                    &mut panel_ui,
                                                    &mut hydration_gen,
                                                    &model_tx,
                                                    &session,
                                                    &waker,
                                                    PanelDocsWiring {
                                                        model: &model,
                                                        generation: docs_gen,
                                                        tx: &docs_tx,
                                                    },
                                                );
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
                                                panel_width,
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
                            Action::NewWorkspace => {
                                begin_new_workspace_prompt(&mut host_input, &mut model);
                            }
                            Action::SwitchWorkspace => {
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
                                    if wizard_ui.is_some() || creating.is_some() {
                                        model.status =
                                            "worktree creation already in progress".into();
                                    } else {
                                        // Open the wizard instantly (pure
                                        // prefill) and start the worker, which
                                        // speculatively creates the worktree
                                        // under the candidate name while the
                                        // user reads the form.
                                        create_gen += 1;
                                        let w = wizard::NewWorktreeWizard::new(
                                            root.clone(),
                                            keymap.config(),
                                        );
                                        let (cmd_tx, cmd_rx) = std::sync::mpsc::channel();
                                        let ctx = wizard::WorkerCtx {
                                            cfg: keymap.config().clone(),
                                            repo_root: root,
                                            candidate: w.candidate(),
                                            generation: create_gen,
                                            db_path: None,
                                        };
                                        let tx = create_tx.clone();
                                        let wk = waker.clone();
                                        task::spawn_blocking(move || {
                                            wizard::run_worker(ctx, cmd_rx, tx, move || {
                                                let _ = wk.wake();
                                            });
                                        });
                                        creating = Some(wizard::CreationProgress::new(
                                            create_gen,
                                            w.candidate(),
                                        ));
                                        wizard_cmd_tx = Some(cmd_tx);
                                        wizard_ui = Some(w);
                                    }
                                } else {
                                    superzej_core::msg::warn(
                                        "new-worktree: not inside a git repository",
                                    );
                                }
                            }
                            Action::NewTab => {
                                // A fresh tab WITHIN the active worktree. Eagerly
                                // spawn its shell so the new tab never reuses an
                                // existing pane — Tab::new() uses Leaf(0) as a
                                // placeholder, but pane-0 is the very first shell
                                // ever spawned, so it already exists and gets
                                // shared with the new tab if we don't override it.
                                let cwd = active_cwd(&session);
                                if let Some(g) = session.active_group_mut() {
                                    g.add_tab();
                                }
                                let cfg = keymap.config().clone();
                                match spawn_worktree_shell_pane(
                                    &mut panes,
                                    &cfg,
                                    cwd.as_deref(),
                                    chrome.center,
                                ) {
                                    Ok(id) => {
                                        if let Some(tab) = session
                                            .active_group_mut()
                                            .and_then(|g| g.active_tab_mut())
                                        {
                                            tab.center = crate::center::CenterTree::Leaf(id);
                                            tab.focused_pane = id;
                                        }
                                    }
                                    Err(e) => {
                                        model.status = format!("new tab spawn failed: {e:#}");
                                    }
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
                                        if let Ok(db) = superzej_core::db::Db::open() {
                                            forget_worktree_group(&db, &session.id, &g);
                                        }
                                    }
                                    crate::session::CloseResult::Nothing => {}
                                }
                                persist_session_layout(&session);
                                // Close always lands the user on the center
                                // terminal of whichever tab is now active.
                                focus.zone = crate::focus::Zone::Center;
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
                                if let Some(g) = session.close_active_group()
                                    && let Ok(db) = superzej_core::db::Db::open()
                                {
                                    forget_worktree_group(&db, &session.id, &g);
                                }
                                persist_session_layout(&session);
                                // Close always lands the user on the center
                                // terminal of whichever worktree is now active.
                                focus.zone = crate::focus::Zone::Center;
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
                            Action::SearchPane => {
                                // Open an incremental search overlay scoped to
                                // the focused pane's history.
                                let max = keymap.config().search.max_results;
                                search = Some(crate::search::SearchOverlay::new(
                                    superzej_core::search::SearchScope::Pane(focused),
                                    focused,
                                    max,
                                ));
                            }
                            Action::SearchGlobal => {
                                // Open the search overlay scoped to the whole
                                // active worktree; user can Tab to widen scope.
                                let max = keymap.config().search.max_results;
                                search = Some(crate::search::SearchOverlay::new(
                                    superzej_core::search::SearchScope::Worktree,
                                    focused,
                                    max,
                                ));
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
                                    panel_width,
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
                                    panel_width,
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
                                    panel_width,
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
                                        panel_width,
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
                                        panel_width,
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
                    panel_width,
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
    fn forgetting_closed_worktree_registry_prevents_restart_readoption() {
        let root = std::env::temp_dir().join(format!(
            "superzej-close-worktree-{}-{}",
            std::process::id(),
            now_secs()
        ));
        let repo = root.join("app");
        let feat = root.join("app-feat");
        std::fs::create_dir_all(&repo).unwrap();
        std::fs::create_dir_all(&feat).unwrap();
        let db = superzej_core::db::Db::open_at(&root.join("state/superzej.db")).unwrap();
        let repo_s = repo.to_string_lossy().into_owned();
        let feat_s = feat.to_string_lossy().into_owned();
        let mut session = Session {
            id: repo_s.clone(),
            worktrees: vec![
                WorktreeGroup::new("app/home", GroupKind::Home, &repo_s),
                WorktreeGroup::new("app/feat", GroupKind::Branch, &feat_s),
            ],
            active: 1,
        };
        session.persist(&db, &repo_s, now_secs()).unwrap();
        db.put_worktree("app/feat", &repo_s, &feat_s, "feat", None)
            .unwrap();

        let closing = session.worktrees[1].clone();
        forget_worktree_group(&db, &session.id, &closing);
        session.close_active_group();
        session.persist(&db, &repo_s, now_secs()).unwrap();

        let resurrected = Session::resurrect(&db, &repo_s).unwrap();
        assert_eq!(
            resurrected
                .worktrees
                .iter()
                .map(|g| g.name.as_str())
                .collect::<Vec<_>>(),
            vec!["app/home"]
        );
        assert!(
            db.worktree_for_tab(&superzej_core::db::session(), "app/feat")
                .unwrap()
                .is_none()
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn resurrect_orders_worktrees_by_persisted_position() {
        let root = std::env::temp_dir().join(format!(
            "superzej-resurrect-order-{}-{}",
            std::process::id(),
            now_secs()
        ));
        let repo = root.join("app");
        let alpha = root.join("app-alpha");
        let beta = root.join("app-beta");
        for d in [&repo, &alpha, &beta] {
            std::fs::create_dir_all(d).unwrap();
        }
        let db = superzej_core::db::Db::open_at(&root.join("state/superzej.db")).unwrap();
        let repo_s = repo.to_string_lossy().into_owned();
        let alpha_s = alpha.to_string_lossy().into_owned();
        let beta_s = beta.to_string_lossy().into_owned();

        let session = Session {
            id: repo_s.clone(),
            worktrees: vec![
                WorktreeGroup::new("app/home", GroupKind::Home, &repo_s),
                WorktreeGroup::new("app/alpha", GroupKind::Branch, &alpha_s),
                WorktreeGroup::new("app/beta", GroupKind::Branch, &beta_s),
            ],
            active: 0,
        };
        session.persist(&db, &repo_s, now_secs()).unwrap();
        // Register both branches; positions are assigned in call order.
        db.put_worktree("app/alpha", &repo_s, &alpha_s, "alpha", None)
            .unwrap();
        db.put_worktree("app/beta", &repo_s, &beta_s, "beta", None)
            .unwrap();

        // Registered branches come back in creation order (alpha before beta);
        // home has no registry row, so it sorts last in the raw session vec
        // (the sidebar floats it first at display time).
        let r = Session::resurrect(&db, &repo_s).unwrap();
        assert_eq!(
            r.worktrees
                .iter()
                .map(|g| g.name.as_str())
                .collect::<Vec<_>>(),
            vec!["app/alpha", "app/beta", "app/home"]
        );

        // A manual reorder (swap positions) survives resurrect: beta now
        // precedes alpha.
        db.swap_worktree_positions(&alpha_s, &beta_s).unwrap();
        let r = Session::resurrect(&db, &repo_s).unwrap();
        assert_eq!(
            r.worktrees
                .iter()
                .map(|g| g.name.as_str())
                .collect::<Vec<_>>(),
            vec!["app/beta", "app/alpha", "app/home"]
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn new_workspace_action_starts_path_or_url_input() {
        let mut model = FrameModel::default();
        let mut host_input = None;

        begin_new_workspace_prompt(&mut host_input, &mut model);

        let (input, kind) = host_input.expect("new workspace should open an input overlay");
        assert_eq!(kind, HostInputKind::NewWorkspace);
        assert!(
            input.title.contains("path or URL"),
            "prompt title should explain accepted input: {:?}",
            input.title
        );
        assert!(
            model.status.contains("path or URL"),
            "status should make shortcut/menu feedback visible: {:?}",
            model.status
        );
    }

    #[test]
    fn workspace_input_accepts_existing_directory_workspace() {
        let db_root = std::env::temp_dir().join(format!(
            "superzej-test-db-{}-{}",
            std::process::id(),
            now_secs()
        ));
        let db = superzej_core::db::Db::open_at(&db_root.join("superzej.db")).unwrap();
        let mut session = crate::session::Session {
            id: "/old".into(),
            ..Default::default()
        };
        let dir = std::env::temp_dir().join(format!(
            "superzej-test-ws-{}-{}",
            std::process::id(),
            now_secs()
        ));
        std::fs::create_dir_all(&dir).unwrap();

        let path = create_workspace_from_input(dir.to_str().unwrap(), &mut session, &db)
            .expect("plain directory workspaces should be accepted");

        assert_eq!(path, dir);
        assert_eq!(session.id, dir.to_string_lossy());
        assert_eq!(db.workspaces().unwrap()[0].kind, "dir");
    }

    #[test]
    fn center_context_hints_include_close_tab_and_split_controls() {
        let cfg = superzej_core::config::Config::default();
        let focus = crate::focus::FocusState::default();
        let panel = crate::panel::PanelUi::default();
        let hints = context_hints(&focus, &panel, &cfg);

        let has = |c: &str, l: &str| hints.iter().any(|(hc, hl)| hc == c && hl == l);
        assert!(has("Alt-x", "close tab"), "hints were {hints:?}");
        assert!(has("Alt-p", "smart split"), "hints were {hints:?}");
        assert!(has("Alt-n", "split↓"), "hints were {hints:?}");
        assert!(has("Alt-N", "split→"), "hints were {hints:?}");
    }

    #[test]
    fn center_context_hints_follow_keybind_overrides() {
        let mut cfg = superzej_core::config::Config::default();
        cfg.keybinds.insert("close-tab".into(), "Ctrl Alt x".into());
        let focus = crate::focus::FocusState::default();
        let panel = crate::panel::PanelUi::default();
        let hints = context_hints(&focus, &panel, &cfg);

        let has = |c: &str, l: &str| hints.iter().any(|(hc, hl)| hc == c && hl == l);
        assert!(has("Ctrl-Alt-x", "close tab"), "hints were {hints:?}");
        assert!(!has("Alt-x", "close tab"), "hints were {hints:?}");
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

    #[test]
    fn drawer_cancel_keys_hide_the_file_picker() {
        assert!(drawer_cancel_key(&KeyCode::Escape, Modifiers::NONE));
        assert!(drawer_cancel_key(&KeyCode::Char('\x1b'), Modifiers::NONE));
        assert!(drawer_cancel_key(&KeyCode::Char('q'), Modifiers::NONE));
        assert!(drawer_cancel_key(&KeyCode::Char('Q'), Modifiers::SHIFT));
        assert!(!drawer_cancel_key(&KeyCode::Char('q'), Modifiers::CTRL));
        assert!(!drawer_cancel_key(&KeyCode::Char('j'), Modifiers::NONE));
    }

    #[test]
    fn font_palette_has_escape_ctrl_c_and_empty_q_cancels() {
        let mut p = crate::palette::Palette::new(vec![crate::palette::PaletteItem::new(
            "font:JetBrainsMono Nerd Font",
            "JetBrainsMono Nerd Font",
        )]);
        assert!(palette_cancel_key(&p, &KeyCode::Escape, Modifiers::NONE));
        assert!(palette_cancel_key(
            &p,
            &KeyCode::Char('\x1b'),
            Modifiers::NONE
        ));
        assert!(palette_cancel_key(&p, &KeyCode::Char('c'), Modifiers::CTRL));
        assert!(palette_cancel_key(&p, &KeyCode::Char('q'), Modifiers::NONE));

        p.push_char('j');
        assert!(!palette_cancel_key(
            &p,
            &KeyCode::Char('q'),
            Modifiers::NONE
        ));
    }

    #[test]
    fn generic_command_palette_does_not_treat_plain_q_as_cancel() {
        let p =
            crate::palette::Palette::new(vec![crate::palette::PaletteItem::new("quit", "Quit")]);
        assert!(!palette_cancel_key(
            &p,
            &KeyCode::Char('q'),
            Modifiers::NONE
        ));
        assert!(palette_cancel_key(&p, &KeyCode::Escape, Modifiers::NONE));
        assert!(palette_cancel_key(
            &p,
            &KeyCode::Char('\x1b'),
            Modifiers::NONE
        ));
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

    fn three_worktree_session() -> Session {
        Session {
            id: "s1".into(),
            worktrees: vec![
                WorktreeGroup::new("app/home", GroupKind::Home, "/tmp/app"),
                WorktreeGroup::new("app/alpha", GroupKind::Branch, "/tmp/app-alpha"),
                WorktreeGroup::new("app/beta", GroupKind::Branch, "/tmp/app-beta"),
            ],
            active: 2, // beta
        }
    }

    #[test]
    fn move_active_worktree_reorders_within_workspace_and_anchors_home() {
        // Holds the env lock: move_active_worktree opens the user DB to persist
        // the swap; point it at a throwaway scope (the swap no-ops on unknown
        // paths — we assert the in-memory reorder, which is the user-visible bit).
        let _env = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let state_home = std::env::temp_dir().join(format!(
            "superzej-move-wt-{}-{}",
            std::process::id(),
            now_secs()
        ));
        // SAFETY: test holds ENV_LOCK; sets/clears an XDG var around the calls.
        unsafe { std::env::set_var("XDG_STATE_HOME", &state_home) };

        let mut session = three_worktree_session();
        let mut model = build_initial_model(&session);
        model.sidebar_workspaces = vec![("app".into(), "app".into(), "repo".into(), String::new())];
        let mut sb = SidebarState::default();
        sb.rebuild(&mut model, &session);

        // Move beta up: it swaps with alpha and remains active.
        assert!(sb.move_active_worktree(&mut model, &mut session, true));
        let order: Vec<&str> = session.worktrees.iter().map(|g| g.name.as_str()).collect();
        assert_eq!(order, vec!["app/home", "app/beta", "app/alpha"]);
        assert_eq!(session.worktrees[session.active].name, "app/beta");

        // Move beta up again: the slot above is home — blocked, nothing moves.
        assert!(!sb.move_active_worktree(&mut model, &mut session, true));
        let order: Vec<&str> = session.worktrees.iter().map(|g| g.name.as_str()).collect();
        assert_eq!(order, vec!["app/home", "app/beta", "app/alpha"]);

        unsafe { std::env::remove_var("XDG_STATE_HOME") };
        let _ = std::fs::remove_dir_all(&state_home);
    }

    #[test]
    fn move_under_computed_sort_flips_to_manual() {
        let _env = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let state_home = std::env::temp_dir().join(format!(
            "superzej-move-flip-{}-{}",
            std::process::id(),
            now_secs()
        ));
        // SAFETY: test holds ENV_LOCK; sets/clears an XDG var around the calls.
        unsafe { std::env::set_var("XDG_STATE_HOME", &state_home) };

        let mut session = three_worktree_session();
        let mut model = build_initial_model(&session);
        model.sidebar_workspaces = vec![("app".into(), "app".into(), "repo".into(), String::new())];
        let mut sb = SidebarState::default();
        sb.view.sort = crate::sidebar::SortMode::Name;
        sb.rebuild(&mut model, &session);

        // Moving under a computed sort flips the workspace to Manual so the move
        // is visible and persists.
        assert!(sb.move_active_worktree(&mut model, &mut session, true));
        assert_eq!(sb.view.sort, crate::sidebar::SortMode::Manual);

        unsafe { std::env::remove_var("XDG_STATE_HOME") };
        let _ = std::fs::remove_dir_all(&state_home);
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
            SidebarOutcome::CloseGroups(t) => {
                assert_eq!(t.len(), 2);
            }
            _ => panic!("expected CloseGroups"),
        }
    }

    #[test]
    fn sidebar_destructive_actions_reanchor_cursor_to_active_row() {
        let mut session = two_worktree_session();
        let mut model = build_initial_model(&session);
        model.sidebar_workspaces = vec![("app".into(), "app".into(), "repo".into(), String::new())];
        let mut sb = focused_state(&mut model, &session);
        sb.cursor = 0; // stale cursor on the workspace header after a delete/re-sort

        session.switch_to(1);
        refresh_tab_model(&mut model, &session, &mut sb);
        sb.focus_active_row(&mut model);

        let row = sb
            .selected_row(&model)
            .expect("active row should be visible");
        assert!(row.active, "cursor should land on active row, got {row:?}");
        assert_eq!(row.label, "feat");
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
    fn load_or_seed_session_ignores_launch_directory() {
        // Directory-agnostic: launching from an unrelated cwd reopens the
        // most-recently-active workspace, never a workspace keyed to the cwd.
        let _env = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let state_home =
            std::env::temp_dir().join(format!("test_db_agnostic_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&state_home);
        let db_path = state_home.join("superzej/superzej.db");
        std::fs::create_dir_all(db_path.parent().unwrap()).unwrap();

        let db = superzej_core::db::Db::open_at(&db_path).unwrap();
        // A registered workspace unrelated to the launch cwd below.
        let _ = db.put_workspace("/tmp/app", "app", "repo");
        let wt_dir = state_home.join("app-feat");
        std::fs::create_dir_all(&wt_dir).unwrap();
        db.put_tab_group(
            "/tmp/app",
            &superzej_core::models::TabGroupRow {
                name: "app/feat".into(),
                kind: "branch".into(),
                worktree: wt_dir.to_string_lossy().into_owned(),
                ordinal: 0,
                active_tab: 0,
            },
        )
        .unwrap();

        // SAFETY: test holds ENV_LOCK; sets/clears an XDG var around one call.
        unsafe { std::env::set_var("XDG_STATE_HOME", &state_home) };
        // Launch from a directory unrelated to either workspace.
        let (session, _) = load_or_seed_session(std::path::Path::new("/tmp/somewhere-unrelated"));
        unsafe { std::env::remove_var("XDG_STATE_HOME") };
        let _ = std::fs::remove_dir_all(&state_home);

        assert_eq!(
            session.id, "/tmp/app",
            "should reopen the most-recently-active workspace, not the cwd"
        );
        assert!(
            session.worktrees.iter().any(|g| g.name == "app/feat"),
            "the recent workspace's worktree should be present"
        );
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
        let model = build_model(&session, &db, crate::hydrate::HydrateHints::default());

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

    // ── drain helpers ────────────────────────────────────────────────────────

    fn mk_key(code: KeyCode) -> InputEvent {
        InputEvent::Key(termwiz::input::KeyEvent {
            key: code,
            modifiers: Modifiers::NONE,
        })
    }

    fn mk_wheel(up: bool) -> InputEvent {
        use termwiz::input::{MouseButtons, MouseEvent};
        let mut buttons = MouseButtons::VERT_WHEEL;
        if up {
            buttons |= MouseButtons::WHEEL_POSITIVE;
        }
        InputEvent::Mouse(MouseEvent {
            x: 1,
            y: 1,
            mouse_buttons: buttons,
            modifiers: Modifiers::NONE,
        })
    }

    #[test]
    fn drain_key_repeats_coalesces_identical_keys() {
        let key = termwiz::input::KeyEvent {
            key: KeyCode::DownArrow,
            modifiers: Modifiers::NONE,
        };
        // Three identical repeats then a different key.
        let mut q: std::collections::VecDeque<InputEvent> = [
            mk_key(KeyCode::DownArrow),
            mk_key(KeyCode::DownArrow),
            mk_key(KeyCode::Char('x')),
        ]
        .into();
        let (n, leftover) = drain_key_repeats(&key, || q.pop_front());
        assert_eq!(n, 3);
        assert!(matches!(
            leftover,
            Some(InputEvent::Key(k)) if k.key == KeyCode::Char('x')
        ));
        // Empty queue → just the first (count = 1).
        let (n, leftover) = drain_key_repeats(&key, || None);
        assert_eq!(n, 1);
        assert!(leftover.is_none());
    }

    #[test]
    fn drain_key_repeats_stops_on_different_modifiers() {
        let key = termwiz::input::KeyEvent {
            key: KeyCode::DownArrow,
            modifiers: Modifiers::NONE,
        };
        // Same key but with Shift — must stop the drain.
        let shifted = InputEvent::Key(termwiz::input::KeyEvent {
            key: KeyCode::DownArrow,
            modifiers: Modifiers::SHIFT,
        });
        let mut q: std::collections::VecDeque<InputEvent> =
            [mk_key(KeyCode::DownArrow), shifted].into();
        let (n, leftover) = drain_key_repeats(&key, || q.pop_front());
        assert_eq!(n, 2); // first + one plain repeat
        assert!(matches!(
            leftover,
            Some(InputEvent::Key(k)) if k.modifiers == Modifiers::SHIFT
        ));
    }

    #[test]
    fn drain_wheel_ticks_coalesces_same_direction() {
        // 4 up ticks then a down tick.
        let mut q: std::collections::VecDeque<InputEvent> = [
            mk_wheel(true),
            mk_wheel(true),
            mk_wheel(true),
            mk_wheel(false), // opposite direction — stops drain
        ]
        .into();
        let (n, leftover) = drain_wheel_ticks(true, || q.pop_front());
        assert_eq!(n, 4, "first tick + 3 repeats = 4 total");
        assert!(
            matches!(&leftover, Some(InputEvent::Mouse(m)) if !m.mouse_buttons.contains(termwiz::input::MouseButtons::WHEEL_POSITIVE)),
            "leftover should be the down-wheel event"
        );
        // The leftover is back in the caller's hands; the queue should be empty now.
        assert!(q.is_empty());
    }

    #[test]
    fn drain_wheel_ticks_stops_on_direction_reversal() {
        // Only one tick in the queue before a reversal.
        let mut q: std::collections::VecDeque<InputEvent> = [mk_wheel(false)].into();
        let (n, leftover) = drain_wheel_ticks(true, || q.pop_front());
        // count = 1 (the original event the caller already consumed), leftover = the down.
        assert_eq!(n, 1);
        assert!(leftover.is_some());
    }

    #[test]
    fn drain_wheel_ticks_stops_on_non_wheel_event() {
        // A keypress interrupts the wheel drain.
        let mut q: std::collections::VecDeque<InputEvent> =
            [mk_wheel(true), mk_key(KeyCode::Char('q'))].into();
        let (n, leftover) = drain_wheel_ticks(true, || q.pop_front());
        assert_eq!(n, 2, "first + one more wheel = 2");
        assert!(matches!(leftover, Some(InputEvent::Key(_))));
    }

    #[test]
    fn drain_wheel_ticks_empty_queue_returns_one() {
        let (n, leftover) = drain_wheel_ticks(true, || None);
        assert_eq!(n, 1);
        assert!(leftover.is_none());
    }

    #[test]
    fn drain_wheel_ticks_down_direction() {
        // Symmetric: draining down-wheel events.
        let mut q: std::collections::VecDeque<InputEvent> =
            [mk_wheel(false), mk_wheel(false), mk_wheel(true)].into();
        let (n, leftover) = drain_wheel_ticks(false, || q.pop_front());
        assert_eq!(n, 3);
        assert!(
            matches!(&leftover, Some(InputEvent::Mouse(m)) if m.mouse_buttons.contains(termwiz::input::MouseButtons::WHEEL_POSITIVE))
        );
    }

    #[test]
    fn drain_event_repeats_stops_on_first_mismatch() {
        // Exercise the generic core directly with an arbitrary predicate over
        // InputEvent: accept only Char('a') keys; stop on Char('b').
        let mut q: std::collections::VecDeque<InputEvent> = [
            mk_key(KeyCode::Char('a')),
            mk_key(KeyCode::Char('a')),
            mk_key(KeyCode::Char('b')), // mismatch → leftover
            mk_key(KeyCode::Char('a')), // unreachable in this drain
        ]
        .into();
        let (n, leftover) = drain_event_repeats(
            |ev| matches!(ev, InputEvent::Key(k) if k.key == KeyCode::Char('a')),
            || q.pop_front(),
        );
        assert_eq!(n, 3); // first + 2 repeats
        assert!(matches!(
            leftover,
            Some(InputEvent::Key(k)) if k.key == KeyCode::Char('b')
        ));
        // The 4th event is still in the queue (not consumed by the drain).
        assert_eq!(q.len(), 1);
    }

    #[test]
    fn drain_event_repeats_all_matching_drains_to_empty() {
        let mut q: std::collections::VecDeque<InputEvent> =
            [mk_key(KeyCode::Char('a')), mk_key(KeyCode::Char('a'))].into();
        let (n, leftover) = drain_event_repeats(
            |ev| matches!(ev, InputEvent::Key(k) if k.key == KeyCode::Char('a')),
            || q.pop_front(),
        );
        assert_eq!(n, 3); // first + 2 from queue
        assert!(leftover.is_none());
        assert!(q.is_empty());
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
