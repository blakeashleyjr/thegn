//! The right panel: the accordion sidebar over the focused worktree — a
//! branch-header zone, numbered one-line sections (changes · git · files ·
//! tests · debug · sandbox · db · telemetry · keys) with live summaries and
//! the open section's content beneath it. `[panel] sections` in the config
//! reorders the accordion and hides sections entirely.
//!
//! Split into two halves:
//! - [`PanelData`] — the git/GitHub payload, rebuilt by the host's background
//!   model hydration and carried on the `FrameModel`. Cheap to clone, `Send`.
//! - [`PanelUi`] — the interactive state (open section, panel width, row
//!   cursor, the banked hunk previews). Owned by the event loop so it
//!   survives data refreshes.
//!
//! Rendering lives in `chrome.rs` next to the other `draw_*` surfaces; this
//! module owns the data model + the pure key→intent navigation logic.
//! `budget` owns the pure vertical-allocation math.

pub mod budget;
pub mod docs;
pub mod frame;
pub mod gitfull;
pub mod gitui;
pub mod graph;
pub mod sections;
pub mod staging;

use termwiz::input::{KeyCode, Modifiers};

/// A mouse/row target inside the rendered panel. The frame builder attaches
/// these to rows; the loop resolves clicks and row-mode Enter against them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PanelHit {
    /// A closed (or open) section's one-line row.
    OpenSection(Section),
    /// The "… +N more · e expand" overflow row.
    Expand,
    /// The i-th actionable row of a section's content.
    Row(Section, usize),
}

/// One of the accordion sections, in built-in display order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Section {
    #[default]
    Changes,
    Commits,
    Branches,
    Stash,
    Git,
    Files,
    Tests,
    Debug,
    Sandbox,
    Db,
    Telemetry,
    Keys,
}

/// The accordion's built-in display order — the default when `[panel]
/// sections` is unset. The live order (config-reordered, possibly trimmed)
/// rides on [`PanelUi::order`]; the numbered jump keys index THAT.
pub const SECTION_ORDER: [Section; 12] = [
    Section::Changes,
    Section::Commits,
    Section::Branches,
    Section::Stash,
    Section::Git,
    Section::Files,
    Section::Tests,
    Section::Debug,
    Section::Sandbox,
    Section::Db,
    Section::Telemetry,
    Section::Keys,
];

impl Section {
    /// The one-line row label.
    pub fn label(self) -> &'static str {
        match self {
            Section::Changes => "changes",
            Section::Commits => "commits",
            Section::Branches => "branches",
            Section::Stash => "stash",
            Section::Git => "git",
            Section::Files => "files",
            Section::Tests => "tests",
            Section::Debug => "debug",
            Section::Sandbox => "sandbox",
            Section::Db => "db",
            Section::Telemetry => "telemetry",
            Section::Keys => "keys",
        }
    }

    /// The lazygit-family sections that share [`gitui::GitUi`] state and the
    /// Full-width git frame.
    pub fn is_git_family(self) -> bool {
        matches!(
            self,
            Section::Changes | Section::Commits | Section::Branches | Section::Stash | Section::Git
        )
    }

    /// The git context a git-family section's list maps to.
    pub fn home_view(self) -> Option<gitui::GitView> {
        match self {
            Section::Changes => Some(gitui::GitView::Files),
            Section::Commits => Some(gitui::GitView::Commits),
            Section::Branches => Some(gitui::GitView::Branches),
            Section::Stash => Some(gitui::GitView::Stash),
            _ => None,
        }
    }

    /// Stable id for ui_state persistence and `[panel] sections` config keys.
    pub fn as_key(self) -> &'static str {
        self.label()
    }

    pub fn from_key(key: &str) -> Option<Section> {
        SECTION_ORDER.iter().copied().find(|s| s.as_key() == key)
    }
}

/// Resolve `[panel] sections` into the live accordion order: config keys map
/// to sections (unknown keys ignored, duplicates collapse to their first
/// position); an empty/absent list — or one that names no real section —
/// falls back to the built-in order. Sections left out are hidden.
pub fn resolve_order(cfg: &superzej_core::config::Config) -> Vec<Section> {
    let mut out: Vec<Section> = Vec::new();
    for key in &cfg.panel.sections {
        if let Some(s) = Section::from_key(key)
            && !out.contains(&s)
        {
            out.push(s);
        }
    }
    if out.is_empty() {
        out.extend(SECTION_ORDER);
    }
    out
}

/// Where a changed file sits in the index.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stage {
    Staged,
    Unstaged,
    Conflict,
    Untracked,
}

/// One row of the changes section: porcelain status joined with the
/// diffstat, path split for the dim-dir + bright-name rendering.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChangeRow {
    /// Display status ("M", "A", "D", "!U", "?").
    pub status: String,
    pub stage: Stage,
    /// Directory prefix (possibly shortened), "" for repo-root files.
    pub dir: String,
    /// File name (the bright part).
    pub name: String,
    /// Full repo-relative path (the action target).
    pub path: String,
    pub added: u32,
    pub deleted: u32,
}

/// A merge/rebase/cherry-pick in progress, for the header zone.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MergeBanner {
    /// Header chip text ("MERGING", "REBASING", "CHERRY-PICK").
    pub label: String,
    /// What is being merged in (best-effort; may be empty).
    pub onto: String,
    /// Files still in conflict.
    pub unresolved: usize,
    /// First-seen unresolved count this merge ("resolved X/Y" bar);
    /// `None` hides the progress bar.
    pub total: Option<usize>,
}

/// A trimmed test snapshot for the tests section (read from `test_cache`
/// without the full explorer tree).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TestsLite {
    pub passed: usize,
    pub failed: usize,
    pub skipped: usize,
    pub error: Option<String>,
    /// `(name, location)` of failing tests, capped.
    pub failures: Vec<(String, String)>,
    /// Recent runs, newest first.
    pub history: Vec<crate::testkit::model::TestRunRec>,
}

// The Tests model (TestState/TestTask/TestNode/TestPanelState + parsers, tree,
// and locate helpers) lives in `testkit::model` and is re-exported so the panel
// and chrome keep using `crate::panel::TestNode` etc.
pub use crate::testkit::model::*;

/// A pass/fail/pending tri-state mirrored from `github::Bucket` (decoupled so
/// the host doesn't depend on that type in its render path).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(dead_code)]
pub enum CheckState {
    Pass,
    Fail,
    Pending,
}

/// One changed file in the Diff tab's file list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiffFile {
    /// A single-char status glyph: `A` added, `D` deleted, `M` modified.
    pub status: char,
    pub path: String,
    pub added: u32,
    pub deleted: u32,
}

/// One CI check row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CheckLine {
    pub name: String,
    pub state: CheckState,
    /// Seconds run (completed) or running-for (pending), when known.
    pub duration_secs: Option<i64>,
    pub details_url: Option<String>,
}

/// A compact PR summary for the PR tab.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PrSummary {
    pub number: u64,
    pub title: String,
    pub state: String, // OPEN | CLOSED | MERGED
    pub url: String,
    pub is_draft: bool,
    pub review_decision: Option<String>,
}

/// A branch row's PR badge, joined from the per-repo `pr_branch_cache`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PrBadge {
    pub number: u64,
    pub state: String,
    pub is_draft: bool,
    pub url: String,
}

/// One row of the branches section.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BranchRow {
    pub name: String,
    pub is_head: bool,
    pub upstream: Option<String>,
    pub ahead: usize,
    pub behind: usize,
    pub upstream_gone: bool,
    pub sha: String,
    pub date: i64,
    pub subject: String,
    pub pr: Option<PrBadge>,
}

/// One row of the commits section (structured, parents included for the
/// graph).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct CommitRow {
    pub sha: String,
    pub short: String,
    pub subject: String,
    pub author: String,
    pub date: i64,
    pub refs: String,
    pub parents: Vec<String>,
}

/// One row of the stash section.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StashRow {
    pub index: usize,
    pub sha: String,
    pub date: i64,
    pub message: String,
}

/// The panel's data payload (git + GitHub), rebuilt on background refresh.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PanelData {
    pub branch: String,
    pub files: Vec<DiffFile>,
    /// `Some` when a PR exists; `None` otherwise (see `pr_note`).
    pub pr: Option<PrSummary>,
    /// A short human note when there's no PR ("no pull request", "gh not
    /// authenticated", an error). Shown in the git section body.
    pub pr_note: Option<String>,
    pub checks: Vec<CheckLine>,
    /// Commits `(ahead, behind)` upstream; `None` without a tracking branch.
    pub ahead_behind: Option<(usize, usize)>,
    /// A merge/rebase/cherry-pick in progress (header zone).
    pub merge: Option<MergeBanner>,
    /// Porcelain status joined with the diffstat (changes section).
    pub changes: Vec<ChangeRow>,
    pub stash_count: usize,
    /// Recent `git log --graph` rows (git section LOG block).
    pub log: Vec<superzej_svc::git::LogRow>,
    /// PR base ref ("main") and head→base diffstat, when a PR exists.
    pub pr_base: String,
    /// Review threads (unresolved first) and open issues, from the PR cache.
    pub threads: Vec<superzej_core::github::ReviewThreadRow>,
    pub issues: Vec<superzej_core::github::IssueRow>,
    /// Trimmed test snapshot (summary + failures + history) from test_cache.
    pub tests: Option<TestsLite>,
    /// Total tracked-file count for the files summary ("214 · 29.5k loc").
    pub file_count: Option<u64>,
    /// All tracked files from `git ls-files` — populated while Files is open.
    /// The Files section renders this as a collapsible tree with changed-file
    /// highlights drawn from [`changes`].
    pub all_files: Vec<String>,
    /// Local branches with upstream/divergence + PR badges (branches section).
    pub branches: Vec<BranchRow>,
    /// Structured recent commits (commits section + graph feed). Loaded from
    /// cache synchronously; refreshed by a background `git log` worker.
    pub commits: Vec<CommitRow>,
    /// True when the commits section is visible but the cache is missing/stale
    /// and a background refresh has been kicked.
    pub commits_loading: bool,
    /// Stash entries (stash section).
    pub stashes: Vec<StashRow>,
}

/// One row of the Files tab's accordion tree, in display order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileEntry {
    /// Repo-relative path ("src/run.rs", dirs without trailing slash).
    pub path: String,
    /// Leaf label shown in the row.
    pub name: String,
    /// Nesting depth (top-level entries are 0).
    pub depth: u8,
    pub is_dir: bool,
}

/// Flatten a sorted list of repo-relative FILE paths (à la `git ls-files`)
/// into a display-ordered tree with synthesized directory rows.
pub fn build_file_tree(paths: &[String]) -> Vec<FileEntry> {
    let mut out: Vec<FileEntry> = Vec::new();
    let mut sorted: Vec<&String> = paths.iter().filter(|p| !p.is_empty()).collect();
    // Order directories before their contents and group siblings: comparing
    // component-wise on the split path achieves both with plain sort.
    sorted.sort_by(|a, b| {
        a.split('/')
            .collect::<Vec<_>>()
            .cmp(&b.split('/').collect::<Vec<_>>())
    });
    sorted.dedup();
    let mut known_dirs: std::collections::HashSet<String> = std::collections::HashSet::new();
    for path in sorted {
        let parts: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
        if parts.is_empty() {
            continue;
        }
        // Synthesize any missing ancestor dir rows.
        for d in 1..parts.len() {
            let dir = parts[..d].join("/");
            if known_dirs.insert(dir.clone()) {
                out.push(FileEntry {
                    name: parts[d - 1].to_string(),
                    path: dir,
                    depth: (d - 1) as u8,
                    is_dir: true,
                });
            }
        }
        out.push(FileEntry {
            name: parts[parts.len() - 1].to_string(),
            path: path.clone(),
            depth: (parts.len() - 1) as u8,
            is_dir: false,
        });
    }
    out
}

/// Filter a full file tree to only the rows visible given the collapsed set.
/// A row is hidden when any ancestor directory path is in `collapsed`.
/// Returns pairs of `(original_index, entry_ref)` in display order.
pub fn file_tree_visible<'a>(
    tree: &'a [FileEntry],
    collapsed: &std::collections::HashSet<String>,
) -> Vec<(usize, &'a FileEntry)> {
    tree.iter()
        .enumerate()
        .filter(|(_, e)| {
            // An entry is visible iff none of its ancestor dir paths are collapsed.
            let parts: Vec<&str> = e.path.split('/').collect();
            // Check every prefix up to (but not including) the entry itself.
            let ancestor_count = if e.is_dir {
                parts.len().saturating_sub(1)
            } else {
                parts.len().saturating_sub(1)
            };
            for d in 1..=ancestor_count {
                if collapsed.contains(&parts[..d].join("/")) {
                    return false;
                }
            }
            true
        })
        .collect()
}

/// The panel's interactive state, owned by the event loop.
#[derive(Debug, Clone)]
pub struct PanelUi {
    /// The full test-explorer state (detected task, per-test status map,
    /// display tree, cursor/scroll/filter) backing the tests section's runs.
    pub tests: TestPanelState,
    /// The live accordion order (config-resolved, possibly trimmed); the
    /// numbered jump keys (`1..=order.len()`) index this. Never empty.
    pub order: Vec<Section>,
    /// Which accordion section is open (exactly one).
    pub open: Section,
    /// The view selector, cycled by `e` (Normal → Half → Full): every section
    /// renders a distinct body per width — compact, deep, and full-screen.
    pub width: crate::layout::PanelWidth,
    /// True while the cursor walks the open section's rows instead of the
    /// section list.
    pub row_mode: bool,
    /// Row cursor within the open section (row-mode).
    pub cursor: usize,
    /// Highlighted change row (inlines its hunk preview), changes section.
    pub chg_sel: Option<usize>,
    /// Inline hunk previews by path, filled by the background hunk fetch.
    pub hunks: std::collections::HashMap<String, Vec<superzej_svc::git::Hunk>>,
    /// Acceptance cutoff for arriving hunk fetches: results tagged with an
    /// older generation (pre-worktree-switch strays) are dropped.
    pub hunks_gen: u64,
    /// Scroll offset for the tall full-view bodies (sbs diff, git log, the
    /// cheatsheet). Reset on section open and width cycle.
    pub scroll: usize,
    /// Current hunk in the changes section's full side-by-side view.
    pub diff_hunk: usize,
    /// Loop-fetched documents the section bodies render from.
    pub docs: docs::PanelDocs,
    /// The git-family interaction state (lazygit contexts, marks, flows).
    pub git: gitui::GitUi,
    /// Repo-relative dir paths that are collapsed in the Files accordion tree.
    /// Persisted to the DB (`ui_state` table, prefix `panel.files.col/`) so the
    /// tree survives restarts.
    pub files_collapsed: std::collections::HashSet<String>,
}

impl Default for PanelUi {
    fn default() -> Self {
        PanelUi {
            tests: TestPanelState::default(),
            order: SECTION_ORDER.to_vec(),
            open: Section::default(),
            width: crate::layout::PanelWidth::default(),
            row_mode: false,
            cursor: 0,
            chg_sel: None,
            hunks: std::collections::HashMap::new(),
            hunks_gen: 0,
            scroll: 0,
            diff_hunk: 0,
            docs: docs::PanelDocs::default(),
            git: gitui::GitUi::default(),
            files_collapsed: std::collections::HashSet::new(),
        }
    }
}

impl PanelUi {
    /// Reset when focus leaves the panel: drop back to section mode and
    /// dismiss the change preview. The open section, width, and row cursor
    /// are intentionally kept so returning lands where you left off.
    pub fn reset_on_leave(&mut self) {
        self.row_mode = false;
        self.chg_sel = None;
    }

    /// Adopt a config-resolved order; the open section snaps to the first
    /// visible one when the config hid it.
    pub fn set_order(&mut self, order: Vec<Section>) {
        debug_assert!(!order.is_empty());
        if !order.contains(&self.open)
            && let Some(&first) = order.first()
        {
            self.open = first;
        }
        self.order = order;
    }

    /// The position of the open section in the live order (0 when stale).
    fn open_index(&self) -> usize {
        self.order.iter().position(|s| *s == self.open).unwrap_or(0)
    }

    /// The section after the open one, wrapping within the live order.
    pub fn next_section(&self) -> Section {
        self.order[(self.open_index() + 1) % self.order.len()]
    }

    /// The section before the open one, wrapping within the live order.
    pub fn prev_section(&self) -> Section {
        self.order[(self.open_index() + self.order.len() - 1) % self.order.len()]
    }
}

/// Join porcelain status with the diffstat into display-ordered change rows:
/// staged → unstaged → conflicts → untracked, path-ordered within a group.
pub fn build_change_rows(
    status: &[superzej_svc::git::FileStatus],
    diff: &[superzej_svc::git::DiffEntry],
) -> Vec<ChangeRow> {
    let counts: std::collections::HashMap<&str, (u32, u32)> = diff
        .iter()
        .map(|d| (d.path.as_str(), (d.added, d.deleted)))
        .collect();
    let mut rows: Vec<ChangeRow> = status
        .iter()
        .map(|f| {
            let stage = if superzej_svc::git::is_conflict(f) {
                Stage::Conflict
            } else if f.staged == '?' || f.unstaged == '?' {
                Stage::Untracked
            } else if f.staged != ' ' && f.unstaged == ' ' {
                Stage::Staged
            } else {
                Stage::Unstaged
            };
            let status_str = match stage {
                Stage::Conflict => "!U".to_string(),
                Stage::Untracked => "?".to_string(),
                Stage::Staged => f.staged.to_string(),
                Stage::Unstaged => (if f.unstaged != ' ' {
                    f.unstaged
                } else {
                    f.staged
                })
                .to_string(),
            };
            let (dir, name) = match f.path.rsplit_once('/') {
                Some((d, n)) => (format!("{d}/"), n.to_string()),
                None => (String::new(), f.path.clone()),
            };
            let (added, deleted) = counts.get(f.path.as_str()).copied().unwrap_or((0, 0));
            ChangeRow {
                status: status_str,
                stage,
                dir,
                name,
                path: f.path.clone(),
                added,
                deleted,
            }
        })
        .collect();
    let group = |s: Stage| match s {
        Stage::Staged => 0u8,
        Stage::Unstaged => 1,
        Stage::Conflict => 2,
        Stage::Untracked => 3,
    };
    rows.sort_by(|a, b| {
        group(a.stage)
            .cmp(&group(b.stage))
            .then(a.path.cmp(&b.path))
    });
    rows
}

/// Trim a full test cache into the section snapshot: summary numbers, the
/// failing tests (name + file:line), and the run history.
pub fn tests_lite(cache: &crate::testkit::model::TestCache) -> TestsLite {
    const FAILURE_CAP: usize = 6;
    let failures = cache
        .nodes
        .iter()
        .filter(|n| n.kind == TestNodeKind::Test && n.state == TestState::Fail)
        .take(FAILURE_CAP)
        .map(|n| {
            let at = n
                .location
                .as_ref()
                .map(|l| format!("{}:{}", l.path, l.line))
                .or_else(|| {
                    n.message
                        .as_ref()
                        .map(|m| m.lines().next().unwrap_or("").to_string())
                })
                .unwrap_or_default();
            (n.label.clone(), at)
        })
        .collect();
    TestsLite {
        passed: cache.summary.passed,
        failed: cache.summary.failed,
        skipped: cache.summary.skipped,
        error: cache.summary.error.clone(),
        failures,
        history: cache.history.clone(),
    }
}

/// A decoded accordion intent. `None` falls through to the loop's
/// per-section action keys, then the global keymap (e.g. Esc in section mode
/// leaves the panel zone).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PanelMsg {
    /// Open a section (mouse click / jump key).
    Open(Section),
    /// Shift+Down/j (Shift+Up/k): hop to the next/previous section.
    NextSection,
    PrevSection,
    /// Esc: dismiss an expanded change preview, else leave the panel zone.
    LeaveRows,
    /// Down/j (Up/k): step the cursor through the open section's items.
    CursorDown,
    CursorUp,
    /// Enter on a row (changes: toggle the hunk preview; git: jump to a
    /// thread; files: open/fold; tests: open the failing test).
    Select,
    /// `e`: cycle the view width (Normal → Half → Full).
    ToggleExpand,
    /// Space in the changes section: stage/unstage the selected file.
    StageToggle,
}

/// Map a raw key (plus modifiers) to an accordion intent. Pure; per-section
/// *action* keys (run tests, merge PR, …) are resolved by the event loop on
/// top of this.
///
/// Navigation is single-layered. Plain Down/j (Up/k) walk the panel as one
/// flat list: step through the open section's items, then flow into the
/// adjacent accordion (every accordion is visited — one with no items is just
/// passed through). Shift+Down/j (Shift+Up/k) skip straight to the next/
/// previous accordion header. Enter activates the highlighted row; Esc leaves.
pub fn accordion_key(key: &KeyCode, mods: Modifiers, ui: &PanelUi) -> Option<PanelMsg> {
    let shift = mods.contains(Modifiers::SHIFT);
    // Always-available jumps (the view cycle, numbered section jumps) read
    // the same wherever the panel is.
    match key {
        KeyCode::Char('e') => return Some(PanelMsg::ToggleExpand),
        KeyCode::Char(c @ '1'..='9') => {
            let idx = (*c as usize) - ('1' as usize);
            if let Some(&s) = ui.order.get(idx) {
                return Some(PanelMsg::Open(s));
            }
        }
        _ => {}
    }
    match key {
        // Shift hops between sections; plain steps through the open one's rows.
        KeyCode::DownArrow | KeyCode::Char('j' | 'J') if shift => Some(PanelMsg::NextSection),
        KeyCode::UpArrow | KeyCode::Char('k' | 'K') if shift => Some(PanelMsg::PrevSection),
        KeyCode::DownArrow | KeyCode::Char('j') => Some(PanelMsg::CursorDown),
        KeyCode::UpArrow | KeyCode::Char('k') => Some(PanelMsg::CursorUp),
        KeyCode::Enter => Some(PanelMsg::Select),
        KeyCode::Escape => Some(PanelMsg::LeaveRows),
        KeyCode::Char(' ') if ui.open == Section::Changes => Some(PanelMsg::StageToggle),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn section_order_jump_and_cycle() {
        assert_eq!(SECTION_ORDER.len(), 12);
        let ui = PanelUi::default(); // open = Changes, built-in order
        assert_eq!(ui.next_section(), Section::Commits);
        assert_eq!(ui.prev_section(), Section::Keys); // wraps
        let keys = PanelUi {
            open: Section::Keys,
            ..Default::default()
        };
        assert_eq!(keys.next_section(), Section::Changes); // wraps
        for s in SECTION_ORDER {
            assert_eq!(Section::from_key(s.as_key()), Some(s));
            assert!(!s.label().is_empty());
        }
        assert_eq!(Section::from_key("nope"), None);
    }

    #[test]
    fn resolve_order_reorders_hides_and_survives_junk() {
        let mut cfg = superzej_core::config::Config::default();
        // Empty (the default) → the built-in order, all sections visible.
        assert_eq!(resolve_order(&cfg), SECTION_ORDER.to_vec());
        // A custom list reorders; omitted sections are hidden; unknown keys
        // and duplicates are ignored.
        cfg.panel.sections = vec![
            "git".into(),
            "telemetry".into(),
            "nope".into(),
            "changes".into(),
            "git".into(),
        ];
        assert_eq!(
            resolve_order(&cfg),
            vec![Section::Git, Section::Telemetry, Section::Changes]
        );
        // All-junk lists fall back to the built-in order (never empty).
        cfg.panel.sections = vec!["bogus".into()];
        assert_eq!(resolve_order(&cfg), SECTION_ORDER.to_vec());
    }

    #[test]
    fn set_order_snaps_a_hidden_open_section_to_first_visible() {
        let mut ui = PanelUi {
            open: Section::Tests,
            ..Default::default()
        };
        ui.set_order(vec![Section::Git, Section::Changes]);
        assert_eq!(ui.open, Section::Git);
        // Cycling walks the trimmed order, wrapping.
        assert_eq!(ui.next_section(), Section::Changes);
        assert_eq!(ui.prev_section(), Section::Changes);
        // A still-visible open section is kept.
        ui.open = Section::Changes;
        ui.set_order(vec![Section::Changes, Section::Db]);
        assert_eq!(ui.open, Section::Changes);
    }

    fn fstat(staged: char, unstaged: char, path: &str) -> superzej_svc::git::FileStatus {
        superzej_svc::git::FileStatus {
            path: path.into(),
            staged,
            unstaged,
        }
    }

    #[test]
    fn change_rows_group_and_join_diffstat() {
        let status = vec![
            fstat('?', '?', "notes/scratch.md"),
            fstat('U', 'U', "web/wp-config.php"),
            fstat(' ', 'M', "host/src/panel.rs"),
            fstat('M', ' ', "host/src/tabs.rs"),
            fstat('A', ' ', "core/src/sandbox.rs"),
            fstat('M', 'M', "Cargo.toml"),
        ];
        let diff = vec![
            superzej_svc::git::DiffEntry {
                path: "host/src/tabs.rs".into(),
                added: 84,
                deleted: 21,
            },
            superzej_svc::git::DiffEntry {
                path: "Cargo.toml".into(),
                added: 3,
                deleted: 1,
            },
        ];
        let rows = build_change_rows(&status, &diff);
        let order: Vec<(&str, Stage)> = rows.iter().map(|r| (r.path.as_str(), r.stage)).collect();
        assert_eq!(
            order,
            vec![
                ("core/src/sandbox.rs", Stage::Staged),
                ("host/src/tabs.rs", Stage::Staged),
                ("Cargo.toml", Stage::Unstaged), // MM = partially staged
                ("host/src/panel.rs", Stage::Unstaged),
                ("web/wp-config.php", Stage::Conflict),
                ("notes/scratch.md", Stage::Untracked),
            ]
        );
        let tabs = rows.iter().find(|r| r.name == "tabs.rs").unwrap();
        assert_eq!((tabs.added, tabs.deleted), (84, 21));
        assert_eq!(tabs.dir, "host/src/");
        assert_eq!(tabs.status, "M");
        let conflict = rows.iter().find(|r| r.stage == Stage::Conflict).unwrap();
        assert_eq!(conflict.status, "!U");
        let untracked = rows.iter().find(|r| r.stage == Stage::Untracked).unwrap();
        assert_eq!(untracked.status, "?");
        assert_eq!((untracked.added, untracked.deleted), (0, 0));
        // Root files carry no dir prefix.
        let root = rows.iter().find(|r| r.name == "Cargo.toml").unwrap();
        assert_eq!(root.dir, "");
        assert!(build_change_rows(&[], &[]).is_empty());
    }

    #[test]
    fn tests_lite_trims_failures_and_keeps_history() {
        use crate::testkit::model::*;
        let node = |id: &str, state: TestState, loc: Option<(&str, usize)>| TestNode {
            id: id.into(),
            label: id.into(),
            depth: 0,
            kind: TestNodeKind::Test,
            state,
            location: loc.map(|(p, l)| TestLocation {
                path: p.into(),
                line: l,
                column: None,
            }),
            message: Some("expected EPERM, got Ok(())".into()),
            placeholder: false,
        };
        let cache = TestCache {
            nodes: vec![
                node("a::ok", TestState::Pass, None),
                node("b::fails", TestState::Fail, Some(("tabs.rs", 412))),
                node("c::fails_no_loc", TestState::Fail, None),
            ],
            summary: TestSummary {
                passed: 124,
                failed: 2,
                skipped: 4,
                ..Default::default()
            },
            history: vec![TestRunRec {
                at: 1,
                passed: 124,
                failed: 2,
                skipped: 4,
                duration_ms: 11_300,
                branch: "main".into(),
            }],
            ..Default::default()
        };
        let lite = tests_lite(&cache);
        assert_eq!((lite.passed, lite.failed, lite.skipped), (124, 2, 4));
        assert_eq!(lite.failures.len(), 2);
        assert_eq!(lite.failures[0], ("b::fails".into(), "tabs.rs:412".into()));
        // No location falls back to the first message line.
        assert_eq!(lite.failures[1].1, "expected EPERM, got Ok(())");
        assert_eq!(lite.history.len(), 1);
    }

    #[test]
    fn accordion_keys_unified_navigation() {
        let ui = PanelUi::default(); // open = Changes
        let none = Modifiers::NONE;
        let shift = Modifiers::SHIFT;
        // Plain Down/j (Up/k) step item-by-item through the open section.
        assert_eq!(
            accordion_key(&KeyCode::Char('j'), none, &ui),
            Some(PanelMsg::CursorDown)
        );
        assert_eq!(
            accordion_key(&KeyCode::DownArrow, none, &ui),
            Some(PanelMsg::CursorDown)
        );
        assert_eq!(
            accordion_key(&KeyCode::UpArrow, none, &ui),
            Some(PanelMsg::CursorUp)
        );
        // Shift hops between sections (arrows and J/K both work — termwiz
        // uppercases shifted letters but keeps the modifier).
        assert_eq!(
            accordion_key(&KeyCode::DownArrow, shift, &ui),
            Some(PanelMsg::NextSection)
        );
        assert_eq!(
            accordion_key(&KeyCode::Char('J'), shift, &ui),
            Some(PanelMsg::NextSection)
        );
        assert_eq!(
            accordion_key(&KeyCode::Char('K'), shift, &ui),
            Some(PanelMsg::PrevSection)
        );
        // Enter activates the highlighted row; Esc leaves.
        assert_eq!(
            accordion_key(&KeyCode::Enter, none, &ui),
            Some(PanelMsg::Select)
        );
        assert_eq!(
            accordion_key(&KeyCode::Escape, none, &ui),
            Some(PanelMsg::LeaveRows)
        );
        // Digits jump (indexing the LIVE order) and `e` cycles the view.
        assert_eq!(
            accordion_key(&KeyCode::Char('3'), none, &ui),
            Some(PanelMsg::Open(Section::Branches))
        );
        assert_eq!(
            accordion_key(&KeyCode::Char('8'), none, &ui),
            Some(PanelMsg::Open(Section::Debug))
        );
        assert_eq!(
            accordion_key(&KeyCode::Char('9'), none, &ui),
            Some(PanelMsg::Open(Section::Sandbox))
        );
        // A digit past the visible count is not an accordion intent.
        let trimmed = PanelUi {
            order: vec![Section::Changes, Section::Git],
            ..Default::default()
        };
        assert_eq!(
            accordion_key(&KeyCode::Char('2'), none, &trimmed),
            Some(PanelMsg::Open(Section::Git))
        );
        assert_eq!(accordion_key(&KeyCode::Char('3'), none, &trimmed), None);
        assert_eq!(
            accordion_key(&KeyCode::Char('e'), none, &ui),
            Some(PanelMsg::ToggleExpand)
        );
        // The retired overlay keys fall through (forwarded to panes).
        assert_eq!(accordion_key(&KeyCode::Char('t'), none, &ui), None);
        assert_eq!(accordion_key(&KeyCode::Char('?'), none, &ui), None);
        // Space stages only in the changes section.
        assert_eq!(
            accordion_key(&KeyCode::Char(' '), none, &ui),
            Some(PanelMsg::StageToggle)
        );
        let git = PanelUi {
            open: Section::Git,
            ..Default::default()
        };
        assert_eq!(accordion_key(&KeyCode::Char(' '), none, &git), None);
    }

    #[test]
    fn file_tree_synthesizes_dirs_in_display_order() {
        let paths = vec![
            "src/main.rs".to_string(),
            "README.md".to_string(),
            "src/cmd/pr.rs".to_string(),
            "src/cmd/diff.rs".to_string(),
        ];
        let tree = build_file_tree(&paths);
        let rows: Vec<(String, u8, bool)> = tree
            .iter()
            .map(|e| (e.path.clone(), e.depth, e.is_dir))
            .collect();
        assert_eq!(
            rows,
            vec![
                ("README.md".into(), 0, false),
                ("src".into(), 0, true),
                ("src/cmd".into(), 1, true),
                ("src/cmd/diff.rs".into(), 2, false),
                ("src/cmd/pr.rs".into(), 2, false),
                ("src/main.rs".into(), 1, false),
            ]
        );
    }

    #[test]
    fn file_tree_visible_respects_collapsed_dirs() {
        let paths = vec![
            "src/main.rs".to_string(),
            "README.md".to_string(),
            "src/cmd/pr.rs".to_string(),
            "src/cmd/diff.rs".to_string(),
        ];
        let tree = build_file_tree(&paths);

        // No collapsed dirs: all 6 rows visible.
        let collapsed = std::collections::HashSet::new();
        let vis = file_tree_visible(&tree, &collapsed);
        assert_eq!(vis.len(), 6);

        // Collapse "src": only README.md and "src" row visible (3 hidden).
        let mut collapsed = std::collections::HashSet::new();
        collapsed.insert("src".to_string());
        let vis = file_tree_visible(&tree, &collapsed);
        assert_eq!(vis.len(), 2);
        assert_eq!(vis[0].1.path, "README.md");
        assert_eq!(vis[1].1.path, "src");

        // Collapse only "src/cmd": src + src/main.rs + README.md visible (3),
        // src/cmd is visible but its children are not.
        let mut collapsed = std::collections::HashSet::new();
        collapsed.insert("src/cmd".to_string());
        let vis = file_tree_visible(&tree, &collapsed);
        assert_eq!(vis.len(), 4); // README.md, src, src/cmd, src/main.rs
        let paths_vis: Vec<&str> = vis.iter().map(|(_, e)| e.path.as_str()).collect();
        assert!(paths_vis.contains(&"src/cmd"));
        assert!(!paths_vis.contains(&"src/cmd/pr.rs"));
        assert!(!paths_vis.contains(&"src/cmd/diff.rs"));
    }

    #[test]
    fn reset_on_leave_clears_drill_state_but_keeps_cursors() {
        let mut ui = PanelUi {
            open: Section::Git,
            width: crate::layout::PanelWidth::Half,
            row_mode: true,
            cursor: 3,
            chg_sel: Some(1),
            ..Default::default()
        };
        ui.reset_on_leave();
        // Back to section mode with the change preview dismissed…
        assert!(!ui.row_mode);
        assert_eq!(ui.chg_sel, None);
        // …while the open section, width, and row cursor survive so
        // returning lands where the user left off.
        assert_eq!(ui.open, Section::Git);
        assert_eq!(ui.width, crate::layout::PanelWidth::Half);
        assert_eq!(ui.cursor, 3);
    }
}
