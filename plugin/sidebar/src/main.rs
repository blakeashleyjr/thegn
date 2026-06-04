//! superzej sidebar — a left-pinned zellij plugin: a tree of **repos** →
//! **worktrees** → **tabs**, under a bold "WORKSPACES" header.
//!
//! Everything lives in one zellij session now: each repo is a group of tabs
//! named `{repo_slug}/{branch}[ ·N]` (the main checkout is `{slug}/home`; the
//! ` ·N` pages come from `superzej new-tab`). The sidebar merges the live
//! `TabUpdate` (open repos + their worktrees and pages, with tab positions)
//! with `superzej workspaces` (every managed repo + display name, so closed
//! repos still show, dimmed). Selecting is a **tab switch** — never a session
//! teleport: the sidebar/tabbar/panel stay put and only the middle terminal +
//! right panel change.
//!
//! Interactions (keyboard cursor ↑/↓ or j/k, or mouse hover + click / Enter):
//!   - repo row      → switch to its `{slug}/home` tab (open it first if closed)
//!   - worktree row  → switch to its base tab (`home` is the main checkout's
//!     worktree; page rows `·1`/`·2`/… appear only when a worktree has >1 tab)
//!   - page row      → switch to that tab
//!   - `+ worktree`  → `superzej new-worktree --repo <path>` (a new tab)
//!   - `+ new workspace` → fzf repo browser over `$HOME`
//!
//! A `superzej_toggle` pipe hides/shows it; `superzej_refresh` re-pulls the repos.

use std::collections::BTreeMap;
use zellij_tile::prelude::*;

// superzej theme palette (truecolor), kept in sync with config/zellij.kdl.
const CYAN: &str = "94;218;207"; // focus accent / active
const DARK: &str = "20;22;31"; // text on cyan
const SEL: &str = "40;44;62"; // selection / hover bg
const BRIGHT: &str = "224;228;240";
const TEXT: &str = "192;197;211";
const MUTED: &str = "108;112;134";
const RESET: &str = "\u{1b}[0m";

// Rendered layout: row 0 = WORKSPACES header, row 1 = rule, rows 2.. = `self.rows`.
const BODY_START: usize = 2;

#[derive(Default)]
struct State {
    repos: Vec<DbRepo>,          // every managed repo (from `superzej workspaces`)
    tabs: Vec<TabRow>,           // live tabs (from TabUpdate)
    active_repo: Option<String>, // prefix of the focused tab
    views: Vec<RepoView>,        // merged tree, rebuilt on any change
    rows: Vec<Row>,              // flat, selectable rows in display order
    focused: bool,
    hover: Option<usize>,
    cursor: Option<usize>,
    my_id: Option<u32>,
    hidden: bool, // suppressed via hide_pane_with_id (superzej_toggle pipe)
    // The panel plugin pane in our tab: (pane id, is_suppressed). The base
    // swap layout only matches with all surfaces present, so re-showing while
    // the sibling is hidden flashes it in for the relayout (see set_hidden).
    sibling: Option<(u32, bool)>,
}

/// A managed repo as reported by `superzej workspaces` (slug, name, path).
struct DbRepo {
    slug: String,
    name: String,
    path: String,
}

/// A live zellij tab: its full `{slug}/{branch}` name split out, plus position.
struct TabRow {
    repo: String,
    branch: String,
    position: usize,
    active: bool,
}

/// A repo as shown in the tree: its display name/path, whether it's open, the
/// position of its home tab (if open), and its worktrees (repo → worktree →
/// tabs; `home` is the main checkout's worktree, listed first).
struct RepoView {
    name: String,
    path: String,
    home_pos: Option<usize>,
    worktrees: Vec<WorktreeView>,
    active: bool, // the focused tab belongs to this repo
}

/// A worktree and its zellij tabs (the `·N` pages from `superzej new-tab`).
struct WorktreeView {
    label: String,        // "home" or the branch slug
    pages: Vec<PageView>, // sorted by page number; page 1 = the base tab
    active: bool,         // one of its pages is the focused tab
}

struct PageView {
    n: u32,
    position: usize,
    active: bool,
}

#[derive(Clone, Copy)]
enum Row {
    Repo(usize),               // index into self.views
    Worktree(usize, usize),    // (view index, worktree index)
    Page(usize, usize, usize), // (view, worktree, page index)
    AddWorktree(usize),        // view index — add a worktree to this repo
    AddNew,                    // add a brand-new workspace
}

/// Split a `{repo_slug}/{branch}` tab name into (repo, branch).
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
            PermissionType::ReadApplicationState,   // Tab/Pane updates
            PermissionType::ChangeApplicationState, // switch tabs
            PermissionType::RunCommands,            // pull repos / open
            PermissionType::ReadCliPipes,           // unblock CLI toggle pipes
        ]);
        self.my_id = Some(get_plugin_ids().plugin_id);
        subscribe(&[
            EventType::TabUpdate,
            EventType::PaneUpdate,
            EventType::Mouse,
            EventType::Key,
            EventType::RunCommandResult, // `superzej workspaces` results
            EventType::PermissionRequestResult, // re-run loads once granted
        ]);
        set_selectable(true);
        // These may be denied if the (cached) permission grant hasn't landed
        // yet — PermissionRequestResult below retries them.
        self.pull_visibility();
        self.pull_repos();
    }

    fn update(&mut self, event: Event) -> bool {
        match event {
            Event::PermissionRequestResult(_) => {
                // The load()-time run_commands race the permission grant and
                // may have been denied — re-issue them now.
                self.pull_visibility();
                self.pull_repos();
                false
            }
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
                        TabRow {
                            repo,
                            branch,
                            position: t.position,
                            active: t.active,
                        }
                    })
                    .collect();
                // A new/closed tab may belong to a not-yet-pulled repo — refresh.
                self.pull_repos();
                self.rebuild();
                true
            }
            Event::RunCommandResult(_code, stdout, _stderr, ctx) => {
                if ctx.get("cmd").map(String::as_str) == Some("visstate") {
                    if String::from_utf8_lossy(&stdout).trim() == "false" {
                        self.set_hidden(true);
                    }
                    return false;
                }
                if ctx.get("cmd").map(String::as_str) == Some("repos") {
                    let text = String::from_utf8_lossy(&stdout);
                    self.repos = text
                        .lines()
                        .filter_map(|l| {
                            let mut it = l.split('\t');
                            let slug = it.next()?.to_string();
                            if slug.is_empty() {
                                return None;
                            }
                            let name = it.next().unwrap_or(&slug).to_string();
                            let path = it.next().unwrap_or("").to_string();
                            Some(DbRepo { slug, name, path })
                        })
                        .collect();
                    self.rebuild();
                    return true;
                }
                false
            }
            Event::PaneUpdate(manifest) => self.refresh_focus(&manifest),
            Event::Mouse(Mouse::Hover(line, _col)) => {
                let idx = self.line_to_row(line);
                if self.hover != idx {
                    self.hover = idx;
                    return true;
                }
                false
            }
            Event::Mouse(Mouse::LeftClick(line, _col)) => {
                if let Some(row) = self.line_to_row(line) {
                    self.cursor = Some(row);
                    self.activate(row);
                }
                false
            }
            Event::Key(key) => self.on_key(key),
            _ => false,
        }
    }

    fn pipe(&mut self, pipe: PipeMessage) -> bool {
        // CLI pipes block until explicitly unblocked, and the CLI client sends
        // a trailing payload-less message on stdin EOF that would double-fire
        // the toggle — unblock both but only act on the payload-bearing one.
        if let PipeSource::Cli(id) = &pipe.source {
            unblock_cli_pipe_input(id);
            if pipe.payload.is_none() {
                return false;
            }
        }
        match pipe.name.as_str() {
            "superzej_refresh" => {
                self.pull_repos();
                false
            }
            // Hide/show in place: the pane is suppressed (not closed) and the
            // tiled layout reflows; re-showing reapplies the base swap layout
            // so every pane snaps back to its template slot. Broadcast reaches
            // every per-tab instance, keeping tabs in sync.
            "superzej_toggle" => {
                self.set_hidden(!self.hidden);
                false
            }
            "superzej_show" => {
                if self.hidden {
                    self.set_hidden(false);
                }
                false
            }
            _ => false,
        }
    }

    fn render(&mut self, rows: usize, cols: usize) {
        let mut out = String::new();

        if self.focused {
            line_bg(&mut out, " WORKSPACES", cols, DARK, CYAN, true);
        } else {
            line_fg(&mut out, " WORKSPACES", cols, BRIGHT, true);
        }
        let rule: String = "─".repeat(cols);
        line_fg(
            &mut out,
            &rule,
            cols,
            if self.focused { CYAN } else { MUTED },
            false,
        );

        for (ri, row) in self.rows.iter().enumerate() {
            if BODY_START + ri >= rows {
                break;
            }
            let selected = self.cursor == Some(ri) || self.hover == Some(ri);
            let (text, color) = self.row_view(*row);
            if selected {
                line_bg(&mut out, &text, cols, color, SEL, true);
            } else {
                line_fg(&mut out, &text, cols, color, false);
            }
        }
        print!("{out}");
    }
}

impl State {
    /// Suppress or restore our own pane and persist the visibility so
    /// instances in new tabs start consistent. `show_pane_with_id` with
    /// `should_focus_pane=false` restores the pane without stealing focus or
    /// switching tabs (unlike `show_self`) — but zellij re-embeds it wherever
    /// it finds room, so we reapply the base swap layout right after, which
    /// snaps every pane back to its template slot (sidebar left, panel right).
    fn set_hidden(&mut self, hidden: bool) {
        let Some(id) = self.my_id else { return };
        if hidden == self.hidden {
            return;
        }
        if hidden {
            hide_pane_with_id(PaneId::Plugin(id));
        } else {
            show_pane_with_id(PaneId::Plugin(id), false, false);
            if let Some((sid, _)) = self.sibling {
                // Flash the sibling in (no-op if already visible) so the
                // 5-pane base template matches for the relayout. If it is
                // supposed to be hidden, its own instance notices the
                // mismatch on the next PaneUpdate and re-hides itself (see
                // refresh_focus) — our suppressed instance can't know its
                // sibling's current state, so we never re-hide it ourselves.
                show_pane_with_id(PaneId::Plugin(sid), false, false);
            }
            next_swap_layout();
        }
        self.hidden = hidden;
        run_command(
            &[
                "sh",
                "-c",
                &format!(
                    "mkdir -p ~/.superzej && echo {} > ~/.superzej/.sidebar_state",
                    !hidden
                ),
            ],
            BTreeMap::new(),
        );
    }

    /// Re-apply persisted visibility: a toggle may have hidden the sidebar
    /// before this instance loaded (each tab embeds its own instance). The
    /// reply arrives as a `RunCommandResult` tagged `cmd=visstate`.
    fn pull_visibility(&self) {
        run_command(
            &[
                "sh",
                "-c",
                "cat ~/.superzej/.sidebar_state 2>/dev/null || true",
            ],
            BTreeMap::from([("cmd".to_string(), "visstate".to_string())]),
        );
    }

    /// Ask the host for the managed-repo inventory; the reply arrives as a
    /// `RunCommandResult` tagged `cmd=repos`.
    fn pull_repos(&self) {
        let mut ctx = BTreeMap::new();
        ctx.insert("cmd".to_string(), "repos".to_string());
        run_command(&["superzej", "workspaces"], ctx);
    }

    /// Merge `self.repos` (all managed) with `self.tabs` (live) into the tree.
    fn rebuild(&mut self) {
        // Repos known to the DB, plus any live-tab prefix not yet in the DB.
        let mut order: Vec<(String, String, String)> = self
            .repos
            .iter()
            .map(|r| (r.slug.clone(), r.name.clone(), r.path.clone()))
            .collect();
        for t in &self.tabs {
            if !t.repo.is_empty() && !order.iter().any(|(s, _, _)| *s == t.repo) {
                order.push((t.repo.clone(), t.repo.clone(), String::new()));
            }
        }

        self.views = order
            .into_iter()
            .map(|(slug, name, path)| {
                // Group this repo's tabs by worktree base; pages sort by
                // number, worktrees by home-first then lowest tab position.
                let mut home_pos = None;
                let mut worktrees: Vec<WorktreeView> = Vec::new();
                for t in self.tabs.iter().filter(|t| t.repo == slug) {
                    let (base, n) = split_page(&t.branch);
                    if base == "home" && n == 1 {
                        home_pos = Some(t.position);
                    }
                    let w = match worktrees.iter_mut().find(|w| w.label == base) {
                        Some(w) => w,
                        None => {
                            worktrees.push(WorktreeView {
                                label: base,
                                pages: Vec::new(),
                                active: false,
                            });
                            worktrees.last_mut().unwrap()
                        }
                    };
                    w.pages.push(PageView {
                        n,
                        position: t.position,
                        active: t.active,
                    });
                    w.active |= t.active;
                }
                for w in &mut worktrees {
                    w.pages.sort_by_key(|p| p.n);
                }
                worktrees.sort_by_key(|w| {
                    (
                        w.label != "home",
                        w.pages.iter().map(|p| p.position).min().unwrap_or(usize::MAX),
                    )
                });
                RepoView {
                    name,
                    path,
                    home_pos,
                    worktrees,
                    active: self.active_repo.as_deref() == Some(slug.as_str()),
                }
            })
            .collect();
        // STATIC order: sort by display name only — never by open/selected state,
        // so a repo keeps its position when you select or open it (no jumping).
        self.views
            .sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()));

        self.rows.clear();
        for (vi, v) in self.views.iter().enumerate() {
            self.rows.push(Row::Repo(vi));
            for (wi, w) in v.worktrees.iter().enumerate() {
                self.rows.push(Row::Worktree(vi, wi));
                // Page rows only when the worktree has more than one tab.
                if w.pages.len() > 1 {
                    for pi in 0..w.pages.len() {
                        self.rows.push(Row::Page(vi, wi, pi));
                    }
                }
            }
            if v.home_pos.is_some() {
                self.rows.push(Row::AddWorktree(vi));
            }
        }
        self.rows.push(Row::AddNew);
        if let Some(c) = self.cursor {
            if c >= self.rows.len() {
                self.cursor = Some(self.rows.len().saturating_sub(1));
            }
        }
    }

    fn row_view(&self, row: Row) -> (String, &'static str) {
        match row {
            Row::Repo(vi) => {
                let v = &self.views[vi];
                let open = v.home_pos.is_some() || !v.worktrees.is_empty();
                if !open {
                    return (format!("○ {}", v.name), MUTED);
                }
                let marker = if v.active { "▌ " } else { "  " };
                let color = if v.active { CYAN } else { TEXT };
                (format!("{marker}{}", v.name), color)
            }
            Row::Worktree(vi, wi) => {
                let v = &self.views[vi];
                let w = &v.worktrees[wi];
                let last = wi + 1 == v.worktrees.len();
                let glyph = if last { "└" } else { "├" };
                let color = if w.active { CYAN } else { MUTED };
                (format!("  {glyph} {}", w.label), color)
            }
            Row::Page(vi, wi, pi) => {
                let v = &self.views[vi];
                let w = &v.worktrees[wi];
                let p = &w.pages[pi];
                // Continue the parent's trunk line unless it was the last
                // worktree; pages get their own ├/└ connectors.
                let trunk = if wi + 1 == v.worktrees.len() { " " } else { "│" };
                let glyph = if pi + 1 == w.pages.len() { "└" } else { "├" };
                let color = if p.active { CYAN } else { MUTED };
                (format!("  {trunk}   {glyph} ·{}", p.n), color)
            }
            Row::AddWorktree(_) => ("  + worktree".to_string(), MUTED),
            Row::AddNew => ("+ new workspace".to_string(), MUTED),
        }
    }

    fn on_key(&mut self, key: KeyWithModifier) -> bool {
        if self.rows.is_empty() {
            return false;
        }
        let last = self.rows.len() - 1;
        match key.bare_key {
            BareKey::Down | BareKey::Char('j') => {
                self.cursor = Some(self.cursor.map_or(0, |c| (c + 1).min(last)));
                true
            }
            BareKey::Up | BareKey::Char('k') => {
                self.cursor = Some(self.cursor.map_or(last, |c| c.saturating_sub(1)));
                true
            }
            BareKey::Enter | BareKey::Right | BareKey::Char('l') => {
                if let Some(c) = self.cursor {
                    self.activate(c);
                }
                false
            }
            _ => false,
        }
    }

    /// Act on a row. Selecting is a tab switch (`switch_tab_to`, 1-indexed) — no
    /// session change. Opening a closed repo / adding a worktree shells out.
    fn activate(&mut self, row: usize) {
        match self.rows.get(row).copied() {
            Some(Row::Repo(vi)) => {
                let v = &self.views[vi];
                if let Some(pos) = v.home_pos {
                    switch_tab_to(pos as u32 + 1);
                } else {
                    // Closed repo → open its home tab via the binary.
                    run_floating(&["superzej", "new-workspace", v.path.as_str()]);
                }
            }
            Some(Row::Worktree(vi, wi)) => {
                // The worktree row stands for its base tab; fall back to the
                // lowest page if ·1 was closed.
                let w = &self.views[vi].worktrees[wi];
                if let Some(p) = w.pages.iter().find(|p| p.n == 1).or_else(|| w.pages.first()) {
                    switch_tab_to(p.position as u32 + 1);
                }
            }
            Some(Row::Page(vi, wi, pi)) => {
                switch_tab_to(self.views[vi].worktrees[wi].pages[pi].position as u32 + 1);
            }
            Some(Row::AddWorktree(vi)) => {
                let path = self.views[vi].path.clone();
                run_floating(&["superzej", "new-worktree", "--repo", path.as_str()]);
            }
            Some(Row::AddNew) => {
                run_floating_big(&["superzej", "new-workspace", "--from-home"]);
            }
            None => {}
        }
    }

    fn refresh_focus(&mut self, manifest: &PaneManifest) -> bool {
        let Some(id) = self.my_id else { return false };
        let mut focused = false;
        for panes in manifest.panes.values() {
            if !panes.iter().any(|p| p.is_plugin && p.id == id) {
                continue; // another tab's panes
            }
            for p in panes {
                if p.is_plugin && p.id == id {
                    focused = p.is_focused;
                    // Self-enforce: a sibling's show flashes us in for its
                    // relayout — if we're supposed to be hidden, re-hide.
                    if self.hidden && !p.is_suppressed {
                        hide_pane_with_id(PaneId::Plugin(id));
                    }
                }
                // Track our tab's panel pane (default title = its plugin url).
                if p.is_plugin && p.title.contains("panel.wasm") {
                    self.sibling = Some((p.id, p.is_suppressed));
                }
            }
        }
        if self.focused != focused {
            self.focused = focused;
            if focused {
                if self.cursor.is_none() {
                    self.cursor = self
                        .rows
                        .iter()
                        .position(|r| matches!(r, Row::Repo(vi) if self.views[*vi].active))
                        .or(Some(0));
                }
            } else {
                self.hover = None;
            }
            return true;
        }
        false
    }

    fn line_to_row(&self, line: isize) -> Option<usize> {
        if line < BODY_START as isize {
            return None;
        }
        let idx = (line as usize) - BODY_START;
        (idx < self.rows.len()).then_some(idx)
    }
}

/// Open a small floating pane that runs `cmd` and closes on exit (used for
/// helper invocations that issue zellij actions, which need a real pane's env).
fn run_floating(cmd: &[&str]) {
    let mut argv = vec![
        "zellij",
        "run",
        "--floating",
        "--width",
        "50%",
        "--height",
        "30%",
        "--close-on-exit",
        "--",
    ];
    argv.extend_from_slice(cmd);
    run_command(&argv, BTreeMap::new());
}

/// A large floating pane (for the fzf repo browser).
fn run_floating_big(cmd: &[&str]) {
    let mut argv = vec![
        "zellij",
        "run",
        "--floating",
        "--width",
        "85%",
        "--height",
        "80%",
        "--close-on-exit",
        "--",
    ];
    argv.extend_from_slice(cmd);
    run_command(&argv, BTreeMap::new());
}

/// Truncate `text` to `cols` display chars (ANSI escapes pass through), returning
/// the body and the visible char count consumed.
fn clip(text: &str, cols: usize) -> (String, usize) {
    let mut shown = 0;
    let mut buf = String::new();
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\u{1b}' {
            buf.push(c);
            for e in chars.by_ref() {
                buf.push(e);
                if e.is_ascii_alphabetic() {
                    break;
                }
            }
            continue;
        }
        if shown >= cols {
            break;
        }
        buf.push(c);
        shown += 1;
    }
    (buf, shown)
}

/// A line with a truecolor foreground, optionally bold. No background.
fn line_fg(out: &mut String, text: &str, cols: usize, rgb: &str, bold: bool) {
    let (body, _) = clip(text, cols);
    if bold {
        out.push_str("\u{1b}[1m");
    }
    out.push_str(&format!("\u{1b}[38;2;{rgb}m"));
    out.push_str(&body);
    out.push_str(RESET);
    out.push_str("\r\n");
}

/// A full-width line with fg + bg fill (padded with spaces to `cols`).
fn line_bg(out: &mut String, text: &str, cols: usize, fg: &str, bg: &str, bold: bool) {
    let (body, shown) = clip(text, cols);
    if bold {
        out.push_str("\u{1b}[1m");
    }
    out.push_str(&format!("\u{1b}[38;2;{fg}m\u{1b}[48;2;{bg}m"));
    out.push_str(&body);
    if shown < cols {
        out.push_str(&" ".repeat(cols - shown));
    }
    out.push_str(RESET);
    out.push_str("\r\n");
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
