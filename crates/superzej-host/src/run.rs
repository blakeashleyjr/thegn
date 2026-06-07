//! The spike's interactive loop: own the outer terminal, run one shell pane
//! inside the chrome cross, render it, route input. Single-threaded poll loop —
//! `poll_input` doubles as the ~60fps frame tick; pane output is coalesced
//! between polls and painted via `BufferedTerminal::draw_from_screen` + `flush`,
//! which diffs against the prior frame and emits only changed cells (no
//! clear-and-redraw → no flashing). The tokio mpsc event loop arrives in Phase 2.

use anyhow::{Context, Result};
use std::path::Path;
use std::sync::mpsc::{Receiver, TryRecvError, channel};
use std::time::Duration;

use termwiz::caps::Capabilities;
use termwiz::input::{InputEvent, KeyCode, Modifiers};
use termwiz::surface::{Change, Position, Surface};
use termwiz::terminal::buffered::BufferedTerminal;
use termwiz::terminal::{Terminal, new_terminal};

use crate::chrome::{FrameModel, render_tab};
use crate::compositor::Rect;
use crate::layout;
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

fn pane_shell_argv() -> Vec<String> {
    let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".into());
    let login = std::env::var("SUPERZEJ_LOGIN_SHELL")
        .map(|v| matches!(v.as_str(), "1" | "true" | "TRUE" | "yes" | "YES"))
        .unwrap_or(false);
    shell_argv_from(&shell, login)
}

/// Translate a termwiz key event into the bytes a terminal app expects on stdin.
fn key_bytes(key: &KeyCode, mods: Modifiers) -> Option<Vec<u8>> {
    match key {
        KeyCode::Char(c) => {
            if mods.contains(Modifiers::CTRL) {
                let b = (c.to_ascii_uppercase() as u8).wrapping_sub(0x40);
                Some(vec![b & 0x1f])
            } else {
                let mut buf = [0u8; 4];
                Some(c.encode_utf8(&mut buf).as_bytes().to_vec())
            }
        }
        KeyCode::Enter => Some(vec![b'\r']),
        KeyCode::Backspace => Some(vec![0x7f]),
        KeyCode::Tab => Some(vec![b'\t']),
        KeyCode::Escape => Some(vec![0x1b]),
        KeyCode::LeftArrow => Some(b"\x1b[D".to_vec()),
        KeyCode::RightArrow => Some(b"\x1b[C".to_vec()),
        KeyCode::UpArrow => Some(b"\x1b[A".to_vec()),
        KeyCode::DownArrow => Some(b"\x1b[B".to_vec()),
        KeyCode::Delete => Some(b"\x1b[3~".to_vec()),
        KeyCode::Home => Some(b"\x1b[H".to_vec()),
        KeyCode::End => Some(b"\x1b[F".to_vec()),
        _ => None,
    }
}

/// Build the palette's item list: the command actions + a nav row per open tab
/// (`tab:<name>`), ordered by frecency for the empty-query view (the host port
/// of the old engine's command + nav + frecency sources).
fn build_palette(session: &crate::session::Session) -> Vec<crate::palette::PaletteItem> {
    use crate::palette::PaletteItem;
    let mut items = vec![
        PaletteItem::new("new-worktree", "New worktree"),
        PaletteItem::new("new-workspace", "New workspace"),
        PaletteItem::new("switch-workspace", "Switch workspace"),
        PaletteItem::new("close-worktree", "Close worktree"),
        PaletteItem::new("show-diff", "Show diff"),
        PaletteItem::new("open-pr", "Open pull request"),
        PaletteItem::new("files-drawer", "Toggle files drawer"),
        PaletteItem::new("lazygit", "Open lazygit"),
        PaletteItem::new("quit", "Quit superzej"),
    ];
    for t in &session.tabs {
        items.push(PaletteItem::new(
            format!("tab:{}", t.name),
            format!("→ {}", t.name),
        ));
    }
    let usage = superzej_core::db::Db::open()
        .ok()
        .and_then(|db| db.palette_usage().ok())
        .unwrap_or_default();
    crate::palette::order_by_frecency(items, &usage)
}

pub fn now_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Resurrect the persisted tab list, seeding a single Home tab for the current
/// worktree if the session is empty (and persisting it so the next launch
/// restores it). The native host owns this — it's the resurrect path that
/// replaced zellij's session serialization.
fn load_or_seed_session(cwd: &std::path::Path) -> crate::session::Session {
    use crate::center::CenterTree;
    use crate::session::{Session, Tab, TabKind};

    let sess = superzej_core::db::session();
    let base = cwd
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "workspace".into());

    let session_name = if let Ok(state_home) = std::env::var("XDG_STATE_HOME") {
        // Use the explicit DB in test scenarios
        let path = std::path::Path::new(&state_home).join("superzej/superzej.db");
        if let Ok(db) = superzej_core::db::Db::open_at(&path) {
            db.workspaces()
                .unwrap_or_default()
                .into_iter()
                .find(|w| Path::new(&w.repo_path) == cwd || w.repo_path == sess)
                .map(|w| w.repo_path)
                .unwrap_or_else(|| sess.clone())
        } else {
            sess.clone()
        }
    } else if let Ok(db) = superzej_core::db::Db::open() {
        // Use the workspace from DB if available for cwd
        db.workspaces()
            .unwrap_or_default()
            .into_iter()
            .find(|w| Path::new(&w.repo_path) == cwd || w.repo_path == sess)
            .map(|w| w.repo_path)
            .unwrap_or_else(|| sess.clone())
    } else {
        sess.clone()
    };

    let Ok(db) = (if let Ok(state_home) = std::env::var("XDG_STATE_HOME") {
        let path = std::path::Path::new(&state_home).join("superzej/superzej.db");
        superzej_core::db::Db::open_at(&path)
    } else {
        superzej_core::db::Db::open()
    }) else {
        // No DB — synthesize an ephemeral single-tab session.
        return Session {
            id: sess.to_string(),
            tabs: vec![Tab {
                name: format!("{base}/home"),
                kind: TabKind::Home,
                worktree: cwd.to_string_lossy().into_owned(),
                center: CenterTree::Leaf(0),
                focused_pane: 0,
            }],
            active: 0,
        };
    };

    let mut session = Session::resurrect(&db, &session_name).unwrap_or_default();
    if session.tabs.is_empty() {
        session.tabs.push(Tab {
            name: format!("{base}/home"),
            kind: TabKind::Home,
            worktree: cwd.to_string_lossy().into_owned(),
            center: CenterTree::Leaf(0),
            focused_pane: 0,
        });
        session.active = 0;
        let _ = session.persist(&db, &session_name, now_secs());
    }
    session.id = session_name; // Need to add id to session
    session
}

fn active_tab_path(session: &crate::session::Session) -> std::path::PathBuf {
    session
        .tabs
        .get(session.active)
        .and_then(|t| {
            (!t.worktree.is_empty() && std::path::Path::new(&t.worktree).is_dir())
                .then(|| std::path::PathBuf::from(&t.worktree))
        })
        .or_else(|| std::env::current_dir().ok())
        .unwrap_or_else(|| ".".into())
}

/// A cheap first-frame model: no git, no diff, no DB recents. It gives the
/// user immediate chrome/status while the expensive model hydrates in the
/// background.
fn build_initial_model(session: &crate::session::Session) -> FrameModel {
    let active_name = session
        .tabs
        .get(session.active)
        .map(|t| t.name.clone())
        .unwrap_or_else(|| "workspace/home".into());
    FrameModel {
        tabs: session.tabs.iter().map(|t| t.name.clone()).collect(),
        active_tab: session.active,
        sidebar: vec!["hydrating…".into()],
        sidebar_selected: 0,
        panel: vec![active_name, "hydrating git status…".into()],
        status: "Starting szhost… panes are usable while git status hydrates".into(),
        accent: superzej_core::theme::TEAL.to_string(),
    }
}

/// Build the chrome model from the resurrected session + the current worktree's
/// git state (best-effort — the host stays up even with no repo / no DB). This
/// is the in-process data flow the chrome relies on: read core + svc directly,
/// no IPC. This can be slow on large repos, so launch calls it on a background
/// worker after the first frame is already possible.
fn build_model(session: &crate::session::Session) -> FrameModel {
    use superzej_core::remote::GitLoc;
    use superzej_svc::git::{GitBackend, GixGit};

    let cwd = active_tab_path(session);
    let loc = GitLoc::for_worktree(&cwd);
    let git = GixGit::new();
    let branch = git.current_branch(&loc).unwrap_or_else(|_| "—".into());

    // Sidebar: workspaces from the DB (best-effort).
    let sidebar = superzej_core::db::Db::open()
        .ok()
        .and_then(|db| db.workspaces().ok())
        .map(|ws| ws.into_iter().map(|w| w.name).collect())
        .unwrap_or_default();

    // Panel: a quick diff summary for the active worktree.
    let mut panel = vec![branch.clone(), String::new()];
    if let Ok(files) = git.diff_files(&loc, "HEAD") {
        let (add, del): (u32, u32) = files
            .iter()
            .fold((0, 0), |(a, d), f| (a + f.added, d + f.deleted));
        panel.push(format!("{} files  +{add} -{del}", files.len()));
    }

    FrameModel {
        tabs: session.tabs.iter().map(|t| t.name.clone()).collect(),
        active_tab: session.active,
        sidebar,
        sidebar_selected: 0,
        panel,
        status: "Cmd-K menu   Alt-w worktree   Alt-o switch   Ctrl-Q quit".into(),
        accent: superzej_core::theme::TEAL.to_string(),
    }
}

fn apply_mode_status(model: &mut FrameModel, mode: crate::keymap::Mode) {
    model.status = format!(
        "{} mode   Ctrl-Alt-v vim   Ctrl-Alt-e emacs   Ctrl-Alt-n normal   Ctrl-K menu   Alt-w worktree",
        mode.as_str()
    );
}

fn spawn_model_hydration(session: crate::session::Session) -> Receiver<FrameModel> {
    let (tx, rx) = channel();
    std::thread::spawn(move || {
        let _ = tx.send(build_model(&session));
    });
    rx
}

pub fn main() -> Result<()> {
    let caps = Capabilities::new_from_env().context("term capabilities")?;
    let mut term = new_terminal(caps).context("open terminal")?;
    term.set_raw_mode().context("raw mode")?;
    term.enter_alternate_screen().context("alt screen")?;
    let size = term.get_screen_size().context("screen size")?;
    let (rows, cols) = (size.rows, size.cols);

    let mut buf = BufferedTerminal::new(term).context("buffered terminal")?;

    let cwd = std::env::current_dir().unwrap_or_else(|_| ".".into());
    let session = load_or_seed_session(&cwd);
    let cfg =
        superzej_core::config::Config::load_layered(&superzej_core::config::ProcessEnv, None, None);
    let keymap = crate::keymap::default_keymap_with_config(&cfg);
    let mode = crate::keymap::Mode::Normal;
    let mut model = build_initial_model(&session);
    apply_mode_status(&mut model, mode);
    let model_rx = spawn_model_hydration(session.clone());

    let result = event_loop(&mut buf, session, model, model_rx, rows, cols, keymap, mode);

    let _ = buf.terminal().exit_alternate_screen();
    let _ = buf.terminal().set_cooked_mode();
    result
}

/// The global pane registry. A tab's panes are identified by the real ids in its
/// `CenterTree`; this just owns the live `PtyPane`s keyed by id.
struct Panes {
    table: std::collections::HashMap<u32, PtyPane>,
    next_id: u32,
    tx: std::sync::mpsc::Sender<PaneEvent>,
}

impl Panes {
    fn new(tx: std::sync::mpsc::Sender<PaneEvent>) -> Self {
        Self {
            table: std::collections::HashMap::new(),
            next_id: 1,
            tx,
        }
    }

    /// Spawn one shell pane in `cwd`, sized to `center`; returns its id.
    fn spawn(&mut self, cwd: Option<&std::path::Path>, center: Rect) -> Result<u32> {
        let id = self.next_id;
        self.next_id += 1;
        let argv = pane_shell_argv();
        let pane = PtyPane::spawn(
            id,
            &argv,
            cwd,
            center.rows.max(1) as u16,
            center.cols.max(1) as u16,
            self.tx.clone(),
        )?;
        self.table.insert(id, pane);
        Ok(id)
    }

    /// Ensure every leaf in `tab.center` is backed by a live pane. On first focus
    /// (or after resurrect, whose ids are stale) this spawns fresh panes and
    /// remaps the tree's leaf ids + the focused id onto them.
    fn materialize(&mut self, tab: &mut crate::session::Tab, center: Rect) -> Result<()> {
        let leaves = tab.center.pane_ids();
        if leaves.iter().all(|id| self.table.contains_key(id)) {
            return Ok(()); // already live
        }
        let cwd = (!tab.worktree.is_empty() && std::path::Path::new(&tab.worktree).is_dir())
            .then(|| std::path::PathBuf::from(&tab.worktree))
            .or_else(|| std::env::current_dir().ok());
        let mut map = std::collections::HashMap::new();
        for old in &leaves {
            if !map.contains_key(old) {
                let fresh = self.spawn(cwd.as_deref(), center)?;
                map.insert(*old, fresh);
            }
        }
        let old_focus = tab.focused_pane;
        tab.center.remap(&mut |old| map[&old]);
        tab.focused_pane = map
            .get(&old_focus)
            .copied()
            .or_else(|| tab.center.pane_ids().first().copied())
            .unwrap_or(0);
        Ok(())
    }
}

/// Resize each pane in `tree` to the rect it occupies within `center`.
fn relayout(panes: &mut Panes, tree: &crate::center::CenterTree, center: Rect) {
    for (id, rect) in tree.layout(center) {
        if let Some(p) = panes.table.get_mut(&id) {
            let _ = p.resize(rect.rows.max(1) as u16, rect.cols.max(1) as u16);
        }
    }
}

fn refresh_tab_model(model: &mut FrameModel, session: &crate::session::Session) {
    model.tabs = session.tabs.iter().map(|t| t.name.clone()).collect();
    model.active_tab = session.active;
}

#[derive(Debug, Clone, Copy)]
struct PtyDrainBudget {
    max_chunks: usize,
    max_bytes: usize,
}

impl Default for PtyDrainBudget {
    fn default() -> Self {
        Self {
            max_chunks: 64,
            max_bytes: 512 * 1024,
        }
    }
}

#[derive(Debug, Default, PartialEq, Eq)]
struct PtyDrainStats {
    chunks: usize,
    bytes: usize,
    budget_exhausted: bool,
    disconnected: bool,
}

fn drain_pty_events(
    rx: &Receiver<PaneEvent>,
    budget: PtyDrainBudget,
    mut handle: impl FnMut(PaneEvent),
) -> PtyDrainStats {
    let mut stats = PtyDrainStats::default();
    loop {
        if stats.chunks >= budget.max_chunks || stats.bytes >= budget.max_bytes {
            stats.budget_exhausted = true;
            break;
        }
        match rx.try_recv() {
            Ok(ev) => {
                stats.chunks += 1;
                stats.bytes += ev.byte_len();
                handle(ev);
            }
            Err(TryRecvError::Empty) => break,
            Err(TryRecvError::Disconnected) => {
                stats.disconnected = true;
                break;
            }
        }
    }
    stats
}
fn tab_cwd(tab: &crate::session::Tab) -> Option<std::path::PathBuf> {
    (!tab.worktree.is_empty() && std::path::Path::new(&tab.worktree).is_dir())
        .then(|| std::path::PathBuf::from(&tab.worktree))
        .or_else(|| std::env::current_dir().ok())
}

#[allow(clippy::too_many_arguments)]
fn event_loop<T: Terminal>(
    buf: &mut BufferedTerminal<T>,
    mut session: crate::session::Session,
    mut model: FrameModel,
    model_rx: Receiver<FrameModel>,
    mut rows: usize,
    mut cols: usize,
    mut keymap: crate::keymap::KeyMap,
    mut mode: crate::keymap::Mode,
) -> Result<()> {
    let mut scratch = Surface::new(cols, rows);
    let mut want_sidebar = true;
    let mut want_panel = true;
    let mut chrome = layout::compute(cols, rows, want_sidebar, want_panel);
    let mut dirty = true;
    let mut palette: Option<crate::palette::Palette> = None;

    let (tx, rx) = channel::<PaneEvent>();
    let mut panes = Panes::new(tx);
    let mut need_relayout = true;

    loop {
        if session.tabs.is_empty() {
            return Ok(()); // last tab closed
        }
        let active = session.active;
        // The active tab's panes are spawned lazily on first focus.
        panes.materialize(&mut session.tabs[active], chrome.center)?;
        if need_relayout {
            let tree = session.tabs[active].center.clone();
            relayout(&mut panes, &tree, chrome.center);
            need_relayout = false;
        }
        let focused = session.tabs[active].focused_pane;
        let tree = session.tabs[active].center.clone();
        let visible: Vec<u32> = tree.pane_ids();

        // 1. Drain pending PTY output, routed by pane id. Only output from a pane
        //    visible in the active tab dirties the frame; others advance silently.
        //    The drain is budgeted so a chatty pane cannot starve rendering/input.
        let drain = drain_pty_events(&rx, PtyDrainBudget::default(), |ev| match ev {
            PaneEvent::Output(id, b) => {
                if let Some(p) = panes.table.get_mut(&id) {
                    p.feed(&b);
                    if visible.contains(&id) {
                        dirty = true;
                    }
                }
            }
            PaneEvent::Exit(id) => {
                panes.table.remove(&id);
                // Find the owning tab and either drop the pane from its split
                // or, if it was the tab's only pane, close the tab.
                if let Some(ti) = session
                    .tabs
                    .iter()
                    .position(|t| t.center.pane_ids().contains(&id))
                {
                    let sole = session.tabs[ti].center.pane_ids().len() == 1;
                    if sole {
                        if ti == session.active {
                            session.close_active();
                        } else {
                            session.tabs.remove(ti);
                            if session.active > ti {
                                session.active -= 1;
                            }
                        }
                        refresh_tab_model(&mut model, &session);
                    } else {
                        session.tabs[ti].center.remove(id);
                        if session.tabs[ti].focused_pane == id {
                            if let Some(first) = session.tabs[ti].center.pane_ids().first() {
                                session.tabs[ti].focused_pane = *first;
                            }
                        }
                        need_relayout = true;
                    }
                }
                dirty = true;
            }
        });
        if drain.disconnected {
            return Ok(());
        }
        if drain.budget_exhausted {
            dirty = true;
        }
        if session.tabs.is_empty() {
            return Ok(());
        }

        while let Ok(next_model) = model_rx.try_recv() {
            model = next_model;
            refresh_tab_model(&mut model, &session);
            apply_mode_status(&mut model, mode);
            dirty = true;
        }

        // 2. Render if anything changed (diff-flush): all visible panes of the
        //    active tab + the chrome, with the hardware cursor in the focused pane.
        if dirty {
            if scratch.dimensions() != (cols, rows) {
                scratch = Surface::new(cols, rows);
            }
            crate::chrome::clear_frame(&mut scratch);
            render_tab(&mut scratch, &chrome, &tree, focused, &model, |id| {
                panes.table.get(&id).map(|p| p.emulator())
            });
            if let Some(pal) = &palette {
                pal.render(
                    &mut scratch,
                    Rect {
                        x: 0,
                        y: 0,
                        cols,
                        rows,
                    },
                );
            }
            buf.draw_from_screen(&scratch, 0, 0);
            if palette.is_none() {
                let focused_rect = tree
                    .layout(chrome.center)
                    .into_iter()
                    .find(|(id, _)| *id == focused)
                    .map(|(_, r)| r);
                if let (Some(rect), Some(p)) = (focused_rect, panes.table.get(&focused)) {
                    let (cur_row, cur_col) = p.emulator().cursor();
                    buf.add_change(Change::CursorPosition {
                        x: Position::Absolute(rect.x + cur_col as usize),
                        y: Position::Absolute(rect.y + cur_row as usize),
                    });
                }
            }
            buf.flush().context("flush")?;
            dirty = false;
        }

        // 3. Poll input (also the ~60fps frame tick).
        match buf.terminal().poll_input(Some(Duration::from_millis(16))) {
            Ok(Some(InputEvent::Key(k))) => {
                // Modal: when the palette is open it captures all keys.
                if let Some(p) = palette.as_mut() {
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
                                // Nav: jump to an open tab.
                                if let Some(name) = key.strip_prefix("tab:") {
                                    if let Some(i) =
                                        session.tabs.iter().position(|t| t.name == name)
                                    {
                                        session.switch_to(i);
                                        refresh_tab_model(&mut model, &session);
                                        need_relayout = true;
                                    }
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
                    dirty = true;
                    continue;
                }
                // Global/mode chords are intercepted by the keymap; everything
                // else is forwarded to the focused pane.
                let input_key = crate::sequence::Key::modified(k.key, k.modifiers);
                match keymap.dispatch(mode, input_key) {
                    crate::sequence::MatchResult::Matched(action) => {
                        use crate::keymap::Action;
                        match action {
                            Action::SwitchMode(next) => {
                                mode = next;
                                keymap.reset();
                                apply_mode_status(&mut model, mode);
                            }
                            Action::Custom(idx) => {
                                if let Some(ca) = keymap.custom_actions().get(idx as usize) {
                                    let mut cmd =
                                        std::process::Command::new(superzej_core::util::shell());
                                    cmd.arg("-c").arg(&ca.run);
                                    if ca.floating {
                                        let cwd = tab_cwd(&session.tabs[active]);
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
                                palette =
                                    Some(crate::palette::Palette::new(build_palette(&session)));
                            }
                            Action::ToggleSidebar => {
                                want_sidebar = !want_sidebar;
                                chrome = layout::compute(cols, rows, want_sidebar, want_panel);
                                need_relayout = true;
                            }
                            Action::TogglePanel => {
                                want_panel = !want_panel;
                                chrome = layout::compute(cols, rows, want_sidebar, want_panel);
                                need_relayout = true;
                            }
                            Action::NextTab => {
                                session.next_tab();
                                refresh_tab_model(&mut model, &session);
                                need_relayout = true;
                            }
                            Action::PrevTab => {
                                session.prev_tab();
                                refresh_tab_model(&mut model, &session);
                                need_relayout = true;
                            }
                            Action::SplitDown | Action::SplitRight => {
                                let dir = if action == Action::SplitDown {
                                    crate::center::Dir::Col
                                } else {
                                    crate::center::Dir::Row
                                };
                                let cwd = tab_cwd(&session.tabs[active]);
                                let new = panes.spawn(cwd.as_deref(), chrome.center)?;
                                if session.tabs[active].center.split(focused, dir, new) {
                                    session.tabs[active].focused_pane = new;
                                    need_relayout = true;
                                } else {
                                    // target not found (shouldn't happen); reap the pane
                                    panes.table.remove(&new);
                                }
                            }
                            Action::FocusLeft
                            | Action::FocusRight
                            | Action::FocusUp
                            | Action::FocusDown => {
                                use crate::center::Move;
                                let mv = match action {
                                    Action::FocusLeft => Move::Left,
                                    Action::FocusRight => Move::Right,
                                    Action::FocusUp => Move::Up,
                                    _ => Move::Down,
                                };
                                let layout = session.tabs[active].center.layout(chrome.center);
                                if let Some(n) = crate::center::neighbor(&layout, focused, mv) {
                                    session.tabs[active].focused_pane = n;
                                }
                            }
                            Action::NewWorkspace | Action::SwitchWorkspace => {
                                if let Ok(db) = superzej_core::db::Db::open() {
                                    if let Some(target) = palette
                                        .as_ref()
                                        .and_then(|p| p.selected_item())
                                        .map(|i| i.key.clone())
                                    {
                                        let repo_path = target
                                            .strip_prefix("repo:")
                                            .unwrap_or(&target)
                                            .to_string();
                                        if session.switch_to_workspace(&repo_path, &db).is_ok() {
                                            refresh_tab_model(&mut model, &session);
                                            need_relayout = true;
                                        }
                                    }
                                }
                            }
                            Action::NewWorktree => {
                                // Add a new tab using the current workspace's root.
                                // In a full implementation this would branch/pick,
                                // but for the spike we just open a local shell pane in a new tab.
                                let src = &session.tabs[active];
                                let n = session.tabs.len();
                                let tab = crate::session::Tab {
                                    name: format!("{} ·{}", src.name, n),
                                    kind: crate::session::TabKind::Worktree,
                                    worktree: src.worktree.clone(),
                                    center: crate::center::CenterTree::Leaf(0),
                                    focused_pane: 0,
                                };
                                session.add_tab(tab);
                                refresh_tab_model(&mut model, &session);
                                need_relayout = true;
                            }
                            Action::NewTab => {
                                // A fresh tab on the same worktree (an "extra" page).
                                let src = &session.tabs[active];
                                let n = session.tabs.len();
                                let tab = crate::session::Tab {
                                    name: format!("{} ·{}", src.name, n),
                                    kind: crate::session::TabKind::Extra,
                                    worktree: src.worktree.clone(),
                                    center: crate::center::CenterTree::Leaf(0),
                                    focused_pane: 0,
                                };
                                session.add_tab(tab);
                                refresh_tab_model(&mut model, &session);
                                need_relayout = true;
                            }
                            Action::CloseWorktree => {
                                // Close the active tab; reap its panes' processes.
                                for id in session.tabs[active].center.pane_ids() {
                                    panes.table.remove(&id);
                                }
                                session.close_active();
                                refresh_tab_model(&mut model, &session);
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
                            // New/switch worktree+workspace and tool floats: recognized
                            // and consumed; they land with the sandbox::enter_argv spawn
                            // + branch/repo picker wiring.
                            _ => {}
                        }
                        dirty = true;
                        continue;
                    }
                    crate::sequence::MatchResult::Pending => {
                        model.status = format!("{} mode   awaiting next key…", mode.as_str());
                        dirty = true;
                        continue;
                    }
                    crate::sequence::MatchResult::None => {}
                }
                if let Some(bytes) = key_bytes(&k.key, k.modifiers) {
                    if let Some(p) = panes.table.get_mut(&focused) {
                        p.write_input(&bytes)?;
                        keymap.reset();
                    }
                }
            }
            Ok(Some(InputEvent::Resized { rows: r, cols: c })) => {
                rows = r;
                cols = c;
                chrome = layout::compute(cols, rows, want_sidebar, want_panel);
                need_relayout = true;
                buf.resize(cols, rows);
                dirty = true;
            }
            Ok(Some(InputEvent::Paste(s))) => {
                if let Some(p) = panes.table.get_mut(&focused) {
                    p.write_input(s.as_bytes())?;
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
    use crate::session::{Session, Tab, TabKind};

    fn one_tab_session() -> Session {
        Session {
            id: "s1".into(),
            tabs: vec![Tab {
                name: "app/home".into(),
                kind: TabKind::Home,
                worktree: "/tmp/app".into(),
                center: CenterTree::Leaf(0),
                focused_pane: 0,
            }],
            active: 0,
        }
    }

    #[test]
    fn load_or_seed_session_recovers_tabs_from_db_when_present() {
        let state_home = std::env::temp_dir().join(format!("test_db_{}", std::process::id()));
        let db_path = state_home.join("superzej/superzej.db");
        std::fs::create_dir_all(db_path.parent().unwrap()).unwrap();

        let db = superzej_core::db::Db::open_at(&db_path).unwrap();
        let _ = db.put_workspace("/tmp/app", "app");
        let mk = |name: &str, wt: &str| superzej_core::models::TabLayoutRow {
            tab_name: name.into(),
            kind: "worktree".into(),
            worktree: wt.into(),
            pane_tree: r#"{"leaf":0}"#.into(),
            ordinal: 0,
            focused_pane: 0,
        };
        db.put_tab_layout("/tmp/app", &mk("app/feat", "/tmp/app-feat"))
            .unwrap();

        std::env::set_var("XDG_STATE_HOME", &state_home);

        let session = load_or_seed_session(std::path::Path::new("/tmp/app"));

        std::env::remove_var("XDG_STATE_HOME");

        assert_eq!(session.tabs.len(), 1);
        assert_eq!(session.tabs[0].name, "app/feat");
        assert_eq!(session.id, "/tmp/app");
    }

    #[test]
    fn hydration_worker_loads_real_workspaces_into_sidebar() {
        let state_home =
            std::env::temp_dir().join(format!("test_db_sidebar_{}_state", std::process::id()));
        let db_path = state_home.join("superzej/superzej.db");
        std::fs::create_dir_all(db_path.parent().unwrap()).unwrap();

        let db = superzej_core::db::Db::open_at(&db_path).unwrap();
        let _ = db.put_workspace("/tmp/repo1", "repo1");
        // Ensure some time passes so timestamps are distinctly different
        std::thread::sleep(std::time::Duration::from_millis(10));
        let _ = db.put_workspace("/tmp/repo2", "repo2");

        std::env::set_var("XDG_STATE_HOME", &state_home);

        let session = load_or_seed_session(std::path::Path::new("/tmp/repo1"));

        // Ensure Db::open inside build_model will use the state_home
        let model = build_model(&session);

        std::env::remove_var("XDG_STATE_HOME");

        assert!(model.sidebar.contains(&"repo1".to_string()));
        assert!(model.sidebar.contains(&"repo2".to_string()));
    }

    #[test]
    fn initial_model_is_cheap_and_marks_hydration_pending() {
        let session = one_tab_session();
        let model = build_initial_model(&session);
        assert_eq!(model.tabs, vec!["app/home".to_string()]);
        assert_eq!(model.active_tab, 0);
        assert_eq!(model.sidebar, vec!["hydrating…".to_string()]);
        assert!(model.panel.iter().any(|l| l.contains("hydrating")));
        assert!(model.status.contains("Starting szhost"));
    }
    #[test]
    fn action_new_worktree_adds_tab_and_focuses_it() {
        let mut session = one_tab_session();
        let mut model = build_initial_model(&session);
        let chrome = layout::compute(160, 40, true, true);
        let mut keymap =
            crate::keymap::default_keymap_with_config(&superzej_core::config::Config::default());

        // Simulating the Action block manually since the event loop is complex to instantiate
        let active = session.active;
        let src = session.tabs[active].clone();
        let n = session.tabs.len();
        let tab = crate::session::Tab {
            name: format!("{} ·{}", src.name, n),
            kind: crate::session::TabKind::Worktree,
            worktree: src.worktree.clone(),
            center: crate::center::CenterTree::Leaf(0),
            focused_pane: 0,
        };
        session.add_tab(tab);
        refresh_tab_model(&mut model, &session);

        assert_eq!(session.tabs.len(), 2);
        assert_eq!(session.active, 1);
        assert_eq!(session.tabs[1].name, "app/home ·1");
        assert_eq!(model.active_tab, 1);
        assert_eq!(model.tabs[1], "app/home ·1");
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
    fn shell_argv_honors_login_override() {
        assert_eq!(
            shell_argv_from("/bin/bash", true),
            vec!["/bin/bash".to_string(), "-l".to_string()]
        );
    }

    #[test]
    fn pty_drain_stops_at_chunk_budget() {
        let (tx, rx) = channel();
        tx.send(PaneEvent::Output(1, b"one".to_vec())).unwrap();
        tx.send(PaneEvent::Output(1, b"two".to_vec())).unwrap();
        tx.send(PaneEvent::Output(1, b"three".to_vec())).unwrap();

        let mut seen = Vec::new();
        let stats = drain_pty_events(
            &rx,
            PtyDrainBudget {
                max_chunks: 2,
                max_bytes: usize::MAX,
            },
            |ev| seen.push(ev.byte_len()),
        );
        assert_eq!(stats.chunks, 2);
        assert_eq!(seen, vec![3, 3]);
        assert!(stats.budget_exhausted);
        assert!(matches!(rx.try_recv(), Ok(PaneEvent::Output(_, b)) if b == b"three"));
    }

    #[test]
    fn pty_drain_reports_disconnected_after_queue_drains() {
        let (tx, rx) = channel();
        tx.send(PaneEvent::Output(1, b"one".to_vec())).unwrap();
        drop(tx);

        let stats = drain_pty_events(&rx, PtyDrainBudget::default(), |_| {});
        assert_eq!(stats.chunks, 1);
        assert!(stats.disconnected);
    }

    #[test]
    fn tab_switch_refreshes_model_without_changing_chrome_layout() {
        let mut session = one_tab_session();
        session.add_tab(Tab {
            name: "app/feat".into(),
            kind: TabKind::Worktree,
            worktree: "/tmp/app-feat".into(),
            center: CenterTree::Leaf(0),
            focused_pane: 0,
        });
        let mut model = build_initial_model(&session);
        let chrome = layout::compute(160, 40, true, true);
        let before = chrome.clone();

        session.switch_to(1);
        refresh_tab_model(&mut model, &session);

        assert_eq!(model.active_tab, 1);
        assert_eq!(
            model.tabs,
            vec!["app/home".to_string(), "app/feat".to_string()]
        );
        assert_eq!(
            chrome, before,
            "tab switches must reuse the chrome snapshot"
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
        let mut tab = Tab {
            name: "app/feat".into(),
            kind: TabKind::Worktree,
            worktree: "/tmp/app-feat".into(),
            center: CenterTree::Split {
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
            },
            focused_pane: 4,
        };

        assert!(remap_warmed_tab_ids(&mut tab, 4, &[(3, 20), (4, 21)]));

        assert_eq!(tab.center.pane_ids(), vec![20, 21]);
        assert_eq!(tab.focused_pane, 21);
    }

    #[test]
    fn warmed_tab_remap_rejects_stale_tree() {
        let mut tab = Tab {
            name: "app/feat".into(),
            kind: TabKind::Worktree,
            worktree: "/tmp/app-feat".into(),
            center: CenterTree::Leaf(99),
            focused_pane: 99,
        };
        let before = tab.clone();

        assert!(!remap_warmed_tab_ids(&mut tab, 4, &[(3, 20), (4, 21)]));
        assert_eq!(tab, before);
    }
}
