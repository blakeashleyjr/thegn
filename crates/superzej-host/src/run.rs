//! The spike's interactive loop: own the outer terminal, run one shell pane
//! inside the chrome cross, render it, route input. Single-threaded poll loop —
//! `poll_input` doubles as the ~60fps frame tick; pane output is coalesced
//! between polls and painted via `BufferedTerminal::draw_from_screen` + `flush`,
//! which diffs against the prior frame and emits only changed cells (no
//! clear-and-redraw → no flashing). The tokio mpsc event loop arrives in Phase 2.

use anyhow::{Context, Result};
use std::sync::mpsc::{TryRecvError, channel};
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

fn now_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Resurrect the persisted tab list, seeding a single Home tab for the current
/// worktree if the session is empty (and persisting it so the next launch
/// restores it). The native host owns this — it's the resurrect path that
/// replaced zellij's session serialization.
fn load_or_seed_session(cwd: &std::path::Path, branch: &str) -> crate::session::Session {
    use crate::center::CenterTree;
    use crate::session::{Session, Tab, TabKind};

    let sess = superzej_core::db::session();
    let base = cwd
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "workspace".into());

    let Ok(db) = superzej_core::db::Db::open() else {
        // No DB — synthesize an ephemeral single-tab session.
        return Session {
            tabs: vec![Tab {
                name: format!("{base}/{branch}"),
                kind: TabKind::Home,
                worktree: cwd.to_string_lossy().into_owned(),
                center: CenterTree::Leaf(0),
                focused_pane: 0,
            }],
            active: 0,
        };
    };

    let mut session = Session::resurrect(&db, &sess).unwrap_or_default();
    if session.tabs.is_empty() {
        session.tabs.push(Tab {
            name: format!("{base}/{branch}"),
            kind: TabKind::Home,
            worktree: cwd.to_string_lossy().into_owned(),
            center: CenterTree::Leaf(0),
            focused_pane: 0,
        });
        session.active = 0;
        let _ = session.persist(&db, &sess, now_secs());
    }
    session
}

/// Build the chrome model from the resurrected session + the current worktree's
/// git state (best-effort — the host stays up even with no repo / no DB). This
/// is the in-process data flow the chrome relies on: read core + svc directly,
/// no IPC.
fn build_model(session: &crate::session::Session) -> FrameModel {
    use superzej_core::remote::GitLoc;
    use superzej_svc::git::{GitBackend, GixGit};

    let cwd = std::env::current_dir().unwrap_or_else(|_| ".".into());
    let loc = GitLoc::for_worktree(&cwd);
    let git = GixGit::new();
    let branch = git.current_branch(&loc).unwrap_or_else(|_| "—".into());

    // Sidebar: recent repos from the DB (best-effort).
    let sidebar = superzej_core::db::Db::open()
        .ok()
        .and_then(|db| db.recent_repos(20).ok())
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

pub fn main() -> Result<()> {
    let caps = Capabilities::new_from_env().context("term capabilities")?;
    let mut term = new_terminal(caps).context("open terminal")?;
    term.set_raw_mode().context("raw mode")?;
    term.enter_alternate_screen().context("alt screen")?;
    let size = term.get_screen_size().context("screen size")?;
    let (rows, cols) = (size.rows, size.cols);

    let mut buf = BufferedTerminal::new(term).context("buffered terminal")?;

    let cwd = std::env::current_dir().unwrap_or_else(|_| ".".into());
    let branch = {
        use superzej_svc::git::{GitBackend, GixGit};
        GixGit::new()
            .current_branch(&superzej_core::remote::GitLoc::for_worktree(&cwd))
            .unwrap_or_else(|_| "—".into())
    };
    let session = load_or_seed_session(&cwd, &branch);
    let model = build_model(&session);

    let result = event_loop(&mut buf, session, model, rows, cols);

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
        let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".into());
        let pane = PtyPane::spawn(
            id,
            &[shell, "-l".into()],
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

fn tab_cwd(tab: &crate::session::Tab) -> Option<std::path::PathBuf> {
    (!tab.worktree.is_empty() && std::path::Path::new(&tab.worktree).is_dir())
        .then(|| std::path::PathBuf::from(&tab.worktree))
        .or_else(|| std::env::current_dir().ok())
}

fn event_loop<T: Terminal>(
    buf: &mut BufferedTerminal<T>,
    mut session: crate::session::Session,
    mut model: FrameModel,
    mut rows: usize,
    mut cols: usize,
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
        loop {
            match rx.try_recv() {
                Ok(PaneEvent::Output(id, b)) => {
                    if let Some(p) = panes.table.get_mut(&id) {
                        p.feed(&b);
                        if visible.contains(&id) {
                            dirty = true;
                        }
                    }
                }
                Ok(PaneEvent::Exit(id)) => {
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
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => return Ok(()),
            }
        }
        if session.tabs.is_empty() {
            return Ok(());
        }

        // 2. Render if anything changed (diff-flush): all visible panes of the
        //    active tab + the chrome, with the hardware cursor in the focused pane.
        if dirty {
            if scratch.dimensions() != (cols, rows) {
                scratch = Surface::new(cols, rows);
            }
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
                // Global chords are intercepted by the keymap; everything else is
                // forwarded to the focused pane.
                if let Some(action) = crate::keymap::map_key(&k.key, k.modifiers) {
                    use crate::keymap::Action;
                    match action {
                        Action::Quit => return Ok(()),
                        Action::OpenPalette => {
                            palette = Some(crate::palette::Palette::new(build_palette(&session)));
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
                if let Some(bytes) = key_bytes(&k.key, k.modifiers) {
                    if let Some(p) = panes.table.get_mut(&focused) {
                        p.write_input(&bytes)?;
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
                }
            }
            Ok(_) | Err(_) => {}
        }
    }
}
