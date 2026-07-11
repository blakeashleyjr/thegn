# Tabbed Right Panel Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Transform the single-view right panel into a three-tab WASM plugin — Modified Files (drill-down diff), PR status/actions, and CI Checks — with keyboard navigation and editor integration.

**Architecture:** The panel plugin (`plugin/panel/src/main.rs`) gains a `Tab` enum, a `DiffView` enum for drill-down state, and per-tab render/key methods. The binary (`src/commands/diff.rs`) gets two new flags (`--files` and `--file`) to serve file-level data. No new dependencies.

**Tech Stack:** Rust 2021, zellij-tile 0.44 (WASM), serde_json 1, clap 4.5

---

### Task 1: Foundation — Tab Bar, Tab Switching, Restructured State

**Files:**

- Modify: `plugin/panel/src/main.rs` (full restructure)

Replace the single-view panel with a tabbed interface. Add `Tab`, `DiffView`, and `FileEntry` types. Restructure `render()` to branch on `current_tab`. PR and Checks tabs render stubs for now.

- [ ] **Step 1: Add types and extend State**

Add these before `register_plugin!(State)`:

```rust
#[derive(Clone, Copy, PartialEq, Eq)]
enum Tab { Diff, Pr, Checks }

#[derive(Clone, Copy, PartialEq, Eq)]
enum DiffView { FileList, FileDiff }

struct FileEntry {
    status: char,
    path: String,
}
```

Replace the `State` struct:

```rust
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
    // new fields:
    current_tab: Tab,
    diff_view: DiffView,
    diff_scroll: usize,
    files: Vec<FileEntry>,
    file_diff: String,
}
```

- [ ] **Step 2: Add tab bar render helper**

Add after the `fn short()` helper:

```rust
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
```

- [ ] **Step 3: Restructure `render()` with tab dispatch**

Replace the existing `render()` method entirely:

```rust
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
```

Add stub tab render methods:

```rust
fn render_diff_tab(&mut self, out: &mut String, _rows: usize, cols: usize) {
    match &self.worktree {
        None => push(out, "  (focus a worktree tab)", cols),
        Some(wt) => {
            push(out, &format!("\u{1b}[2m{}\u{1b}[0m", short(wt)), cols);
            push(out, "  DIFF tab — file list coming in P3", cols);
        }
    }
}

fn render_pr_tab(&mut self, out: &mut String, _rows: usize, cols: usize) {
    match &self.worktree {
        None => push(out, "  (focus a worktree tab)", cols),
        Some(wt) => {
            push(out, &format!("\u{1b}[2m{}\u{1b}[0m", short(wt)), cols);
            self.render_pr(out, cols);
        }
    }
}

fn render_checks_tab(&mut self, out: &mut String, _rows: usize, cols: usize) {
    match &self.worktree {
        None => push(out, "  (focus a worktree tab)", cols),
        Some(wt) => {
            push(out, &format!("\u{1b}[2m{}\u{1b}[0m", short(wt)), cols);
            push(out, "  CHECKS tab — coming in P6", cols);
        }
    }
}
```

- [ ] **Step 4: Add help_bar method**

```rust
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
```

- [ ] **Step 5: Add tab switching to `on_key`**

Replace the existing `on_key` method. Tab switching is handled first, then per-tab dispatch:

```rust
fn on_key(&mut self, key: KeyWithModifier) -> bool {
    let BareKey::Char(c) = key.bare_key else {
        return false;
    };
    match c {
        '1' => { self.current_tab = Tab::Diff; self.diff_scroll = 0; return true; }
        '2' => { self.current_tab = Tab::Pr; return true; }
        '3' => { self.current_tab = Tab::Checks; return true; }
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
    match self.current_tab {
        Tab::Diff => self.on_diff_key(c),
        Tab::Pr => self.on_pr_key(c),
        Tab::Checks => self.on_checks_key(c),
    }
}
```

Add the per-tab key handlers (stubbed for now, PR port comes in P5):

```rust
fn on_diff_key(&mut self, c: char) -> bool {
    let Some(_wt) = self.worktree.clone() else { return false; };
    match c {
        'f' => { self.fetch(true); self.status_line = "refreshing…".into(); true }
        _ => false,
    }
}

fn on_pr_key(&mut self, c: char) -> bool {
    let Some(wt) = self.worktree.clone() else { return false; };
    match c {
        'o' => self.action_inline(&wt, &["pr", "open"]),
        'r' => self.action_inline(&wt, &["pr", "rerun-checks"]),
        'f' => { self.fetch(true); self.status_line = "refreshing…".into(); true }
        'm' => self.action_floating(&wt, &["pr", "merge", "--delete-branch"]),
        'c' => self.action_floating(&wt, &["pr", "create"]),
        'a' => self.action_floating(&wt, &["pr", "approve"]),
        _ => false,
    }
}

fn on_checks_key(&mut self, c: char) -> bool {
    let Some(wt) = self.worktree.clone() else { return false; };
    match c {
        'r' => self.action_inline(&wt, &["pr", "rerun-checks"]),
        'f' => { self.fetch(true); self.status_line = "refreshing…".into(); true }
        _ => false,
    }
}
```

- [ ] **Step 6: Commit**

```bash
git add plugin/panel/src/main.rs
git commit -m "feat(panel): tab bar with Diff/PR/Checks tabs and tab switching"
```

---

### Task 2: Diff CLI — `--files` and `--file` flags

**Files:**

- Modify: `src/cli.rs` (add flags)
- Modify: `src/commands/diff.rs` (handle flags)
- Modify: `src/main.rs` (pass flags)

- [ ] **Step 1: Add flags to CLI definition**

In `src/cli.rs`, add `files: bool` and `file: Option<String>` to the `Diff` command variant:

```rust
/// Emit a colorized, non-paged git diff for a worktree.
Diff {
    #[arg(long)]
    worktree: Option<String>,
    /// Diff against this base ref (default: the worktree's resolved base).
    #[arg(long)]
    base: Option<String>,
    /// Summary (--stat) only.
    #[arg(long)]
    stat: bool,
    /// List modified files as TSV (status\tpath).
    #[arg(long)]
    files: bool,
    /// Full diff of a single file.
    #[arg(long)]
    file: Option<String>,
},
```

- [ ] **Step 2: Handle flags in `diff::run`**

In `src/commands/diff.rs`, update `run()` signature and add the two new branches:

```rust
pub fn run(
    worktree: Option<String>,
    base: Option<String>,
    stat: bool,
    files: bool,
    file_path: Option<String>,
) -> Result<()> {
    let wt = resolve_worktree(worktree);

    let base = base.unwrap_or_else(|| {
        let root = repo::main_worktree(&wt).unwrap_or_else(|| wt.clone());
        worktree::default_branch(&root)
    });

    let target =
        util::git_out(&wt, &["merge-base", &base, "HEAD"]).unwrap_or_else(|| "HEAD".to_string());

    // --files: TSV of modified files
    if files {
        let output = util::git_out(&wt, &["diff", "--name-status", &target])
            .unwrap_or_default();
        println!("{output}");
        return Ok(());
    }

    // --file <path>: full diff of a single file
    if let Some(fp) = file_path {
        let mut args = vec!["-c", "color.ui=always", "diff", &target, "--", &fp];
        if util::have("delta") {
            let cmd = format!(
                "git -c color.ui=always diff {} -- {} | delta --paging=never --color-only",
                target,
                shell_words(&fp),
            );
            let _ = Command::new("sh").arg("-c").arg(&cmd).current_dir(&wt).status();
        } else {
            run_git(&wt, &args);
        }
        return Ok(());
    }

    // Existing behavior: full diff or --stat
    if !stat && util::have("delta") {
        let cmd = format!(
            "git -c color.ui=always diff {target} | delta --paging=never --color-only"
        );
        let _ = Command::new("sh").arg("-c").arg(&cmd).current_dir(&wt).status();
        return Ok(());
    }

    let mut args = vec!["-c", "color.ui=always", "diff"];
    if stat {
        args.push("--stat");
    }
    args.push(&target);
    run_git(&wt, &args);
    Ok(())
}
```

Add a helper for shell-quoting file paths:

```rust
/// Simple shell-quoting: wrap in single quotes, escaping internal single quotes.
fn shell_words(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}
```

- [ ] **Step 3: Update main.rs dispatch**

In `src/main.rs`, update the `Diff` match arm:

```rust
Command::Diff {
    worktree,
    base,
    stat,
    files,
    file,
} => commands::diff::run(worktree, base, stat, files, file),
```

- [ ] **Step 4: Build and test**

```bash
cargo build --release
# Test --files:
./target/release/thegn diff --files --worktree /tmp/some-repo 2>/dev/null || \
  echo "Run from a git repo to test: thegn diff --files"
# Test --file:
./target/release/thegn diff --file README.md 2>/dev/null || echo "(needs git context)"
```

- [ ] **Step 5: Commit**

```bash
git add src/cli.rs src/commands/diff.rs src/main.rs
git commit -m "feat(diff): add --files and --file flags for panel data"
```

---

### Task 3: FileList View in the Panel

**Files:**

- Modify: `plugin/panel/src/main.rs`

Add modified-files fetching, parsing, and render with cursor navigation.

- [ ] **Step 1: Add files fetch + result handling**

In the `impl State` block, add `fetch_files()` and update `on_result`:

```rust
fn fetch_files(&self) {
    let Some(wt) = self.worktree.clone() else { return };
    let mut ctx = BTreeMap::new();
    ctx.insert("cmd".to_string(), "files".to_string());
    run_command(&["thegn", "diff", "--files", "--worktree", &wt], ctx);
}
```

Update `fetch()` to also call `fetch_files()`:

```rust
fn fetch(&mut self, refresh: bool) {
    let Some(wt) = self.worktree.clone() else { return };
    // PR fetch (existing)
    let mut pr_ctx = BTreeMap::new();
    pr_ctx.insert("cmd".to_string(), "pr".to_string());
    let mut pr_cmd = vec!["thegn", "pr", "status", "--json", "--worktree", &wt];
    if refresh { pr_cmd.push("--refresh"); }
    run_command(&pr_cmd, pr_ctx);

    // Files fetch (new)
    let mut file_ctx = BTreeMap::new();
    file_ctx.insert("cmd".to_string(), "files".to_string());
    run_command(&["thegn", "diff", "--files", "--worktree", &wt], file_ctx);

    // Diff --stat (existing)
    let mut diff_ctx = BTreeMap::new();
    diff_ctx.insert("cmd".to_string(), "diff".to_string());
    run_command(&["thegn", "diff", "--stat", "--worktree", &wt], diff_ctx);
}
```

Add the `"files"` handler to `on_result`:

```rust
Some("files") => {
    self.files = text
        .lines()
        .filter_map(|l| {
            let (status, path) = l.split_once('\t')?;
            Some(FileEntry {
                status: status.chars().next().unwrap_or('?'),
                path: path.to_string(),
            })
        })
        .collect();
    // Reset cursor when file list changes
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
```

Add `"file_diff"` context to the fetch_file_diff call:

```rust
fn fetch_file_diff(&self, path: &str) {
    let Some(wt) = self.worktree.clone() else { return };
    let mut ctx = BTreeMap::new();
    ctx.insert("cmd".to_string(), "file_diff".to_string());
    run_command(&["thegn", "diff", "--file", path, "--worktree", &wt], ctx);
}
```

- [ ] **Step 2: Implement `render_diff_tab` FileList view**

Replace the stub:

```rust
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

    // Calculate visible range
    let header_rows = 1; // worktree path
    let footer_rows = 1; // help bar (already rendered outside)
    let available = rows.saturating_sub(header_rows + footer_rows);
    let scroll_off = self.diff_scroll.min(
        self.files.len().saturating_sub(available).max(0),
    );
    let visible = &self.files[scroll_off..]
        .iter()
        .take(available)
        .collect::<Vec<_>>();

    for (i, f) in visible.iter().enumerate() {
        let abs_idx = scroll_off + i;
        let is_cursor = self.focused && abs_idx == self.diff_scroll;
        let line = format!(
            "{}{}",
            match f.status {
                'M' => "\u{1b}[33mM\u{1b}[0m",  // yellow
                'A' => "\u{1b}[32mA\u{1b}[0m",  // green
                'D' => "\u{1b}[31mD\u{1b}[0m",  // red
                'R' => "\u{1b}[34mR\u{1b}[0m",  // blue
                'C' => "\u{1b}[35mC\u{1b}[0m",  // magenta
                _ => "?",
            },
            f.path
        );
        if is_cursor {
            // Highlight line with selection bg
            let full = format!("\u{1b}[48;2;40;44;62m  {line}\u{1b}[0m");
            push(out, &full, cols);
        } else {
            push(out, &format!("  {line}"), cols);
        }
    }
}
```

- [ ] **Step 3: Add cursor navigation to `on_diff_key`**

```rust
fn on_diff_key(&mut self, c: char) -> bool {
    let Some(_wt) = self.worktree.clone() else { return false };
    match self.diff_view {
        DiffView::FileList => self.on_file_list_key(c),
        DiffView::FileDiff => self.on_file_diff_key(c),
    }
}

fn on_file_list_key(&mut self, c: char) -> bool {
    let Some(_wt) = self.worktree.clone() else { return false };
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
            if let Some(entry) = self.files.get(self.diff_scroll) {
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
        'f' => { self.fetch(true); self.status_line = "refreshing…".into(); true }
        _ => false,
    }
}
```

Add the `open_in_editor` helper:

```rust
fn open_in_editor(&self, path: &str) {
    let Some(wt) = &self.worktree else { return };
    let editor = std::env::var("EDITOR")
        .or_else(|_| std::env::var("VISUAL"))
        .unwrap_or_else(|_| "vi".to_string());
    let mut cmd = vec!["zellij", "run", "--floating", "--close-on-exit", "--cwd", wt, "--", &editor, path];
    run_command(&cmd, BTreeMap::new());
}
```

- [ ] **Step 4: Update `render` for cursor guard**

In `render()`, add a call to clamp the cursor before rendering:

After the tab bar + rule, before the tab dispatch:

```rust
// Clamp cursor before rendering
if self.current_tab == Tab::Diff && self.diff_view == DiffView::FileList {
    if !self.files.is_empty() {
        self.diff_scroll = self.diff_scroll.min(self.files.len() - 1);
    }
}
```

- [ ] **Step 5: Commit**

```bash
git add plugin/panel/src/main.rs
git commit -m "feat(panel): file list view with cursor navigation and drill-in"
```

---

### Task 4: FileDiff View with Scrolling

**Files:**

- Modify: `plugin/panel/src/main.rs`

Add the FileDiff render with half-page scrolling, Esc back, and scroll indicator.

- [ ] **Step 1: Implement `render_file_diff`**

```rust
fn render_file_diff(&self, out: &mut String, rows: usize, cols: usize) {
    let Some(wt) = &self.worktree else {
        push(out, "  (focus a worktree tab)", cols);
        return;
    };
    // Header line with path and Esc hint
    let header = format!(
        "{}  \u{1b}[2m[Esc back]\u{1b}[0m",
        short(wt)
    );
    push(out, &header, cols);

    // File name separator
    let selected_path = self.files.get(self.diff_scroll).map(|f| f.path.as_str())
        .unwrap_or("");
    let sep = format!("\u{1b}[38;2;{}m── {selected_path} ──\u{1b}[0m", CYAN);
    push(out, &sep, cols);

    // Diff body with half-page scroll
    let consumed = 3; // worktree path + separator + empty line start
    let available = rows.saturating_sub(consumed + 1); // +1 for help bar
    if available == 0 { return; }

    let diff_lines: Vec<&str> = self.file_diff.lines().collect();
    if diff_lines.is_empty() {
        push(out, "  (no diff — untracked or binary file)", cols);
        return;
    }

    let max_scroll = diff_lines.len().saturating_sub(available);
    let scroll = self.diff_scroll.min(max_scroll);
    let visible = &diff_lines[scroll..].iter().take(available).copied().collect::<Vec<_>>();

    for line in visible {
        // Clean ANSI reset at end already handled by push()
        push(out, line, cols);
    }

    // Scroll indicator
    if max_scroll > 0 {
        let pct = if diff_lines.is_empty() { 0 } else {
            (scroll as f64 / diff_lines.len() as f64 * 100.0) as usize
        };
        push(
            out,
            &format!("\u{1b}[2m── {pct}% ──\u{1b}[0m"),
            cols,
        );
    }
}
```

- [ ] **Step 2: Add scroll keys to `on_file_diff_key`**

```rust
fn on_file_diff_key(&mut self, c: char) -> bool {
    let Some(_wt) = self.worktree.clone() else { return false };
    match c {
        'j' | 'J' => {
            // Half-page scroll down
            let diff_lines = self.file_diff.lines().count();
            let max_scroll = diff_lines.saturating_sub(1);
            let step = max_scroll.saturating_div(2).max(1);
            self.diff_scroll = (self.diff_scroll + step).min(max_scroll);
            true
        }
        'k' | 'K' => {
            // Half-page scroll up
            let step = self.diff_scroll.saturating_div(2).max(1);
            self.diff_scroll = self.diff_scroll.saturating_sub(step);
            true
        }
        '\u{1b}' => {
            // Esc — back to file list
            self.diff_view = DiffView::FileList;
            self.file_diff.clear();
            self.diff_scroll = 0;
            true
        }
        'o' | 'O' => {
            if let Some(entry) = self.files.get(self.diff_scroll) {
                self.open_in_editor(&entry.path);
            }
            true
        }
        'f' => { self.fetch(true); self.status_line = "refreshing…".into(); true }
        _ => false,
    }
}
```

Note: In zellij-tile, Esc arrives as `BareKey::Esc` not `BareKey::Char('\u{1b}')`. Let me use the proper variant:

Actually, looking at the existing code, `BareKey` enum variants are imported from `zellij_tile::prelude::*`. Let me check what Esc looks like. The existing on_key handler matches on `BareKey::Char`. Esc might be `BareKey::Esc`.

Let me adjust - I need to restructure on_key slightly. Let me change the approach: handle Esc at the `on_key` level before dispatching:

```rust
fn on_key(&mut self, key: KeyWithModifier) -> bool {
    let bare = key.bare_key;
    // Handle Esc first (universal back)
    if bare == BareKey::Esc {
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
    let BareKey::Char(c) = bare else { return false };
    // ... rest of tab switching and dispatching
}
```

- [ ] **Step 3: Commit**

```bash
git add plugin/panel/src/main.rs
git commit -m "feat(panel): file diff view with half-page scroll and Esc back"
```

---

### Task 5: PR Tab — Port Existing Render

**Files:**

- Modify: `plugin/panel/src/main.rs`

Port the existing PR rendering and action keybindings from the old single-view layout into the dedicated PR tab. The `render_pr` method needs minimal changes — it gets called from `render_pr_tab` instead of `render`.

- [ ] **Step 1: Update `render_pr_tab`**

Replace the stub:

```rust
fn render_pr_tab(&mut self, out: &mut String, _rows: usize, cols: usize) {
    let Some(wt) = &self.worktree else {
        push(out, "  (focus a worktree tab)", cols);
        return;
    };
    push(out, &format!("\u{1b}[2m{}\u{1b}[0m", short(wt)), cols);
    self.render_pr(out, cols);
}
```

The existing `render_pr` method is already correct for the PR data format. No changes needed — it reads `self.pr` and renders the PR status block.

- [ ] **Step 2: Verify `on_pr_key` is already wired**

The `on_pr_key` method was set up in Task 1 with `o`/`c`/`m`/`a`/`r`/`f` bindings. Verify it matches the old panel's action keys:

| Key | Old action                                    | New action                    |
| --- | --------------------------------------------- | ----------------------------- |
| `o` | `["pr", "open"]`                              | `["pr", "open"]` ✓            |
| `c` | floating `["pr", "create"]`                   | floating `["pr", "create"]` ✓ |
| `m` | floating `["pr", "merge", "--delete-branch"]` | same ✓                        |
| `a` | floating `["pr", "approve"]`                  | same ✓                        |
| `r` | `["pr", "rerun-checks"]`                      | same ✓                        |
| `f` | force refresh                                 | same ✓                        |

The `action_inline` and `action_floating` methods already exist from the old panel — no changes needed.

- [ ] **Step 3: Commit**

```bash
git add plugin/panel/src/main.rs
git commit -m "feat(panel): PR tab with actions ported from old panel"
```

---

### Task 6: Checks Tab — Detailed Check List

**Files:**

- Modify: `plugin/panel/src/main.rs`

Render individual check runs from `statusCheckRollup` in the cached PR data.

- [ ] **Step 1: Implement `render_checks_tab`**

Replace the stub:

```rust
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

    for check in arr {
        let name = check.get("name").and_then(|v| v.as_str()).unwrap_or("?");
        let status = check.get("status").and_then(|v| v.as_str()).unwrap_or("");
        let conclusion = check.get("conclusion").and_then(|v| v.as_str());
        let state = check.get("state").and_then(|v| v.as_str());

        let (icon, color) = if let Some(c) = conclusion {
            match c.to_uppercase().as_str() {
                "SUCCESS" | "NEUTRAL" | "SKIPPED" => ("✔", "32"),
                "FAILURE" | "TIMED_OUT" | "CANCELLED" | "STARTUP_FAILURE" => ("✗", "31"),
                _ => ("⧗", "33"),
            }
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

    if arr.is_empty() {
        push(out, "  no checks to display", cols);
    }
}
```

- [ ] **Step 2: Verify `on_checks_key` is already wired**

The handler from Task 1 has `r` → rerun and `f` → refresh. This is sufficient for a first implementation.

- [ ] **Step 3: Commit**

```bash
git add plugin/panel/src/main.rs
git commit -m "feat(panel): checks tab with individual check run status"
```

---

### Task 7: Timer Refresh for All Data Sources

**Files:**

- Modify: `plugin/panel/src/main.rs`

The timer event (every 15 seconds) should refresh files and PR data without resetting the user's current view.

- [ ] **Step 1: Update timer handler**

The existing `Event::Timer` handler already calls `self.fetch(false)`. The `fetch` method (updated in Task 3) now fires `pr`, `files`, and `diff --stat` commands. Since the timer runs in the background and results arrive asynchronously via `RunCommandResult`, the current view state (tab, diff_view, diff_scroll) is preserved naturally — only `self.files`, `self.pr`, and `self.diff` are overwritten.

No code change needed — the existing timer handler already calls `fetch(false)`:

```rust
Event::Timer(_) => {
    set_timeout(REFRESH_SECS);
    if self.worktree.is_some() {
        self.fetch(false);
    }
    false
}
```

This already works correctly because:

- `RunCommandResult` updates specific fields (`self.files`, `self.pr`, `self.diff`) without touching navigation state
- `DiffView::FileDiff` is reset to `FileList` in the `"files"` handler only when files change (which is acceptable — file list changes mean positions may shift)
- `diff_scroll` is preserved across refreshes

- [ ] **Step 2: Verify build**

```bash
cd plugin/panel && cargo build --release --target wasm32-wasip1 2>&1 || echo "fix errors"
```

- [ ] **Step 3: Full integration smoke check**

```bash
cd /home/blake/code/thegn
cargo build --release
# List modified files (run from a git repo):
./target/release/thegn diff --files
# Single file diff:
./target/release/thegn diff --file src/main.rs
```

- [ ] **Step 4: Commit**

```bash
git add plugin/panel/src/main.rs
git commit -m "chore(panel): ensure timer refreshes all data sources"
```

---

### Spec Coverage

- **Tab bar** → Task 1 (tab_bar helper, three tabs, active/inactive styling)
- **`1`/`2`/`3` tab switching** → Task 1 (on_key digit dispatch)
- **Tab cycle** → Task 1 (`\t` handler)
- **Tab-PR rendering** → Task 5 (port existing render_pr)
- **Tab-PR actions** → Task 1 `on_pr_key` (o/c/m/a/r/f)
- **Tab-Checks list** → Task 6 (render_checks_tab with rollup)
- **Tab-Checks rerun** → Task 1 `on_checks_key` (`r` key)
- **`--files` CLI flag** → Task 2 (diff.rs)
- **`--file` CLI flag** → Task 2 (diff.rs)
- **FileList render** → Task 3 (render_file_list)
- **FileList cursor nav** → Task 3 (on_file_list_key j/k)
- **FileList Enter drill** → Task 3 (on_file_list_key Enter → fetch_file_diff)
- **FileList `o` edit** → Task 3 (open_in_editor)
- **FileDiff render** → Task 4 (render_file_diff)
- **FileDiff half-page scroll** → Task 4 (on_file_diff_key j/k)
- **FileDiff Esc back** → Task 4 (on_key Esc handler)
- **FileDiff `o` edit** → Task 4 (on_file_diff_key o)
- **Help bar** → Task 1 (help_bar method)
- **Timer refresh** → Task 7 (existing timer still works)
- **Edge: deleted/untracked files** → Task 3 (status char colors), Task 4 ("no diff" message)
- **Edge: no worktree** → Task 1 (guard in all tab renders)
