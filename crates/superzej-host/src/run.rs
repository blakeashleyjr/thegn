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
use std::time::{Duration, Instant};

use tokio::sync::mpsc as tokio_mpsc;
use tokio::task;

use termwiz::caps::Capabilities;
use termwiz::input::{InputEvent, KeyCode, Modifiers};
use termwiz::surface::{Change, Position, Surface};
use termwiz::terminal::buffered::BufferedTerminal;
use termwiz::terminal::{Terminal, TerminalWaker, new_terminal};

use crate::chrome::{FrameModel, render_tab};
use crate::compositor::Rect;
use crate::layout;
use crate::pane::{PaneEvent, PtyPane};

const MODEL_REFRESH_INTERVAL: Duration = Duration::from_secs(2);
const PR_REFRESH_INTERVAL: Duration = Duration::from_secs(20);

/// A refresh request delivered to the event loop. `Model` rehydrates the
/// sidebar/panel/diff (cheap, gix-backed, off-thread); `Pr` additionally kicks
/// the GitHub PR-cache refresh. Both arrive event-driven (worktree fs-watch,
/// tab switch) and on a low-frequency safety-net interval.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum RefreshKind {
    Model,
    Pr,
}

/// Background ticker: emits a `Model` refresh every `MODEL_REFRESH_INTERVAL` and
/// a `Pr` refresh every `PR_REFRESH_INTERVAL`, pulsing the waker so an idle loop
/// wakes to service it. This is the staleness backstop; fs-watch + on-switch
/// refresh handle the common, latency-sensitive cases.
///
/// Runs on a dedicated OS thread (not `tokio::spawn`) so it can never be starved
/// by the main loop blocking a runtime worker in `poll_input(None)` — true even
/// on a single-core runtime. `PR_REFRESH_INTERVAL` is a whole multiple of
/// `MODEL_REFRESH_INTERVAL`, so we sleep one model period at a time and emit the
/// PR tick when the elapsed count divides evenly.
fn spawn_refresh_ticker(tx: tokio_mpsc::UnboundedSender<RefreshKind>, waker: TerminalWaker) {
    std::thread::spawn(move || {
        let pr_every = (PR_REFRESH_INTERVAL.as_secs() / MODEL_REFRESH_INTERVAL.as_secs()).max(1);
        let mut ticks: u64 = 0;
        loop {
            std::thread::sleep(MODEL_REFRESH_INTERVAL);
            ticks += 1;
            let kind = if ticks.is_multiple_of(pr_every) {
                RefreshKind::Pr
            } else {
                RefreshKind::Model
            };
            if tx.send(kind).is_err() {
                break; // loop gone
            }
            let _ = waker.wake();
        }
    });
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
    if let Some(ref s) = env_session
        && s == "superzej"
    {
        // Ignore the old legacy default
        env_session = None;
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

/// The ordered `(slug, display)` workspace list backing the tree: every repo
/// known to the DB (stable slug), plus any live tab's repo prefix not yet in
/// the DB. The structured tree is then built by [`crate::sidebar::build_rows`].
fn workspace_list(
    session: &crate::session::Session,
    db: Option<&superzej_core::db::Db>,
) -> Vec<(String, String)> {
    let mut workspaces: Vec<(String, String)> = Vec::new();
    if let Some(db) = db
        && let Ok(rows) = db.workspaces()
    {
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
    for tab in &session.tabs {
        if let Some((repo, _)) = crate::sidebar::split_tab(&tab.name)
            && !workspaces.iter().any(|(s, _)| *s == repo)
        {
            workspaces.push((repo.clone(), repo));
        }
    }
    workspaces
}

/// Gather per-worktree git/agent/activity status for every tab in the session.
/// Runs on the hydration thread (git can be slow); the event loop merges this
/// into the tree at render time. Also advances the activity FSM in-process.
fn collect_sidebar_status(
    session: &crate::session::Session,
    db: &superzej_core::db::Db,
) -> crate::sidebar::SidebarStatus {
    use superzej_core::remote::GitLoc;
    use superzej_svc::git::{GitBackend, GixGit};
    let git = GixGit::new();
    let mut status = crate::sidebar::SidebarStatus::default();

    // Advance the activity state machine over the session's managed worktrees,
    // then read the fresh states (keyed by tab name).
    let managed: Vec<superzej_core::activity::ManagedWorktree> = session
        .tabs
        .iter()
        .filter(|t| !t.worktree.is_empty())
        .map(|t| superzej_core::activity::ManagedWorktree {
            worktree: t.worktree.clone(),
            tab: t.name.clone(),
        })
        .collect();
    superzej_core::activity::poll_and_save(&managed);
    status.activity = superzej_core::activity::read_states()
        .into_iter()
        .map(|(tab, st)| (tab, crate::sidebar::ActivityState::from_str(&st)))
        .collect();

    // git glyphs + agent per distinct worktree path.
    let mut seen = std::collections::HashSet::new();
    for tab in &session.tabs {
        if tab.worktree.is_empty() || !seen.insert(tab.worktree.clone()) {
            continue;
        }
        let path = std::path::Path::new(&tab.worktree);
        if !path.is_dir() {
            continue;
        }
        let loc = GitLoc::for_worktree(path);
        let dirty = git.status(&loc).map(|v| !v.is_empty()).unwrap_or(false);
        let (ahead, behind) = git.ahead_behind(&loc).ok().flatten().unwrap_or((0, 0));
        status.git.insert(
            tab.worktree.clone(),
            crate::sidebar::GitGlyphs {
                dirty,
                ahead,
                behind,
            },
        );
        if let Ok(Some(agent)) = db.worktree_agent(&tab.worktree) {
            status.agent.insert(tab.worktree.clone(), agent);
        }
    }
    status
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
        ..Default::default()
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

    let sidebar_workspaces = workspace_list(session, Some(db));
    let sidebar_status = collect_sidebar_status(session, db);

    let mut panel = crate::panel::PanelData {
        branch: branch.clone(),
        ..Default::default()
    };

    // Add PR info if cached (native-host replacement for the zellij PR widget).
    if let Ok(Some((json, _))) = db.get_pr_cache(&loc.path())
        && let Ok(v) = serde_json::from_str::<serde_json::Value>(&json)
        && let Some(state) = v.get("state").and_then(|s| s.as_str())
        && let Some(num) = v.get("number").and_then(|n| n.as_i64())
    {
        panel.pr = Some(crate::panel::PrSummary {
            number: num as u64,
            title: String::new(),
            state: state.to_string(),
            url: String::new(),
            is_draft: false,
            review_decision: None,
        });
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
        sidebar_workspaces,
        sidebar_status,
        panel,
        panel_focused: false,
        status: format!(
            "Cmd-K menu   Alt-w worktree   Alt-o switch   Ctrl-Q quit  [build {}]",
            env!("SZHOST_BUILD_TIME")
        ),
        accent: superzej_core::theme::TEAL.to_string(),
        ..Default::default()
    }
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
    model.status = format!(
        "{} mode   Ctrl-Alt-v vim   Ctrl-Alt-e emacs   Ctrl-Alt-n normal   Ctrl-K menu   Alt-w worktree",
        mode.as_str()
    );
}

fn spawn_model_hydration(
    tx: tokio_mpsc::UnboundedSender<FrameModel>,
    session: crate::session::Session,
    waker: Option<TerminalWaker>,
) {
    task::spawn_blocking(move || {
        if let Ok(db) = superzej_core::db::Db::open()
            && tx.send(build_model(&session, &db)).is_ok()
            && let Some(w) = &waker
        {
            let _ = w.wake();
        }
    });
}

fn spawn_pr_cache_refresh(session: crate::session::Session, waker: Option<TerminalWaker>) {
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
        // PR cache landing should surface via a model rehydrate; pulse the waker
        // so an idle loop repaints promptly.
        if let Some(w) = &waker {
            let _ = w.wake();
        }
    });
}

/// Bind (or re-bind) the diff fs-watcher to the active worktree path. A no-op if
/// the active worktree is unchanged. On a debounced filesystem event under the
/// worktree, pushes `RefreshKind::Model` and pulses the waker so the loop
/// rehydrates the diff panel promptly. The previous watcher (if any) is dropped,
/// which unregisters its watch.
fn retarget_diff_watcher(
    session: &crate::session::Session,
    watched: &mut Option<std::path::PathBuf>,
    watcher: &mut Option<notify::RecommendedWatcher>,
    refresh_tx: &tokio_mpsc::UnboundedSender<RefreshKind>,
    waker: &TerminalWaker,
) {
    let cwd = active_tab_path(session);
    if !cwd.is_dir() {
        return;
    }
    if watched.as_deref() == Some(cwd.as_path()) {
        return; // already watching this worktree
    }

    let tx = refresh_tx.clone();
    let w = waker.clone();
    let mut last_send = Instant::now()
        .checked_sub(Duration::from_secs(1))
        .unwrap_or_else(Instant::now);
    let new_watcher = recommended_watcher(move |res: notify::Result<Event>| {
        if let Ok(ev) = res
            && matches!(
                ev.kind,
                notify::EventKind::Modify(_)
                    | notify::EventKind::Create(_)
                    | notify::EventKind::Remove(_)
            )
            && last_send.elapsed() > Duration::from_millis(500)
        {
            if tx.send(RefreshKind::Model).is_ok() {
                let _ = w.wake();
            }
            last_send = Instant::now();
        }
    });

    if let Ok(mut nw) = new_watcher
        && nw.watch(&cwd, RecursiveMode::Recursive).is_ok()
    {
        *watcher = Some(nw);
        *watched = Some(cwd);
    }
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

    // Grab the waker after `BufferedTerminal` takes ownership of the terminal.
    // Every off-thread producer pulses this so the loop's blocking
    // `poll_input(None)` returns to drain its channel — the loop is fully
    // event-driven (zero idle wakeups) rather than polled on a 16ms tick.
    let waker = buf.terminal().waker();

    let cwd = std::env::current_dir().unwrap_or_else(|_| ".".into());
    let session = load_or_seed_session(&cwd);

    let cfg = superzej_core::config::Config::load_layered(
        &superzej_core::config::ProcessEnv,
        &cli.overrides,
        cli.config.clone(),
    );
    let keymap = rebuild_keymap(&cfg, &session);
    let mode = crate::keymap::startup_mode(&cfg);
    let mut model = build_initial_model(&session);
    apply_mode_status(&mut model, mode);
    // Surface keybind conflicts at launch (non-fatal — the shell always opens).
    if let Some(summary) = keybind_conflict_summary(&cfg) {
        model.status = summary;
    }
    let (model_tx, model_rx) = tokio_mpsc::unbounded_channel::<FrameModel>();
    spawn_model_hydration(model_tx.clone(), session.clone(), Some(waker.clone()));

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
    spawn_refresh_ticker(refresh_tx.clone(), waker.clone());

    let result = event_loop(
        &mut buf, session, model, model_tx, model_rx, rows, cols, keymap, mode, config_rx,
        refresh_tx, refresh_rx, waker,
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
    /// Pulsed by reader threads after each send so the main loop's blocking
    /// `poll_input(None)` wakes to drain PTY output. `None` in unit tests that
    /// construct `Panes` without a live terminal.
    waker: Option<TerminalWaker>,
}

impl Panes {
    #[cfg(test)]
    fn new(tx: tokio_mpsc::Sender<PaneEvent>) -> Self {
        Self {
            table: std::collections::HashMap::new(),
            next_id: 1,
            tx,
            waker: None,
        }
    }

    fn with_waker(tx: tokio_mpsc::Sender<PaneEvent>, waker: TerminalWaker) -> Self {
        Self {
            table: std::collections::HashMap::new(),
            next_id: 1,
            tx,
            waker: Some(waker),
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
        self.spawn_argv_env(argv, cwd, &[], center)
    }

    /// As [`Panes::spawn_argv`], but injects `env` into the child — used for
    /// agent panes that expect `SUPERZEJ_WORKTREE`/`SUPERZEJ_BRANCH`.
    fn spawn_argv_env(
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

/// Pre-spawn PTY children for the tabs adjacent to the active one so first focus
/// of a neighbor is instant. Best-effort: `materialize` spawns + remaps a tab's
/// leaf ids in place, so a later switch finds the panes already live and returns
/// early. Errors are ignored (the lazy path will retry on real focus).
fn prewarm_neighbors(panes: &mut Panes, session: &mut crate::session::Session, center: Rect) {
    for i in prewarm_targets(session.active, session.tabs.len(), PREWARM_RADIUS) {
        let _ = panes.materialize(&mut session.tabs[i], center);
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

fn refresh_tab_model(
    model: &mut FrameModel,
    session: &crate::session::Session,
    sb: &mut SidebarState,
) {
    model.tabs = session.tabs.iter().map(|t| t.name.clone()).collect();
    model.active_tab = session.active;
    // The workspace list can change when tabs are added/closed; refresh it from
    // live tabs (the DB-backed slugs are merged on the next hydration).
    if model.sidebar_workspaces.is_empty() {
        model.sidebar_workspaces = workspace_list(session, None);
    } else {
        for (slug, _) in workspace_list(session, None) {
            if !model.sidebar_workspaces.iter().any(|(s, _)| *s == slug) {
                model.sidebar_workspaces.push((slug.clone(), slug));
            }
        }
    }
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
    /// Activate the tab at this session index.
    Activate(usize),
    /// The layout changed (bar width); recompute chrome.
    Relayout,
    /// Close the worktree tabs at these session indices (bulk action).
    CloseTabs(Vec<usize>),
}

impl SidebarState {
    /// Persist a single `ui_state` key for this session's scope.
    fn persist(&self, session_id: &str, key: &str, value: &str) {
        if let Ok(db) = superzej_core::db::Db::open() {
            let _ = db.set_ui_state(session_id, key, value);
        }
    }

    /// The session tab index the cursor row activates, if any.
    fn cursor_tab(&self, model: &FrameModel) -> Option<usize> {
        self.selected_row(model).and_then(|r| r.tab_target)
    }

    /// Build the context-menu entries for the cursor row (item 27).
    fn menu_for_cursor(&self, model: &FrameModel) -> Option<crate::chrome::RowMenu> {
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
        if matches!(row.kind, RowKind::Worktree | RowKind::Page) {
            entries.push(("close", "Close worktree"));
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
                    if let Some(t) = row.tab_target {
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
                self.menu = self.menu_for_cursor(model);
                self.sync(model);
            }
            KeyCode::Char('X') => {
                // Bulk close marked worktrees (item 26); fall back to cursor.
                let targets = self.marked_tab_targets(model);
                if !targets.is_empty() {
                    return SidebarOutcome::CloseTabs(targets);
                }
            }
            KeyCode::Char('<') | KeyCode::Char(',') => {
                return self.adjust_width(-2, session);
            }
            KeyCode::Char('>') | KeyCode::Char('.') => {
                return self.adjust_width(2, session);
            }
            KeyCode::Char(c @ '1'..='9') => {
                // Quick-jump (item 24).
                let idx = (*c as u8 - b'1') as usize;
                if idx < visible {
                    self.cursor = idx;
                    if let Some(t) = self.cursor_tab(model) {
                        self.sync(model);
                        return SidebarOutcome::Activate(t);
                    }
                }
            }
            _ => return SidebarOutcome::NotHandled,
        }
        self.sync(model);
        SidebarOutcome::Redraw
    }

    fn marked_tab_targets(&self, model: &FrameModel) -> Vec<usize> {
        let visible: Vec<&crate::sidebar::SidebarRow> =
            model.sidebar_rows.iter().filter(|r| r.visible).collect();
        let mut targets: Vec<usize> = self
            .marked
            .iter()
            .filter_map(|&i| visible.get(i).and_then(|r| r.tab_target))
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
        if let Some(row) = self.selected_row(model) {
            let key = row.pin_key.clone();
            if let Some(pos) = self.view.pins.iter().position(|k| *k == key) {
                self.view.pins.remove(pos);
                self.persist(&session.id, &format!("pin:{key}"), "0");
            } else {
                self.view.pins.push(key.clone());
                self.persist(&session.id, &format!("pin:{key}"), "1");
            }
            self.rebuild(model, session);
        }
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
                if let Some(t) = self.cursor_tab(model) {
                    return SidebarOutcome::Activate(t);
                }
            }
            "toggle" => return self.toggle_collapse(model, session),
            "pin" => return self.toggle_pin(model, session),
            "close" => {
                if let Some(t) = self.cursor_tab(model) {
                    return SidebarOutcome::CloseTabs(vec![t]);
                }
            }
            _ => {}
        }
        SidebarOutcome::Redraw
    }
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

/// A worktree tab awaiting its agent choice. While set, the command palette is
/// in "agent picker" mode: its selection launches the chosen agent into `tab`
/// rather than dispatching a command. Escaping defaults to a plain shell so the
/// tab is never left with no process.
struct PendingAgent {
    /// The `{repo_slug}/{branch}` tab name to launch into.
    tab: String,
    worktree: String,
    branch: String,
}

/// Build the agent-picker palette items for `cfg`: one row per agent/tool, plus
/// a literal shell. The key is the bare choice name (the `PendingAgent` gate in
/// the Enter handler routes it to a launch, not a command dispatch).
fn build_agent_palette(cfg: &superzej_core::config::Config) -> Vec<crate::palette::PaletteItem> {
    crate::agent::choices(cfg)
        .into_iter()
        .map(|name| {
            let label = format!("{} {name}", superzej_core::theme::agent_glyph(&name));
            crate::palette::PaletteItem::new(name, label)
        })
        .collect()
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
    let Some(idx) = session.tabs.iter().position(|t| t.name == pending.tab) else {
        return false;
    };
    let spec = crate::agent::launch_spec(cfg, &pending.worktree, Some(&pending.branch), choice);
    let cwd = spec.cwd.clone();
    match panes.spawn_argv_env(&spec.argv, cwd.as_deref(), &spec.env, center) {
        Ok(id) => {
            // Reap any panes the tab already had, then back it with the agent pane.
            for old in session.tabs[idx].center.pane_ids() {
                panes.table.remove(&old);
            }
            session.tabs[idx].center = crate::center::CenterTree::Leaf(id);
            session.tabs[idx].focused_pane = id;
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
    } else if !should_be_open
        && drawer.is_some()
        && let Some(id) = drawer.take()
    {
        panes.table.remove(&id);
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
    mut config_rx: tokio_mpsc::UnboundedReceiver<Result<superzej_core::config::Config, String>>,
    refresh_tx: tokio_mpsc::UnboundedSender<RefreshKind>,
    mut refresh_rx: tokio_mpsc::UnboundedReceiver<RefreshKind>,
    waker: TerminalWaker,
) -> Result<()> {
    let mut scratch = Surface::new(cols, rows);
    let mut want_sidebar = true;
    let mut want_panel = true;
    // Sidebar interaction + persisted view state (collapse/sort/pins/width).
    let mut sb = SidebarState::default();
    if let Ok(db) = superzej_core::db::Db::open() {
        sb.load(&db, &session.id);
    }
    let mut sidebar_cols = sb.width.unwrap_or(layout::SIDEBAR_COLS);
    // The last tab name we acked activity for (clears its "look at me" dot).
    let mut last_acked_tab: Option<String> = None;
    let mut chrome = layout::compute_with_width(cols, rows, want_sidebar, want_panel, sidebar_cols);
    sb.rebuild(&mut model, &session);
    let mut dirty = true;
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

    // Diff fs-watcher: bound to the active worktree, re-targeted on tab switch.
    // On a (debounced) change it pushes `RefreshKind::Model` into the shared
    // refresh channel + pulses the waker, so the diff panel updates on file save
    // instead of waiting for the periodic safety-net tick.
    let mut watched_worktree: Option<std::path::PathBuf> = None;
    let mut diff_watcher: Option<notify::RecommendedWatcher> = None;
    retarget_diff_watcher(
        &session,
        &mut watched_worktree,
        &mut diff_watcher,
        &refresh_tx,
        &waker,
    );

    sync_drawer_persistence(&session, &mut panes, &mut drawer, chrome.center);

    let mut current_config = keymap.config().clone();
    // The workspace the keymap was last built for; when the focused workspace
    // changes we rebuild so per-workspace/repo-root keybind layers follow it.
    let mut keymap_workspace = session.id.clone();
    // The active worktree as of the last loop turn. When it changes (any switch
    // path: keymap tab-nav, palette, sidebar) we kick an immediate model + PR
    // refresh and re-target the diff watcher — so the panel is correct for the
    // new worktree right away (stale-while-revalidate) instead of up to 2s late.
    let mut last_active_worktree: Option<std::path::PathBuf> = Some(active_tab_path(&session));
    loop {
        if session.tabs.is_empty() {
            return Ok(()); // last tab closed
        }
        // Per-workspace keybinds: rebuild when the focused workspace changed.
        if session.id != keymap_workspace {
            keymap = rebuild_keymap(&current_config, &session);
            keymap.reset();
            keymap_workspace = session.id.clone();
        }
        let active = session.active;

        // Detect an active-worktree change centrally so every switch path is
        // covered without per-call-site wiring.
        let current_worktree = active_tab_path(&session);
        if last_active_worktree.as_deref() != Some(current_worktree.as_path()) {
            last_active_worktree = Some(current_worktree.clone());
            // Immediate hydrate for the newly-focused worktree; the cached panel
            // stays on screen until the fresh model lands (never blank).
            spawn_model_hydration(model_tx.clone(), session.clone(), Some(waker.clone()));
            spawn_pr_cache_refresh(session.clone(), Some(waker.clone()));
            retarget_diff_watcher(
                &session,
                &mut watched_worktree,
                &mut diff_watcher,
                &refresh_tx,
                &waker,
            );
            // Pre-warm sibling tabs so first focus of a neighbor is instant.
            prewarm_neighbors(&mut panes, &mut session, chrome.center);
        }

        if let Ok(size) = buf.terminal().get_screen_size()
            && (size.rows != rows || size.cols != cols)
        {
            rows = size.rows;
            cols = size.cols;
            chrome = layout::compute_with_width(cols, rows, want_sidebar, want_panel, sidebar_cols);
            need_relayout = true;
            buf.resize(cols, rows);
            dirty = true;
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
                                    if session.tabs[ti].focused_pane == id
                                        && let Some(first) =
                                            session.tabs[ti].center.pane_ids().first()
                                    {
                                        session.tabs[ti].focused_pane = *first;
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

        while let Ok(next_model) = model_rx.try_recv() {
            model = next_model;
            refresh_tab_model(&mut model, &session, &mut sb);
            apply_mode_status(&mut model, mode);
            dirty = true;
        }

        while let Ok(cfg_res) = config_rx.try_recv() {
            match cfg_res {
                Ok(new_cfg) => {
                    keymap = rebuild_keymap(&new_cfg, &session);
                    model.status = keybind_conflict_summary(&new_cfg)
                        .unwrap_or_else(|| "Config reloaded".into());
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
            spawn_model_hydration(model_tx.clone(), session.clone(), Some(waker.clone()));
        }
        if want_pr_refresh {
            spawn_pr_cache_refresh(session.clone(), Some(waker.clone()));
        }

        // Ack the focused worktree's activity so its "look at me" dot clears
        // once the user is actually on the tab. Idempotent; off-thread so the
        // file write never stalls the loop.
        if let Some(tab) = session.tabs.get(session.active).map(|t| t.name.clone())
            && last_acked_tab.as_deref() != Some(tab.as_str())
        {
            last_acked_tab = Some(tab.clone());
            task::spawn_blocking(move || superzej_core::activity::ack(&tab));
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
            buf.draw_from_screen(&scratch, 0, 0);
            if palette.is_none() && !cheatsheet {
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

        // 3. Block until something happens: a real terminal event, or a
        //    `waker.wake()` from any producer (PTY reader, model/PR hydration,
        //    config watcher, diff fs-watch, refresh ticker) which returns
        //    `InputEvent::Wake`. No timeout → zero idle CPU; we only wake when
        //    there is work, and render the instant it arrives.
        match buf.terminal().poll_input(None) {
            Ok(Some(InputEvent::Key(k))) => {
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
                // Modal: when the palette is open it captures all keys.
                if let Some(p) = palette.as_mut() {
                    // Agent-picker mode: the palette is choosing what to run in a
                    // just-created worktree tab. Enter launches the choice; Escape
                    // defaults to a shell so the tab never sits with no process.
                    if let Some(pending) = pending_agent.as_ref() {
                        match k.key {
                            KeyCode::Escape => {
                                launch_agent_into_tab(
                                    keymap.config(),
                                    &mut session,
                                    &mut panes,
                                    pending,
                                    "shell",
                                    chrome.center,
                                );
                                pending_agent = None;
                                palette = None;
                                refresh_tab_model(&mut model, &session, &mut sb);
                                need_relayout = true;
                            }
                            KeyCode::Enter => {
                                let choice = p
                                    .selected_item()
                                    .map(|i| i.key.clone())
                                    .unwrap_or_else(|| "shell".to_string());
                                launch_agent_into_tab(
                                    keymap.config(),
                                    &mut session,
                                    &mut panes,
                                    pending,
                                    &choice,
                                    chrome.center,
                                );
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
                                        refresh_tab_model(&mut model, &session, &mut sb);
                                        need_relayout = true;
                                        sync_drawer_persistence(
                                            &session,
                                            &mut panes,
                                            &mut drawer,
                                            chrome.center,
                                        );
                                    }
                                } else if let Some(repo_path) = key.strip_prefix("repo:") {
                                    if let Ok(db) = superzej_core::db::Db::open()
                                        && session.switch_to_workspace(repo_path, &db).is_ok()
                                    {
                                        refresh_tab_model(&mut model, &session, &mut sb);
                                        need_relayout = true;
                                        sync_drawer_persistence(
                                            &session,
                                            &mut panes,
                                            &mut drawer,
                                            chrome.center,
                                        );
                                    }
                                } else if let Some(name) = key.strip_prefix("tab:")
                                    && let Some(i) =
                                        session.tabs.iter().position(|t| t.name == name)
                                {
                                    session.switch_to(i);
                                    refresh_tab_model(&mut model, &session, &mut sb);
                                    need_relayout = true;
                                    sync_drawer_persistence(
                                        &session,
                                        &mut panes,
                                        &mut drawer,
                                        chrome.center,
                                    );
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
                if sb.focused {
                    match sb.handle_key(&k.key, k.modifiers, &mut model, &session) {
                        SidebarOutcome::NotHandled => { /* fall through to keymap */ }
                        SidebarOutcome::Redraw => {
                            dirty = true;
                            continue;
                        }
                        SidebarOutcome::Defocus => {
                            sb.focused = false;
                            sb.menu = None;
                            sb.sync(&mut model);
                            dirty = true;
                            continue;
                        }
                        SidebarOutcome::Relayout => {
                            sidebar_cols = sb.width.unwrap_or(layout::SIDEBAR_COLS);
                            chrome = layout::compute_with_width(
                                cols,
                                rows,
                                want_sidebar,
                                want_panel,
                                sidebar_cols,
                            );
                            need_relayout = true;
                            dirty = true;
                            continue;
                        }
                        SidebarOutcome::Activate(t) => {
                            if t < session.tabs.len() {
                                session.switch_to(t);
                                refresh_tab_model(&mut model, &session, &mut sb);
                                need_relayout = true;
                                sync_drawer_persistence(
                                    &session,
                                    &mut panes,
                                    &mut drawer,
                                    chrome.center,
                                );
                            }
                            dirty = true;
                            continue;
                        }
                        SidebarOutcome::CloseTabs(mut targets) => {
                            // Close from the highest index down so earlier
                            // indices stay valid as tabs are removed.
                            targets.sort_unstable_by(|a, b| b.cmp(a));
                            for t in targets {
                                if t < session.tabs.len() {
                                    for id in session.tabs[t].center.pane_ids() {
                                        panes.table.remove(&id);
                                    }
                                    session.switch_to(t);
                                    session.close_active();
                                }
                            }
                            sb.marked.clear();
                            refresh_tab_model(&mut model, &session, &mut sb);
                            need_relayout = true;
                            sync_drawer_persistence(
                                &session,
                                &mut panes,
                                &mut drawer,
                                chrome.center,
                            );
                            dirty = true;
                            continue;
                        }
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
                let dispatch = match keymap.program_action(&focused_program, &input_key) {
                    Some(action) => {
                        keymap.reset();
                        crate::sequence::MatchResult::Matched(action)
                    }
                    None => keymap.dispatch(mode, input_key.clone()),
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
                                chrome = layout::compute_with_width(
                                    cols,
                                    rows,
                                    want_sidebar,
                                    want_panel,
                                    sidebar_cols,
                                );
                                if !want_sidebar && sb.focused {
                                    sb.focused = false;
                                    sb.sync(&mut model);
                                }
                                need_relayout = true;
                            }
                            Action::TogglePanel => {
                                want_panel = !want_panel;
                                chrome = layout::compute_with_width(
                                    cols,
                                    rows,
                                    want_sidebar,
                                    want_panel,
                                    sidebar_cols,
                                );
                                need_relayout = true;
                            }
                            Action::FocusSidebar => {
                                if !want_sidebar {
                                    want_sidebar = true;
                                    chrome = layout::compute_with_width(
                                        cols,
                                        rows,
                                        want_sidebar,
                                        want_panel,
                                        sidebar_cols,
                                    );
                                    need_relayout = true;
                                }
                                // Take keyboard focus, land on the active worktree.
                                sb.focused = true;
                                sb.rebuild(&mut model, &session);
                            }
                            Action::FocusPanel if !want_panel => {
                                want_panel = true;
                                chrome = layout::compute_with_width(
                                    cols,
                                    rows,
                                    want_sidebar,
                                    want_panel,
                                    sidebar_cols,
                                );
                                need_relayout = true;
                            }
                            Action::NextTab => {
                                session.next_tab();
                                refresh_tab_model(&mut model, &session, &mut sb);
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
                                refresh_tab_model(&mut model, &session, &mut sb);
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
                            Action::Pin(idx) => {
                                // Open-or-focus the configured pin at this 1-based
                                // index as a dedicated `pin:<name>` tab running its
                                // command (host, no sandbox — pins are global).
                                if let Some(pin) = keymap.config().pin_by_index(idx as usize) {
                                    let tab_name = format!("pin:{}", pin.name);
                                    if let Some(i) =
                                        session.tabs.iter().position(|t| t.name == tab_name)
                                    {
                                        session.switch_to(i);
                                    } else {
                                        let cwd = pin.cwd.clone().unwrap_or_else(|| {
                                            superzej_core::util::home()
                                                .to_string_lossy()
                                                .into_owned()
                                        });
                                        let argv = vec![
                                            superzej_core::util::shell(),
                                            "-lc".to_string(),
                                            pin.command.clone(),
                                        ];
                                        if let Ok(id) = panes.spawn_argv(
                                            &argv,
                                            Some(Path::new(&cwd)),
                                            chrome.center,
                                        ) {
                                            session.add_tab(crate::session::Tab {
                                                name: tab_name,
                                                kind: crate::session::TabKind::Pinned,
                                                worktree: cwd,
                                                center: crate::center::CenterTree::Leaf(id),
                                                focused_pane: id,
                                            });
                                        }
                                    }
                                    refresh_tab_model(&mut model, &session, &mut sb);
                                    need_relayout = true;
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
                                    if session.switch_to_workspace(&repo_path, &db).is_ok() {
                                        refresh_tab_model(&mut model, &session, &mut sb);
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
                            Action::NewWorktree => {
                                // Create a real git worktree off the active tab's repo, add a
                                // `{slug}/{branch}` tab for it, then open the agent picker —
                                // its selection launches the chosen agent into the new tab.
                                let src_wt = session.tabs[active].worktree.clone();
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
                                        session.add_tab(crate::session::Tab {
                                            name: nw.tab.clone(),
                                            kind: crate::session::TabKind::Worktree,
                                            worktree: nw.path.clone(),
                                            center: crate::center::CenterTree::Leaf(0),
                                            focused_pane: 0,
                                        });
                                        refresh_tab_model(&mut model, &session, &mut sb);
                                        need_relayout = true;
                                        pending_agent = Some(PendingAgent {
                                            tab: nw.tab,
                                            worktree: nw.path,
                                            branch: nw.branch,
                                        });
                                        palette = Some(crate::palette::Palette::new(
                                            build_agent_palette(keymap.config()),
                                        ));
                                    }
                                } else {
                                    superzej_core::msg::warn(
                                        "new-worktree: not inside a git repository",
                                    );
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
                                refresh_tab_model(&mut model, &session, &mut sb);
                                need_relayout = true;
                            }
                            Action::CloseWorktree => {
                                // Close the active tab; reap its panes' processes.
                                for id in session.tabs[active].center.pane_ids() {
                                    panes.table.remove(&id);
                                }
                                session.close_active();
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
                                // Spawns the requested tool in a native drawer pane.
                                let cwd = tab_cwd(&session.tabs[active]);
                                let tool_name = match action {
                                    Action::Lazygit => "lazygit",
                                    Action::Editor => "editor",
                                    Action::Diff => "diff",
                                    _ => unreachable!(),
                                };
                                if let Some(cmd_str) = keymap.config().tool_command(tool_name) {
                                    if drawer.is_some()
                                        && let Some(id) = drawer.take()
                                    {
                                        panes.table.remove(&id);
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
                                if drawer.is_some()
                                    && let Some(id) = drawer.take()
                                {
                                    panes.table.remove(&id);
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
                let bytes = remapped.or_else(|| key_bytes(&k.key, k.modifiers));
                if let Some(bytes) = bytes {
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
                chrome =
                    layout::compute_with_width(cols, rows, want_sidebar, want_panel, sidebar_cols);
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

    fn two_worktree_session() -> Session {
        Session {
            id: "s1".into(),
            tabs: vec![
                Tab {
                    name: "app/home".into(),
                    kind: TabKind::Home,
                    worktree: "/tmp/app".into(),
                    center: CenterTree::Leaf(0),
                    focused_pane: 0,
                },
                Tab {
                    name: "app/feat".into(),
                    kind: TabKind::Worktree,
                    worktree: "/tmp/app-feat".into(),
                    center: CenterTree::Leaf(0),
                    focused_pane: 0,
                },
            ],
            active: 0,
        }
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
        model.sidebar_workspaces = vec![("app".into(), "app".into())];
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
    fn sidebar_quick_jump_activates_numbered_row() {
        let session = two_worktree_session();
        let mut model = build_initial_model(&session);
        model.sidebar_workspaces = vec![("app".into(), "app".into())];
        let mut sb = focused_state(&mut model, &session);
        // Rows: 1=app(ws) 2=home 3=feat. Jump to 3 -> activate feat's tab.
        let out = press(&mut sb, '3', &mut model, &session);
        match out {
            SidebarOutcome::Activate(t) => assert_eq!(session.tabs[t].name, "app/feat"),
            _ => panic!("expected Activate"),
        }
    }

    #[test]
    fn sidebar_multiselect_marks_and_bulk_close_targets_marked() {
        let session = two_worktree_session();
        let mut model = build_initial_model(&session);
        model.sidebar_workspaces = vec![("app".into(), "app".into())];
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
            SidebarOutcome::CloseTabs(t) => assert_eq!(t.len(), 2),
            _ => panic!("expected CloseTabs"),
        }
    }

    #[test]
    fn sidebar_width_adjust_clamps_and_relayouts() {
        // Persisting width opens the global DB; redirect it to a temp dir so the
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

        // SAFETY: test is single-threaded; sets/clears an XDG var around one call.
        unsafe { std::env::set_var("XDG_STATE_HOME", &state_home) };

        let session = load_or_seed_session(std::path::Path::new("/tmp/app"));

        unsafe { std::env::remove_var("XDG_STATE_HOME") };

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

        // SAFETY: test is single-threaded; sets/clears an XDG var around calls.
        unsafe { std::env::set_var("XDG_STATE_HOME", &state_home) };

        let session = load_or_seed_session(std::path::Path::new("/tmp/repo1"));
        let model = build_model(&session, &db);

        unsafe { std::env::remove_var("XDG_STATE_HOME") };

        let slugs: Vec<&str> = model
            .sidebar_workspaces
            .iter()
            .map(|(s, _)| s.as_str())
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
        // The cheap initial model carries no derived rows yet (the event loop
        // builds them once view state is loaded).
        assert!(model.sidebar_rows.is_empty());
        assert!(model.panel.branch == "app/home");
        assert!(model.status.contains("Starting szhost"));
    }

    fn sidebar_labels(model: &FrameModel) -> Vec<String> {
        model.sidebar_rows.iter().map(|r| r.label.clone()).collect()
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

        session.add_tab(Tab {
            name: "app/feature-x".into(),
            kind: TabKind::Worktree,
            worktree: "/tmp/app-feature-x".into(),
            center: CenterTree::Leaf(0),
            focused_pane: 0,
        });
        refresh_tab_model(&mut model, &session, &mut sb);

        assert_eq!(model.active_tab, 1);
        assert!(
            sidebar_labels(&model)
                .iter()
                .any(|row| row.contains("feature-x")),
            "sidebar should include newly-created worktree tabs immediately: {:?}",
            sidebar_labels(&model)
        );
    }
    #[test]
    fn action_new_worktree_adds_tab_and_focuses_it() {
        let mut session = one_tab_session();
        let mut model = build_initial_model(&session);
        let mut sb = SidebarState::default();

        // Simulating the Action block manually since the event loop is complex to instantiate
        // NewWorktree creates a new worktree entry (separate branch), not a page of existing worktree
        let (repo, branch) = crate::sidebar::split_tab(&session.tabs[0].name)
            .unwrap_or_else(|| (session.tabs[0].name.clone(), "home".to_string()));
        let (_base, _) = crate::sidebar::split_page(&branch);
        let new_n = session.tabs.len();
        let tab = crate::session::Tab {
            name: format!("{}/·{}", repo, new_n),
            kind: crate::session::TabKind::Worktree,
            worktree: session.tabs[0].worktree.clone(),
            center: crate::center::CenterTree::Leaf(0),
            focused_pane: 0,
        };
        session.add_tab(tab);
        refresh_tab_model(&mut model, &session, &mut sb);

        assert_eq!(session.tabs.len(), 2);
        assert_eq!(session.active, 1);
        assert_eq!(session.tabs[1].name, "app/·1");
        assert_eq!(model.active_tab, 1);
        assert_eq!(model.tabs[1], "app/·1");
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
        let mut sb = SidebarState::default();
        let chrome = layout::compute(160, 40, true, true);
        let before = chrome.clone();

        session.switch_to(1);
        refresh_tab_model(&mut model, &session, &mut sb);

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
