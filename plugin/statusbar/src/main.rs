//! superzej statusbar — a thin, context-aware bottom hint bar.
//!
//! Replaces zellij's default two-line status bar with a single curated line of
//! the most useful keys for *right now*: it switches on the input **mode** (from
//! `ModeUpdate`) and, in Normal mode, on whether the focused tab is a repo
//! **home** tab or a **worktree** tab (from `TabUpdate`). So the hints stay
//! short and relevant instead of a wall of bindings.

use std::collections::BTreeMap;
use zellij_tile::prelude::*;

mod theme;
use theme::{BG0, BG1, DIM, GHOST, RESET, TEAL};

/// Auto-collapse thresholds (total terminal cols). The panel is the widest
/// chrome (~27%), so it folds first to give the center room; the sidebar
/// (~12%) folds only on a genuinely cramped terminal.
///
/// The sidebar threshold must sit *above* zellij's ~64-col floor for relaying
/// out the sidebar+center+bars: a fold only fires on a width zellij actually
/// lays out, and while the sidebar is shown zellij won't shrink the tab below
/// that floor — so a threshold at/under it would never trigger (the sidebar's
/// own presence blocks the narrow relayout that would collapse it).
const PANEL_MIN_TOTAL_COLS: usize = 100;
const SIDEBAR_MIN_TOTAL_COLS: usize = 76;

/// The bottom file-manager drawer's pane name (set by `superzej files` when it
/// spawns the floating pane). Kept in sync with `commands::files::PANE_NAME`.
const FILES_PANE: &str = "superzej-files";

#[derive(Default)]
struct State {
    mode: Option<InputMode>,
    worktree_ctx: bool, // focused tab is a worktree (not the repo home)
    accent: String,     // "R;G;B" from `superzej theme` (TEAL until it lands)
    // Visibility controller. The statusbar is the one chrome surface that is
    // never hidden and always full-width, so it owns hide/show for the sidebar
    // and panel: a suppressed plugin can't reliably re-show *itself* (nor see
    // the terminal width while suppressed), but an always-visible pane can
    // hide/show *another* pane and reapply the layout. Per-surface state is
    // `manual` (the Ctrl+Alt+s/p toggle) OR `auto` (narrow terminal); the pane
    // is suppressed when either holds.
    my_id: Option<u32>,
    sidebar: Surface,
    panel: Surface,
    term_cols: usize, // last width seen in render (the statusbar spans full width)
    // The bottom file-manager drawer. Unlike sidebar/panel it is a spawn/close
    // command pane (not a suppressed plugin), so the statusbar only needs its id
    // — to close it on the toggle pipe — and to re-open it (per-worktree) when
    // its tab (re)loads. No reconcile/`next_swap_layout` involvement.
    files_id: Option<u32>,         // drawer pane id in THIS tab (None ⇒ closed)
    my_tab_index: Option<usize>,   // manifest key of the tab holding our own pane
    active_tab_pos: Option<usize>, // position of the session's active tab
    active_tab_name: String,       // its name (for the restore `--tab` arg)
    session: Option<String>,       // current session name (restore `--session` arg)
    restore_poked: bool,           // restore already requested for this activation
}

/// Tracked visibility of one controlled chrome surface.
#[derive(Default)]
struct Surface {
    id: Option<u32>,  // its pane id (stable per tab once seen)
    manual: bool,     // user toggled it hidden
    auto: bool,       // narrow-terminal auto-collapse wants it hidden
    suppressed: bool, // what the live layout currently shows
}

register_plugin!(State);

impl ZellijPlugin for State {
    fn load(&mut self, _config: BTreeMap<String, String>) {
        self.accent = TEAL.into();
        request_permission(&[
            PermissionType::ReadApplicationState,
            PermissionType::ChangeApplicationState, // hide/show sidebar+panel
            PermissionType::RunCommands,            // `superzej theme`, persist visibility
            PermissionType::ReadCliPipes,           // unblock CLI toggle pipes
        ]);
        self.my_id = Some(get_plugin_ids().plugin_id);
        subscribe(&[
            EventType::ModeUpdate,
            EventType::TabUpdate,
            EventType::PaneUpdate,
            EventType::SessionUpdate, // session name for the drawer restore poke
            EventType::RunCommandResult,
            EventType::PermissionRequestResult,
        ]);
        fetch_theme();
        // Restore any persisted manual-hide (a toggle may have hidden a surface
        // before this per-tab statusbar loaded). Replies tagged vis_sidebar/vis_panel.
        self.pull_visibility();
    }

    fn update(&mut self, event: Event) -> bool {
        match event {
            Event::ModeUpdate(info) => {
                if self.mode != Some(info.mode) {
                    self.mode = Some(info.mode);
                    return true;
                }
                false
            }
            // The load()-time fetch/pull race the permission grant; redo once
            // permissions actually land.
            Event::PermissionRequestResult(_) => {
                fetch_theme();
                self.pull_visibility();
                false
            }
            // Track the session name so the drawer restore poke can target the
            // right session from its plugin-spawned `superzej files` (which
            // can't read it from env). Same source the tabbar uses for new-tab.
            Event::SessionUpdate(infos, _) => {
                self.session = infos
                    .iter()
                    .find(|s| s.is_current_session)
                    .map(|s| s.name.clone());
                false
            }
            // The controller watches geometry: every resize/structure change
            // arrives here (the statusbar is always visible, so it never misses
            // one). Recompute auto-hide from the total width and reconcile.
            Event::PaneUpdate(manifest) => {
                self.scan_panes(&manifest);
                self.reconcile();
                self.maybe_restore();
                false
            }
            Event::RunCommandResult(code, stdout, _, ctx)
                if ctx.get("cmd").map(|s| s.as_str()) == Some("theme") =>
            {
                if code == Some(0) {
                    if let Some(rgb) = parse_rgb_line(&stdout) {
                        if self.accent != rgb {
                            self.accent = rgb;
                            return true;
                        }
                    }
                }
                false
            }
            // Persisted manual-hide ("false" == hidden) for each surface.
            Event::RunCommandResult(_, stdout, _, ctx)
                if matches!(
                    ctx.get("cmd").map(|s| s.as_str()),
                    Some("vis_sidebar") | Some("vis_panel")
                ) =>
            {
                let hidden = String::from_utf8_lossy(&stdout).trim() == "false";
                match ctx.get("cmd").map(|s| s.as_str()) {
                    Some("vis_sidebar") => self.sidebar.manual = hidden,
                    Some("vis_panel") => self.panel.manual = hidden,
                    _ => {}
                }
                self.reconcile();
                false
            }
            Event::TabUpdate(tabs) => {
                // Track the active tab (position + name) so a worktree tab can
                // restore its drawer when it becomes/loads as the focused tab.
                if let Some(active) = tabs.iter().find(|t| t.active) {
                    self.active_tab_pos = Some(active.position);
                    self.active_tab_name = active.name.clone();
                }
                // `{slug}/home` => home tab; anything else under a repo is a worktree.
                let wt = tabs
                    .iter()
                    .find(|t| t.active)
                    .map(|t| {
                        t.name
                            .rsplit_once('/')
                            .map(|(_, b)| b != "home")
                            .unwrap_or(false)
                    })
                    .unwrap_or(false);
                let changed = self.worktree_ctx != wt;
                self.worktree_ctx = wt;
                self.maybe_restore();
                changed
            }
            _ => false,
        }
    }

    fn pipe(&mut self, pipe: PipeMessage) -> bool {
        // CLI pipes (`zellij pipe`) deliver twice (payload + stdin-EOF) and block
        // until unblocked; unblock both, act only on the payload-bearing one.
        if let PipeSource::Cli(id) = &pipe.source {
            unblock_cli_pipe_input(id);
            if pipe.payload.is_none() {
                return false;
            }
        }
        match pipe.name.as_str() {
            "superzej_toggle_sidebar" => {
                self.sidebar.manual = !self.sidebar.manual;
                self.reconcile();
                self.persist("sidebar", !self.sidebar.manual);
            }
            "superzej_toggle_panel" => {
                self.panel.manual = !self.panel.manual;
                self.reconcile();
                self.persist("panel", !self.panel.manual);
            }
            "superzej_show_sidebar" => {
                if self.sidebar.manual {
                    self.sidebar.manual = false;
                    self.reconcile();
                    self.persist("sidebar", true);
                }
            }
            "superzej_show_panel" => {
                if self.panel.manual {
                    self.panel.manual = false;
                    self.reconcile();
                    self.persist("panel", true);
                }
            }
            // Close the drawer by id (the CLI can only close the focused pane).
            // Only the tab actually holding the drawer has its id, so a broadcast
            // pipe closes exactly the one drawer.
            "superzej_close_files" => {
                if let Some(id) = self.files_id {
                    close_pane_with_id(PaneId::Terminal(id));
                }
            }
            _ => {}
        }
        false
    }

    fn render(&mut self, _rows: usize, cols: usize) {
        // render fires on every resize (PaneUpdate does not), and our pane is
        // full-width — so this is the reliable total-width signal.
        self.eval_width(cols);
        let mode = self.mode.unwrap_or(InputMode::Normal);
        let chips = self.chips(mode);
        let accent = self.accent.as_str();

        let mut out = String::new();
        out.push_str(&format!("\u{1b}[48;2;{BG1}m")); // bar background
        let mut col = 0usize;

        // A mode indicator chip on the left when not in Normal mode.
        if mode != InputMode::Normal {
            let label = mode_name(mode);
            push_raw(
                &mut out,
                &mut col,
                cols,
                &format!(
                    "\u{1b}[1m\u{1b}[38;2;{BG0}m\u{1b}[48;2;{accent}m {label} \u{1b}[0m\u{1b}[48;2;{BG1}m"
                ),
                label.chars().count() + 2,
            );
        } else {
            push_raw(&mut out, &mut col, cols, " ", 1);
        }

        for (i, (key, label)) in chips.iter().enumerate() {
            if i > 0 {
                push_raw(
                    &mut out,
                    &mut col,
                    cols,
                    &format!("\u{1b}[38;2;{GHOST}m  ·  "),
                    5,
                );
            } else {
                push_raw(&mut out, &mut col, cols, " ", 1);
            }
            push_raw(
                &mut out,
                &mut col,
                cols,
                &format!("\u{1b}[1m\u{1b}[38;2;{accent}m{key}\u{1b}[0m\u{1b}[48;2;{BG1}m"),
                key.chars().count(),
            );
            push_raw(
                &mut out,
                &mut col,
                cols,
                &format!("\u{1b}[38;2;{DIM}m {label}"),
                1 + label.chars().count(),
            );
        }

        // Pad the rest of the line with the bar background.
        if col < cols {
            out.push_str(&format!("\u{1b}[38;2;{GHOST}m{}", " ".repeat(cols - col)));
        }
        out.push_str(RESET);
        print!("{out}");
    }
}

/// Kick off `superzej theme`; the accent lands via RunCommandResult.
fn fetch_theme() {
    let mut ctx = BTreeMap::new();
    ctx.insert("cmd".to_string(), "theme".to_string());
    run_command(&["superzej", "theme"], ctx);
}

/// Outcome of the per-activation drawer-restore check (see `restore_decision`).
enum RestoreDecision {
    /// Not our active tab — re-arm so the next focus can fire.
    Disarm,
    /// Conditions not met (not focused, no session yet, already open/poked).
    Skip,
    /// Re-open this worktree's drawer (it was left open).
    Fire,
}

/// Pure restore gate: fire once per activation when our tab is the active
/// worktree tab, no drawer is open, and the session name is known. Kept free of
/// `self`/host calls so the branch table is unit-testable.
fn restore_decision(
    my_tab_index: Option<usize>,
    active_tab_pos: Option<usize>,
    worktree_ctx: bool,
    files_open: bool,
    restore_poked: bool,
    has_session: bool,
) -> RestoreDecision {
    let (Some(mine), Some(active)) = (my_tab_index, active_tab_pos) else {
        return RestoreDecision::Skip;
    };
    if mine != active {
        return RestoreDecision::Disarm;
    }
    if has_session && worktree_ctx && !files_open && !restore_poked {
        RestoreDecision::Fire
    } else {
        RestoreDecision::Skip
    }
}

/// First line of stdout as a validated "R;G;B" triple.
fn parse_rgb_line(stdout: &[u8]) -> Option<String> {
    let s = String::from_utf8_lossy(stdout);
    let line = s.lines().next()?.trim();
    let ok = line.split(';').filter(|p| p.parse::<u8>().is_ok()).count() == 3;
    ok.then(|| line.to_string())
}

impl State {
    /// Discover the sidebar/panel pane ids in this tab and sync their live
    /// suppression from the manifest. The width-driven auto-hide is decided in
    /// `render` (zellij fires render on every resize but only sometimes a
    /// PaneUpdate), so this just keeps ids and the `suppressed` mirror fresh.
    fn scan_panes(&mut self, manifest: &PaneManifest) {
        let Some(me) = self.my_id else { return };
        // Only the layer (tab) that holds our own pane — other tabs carry the
        // same plugin urls under different ids.
        let Some((idx, panes)) = manifest
            .panes
            .iter()
            .find(|(_, ps)| ps.iter().any(|p| p.is_plugin && p.id == me))
        else {
            return;
        };
        self.my_tab_index = Some(*idx);
        // The drawer is a non-plugin (command) pane named FILES_PANE; track its
        // id so the close pipe can target it, and so restore knows it's open.
        // Absent ⇒ closed (e.g. the user quit yazi, or it was never opened).
        let mut files_id = None;
        for p in panes {
            if p.is_plugin {
                match p.plugin_url.as_deref() {
                    Some(u) if u.contains("sidebar.wasm") => {
                        self.sidebar.id = Some(p.id);
                        self.sidebar.suppressed = p.is_suppressed;
                    }
                    Some(u) if u.contains("panel.wasm") => {
                        self.panel.id = Some(p.id);
                        self.panel.suppressed = p.is_suppressed;
                    }
                    _ => {}
                }
            } else if p.title == FILES_PANE {
                files_id = Some(p.id);
            }
        }
        self.files_id = files_id;
    }

    /// Per-worktree drawer auto-restore: when our tab is the active worktree tab
    /// and no drawer is open, ask `superzej files --restore` to re-open it iff it
    /// was left open for this worktree. Fires once per activation; the CLI side
    /// no-ops when the worktree was last closed or a drawer is already present.
    fn maybe_restore(&mut self) {
        match restore_decision(
            self.my_tab_index,
            self.active_tab_pos,
            self.worktree_ctx,
            self.files_id.is_some(),
            self.restore_poked,
            self.session.is_some(),
        ) {
            RestoreDecision::Disarm => self.restore_poked = false,
            RestoreDecision::Skip => {}
            RestoreDecision::Fire => {
                self.restore_poked = true;
                let session = self.session.clone().unwrap_or_default();
                let mut ctx = BTreeMap::new();
                ctx.insert("cmd".to_string(), "files_restore".to_string());
                run_command(
                    &[
                        "superzej",
                        "files",
                        "--restore",
                        "--tab",
                        &self.active_tab_name,
                        "--session",
                        &session,
                    ],
                    ctx,
                );
            }
        }
    }

    /// Recompute the width-driven auto-hide from the total terminal width and
    /// reconcile. Called from `render` (the reliable per-resize signal) — the
    /// statusbar spans the full width, so its render `cols` is the terminal width.
    fn eval_width(&mut self, cols: usize) {
        if cols == 0 || cols == self.term_cols {
            return;
        }
        self.term_cols = cols;
        self.panel.auto = cols < PANEL_MIN_TOTAL_COLS;
        self.sidebar.auto = cols < SIDEBAR_MIN_TOTAL_COLS;
        self.reconcile();
    }

    /// Reconcile the live layout with the desired visibility of both surfaces
    /// (a surface should be hidden when `manual` or `auto`).
    ///
    /// A *hide* just reflows the tiled layout, so it needs no relayout. A
    /// *show*, though, re-inserts the pane via `add_tiled_pane` — a raw ~50%
    /// split with the center — and only `next_swap_layout()` snaps it back to
    /// its template slot. That restore is reliable ONLY at the full 5-pane set:
    /// the base template has five slots, and zellij matches neither it nor any
    /// swap variant (all `min_panes=6`) while a *sibling* surface is suppressed.
    /// Running `next_swap_layout()` at 4 panes leaves the shown surface stuck as
    /// the 50% split — the "panel jammed half-way into the center" bug.
    ///
    /// So if anything needs showing, first un-suppress BOTH surfaces, relayout
    /// once at the full 5 panes, then re-hide whichever should stay hidden
    /// (a brief sibling flash, but every pane lands in its slot). Driven from
    /// the statusbar's always-visible context.
    fn reconcile(&mut self) {
        let hidden = |s: &Surface| s.manual || s.auto;
        let need_show = [&self.sidebar, &self.panel]
            .iter()
            .any(|s| s.id.is_some() && s.suppressed && !hidden(s));
        if need_show {
            for s in [&mut self.sidebar, &mut self.panel] {
                if let (Some(id), true) = (s.id, s.suppressed) {
                    show_pane_with_id(PaneId::Plugin(id), false, false);
                    s.suppressed = false;
                }
            }
            next_swap_layout();
        }
        for s in [&mut self.sidebar, &mut self.panel] {
            let Some(id) = s.id else { continue };
            if hidden(s) && !s.suppressed {
                hide_pane_with_id(PaneId::Plugin(id));
                s.suppressed = true;
            }
        }
    }

    /// Ask for both persisted manual-hide flags (replies tagged vis_sidebar/vis_panel).
    fn pull_visibility(&self) {
        for (file, tag) in [
            (".sidebar_state", "vis_sidebar"),
            (".panel_state", "vis_panel"),
        ] {
            run_command(
                &[
                    "sh",
                    "-c",
                    // Honor SUPERZEJ_DIR so a dev/test instance reads its own state.
                    &format!(
                        "cat \"${{SUPERZEJ_DIR:-$HOME/.superzej}}/{file}\" 2>/dev/null || true"
                    ),
                ],
                BTreeMap::from([("cmd".to_string(), tag.to_string())]),
            );
        }
    }

    /// Persist a surface's manual-visibility so new tabs start consistent
    /// (the file holds "true" when visible). Auto-hide is never persisted.
    fn persist(&self, surface: &str, visible: bool) {
        let file = match surface {
            "sidebar" => ".sidebar_state",
            _ => ".panel_state",
        };
        run_command(
            &[
                "sh",
                "-c",
                &format!(
                    "d=\"${{SUPERZEJ_DIR:-$HOME/.superzej}}\"; mkdir -p \"$d\" && echo {visible} > \"$d/{file}\""
                ),
            ],
            BTreeMap::new(),
        );
    }

    /// (key, label) chips for the current mode + tab context.
    fn chips(&self, mode: InputMode) -> Vec<(&'static str, &'static str)> {
        match mode {
            InputMode::Normal => {
                let mut v = vec![
                    ("Cmd-K", "menu"),
                    ("A-←→", "tabs"),
                    ("S-A-←→", "panes"),
                    ("A-W", "new repo"),
                    ("A-w", "worktree"),
                    ("A-n", "split"),
                ];
                if self.worktree_ctx {
                    v.extend_from_slice(&[("A-g", "lazygit"), ("A-e", "edit"), ("A-X", "close")]);
                } else {
                    v.extend_from_slice(&[("A-o", "switch repo"), ("A-d", "dashboard")]);
                }
                v
            }
            InputMode::Pane => vec![
                ("→", "right"),
                ("↓", "down"),
                ("x", "close"),
                ("f", "fullscreen"),
                ("w", "float"),
                ("z", "frames"),
                ("⏎", "done"),
            ],
            InputMode::Tab => vec![
                ("n", "new"),
                ("x", "close"),
                ("←→", "move"),
                ("r", "rename"),
                ("⏎", "done"),
            ],
            InputMode::Resize => vec![("←↑↓→", "resize"), ("+-", "size"), ("⏎", "done")],
            InputMode::Move => vec![("←↑↓→", "move pane"), ("⏎", "done")],
            InputMode::Scroll => vec![
                ("↑↓", "scroll"),
                ("/", "search"),
                ("e", "edit"),
                ("⏎", "done"),
            ],
            InputMode::Session => vec![("d", "detach"), ("w", "sessions"), ("⏎", "done")],
            InputMode::Locked => vec![("Ctrl-g", "unlock")],
            _ => vec![("⏎", "done"), ("Esc", "cancel")],
        }
    }
}

fn mode_name(mode: InputMode) -> &'static str {
    match mode {
        InputMode::Pane => "PANE",
        InputMode::Tab => "TAB",
        InputMode::Resize => "RESIZE",
        InputMode::Move => "MOVE",
        InputMode::Scroll => "SCROLL",
        InputMode::Session => "SESSION",
        InputMode::Locked => "LOCKED",
        InputMode::RenameTab => "RENAME TAB",
        InputMode::RenamePane => "RENAME PANE",
        InputMode::Search => "SEARCH",
        InputMode::EnterSearch => "SEARCH",
        _ => "MODE",
    }
}

/// Append `text` (which carries its own ANSI) if at least part of its `width`
/// visible cells fit; advances `col`. We stop adding once full (no mid-chip
/// clipping — chips are short, so this keeps escapes well-formed).
fn push_raw(out: &mut String, col: &mut usize, cols: usize, text: &str, width: usize) {
    if *col >= cols {
        return;
    }
    if *col + width > cols {
        // No room for the whole chip — stop here to avoid splitting escapes.
        *col = cols;
        return;
    }
    out.push_str(text);
    *col += width;
}
