//! The right panel: a tabbed Diff / Files / PR / Checks view over the focused
//! worktree.
//!
//! Split into two halves:
//! - [`PanelData`] — the git/GitHub payload, rebuilt by the host's background
//!   model hydration and carried on the `FrameModel`. Cheap to clone, `Send`.
//! - [`PanelUi`] — the interactive state (current tab, file cursor, scroll,
//!   drill-in view). Owned by the event loop so it survives data refreshes.
//!
//! Rendering lives in `chrome.rs` next to the other `draw_*` surfaces; this
//! module owns the data model + the pure key→intent navigation logic.

use termwiz::input::KeyCode;

/// Which of the panel tabs is showing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum PanelTab {
    #[default]
    Diff,
    Files,
    Pr,
    Checks,
    Tests,
}

impl PanelTab {
    /// Cycle to the next tab (Tab key).
    #[allow(dead_code)]
    pub fn next(self) -> Self {
        match self {
            PanelTab::Diff => PanelTab::Files,
            PanelTab::Files => PanelTab::Pr,
            PanelTab::Pr => PanelTab::Checks,
            PanelTab::Checks => PanelTab::Tests,
            PanelTab::Tests => PanelTab::Diff,
        }
    }
}

// The Tests model (TestState/TestTask/TestNode/TestPanelState + parsers, tree,
// and locate helpers) lives in `testkit::model` and is re-exported so the panel
// and chrome keep using `crate::panel::TestNode` etc.
pub use crate::testkit::model::*;

/// The Diff tab has two stacked views: the file list and a single-file diff.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[allow(dead_code)]
pub enum DiffView {
    #[default]
    FileList,
    FileDiff,
}

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

/// One CI check in the Checks tab.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CheckLine {
    pub name: String,
    pub state: CheckState,
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

/// The panel's data payload (git + GitHub), rebuilt on background refresh.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PanelData {
    pub branch: String,
    pub files: Vec<DiffFile>,
    /// `Some` when a PR exists; `None` otherwise (see `pr_note`).
    pub pr: Option<PrSummary>,
    /// A short human note when there's no PR ("no pull request", "gh not
    /// authenticated", an error). Shown in the PR tab body.
    pub pr_note: Option<String>,
    pub checks: Vec<CheckLine>,
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

/// The indices (into `entries`) currently visible: an entry hides when ANY
/// ancestor directory is collapsed.
pub fn visible_file_indices(
    entries: &[FileEntry],
    collapsed: &std::collections::HashSet<String>,
) -> Vec<usize> {
    entries
        .iter()
        .enumerate()
        .filter(|(_, e)| {
            let parts: Vec<&str> = e.path.split('/').collect();
            !(1..parts.len()).any(|d| collapsed.contains(&parts[..d].join("/")))
        })
        .map(|(i, _)| i)
        .collect()
}

/// The panel's interactive state, owned by the event loop.
#[derive(Debug, Clone, Default)]
#[allow(dead_code)]
pub struct PanelUi {
    pub tab: PanelTab,
    pub diff_view: DiffView,
    /// Cursor row in the Diff file list.
    pub diff_cursor: usize,
    /// Half-page scroll offset for the FileDiff body.
    pub diff_scroll: usize,
    /// Raw (already-highlighted) diff text for the drilled-in file.
    pub file_diff: String,
    /// Path of the file currently drilled into (for the help bar / re-fetch).
    pub focused_path: String,
    /// The Files tab's flattened tree (display order, dirs synthesized).
    pub files: Vec<FileEntry>,
    /// Collapsed directory paths (accordion state).
    pub files_collapsed: std::collections::HashSet<String>,
    /// Cursor index into the VISIBLE rows of the Files tree.
    pub files_cursor: usize,
    /// Scroll offset (visible-row index of the first drawn row).
    pub files_scroll: usize,
    /// Which worktree `files` was built for (rebuilt when it changes).
    pub files_worktree: String,
    /// True while a file preview (syntax-highlighted document in `file_diff`)
    /// is open on the Files tab.
    pub files_preview: bool,
    /// Tests tab: detected `(framework, command)`, latest results, and state.
    /// Tests tab: the full test-explorer state (detected task, per-test status
    /// map, display tree, cursor/scroll/filter).
    pub tests: TestPanelState,
}

impl PanelUi {
    /// True while a drilled-in document view is open (single-file diff or
    /// file preview) — the panel widens to a reading width while this holds.
    pub fn drilled(&self) -> bool {
        (self.tab == PanelTab::Diff && self.diff_view == DiffView::FileDiff)
            || (self.tab == PanelTab::Files && self.files_preview)
    }

    /// Full reset when focus leaves the panel: back to the file list, preview
    /// closed, document scroll rewound. Cursors are intentionally kept so
    /// returning lands where you left off.
    pub fn reset_on_leave(&mut self) {
        self.diff_view = DiffView::FileList;
        self.files_preview = false;
        self.diff_scroll = 0;
    }
}

/// A decoded panel navigation intent. `None` means the key isn't owned by the
/// panel and should fall through to the global keymap.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(dead_code)]
pub enum PanelNav {
    SelectTab(PanelTab),
    CycleTab,
    Up,
    Down,
    /// Enter: drill into the selected file (Diff:FileList only).
    Enter,
    /// Esc: back out of FileDiff, or leave the panel from a top-level view.
    Back,
    /// `o`: open file in editor (Diff/Files) or PR in browser (PR tab).
    Open,
    /// `O` (Shift+o): open the selected file with the external/system opener.
    OpenExternal,
    /// `J`/`K` (Shift): jump to the next/previous file's document (diff or
    /// preview), opening the document view if it isn't up yet.
    NextDoc,
    PrevDoc,
    /// `t`: promote the current artifact (diff / preview) into a center tab…
    OpenTab,
    /// …or `s`: into a center pane split.
    OpenPane,
    /// `e`: the editor on the selection, in a center pane split (`o` = tab).
    OpenEditorPane,
    /// `y`: reveal the selected entry's directory in the yazi drawer (Files).
    RevealDrawer,
    Merge,
    Approve,
    Create,
    Rerun,
    /// Tests: run all / run failed / refresh discovery / peek in bat / debug.
    RunAll,
    RunFailed,
    Refresh,
    Peek,
    Debug,
}

/// Map a raw key to a panel nav intent given the current tab/view context.
/// Context matters: `j`/`k` scroll in FileDiff but move the cursor in FileList,
/// and the action keys (`m`/`a`/`c`/`r`/`o`) only bind on their relevant tab.
#[allow(dead_code)]
pub fn panel_nav_key(key: &KeyCode, tab: PanelTab, view: DiffView) -> Option<PanelNav> {
    match key {
        KeyCode::Char('1') => Some(PanelNav::SelectTab(PanelTab::Diff)),
        KeyCode::Char('2') => Some(PanelNav::SelectTab(PanelTab::Files)),
        KeyCode::Char('3') => Some(PanelNav::SelectTab(PanelTab::Pr)),
        KeyCode::Char('4') => Some(PanelNav::SelectTab(PanelTab::Checks)),
        KeyCode::Char('5') => Some(PanelNav::SelectTab(PanelTab::Tests)),
        KeyCode::Tab => Some(PanelNav::CycleTab),
        KeyCode::UpArrow | KeyCode::Char('k') => Some(PanelNav::Up),
        KeyCode::DownArrow | KeyCode::Char('j') => Some(PanelNav::Down),
        KeyCode::Enter => match (tab, view) {
            (PanelTab::Diff, DiffView::FileList) => Some(PanelNav::Enter),
            // Files: toggle a dir's accordion / open a file in the pager.
            (PanelTab::Files, _) => Some(PanelNav::Enter),
            _ => None,
        },
        KeyCode::Escape => Some(PanelNav::Back),
        KeyCode::Char('o') => match tab {
            PanelTab::Diff | PanelTab::Files | PanelTab::Pr | PanelTab::Tests => {
                Some(PanelNav::Open)
            }
            PanelTab::Checks => None,
        },
        // Tests-tab actions (TAP/JSON/report explorer): run-all/failed, refresh
        // discovery, peek in bat, debug handoff.
        KeyCode::Char('R') if tab == PanelTab::Tests => Some(PanelNav::RunAll),
        KeyCode::Char('f') if tab == PanelTab::Tests => Some(PanelNav::RunFailed),
        KeyCode::Char('u') if tab == PanelTab::Tests => Some(PanelNav::Refresh),
        KeyCode::Char('b') if tab == PanelTab::Tests => Some(PanelNav::Peek),
        KeyCode::Char('d') if tab == PanelTab::Tests => Some(PanelNav::Debug),
        KeyCode::Char('O') if tab == PanelTab::Files => Some(PanelNav::OpenExternal),
        KeyCode::Char('J') if matches!(tab, PanelTab::Diff | PanelTab::Files) => {
            Some(PanelNav::NextDoc)
        }
        KeyCode::Char('K') if matches!(tab, PanelTab::Diff | PanelTab::Files) => {
            Some(PanelNav::PrevDoc)
        }
        KeyCode::Char('t') if matches!(tab, PanelTab::Diff | PanelTab::Files) => {
            Some(PanelNav::OpenTab)
        }
        KeyCode::Char('s') if matches!(tab, PanelTab::Diff | PanelTab::Files) => {
            Some(PanelNav::OpenPane)
        }
        KeyCode::Char('e') if matches!(tab, PanelTab::Diff | PanelTab::Files) => {
            Some(PanelNav::OpenEditorPane)
        }
        KeyCode::Char('y') if tab == PanelTab::Files => Some(PanelNav::RevealDrawer),
        KeyCode::Char('m') if tab == PanelTab::Pr => Some(PanelNav::Merge),
        KeyCode::Char('a') if tab == PanelTab::Pr => Some(PanelNav::Approve),
        KeyCode::Char('c') if tab == PanelTab::Pr => Some(PanelNav::Create),
        KeyCode::Char('r') if matches!(tab, PanelTab::Pr | PanelTab::Checks | PanelTab::Tests) => {
            Some(PanelNav::Rerun)
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tab_cycles_diff_files_pr_checks_tests() {
        assert_eq!(PanelTab::Diff.next(), PanelTab::Files);
        assert_eq!(PanelTab::Files.next(), PanelTab::Pr);
        assert_eq!(PanelTab::Pr.next(), PanelTab::Checks);
        assert_eq!(PanelTab::Checks.next(), PanelTab::Tests);
        assert_eq!(PanelTab::Tests.next(), PanelTab::Diff);
    }

    #[test]
    fn digit_keys_select_tabs() {
        let d = DiffView::FileList;
        assert_eq!(
            panel_nav_key(&KeyCode::Char('1'), PanelTab::Pr, d),
            Some(PanelNav::SelectTab(PanelTab::Diff))
        );
        assert_eq!(
            panel_nav_key(&KeyCode::Char('2'), PanelTab::Diff, d),
            Some(PanelNav::SelectTab(PanelTab::Files))
        );
        assert_eq!(
            panel_nav_key(&KeyCode::Char('3'), PanelTab::Diff, d),
            Some(PanelNav::SelectTab(PanelTab::Pr))
        );
        assert_eq!(
            panel_nav_key(&KeyCode::Char('4'), PanelTab::Diff, d),
            Some(PanelNav::SelectTab(PanelTab::Checks))
        );
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
    fn collapsed_dirs_hide_descendants_only() {
        let tree = build_file_tree(&[
            "src/cmd/pr.rs".to_string(),
            "src/main.rs".to_string(),
            "README.md".to_string(),
        ]);
        let mut collapsed = std::collections::HashSet::new();
        collapsed.insert("src".to_string());
        let vis: Vec<&str> = visible_file_indices(&tree, &collapsed)
            .into_iter()
            .map(|i| tree[i].path.as_str())
            .collect();
        assert_eq!(vis, vec!["README.md", "src"]);

        // Collapsing a nested dir keeps its siblings.
        let mut collapsed = std::collections::HashSet::new();
        collapsed.insert("src/cmd".to_string());
        let vis: Vec<&str> = visible_file_indices(&tree, &collapsed)
            .into_iter()
            .map(|i| tree[i].path.as_str())
            .collect();
        assert_eq!(vis, vec!["README.md", "src", "src/cmd", "src/main.rs"]);
    }

    #[test]
    fn files_tab_keys() {
        let d = DiffView::FileList;
        assert_eq!(
            panel_nav_key(&KeyCode::Enter, PanelTab::Files, d),
            Some(PanelNav::Enter)
        );
        assert_eq!(
            panel_nav_key(&KeyCode::Char('o'), PanelTab::Files, d),
            Some(PanelNav::Open)
        );
        assert_eq!(
            panel_nav_key(&KeyCode::Char('O'), PanelTab::Files, d),
            Some(PanelNav::OpenExternal)
        );
        assert_eq!(
            panel_nav_key(&KeyCode::Char('y'), PanelTab::Files, d),
            Some(PanelNav::RevealDrawer)
        );
        assert_eq!(panel_nav_key(&KeyCode::Char('O'), PanelTab::Diff, d), None);
    }

    #[test]
    fn reset_on_leave_clears_drill_state_but_keeps_cursors() {
        let mut ui = PanelUi {
            tab: PanelTab::Files,
            files_preview: true,
            diff_view: DiffView::FileDiff,
            diff_scroll: 7,
            files_cursor: 4,
            diff_cursor: 2,
            ..Default::default()
        };
        ui.reset_on_leave();
        assert!(!ui.drilled());
        assert_eq!(ui.diff_view, DiffView::FileList);
        assert!(!ui.files_preview);
        assert_eq!(ui.diff_scroll, 0);
        assert_eq!(ui.files_cursor, 4);
        assert_eq!(ui.diff_cursor, 2);
    }

    #[test]
    fn doc_navigation_and_promotion_keys() {
        let d = DiffView::FileList;
        for tab in [PanelTab::Diff, PanelTab::Files] {
            assert_eq!(
                panel_nav_key(&KeyCode::Char('J'), tab, d),
                Some(PanelNav::NextDoc)
            );
            assert_eq!(
                panel_nav_key(&KeyCode::Char('K'), tab, d),
                Some(PanelNav::PrevDoc)
            );
            assert_eq!(
                panel_nav_key(&KeyCode::Char('t'), tab, d),
                Some(PanelNav::OpenTab)
            );
            assert_eq!(
                panel_nav_key(&KeyCode::Char('s'), tab, d),
                Some(PanelNav::OpenPane)
            );
        }
        assert_eq!(panel_nav_key(&KeyCode::Char('J'), PanelTab::Pr, d), None);
        assert_eq!(
            panel_nav_key(&KeyCode::Char('t'), PanelTab::Checks, d),
            None
        );
        assert_eq!(
            panel_nav_key(&KeyCode::Char('e'), PanelTab::Files, d),
            Some(PanelNav::OpenEditorPane)
        );
    }

    #[test]
    fn enter_drills_only_in_diff_filelist() {
        assert_eq!(
            panel_nav_key(&KeyCode::Enter, PanelTab::Diff, DiffView::FileList),
            Some(PanelNav::Enter)
        );
        assert_eq!(
            panel_nav_key(&KeyCode::Enter, PanelTab::Diff, DiffView::FileDiff),
            None
        );
        assert_eq!(
            panel_nav_key(&KeyCode::Enter, PanelTab::Pr, DiffView::FileList),
            None
        );
    }

    #[test]
    fn action_keys_are_tab_scoped() {
        let d = DiffView::FileList;
        // m/a/c only on PR tab.
        assert_eq!(
            panel_nav_key(&KeyCode::Char('m'), PanelTab::Pr, d),
            Some(PanelNav::Merge)
        );
        assert_eq!(panel_nav_key(&KeyCode::Char('m'), PanelTab::Diff, d), None);
        // r on PR and Checks.
        assert_eq!(
            panel_nav_key(&KeyCode::Char('r'), PanelTab::Checks, d),
            Some(PanelNav::Rerun)
        );
        assert_eq!(panel_nav_key(&KeyCode::Char('r'), PanelTab::Diff, d), None);
        // o on Diff (edit) and PR (browser), not Checks.
        assert_eq!(
            panel_nav_key(&KeyCode::Char('o'), PanelTab::Diff, d),
            Some(PanelNav::Open)
        );
        assert_eq!(
            panel_nav_key(&KeyCode::Char('o'), PanelTab::Pr, d),
            Some(PanelNav::Open)
        );
        assert_eq!(
            panel_nav_key(&KeyCode::Char('o'), PanelTab::Checks, d),
            None
        );
    }
}
