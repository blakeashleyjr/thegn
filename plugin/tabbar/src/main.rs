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
use theme::{BG0, DIM, FAINT, MAGENTA, PANEL, PANEL2, RESET, TEAL, TEXT};

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
            EventType::RunCommandResult,
            EventType::PermissionRequestResult,
        ]);
        set_selectable(true);
        fetch_theme();
    }

    fn update(&mut self, event: Event) -> bool {
        match event {
            // The load()-time fetch races the permission grant; re-pull once
            // permissions actually land.
            Event::PermissionRequestResult(_) => {
                fetch_theme();
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
            Event::TabUpdate(tabs) => {
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
                let Some(panes) = manifest
                    .panes
                    .values()
                    .find(|ps| ps.iter().any(|p| p.is_plugin && p.id == me))
                else {
                    return false;
                };
                let centers: Vec<&PaneInfo> = panes
                    .iter()
                    .filter(|p| !p.is_plugin && !p.is_floating && !p.is_suppressed)
                    .collect();
                if let (Some(left), Some(right)) = (
                    centers.iter().map(|p| p.pane_x).min(),
                    centers.iter().map(|p| p.pane_x + p.pane_columns).max(),
                ) {
                    let (x, w) = (left, right - left);
                    if (x, w) != (self.center_x, self.center_w) {
                        self.center_x = x;
                        self.center_w = w;
                        return true;
                    }
                }
                false
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

        // Fill the rest of the bar with the grey background.
        while col < cols {
            out.push(' ');
            col += 1;
        }
        out.push_str(RESET);
        print!("{out}");
    }
}

/// Sentinel "tab position" for the trailing `+` (new page) chip.
const NEW_PAGE: usize = usize::MAX;

/// Kick off `superzej theme`; the accent lands via RunCommandResult.
fn fetch_theme() {
    let mut ctx = BTreeMap::new();
    ctx.insert("cmd".to_string(), "theme".to_string());
    run_command(&["superzej", "theme"], ctx);
}

/// First line of stdout as a validated "R;G;B" triple.
fn parse_rgb_line(stdout: &[u8]) -> Option<String> {
    let s = String::from_utf8_lossy(stdout);
    let line = s.lines().next()?.trim();
    let ok = line.split(';').filter(|p| p.parse::<u8>().is_ok()).count() == 3;
    ok.then(|| line.to_string())
}

impl State {
    /// Which tab position (if any) sits under viewport column `col`.
    fn col_to_index(&self, col: usize) -> Option<usize> {
        self.spans
            .iter()
            .find(|(s, e, _)| col >= *s && col < *e)
            .map(|(_, _, pos)| *pos)
    }
}

#[cfg(test)]
mod tests {
    use super::split_page;

    #[test]
    fn splits_page_suffixes() {
        assert_eq!(split_page("x \u{b7}2"), ("x".to_string(), 2));
        assert_eq!(split_page("x \u{b7}12"), ("x".to_string(), 12));
        assert_eq!(split_page("x"), ("x".to_string(), 1));
        assert_eq!(split_page("x \u{b7}y"), ("x \u{b7}y".to_string(), 1));
        assert_eq!(split_page("home"), ("home".to_string(), 1));
    }
}
