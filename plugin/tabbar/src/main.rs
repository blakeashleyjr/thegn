//! superzej tabbar — a thin, centered strip of the **focused worktree's**
//! tabs (its `·N` pages from `superzej new-tab`). Tabs are named
//! `{repo_slug}/{branch}[ ·N]` (all repos share one session); this strip
//! shows one chip per page of the focused tab's worktree (`1`, `·2`, `·3`,
//! …) — switching worktrees/repos is the sidebar's job. It replaces zellij's
//! built-in `tab-bar` so there is no "Zellij (session)" wordmark and no
//! swap-layout ("BASE") indicator. The active page is a filled cyan chip;
//! clicking/hovering targets it.
//!
//! It's a borderless 1-row strip spanning the **full width** along the very top
//! of the session layout, above the three framed boxes (sidebar | center |
//! panel). It carries the `✦ superzej` title at the far left and centers the
//! page chips over the **center column** (read from the pane manifest, so they
//! stay centered even when the sidebar/panel fold on a narrow terminal).

use std::collections::BTreeMap;
use zellij_tile::prelude::*;

mod theme;
use theme::{BG0, DIM, FAINT, GHOST, MAGENTA, PANEL, PANEL2, RESET, TEAL, TEXT, fg};

#[derive(Default)]
struct State {
    tabs: Vec<Tab>,
    accent: String, // "R;G;B" from `superzej theme` (TEAL until it lands)
    // The focused tab's (repo, worktree base): only its pages are shown.
    active_wt: Option<(String, String)>,
    my_id: Option<u32>,
    // The center column's horizontal span (absolute cols), from the manifest —
    // the tab chips are centered over it (not the full-width bar). 0 width =
    // unknown yet, fall back to centering across the whole bar.
    center_x: usize,
    center_w: usize,
    hover: Option<usize>,
    // Clickable column spans for each tab, cached from the last render:
    // (start_col, end_col_exclusive, tab_position).
    spans: Vec<(usize, usize, usize)>,
    // For the `superzej_new_tab` pipe: the session name, passed to the binary
    // because plugin-spawned commands can't rely on env/cwd. (A keybind
    // MessagePlugin broadcast hits every per-tab instance, and zellij starves
    // background-tab plugins of Tab/Pane updates while still delivering
    // pipes — so no instance-side "am I focused" guard can be trusted. Every
    // instance fires; `superzej new-tab` resolves the focused tab itself via
    // dump-layout and dedupes concurrent invocations with a lockfile.)
    session: Option<String>,
    // System stats for the far-right widget, polled from `superzej stats` on a
    // timer. Each field is independent; a `None`/empty value drops it from the
    // strip (e.g. `gpu` on a box with no readable GPU counter).
    cpu: Option<u8>,
    mem: Option<u8>,
    gpu: Option<u8>,
    time: String,
    // Keyboard selection of a stat segment, indexing the *present* segments in
    // CPU→MEM→GPU order (GPU drops on boxes with no counter). `Some` means the
    // top bar is "selected": Super+Alt+Up focuses this pane and sets it, plain
    // ←/→ (h/l) move it, Enter opens the matching monitor, Esc clears it.
    sel: Option<u8>,
    // Broadcast guard: the Super+Alt+Up keybind hits every per-tab instance, so
    // only the one whose tab is active acts (mirrors the sidebar) — otherwise a
    // background instance's focus_plugin_pane would teleport to its tab.
    my_tab: Option<usize>,
    active_tab: Option<usize>,
    // The center terminal to split the monitor under (the last-focused center
    // pane in this tab), so the embedded monitor lands at the bottom of the
    // center column rather than under the top strip.
    center_id: Option<u32>,
}

struct Tab {
    repo: String, // `{slug}` prefix (the repo this tab belongs to)
    base: String, // worktree base (branch with any ` ·N` page suffix stripped)
    page: u32,    // page number (1 = the base tab)
    position: usize,
    active: bool,
}

/// Split a `{repo_slug}/{branch}` tab name into (repo, branch). A name with no
/// `/` (legacy / ad-hoc tab) groups under repo "" and shows verbatim.
fn split_tab(name: &str) -> (String, String) {
    match name.split_once('/') {
        Some((r, b)) => (r.to_string(), b.to_string()),
        None => (String::new(), name.to_string()),
    }
}

/// Split a branch part into (worktree base, page): `"x ·2"` → `("x", 2)`,
/// `"x"` → `("x", 1)`. Mirrors the binary's `strip_page_suffix` (the suffix
/// counts only when all digits).
fn split_page(branch: &str) -> (String, u32) {
    if let Some((base, suffix)) = branch.rsplit_once(" \u{b7}") {
        if !suffix.is_empty() && suffix.chars().all(|c| c.is_ascii_digit()) {
            if let Ok(n) = suffix.parse() {
                return (base.to_string(), n);
            }
        }
    }
    (branch.to_string(), 1)
}

register_plugin!(State);

impl ZellijPlugin for State {
    fn load(&mut self, _config: BTreeMap<String, String>) {
        self.accent = TEAL.into();
        request_permission(&[
            PermissionType::ReadApplicationState,   // Tab/Session updates
            PermissionType::ChangeApplicationState, // switch tabs
            PermissionType::RunCommands,            // `superzej new-tab` pipe + theme
            PermissionType::ReadCliPipes,           // unblock CLI pipes
        ]);
        self.my_id = Some(get_plugin_ids().plugin_id);
        subscribe(&[
            EventType::TabUpdate,
            EventType::SessionUpdate,
            EventType::PaneUpdate,
            EventType::Mouse,
            EventType::Key,
            EventType::RunCommandResult,
            EventType::PermissionRequestResult,
            EventType::Timer,
        ]);
        set_selectable(true);
        fetch_theme();
        fetch_stats();
        set_timeout(STATS_SECS);
    }

    fn update(&mut self, event: Event) -> bool {
        match event {
            // The load()-time fetch races the permission grant; re-pull once
            // permissions actually land.
            Event::PermissionRequestResult(_) => {
                fetch_theme();
                fetch_stats();
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
                if ctx.get("cmd").map(|s| s.as_str()) == Some("stats") =>
            {
                // Repaint only when a value actually changed (the timer fires
                // every few seconds regardless).
                code == Some(0) && self.update_stats(&stdout)
            }
            // Re-poll stats and re-arm the timer.
            Event::Timer(_) => {
                fetch_stats();
                set_timeout(STATS_SECS);
                false
            }
            Event::TabUpdate(tabs) => {
                self.active_tab = tabs.iter().find(|t| t.active).map(|t| t.position);
                self.active_wt = tabs.iter().find(|t| t.active).map(|t| {
                    let (repo, branch) = split_tab(&t.name);
                    (repo, split_page(&branch).0)
                });
                self.tabs = tabs
                    .into_iter()
                    .map(|t| {
                        let raw = if t.name.is_empty() {
                            format!("tab {}", t.position + 1)
                        } else {
                            t.name
                        };
                        let (repo, branch) = split_tab(&raw);
                        let (base, page) = split_page(&branch);
                        Tab {
                            repo,
                            base,
                            page,
                            position: t.position,
                            active: t.active,
                        }
                    })
                    .collect();
                // Page order within the strip (positions can interleave when
                // pages were opened from different tabs).
                self.tabs.sort_by_key(|t| t.page);
                true
            }
            Event::SessionUpdate(infos, _resurrectable) => {
                self.session = infos
                    .iter()
                    .find(|s| s.is_current_session)
                    .map(|s| s.name.clone());
                false
            }
            // Track the center column's span so the tab chips center over the
            // terminals, not the full-width bar. The center is the tiled,
            // non-plugin pane(s) in our tab; their bounding x-range works
            // whether or not the sidebar/panel are folded.
            Event::PaneUpdate(manifest) => {
                let Some(me) = self.my_id else { return false };
                let Some((tab_pos, panes)) = manifest
                    .panes
                    .iter()
                    .find(|(_, ps)| ps.iter().any(|p| p.is_plugin && p.id == me))
                else {
                    return false;
                };
                self.my_tab = Some(*tab_pos);
                let mut changed = false;
                // Drop the stat selection once focus leaves the top bar (e.g.
                // the user moved focus away with Super+Alt+Left) so a stale
                // highlight doesn't linger.
                let me_focused = panes
                    .iter()
                    .any(|p| p.is_plugin && p.id == me && p.is_focused);
                if !me_focused && self.sel.is_some() {
                    self.sel = None;
                    changed = true;
                }
                let centers: Vec<&PaneInfo> = panes
                    .iter()
                    .filter(|p| !p.is_plugin && !p.is_floating && !p.is_suppressed)
                    .collect();
                // Remember the center terminal to split the monitor under: the
                // focused center pane when there is one, else keep the last
                // known (so grabbing top-bar focus doesn't lose it), falling
                // back to the first center pane.
                if let Some(f) = centers.iter().find(|p| p.is_focused) {
                    self.center_id = Some(f.id);
                } else if self.center_id.is_none() {
                    self.center_id = centers.first().map(|p| p.id);
                }
                if let (Some(left), Some(right)) = (
                    centers.iter().map(|p| p.pane_x).min(),
                    centers.iter().map(|p| p.pane_x + p.pane_columns).max(),
                ) {
                    let (x, w) = (left, right - left);
                    if (x, w) != (self.center_x, self.center_w) {
                        self.center_x = x;
                        self.center_w = w;
                        changed = true;
                    }
                }
                changed
            }
            Event::Mouse(Mouse::Hover(_line, col)) => {
                let idx = self.col_to_index(col);
                if self.hover != idx {
                    self.hover = idx;
                    return true;
                }
                false
            }
            Event::Mouse(Mouse::LeftClick(_line, col)) => {
                match self.col_to_index(col) {
                    // The trailing `+` chip: a new page on this worktree.
                    Some(NEW_PAGE) => {
                        if let Some(s) = self.session.clone() {
                            run_command(&["superzej", "new-tab", "--session", &s], BTreeMap::new());
                        }
                    }
                    // switch_tab_to is 1-indexed.
                    Some(pos) => switch_tab_to(pos as u32 + 1),
                    None => {}
                }
                false
            }
            // Keys only arrive while this pane is focused (Super+Alt+Up focuses
            // it via the `superzej_select_topbar` pipe). Drive the stat cursor.
            Event::Key(key) => self.on_key(key),
            _ => false,
        }
    }

    fn pipe(&mut self, pipe: PipeMessage) -> bool {
        // CLI pipes block until unblocked and send a trailing payload-less
        // message on stdin EOF — unblock both, act only on the first (same
        // guard as the sidebar/panel).
        if let PipeSource::Cli(id) = &pipe.source {
            unblock_cli_pipe_input(id);
            if pipe.payload.is_none() {
                return false;
            }
        }
        // Alt+t / tab-mode `n`: open a second full-chrome tab on the focused
        // worktree. Run via the plugin (no spawned command pane, no floating
        // flash). Every per-tab instance fires; the binary resolves the focused
        // tab from dump-layout (always fresh) and a lockfile collapses the
        // concurrent invocations to one tab.
        if pipe.name == "superzej_new_tab" {
            if let Some(s) = self.session.clone() {
                run_command(&["superzej", "new-tab", "--session", &s], BTreeMap::new());
            }
        }
        // Super+Alt+Up: select the top bar. The broadcast hits every per-tab
        // instance; only the active tab's responds (else focus_plugin_pane would
        // teleport to a background tab). Focus this pane so plain ←/→/Enter/Esc
        // land here, and seed the cursor on the first stat segment.
        if pipe.name == "superzej_select_topbar"
            && self.my_tab.is_some()
            && self.my_tab == self.active_tab
            && !self.stat_kinds().is_empty()
        {
            self.sel = Some(0);
            if let Some(id) = self.my_id {
                focus_plugin_pane(id, false, false);
            }
            return true;
        }
        false
    }

    fn render(&mut self, _rows: usize, cols: usize) {
        const STAR: &str = "\u{2726}"; // ✦
        let version = concat!("v", env!("CARGO_PKG_VERSION"));
        let accent = self.accent.clone();
        let bar = PANEL; // the grey top-bar fill — clearly lighter than the base bg
        // Re-applied after every RESET so spaces/chips keep the bar background.
        let set_bar = format!("\u{1b}[48;2;{bar}m");

        let mut out = String::new();
        let mut col = 0usize;
        self.spans.clear();
        out.push_str(&set_bar);

        // ── Title at the far left (moved here from the sidebar) ──────────────
        // " ✦ superzej v0.1.0", magenta star + accent name. Dropped only if the
        // bar is too narrow to hold it.
        let title = format!("superzej {version}");
        let title_w = 1 + 1 + 1 + title.chars().count(); // space + star + space + text
        if title_w + 4 <= cols {
            out.push(' ');
            out.push_str(&format!(
                "\u{1b}[1m\u{1b}[38;2;{MAGENTA}m{STAR}\u{1b}[0m{set_bar} \u{1b}[1m\u{1b}[38;2;{accent}m{title}\u{1b}[0m{set_bar}"
            ));
            col += title_w;
        }
        let title_end = col;

        // ── Tab chips, centered over the CENTER column ──────────────────────
        // Only the focused worktree's pages are shown: ` 1 ` / ` ·N ` + ` + `.
        let visible: Vec<usize> = self
            .tabs
            .iter()
            .enumerate()
            .filter(|(_, t)| {
                self.active_wt
                    .as_ref()
                    .is_none_or(|(r, b)| t.repo == *r && t.base == *b)
            })
            .map(|(i, _)| i)
            .collect();
        let labels: Vec<String> = visible
            .iter()
            .map(|&i| {
                let t = &self.tabs[i];
                if t.page == 1 {
                    " 1 ".to_string()
                } else {
                    format!(" \u{b7}{} ", t.page)
                }
            })
            .collect();
        let sep = 1usize; // one blank cell between chips
        let plus_w = 3usize + sep; // trailing ` + ` new-page chip
        let total: usize = labels.iter().map(|l| l.chars().count()).sum::<usize>()
            + sep * labels.len().saturating_sub(1)
            + plus_w;

        // Center within the center column's span (fallback: the whole bar).
        let (cx, cw) = if self.center_w > 0 {
            (self.center_x, self.center_w)
        } else {
            (0, cols)
        };
        let mut start = cx + cw.saturating_sub(total) / 2;
        if start < title_end + 1 {
            start = title_end + 1; // never overlap the title
        }
        while col < start && col < cols {
            out.push(' ');
            col += 1;
        }

        for (j, label) in labels.iter().enumerate() {
            if col >= cols {
                break;
            }
            let i = visible[j];
            let w = label.chars().count();
            let chip_start = col;
            let active = self.tabs[i].active;
            let hovered = self.hover == Some(self.tabs[i].position);
            if active {
                out.push_str(&format!("\u{1b}[1m\u{1b}[38;2;{BG0}m\u{1b}[48;2;{accent}m"));
            } else if hovered {
                out.push_str(&format!(
                    "\u{1b}[1m\u{1b}[38;2;{TEXT}m\u{1b}[48;2;{PANEL2}m"
                ));
            } else {
                out.push_str(&format!("\u{1b}[38;2;{DIM}m"));
            }
            out.push_str(label);
            out.push_str(RESET);
            out.push_str(&set_bar);
            self.spans
                .push((chip_start, chip_start + w, self.tabs[i].position));
            col += w;
            if j + 1 < labels.len() {
                out.push(' ');
                col += 1;
            }
        }

        // Trailing ` + ` chip: a new page on this worktree (same as Alt+t).
        if col + plus_w <= cols {
            out.push(' ');
            col += 1;
            let chip_start = col;
            if self.hover == Some(NEW_PAGE) {
                out.push_str(&format!(
                    "\u{1b}[1m\u{1b}[38;2;{accent}m + \u{1b}[0m{set_bar}"
                ));
            } else {
                out.push_str(&format!("\u{1b}[38;2;{FAINT}m + \u{1b}[0m{set_bar}"));
            }
            self.spans.push((chip_start, chip_start + 3, NEW_PAGE));
            col += 3;
        }

        // ── System-stats widget, pinned to the far right ───────────────────
        // The chips center over the (left-of-panel) center column, so the
        // widget's right-edge slot doesn't collide with them at normal widths.
        // On a terminal too narrow to fit both, the chips win and the widget is
        // dropped rather than overlapped.
        let (widget, ww) = self.stats_widget();
        let right_start = cols.saturating_sub(ww);
        if ww > 0 && col <= right_start {
            while col < right_start {
                out.push(' ');
                col += 1;
            }
            out.push_str(&set_bar); // keep the bar background under the widget
            out.push_str(&widget);
        } else {
            while col < cols {
                out.push(' ');
                col += 1;
            }
        }
        out.push_str(RESET);
        print!("{out}");
    }
}

/// Sentinel "tab position" for the trailing `+` (new page) chip.
const NEW_PAGE: usize = usize::MAX;

/// How often to re-poll `superzej stats` for the right-hand widget.
const STATS_SECS: f64 = 2.0;

/// Kick off `superzej theme`; the accent lands via RunCommandResult.
fn fetch_theme() {
    let mut ctx = BTreeMap::new();
    ctx.insert("cmd".to_string(), "theme".to_string());
    run_command(&["superzej", "theme"], ctx);
}

/// Kick off `superzej stats`; the values land via RunCommandResult.
fn fetch_stats() {
    let mut ctx = BTreeMap::new();
    ctx.insert("cmd".to_string(), "stats".to_string());
    run_command(&["superzej", "stats"], ctx);
}

/// First line of stdout as a validated "R;G;B" triple.
fn parse_rgb_line(stdout: &[u8]) -> Option<String> {
    let s = String::from_utf8_lossy(stdout);
    let line = s.lines().next()?.trim();
    let ok = line.split(';').filter(|p| p.parse::<u8>().is_ok()).count() == 3;
    ok.then(|| line.to_string())
}

/// Next stat cursor after moving right (`true`) or left (`false`), clamped to
/// `[0, n)`. From `None`, either direction lands on the first segment; with no
/// segments (`n == 0`) there is nothing to select.
fn step_sel(cur: Option<u8>, n: usize, right: bool) -> Option<u8> {
    if n == 0 {
        return None;
    }
    let last = (n - 1) as u8;
    Some(match cur {
        None => 0,
        Some(s) if right => (s + 1).min(last),
        Some(s) => s.saturating_sub(1),
    })
}

impl State {
    /// Which tab position (if any) sits under viewport column `col`.
    fn col_to_index(&self, col: usize) -> Option<usize> {
        self.spans
            .iter()
            .find(|(s, e, _)| col >= *s && col < *e)
            .map(|(_, _, pos)| *pos)
    }

    /// The stat segments actually present, in display order — `sel` indexes into
    /// this (GPU drops on boxes with no readable counter).
    fn stat_kinds(&self) -> Vec<&'static str> {
        let mut v = Vec::new();
        if self.cpu.is_some() {
            v.push("cpu");
        }
        if self.mem.is_some() {
            v.push("mem");
        }
        if self.gpu.is_some() {
            v.push("gpu");
        }
        v
    }

    /// Drive the stat cursor while the top bar is selected (this pane focused).
    /// ←/h and →/l move it (clamped); Enter opens the monitor; Esc cancels.
    fn on_key(&mut self, key: KeyWithModifier) -> bool {
        let n = self.stat_kinds().len();
        if n == 0 {
            return false;
        }
        // The stat cursor responds to PLAIN keys only (←/→/h/l/Enter/Esc). A
        // modifier-carrying chord that leaked here (rather than firing its own
        // keybind) must not move the cursor — mirrors the sidebar guard.
        if !key.key_modifiers.is_empty() {
            return false;
        }
        match key.bare_key {
            BareKey::Left | BareKey::Char('h') => {
                self.sel = step_sel(self.sel, n, false);
                true
            }
            BareKey::Right | BareKey::Char('l') => {
                self.sel = step_sel(self.sel, n, true);
                true
            }
            BareKey::Enter => {
                self.activate();
                true
            }
            BareKey::Esc => {
                self.sel = None;
                self.refocus_center();
                true
            }
            _ => false,
        }
    }

    /// The stat kind under the cursor, if any.
    fn selected_kind(&self) -> Option<&'static str> {
        self.sel
            .and_then(|i| self.stat_kinds().get(i as usize).copied())
    }

    /// Open the monitor for the selected stat, then drop the selection and hand
    /// focus back to the center terminal. `superzej monitor` opens the monitor
    /// as a FLOATING pane (it overlays the center rather than reflowing the
    /// chrome), so refocusing the center just restores a sensible focus target
    /// for when the float closes.
    fn activate(&mut self) {
        let kind = self.selected_kind();
        self.sel = None;
        let Some(kind) = kind else { return };
        self.refocus_center();
        let mut ctx = BTreeMap::new();
        ctx.insert("cmd".to_string(), "monitor".to_string());
        run_command(&["superzej", "monitor", kind], ctx);
    }

    /// Return focus to the tracked center terminal, if any.
    fn refocus_center(&self) {
        if let Some(id) = self.center_id {
            focus_terminal_pane(id, false, false);
        }
    }

    /// Parse a `cpu=NN mem=NN gpu=NN time=HH:MM` line from `superzej stats`
    /// into the widget fields. Returns whether any value changed (so the caller
    /// can skip a no-op repaint on an unchanged tick).
    fn update_stats(&mut self, stdout: &[u8]) -> bool {
        let s = String::from_utf8_lossy(stdout);
        let line = s.lines().next().unwrap_or("");
        let (mut cpu, mut mem, mut gpu, mut time) = (None, None, None, String::new());
        for tok in line.split_whitespace() {
            let Some((k, v)) = tok.split_once('=') else {
                continue;
            };
            match k {
                "cpu" => cpu = v.parse().ok(),
                "mem" => mem = v.parse().ok(),
                "gpu" => gpu = v.parse().ok(),
                "time" => time = v.to_string(),
                _ => {}
            }
        }
        let changed = (cpu, mem, gpu) != (self.cpu, self.mem, self.gpu) || time != self.time;
        self.cpu = cpu;
        self.mem = mem;
        self.gpu = gpu;
        self.time = time;
        changed
    }

    /// The far-right stats strip and its display width (0 when no data has
    /// arrived yet). Labels are FAINT, values TEXT, the clock the accent, joined
    /// by a GHOST "·"; one cell of padding on each end.
    fn stats_widget(&self) -> (String, usize) {
        // The selected segment is a filled accent chip (BG0 on accent, bold),
        // then RESET back to the grey bar fill so the separator/clock keep it.
        let set_bar = format!("\u{1b}[48;2;{PANEL}m");
        let metric = |label: &str, v: u8, selected: bool| -> (String, usize) {
            let val = format!("{v}%");
            let width = label.chars().count() + 1 + val.chars().count();
            let s = if selected {
                format!(
                    "\u{1b}[1m\u{1b}[38;2;{BG0}m\u{1b}[48;2;{}m{label} {val}{RESET}{set_bar}",
                    self.accent
                )
            } else {
                format!("{}{label} {}{val}", fg(FAINT), fg(TEXT))
            };
            (s, width)
        };
        let mut parts: Vec<(String, usize)> = Vec::new();
        let mut si: u8 = 0; // stat-segment index, for matching self.sel
        if let Some(c) = self.cpu {
            parts.push(metric("CPU", c, self.sel == Some(si)));
            si += 1;
        }
        if let Some(m) = self.mem {
            parts.push(metric("MEM", m, self.sel == Some(si)));
            si += 1;
        }
        if let Some(g) = self.gpu {
            parts.push(metric("GPU", g, self.sel == Some(si)));
            si += 1;
        }
        let _ = si;
        if !self.time.is_empty() {
            let w = self.time.chars().count();
            parts.push((format!("{}{}", fg(&self.accent), self.time), w));
        }
        if parts.is_empty() {
            return (String::new(), 0);
        }
        let sep = format!(" {}\u{b7} ", fg(GHOST)); // " · ", 3 cells
        let mut out = String::from(" "); // leading pad
        let mut width = 1usize;
        for (i, (rendered, w)) in parts.iter().enumerate() {
            if i > 0 {
                out.push_str(&sep);
                width += 3;
            }
            out.push_str(rendered);
            width += w;
        }
        out.push(' '); // trailing pad
        width += 1;
        (out, width)
    }
}

#[cfg(test)]
mod tests {
    use super::{State, split_page, step_sel};

    #[test]
    fn splits_page_suffixes() {
        assert_eq!(split_page("x \u{b7}2"), ("x".to_string(), 2));
        assert_eq!(split_page("x \u{b7}12"), ("x".to_string(), 12));
        assert_eq!(split_page("x"), ("x".to_string(), 1));
        assert_eq!(split_page("x \u{b7}y"), ("x \u{b7}y".to_string(), 1));
        assert_eq!(split_page("home"), ("home".to_string(), 1));
    }

    fn st(cpu: Option<u8>, mem: Option<u8>, gpu: Option<u8>) -> State {
        State {
            cpu,
            mem,
            gpu,
            ..State::default()
        }
    }

    #[test]
    fn stat_kinds_lists_present_segments_in_order() {
        assert_eq!(
            st(Some(1), Some(2), Some(3)).stat_kinds(),
            ["cpu", "mem", "gpu"]
        );
        // GPU drops on a box with no counter.
        assert_eq!(st(Some(1), Some(2), None).stat_kinds(), ["cpu", "mem"]);
        // Only what's present, preserving order.
        assert_eq!(st(None, None, Some(3)).stat_kinds(), ["gpu"]);
        assert!(st(None, None, None).stat_kinds().is_empty());
    }

    #[test]
    fn step_sel_clamps_at_both_ends() {
        // From None either direction lands on the first segment.
        assert_eq!(step_sel(None, 3, true), Some(0));
        assert_eq!(step_sel(None, 3, false), Some(0));
        // Right advances and clamps at the last index.
        assert_eq!(step_sel(Some(0), 3, true), Some(1));
        assert_eq!(step_sel(Some(2), 3, true), Some(2));
        // Left retreats and clamps at zero.
        assert_eq!(step_sel(Some(2), 3, false), Some(1));
        assert_eq!(step_sel(Some(0), 3, false), Some(0));
        // No segments: nothing selectable.
        assert_eq!(step_sel(Some(0), 0, true), None);
        assert_eq!(step_sel(None, 0, false), None);
    }

    #[test]
    fn selected_kind_maps_cursor_to_present_segment() {
        // All three present: index → kind directly.
        let mut s = st(Some(1), Some(2), Some(3));
        s.sel = Some(0);
        assert_eq!(s.selected_kind(), Some("cpu"));
        s.sel = Some(2);
        assert_eq!(s.selected_kind(), Some("gpu"));
        // GPU absent: index 1 is MEM (cpu/mem only), index 2 is out of range.
        let mut s = st(Some(1), Some(2), None);
        s.sel = Some(1);
        assert_eq!(s.selected_kind(), Some("mem"));
        s.sel = Some(2);
        assert_eq!(s.selected_kind(), None);
        // No selection.
        let mut s = st(Some(1), None, None);
        s.sel = None;
        assert_eq!(s.selected_kind(), None);
    }

    #[test]
    fn cpu_and_mem_both_map_to_the_system_monitor_kind() {
        // The plugin emits the segment kind; `superzej monitor` maps cpu/mem →
        // the system monitor and gpu → the gpu monitor (see config::tests).
        let mut s = st(Some(10), Some(20), Some(30));
        s.sel = Some(0);
        assert_eq!(s.selected_kind(), Some("cpu"));
        s.sel = Some(1);
        assert_eq!(s.selected_kind(), Some("mem"));
    }

    #[test]
    fn highlighted_segment_uses_accent_background() {
        let mut s = st(Some(42), Some(63), Some(7));
        s.accent = "1;2;3".into();
        // Nothing selected: no accent-background fill in the widget.
        let (plain, _) = s.stats_widget();
        assert!(
            !plain.contains("48;2;1;2;3m"),
            "unselected widget should not fill accent bg"
        );
        // Selecting MEM paints it as a bold accent chip (BG0 fg on accent bg).
        s.sel = Some(1);
        let (sel, _) = s.stats_widget();
        assert!(
            sel.contains("48;2;1;2;3m"),
            "selected segment should fill the accent bg"
        );
        assert!(sel.contains("\u{1b}[1m"), "selected segment should be bold");
    }

    #[test]
    fn highlighted_segment_keeps_widget_width() {
        // Selecting a segment must not change the reported width (only styling),
        // or the right-edge layout in render() would shift under selection.
        let mut s = st(Some(42), Some(63), Some(7));
        s.time = "12:34".into();
        let (_, w_plain) = s.stats_widget();
        for i in 0..3 {
            s.sel = Some(i);
            let (_, w_sel) = s.stats_widget();
            assert_eq!(w_sel, w_plain, "width changed when segment {i} selected");
        }
    }
}
