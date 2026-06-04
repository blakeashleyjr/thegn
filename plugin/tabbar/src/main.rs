//! superzej tabbar — a thin, centered strip of the **active repo's** branch
//! tabs. Tabs are named `{repo_slug}/{branch}` (all repos share one session);
//! this strip shows only the tabs whose prefix matches the focused tab's repo,
//! rendering just the branch suffix. It replaces zellij's built-in `tab-bar` so
//! there is no "Zellij (session)" wordmark and no swap-layout ("BASE")
//! indicator. The active tab is a filled cyan chip; clicking/hovering targets it.
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
    active_repo: Option<String>, // prefix of the focused tab; we show only its tabs
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
    repo: String,   // `{slug}` prefix (the repo this tab belongs to)
    branch: String, // display label (the `{branch}` suffix)
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
                self.active_repo = tabs.iter().find(|t| t.active).map(|t| split_tab(&t.name).0);
                self.tabs = tabs
                    .into_iter()
                    .map(|t| {
                        let raw = if t.name.is_empty() {
                            format!("tab {}", t.position + 1)
                        } else {
                            t.name
                        };
                        let (repo, branch) = split_tab(&raw);
                        Tab {
                            repo,
                            branch,
                            position: t.position,
                            active: t.active,
                        }
                    })
                    .collect();
                self.tabs.sort_by_key(|t| t.position);
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

        // Only the active repo's tabs are shown; render their branch suffixes.
        let visible: Vec<usize> = self
            .tabs
            .iter()
            .enumerate()
            .filter(|(_, t)| self.active_repo.as_deref().is_none_or(|r| t.repo == r))
            .map(|(i, _)| i)
            .collect();
        // Build each tab's visible label and total width first, so we can center.
        let labels: Vec<String> = visible
            .iter()
            .map(|&i| format!(" {} ", self.tabs[i].branch))
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
