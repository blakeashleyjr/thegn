//! The right panel: a tabbed accordion sidebar over the focused worktree.
//! Three top-level tabs group the sections by concern:
//! - **Git** — Changes, Commits, Branches, Stash, Files
//! - **Work** — PR, Issues, Jobs, Tests
//! - **System** — Notifications, Logs, Sandbox, Telemetry, Keys
//!
//! Each tab shows its own accordion; `[panel] sections` reorders/hides sections
//! within the full list, and tabs filter that list to their assigned sections.
//!
//! Split into two halves:
//! - [`PanelData`] — the git/GitHub payload, rebuilt by the host's background
//!   model hydration and carried on the `FrameModel`. Cheap to clone, `Send`.
//! - [`PanelUi`] — the interactive state (active tab, open section, panel
//!   width, row cursor, the banked hunk previews). Owned by the event loop so
//!   it survives data refreshes.
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
pub mod rollback;
pub mod scope;
pub mod sections;
pub mod staging;

use termwiz::input::{KeyCode, Modifiers};

/// The three top-level panel tabs that group sections by concern.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum PanelTab {
    /// Git operations: Changes, Commits, Branches, Stash, Files.
    #[default]
    Git,
    /// Work-item context: PR, Issues, Jobs, Tests.
    Work,
    /// System & monitoring: Notifications, Logs, Sandbox, Telemetry, Keys.
    #[allow(dead_code)]
    System,
}

impl PanelTab {
    pub fn label(self) -> &'static str {
        match self {
            PanelTab::Git => "git",
            PanelTab::Work => "work",
            PanelTab::System => "system",
        }
    }

    pub fn as_key(self) -> &'static str {
        self.label()
    }

    pub fn from_key(key: &str) -> Option<PanelTab> {
        match key {
            "git" => Some(PanelTab::Git),
            "work" => Some(PanelTab::Work),
            "system" => Some(PanelTab::System),
            _ => None,
        }
    }

    pub fn next(self) -> PanelTab {
        match self {
            PanelTab::Git => PanelTab::Work,
            PanelTab::Work => PanelTab::System,
            PanelTab::System => PanelTab::Git,
        }
    }

    pub fn prev(self) -> PanelTab {
        match self {
            PanelTab::Git => PanelTab::System,
            PanelTab::Work => PanelTab::Git,
            PanelTab::System => PanelTab::Work,
        }
    }
}

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
    /// A click on the tab bar: resolved by x-position via `panel_tab_hit`.
    #[allow(dead_code)]
    Tab(PanelTab),
}

/// One of the accordion sections, in built-in display order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Section {
    #[default]
    Changes,
    Commits,
    Branches,
    Stash,
    /// Unified cross-repo, cross-tool "My Work" feed: assigned issues (all
    /// providers), review-requested / authored PRs, high-priority notifications.
    Mine,
    /// Cross-worktree attention stream (multibuffer-style): failing CI (and,
    /// later, dirty files / content matches) across *all* worktrees, grouped by
    /// worktree with per-row source labels. Read-only.
    Across,
    /// Pull-request state, CI checks, review threads. (Renamed from "git".)
    Pr,
    /// CI/CD runs across providers (AV group): run history + per-run state.
    Ci,
    /// Local merge queue (the fold-actor): per-branch land/defer status.
    MergeQueue,
    Issues,
    Files,
    /// Compiler / linter / test diagnostics.
    Problems,
    /// Configured shell jobs (build, test, run). (Renamed from "tasks".)
    Jobs,
    Tests,
    /// LSP / tree-sitter document-symbol outline for the selected file.
    Symbols,
    Notifications,
    Logs,
    Sandbox,
    /// Configured `[host.*]` machines (hosts-as-resources): per-host state,
    /// inventory, and provision/re-probe/consent/rm-cache actions.
    Hosts,
    /// Configured `[env.<name>]` environments: placement kind, region/size, and
    /// (for provider envs) whether a token resolves. Authored via the palette
    /// "New environment…" wizard / `superzej env create`.
    Environments,
    /// Active ingress shares (`[share]`): the ports this worktree exposes and
    /// their public URLs.
    Share,
    /// Active auto port forwards (`[forward]`): sandbox-internal dev-server ports
    /// forwarded to the host's loopback for browser preview.
    Forward,
    Telemetry,
    /// Now-playing + transport for the optional `[media]` feature. Hidden unless
    /// `[media] enabled`.
    Media,
    Keys,
    // Placeholder sections — kept as dead variants for future use.
    #[allow(dead_code)]
    Debug,
    #[allow(dead_code)]
    Db,
}

/// The accordion's built-in display order — the default when `[panel]
/// sections` is unset. Grouped by tab:
/// - Git (5): Changes, Commits, Branches, Stash, Files
/// - Work (10): Mine, Across, Pr, Ci, MergeQueue, Issues, Problems, Jobs, Tests, Symbols
/// - System (10): Notifications, Logs, Sandbox, Hosts, Environments, Share, Forward, Telemetry, Media, Keys
///
/// The live order (config-reordered, possibly trimmed) rides on
/// [`PanelUi::order`]; numbered jump keys index the ACTIVE TAB's slice.
pub const SECTION_ORDER: [Section; 25] = [
    // Git tab
    Section::Changes,
    Section::Commits,
    Section::Branches,
    Section::Stash,
    Section::Files,
    // Work tab
    Section::Mine,
    Section::Across,
    Section::Pr,
    Section::Ci,
    Section::MergeQueue,
    Section::Issues,
    Section::Problems,
    Section::Jobs,
    Section::Tests,
    Section::Symbols,
    // System tab
    Section::Notifications,
    Section::Logs,
    Section::Sandbox,
    Section::Hosts,
    Section::Environments,
    Section::Share,
    Section::Forward,
    Section::Telemetry,
    Section::Media,
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
            Section::Mine => "mine",
            Section::Across => "across",
            Section::Pr => "pr",
            Section::Ci => "ci",
            Section::MergeQueue => "merge",
            Section::Files => "files",
            Section::Problems => "problems",
            Section::Jobs => "jobs",
            Section::Tests => "tests",
            Section::Symbols => "symbols",
            Section::Debug => "debug",
            Section::Sandbox => "sandbox",
            Section::Hosts => "hosts",
            Section::Environments => "environments",
            Section::Db => "db",
            Section::Telemetry => "telemetry",
            Section::Keys => "keys",
            Section::Issues => "issues",
            Section::Notifications => "notifications",
            Section::Logs => "logs",
            Section::Media => "media",
            Section::Share => "share",
            Section::Forward => "forward",
        }
    }

    /// Which top-level tab this section belongs to.
    pub fn tab(self) -> PanelTab {
        match self {
            Section::Changes
            | Section::Commits
            | Section::Branches
            | Section::Stash
            | Section::Files => PanelTab::Git,
            Section::Mine
            | Section::Across
            | Section::Pr
            | Section::Ci
            | Section::MergeQueue
            | Section::Issues
            | Section::Problems
            | Section::Jobs
            | Section::Tests
            | Section::Symbols => PanelTab::Work,
            Section::Notifications
            | Section::Logs
            | Section::Sandbox
            | Section::Hosts
            | Section::Environments
            | Section::Share
            | Section::Forward
            | Section::Telemetry
            | Section::Media
            | Section::Keys
            | Section::Debug
            | Section::Db => PanelTab::System,
        }
    }

    /// The lazygit-family sections that share [`gitui::GitUi`] state and the
    /// Full-width git frame.
    pub fn is_git_family(self) -> bool {
        matches!(
            self,
            Section::Changes | Section::Commits | Section::Branches | Section::Stash | Section::Pr
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
        // Back-compat aliases for renamed sections (old configs use these keys).
        let key = match key {
            "prs" | "git" => "pr",
            "tasks" => "jobs",
            k => k,
        };
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

/// Lightweight record of a completed (or running) generic task run.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TaskRunRecord {
    /// Matches `Task::name`.
    pub name: String,
    pub exit_code: i32,
    pub duration_ms: u64,
    /// Unix milliseconds when the run finished (0 when running).
    pub finished_at: i64,
    /// Last few lines of captured stdout+stderr (display only).
    pub output_tail: String,
    /// True while the task process is still running.
    pub running: bool,
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

// Blame parsing moved to `superzej_core::blame` so the semantic layer can group
// it by entity under the core coverage gate; re-exported here for the panel.
pub use superzej_core::blame::{BlameRow, parse_blame_porcelain};

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
    /// Recent CI runs (newest first) for the current branch, from `ci_runs_cache`
    /// — feeds the `Ci` section rollup (AV group). Empty when CI is off/undetected.
    pub ci_runs: Vec<superzej_core::ci::CiRun>,
    /// Cross-worktree attention stream (the `Across` section): failing CI and
    /// (later) dirty files / content matches from *all* worktrees, grouped by
    /// worktree. Built off-loop during hydration; empty when nothing needs
    /// attention. See [`superzej_core::aggregate`].
    pub across: superzej_core::aggregate::Aggregation,
    /// The local merge queue (the fold-actor), from `merge_queue` — feeds the
    /// `MergeQueue` section + statusbar badge. Empty when the queue is unused.
    pub merge_queue: Vec<superzej_core::db::MergeQueueRow>,
    /// Now-playing snapshot for the optional `[media]` feature (`Media` section +
    /// statusbar badge). `None` when media is disabled or no player is loaded.
    pub media: Option<superzej_core::media::MediaState>,
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
    /// highlights drawn from `changes`.
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
    /// Entity-level view of pending changes (semantic git layer, items 311/313/
    /// 317): per-file entity churn + impact. Built off-thread from `git diff
    /// HEAD`; feeds the changes-section impact line and the commit-message
    /// prefill. `None` until first computed / when there are no entity changes.
    pub entities: Option<superzej_core::semantic::EntitySummary>,
    /// Issues from the configured tracker (Linear/GitHub/Jira), loaded from
    /// the `issue_cache` DB table. Empty when no provider is configured.
    pub tracker_issues: Vec<superzej_core::issue::Issue>,
    /// Issue ids (in `"<provider>:<key>"` form) linked to the current worktree.
    pub tracker_links: Vec<String>,
    /// Whether any `[issues]` provider is configured for this repo. Distinguishes
    /// "off" (no provider) from "clear" (configured but currently empty).
    pub issues_configured: bool,
    /// Unified cross-repo "My Work" feed (the `Mine` section), loaded from the
    /// `my_work_cache` DB row. Spans every repo, not just the active worktree.
    pub my_work: Vec<superzej_core::work::WorkRow>,
    /// Neutral unread notification count (Alert + Notice priority; Info excluded).
    /// Drives the dim "N unread" badge.
    pub unread_notifications: usize,
    /// Alert-priority unread count — drives the red ⚑ attention flag.
    pub alert_notifications: usize,
    /// Full notification list (newest first, capped at 50) for the inbox section.
    pub notifications: Vec<superzej_core::notification::Notification>,
    /// Last 500 lines of szhost.log, parsed. Empty when SUPERZEJ_LOG is unset.
    pub log_lines: Vec<superzej_core::log_view::LogLine>,
    /// A bounded tail (~400 lines) of parsed szhost.log, kept on *every* refresh
    /// (unlike `log_lines`, which is section-gated) so the notification
    /// drilldown log modal always has data to show without new blocking I/O.
    pub log_tail: Vec<superzej_core::log_view::LogLine>,
    /// Structured logs for the sz-log feature.
    pub log_lines_structured: Vec<superzej_core::log::parser::ParsedLog>,
    /// Configured + auto-discovered task specs for the Tasks section.
    pub task_specs: Vec<superzej_core::config::Task>,
    /// Last-run record per task name (keyed by `Task::name`).
    pub task_last_runs: std::collections::HashMap<String, TaskRunRecord>,
    /// Compiler/linter/test diagnostics collected from task output (Problems section).
    pub diagnostics: Vec<DiagnosticItem>,
    /// Document-symbol outline for the selected file (Symbols section). The
    /// `file` is the repo-relative path the outline was computed for ("" = none).
    pub symbols_file: String,
    pub symbols: Vec<SymbolRow>,
    /// Per-`[host.*]` display snapshots (hosts-as-resources): built off-loop
    /// from the config + DB by hydration, live-merged on the loop from
    /// `HostRuntime` after each drain. Feeds the `Hosts` section, the sidebar
    /// HOSTS block, and the wizard readiness badges. Empty without `[host.*]`.
    pub hosts: Vec<crate::host_ui::HostSnapshot>,
    /// Per-`[env.<name>]` display snapshots for the `Environments` section: built
    /// off-loop from the config by hydration (placement kind, region/size, token
    /// presence). Empty without any `[env.*]`.
    pub environments: Vec<crate::env_ui::EnvSnapshot>,
}

/// One row of the Symbols outline (or, in references mode, a reference site): a
/// named entity at a 1-based line, with a short kind label and nesting depth.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SymbolRow {
    /// Short kind label ("fn", "struct", …; "→" for a reference site).
    pub kind: String,
    pub name: String,
    /// Repo-relative file path (the navigation target).
    pub file: String,
    pub line: u64,
    /// 0-based column of the symbol name (the position used for find-references).
    pub col: u32,
    /// Nesting depth for indentation (top-level = 0).
    pub depth: u16,
}

/// Severity level for a compiler/linter diagnostic.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Default)]
pub enum Severity {
    #[default]
    Error = 0,
    Warning = 1,
    Info = 2,
    Hint = 3,
}

/// One structured diagnostic item for the Problems panel.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiagnosticItem {
    /// Repo-relative file path.
    pub file: String,
    pub line: u64,
    pub col: Option<u64>,
    pub severity: Severity,
    pub message: String,
    /// Which task produced this (e.g. "cargo clippy", "pytest").
    pub source: String,
    pub code: Option<String>,
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
            let ancestor_count = parts.len().saturating_sub(1);
            for d in 1..=ancestor_count {
                if collapsed.contains(&parts[..d].join("/")) {
                    return false;
                }
            }
            true
        })
        .collect()
}

/// An inline file view shown in the Files section (full pane): the contents of
/// a selected file, scrolled with j/k, dismissed with esc. The read happens
/// off-thread; until it lands `loading` is true. The scroll math is pure and
/// unit-tested.
#[derive(Debug, Clone, Default)]
pub struct FilePreview {
    /// Repo-relative path being shown (the header label).
    pub path: String,
    /// File contents, one entry per line (tabs already expanded).
    pub lines: Vec<String>,
    /// True while the background read is still in flight.
    pub loading: bool,
    /// Set when the read failed (binary, too large, unreadable) — shown in
    /// place of content.
    pub error: Option<String>,
    /// Index of the topmost visible content line.
    pub scroll: usize,
}

impl FilePreview {
    /// A freshly-opened preview waiting on its background read.
    pub fn loading(path: impl Into<String>) -> Self {
        FilePreview {
            path: path.into(),
            loading: true,
            ..FilePreview::default()
        }
    }

    /// The largest valid scroll offset for a `viewport`-row content area: the
    /// last line still anchored so at least one row of content shows.
    pub fn max_scroll(&self, viewport: usize) -> usize {
        self.lines.len().saturating_sub(viewport.max(1))
    }

    /// Scroll by `delta` rows (negative = up), clamped to the content bounds.
    pub fn scroll_by(&mut self, delta: isize, viewport: usize) {
        let max = self.max_scroll(viewport) as isize;
        self.scroll = (self.scroll as isize + delta).clamp(0, max) as usize;
    }
}

/// The panel's interactive state, owned by the event loop.
#[derive(Debug, Clone)]
pub struct PanelUi {
    /// The full test-explorer state (detected task, per-test status map,
    /// display tree, cursor/scroll/filter) backing the tests section's runs.
    pub tests: TestPanelState,
    /// The live accordion order (config-resolved, possibly trimmed) across ALL
    /// tabs. Sections are filtered to the active tab before display.
    /// The numbered jump keys index the ACTIVE TAB's slice. Never empty.
    pub order: Vec<Section>,
    /// The active top-level tab (Git / Work / System).
    pub tab: PanelTab,
    /// Which accordion section is open (exactly one; must belong to `tab`).
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
    /// True while the changes section's semantic-impact footer is expanded into
    /// its per-file / per-entity breakdown. Kept separate from `chg_sel` so that
    /// stays a pure, always-in-range change index.
    pub impact_open: bool,
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
    /// When set, the Files section shows this file's contents inline (full
    /// pane) instead of the tree — `enter` on a file opens it, `esc` closes it.
    /// Populated off-thread by the file-read worker.
    pub file_preview: Option<FilePreview>,
    /// Row cursor within the Problems section's list.
    pub problems_cursor: usize,
    /// Row cursor within the Symbols outline list.
    pub symbols_cursor: usize,
    /// When true, the Symbols section shows find-references results for the last
    /// selected symbol instead of the file outline (`r` toggles on, `o`/esc off).
    pub symbols_show_refs: bool,
    /// Row cursor within the Tasks section's list.
    pub tasks_cursor: usize,
    /// Row cursor within the Issues section's list (persisted across section switches).
    pub issues_cursor: usize,
    /// Active free-text filter query for the Issues section (`/` mode).
    pub issues_filter: String,
    /// Optional project/sprint ID filter for the Issues section (`f` key cycles).
    pub issues_project_filter: Option<String>,
    /// Row cursor within the Notifications section's list.
    pub notifications_cursor: usize,
    /// When true, the Notifications section shows already-read items too (`A` toggle).
    pub notifications_show_read: bool,
    /// Active free-text filter query for the Notifications section (`/` mode).
    pub notifications_filter: String,
    /// True while the Notifications section's filter field is being edited.
    pub notifications_filter_editing: bool,
    /// Row cursor within the Logs section's list.
    pub logs_cursor: usize,
    /// Active free-text filter query for the Logs section (`/` mode).
    pub logs_filter: String,
    /// True while the Logs section's filter field is being edited.
    pub logs_filter_editing: bool,
    /// Level gate: `None` = all levels; `Some(L)` shows lines at L and above.
    pub logs_level: Option<superzej_core::log_view::LogLevel>,
    /// When true, the cursor auto-jumps to the newest line on each hydration.
    pub logs_tail: bool,
}

impl Default for PanelUi {
    fn default() -> Self {
        PanelUi {
            tests: TestPanelState::default(),
            order: SECTION_ORDER.to_vec(),
            tab: PanelTab::default(),
            open: Section::default(),
            width: crate::layout::PanelWidth::default(),
            row_mode: false,
            cursor: 0,
            chg_sel: None,
            impact_open: false,
            hunks: std::collections::HashMap::new(),
            hunks_gen: 0,
            scroll: 0,
            diff_hunk: 0,
            docs: docs::PanelDocs::default(),
            git: gitui::GitUi::default(),
            files_collapsed: std::collections::HashSet::new(),
            file_preview: None,
            problems_cursor: 0,
            symbols_cursor: 0,
            symbols_show_refs: false,
            tasks_cursor: 0,
            issues_cursor: 0,
            issues_filter: String::new(),
            issues_project_filter: None,
            notifications_cursor: 0,
            notifications_show_read: false,
            notifications_filter: String::new(),
            notifications_filter_editing: false,
            logs_cursor: 0,
            logs_filter: String::new(),
            logs_filter_editing: false,
            logs_level: None,
            logs_tail: true,
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
        self.impact_open = false;
    }

    /// Adopt a config-resolved order; the open section snaps to the first
    /// section in the current tab when the config hid it.
    pub fn set_order(&mut self, order: Vec<Section>) {
        debug_assert!(!order.is_empty());
        self.order = order;
        self.ensure_open_valid();
    }

    /// Switch to a different top-level tab; snaps `open` to the first section
    /// of that tab that appears in the live order.
    pub fn switch_tab(&mut self, tab: PanelTab) {
        self.tab = tab;
        self.row_mode = false;
        self.cursor = 0;
        self.ensure_open_valid();
    }

    /// Open a section, keeping the visible tab in step with it. Row navigation
    /// flows across the *whole* section order, so a Down at the bottom of a
    /// tab's last section lands on the next tab's first section — without this
    /// the tab strip and accordion (both keyed off `self.tab`) would keep
    /// rendering the old tab, and `ensure_open_valid` would later snap `open`
    /// back. The invariant is `self.tab == self.open.tab()`.
    pub fn open_section(&mut self, s: Section) {
        self.open = s;
        self.tab = s.tab();
    }

    /// Ensure `open` is a section that belongs to the current tab and exists
    /// in the live order. If not, snap to the first valid section.
    fn ensure_open_valid(&mut self) {
        if self.open.tab() == self.tab && self.order.contains(&self.open) {
            return;
        }
        // Try to find a section for the current tab.
        if let Some(&s) = self.order.iter().find(|s| s.tab() == self.tab) {
            self.open = s;
        } else if let Some(&s) = self.order.first() {
            self.open = s;
        }
    }

    /// Sections in the live order that belong to the active tab.
    pub fn tab_sections(&self) -> Vec<Section> {
        self.order
            .iter()
            .copied()
            .filter(|s| s.tab() == self.tab)
            .collect()
    }

    /// Count of sections in the active tab (used for budget math).
    pub fn visible_section_count(&self) -> usize {
        self.order.iter().filter(|s| s.tab() == self.tab).count()
    }

    /// The position of the open section within the active tab's slice (0 when stale).
    fn open_index(&self) -> usize {
        self.tab_sections()
            .iter()
            .position(|s| *s == self.open)
            .unwrap_or(0)
    }

    /// The section after the open one, wrapping within the active tab.
    pub fn next_section(&self) -> Section {
        let secs = self.tab_sections();
        if secs.is_empty() {
            return self.open;
        }
        secs[(self.open_index() + 1) % secs.len()]
    }

    /// The section before the open one, wrapping within the active tab.
    pub fn prev_section(&self) -> Section {
        let secs = self.tab_sections();
        if secs.is_empty() {
            return self.open;
        }
        secs[(self.open_index() + secs.len() - 1) % secs.len()]
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
    /// Enter on a row (changes: toggle the hunk preview; pr: jump to a
    /// thread; files: open/fold; tests: open the failing test).
    Select,
    /// `e`: cycle the view width (Normal → Half → Full).
    ToggleExpand,
    /// Space in the changes section: stage/unstage the selected file.
    StageToggle,
    /// `[`/`]` or Alt+1/2/3: switch the active top-level tab.
    #[allow(dead_code)]
    SwitchTab(PanelTab),
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
///
/// `Alt+1/2/3` switch the active tab directly (Git/Work/System). These are
/// intercepted by the event loop before reaching this function (the loop
/// handles them with panel priority so they shadow the global pin-summon
/// shortcuts). Within a tab, `1..=N` jump to that tab's N-th section.
pub fn accordion_key(key: &KeyCode, mods: Modifiers, ui: &PanelUi) -> Option<PanelMsg> {
    let shift = mods.contains(Modifiers::SHIFT);
    let alt = mods.contains(Modifiers::ALT);

    // Always-available jumps (the view cycle, numbered section jumps) read
    // the same wherever the panel is. Digits index the ACTIVE TAB's sections.
    match key {
        KeyCode::Char('e') => return Some(PanelMsg::ToggleExpand),
        KeyCode::Char(c @ '1'..='9') if !alt => {
            let idx = (*c as usize) - ('1' as usize);
            let tab_secs = ui.tab_sections();
            if let Some(&s) = tab_secs.get(idx) {
                return Some(PanelMsg::Open(s));
            }
        }
        _ => {}
    }
    match key {
        // Shift always hops between section headers regardless of row position.
        KeyCode::DownArrow | KeyCode::Char('j' | 'J') if shift => Some(PanelMsg::NextSection),
        KeyCode::UpArrow | KeyCode::Char('k' | 'K') if shift => Some(PanelMsg::PrevSection),
        // j/k/arrows always navigate items within the open section first.
        // At the boundary (top or bottom of the list) they flow into the
        // adjacent section — the event loop handles boundary detection.
        KeyCode::DownArrow | KeyCode::Char('j') => Some(PanelMsg::CursorDown),
        KeyCode::UpArrow | KeyCode::Char('k') => Some(PanelMsg::CursorUp),
        // Enter activates the highlighted row.
        KeyCode::Enter => Some(PanelMsg::Select),
        // Esc peels back one level: expanded change preview → leave panel zone.
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
        assert_eq!(SECTION_ORDER.len(), 25);
        // Default tab = Git; Changes is in Git tab.
        let ui = PanelUi::default(); // open = Changes, tab = Git
        assert_eq!(ui.next_section(), Section::Commits); // next in Git tab
        assert_eq!(ui.prev_section(), Section::Files); // wraps within Git tab
        // Files is last in Git tab; next wraps to Changes.
        let files = PanelUi {
            open: Section::Files,
            ..Default::default()
        };
        assert_eq!(files.next_section(), Section::Changes); // wraps within Git tab
        for s in SECTION_ORDER {
            assert_eq!(Section::from_key(s.as_key()), Some(s));
            assert!(!s.label().is_empty());
        }
        // Back-compat aliases still resolve.
        assert_eq!(Section::from_key("prs"), Some(Section::Pr));
        assert_eq!(Section::from_key("git"), Some(Section::Pr));
        assert_eq!(Section::from_key("tasks"), Some(Section::Jobs));
        assert_eq!(Section::from_key("nope"), None);
    }

    #[test]
    fn resolve_order_reorders_hides_and_survives_junk() {
        let mut cfg = superzej_core::config::Config::default();
        // Empty (the default) → the built-in order, all sections visible.
        assert_eq!(resolve_order(&cfg), SECTION_ORDER.to_vec());
        // A custom list reorders; omitted sections are hidden; unknown keys
        // and duplicates are ignored. Old "prs" alias still resolves.
        cfg.panel.sections = vec!["prs".into(), "nope".into(), "changes".into(), "prs".into()];
        assert_eq!(resolve_order(&cfg), vec![Section::Pr, Section::Changes]);
        // New key "pr" also works.
        cfg.panel.sections = vec!["pr".into(), "changes".into()];
        assert_eq!(resolve_order(&cfg), vec![Section::Pr, Section::Changes]);
        // All-junk lists fall back to the built-in order (never empty).
        cfg.panel.sections = vec!["bogus".into()];
        assert_eq!(resolve_order(&cfg), SECTION_ORDER.to_vec());
    }

    #[test]
    fn set_order_snaps_a_hidden_open_section_to_first_visible() {
        // Tests is a Work-tab section; default tab is Git.
        // set_order with only Pr+Changes → open snaps to Pr (first Work section).
        let mut ui = PanelUi {
            tab: PanelTab::Work,
            open: Section::Tests,
            ..Default::default()
        };
        ui.set_order(vec![Section::Pr, Section::Changes]);
        // Tests is not in order → snaps to Pr (first Work section visible).
        assert_eq!(ui.open, Section::Pr);
        // Cycling walks only the Work tab's sections (only Pr here).
        assert_eq!(ui.next_section(), Section::Pr); // wraps to itself
        assert_eq!(ui.prev_section(), Section::Pr);
        // A still-visible open section is kept.
        ui.open = Section::Pr;
        ui.set_order(vec![Section::Pr, Section::Changes]);
        assert_eq!(ui.open, Section::Pr);
    }

    #[test]
    fn open_section_syncs_the_tab_across_boundaries() {
        // Row navigation flows across the whole order, so opening a section can
        // land in another tab. The tab must follow, or the strip + accordion
        // (both keyed off `tab`) keep rendering the old tab and
        // `ensure_open_valid` snaps `open` back.
        let mut ui = PanelUi::default();
        assert_eq!(ui.tab, PanelTab::Git); // default lands on Git

        ui.open_section(Section::Pr); // Work-tab section
        assert_eq!(ui.open, Section::Pr);
        assert_eq!(ui.tab, PanelTab::Work);

        ui.open_section(Section::Logs); // System-tab section
        assert_eq!(ui.tab, PanelTab::System);

        ui.open_section(Section::Changes); // back into Git
        assert_eq!(ui.tab, PanelTab::Git);

        // Invariant: tab always matches the open section's tab.
        assert_eq!(ui.tab, ui.open.tab());
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
        let ui = PanelUi::default(); // open = Changes, tab = Git, row_mode = false
        let none = Modifiers::NONE;
        let shift = Modifiers::SHIFT;
        let alt = Modifiers::ALT;
        // j/k always navigate items (CursorDown/CursorUp); the event loop
        // handles boundary flow into adjacent sections. row_mode no longer
        // gates cursor navigation — the item-first model is always active.
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
        // row_mode=true still works (no regression): same CursorDown/Up.
        let row_ui = PanelUi {
            row_mode: true,
            ..Default::default()
        };
        assert_eq!(
            accordion_key(&KeyCode::Char('j'), none, &row_ui),
            Some(PanelMsg::CursorDown)
        );
        assert_eq!(
            accordion_key(&KeyCode::DownArrow, none, &row_ui),
            Some(PanelMsg::CursorDown)
        );
        assert_eq!(
            accordion_key(&KeyCode::UpArrow, none, &row_ui),
            Some(PanelMsg::CursorUp)
        );
        // Shift always hops between sections regardless of row_mode.
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
        // Digits jump within the ACTIVE TAB's sections (Git tab: Changes, Commits,
        // Branches, Stash, Files — indices 1..=5).
        assert_eq!(
            accordion_key(&KeyCode::Char('1'), none, &ui),
            Some(PanelMsg::Open(Section::Changes))
        );
        assert_eq!(
            accordion_key(&KeyCode::Char('3'), none, &ui),
            Some(PanelMsg::Open(Section::Branches))
        );
        assert_eq!(
            accordion_key(&KeyCode::Char('5'), none, &ui),
            Some(PanelMsg::Open(Section::Files))
        );
        // A digit past the tab's section count is not an accordion intent.
        assert_eq!(accordion_key(&KeyCode::Char('6'), none, &ui), None);
        // In Work tab, digits index Work sections (Mine, Across, Pr, Ci,
        // MergeQueue, Issues, Problems, Jobs, Tests, Symbols).
        let work_ui = PanelUi {
            tab: PanelTab::Work,
            open: Section::Pr,
            ..Default::default()
        };
        assert_eq!(
            accordion_key(&KeyCode::Char('1'), none, &work_ui),
            Some(PanelMsg::Open(Section::Mine))
        );
        assert_eq!(
            accordion_key(&KeyCode::Char('2'), none, &work_ui),
            Some(PanelMsg::Open(Section::Across))
        );
        // Work order: Mine, Across, Pr, Ci, MergeQueue, … → '3' Pr, '4' Ci, '5' MergeQueue.
        assert_eq!(
            accordion_key(&KeyCode::Char('3'), none, &work_ui),
            Some(PanelMsg::Open(Section::Pr))
        );
        assert_eq!(
            accordion_key(&KeyCode::Char('4'), none, &work_ui),
            Some(PanelMsg::Open(Section::Ci))
        );
        assert_eq!(
            accordion_key(&KeyCode::Char('5'), none, &work_ui),
            Some(PanelMsg::Open(Section::MergeQueue))
        );
        // A custom order filters to the tab's sections.
        let trimmed = PanelUi {
            order: vec![Section::Changes, Section::Pr],
            ..Default::default() // tab = Git
        };
        // Git tab has only Changes (Pr is Work tab).
        assert_eq!(
            accordion_key(&KeyCode::Char('1'), none, &trimmed),
            Some(PanelMsg::Open(Section::Changes))
        );
        assert_eq!(accordion_key(&KeyCode::Char('2'), none, &trimmed), None);
        assert_eq!(
            accordion_key(&KeyCode::Char('e'), none, &ui),
            Some(PanelMsg::ToggleExpand)
        );
        // Alt+1/2/3 tab switching is handled by the event loop before
        // accordion_key (they shadow global pin-summon; see run.rs).
        // accordion_key itself does NOT produce SwitchTab for these chords.
        assert_eq!(accordion_key(&KeyCode::Char('1'), alt, &ui), None);
        assert_eq!(accordion_key(&KeyCode::Char('2'), alt, &ui), None);
        assert_eq!(accordion_key(&KeyCode::Char('3'), alt, &ui), None);
        // [ / ] are no longer tab-switching keys; they fall through to
        // per-section handlers (hunk navigation in Changes full view, etc.).
        assert_eq!(accordion_key(&KeyCode::Char('['), none, &ui), None);
        assert_eq!(accordion_key(&KeyCode::Char(']'), none, &ui), None);
        // The retired overlay keys fall through (forwarded to panes).
        assert_eq!(accordion_key(&KeyCode::Char('t'), none, &ui), None);
        assert_eq!(accordion_key(&KeyCode::Char('?'), none, &ui), None);
        // Space stages only in the changes section.
        assert_eq!(
            accordion_key(&KeyCode::Char(' '), none, &ui),
            Some(PanelMsg::StageToggle)
        );
        let pr = PanelUi {
            tab: PanelTab::Work,
            open: Section::Pr,
            ..Default::default()
        };
        assert_eq!(accordion_key(&KeyCode::Char(' '), none, &pr), None);
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
            tab: PanelTab::Work,
            open: Section::Pr,
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
        assert_eq!(ui.open, Section::Pr);
        assert_eq!(ui.width, crate::layout::PanelWidth::Half);
        assert_eq!(ui.cursor, 3);
    }

    #[test]
    fn file_preview_scroll_clamps_to_content_bounds() {
        let mut fp = FilePreview {
            path: "src/main.rs".into(),
            lines: (0..100).map(|i| format!("line {i}")).collect(),
            ..FilePreview::default()
        };
        // viewport of 20 rows over 100 lines: last valid top is 80.
        assert_eq!(fp.max_scroll(20), 80);
        fp.scroll_by(1000, 20);
        assert_eq!(fp.scroll, 80, "cannot scroll past the end");
        fp.scroll_by(-1000, 20);
        assert_eq!(fp.scroll, 0, "cannot scroll above the top");
        fp.scroll_by(25, 20);
        assert_eq!(fp.scroll, 25);
    }

    #[test]
    fn file_preview_shorter_than_viewport_never_scrolls() {
        let mut fp = FilePreview {
            lines: vec!["a".into(), "b".into()],
            ..FilePreview::default()
        };
        assert_eq!(fp.max_scroll(40), 0);
        // Any scroll attempt is pinned to the top when content fits.
        fp.scroll_by(5, 40);
        assert_eq!(fp.scroll, 0);
    }

    #[test]
    fn file_preview_loading_constructor_sets_flag_and_path() {
        let fp = FilePreview::loading("README.md");
        assert!(fp.loading);
        assert_eq!(fp.path, "README.md");
        assert!(fp.lines.is_empty());
        assert!(fp.error.is_none());
    }
}
