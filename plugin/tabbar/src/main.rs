//! superzej tabbar — a thin, centered strip of the **focused worktree's**
//! tabs (its `·N` pages from `superzej new-tab`). Tabs are named
//! `{repo_slug}/{branch}[ ·N]` (all repos share one session); this strip
//! shows one chip per page of the focused tab's worktree (`1`, `·2`, `·3`,
//! …) — switching worktrees/repos is the sidebar's job. It replaces zellij's
//! built-in `tab-bar` so there is no "Zellij (session)" wordmark and no
//! swap-layout ("BASE") indicator. The active page is a filled cyan chip;
//! clicking/hovering targets it.
//!
//! It lives in the middle column of the session layout (above the terminals,
//! between the sidebar and the diff/PR panel), so the strip sits indented over
//! the terminal rather than spanning the full width.

use std::collections::BTreeMap;
use zellij_tile::prelude::*;

// superzej theme palette (truecolor), kept in sync with config/zellij.kdl.
const CYAN: &str = "94;218;207"; // active chip bg
const DARK: &str = "20;22;31"; // text on cyan
const SEL: &str = "40;44;62"; // hover bg
const TEXT: &str = "192;197;211";
const MUTED: &str = "108;112;134";
const RESET: &str = "\u{1b}[0m";

#[derive(Default)]
struct State {
    tabs: Vec<Tab>,
    // The focused tab's (repo, worktree base): only its pages are shown.
    active_wt: Option<(String, String)>,
    hidden: bool,
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
        request_permission(&[
            PermissionType::ReadApplicationState,   // Tab/Session updates
            PermissionType::ChangeApplicationState, // switch tabs
            PermissionType::RunCommands,            // `superzej new-tab` pipe
            PermissionType::ReadCliPipes,           // unblock CLI pipes
        ]);
        subscribe(&[
            EventType::TabUpdate,
            EventType::SessionUpdate,
            EventType::Mouse,
        ]);
        set_selectable(true);
    }

    fn update(&mut self, event: Event) -> bool {
        match event {
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
            Event::Mouse(Mouse::Hover(_line, col)) => {
                let idx = self.col_to_index(col);
                if self.hover != idx {
                    self.hover = idx;
                    return true;
                }
                false
            }
            Event::Mouse(Mouse::LeftClick(_line, col)) => {
                if let Some(pos) = self.col_to_index(col) {
                    // switch_tab_to is 1-indexed.
                    switch_tab_to(pos as u32 + 1);
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
        match pipe.name.as_str() {
            "superzej_toggle" => {
                if self.hidden {
                    show_self(true);
                } else {
                    hide_self();
                }
                self.hidden = !self.hidden;
            }
            // Alt+t / tab-mode `n`: open a second full-chrome tab on the
            // focused worktree. Run via the plugin (no spawned command pane,
            // no floating flash). Every per-tab instance fires; the binary
            // resolves the focused tab from dump-layout (always fresh) and a
            // lockfile collapses the concurrent invocations to one tab.
            "superzej_new_tab" => {
                if let Some(s) = self.session.clone() {
                    run_command(
                        &["superzej", "new-tab", "--session", &s],
                        BTreeMap::new(),
                    );
                }
            }
            _ => {}
        }
        false
    }

    fn render(&mut self, _rows: usize, cols: usize) {
        const WORDMARK: &str = "superzej";
        // Reserve a left wordmark (replaces zellij's old top-left "Zellij" label),
        // then center the tabs within the remaining width.
        let brand_w = (WORDMARK.chars().count() + 1).min(cols); // +1 trailing space
        let avail = cols.saturating_sub(brand_w);

        // Only the focused worktree's pages are shown.
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
        // Build each page's visible chip label and total width first, so we
        // can center: ` 1 ` for the base tab, ` ·N ` for the extra pages.
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
        let total: usize = labels.iter().map(|l| l.chars().count()).sum::<usize>()
            + sep * labels.len().saturating_sub(1);
        let left_pad = if total < avail {
            (avail - total) / 2
        } else {
            0
        };

        let mut out = String::new();
        let mut col = 0usize;
        self.spans.clear();

        // Wordmark (dim cyan, bold), pinned left.
        out.push_str(&format!("\u{1b}[1m\u{1b}[38;2;{CYAN}m{WORDMARK}{RESET}"));
        col += WORDMARK.chars().count();
        out.push(' ');
        col += 1;

        // Centering pad within the remaining width.
        out.push_str(&" ".repeat(left_pad));
        col += left_pad;

        for (j, label) in labels.iter().enumerate() {
            let i = visible[j];
            let w = label.chars().count();
            let start = col;
            let active = self.tabs[i].active;
            let hovered = self.hover == Some(self.tabs[i].position);
            if active {
                out.push_str(&format!("\u{1b}[1m\u{1b}[38;2;{DARK}m\u{1b}[48;2;{CYAN}m"));
            } else if hovered {
                out.push_str(&format!("\u{1b}[1m\u{1b}[38;2;{TEXT}m\u{1b}[48;2;{SEL}m"));
            } else {
                out.push_str(&format!("\u{1b}[38;2;{MUTED}m"));
            }
            out.push_str(label);
            out.push_str(RESET);
            self.spans.push((start, start + w, self.tabs[i].position));
            col += w;
            if j + 1 < labels.len() {
                out.push(' ');
                col += 1;
            }
        }
        print!("{out}");
    }
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
