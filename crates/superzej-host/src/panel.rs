//! The right panel: a tabbed Diff / PR / Checks view over the focused worktree.
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

/// Which of the three panel tabs is showing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum PanelTab {
    #[default]
    Diff,
    Pr,
    Checks,
}

impl PanelTab {
    /// Cycle to the next tab (Tab key).
    #[allow(dead_code)]
    pub fn next(self) -> Self {
        match self {
            PanelTab::Diff => PanelTab::Pr,
            PanelTab::Pr => PanelTab::Checks,
            PanelTab::Checks => PanelTab::Diff,
        }
    }
}

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
    /// `o`: open file in editor (Diff) or PR in browser (PR tab).
    Open,
    Merge,
    Approve,
    Create,
    Rerun,
}

/// Map a raw key to a panel nav intent given the current tab/view context.
/// Context matters: `j`/`k` scroll in FileDiff but move the cursor in FileList,
/// and the action keys (`m`/`a`/`c`/`r`/`o`) only bind on their relevant tab.
#[allow(dead_code)]
pub fn panel_nav_key(key: &KeyCode, tab: PanelTab, view: DiffView) -> Option<PanelNav> {
    match key {
        KeyCode::Char('1') => Some(PanelNav::SelectTab(PanelTab::Diff)),
        KeyCode::Char('2') => Some(PanelNav::SelectTab(PanelTab::Pr)),
        KeyCode::Char('3') => Some(PanelNav::SelectTab(PanelTab::Checks)),
        KeyCode::Tab => Some(PanelNav::CycleTab),
        KeyCode::UpArrow | KeyCode::Char('k') => Some(PanelNav::Up),
        KeyCode::DownArrow | KeyCode::Char('j') => Some(PanelNav::Down),
        KeyCode::Enter => {
            if tab == PanelTab::Diff && view == DiffView::FileList {
                Some(PanelNav::Enter)
            } else {
                None
            }
        }
        KeyCode::Escape => Some(PanelNav::Back),
        KeyCode::Char('o') => match tab {
            PanelTab::Diff | PanelTab::Pr => Some(PanelNav::Open),
            PanelTab::Checks => None,
        },
        KeyCode::Char('m') if tab == PanelTab::Pr => Some(PanelNav::Merge),
        KeyCode::Char('a') if tab == PanelTab::Pr => Some(PanelNav::Approve),
        KeyCode::Char('c') if tab == PanelTab::Pr => Some(PanelNav::Create),
        KeyCode::Char('r') if matches!(tab, PanelTab::Pr | PanelTab::Checks) => {
            Some(PanelNav::Rerun)
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tab_cycles_diff_pr_checks() {
        assert_eq!(PanelTab::Diff.next(), PanelTab::Pr);
        assert_eq!(PanelTab::Pr.next(), PanelTab::Checks);
        assert_eq!(PanelTab::Checks.next(), PanelTab::Diff);
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
            Some(PanelNav::SelectTab(PanelTab::Pr))
        );
        assert_eq!(
            panel_nav_key(&KeyCode::Char('3'), PanelTab::Diff, d),
            Some(PanelNav::SelectTab(PanelTab::Checks))
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
