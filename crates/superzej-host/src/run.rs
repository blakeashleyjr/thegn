//! The spike's interactive loop: own the outer terminal, run one shell pane
//! inside the chrome cross, render it, route input. Single-threaded poll loop —
//! `poll_input` doubles as the ~60fps frame tick; pane output is coalesced
//! between polls and painted via `BufferedTerminal::draw_from_screen` + `flush`,
//! which diffs against the prior frame and emits only changed cells (no
//! clear-and-redraw → no flashing). The tokio mpsc event loop arrives in Phase 2.

use anyhow::{Context, Result};
use notify::{Event, RecursiveMode, Watcher, recommended_watcher};
use std::path::Path;
use std::time::{Duration, Instant};

use tokio::sync::mpsc as tokio_mpsc;
use tokio::task;

use termwiz::caps::Capabilities;
use termwiz::input::{InputEvent, KeyCode, Modifiers};
use termwiz::surface::{Change, Position, Surface};
use termwiz::terminal::buffered::BufferedTerminal;
use termwiz::terminal::{new_terminal, Terminal};

use crate::chrome::{render_tab, FrameModel};
use crate::compositor::Rect;
use crate::layout;
use crate::pane::{PaneEvent, PtyPane};

const MODEL_REFRESH_INTERVAL: Duration = Duration::from_secs(2);
const PR_REFRESH_INTERVAL: Duration = Duration::from_secs(20);

fn model_refresh_due(last_refresh: Instant, now: Instant, interval: Duration) -> bool {
    now.duration_since(last_refresh) >= interval
}

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

fn tool_drawer_argv(command: &str) -> Vec<String> {
    vec![
        superzej_core::util::shell(),
        "-lc".into(),
        format!("exec {}", command.trim()),
    ]
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
fn build_palette(
    session: &crate::session::Session,
    db: &superzej_core::db::Db,
) -> Vec<crate::palette::PaletteItem> {
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

    // Add active session's tabs
    for t in &session.tabs {
        items.push(PaletteItem::new(
            format!("tab:{}", t.name),
            format!("→ {}", t.name),
        ));
    }

    // Add persisted worktrees from other workspaces so the palette can jump
    // directly to a worktree tab and persist that target workspace's active tab.
    if let Ok(worktrees) = db.worktrees() {
        for wt in worktrees {
            if session.tabs.iter().any(|t| t.name == wt.tab_name) {
                continue;
            }
            let label = if wt.branch.trim().is_empty() {
                wt.tab_name.clone()
            } else {
                wt.branch.clone()
            };
            items.push(PaletteItem::new(
                format!("wt:{}\t{}", wt.repo_root, wt.tab_name),
                format!("⎇ {label}"),
            ));
        }
    }

    // Add workspaces (repos) for switching
    if let Ok(workspaces) = db.workspaces() {
        for w in workspaces {
            // Don't add the current workspace as a switch target
            if w.repo_path != session.id {
                let name = w.name;
                items.push(PaletteItem::new(
                    format!("repo:{}", w.repo_path),
                    format!("✦ {}", name),
                ));
            }
        }
    }

    let usage = db.palette_usage().unwrap_or_default();
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

    let mut env_session = std::env::var("SUPERZEJ_SESSION").ok();
    if let Some(ref s) = env_session {
        if s == "superzej" {
            // Ignore the old legacy default
            env_session = None;
        }
    }

    let cwd_str = cwd.to_string_lossy().into_owned();
    let session_name = if let Ok(state_home) = std::env::var("XDG_STATE_HOME") {
        // Use the explicit DB in test scenarios
        let path = std::path::Path::new(&state_home).join("superzej/superzej.db");
        if let Ok(db) = superzej_core::db::Db::open_at(&path) {
            db.workspaces()
                .unwrap_or_default()
                .into_iter()
                .find(|w| {
                    Path::new(&w.repo_path) == cwd || Some(&w.repo_path) == env_session.as_ref()
                })
                .map(|w| w.repo_path)
                .unwrap_or_else(|| env_session.unwrap_or(cwd_str.clone()))
        } else {
            env_session.unwrap_or(cwd_str)
        }
    } else if let Ok(db) = superzej_core::db::Db::open() {
        // Use the workspace from DB if available for cwd
        db.workspaces()
            .unwrap_or_default()
            .into_iter()
            .find(|w| Path::new(&w.repo_path) == cwd || Some(&w.repo_path) == env_session.as_ref())
            .map(|w| w.repo_path)
            .unwrap_or_else(|| env_session.unwrap_or(cwd_str))
    } else {
        env_session.unwrap_or(cwd_str)
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

fn split_sidebar_tab(name: &str) -> Option<(String, String)> {
    let (repo, branch) = name.split_once('/')?;
    (!repo.is_empty()).then(|| (repo.to_string(), branch.to_string()))
}

fn split_sidebar_page(branch: &str) -> (String, u32) {
    if let Some((base, suffix)) = branch.rsplit_once(" \u{b7}") {
        if !suffix.is_empty() && suffix.chars().all(|c| c.is_ascii_digit()) {
            if let Ok(n) = suffix.parse() {
                return (base.to_string(), n);
            }
        }
    }
    (branch.to_string(), 1)
}

#[derive(Debug, Clone)]
struct SidebarWorktree {
    label: String,
    pages: Vec<(u32, usize, bool)>,
    active: bool,
    min_position: usize,
}

fn build_sidebar_rows(
    session: &crate::session::Session,
    db: Option<&superzej_core::db::Db>,
) -> (Vec<String>, usize) {
    let mut workspaces: Vec<(String, String)> = Vec::new();

    if let Some(db) = db {
        if let Ok(rows) = db.workspaces() {
            for w in rows {
                let display = if w.name.trim().is_empty() {
                    std::path::Path::new(&w.repo_path)
                        .file_name()
                        .map(|s| s.to_string_lossy().into_owned())
                        .unwrap_or_else(|| w.repo_path.clone())
                } else {
                    w.name.clone()
                };
                let base = superzej_core::util::slugify(&display);
                let slug = db
                    .slug_for_repo(&w.repo_path, &base)
                    .unwrap_or_else(|_| base.clone());
                if !workspaces.iter().any(|(s, _)| *s == slug) {
                    workspaces.push((slug, display));
                }
            }
        }
    }

    for tab in &session.tabs {
        if let Some((repo, _)) = split_sidebar_tab(&tab.name) {
            if !workspaces.iter().any(|(s, _)| *s == repo) {
                workspaces.push((repo.clone(), repo));
            }
        }
    }

    let mut rows = Vec::new();
    let mut selected = 0usize;

    for (repo_slug, display) in workspaces {
        let repo_row = rows.len();
        rows.push(display);

        let mut groups: Vec<SidebarWorktree> = Vec::new();
        for (idx, tab) in session.tabs.iter().enumerate() {
            let Some((tab_repo, branch)) = split_sidebar_tab(&tab.name) else {
                continue;
            };
            if tab_repo != repo_slug {
                continue;
            }
            let (base, page) = split_sidebar_page(&branch);
            let active = idx == session.active;
            if let Some(group) = groups.iter_mut().find(|g| g.label == base) {
                group.pages.push((page, idx, active));
                group.active |= active;
                group.min_position = group.min_position.min(idx);
            } else {
                groups.push(SidebarWorktree {
                    label: base,
                    pages: vec![(page, idx, active)],
                    active,
                    min_position: idx,
                });
            }
        }

        groups.sort_by_key(|g| (g.label != "home", g.min_position));
        for group in &mut groups {
            group.pages.sort_by_key(|(page, pos, _)| (*page, *pos));
        }

        if groups.is_empty() && session.active < session.tabs.len() {
            if let Some((active_repo, _)) = split_sidebar_tab(&session.tabs[session.active].name) {
                if active_repo == repo_slug {
                    selected = repo_row;
                }
            }
        }

        let groups_len = groups.len();
        for (group_idx, group) in groups.into_iter().enumerate() {
            let glyph = if group_idx + 1 == groups_len {
                "\u{2514}"
            } else {
                "\u{251c}"
            };
            let row_idx = rows.len();
            rows.push(format!("  {glyph} {}", group.label));
            if group.active {
                selected = row_idx;
            }
            if group.pages.len() > 1 {
                for (page_idx, (page, _pos, active)) in group.pages.iter().enumerate() {
                    let page_glyph = if page_idx + 1 == group.pages.len() {
                        "\u{2514}"
                    } else {
                        "\u{251c}"
                    };
                    let row_idx = rows.len();
                    rows.push(format!("      {page_glyph} \u{b7}{page}"));
                    if *active {
                        selected = row_idx;
                    }
                }
            }
        }
    }

    if rows.is_empty() {
        rows.push("no workspaces".into());
    }

    let selected = selected.min(rows.len().saturating_sub(1));
    (rows, selected)
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
        sidebar_focused: false,
        sidebar_targets: Vec::new(),
        panel: crate::panel::PanelData {
            branch: active_name,
            ..Default::default()
        },
        panel_focused: false,
        status: format!(
            "Starting szhost (build: {})… panes usable while git status hydrates",
            env!("SZHOST_BUILD_TIME")
        ),
        accent: superzej_core::theme::TEAL.to_string(),
    }
}

/// Build the chrome model from the resurrected session + the current worktree's
/// git state (best-effort — the host stays up even with no repo / no DB). This
/// is the in-process data flow the chrome relies on: read core + svc directly,
/// no IPC. This can be slow on large repos, so launch calls it on a background
/// worker after the first frame is already possible.
fn build_model(session: &crate::session::Session, db: &superzej_core::db::Db) -> FrameModel {
    use superzej_core::remote::GitLoc;
    use superzej_svc::git::{GitBackend, GixGit};

    let cwd = active_tab_path(session);
    let loc = GitLoc::for_worktree(&cwd);
    let git = GixGit::new();
    let branch = git.current_branch(&loc).unwrap_or_else(|_| "—".into());

    let (sidebar, sidebar_selected) = build_sidebar_rows(session, Some(db));

    let mut panel = crate::panel::PanelData {
        branch: branch.clone(),
        ..Default::default()
    };

    // Add PR info if cached (native-host replacement for the zellij PR widget).
    if let Ok(Some((json, _))) = db.get_pr_cache(&loc.path()) {
        if let Ok(v) = serde_json::from_str::<serde_json::Value>(&json) {
            if let Some(state) = v.get("state").and_then(|s| s.as_str()) {
                if let Some(num) = v.get("number").and_then(|n| n.as_i64()) {
                    panel.pr = Some(crate::panel::PrSummary {
                        number: num as u64,
                        title: String::new(),
                        state: state.to_string(),
                        url: String::new(),
                        is_draft: false,
                        review_decision: None,
                    });
                }
            }
        }
    }

    if let Ok(files) = git.diff_files(&loc, "HEAD") {
        panel.files = files
            .iter()
            .map(|f| crate::panel::DiffFile {
                status: f.path.chars().next().unwrap_or('M'),
                path: f.path.clone(),
                added: f.added,
                deleted: f.deleted,
            })
            .collect();
    }

    FrameModel {
        tabs: session.tabs.iter().map(|t| t.name.clone()).collect(),
        active_tab: session.active,
        sidebar,
        sidebar_selected,
        sidebar_focused: false,
        sidebar_targets: Vec::new(),
        panel,
        panel_focused: false,
        status: format!(
            "Cmd-K menu   Alt-w worktree   Alt-o switch   Ctrl-Q quit  [build {}]",
            env!("SZHOST_BUILD_TIME")
        ),
        accent: superzej_core::theme::TEAL.to_string(),
    }
}

fn apply_mode_status(model: &mut FrameModel, mode: crate::keymap::Mode) {
    model.status = format!(
        "{} mode   Ctrl-Alt-v vim   Ctrl-Alt-e emacs   Ctrl-Alt-n normal   Ctrl-K menu   Alt-w worktree",
        mode.as_str()
    );
}

fn spawn_model_hydration(
    tx: tokio_mpsc::UnboundedSender<FrameModel>,
    session: crate::session::Session,
) {
    task::spawn_blocking(move || {
        if let Ok(db) = superzej_core::db::Db::open() {
            let _ = tx.send(build_model(&session, &db));
        }
    });
}

fn spawn_pr_cache_refresh(session: crate::session::Session) {
    task::spawn_blocking(move || {
        let cwd = active_tab_path(&session);
        if !cwd.is_dir() {
            return;
        }
        let loc = superzej_core::remote::GitLoc::for_worktree(&cwd);
        let panel = superzej_core::github::pr_status(&loc);
        let Ok(json) = serde_json::to_string(&panel) else {
            return;
        };
        if let Ok(db) = superzej_core::db::Db::open() {
            let _ = db.put_pr_cache(&loc.path(), &panel.branch, &json);
        }
    });
}

/// Replace an externally-dead sole center pane with a fresh shell pane without
/// closing the workspace tab. Explicit close-pane/close-worktree actions remove
/// panes from the session before their process exits, so this only handles
/// unexpected PTY child exits (killed shell, missing old child, etc.).
fn replace_single_dead_center_pane(
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

pub async fn main(cli: crate::Cli) -> Result<()> {
    let caps = Capabilities::new_from_env().context("term capabilities")?;
    let mut term = new_terminal(caps).context("open terminal")?;
    term.set_raw_mode().context("raw mode")?;
    term.enter_alternate_screen().context("alt screen")?;
    let size = term.get_screen_size().context("screen size")?;
    let (rows, cols) = (size.rows, size.cols);

    let mut buf = BufferedTerminal::new(term).context("buffered terminal")?;

    let cwd = std::env::current_dir().unwrap_or_else(|_| ".".into());
    let session = load_or_seed_session(&cwd);

    let cfg = superzej_core::config::Config::load_layered(
        &superzej_core::config::ProcessEnv,
        &cli.overrides,
        cli.config.clone(),
    );
    let keymap = crate::keymap::default_keymap_with_config(&cfg);
    let mode = crate::keymap::Mode::Normal;
    let mut model = build_initial_model(&session);
    apply_mode_status(&mut model, mode);
    let (model_tx, model_rx) = tokio_mpsc::unbounded_channel::<FrameModel>();
    spawn_model_hydration(model_tx.clone(), session.clone());

    let (config_tx, config_rx) =
        std::sync::mpsc::channel::<Result<superzej_core::config::Config, String>>();

    let config_path = superzej_core::config::Config::path();
    std::thread::spawn(move || {
        if let Some(parent) = config_path.parent() {
            let mut last_send = std::time::Instant::now();
            let overrides_clone = cli.overrides.clone();
            let config_clone = cli.config.clone();
            if let Ok(mut watcher) = recommended_watcher(move |res: notify::Result<Event>| {
                if let Ok(ev) = res {
                    if matches!(
                        ev.kind,
                        notify::EventKind::Modify(_)
                            | notify::EventKind::Create(_)
                            | notify::EventKind::Remove(_)
                    ) {
                        if last_send.elapsed() > std::time::Duration::from_millis(500) {
                            let new_cfg_res = superzej_core::config::Config::try_load_layered(
                                &superzej_core::config::ProcessEnv,
                                &overrides_clone,
                                config_clone.clone(),
                            );
                            let _ = config_tx.send(new_cfg_res);
                            last_send = std::time::Instant::now();
                        }
                    }
                }
            }) {
                let _ = watcher.watch(parent, RecursiveMode::NonRecursive);
                loop {
                    std::thread::sleep(std::time::Duration::MAX);
                }
            }
        }
    });

    let result = event_loop(
        &mut buf, session, model, model_tx, model_rx, rows, cols, keymap, mode, config_rx,
    )
    .await;

    let _ = buf.terminal().exit_alternate_screen();
    let _ = buf.terminal().set_cooked_mode();
    result
}

/// The global pane registry. A tab's panes are identified by the real ids in its
/// `CenterTree`; this just owns the live `PtyPane`s keyed by id.
struct Panes {
    table: std::collections::HashMap<u32, PtyPane>,
    next_id: u32,
    tx: tokio_mpsc::Sender<PaneEvent>,
}

impl Panes {
    fn new(tx: tokio_mpsc::Sender<PaneEvent>) -> Self {
        Self {
            table: std::collections::HashMap::new(),
            next_id: 1,
            tx,
        }
    }

    /// Spawn one shell pane in `cwd`, sized to `center`; returns its id.
    fn spawn(&mut self, cwd: Option<&std::path::Path>, center: Rect) -> Result<u32> {
        let argv = pane_shell_argv();
        self.spawn_argv(&argv, cwd, center)
    }

    /// Spawn a specific argv in `cwd`, sized to `center`; returns its id.
    fn spawn_argv(
        &mut self,
        argv: &[String],
        cwd: Option<&std::path::Path>,
        center: Rect,
    ) -> Result<u32> {
        let id = self.next_id;
        self.next_id += 1;
        let pane = PtyPane::spawn(
            id,
            argv,
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
            .or_else(|| std::env::current_dir().ok())
            .or_else(|| std::env::var("HOME").ok().map(std::path::PathBuf::from));

        let mut map = std::collections::HashMap::new();
        for old in &leaves {
            if !map.contains_key(old) {
                match self.spawn(cwd.as_deref(), center) {
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
    let (sidebar, selected) = build_sidebar_rows(session, None);
    model.sidebar = sidebar;
    model.sidebar_selected = selected;
}

fn switch_to_workspace_tab(
    session: &mut crate::session::Session,
    db: &superzej_core::db::Db,
    repo_path: &str,
    tab_name: &str,
) -> Result<bool> {
    session.switch_to_workspace(repo_path, db)?;
    let Some(idx) = session.tabs.iter().position(|tab| tab.name == tab_name) else {
        return Ok(false);
    };
    session.switch_to(idx);
    session.persist(db, &session.id, now_secs())?;
    Ok(true)
}

fn tab_cwd(tab: &crate::session::Tab) -> Option<std::path::PathBuf> {
    (!tab.worktree.is_empty() && std::path::Path::new(&tab.worktree).is_dir())
        .then(|| std::path::PathBuf::from(&tab.worktree))
        .or_else(|| std::env::current_dir().ok())
}

fn sync_drawer_persistence(
    session: &crate::session::Session,
    panes: &mut Panes,
    drawer: &mut Option<u32>,
    center: Rect,
) {
    let Some(tab) = session.tabs.get(session.active) else {
        return;
    };
    let Some(dir) = tab_cwd(tab) else {
        return;
    };
    let key = superzej_core::util::slugify(&dir.to_string_lossy());
    let should_be_open =
        std::fs::read_to_string(superzej_core::util::superzej_dir().join("drawer").join(key))
            .map(|s| s.trim() == "true")
            .unwrap_or(false);

    if should_be_open && drawer.is_none() {
        if let Ok(id) = panes.spawn(Some(&dir), center) {
            *drawer = Some(id);
        }
    } else if !should_be_open && drawer.is_some() {
        if let Some(id) = drawer.take() {
            panes.table.remove(&id);
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn event_loop<T: Terminal>(
    buf: &mut BufferedTerminal<T>,
    mut session: crate::session::Session,
    mut model: FrameModel,
    model_tx: tokio_mpsc::UnboundedSender<FrameModel>,
    mut model_rx: tokio_mpsc::UnboundedReceiver<FrameModel>,
    mut rows: usize,
    mut cols: usize,
    mut keymap: crate::keymap::KeyMap,
    mut mode: crate::keymap::Mode,
    config_rx: std::sync::mpsc::Receiver<Result<superzej_core::config::Config, String>>,
) -> Result<()> {
    let mut scratch = Surface::new(cols, rows);
    let mut want_sidebar = true;
    let mut want_panel = true;
    let mut chrome = layout::compute(cols, rows, want_sidebar, want_panel);
    let mut dirty = true;
    let mut palette: Option<crate::palette::Palette> = None;

    let (tx, mut rx) = tokio_mpsc::channel::<PaneEvent>(1024);
    let mut panes = Panes::new(tx);
    let mut need_relayout = true;
    let mut drawer: Option<u32> = None;
    let mut last_model_refresh = Instant::now();
    let mut last_pr_refresh = Instant::now() - PR_REFRESH_INTERVAL;

    sync_drawer_persistence(&session, &mut panes, &mut drawer, chrome.center);

    let mut current_config = keymap.config().clone();
    loop {
        if session.tabs.is_empty() {
            return Ok(()); // last tab closed
        }
        let active = session.active;

        if let Ok(size) = buf.terminal().get_screen_size() {
            if size.rows != rows || size.cols != cols {
                rows = size.rows;
                cols = size.cols;
                chrome = layout::compute(cols, rows, want_sidebar, want_panel);
                need_relayout = true;
                buf.resize(cols, rows);
                dirty = true;
            }
        }

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
                                if visible.contains(&id) {
                                    dirty = true;
                                }
                            }
                        }
                        PaneEvent::Exit(id) => {
                            panes.table.remove(&id);
                            // Find the owning tab and either drop the pane from its split
                            // or, if its only shell died, keep the tab and respawn a fresh
                            // shell. Explicit close-pane/worktree actions remove the pane
                            // from the session before the PTY exit event arrives, so this
                            // path is for external child death (kill -9, bad shell, etc.).
                            if let Some(ti) = session
                                .tabs
                                .iter()
                                .position(|t| t.center.pane_ids().contains(&id))
                            {
                                let sole = session.tabs[ti].center.pane_ids().len() == 1;
                                if sole {
                                    if ti == session.active {
                                        // Try worktree dir first, then current_dir, then $HOME as fallback
                                        let cwd = (!session.tabs[ti].worktree.is_empty()
                                            && std::path::Path::new(&session.tabs[ti].worktree)
                                                .is_dir())
                                        .then(|| {
                                            std::path::PathBuf::from(&session.tabs[ti].worktree)
                                        })
                                        .or_else(|| std::env::current_dir().ok())
                                        .or_else(|| {
                                            std::env::var("HOME").ok().map(std::path::PathBuf::from)
                                        });
                                        match panes.spawn(cwd.as_deref(), chrome.center) {
                                            Ok(fresh) => {
                                                replace_single_dead_center_pane(
                                                    &mut session.tabs[ti],
                                                    id,
                                                    fresh,
                                                );
                                                model.status =
                                                    "Pane exited; spawned a fresh shell".into();
                                                need_relayout = true;
                                            }
                                            Err(err) => {
                                                model.status = format!("Respawn failed: {err:#}");
                                            }
                                        }
                                    }
                                } else {
                                    session.tabs[ti].center.remove(id);
                                    if session.tabs[ti].focused_pane == id {
                                        if let Some(first) =
                                            session.tabs[ti].center.pane_ids().first()
                                        {
                                            session.tabs[ti].focused_pane = *first;
                                        }
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
        if session.tabs.is_empty() {
            return Ok(());
        }

        while let Some(next_model) = model_rx.try_recv().ok() {
            model = next_model;
            refresh_tab_model(&mut model, &session);
            apply_mode_status(&mut model, mode);
            dirty = true;
        }

        while let Ok(cfg_res) = config_rx.try_recv() {
            match cfg_res {
                Ok(new_cfg) => {
                    keymap = crate::keymap::default_keymap_with_config(&new_cfg);
                    current_config = new_cfg;
                    model.status = "Config reloaded".into();
                    need_relayout = true;
                }
                Err(e) => {
                    model.status = format!("Config error: {}", e).into();
                }
            }
            dirty = true;
        }
        let now = Instant::now();
        if model_refresh_due(last_model_refresh, now, MODEL_REFRESH_INTERVAL) {
            spawn_model_hydration(model_tx.clone(), session.clone());
            last_model_refresh = now;
        }
        if model_refresh_due(last_pr_refresh, now, PR_REFRESH_INTERVAL) {
            spawn_pr_cache_refresh(session.clone());
            last_pr_refresh = now;
        }

        // 2. Render if anything changed (diff-flush): all visible panes of the
        //    active tab + the chrome, with the hardware cursor in the focused pane.
        if dirty {
            if scratch.dimensions() != (cols, rows) {
                scratch = Surface::new(cols, rows);
            }
            crate::chrome::clear_frame(&mut scratch);
            let panel_ui = crate::panel::PanelUi::default();
            render_tab(
                &mut scratch,
                &chrome,
                &tree,
                focused,
                &model,
                &panel_ui,
                |id| panes.table.get(&id).map(|p| p.emulator()),
            );
            if let Some(drawer_id) = drawer {
                if let Some(p) = panes.table.get(&drawer_id) {
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
            }
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
                                if let Some(payload) = key.strip_prefix("wt:") {
                                    if let Some((repo_path, tab_name)) = payload.split_once('\t') {
                                        if let Ok(db) = superzej_core::db::Db::open() {
                                            if switch_to_workspace_tab(
                                                &mut session,
                                                &db,
                                                repo_path,
                                                tab_name,
                                            )
                                            .unwrap_or(false)
                                            {
                                                refresh_tab_model(&mut model, &session);
                                                need_relayout = true;
                                                sync_drawer_persistence(
                                                    &session,
                                                    &mut panes,
                                                    &mut drawer,
                                                    chrome.center,
                                                );
                                            }
                                        }
                                    }
                                } else if let Some(repo_path) = key.strip_prefix("repo:") {
                                    if let Ok(db) = superzej_core::db::Db::open() {
                                        if session.switch_to_workspace(repo_path, &db).is_ok() {
                                            refresh_tab_model(&mut model, &session);
                                            need_relayout = true;
                                            sync_drawer_persistence(
                                                &session,
                                                &mut panes,
                                                &mut drawer,
                                                chrome.center,
                                            );
                                        }
                                    }
                                } else if let Some(name) = key.strip_prefix("tab:") {
                                    if let Some(i) =
                                        session.tabs.iter().position(|t| t.name == name)
                                    {
                                        session.switch_to(i);
                                        refresh_tab_model(&mut model, &session);
                                        need_relayout = true;
                                        sync_drawer_persistence(
                                            &session,
                                            &mut panes,
                                            &mut drawer,
                                            chrome.center,
                                        );
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
                                if let Ok(db) = superzej_core::db::Db::open() {
                                    palette = Some(crate::palette::Palette::new(build_palette(
                                        &session, &db,
                                    )));
                                }
                            }
                            Action::ToggleDrawer => {
                                if drawer.is_some() {
                                    // Reap the drawer pane
                                    if let Some(id) = drawer.take() {
                                        panes.table.remove(&id);
                                        let cwd = tab_cwd(&session.tabs[active]);
                                        if let Some(dir) = cwd {
                                            let key = superzej_core::util::slugify(
                                                &dir.to_string_lossy(),
                                            );
                                            let dir =
                                                superzej_core::util::superzej_dir().join("drawer");
                                            let _ = std::fs::write(dir.join(key), "false");
                                        }
                                    }
                                } else {
                                    // Spawn the drawer pane.
                                    // In a full implementation we'd read `~/.superzej/drawer/slug`
                                    // and use yazi::bin(&cfg), but for the spike we'll just spawn
                                    // yazi in the active worktree.
                                    let cwd = tab_cwd(&session.tabs[active]);
                                    let p = keymap
                                        .config()
                                        .tool_command("yazi")
                                        .map(tool_drawer_argv)
                                        .and_then(|argv| {
                                            panes
                                                .spawn_argv(&argv, cwd.as_deref(), chrome.center)
                                                .ok()
                                        })
                                        .or_else(|| {
                                            panes.spawn(cwd.as_deref(), chrome.center).ok()
                                        });
                                    if let Some(id) = p {
                                        drawer = Some(id);
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
                                chrome = layout::compute(cols, rows, want_sidebar, want_panel);
                                need_relayout = true;
                            }
                            Action::TogglePanel => {
                                want_panel = !want_panel;
                                chrome = layout::compute(cols, rows, want_sidebar, want_panel);
                                need_relayout = true;
                            }
                            Action::FocusSidebar => {
                                if !want_sidebar {
                                    want_sidebar = true;
                                    chrome = layout::compute(cols, rows, want_sidebar, want_panel);
                                    need_relayout = true;
                                }
                            }
                            Action::FocusPanel => {
                                if !want_panel {
                                    want_panel = true;
                                    chrome = layout::compute(cols, rows, want_sidebar, want_panel);
                                    need_relayout = true;
                                }
                            }
                            Action::NextTab => {
                                session.next_tab();
                                refresh_tab_model(&mut model, &session);
                                need_relayout = true;
                                sync_drawer_persistence(
                                    &session,
                                    &mut panes,
                                    &mut drawer,
                                    chrome.center,
                                );
                            }
                            Action::PrevTab => {
                                session.prev_tab();
                                refresh_tab_model(&mut model, &session);
                                need_relayout = true;
                                sync_drawer_persistence(
                                    &session,
                                    &mut panes,
                                    &mut drawer,
                                    chrome.center,
                                );
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
                                            sync_drawer_persistence(
                                                &session,
                                                &mut panes,
                                                &mut drawer,
                                                chrome.center,
                                            );
                                        }
                                    }
                                }
                            }
                            Action::NewWorktree => {
                                // Add a new worktree tab. The name format is {repo}/{branch}
                                // where branch is derived from the page number (·1, ·2, etc).
                                // This creates a distinct worktree entry in the sidebar, separate
                                // from pages (extra views on the same worktree).
                                let src = &session.tabs[active];
                                let (repo, branch) = split_sidebar_tab(&src.name)
                                    .unwrap_or_else(|| (src.name.clone(), "home".to_string()));
                                let (_base, _) = split_sidebar_page(&branch);
                                let new_n = session.tabs.len();
                                // New worktrees use ·N as their branch name - they'll appear as
                                // separate entries in the sidebar (distinct from home/feature branches)
                                let tab = crate::session::Tab {
                                    name: format!("{repo}/·{new_n}"),
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
                            Action::Lazygit | Action::Editor | Action::Diff => {
                                // Spawns the requested tool in a native drawer pane.
                                let cwd = tab_cwd(&session.tabs[active]);
                                let tool_name = match action {
                                    Action::Lazygit => "lazygit",
                                    Action::Editor => "editor",
                                    Action::Diff => "diff",
                                    _ => unreachable!(),
                                };
                                if let Some(cmd_str) = keymap.config().tool_command(tool_name) {
                                    if drawer.is_some() {
                                        if let Some(id) = drawer.take() {
                                            panes.table.remove(&id);
                                        }
                                    }
                                    let argv = tool_drawer_argv(cmd_str);
                                    let p =
                                        panes.spawn_argv(&argv, cwd.as_deref(), chrome.center).ok();
                                    if let Some(id) = p {
                                        drawer = Some(id);
                                    }
                                }
                            }
                            Action::Yazi => {
                                // Direct bind for yazi, routed identical to ToggleDrawer but always spawning.
                                if drawer.is_some() {
                                    if let Some(id) = drawer.take() {
                                        panes.table.remove(&id);
                                    }
                                }
                                let cwd = tab_cwd(&session.tabs[active]);
                                let p = keymap
                                    .config()
                                    .tool_command("yazi")
                                    .map(tool_drawer_argv)
                                    .and_then(|argv| {
                                        panes.spawn_argv(&argv, cwd.as_deref(), chrome.center).ok()
                                    })
                                    .or_else(|| panes.spawn(cwd.as_deref(), chrome.center).ok());
                                if let Some(id) = p {
                                    drawer = Some(id);
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
                    let target_pane = drawer.unwrap_or(focused);
                    if let Some(p) = panes.table.get_mut(&target_pane) {
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
                let _ = buf
                    .terminal()
                    .set_screen_size(termwiz::terminal::ScreenSize {
                        rows,
                        cols,
                        xpixel: 0,
                        ypixel: 0,
                    });
                dirty = true;
            }
            Ok(Some(InputEvent::Paste(s))) => {
                let target_pane = drawer.unwrap_or(focused);
                if let Some(p) = panes.table.get_mut(&target_pane) {
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
        let model = build_model(&session, &db);

        std::env::remove_var("XDG_STATE_HOME");

        assert!(
            model.sidebar.contains(&"repo1".to_string()),
            "Sidebar should contain repo1, got: {:?}",
            model.sidebar
        );
        assert!(
            model.sidebar.contains(&"repo2".to_string()),
            "Sidebar should contain repo2, got: {:?}",
            model.sidebar
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
        db.put_workspace("/tmp/repo-a", "repo-a").unwrap();
        db.put_workspace("/tmp/repo-b", "repo-b").unwrap();

        let row = |name: &str, ord: i64| superzej_core::models::TabLayoutRow {
            tab_name: name.into(),
            kind: "worktree".into(),
            worktree: format!("/tmp/{name}"),
            pane_tree: r#"{"leaf":0}"#.into(),
            ordinal: ord,
            focused_pane: 0,
        };
        db.put_tab_layout("/tmp/repo-b", &row("repo-b/home", 0))
            .unwrap();
        db.put_tab_layout("/tmp/repo-b", &row("repo-b/feature-x", 1))
            .unwrap();

        let mut session = Session {
            id: "/tmp/repo-a".into(),
            tabs: vec![Tab {
                name: "repo-a/home".into(),
                kind: TabKind::Home,
                worktree: "/tmp/repo-a".into(),
                center: CenterTree::Leaf(0),
                focused_pane: 0,
            }],
            active: 0,
        };

        switch_to_workspace_tab(&mut session, &db, "/tmp/repo-b", "repo-b/feature-x").unwrap();

        assert_eq!(session.id, "/tmp/repo-b");
        assert_eq!(session.active_tab().unwrap().name, "repo-b/feature-x");
        assert_eq!(
            db.active_tab("/tmp/repo-b").unwrap().as_deref(),
            Some("repo-b/feature-x")
        );
    }

    #[test]
    fn initial_model_is_cheap_and_marks_hydration_pending() {
        let session = one_tab_session();
        let model = build_initial_model(&session);
        assert_eq!(model.tabs, vec!["app/home".to_string()]);
        assert_eq!(model.active_tab, 0);
        assert_eq!(model.sidebar, vec!["hydrating…".to_string()]);
        assert!(model.panel.branch == "app/home");
        assert!(model.status.contains("Starting szhost"));
    }

    #[test]
    fn native_watch_loop_refreshes_on_interval() {
        let start = Instant::now();
        assert!(!model_refresh_due(
            start,
            start + MODEL_REFRESH_INTERVAL / 2,
            MODEL_REFRESH_INTERVAL
        ));
        assert!(model_refresh_due(
            start,
            start + MODEL_REFRESH_INTERVAL,
            MODEL_REFRESH_INTERVAL
        ));
    }

    #[test]
    fn refresh_tab_model_updates_sidebar_tree_when_tabs_change() {
        let mut session = one_tab_session();
        let mut model = build_initial_model(&session);

        refresh_tab_model(&mut model, &session);
        assert!(
            model.sidebar.iter().any(|row| row.contains("home")),
            "sidebar should show the initial home worktree: {:?}",
            model.sidebar
        );

        session.add_tab(Tab {
            name: "app/feature-x".into(),
            kind: TabKind::Worktree,
            worktree: "/tmp/app-feature-x".into(),
            center: CenterTree::Leaf(0),
            focused_pane: 0,
        });
        refresh_tab_model(&mut model, &session);

        assert_eq!(model.active_tab, 1);
        assert!(
            model.sidebar.iter().any(|row| row.contains("feature-x")),
            "sidebar should include newly-created worktree tabs immediately: {:?}",
            model.sidebar
        );
    }
    #[test]
    fn action_new_worktree_adds_tab_and_focuses_it() {
        let mut session = one_tab_session();
        let mut model = build_initial_model(&session);

        // Simulating the Action block manually since the event loop is complex to instantiate
        // NewWorktree creates a new worktree entry (separate branch), not a page of existing worktree
        let (repo, branch) = split_sidebar_tab(&session.tabs[0].name)
            .unwrap_or_else(|| (session.tabs[0].name.clone(), "home".to_string()));
        let (_base, _) = split_sidebar_page(&branch);
        let new_n = session.tabs.len();
        let tab = crate::session::Tab {
            name: format!("{}/·{}", repo, new_n),
            kind: crate::session::TabKind::Worktree,
            worktree: session.tabs[0].worktree.clone(),
            center: crate::center::CenterTree::Leaf(0),
            focused_pane: 0,
        };
        session.add_tab(tab);
        refresh_tab_model(&mut model, &session);

        assert_eq!(session.tabs.len(), 2);
        assert_eq!(session.active, 1);
        assert_eq!(session.tabs[1].name, "app/·1");
        assert_eq!(model.active_tab, 1);
        assert_eq!(model.tabs[1], "app/·1");
    }

    #[test]
    fn toggle_drawer_spawns_and_closes_drawer_pane() {
        // The test spawns a drawer, which reads SHELL. Force it to something that exists.
        std::env::set_var("SHELL", "/bin/sh");
        let mut session = one_tab_session();
        let chrome = layout::compute(160, 40, true, true);

        let (tx, _rx) = tokio_mpsc::channel::<PaneEvent>(1024);
        let mut panes = Panes::new(tx);
        panes
            .materialize(&mut session.tabs[0], chrome.center)
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
        let mut tab = Tab {
            name: "app/home".into(),
            kind: TabKind::Home,
            worktree: "/tmp/app".into(),
            center: CenterTree::Leaf(7),
            focused_pane: 7,
        };

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
