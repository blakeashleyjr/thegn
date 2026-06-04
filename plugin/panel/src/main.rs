//! superzej panel — a right-pinned zellij plugin showing the focused tab's
//! worktree: its git diff (--stat) and full GitHub PR state with actions.
//!
//! The WASM sandbox can't shell out, so the panel drives the `superzej` binary
//! via zellij's `run_command` host bridge and reads results back from
//! `RunCommandResult`. PaneInfo carries no cwd, so we identify the focused
//! worktree by (session, tab) and ask `superzej resolve-worktree` for its path,
//! then fetch `pr status` + `diff --stat` for it. A background `superzej pr
//! watch` may also push fresh JSON via `pipe()` (name `superzej_pr`). Action
//! keys run the matching `superzej pr …` subcommand — safe ones inline,
//! destructive/interactive ones (merge/create/approve) in a floating pane.

use std::collections::BTreeMap;
use zellij_tile::prelude::*;

const REFRESH_SECS: f64 = 15.0;

// superzej theme palette (truecolor), kept in sync with config/zellij.kdl.
const CYAN: &str = "94;218;207";
const DARK: &str = "20;22;31";
const BRIGHT: &str = "224;228;240";
const MUTED: &str = "108;112;134";

#[derive(Clone, Copy, PartialEq, Eq)]
enum Tab {
    Diff,
    Pr,
    Checks,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum DiffView {
    FileList,
    FileDiff,
}

#[derive(Clone)]
struct FileEntry {
    status: char,
    path: String,
    added: u32,
    deleted: u32,
}

impl Default for Tab {
    fn default() -> Self {
        Tab::Diff
    }
}
impl Default for DiffView {
    fn default() -> Self {
        DiffView::FileList
    }
}

#[derive(Default)]
struct State {
    session: Option<String>,
    active_tab: Option<String>,
    identity: Option<(String, String)>,
    worktree: Option<String>,
    pr: Option<serde_json::Value>,
    diff: String,
    status_line: String,
    hidden: bool,
    focused: bool,
    my_id: Option<u32>,
    // new tab/diff fields:
    current_tab: Tab,
    diff_view: DiffView,
    diff_scroll: usize,
    files: Vec<FileEntry>,
    file_diff: String,
}

register_plugin!(State);

impl ZellijPlugin for State {
    fn load(&mut self, _config: BTreeMap<String, String>) {
        request_permission(&[
            PermissionType::ReadApplicationState,   // Session/Tab updates
            PermissionType::RunCommands,            // superzej pr/diff/resolve
            PermissionType::ChangeApplicationState, // floating action panes
        ]);
        self.my_id = Some(get_plugin_ids().plugin_id);
        subscribe(&[
            EventType::SessionUpdate,
            EventType::TabUpdate,
            EventType::PaneUpdate,
            EventType::RunCommandResult,
            EventType::Key,
            EventType::Timer,
        ]);
        set_selectable(true);
        set_timeout(REFRESH_SECS);
    }

    fn update(&mut self, event: Event) -> bool {
        match event {
            Event::SessionUpdate(infos, _resurrectable) => {
                self.session = infos
                    .iter()
                    .find(|s| s.is_current_session)
                    .map(|s| s.name.clone());
                self.refocus()
            }
            Event::TabUpdate(tabs) => {
                self.active_tab = tabs.iter().find(|t| t.active).map(|t| t.name.clone());
                self.refocus()
            }
            Event::PaneUpdate(manifest) => self.refresh_focus(&manifest),
            Event::Timer(_) => {
                set_timeout(REFRESH_SECS);
                if self.worktree.is_some() {
                    self.fetch(false);
                }
                false
            }
            Event::RunCommandResult(_code, stdout, _stderr, ctx) => {
                self.on_result(ctx.get("cmd").map(String::as_str), stdout)
            }
            Event::Key(key) => self.on_key(key),
            _ => false,
        }
    }

    fn pipe(&mut self, pipe: PipeMessage) -> bool {
        match pipe.name.as_str() {
            "superzej_toggle" => {
                if self.hidden {
                    show_self(true);
                } else {
                    hide_self();
                }
                self.hidden = !self.hidden;
                false
            }
            "superzej_pr" => {
                if let Some(payload) = pipe.payload {
                    if let Ok(v) = serde_json::from_str::<serde_json::Value>(&payload) {
                        // Only accept pushes for the worktree we're showing.
                        let same =
                            v.get("worktree").and_then(|w| w.as_str()) == self.worktree.as_deref();
                        if same {
                            self.pr = Some(v);
                            return true;
                        }
                    }
                }
                false
            }
            _ => false,
        }
    }

    fn render(&mut self, rows: usize, cols: usize) {
        let mut out = String::new();
        // Tab bar
        tab_bar(&mut out, cols, self.focused, self.current_tab);
        // Rule
        let rule: String = "─".repeat(cols);
        out.push_str(&format!(
            "\u{1b}[38;2;{}m{rule}\u{1b}[0m\r\n",
            if self.focused { CYAN } else { MUTED }
        ));

        // Clamp cursor before rendering diff tab
        if self.current_tab == Tab::Diff && self.diff_view == DiffView::FileList {
            if !self.files.is_empty() {
                self.diff_scroll = self.diff_scroll.min(self.files.len() - 1);
            }
        }

        match self.current_tab {
            Tab::Diff => self.render_diff_tab(&mut out, rows, cols),
            Tab::Pr => self.render_pr_tab(&mut out, rows, cols),
            Tab::Checks => self.render_checks_tab(&mut out, rows, cols),
        }

        if !self.status_line.is_empty() {
            push(
                &mut out,
                &format!("\u{1b}[2m{}\u{1b}[0m", self.status_line),
                cols,
            );
        }
        push(&mut out, self.help_bar(), cols);
        print!("{out}");
    }
}

impl State {
    /// Update `self.focused` from the pane manifest (find our own plugin pane).
    fn refresh_focus(&mut self, manifest: &PaneManifest) -> bool {
        let Some(id) = self.my_id else { return false };
        let mut focused = false;
        for panes in manifest.panes.values() {
            for p in panes {
                if p.is_plugin && p.id == id {
                    focused = p.is_focused;
                }
            }
        }
        if self.focused != focused {
            self.focused = focused;
            return true;
        }
        false
    }

    /// Recompute the focused (session, tab); when it changes, resolve its
    /// worktree path. Returns whether a re-render is warranted.
    fn refocus(&mut self) -> bool {
        let (Some(s), Some(t)) = (self.session.clone(), self.active_tab.clone()) else {
            return false;
        };
        let id = (s.clone(), t.clone());
        if self.identity.as_ref() == Some(&id) {
            return false;
        }
        self.identity = Some(id);
        self.worktree = None;
        self.pr = None;
        self.diff.clear();
        let mut ctx = BTreeMap::new();
        ctx.insert("cmd".to_string(), "resolve".to_string());
        run_command(
            &["superzej", "resolve-worktree", "--session", &s, "--tab", &t],
            ctx,
        );
        true
    }

    /// Kick off `superzej pr status` + `superzej diff --files` + `--stat` for the worktree.
    fn fetch(&mut self, refresh: bool) {
        let Some(wt) = self.worktree.clone() else {
            return;
        };
        let mut pr_ctx = BTreeMap::new();
        pr_ctx.insert("cmd".to_string(), "pr".to_string());
        let mut pr_cmd = vec!["superzej", "pr", "status", "--json", "--worktree", &wt];
        if refresh {
            pr_cmd.push("--refresh");
        }
        run_command(&pr_cmd, pr_ctx);

        let mut file_ctx = BTreeMap::new();
        file_ctx.insert("cmd".to_string(), "files".to_string());
        run_command(
            &["superzej", "diff", "--files", "--worktree", &wt],
            file_ctx,
        );

        let mut diff_ctx = BTreeMap::new();
        diff_ctx.insert("cmd".to_string(), "diff".to_string());
        run_command(&["superzej", "diff", "--stat", "--worktree", &wt], diff_ctx);
    }

    fn on_result(&mut self, cmd: Option<&str>, stdout: Vec<u8>) -> bool {
        let text = String::from_utf8_lossy(&stdout).into_owned();
        match cmd {
            Some("resolve") => {
                let path = text.trim();
                if path.is_empty() {
                    self.worktree = None;
                } else {
                    self.worktree = Some(path.to_string());
                    self.fetch(false);
                }
                true
            }
            Some("pr") => {
                self.pr = serde_json::from_str(text.trim()).ok();
                true
            }
            Some("diff") => {
                self.diff = text;
                true
            }
            Some("files") => {
                self.files = text
                    .lines()
                    .filter_map(|l| {
                        let parts: Vec<&str> = l.splitn(4, '\t').collect();
                        if parts.len() < 2 {
                            return None;
                        }
                        Some(FileEntry {
                            status: parts[0].chars().next().unwrap_or('?'),
                            path: parts[1].to_string(),
                            added: parts.get(2).and_then(|v| v.parse().ok()).unwrap_or(0),
                            deleted: parts.get(3).and_then(|v| v.parse().ok()).unwrap_or(0),
                        })
                    })
                    .collect();
                if self.diff_view == DiffView::FileDiff {
                    self.diff_view = DiffView::FileList;
                }
                self.diff_scroll = 0;
                true
            }
            Some("file_diff") => {
                self.file_diff = text;
                self.diff_view = DiffView::FileDiff;
                self.diff_scroll = 0;
                true
            }
            _ => false,
        }
    }

    fn on_key(&mut self, key: KeyWithModifier) -> bool {
        // Handle Esc first (universal back for FileDiff).
        if key.bare_key == BareKey::Esc {
            match self.current_tab {
                Tab::Diff if self.diff_view == DiffView::FileDiff => {
                    self.diff_view = DiffView::FileList;
                    self.file_diff.clear();
                    self.diff_scroll = 0;
                    return true;
                }
                _ => return false,
            }
        }
        // Map Enter → '\r' so the file-list handler picks it up (Enter is
        // not a BareKey::Char — it would be silently swallowed by the guard).
        if key.bare_key == BareKey::Enter {
            return self.dispatch_char('\r');
        }
        let BareKey::Char(c) = key.bare_key else {
            return false;
        };
        // Tab switching (digits + Tab).
        match c {
            '1' => {
                self.current_tab = Tab::Diff;
                self.diff_scroll = 0;
                return true;
            }
            '2' => {
                self.current_tab = Tab::Pr;
                return true;
            }
            '3' => {
                self.current_tab = Tab::Checks;
                return true;
            }
            '\t' => {
                self.current_tab = match self.current_tab {
                    Tab::Diff => Tab::Pr,
                    Tab::Pr => Tab::Checks,
                    Tab::Checks => Tab::Diff,
                };
                self.diff_scroll = 0;
                return true;
            }
            _ => {}
        }
        self.dispatch_char(c)
    }

    /// Dispatch a character to the active tab's per-tab key handler.
    fn dispatch_char(&mut self, c: char) -> bool {
        match self.current_tab {
            Tab::Diff => self.on_diff_key(c),
            Tab::Pr => self.on_pr_key(c),
            Tab::Checks => self.on_checks_key(c),
        }
    }

    /// Run a safe `superzej <args>` for its effect, then refresh.
    fn action_inline(&mut self, wt: &str, args: &[&str]) {
        let mut cmd = vec!["superzej"];
        cmd.extend_from_slice(args);
        cmd.extend_from_slice(&["--worktree", wt]);
        let mut ctx = BTreeMap::new();
        ctx.insert("cmd".to_string(), "action".to_string());
        run_command(&cmd, ctx);
        self.status_line = format!("ran: {}", args.join(" "));
        self.fetch(true);
    }

    /// Open a floating pane running `superzej <args>` (for confirm/input).
    fn action_floating(&mut self, wt: &str, args: &[&str]) {
        let mut cmd = vec![
            "zellij",
            "run",
            "--floating",
            "--close-on-exit",
            "--cwd",
            wt,
            "--",
            "superzej",
        ];
        cmd.extend_from_slice(args);
        cmd.extend_from_slice(&["--worktree", wt]);
        run_command(&cmd, BTreeMap::new());
        self.status_line = format!("opened: {}", args.join(" "));
    }

    fn help_bar(&self) -> &str {
        match self.current_tab {
            Tab::Diff => match self.diff_view {
                DiffView::FileList => "[Enter] diff  [o] edit  [f] refresh",
                DiffView::FileDiff => "[Esc] back  [o] edit  [f] refresh",
            },
            Tab::Pr => "[o] open PR  [c] create  [m] merge  [a] approve  [r] rerun  [f] refresh",
            Tab::Checks => "[r] rerun  [f] refresh",
        }
    }

    fn render_diff_tab(&mut self, out: &mut String, rows: usize, cols: usize) {
        match self.diff_view {
            DiffView::FileDiff => self.render_file_diff(out, rows, cols),
            DiffView::FileList => self.render_file_list(out, rows, cols),
        }
    }

    fn render_file_list(&self, out: &mut String, rows: usize, cols: usize) {
        let Some(wt) = &self.worktree else {
            push(out, "  (focus a worktree tab)", cols);
            return;
        };
        push(out, &format!("\u{1b}[2m{}\u{1b}[0m", short(wt)), cols);

        if self.files.is_empty() {
            push(out, "  no changes", cols);
            return;
        }

        let header_rows: usize = 1;
        let footer_rows: usize = 1;
        let available = rows.saturating_sub(header_rows + footer_rows);
        if available == 0 {
            return;
        }
        let max_scroll = self.files.len().saturating_sub(available);
        let scroll = self.diff_scroll.min(max_scroll);
        let visible = &self.files[scroll..]
            .iter()
            .take(available)
            .collect::<Vec<_>>();

        for (i, f) in visible.iter().enumerate() {
            let abs_idx = scroll + i;
            let is_cursor = self.focused && abs_idx == self.diff_scroll;
            let status_color = match f.status {
                'M' => "33",
                'A' => "32",
                'D' => "31",
                'R' => "34",
                'C' => "35",
                _ => "0",
            };
            // Line counts: green +N, red -M
            let counts = if f.added > 0 || f.deleted > 0 {
                format!(
                    " \u{1b}[32m+{}\u{1b}[0m\u{1b}[31m-{}\u{1b}[0m",
                    f.added, f.deleted
                )
            } else {
                String::new()
            };
            let line = format!(
                "  \u{1b}[{status_color}m{}\u{1b}[0m {}{}",
                f.status, f.path, counts
            );
            if is_cursor {
                let full = format!("\u{1b}[48;2;40;44;62m{line}\u{1b}[0m");
                push(out, &full, cols);
            } else {
                push(out, &line, cols);
            }
        }
    }

    fn render_file_diff(&self, out: &mut String, rows: usize, cols: usize) {
        let Some(wt) = &self.worktree else {
            push(out, "  (focus a worktree tab)", cols);
            return;
        };
        let header = format!("{}  \u{1b}[2m[Esc back]\u{1b}[0m", short(wt));
        push(out, &header, cols);

        let selected_path = self
            .files
            .get(self.diff_scroll)
            .map(|f| f.path.as_str())
            .unwrap_or("");
        let sep = format!("\u{1b}[38;2;{}m── {} ──\u{1b}[0m", CYAN, selected_path);
        push(out, &sep, cols);

        let consumed: usize = 3;
        let available = rows.saturating_sub(consumed);
        if available == 0 {
            return;
        }

        let diff_lines: Vec<&str> = self.file_diff.lines().collect();
        if diff_lines.is_empty() {
            push(out, "  (no diff — untracked or binary file)", cols);
            return;
        }

        let max_scroll = diff_lines.len().saturating_sub(available);
        let scroll = self.diff_scroll.min(max_scroll);
        for line in diff_lines.iter().skip(scroll).take(available) {
            push(out, line, cols);
        }
    }

    fn render_pr_tab(&mut self, out: &mut String, _rows: usize, cols: usize) {
        let Some(wt) = &self.worktree else {
            push(out, "  (focus a worktree tab)", cols);
            return;
        };
        push(out, &format!("\u{1b}[2m{}\u{1b}[0m", short(wt)), cols);
        self.render_pr(out, cols);
    }

    fn render_checks_tab(&mut self, out: &mut String, _rows: usize, cols: usize) {
        let Some(wt) = &self.worktree else {
            push(out, "  (focus a worktree tab)", cols);
            return;
        };
        push(out, &format!("\u{1b}[2m{}\u{1b}[0m", short(wt)), cols);

        let Some(pr) = &self.pr else {
            push(out, "  PR: loading…", cols);
            return;
        };
        let kind = pr.get("kind").and_then(|v| v.as_str()).unwrap_or("");
        if kind != "pr" {
            push(out, "  no PR — no checks", cols);
            return;
        }
        let Some(rollup) = pr.get("statusCheckRollup") else {
            push(out, "  no checks data", cols);
            return;
        };
        let Some(arr) = rollup.as_array() else {
            push(out, "  no checks data", cols);
            return;
        };
        if arr.is_empty() {
            push(out, "  no checks to display", cols);
            return;
        }
        for check in arr {
            let name = check.get("name").and_then(|v| v.as_str()).unwrap_or("?");
            let conclusion = check.get("conclusion").and_then(|v| v.as_str());
            let status = check.get("status").and_then(|v| v.as_str()).unwrap_or("");
            let state = check.get("state").and_then(|v| v.as_str());

            let (icon, color) = if let Some(c) = conclusion {
                match c.to_uppercase().as_str() {
                    "SUCCESS" | "NEUTRAL" | "SKIPPED" => ("✔", "32"),
                    "FAILURE" | "TIMED_OUT" | "CANCELLED" | "STARTUP_FAILURE" => ("✗", "31"),
                    _ => ("⧗", "33"),
                }
            } else if status == "IN_PROGRESS" || status == "QUEUED" {
                ("⧗", "33")
            } else if let Some(s) = state {
                match s.to_uppercase().as_str() {
                    "SUCCESS" => ("✔", "32"),
                    "FAILURE" | "ERROR" => ("✗", "31"),
                    _ => ("⧗", "33"),
                }
            } else {
                ("⧗", "33")
            };
            push(
                out,
                &format!("  \u{1b}[{color}m{icon}\u{1b}[0m {name}"),
                cols,
            );
        }
    }

    fn on_diff_key(&mut self, c: char) -> bool {
        let Some(_wt) = self.worktree.clone() else {
            return false;
        };
        match self.diff_view {
            DiffView::FileList => self.on_file_list_key(c),
            DiffView::FileDiff => self.on_file_diff_key(c),
        }
    }

    fn on_file_list_key(&mut self, c: char) -> bool {
        let Some(_wt) = self.worktree.clone() else {
            return false;
        };
        match c {
            'j' | 'J' => {
                if !self.files.is_empty() {
                    self.diff_scroll = (self.diff_scroll + 1).min(self.files.len() - 1);
                }
                true
            }
            'k' | 'K' => {
                self.diff_scroll = self.diff_scroll.saturating_sub(1);
                true
            }
            '\r' | '\n' => {
                if let Some(entry) = self.files.get(self.diff_scroll).cloned() {
                    self.fetch_file_diff(&entry.path);
                    self.status_line = format!("diff: {}", entry.path);
                }
                true
            }
            'o' | 'O' => {
                if let Some(entry) = self.files.get(self.diff_scroll) {
                    self.open_in_editor(&entry.path);
                }
                true
            }
            'f' => {
                self.fetch(true);
                self.status_line = "refreshing…".into();
                true
            }
            _ => false,
        }
    }

    fn on_file_diff_key(&mut self, c: char) -> bool {
        let Some(_wt) = self.worktree.clone() else {
            return false;
        };
        match c {
            'j' | 'J' => {
                let diff_lines = self.file_diff.lines().count();
                let max_scroll = diff_lines.saturating_sub(1);
                let step = max_scroll.saturating_div(2).max(1);
                self.diff_scroll = (self.diff_scroll + step).min(max_scroll);
                true
            }
            'k' | 'K' => {
                let step = self.diff_scroll.saturating_div(2).max(1);
                self.diff_scroll = self.diff_scroll.saturating_sub(step);
                true
            }
            'o' | 'O' => {
                if let Some(entry) = self.files.get(self.diff_scroll) {
                    self.open_in_editor(&entry.path);
                }
                true
            }
            'f' => {
                self.fetch(true);
                self.status_line = "refreshing…".into();
                true
            }
            _ => false,
        }
    }

    fn on_pr_key(&mut self, c: char) -> bool {
        let Some(wt) = self.worktree.clone() else {
            return false;
        };
        match c {
            'o' => {
                self.action_inline(&wt, &["pr", "open"]);
                true
            }
            'r' => {
                self.action_inline(&wt, &["pr", "rerun-checks"]);
                true
            }
            'f' => {
                self.fetch(true);
                self.status_line = "refreshing…".into();
                true
            }
            'm' => {
                self.action_floating(&wt, &["pr", "merge", "--delete-branch"]);
                true
            }
            'c' => {
                self.action_floating(&wt, &["pr", "create"]);
                true
            }
            'a' => {
                self.action_floating(&wt, &["pr", "approve"]);
                true
            }
            _ => false,
        }
    }

    fn on_checks_key(&mut self, c: char) -> bool {
        let Some(wt) = self.worktree.clone() else {
            return false;
        };
        match c {
            'r' => {
                self.action_inline(&wt, &["pr", "rerun-checks"]);
                true
            }
            'f' => {
                self.fetch(true);
                self.status_line = "refreshing…".into();
                true
            }
            _ => false,
        }
    }

    fn fetch_file_diff(&self, path: &str) {
        let Some(wt) = self.worktree.clone() else {
            return;
        };
        let mut ctx = BTreeMap::new();
        ctx.insert("cmd".to_string(), "file_diff".to_string());
        run_command(
            &["superzej", "diff", "--file", path, "--worktree", &wt],
            ctx,
        );
    }

    fn open_in_editor(&self, path: &str) {
        let Some(wt) = &self.worktree else { return };
        let editor = "vi";
        let cmd = vec![
            "zellij",
            "run",
            "--floating",
            "--close-on-exit",
            "--cwd",
            wt,
            "--",
            editor,
            path,
        ];
        run_command(&cmd, BTreeMap::new());
    }

    fn render_pr(&self, out: &mut String, cols: usize) {
        let Some(pr) = &self.pr else {
            push(out, "  PR: loading…", cols);
            return;
        };
        let kind = pr.get("kind").and_then(|v| v.as_str()).unwrap_or("");
        match kind {
            "pr" => {
                let num = pr.get("number").and_then(|v| v.as_u64()).unwrap_or(0);
                let title = pr.get("title").and_then(|v| v.as_str()).unwrap_or("");
                let state = pr.get("state").and_then(|v| v.as_str()).unwrap_or("");
                let draft = pr.get("isDraft").and_then(|v| v.as_bool()).unwrap_or(false);
                let review = pr
                    .get("reviewDecision")
                    .and_then(|v| v.as_str())
                    .unwrap_or("—");
                let d = if draft { " draft" } else { "" };
                push(
                    out,
                    &format!("\u{1b}[1mPR #{num}\u{1b}[0m {state}{d}"),
                    cols,
                );
                push(out, &format!("  {title}"), cols);
                if let Some(c) = pr.get("checks") {
                    let p = c.get("passed").and_then(|v| v.as_u64()).unwrap_or(0);
                    let f = c.get("failed").and_then(|v| v.as_u64()).unwrap_or(0);
                    let q = c.get("pending").and_then(|v| v.as_u64()).unwrap_or(0);
                    push(
                        out,
                        &format!(
                            "  CI \u{1b}[32m✔{p}\u{1b}[0m \u{1b}[31m✗{f}\u{1b}[0m ⧗{q}   review: {review}"
                        ),
                        cols,
                    );
                }
            }
            "no_pr" => {
                let b = pr.get("branch").and_then(|v| v.as_str()).unwrap_or("");
                push(out, &format!("  no PR for {b}"), cols);
                push(out, "  press [c] to create one", cols);
            }
            "no_gh" => push(out, "  gh CLI not installed", cols),
            "not_authenticated" => push(out, "  gh not authenticated (gh auth login)", cols),
            "rate_limited" => push(out, "  GitHub rate limited…", cols),
            "error" => {
                let m = pr
                    .get("message")
                    .and_then(|v| v.as_str())
                    .unwrap_or("error");
                push(out, &format!("  error: {m}"), cols);
            }
            _ => push(out, "  PR: …", cols),
        }
    }
}

/// Append one viewport line, hard-truncated to `cols` *display* chars (ANSI
/// escape sequences are passed through and not counted).
fn push(out: &mut String, text: &str, cols: usize) {
    let mut shown = 0;
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\u{1b}' {
            out.push(c);
            for e in chars.by_ref() {
                out.push(e);
                if e.is_ascii_alphabetic() {
                    break;
                }
            }
            continue;
        }
        if shown >= cols {
            break;
        }
        out.push(c);
        shown += 1;
    }
    out.push_str("\u{1b}[0m\r\n");
}

fn short(path: &str) -> String {
    let parts: Vec<&str> = path.rsplitn(3, '/').collect();
    match parts.as_slice() {
        [last, mid, _] => format!("…/{mid}/{last}"),
        _ => path.to_string(),
    }
}

/// Render the three-tab bar. Active tab gets cyan-bg (focused) or bright fg,
/// inactive tabs are muted.
fn tab_bar(out: &mut String, cols: usize, focused: bool, active: Tab) {
    let segs = [
        (" DIFF ", Tab::Diff),
        ("│ PR ", Tab::Pr),
        ("│ CHECKS ", Tab::Checks),
    ];
    let mut line = String::new();
    for (text, tab) in &segs {
        let is_active = *tab == active;
        if is_active && focused {
            line.push_str(&format!(
                "\u{1b}[1m\u{1b}[38;2;{}m\u{1b}[48;2;{}m",
                CYAN, DARK
            ));
        } else if is_active {
            line.push_str(&format!("\u{1b}[1m\u{1b}[38;2;{}m", BRIGHT));
        } else {
            line.push_str(&format!("\u{1b}[38;2;{}m", MUTED));
        }
        line.push_str(text);
        line.push_str("\u{1b}[0m");
    }
    let used: usize = segs.iter().map(|(t, _)| t.len()).sum();
    if used < cols {
        line.push_str(&" ".repeat(cols - used));
    }
    out.push_str(&line);
    out.push_str("\r\n");
}
