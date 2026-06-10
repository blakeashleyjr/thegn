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

/// Pane name (`zellij run --name`) of the command palette's floating pane. Used
/// both to spawn it and to find it in the manifest for the toggle-close.
const PALETTE_PANE_NAME: &str = "superzej-palette";

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
    my_tab: Option<usize>, // tab position this instance lives on (manifest key)
    active_tab: Option<usize>, // the currently-focused tab (from TabUpdate)
    sidebar: Surface,
    panel: Surface,
    term_cols: usize, // last width seen in render (the statusbar spans full width)
    // Bottom-bar selection (Super+Alt+Down focuses this pane). Highlight-only
    // for now — Enter is reserved for a future action. Esc / moving focus away
    // clears it. `center_id` is the terminal to hand focus back to on Esc.
    selected: bool,
    center_id: Option<u32>,
    // Command palette (Super+K). The statusbar owns the toggle: a bare `Run`
    // keybind only ever *spawns* a pane, so a second press can't close the open
    // palette (and rapid presses race a flurry of floating panes that flash open
    // and vanish). Routing Super+K through here makes it a real toggle — open if
    // closed, close if open. `palette_id` is the open palette's floating pane id,
    // tracked from the manifest (None when closed). `active_tab_name` doubles as
    // the drawer restore `--tab` arg.
    active_tab_name: Option<String>,
    palette_id: Option<u32>,
    // The bottom file-manager drawer. Unlike sidebar/panel it is a spawn/close
    // command pane (not a suppressed plugin), so the statusbar only needs its id
    // — to close it on the toggle pipe — and to re-open it (per-worktree) when
    // its tab (re)loads. No reconcile/`next_swap_layout` involvement.
    files_id: Option<u32>,         // drawer pane id in THIS tab (None ⇒ closed)
    my_tab_index: Option<usize>,   // manifest key of the tab holding our own pane
    active_tab_pos: Option<usize>, // position of the session's active tab
    session: Option<String>,       // current session name (restore `--session` arg)
    restore_poked: bool,           // restore already requested for this activation
    focused_pane_command: Option<String>, // The command running in the focused pane (e.g. lazygit, hx)
    custom_hints: std::collections::BTreeMap<String, Vec<(String, String)>>,
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
            EventType::Key,
            EventType::SessionUpdate, // session name for the drawer restore poke
            EventType::RunCommandResult,
            EventType::PermissionRequestResult,
        ]);
        // Selectable so Super+Alt+Down can focus the bottom bar and route keys.
        set_selectable(true);
        fetch_theme();
        fetch_hints();
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
                let old_cmd = self.focused_pane_command.clone();
                self.scan_panes(&manifest);
                self.reconcile();
                self.maybe_restore();
                if self.focused_pane_command != old_cmd {
                    return true;
                }
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
            Event::RunCommandResult(code, stdout, _, ctx)
                if ctx.get("cmd").map(|s| s.as_str()) == Some("hints") =>
            {
                if code == Some(0) {
                    if let Ok(map) = serde_json::from_slice::<
                        std::collections::BTreeMap<String, Vec<(String, String)>>,
                    >(&stdout)
                    {
                        self.custom_hints = map;
                        return true;
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
                let active = tabs.iter().find(|t| t.active);
                self.active_tab = active.map(|t| t.position);
                self.active_tab_pos = active.map(|t| t.position);
                // Needed to spawn the palette with the focused worktree's cwd
                // (`superzej menu --tab <name>` resolves the tree from the DB);
                // also the drawer restore `--tab` arg when a worktree tab loads.
                self.active_tab_name = active.map(|t| t.name.clone());
                // A tab may have just become active — reconcile its chrome now
                // (relayout is deferred while a tab is in the background, since
                // only the active tab may call next_swap_layout). See reconcile().
                self.reconcile();
                // `{slug}/home` => home tab; anything else under a repo is a worktree.
                let wt = active
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
            Event::Key(key) => self.on_key(key),
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
            // Super+Alt+Down: select the bottom bar. Broadcast hits every per-tab
            // instance; only the active tab's responds (else focus_plugin_pane
            // would teleport to a background tab). Highlight-only for now.
            "superzej_select_bottombar" => {
                if self.my_tab.is_some() && self.my_tab == self.active_tab {
                    self.selected = true;
                    if let Some(id) = self.my_id {
                        focus_plugin_pane(id, false, false);
                    }
                    return true;
                }
            }
            // Super+K: toggle the command palette. Broadcast hits every per-tab
            // instance; only the active tab's acts (the palette is a floating
            // pane on the focused tab — a background instance would open/close it
            // on the wrong tab).
            "superzej_toggle_palette" => {
                if self.my_tab.is_some() && self.my_tab == self.active_tab {
                    self.toggle_palette();
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

        // Bottom-bar selected (Super+Alt+Down): a leading accent block as a cue.
        if self.selected {
            push_raw(
                &mut out,
                &mut col,
                cols,
                &format!("\u{1b}[1m\u{1b}[38;2;{accent}m\u{2590}\u{1b}[0m\u{1b}[48;2;{BG1}m"),
                1,
            );
        }

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

        for (i, (key, label)) in chips.into_iter().enumerate() {
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

/// Kick off `superzej hints`; the custom hints land via RunCommandResult.
fn fetch_hints() {
    let mut ctx = BTreeMap::new();
    ctx.insert("cmd".to_string(), "hints".to_string());
    run_command(&["superzej", "hints"], ctx);
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
    /// Open the command palette if closed, close it if open (the Super+K toggle).
    /// Closed/open is read from the manifest (`palette_id`); the pane closes on
    /// exit, so picking an action or pressing Esc clears it without a toggle.
    fn toggle_palette(&mut self) {
        if let Some(id) = self.palette_id.take() {
            close_terminal_pane(id);
            return;
        }
        // Spawn as a floating, close-on-exit pane named so we can find it again.
        // `--tab` lets `superzej menu` chdir into the focused worktree (a
        // plugin-spawned pane doesn't inherit the focused pane's cwd, which the
        // worktree-scoped actions + file/grep sources need).
        let mut argv = vec![
            "zellij",
            "run",
            "--floating",
            "--width",
            "80%",
            "--height",
            "80%",
            "--close-on-exit",
            "--name",
            PALETTE_PANE_NAME,
            "--",
            "superzej",
            "menu",
        ];
        if let Some(tab) = self.active_tab_name.as_deref() {
            argv.push("--tab");
            argv.push(tab);
        }
        run_command(&argv, BTreeMap::new());
    }

    fn on_key(&mut self, key: KeyWithModifier) -> bool {
        // Reserved: Enter has no action yet. Esc (or moving focus away) drops the
        // selection and hands focus back to the center terminal.
        match key.bare_key {
            BareKey::Esc => {
                self.selected = false;
                if let Some(id) = self.center_id {
                    focus_terminal_pane(id, false, false);
                }
                true
            }
            _ => false,
        }
    }

    /// Discover the sidebar/panel pane ids in this tab and sync their live
    /// suppression from the manifest. The width-driven auto-hide is decided in
    /// `render` (zellij fires render on every resize but only sometimes a
    /// PaneUpdate), so this just keeps ids and the `suppressed` mirror fresh.
    fn scan_panes(&mut self, manifest: &PaneManifest) {
        let Some(me) = self.my_id else { return };
        // Only the layer (tab) that holds our own pane — other tabs carry the
        // same plugin urls under different ids. The map key is the tab position,
        // which tells us which tab this instance lives on (for the active-tab gate).
        let Some((tab_pos, panes)) = manifest
            .panes
            .iter()
            .find(|(_, ps)| ps.iter().any(|p| p.is_plugin && p.id == me))
        else {
            return;
        };
        self.my_tab = Some(*tab_pos);
        self.my_tab_index = Some(*tab_pos);
        // Drop the bottom-bar selection once focus leaves this pane.
        if self.selected
            && !panes
                .iter()
                .any(|p| p.is_plugin && p.id == me && p.is_focused)
        {
            self.selected = false;
        }
        // Re-derive the open palette each scan (None once its pane is gone — the
        // palette closes on exit, so this clears when the user picks/dismisses).
        let mut palette = None;
        // The drawer is a non-plugin (command) pane named FILES_PANE; track its
        // id so the close pipe can target it, and so restore knows it's open.
        // Absent ⇒ closed (e.g. the user quit yazi, or it was never opened).
        let mut files_id = None;
        self.focused_pane_command = None;
        for p in panes {
            // The open command palette: the floating pane we spawn as
            // `--name superzej-palette` (see `toggle_palette`).
            if !p.is_plugin && p.is_floating && p.title.contains(PALETTE_PANE_NAME) {
                palette = Some(p.id);
            }
            // The center terminal to hand focus back to on Esc: the focused one,
            // else the first (kept across our own focus grab).
            if !p.is_plugin
                && !p.is_floating
                && !p.is_suppressed
                && (p.is_focused || self.center_id.is_none())
            {
                self.center_id = Some(p.id);
                if p.is_focused {
                    self.focused_pane_command = Some(p.title.clone());
                }
            }
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
        self.palette_id = palette;
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
                let tab = self.active_tab_name.clone().unwrap_or_default();
                let mut ctx = BTreeMap::new();
                ctx.insert("cmd".to_string(), "files_restore".to_string());
                run_command(
                    &[
                        "superzej",
                        "files",
                        "--restore",
                        "--tab",
                        &tab,
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
        // Only the ACTIVE tab's statusbar may relayout. `next_swap_layout()`
        // (and add/hide pane) act on the FOCUSED tab, but the toggle keybind
        // broadcasts to every tab's statusbar instance — so a background
        // instance firing it would mutate the visible tab, cycling its swap
        // layout once per open tab and leaving a surface jammed at a ~50% split
        // (the bug that surfaced after a manual drag with several tabs open).
        // Background tabs defer; each reconciles when it becomes active (the
        // TabUpdate handler calls reconcile() on the new active tab).
        if self.my_tab.is_none() || self.my_tab != self.active_tab {
            return;
        }
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
    fn chips(&self, mode: InputMode) -> Vec<(String, String)> {
        match mode {
            InputMode::Normal => {
                let mut v = vec![
                    ("Cmd-K".to_string(), "menu".to_string()),
                    ("A-←→".to_string(), "tabs".to_string()),
                    ("S-A-←→".to_string(), "panes".to_string()),
                    ("A-W".to_string(), "new repo".to_string()),
                    ("A-w".to_string(), "worktree".to_string()),
                    ("A-n".to_string(), "split".to_string()),
                ];

                if let Some(cmd) = &self.focused_pane_command {
                    let cmd_lower = cmd.to_lowercase();
                    for (tool_name, hints) in &self.custom_hints {
                        if cmd_lower.contains(&tool_name.to_lowercase()) {
                            v.extend(hints.clone());
                            return v;
                        }
                    }
                }

                if self.worktree_ctx {
                    v.extend_from_slice(&[
                        ("A-g".to_string(), "lazygit".to_string()),
                        ("A-e".to_string(), "edit".to_string()),
                        ("A-X".to_string(), "close".to_string()),
                    ]);
                } else {
                    v.extend_from_slice(&[
                        ("A-o".to_string(), "switch repo".to_string()),
                        ("A-d".to_string(), "dashboard".to_string()),
                    ]);
                }
                v
            }
            InputMode::Pane => vec![
                ("→".to_string(), "right".to_string()),
                ("↓".to_string(), "down".to_string()),
                ("x".to_string(), "close".to_string()),
                ("f".to_string(), "fullscreen".to_string()),
                ("w".to_string(), "float".to_string()),
                ("z".to_string(), "frames".to_string()),
                ("⏎".to_string(), "done".to_string()),
            ],
            InputMode::Tab => vec![
                ("n".to_string(), "new".to_string()),
                ("x".to_string(), "close".to_string()),
                ("←→".to_string(), "move".to_string()),
                ("r".to_string(), "rename".to_string()),
                ("⏎".to_string(), "done".to_string()),
            ],
            InputMode::Resize => vec![
                ("←↑↓→".to_string(), "resize".to_string()),
                ("+-".to_string(), "size".to_string()),
                ("⏎".to_string(), "done".to_string()),
            ],
            InputMode::Move => vec![
                ("←↑↓→".to_string(), "move pane".to_string()),
                ("⏎".to_string(), "done".to_string()),
            ],
            InputMode::Scroll => vec![
                ("↑↓".to_string(), "scroll".to_string()),
                ("/".to_string(), "search".to_string()),
                ("e".to_string(), "edit".to_string()),
                ("⏎".to_string(), "done".to_string()),
            ],
            InputMode::Session => vec![
                ("d".to_string(), "detach".to_string()),
                ("w".to_string(), "sessions".to_string()),
                ("⏎".to_string(), "done".to_string()),
            ],
            InputMode::Locked => vec![("Ctrl-g".to_string(), "unlock".to_string())],
            _ => vec![
                ("⏎".to_string(), "done".to_string()),
                ("Esc".to_string(), "cancel".to_string()),
            ],
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_restore_decision_disarm() {
        let dec = restore_decision(Some(1), Some(2), true, false, false, true);
        assert!(matches!(dec, RestoreDecision::Disarm));
    }

    #[test]
    fn test_restore_decision_skip_missing() {
        let dec = restore_decision(None, Some(1), true, false, false, true);
        assert!(matches!(dec, RestoreDecision::Skip));
        let dec2 = restore_decision(Some(1), None, true, false, false, true);
        assert!(matches!(dec2, RestoreDecision::Skip));
    }

    #[test]
    fn test_restore_decision_skip_conditions() {
        // not worktree
        let dec = restore_decision(Some(1), Some(1), false, false, false, true);
        assert!(matches!(dec, RestoreDecision::Skip));
        // files open
        let dec = restore_decision(Some(1), Some(1), true, true, false, true);
        assert!(matches!(dec, RestoreDecision::Skip));
        // already poked
        let dec = restore_decision(Some(1), Some(1), true, false, true, true);
        assert!(matches!(dec, RestoreDecision::Skip));
        // no session
        let dec = restore_decision(Some(1), Some(1), true, false, false, false);
        assert!(matches!(dec, RestoreDecision::Skip));
    }

    #[test]
    fn test_restore_decision_fire() {
        let dec = restore_decision(Some(1), Some(1), true, false, false, true);
        assert!(matches!(dec, RestoreDecision::Fire));
    }

    #[test]
    fn test_parse_rgb_line() {
        assert_eq!(
            parse_rgb_line(b"255;0;128\n"),
            Some("255;0;128".to_string())
        );
        assert_eq!(
            parse_rgb_line(b" 255;0;128 \n"),
            Some("255;0;128".to_string())
        );
        // Invalid
        assert_eq!(parse_rgb_line(b"255;0\n"), None);
        assert_eq!(parse_rgb_line(b"255;0;128;50\n"), None);
        assert_eq!(parse_rgb_line(b"abc;0;128\n"), None);
    }

    #[test]
    fn test_mode_name() {
        assert_eq!(mode_name(InputMode::Normal), "MODE");
        assert_eq!(mode_name(InputMode::Pane), "PANE");
        assert_eq!(mode_name(InputMode::RenamePane), "RENAME PANE");
    }

    #[test]
    fn test_push_raw() {
        let mut out = String::new();
        let mut col = 0;
        let cols = 10;

        // fits
        push_raw(&mut out, &mut col, cols, "hello", 5);
        assert_eq!(out, "hello");
        assert_eq!(col, 5);

        // fits exactly
        push_raw(&mut out, &mut col, cols, "world", 5);
        assert_eq!(out, "helloworld");
        assert_eq!(col, 10);

        // already full
        push_raw(&mut out, &mut col, cols, "!", 1);
        assert_eq!(out, "helloworld");
        assert_eq!(col, 10);
    }

    #[test]
    fn test_push_raw_clips() {
        let mut out = String::new();
        let mut col = 0;
        let cols = 5;

        push_raw(&mut out, &mut col, cols, "abc", 3);
        // Next piece is length 3, only 2 slots remain, so it doesn't push
        push_raw(&mut out, &mut col, cols, "def", 3);
        assert_eq!(out, "abc");
        assert_eq!(col, 5); // col gets clamped to cols
    }
}
