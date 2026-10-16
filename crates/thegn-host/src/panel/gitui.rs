//! The git-family interaction model: lazygit-grade contexts layered over the
//! panel accordion. One width-independent state struct ([`GitUi`]) backs all
//! three panel widths — Normal renders it read-only, Half exposes the core
//! interactions, Full is the multi-region lazygit layout — so widening
//! mid-rebase keeps every cursor, mark, and flow intact.
//!
//! Input is table-driven: each [`GitView`] context declares its keys as data
//! ([`CtxKey`]), and [`git_key`] resolves a raw key against the focused
//! context's table. The same tables feed the help bar and the `?` cheatsheet,
//! so dispatch and documentation can never drift. Everything here is pure;
//! the event loop turns [`GitMsg`]s into svc calls.

use crate::layout::PanelWidth;
use crate::panel::frame::PanelColumn;
use crate::panel::staging::{self, StageDoc};
use termwiz::input::{KeyCode, Modifiers};
use thegn_core::rebase_todo::{TodoAction, TodoEntry};

/// Which git context owns the keys.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum GitView {
    /// The Changes/Files list (working-tree files).
    #[default]
    Files,
    Branches,
    Commits,
    Stash,
    /// A focused file diff with a line cursor (unstaged|staged pane).
    Staging,
    /// The files of a drilled-into commit.
    CommitFiles,
    /// Line-marking an old commit's diff into a custom patch.
    PatchBuilding,
    /// The interactive-rebase TODO editor.
    RebaseTodo,
    /// Annotated blame for a specific file.
    Blame,
}

/// How a wheel tick over a panel column should affect git panel state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GitWheel {
    Legacy,
    DrillStep,
    DetailScroll,
    Ignore,
}

/// How far a drag sample lies past the two-column zone, measured in rows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DragOverflow {
    Inside,
    Above(usize),
    Below(usize),
}

impl GitView {
    /// The context shown in the help bar / cheatsheet title.
    pub fn label(self) -> &'static str {
        match self {
            GitView::Files => "files",
            GitView::Branches => "branches",
            GitView::Commits => "commits",
            GitView::Stash => "stash",
            GitView::Staging => "staging",
            GitView::CommitFiles => "commit files",
            GitView::PatchBuilding => "patch",
            GitView::RebaseTodo => "rebase",
            GitView::Blame => "blame",
        }
    }
}

/// Which pane of the staging view is focused.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum StagePane {
    #[default]
    Unstaged,
    Staged,
}

/// The staging drill-in: a focused file diff with a line cursor. The line
/// address space is the flattened (hunk, line) enumeration of
/// `thegn_core::patch::parse_patch` over the SAME diff text the apply
/// path consumes — selections can never drift from the constructed patch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StagingUi {
    pub path: String,
    pub pane: StagePane,
    /// Cursor into the flattened line list.
    pub cursor: usize,
    /// Range anchor (`v` / shift+↓); selection = min..=max(anchor, cursor).
    pub anchor: Option<usize>,
    pub scroll: usize,
    /// The current diff was synthesized from an untracked worktree file.
    /// Its first stage must establish intent-to-add inside the mutation.
    pub untracked: bool,
}

impl StagingUi {
    pub fn new(path: &str) -> Self {
        StagingUi {
            path: path.to_string(),
            pane: StagePane::Unstaged,
            cursor: 0,
            anchor: None,
            scroll: 0,
            untracked: false,
        }
    }

    /// The selected inclusive line range under the current anchor/cursor.
    pub fn selection(&self) -> std::ops::RangeInclusive<usize> {
        match self.anchor {
            Some(a) => a.min(self.cursor)..=a.max(self.cursor),
            None => self.cursor..=self.cursor,
        }
    }
}

/// Sections whose first Enter enters row mode so its file-level key table can
/// handle the same activation as subsequent Enters.
pub fn enter_enters_row_mode(s: crate::panel::Section) -> bool {
    matches!(
        s,
        crate::panel::Section::Commits
            | crate::panel::Section::Branches
            | crate::panel::Section::Stash
            | crate::panel::Section::Changes
    )
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ChangesDrill {
    OpenEditor(String),
    Atomic,
    Staging {
        path: String,
        pane: StagePane,
        untracked: bool,
    },
}

/// Plan the Changes-row drill without causing a git mutation.
pub fn plan_changes_drill(row: &crate::panel::ChangeRow) -> ChangesDrill {
    if row.submodule.is_some() {
        ChangesDrill::Atomic
    } else if row.stage == crate::panel::Stage::Conflict {
        ChangesDrill::OpenEditor(row.path.clone())
    } else {
        ChangesDrill::Staging {
            path: row.path.clone(),
            pane: if row.stage == crate::panel::Stage::Staged {
                StagePane::Staged
            } else {
                StagePane::Unstaged
            },
            untracked: row.stage == crate::panel::Stage::Untracked,
        }
    }
}

pub fn plan_stage_lines(
    staging: &StagingUi,
    doc_pane: StagePane,
    path: String,
    diff: String,
    indices: Vec<(usize, usize)>,
) -> crate::gitmut::GitOp {
    if staging.untracked && doc_pane == StagePane::Unstaged {
        crate::gitmut::GitOp::StageUntrackedLines {
            path,
            synth_diff: diff,
            indices,
        }
    } else {
        crate::gitmut::GitOp::StageLines {
            path,
            diff,
            indices,
            target: match doc_pane {
                StagePane::Unstaged => crate::gitmut::StageTarget::Unstaged,
                StagePane::Staged => crate::gitmut::StageTarget::Staged,
            },
        }
    }
}

pub fn line_discard_refusal(staging: &StagingUi) -> Option<&'static str> {
    if staging.pane == StagePane::Staged {
        Some("unstage with space")
    } else if staging.untracked {
        Some("untracked file: d on the Changes row deletes it")
    } else {
        None
    }
}

pub fn staging_refetch(
    path: &str,
    old: &[crate::panel::ChangeRow],
    new: &[crate::panel::ChangeRow],
) -> Option<bool> {
    let old_row = old.iter().find(|row| row.path == path);
    let new_row = new.iter().find(|row| row.path == path);
    if old_row == new_row {
        return None;
    }
    Some(new_row.is_some_and(|row| row.stage == crate::panel::Stage::Untracked))
}

pub fn stash_message_or_default(text: &str, branch: &str) -> String {
    if text.trim().is_empty() {
        if branch.is_empty() {
            "WIP".into()
        } else {
            format!("WIP on {branch}")
        }
    } else {
        text.to_string()
    }
}

pub fn commit_precheck(data: &crate::panel::PanelData) -> Option<&'static str> {
    (data.merge.is_none()
        && data
            .changes
            .iter()
            .all(|row| row.stage == crate::panel::Stage::Untracked))
    .then_some("nothing staged: space stages the file, a stages all")
}

/// Interactive-rebase TODO editor state.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct RebaseUi {
    /// The base passed to `git rebase -i` once confirmed.
    pub base: String,
    /// Not yet running: the prepared plan. Running: the LIVE pending
    /// entries read from `rebase-merge/git-rebase-todo` — never a stale
    /// pre-rebase copy (entries the sequencer already executed are gone).
    pub todos: Vec<TodoEntry>,
    pub cursor: usize,
    /// True once the rebase is RUNNING and stopped on a conflict/edit (the
    /// editor then shows done/remaining from `rebase_status`).
    pub running: bool,
    pub conflict: bool,
    /// Running only: a live `rebase_status` load was requested/landed —
    /// gates the safety-net re-kick and ConfirmRebase (a rewrite must never
    /// run from an unloaded editor).
    pub todos_synced: bool,
    /// Running only: the pending entries AS READ from disk. The rewrite op
    /// carries this and refuses to write when the on-disk todo no longer
    /// matches it (externally edited mid-pause).
    pub baseline: Vec<TodoEntry>,
    /// Running only: entries the sequencer already executed (display).
    pub done: usize,
    /// Running only: the commit git stopped at (display, best effort).
    pub stopped_sha: Option<String>,
}

/// Bisect session state mirrored from the backend.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct BisectUi {
    pub bad_term: String,
    pub good_term: String,
    pub good: usize,
    pub skipped: usize,
    pub current: String,
    pub culprit: Option<String>,
}

/// Custom-patch building state: line marks per file of one commit's diff.
/// Mark indices live in the same flattened patch-line address space as
/// staging.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct PatchUi {
    pub commit: String,
    /// The file currently being marked (PatchBuilding view).
    pub path: String,
    /// Marked flattened line indices per path.
    pub marks: std::collections::HashMap<String, std::collections::BTreeSet<usize>>,
}

impl PatchUi {
    /// Total marked lines across all files.
    pub fn marked(&self) -> usize {
        self.marks.values().map(|s| s.len()).sum()
    }
}

/// State for an active merge/cherry-pick/revert mid-conflict.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SequencerUi {
    /// What is being merged/applied in (branch name, commit short, …).
    pub onto: String,
    /// Conflict marker is set while MERGE_HEAD / CHERRY_PICK_HEAD / REVERT_HEAD
    /// is present and at least one file is unresolved.
    pub conflict: bool,
}

/// A modal git flow in progress (rendered as a header chip + main region).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum GitFlow {
    #[default]
    None,
    Rebase(RebaseUi),
    Bisect(BisectUi),
    Patch(PatchUi),
    /// Diff-two-refs mode: everything diffs against this marked ref.
    Diffing(String),
    /// An in-progress `git merge` (possibly conflicted).
    Merge(SequencerUi),
    /// An in-progress `git cherry-pick` (possibly conflicted).
    CherryPick(SequencerUi),
    /// An in-progress `git revert` (possibly conflicted).
    Revert(SequencerUi),
}

/// A per-list fuzzy filter (`/`). Cursor and selection operate in FILTERED
/// index space; `map` translates to source indices at dispatch time.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ListFilter {
    pub view: GitView,
    pub query: String,
    /// True while the filter line is being typed (printable keys edit it).
    pub editing: bool,
    /// Filtered → source index map (identity when query is empty).
    pub map: Vec<usize>,
}

/// An in-flight git mutation: navigation stays live, effects are rejected
/// until the result lands.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingOp {
    pub label: String,
}

/// Which option menu to open. (Reset / diff / cheatsheet menus open through
/// their dedicated messages — `ResetMenu`, `ToggleDiffMark`, `Cheatsheet`.)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MenuKind {
    Rebase,
    Patch,
    BranchActions,
    CustomCommands,
    Bisect,
}

/// One fetched document for the line-cursor views: the path it describes,
/// the pane it came from (unstaged|staged diff for the staging view), the
/// flattened [`StageDoc`] the cursor walks, and the RAW diff text the apply
/// path consumes — captured together so a selection can never be applied
/// against a different diff than the one on screen.
#[derive(Debug, Clone, Default)]
pub struct StageDocState {
    pub path: String,
    pub pane: StagePane,
    pub doc: StageDoc,
    pub diff: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum DocLoad {
    #[default]
    Pending,
    Ready,
    Failed(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommitDetailKey {
    pub base: Option<String>,
    pub sha: String,
}

/// Per-view cursors + scrolls, so drilling around never loses your place.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ViewCursors {
    pub files: usize,
    pub branches: usize,
    pub commits: usize,
    pub stash: usize,
    pub commit_files: usize,
    pub scroll: usize,
}

impl ViewCursors {
    pub fn get(&self, view: GitView) -> usize {
        match view {
            GitView::Files => self.files,
            GitView::Branches => self.branches,
            GitView::Commits => self.commits,
            GitView::Stash => self.stash,
            GitView::CommitFiles => self.commit_files,
            _ => 0,
        }
    }

    pub fn set(&mut self, view: GitView, v: usize) {
        match view {
            GitView::Files => self.files = v,
            GitView::Branches => self.branches = v,
            GitView::Commits => self.commits = v,
            GitView::Stash => self.stash = v,
            GitView::CommitFiles => self.commit_files = v,
            _ => {}
        }
    }
}

/// All git-family interaction state. Width-independent: only the renderers
/// project it differently per [`PanelWidth`].
#[derive(Debug, Clone, Default)]
pub struct GitUi {
    pub focus: GitView,
    /// Views that launched each active drill, oldest first.
    pub return_to: Vec<GitView>,
    /// Width to restore after the last automatically widened drill is left.
    pub widened_from: Option<PanelWidth>,
    pub cur: ViewCursors,
    /// Range anchor in the focused LIST view (`v` / shift+↓).
    pub sel_anchor: Option<usize>,
    pub staging: Option<StagingUi>,
    pub flow: GitFlow,
    /// Copied commit shas (`C`), pasted by `V` (cherry-pick), oldest LAST —
    /// the executor reverses into oldest-first.
    pub clipboard: Vec<String>,
    /// The marked rebase base (`B`).
    pub mark_base: Option<String>,
    /// The marked diff ref (`W`); `flow` carries Diffing when active.
    pub diff_mark: Option<String>,
    pub filter: Option<ListFilter>,
    pub pending: Option<PendingOp>,
    /// The commit drilled into (CommitFiles / PatchBuilding source).
    pub drilled_commit: Option<String>,
    /// The staging view's fetched document (the focused file's unstaged or
    /// staged diff, flattened).
    pub stage_doc: Option<StageDocState>,
    /// The drilled commit's files: `(path, added, deleted)` numstat rows.
    pub commit_files: Vec<(String, u32, u32)>,
    pub commit_files_load: DocLoad,
    /// The patch-building view's fetched document (the drilled commit's diff
    /// limited to the drilled path).
    pub patch_doc: Option<StageDocState>,
    pub patch_load: DocLoad,
    /// Every patch doc fetched this flow, by path — custom-patch rendering
    /// needs the diff of EVERY marked file, not just the one on screen.
    pub patch_docs: std::collections::HashMap<String, StageDocState>,
    /// Failed per-file commit diffs, keyed by commit sha and path.
    pub patch_failed: std::collections::HashMap<(String, String), String>,
    pub patch_selected_path: Option<String>,
    pub commit_show: Option<(CommitDetailKey, std::result::Result<String, String>)>,
    pub commit_show_key: Option<CommitDetailKey>,
    /// Selection used solely to reset the cursor-follow detail scroll.
    pub commit_scroll_key: Option<CommitDetailKey>,
    /// Acceptance cutoff for arriving git-op results (same idiom as
    /// `hunks_gen`).
    pub op_gen: u64,
    /// Initial unresolved-conflict count for the current merge/cherry/revert
    /// flow — captured on first detection so `MergeBanner.total` can render
    /// the "X/Y resolved" bar across successive hydrations.
    pub merge_conflict_total: Option<usize>,
    /// Path of the file currently open in blame view.
    pub blame_path: Option<String>,
    /// Fetched blame rows (empty while the async fetch is in-flight).
    pub blame_rows: Vec<crate::panel::BlameRow>,
    pub blame_load: DocLoad,
    /// Cursor into `blame_rows`.
    pub blame_cursor: usize,
    /// Vertical scroll offset for the blame region.
    pub blame_scroll: usize,
    /// The selected stash's fetched patch (`stash show -p -u`), keyed by SHA.
    pub stash_diff: Option<(String, String)>,
    /// The selected branch's fetched log or read error, keyed by branch name
    /// and observed tip. The main region falls back to HEAD while this is out.
    pub branch_log: Option<(
        String,
        String,
        std::result::Result<Vec<crate::panel::CommitRow>, String>,
    )>,
}

/// Refuse pull/push shortcuts on another branch row: both operations target
/// HEAD, so the status explains that target and how to make the row HEAD.
pub(crate) fn head_only_refusal(
    data: &crate::panel::PanelData,
    row: Option<&crate::panel::BranchRow>,
    verb: &str,
) -> Option<String> {
    let row = row.filter(|row| !row.is_head)?;
    let head = if data.branch.is_empty() {
        "HEAD"
    } else {
        &data.branch
    };
    Some(format!(
        "{verb} acts on HEAD ({head}), not {}; space checks {} out; f fast-forwards it",
        row.name, row.name
    ))
}

impl GitUi {
    /// Whether this context is a detail view rather than an accordion list.
    pub fn is_drill(view: GitView) -> bool {
        matches!(
            view,
            GitView::Staging
                | GitView::CommitFiles
                | GitView::PatchBuilding
                | GitView::RebaseTodo
                | GitView::Blame
        )
    }

    /// Enter a detail view while remembering the context that launched it.
    pub fn drill(&mut self, to: GitView) {
        self.return_to.push(self.focus);
        self.focus = to;
    }

    /// Number of marked lines that would be lost by switching patch commits.
    pub fn patch_conflicts_with(&self, sha: &str) -> Option<usize> {
        match &self.flow {
            GitFlow::Patch(p) if p.commit != sha => Some(p.marked()),
            _ => None,
        }
    }

    fn clear_drill_state(&mut self) {
        self.staging = None;
        self.stage_doc = None;
        self.drilled_commit = None;
        self.commit_files.clear();
        self.commit_files_load = DocLoad::Pending;
        self.patch_docs.clear();
        self.patch_failed.clear();
        self.patch_selected_path = None;
        self.patch_doc = None;
        self.patch_load = DocLoad::Pending;
        self.commit_show = None;
        self.commit_show_key = None;
        self.commit_scroll_key = None;
        self.blame_path = None;
        self.blame_rows.clear();
        self.blame_load = DocLoad::Pending;
        self.blame_cursor = 0;
        self.blame_scroll = 0;
        self.sel_anchor = None;
    }

    /// Return focus to a section's home context, dropping transient drill state.
    pub fn retarget(&mut self, home: GitView) {
        self.clear_drill_state();
        if self
            .filter
            .as_ref()
            .is_some_and(|filter| filter.view != home)
        {
            self.filter = None;
        }
        self.return_to.clear();
        if matches!(&self.flow, GitFlow::Patch(p) if p.marked() == 0) {
            self.flow = GitFlow::None;
            self.patch_docs.clear();
        }
        self.focus = home;
    }

    /// Pop one drill layer. Returns false for a list focus so Esc can leave row mode.
    pub fn back(&mut self, home: GitView) -> bool {
        if !Self::is_drill(self.focus) {
            let stale = self.staging.is_some()
                || self.stage_doc.is_some()
                || self.drilled_commit.is_some()
                || !self.commit_files.is_empty()
                || self.patch_doc.is_some()
                || self.blame_path.is_some()
                || !self.blame_rows.is_empty();
            if stale {
                self.clear_drill_state();
            }
            return false;
        }

        match self.focus {
            GitView::Staging => {
                self.staging = None;
                self.stage_doc = None;
            }
            GitView::PatchBuilding => self.staging = None,
            GitView::CommitFiles => {
                self.drilled_commit = None;
                self.commit_files.clear();
                self.commit_files_load = DocLoad::Pending;
                self.patch_doc = None;
                self.patch_load = DocLoad::Pending;
                if matches!(&self.flow, GitFlow::Patch(p) if p.marked() == 0) {
                    self.flow = GitFlow::None;
                    self.patch_docs.clear();
                }
            }
            GitView::RebaseTodo => {
                if !matches!(&self.flow, GitFlow::Rebase(r) if r.running) {
                    self.flow = GitFlow::None;
                }
            }
            GitView::Blame => {
                self.blame_path = None;
                self.blame_rows.clear();
                self.blame_load = DocLoad::Pending;
                self.blame_cursor = 0;
                self.blame_scroll = 0;
            }
            _ => {}
        }
        let return_to = self.return_to.pop();
        self.focus = return_to
            .filter(|view| *view == home || Self::is_drill(*view))
            .unwrap_or(home);
        true
    }

    /// Repair focus after section, width, or tab changes.
    pub fn normalize(&mut self, home: Option<GitView>, width: PanelWidth) -> bool {
        let Some(home) = home else {
            return false;
        };
        let inconsistent = (Self::is_drill(self.focus) && width == PanelWidth::Normal)
            || (!Self::is_drill(self.focus) && self.focus != home);
        if inconsistent {
            self.retarget(home);
            return true;
        }
        false
    }

    /// Take the remembered width once focus has left the final drill.
    pub fn retract_width(&mut self) -> Option<PanelWidth> {
        if !Self::is_drill(self.focus) {
            self.widened_from.take()
        } else {
            None
        }
    }

    /// The focused list's selected inclusive range (anchor-aware).
    pub fn selection(&self) -> std::ops::RangeInclusive<usize> {
        let cur = self.cur.get(self.focus);
        match self.sel_anchor {
            Some(a) => a.min(cur)..=a.max(cur),
            None => cur..=cur,
        }
    }

    /// Reset transient state when the worktree changes underneath.
    pub fn reset_for_worktree(&mut self, home: Option<GitView>) {
        *self = GitUi {
            focus: home.unwrap_or_default(),
            op_gen: self.op_gen + 1,
            ..GitUi::default()
        };
    }

    pub fn selected_commit_detail_key(&self, sha: &str) -> CommitDetailKey {
        CommitDetailKey {
            base: match &self.flow {
                GitFlow::Diffing(base) => Some(base.clone()),
                _ => None,
            },
            sha: sha.to_string(),
        }
    }

    pub fn accept_commit_files(
        &mut self,
        sha: &str,
        result: std::result::Result<Vec<(String, u32, u32)>, String>,
    ) -> bool {
        if self.drilled_commit.as_deref() != Some(sha) {
            return false;
        }
        match result {
            Ok(files) => {
                self.commit_files = files;
                self.commit_files_load = DocLoad::Ready;
            }
            Err(error) => {
                self.commit_files.clear();
                self.commit_files_load = DocLoad::Failed(error);
            }
        }
        self.cur.commit_files = self
            .cur
            .commit_files
            .min(self.commit_files.len().saturating_sub(1));
        true
    }

    pub fn accept_patch_doc(
        &mut self,
        sha: &str,
        path: &str,
        result: std::result::Result<StageDocState, String>,
    ) -> bool {
        if self.drilled_commit.as_deref() != Some(sha) {
            return false;
        }
        let state = match result {
            Ok(state) => {
                if state.path != path {
                    return false;
                }
                self.patch_failed
                    .remove(&(sha.to_string(), path.to_string()));
                self.patch_load = DocLoad::Ready;
                state
            }
            Err(error) => {
                self.patch_failed
                    .insert((sha.to_string(), path.to_string()), error.clone());
                self.patch_doc = None;
                self.patch_load = DocLoad::Failed(error);
                return true;
            }
        };
        self.patch_docs.insert(state.path.clone(), state.clone());
        if self.staging.as_ref().is_some_and(|s| s.path == state.path) {
            self.patch_doc = Some(state);
        }
        true
    }

    pub fn accept_blame(
        &mut self,
        path: &str,
        result: std::result::Result<Vec<crate::panel::BlameRow>, String>,
    ) -> bool {
        if self.blame_path.as_deref() != Some(path) {
            return false;
        }
        match result {
            Ok(rows) => {
                self.blame_rows = rows;
                self.blame_load = DocLoad::Ready;
            }
            Err(error) => {
                self.blame_rows.clear();
                self.blame_load = DocLoad::Failed(error);
            }
        }
        self.blame_cursor = self
            .blame_cursor
            .min(self.blame_rows.len().saturating_sub(1));
        true
    }

    pub fn accept_commit_show(
        &mut self,
        key: CommitDetailKey,
        result: std::result::Result<String, String>,
    ) -> bool {
        if self.commit_show_key.as_ref() != Some(&key) {
            return false;
        }
        self.commit_show = Some((key, result));
        true
    }

    pub fn open_commit_file(&mut self, sha: &str, path: &str) -> Result<(), usize> {
        if let Some(marked) = self.patch_conflicts_with(sha) {
            if marked > 0 {
                return Err(marked);
            }
            self.flow = GitFlow::None;
            self.patch_docs.clear();
        }
        self.drilled_commit = Some(sha.to_string());
        self.staging = Some(StagingUi::new(path));
        self.patch_doc = self.patch_docs.get(path).cloned();
        self.patch_load = if self.patch_doc.is_some() {
            DocLoad::Ready
        } else {
            DocLoad::Pending
        };
        if let GitFlow::Patch(patch) = &mut self.flow {
            patch.path = path.to_string();
        }
        self.drill(GitView::PatchBuilding);
        Ok(())
    }

    pub fn mark_patch_lines(&mut self, path: &str, toggles: &[usize]) {
        if !matches!(&self.flow, GitFlow::Patch(_)) {
            let Some(commit) = self.drilled_commit.clone() else {
                return;
            };
            self.flow = GitFlow::Patch(PatchUi {
                commit,
                path: path.to_string(),
                marks: Default::default(),
            });
        }
        if let GitFlow::Patch(patch) = &mut self.flow {
            patch.path = path.to_string();
            let marks = patch.marks.entry(path.to_string()).or_default();
            for index in toggles {
                if !marks.remove(index) {
                    marks.insert(*index);
                }
            }
        }
    }

    pub fn click_drill_row(&mut self, index: usize) {
        match self.focus {
            GitView::CommitFiles => {
                self.cur.commit_files = index.min(self.commit_files.len().saturating_sub(1));
            }
            GitView::Blame => {
                self.blame_cursor = index.min(self.blame_rows.len().saturating_sub(1));
            }
            GitView::Staging | GitView::PatchBuilding => {
                if let Some(staging) = self.staging.as_mut() {
                    let doc = match self.focus {
                        GitView::Staging => self.stage_doc.as_ref(),
                        _ => self.patch_doc.as_ref(),
                    };
                    staging.cursor = doc
                        .map(|state| staging::clamp_cursor(&state.doc, index))
                        .unwrap_or(0);
                    staging.anchor = None;
                }
            }
            _ => {}
        }
    }

    pub fn wheel_route(&self, col: Option<PanelColumn>, has_detail_scroll: bool) -> GitWheel {
        let Some(col) = col else {
            return GitWheel::Legacy;
        };
        match (col, Self::is_drill(self.focus), has_detail_scroll) {
            (PanelColumn::Main, true, _) => GitWheel::DrillStep,
            (PanelColumn::Main, false, true) => GitWheel::DetailScroll,
            (PanelColumn::Main, false, false) => GitWheel::Ignore,
            (PanelColumn::Side, true, _) => GitWheel::Ignore,
            (PanelColumn::Side, false, _) => GitWheel::Legacy,
        }
    }

    pub fn drag_drill_to(&mut self, anchor: usize, index: usize) {
        match self.focus {
            GitView::Staging | GitView::PatchBuilding => {
                if let Some(staging) = self.staging.as_mut() {
                    let doc = match self.focus {
                        GitView::Staging => self.stage_doc.as_ref(),
                        GitView::PatchBuilding => self.patch_doc.as_ref(),
                        _ => None,
                    };
                    let cursor = doc
                        .map(|state| staging::clamp_cursor(&state.doc, index))
                        .unwrap_or(0);
                    let anchor = doc
                        .map(|state| staging::clamp_cursor(&state.doc, anchor))
                        .unwrap_or(0);
                    staging.cursor = cursor;
                    staging.anchor = (anchor != cursor).then_some(anchor);
                }
            }
            GitView::CommitFiles | GitView::Blame => self.click_drill_row(index),
            _ => {}
        }
    }

    /// Move a drag target past the visible edge by at most three cursorable
    /// rows for this pointer-motion sample.
    pub fn drag_autoscroll_target(&self, edge: usize, overflow: DragOverflow) -> usize {
        let (distance, down) = match overflow {
            DragOverflow::Inside => return edge,
            DragOverflow::Above(distance) => (distance, false),
            DragOverflow::Below(distance) => (distance, true),
        };
        let steps = distance.min(3);
        match self.focus {
            GitView::Staging | GitView::PatchBuilding => {
                let doc = match self.focus {
                    GitView::Staging => self.stage_doc.as_ref(),
                    GitView::PatchBuilding => self.patch_doc.as_ref(),
                    _ => None,
                };
                let Some(doc) = doc else {
                    return edge;
                };
                let mut target = edge;
                for _ in 0..steps {
                    let mut candidate = if down {
                        target.saturating_add(1)
                    } else if target == 0 {
                        return target;
                    } else {
                        target - 1
                    };
                    let mut found = None;
                    while candidate < doc.doc.lines.len() {
                        if staging::cursorable(&doc.doc, candidate) {
                            found = Some(candidate);
                            break;
                        }
                        if down {
                            candidate = candidate.saturating_add(1);
                        } else if candidate == 0 {
                            break;
                        } else {
                            candidate -= 1;
                        }
                    }
                    let Some(candidate) = found else {
                        break;
                    };
                    target = candidate;
                }
                target
            }
            GitView::CommitFiles => {
                if self.commit_files.is_empty() {
                    0
                } else if down {
                    edge.saturating_add(steps).min(self.commit_files.len() - 1)
                } else {
                    edge.min(self.commit_files.len() - 1).saturating_sub(steps)
                }
            }
            GitView::Blame => {
                if self.blame_rows.is_empty() {
                    0
                } else if down {
                    edge.saturating_add(steps).min(self.blame_rows.len() - 1)
                } else {
                    edge.min(self.blame_rows.len() - 1).saturating_sub(steps)
                }
            }
            _ => edge,
        }
    }

    /// Step the focused view's own cursor up/down. The flow-through list views
    /// (files/branches/commits/stash) are driven by the accordion; the
    /// line-cursor and drill-list views own their cursor and step it here.
    pub fn step_view_cursor(&mut self, down: bool) {
        match self.focus {
            GitView::RebaseTodo => {
                if let GitFlow::Rebase(r) = &mut self.flow {
                    let max = r.todos.len().saturating_sub(1);
                    r.cursor = if down {
                        (r.cursor + 1).min(max)
                    } else {
                        r.cursor.saturating_sub(1)
                    };
                }
            }
            GitView::Staging | GitView::PatchBuilding => {
                let cur = self.staging.as_ref().map_or(0, |s| s.cursor);
                let doc = match self.focus {
                    GitView::Staging => self.stage_doc.as_ref(),
                    _ => self.patch_doc.as_ref(),
                };
                let next = doc.map(|d| step_cursor(&d.doc, cur, down));
                if let (Some(next), Some(s)) = (next, self.staging.as_mut()) {
                    s.cursor = next;
                }
            }
            GitView::Blame => {
                let max = self.blame_rows.len().saturating_sub(1);
                self.blame_cursor = if down {
                    (self.blame_cursor + 1).min(max)
                } else {
                    self.blame_cursor.saturating_sub(1)
                };
            }
            GitView::CommitFiles => {
                let max = self.commit_files.len().saturating_sub(1);
                let previous = self.cur.commit_files;
                self.cur.commit_files = if down {
                    (self.cur.commit_files + 1).min(max)
                } else {
                    self.cur.commit_files.saturating_sub(1)
                };
                if self.cur.commit_files != previous {
                    self.patch_doc = None;
                }
            }
            _ => {}
        }
    }
}

/// A decoded git intent. Everything that mutates the repo flows through the
/// loop's mutation runner; navigation mutates [`GitUi`] directly.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GitMsg {
    /// A real action key pressed at a width that can't host it. Carries the
    /// ORIGINAL message plus the minimum width it needs: the dispatcher
    /// widens the panel and performs the action in one keypress (DWIM),
    /// instead of telling the user to press `e` themselves.
    WidthBlocked(Box<GitMsg>, PanelWidth),
    // navigation / selection
    CursorDown,
    CursorUp,
    ToggleRangeMode,
    /// Enter: drill into the row (file→staging, commit→files, …).
    Drill,
    /// Esc: peel one layer (filter → anchor → drill-out → leave panel).
    Back,
    NextHunk,
    PrevHunk,
    TogglePane,
    // staging / patch
    StageLines,
    SelectHunk,
    DiscardLines,
    /// One-key "revert hunk": discard every changed line of the hunk under the
    /// cursor, after a confirm (item 602).
    RevertHunk,
    StageAll,
    // files
    StageToggleFile,
    DiscardFile,
    // commits
    Squash,
    Fixup,
    Drop,
    Edit,
    Reword,
    Revert,
    MoveUp,
    MoveDown,
    AmendStaged,
    CopyCommits,
    PasteCommits,
    EnterInteractive,
    MarkBase,
    ToggleDiffMark,
    CheckoutSel,
    ResetMenu,
    // branches
    Pull,
    Push,
    NewBranch,
    FastForward,
    RebaseOntoSel,
    DeleteSel,
    CreateWorktree,
    OpenPrInBrowser,
    // stash
    StashPush,
    StashApply,
    StashPop,
    StashDrop,
    // rebase todo editor
    TodoSetAction(TodoAction),
    ConfirmRebase,
    // global-ish
    Commit,
    OpenMenu(MenuKind),
    Undo,
    Redo,
    FilterStart,
    Cheatsheet,
    /// Open the blame view for the selected file (Files / CommitFiles).
    BlameFile,
    /// Open the selected file in `$EDITOR` (CommitFiles).
    OpenFile,
}

/// One context key: chord text (display + parse), help label, message.
#[derive(Debug, Clone)]
pub struct CtxKey {
    pub chord: &'static str,
    pub label: &'static str,
    pub msg: GitMsg,
}

const fn k(chord: &'static str, label: &'static str, msg: GitMsg) -> CtxKey {
    CtxKey { chord, label, msg }
}

/// The full key table per context — the single source for dispatch, the
/// help bar, and the `?` cheatsheet. Order matters: the help bar shows a
/// prefix of this list.
pub fn context_keys(view: GitView) -> Vec<CtxKey> {
    match view {
        GitView::Files => vec![
            k("space", "stage", GitMsg::StageToggleFile),
            // Enter drills into the STAGING view (line-level stage/unstage),
            // so the label names the destination, not a generic "diff".
            k("enter", "stage lines", GitMsg::Drill),
            k("b", "blame", GitMsg::BlameFile),
            k("m", "flow menu", GitMsg::OpenMenu(MenuKind::Rebase)),
            k("c", "commit", GitMsg::Commit),
            k("d", "discard", GitMsg::DiscardFile),
            k("a", "stage all", GitMsg::StageAll),
            k("s", "stash", GitMsg::StashPush),
            k("D", "reset menu", GitMsg::ResetMenu),
            // (`v` range and `/` filter are NOT offered here: no Files action
            // consumes a range selection, and `list_labels` has no Files arm
            // so the filter always answered "not available in this view" —
            // both were advertised no-ops.)
            k("x", "commands", GitMsg::OpenMenu(MenuKind::CustomCommands)),
            k("z", "undo", GitMsg::Undo),
            k("Z", "redo", GitMsg::Redo),
            k("?", "keys", GitMsg::Cheatsheet),
        ],
        GitView::Branches => vec![
            k("space", "checkout", GitMsg::CheckoutSel),
            k("enter", "log", GitMsg::Drill),
            k("n", "new branch", GitMsg::NewBranch),
            k("d", "delete", GitMsg::DeleteSel),
            k("r", "rebase onto", GitMsg::RebaseOntoSel),
            k("f", "fast-forward", GitMsg::FastForward),
            k("p", "pull HEAD", GitMsg::Pull),
            k("P", "push HEAD", GitMsg::Push),
            k("m", "actions", GitMsg::OpenMenu(MenuKind::BranchActions)),
            k("w", "worktree", GitMsg::CreateWorktree),
            k("G", "open PR", GitMsg::OpenPrInBrowser),
            k("x", "commands", GitMsg::OpenMenu(MenuKind::CustomCommands)),
            k("z", "undo", GitMsg::Undo),
            k("Z", "redo", GitMsg::Redo),
            k("/", "filter", GitMsg::FilterStart),
            k("?", "keys", GitMsg::Cheatsheet),
        ],
        GitView::Commits => vec![
            k("enter", "diff", GitMsg::Drill),
            k("s", "squash", GitMsg::Squash),
            k("f", "fixup", GitMsg::Fixup),
            k("d", "drop", GitMsg::Drop),
            // `E`, not lazygit's `e`: the panel-wide width cycle owns bare `e`
            // (docs + hints say so everywhere), and the old binding made the
            // SECOND `e` press (after auto-widen) start an interactive rebase
            // — a documented navigation key rewriting history.
            k("E", "edit (rebase)", GitMsg::Edit),
            k("r", "reword", GitMsg::Reword),
            k("t", "revert", GitMsg::Revert),
            k("i", "interactive", GitMsg::EnterInteractive),
            k("m", "rebase menu", GitMsg::OpenMenu(MenuKind::Rebase)),
            k("A", "amend staged", GitMsg::AmendStaged),
            // The internal cherry-pick clipboard, NOT the OS clipboard — the
            // bare "copy" label read as a sha yank.
            k("C", "copy (pick)", GitMsg::CopyCommits),
            k("V", "paste (pick)", GitMsg::PasteCommits),
            k("B", "mark base", GitMsg::MarkBase),
            k("W", "diff mark", GitMsg::ToggleDiffMark),
            k("D", "reset menu", GitMsg::ResetMenu),
            k("b", "bisect", GitMsg::OpenMenu(MenuKind::Bisect)),
            k("space", "checkout", GitMsg::CheckoutSel),
            k("C-j", "move down", GitMsg::MoveDown),
            k("C-k", "move up", GitMsg::MoveUp),
            k("}", "move down", GitMsg::MoveDown),
            k("{", "move up", GitMsg::MoveUp),
            k("v", "range", GitMsg::ToggleRangeMode),
            k("x", "commands", GitMsg::OpenMenu(MenuKind::CustomCommands)),
            k("z", "undo", GitMsg::Undo),
            k("Z", "redo", GitMsg::Redo),
            k("/", "filter", GitMsg::FilterStart),
            k("?", "keys", GitMsg::Cheatsheet),
        ],
        GitView::Stash => vec![
            // The main region renders the selected stash's real patch
            // (`stash show -p -u`, fetched off-loop) — so "diff" is honest now.
            k("enter", "diff", GitMsg::Drill),
            k("space", "apply", GitMsg::StashApply),
            k("p", "pop", GitMsg::StashPop),
            k("d", "drop", GitMsg::StashDrop),
            k("x", "commands", GitMsg::OpenMenu(MenuKind::CustomCommands)),
            k("z", "undo", GitMsg::Undo),
            k("Z", "redo", GitMsg::Redo),
            k("/", "filter", GitMsg::FilterStart),
            k("?", "keys", GitMsg::Cheatsheet),
        ],
        GitView::Staging => vec![
            k("space", "stage line", GitMsg::StageLines),
            k("a", "hunk", GitMsg::SelectHunk),
            k("v", "range", GitMsg::ToggleRangeMode),
            k("d", "discard", GitMsg::DiscardLines),
            k("R", "revert hunk", GitMsg::RevertHunk),
            k("tab", "staged/unstaged", GitMsg::TogglePane),
            k("[", "prev hunk", GitMsg::PrevHunk),
            k("]", "next hunk", GitMsg::NextHunk),
            k("c", "commit", GitMsg::Commit),
            k("z", "undo", GitMsg::Undo),
            k("?", "keys", GitMsg::Cheatsheet),
        ],
        GitView::CommitFiles => vec![
            k("enter", "view diff", GitMsg::Drill),
            k("o", "open in editor", GitMsg::OpenFile),
            k("b", "blame", GitMsg::BlameFile),
            k("z", "undo", GitMsg::Undo),
            // (No `/` filter: this list renders raw indices, so the filter
            // always answered "not available in this view".)
            k("?", "keys", GitMsg::Cheatsheet),
        ],
        GitView::Blame => vec![
            k("enter", "open commit", GitMsg::Drill),
            k("?", "keys", GitMsg::Cheatsheet),
        ],
        GitView::PatchBuilding => vec![
            k("space", "add to patch", GitMsg::StageLines),
            k("a", "hunk", GitMsg::SelectHunk),
            k("v", "range", GitMsg::ToggleRangeMode),
            k("C-p", "patch menu", GitMsg::OpenMenu(MenuKind::Patch)),
            k("[", "prev hunk", GitMsg::PrevHunk),
            k("]", "next hunk", GitMsg::NextHunk),
            k("?", "keys", GitMsg::Cheatsheet),
        ],
        GitView::RebaseTodo => vec![
            k("enter", "confirm", GitMsg::ConfirmRebase),
            k("p", "pick", GitMsg::TodoSetAction(TodoAction::Pick)),
            k("s", "squash", GitMsg::TodoSetAction(TodoAction::Squash)),
            k("f", "fixup", GitMsg::TodoSetAction(TodoAction::Fixup)),
            k("d", "drop", GitMsg::TodoSetAction(TodoAction::Drop)),
            k("e", "edit", GitMsg::TodoSetAction(TodoAction::Edit)),
            k("r", "reword", GitMsg::TodoSetAction(TodoAction::Reword)),
            k("space", "pick/drop", GitMsg::StageToggleFile),
            k("C-j", "move down", GitMsg::MoveDown),
            k("C-k", "move up", GitMsg::MoveUp),
            k("}", "move down", GitMsg::MoveDown),
            k("{", "move up", GitMsg::MoveUp),
            k("m", "options", GitMsg::OpenMenu(MenuKind::Rebase)),
            k("?", "keys", GitMsg::Cheatsheet),
        ],
    }
}

/// Fuzzy-filter `labels` by `query`, returning matching source indices in
/// score order (neo_frizbee, same engine as the palette). Empty query → identity.
pub fn fuzzy_filter(labels: &[String], query: &str) -> Vec<usize> {
    if query.is_empty() {
        return (0..labels.len()).collect();
    }
    let refs: Vec<&str> = labels.iter().map(|s| s.as_str()).collect();
    crate::fff_backend::fuzzy_rank(query, &refs)
        .into_iter()
        .map(|(i, _)| i)
        .collect()
}

/// A flow-specific hint pair `(chord, label)` to prepend to the help bar when
/// an active sequencer flow overrides what `m` does. Returns `None` when no
/// sequencer is in progress (rebase is tracked separately in `RebaseTodo`).
pub fn flow_hint(flow: &GitFlow) -> Option<(&'static str, &'static str)> {
    match flow {
        GitFlow::Merge(s) if s.conflict => Some(("m", "merge continue/abort")),
        GitFlow::Merge(_) => Some(("m", "merge menu")),
        GitFlow::CherryPick(s) if s.conflict => Some(("m", "cherry continue/abort")),
        GitFlow::CherryPick(_) => Some(("m", "cherry menu")),
        GitFlow::Revert(s) if s.conflict => Some(("m", "revert continue/abort")),
        GitFlow::Revert(_) => Some(("m", "revert menu")),
        _ => None,
    }
}

/// The filter labels of a git list view — MUST match what the section
/// renderers feed `filtered_indices`, or dispatch would target a different
/// row than the one highlighted. Only the views whose renderers apply the
/// filter return labels; the rest filter as identity.
pub fn list_labels(view: GitView, data: &crate::panel::PanelData) -> Option<Vec<String>> {
    match view {
        GitView::Commits => Some(
            data.commits
                .iter()
                .map(|c| format!("{} {}", c.short, c.subject))
                .collect(),
        ),
        GitView::Branches => Some(data.branches.iter().map(|b| b.name.clone()).collect()),
        GitView::Stash => Some(data.stashes.iter().map(|s| s.message.clone()).collect()),
        _ => None,
    }
}

/// Display-ordered source indices of `view` under the live filter — the
/// exact mirror of the section renderers' `filtered_indices`, so a cursor in
/// display space always dispatches against the row it highlights.
pub fn display_map(ui: &GitUi, view: GitView, data: &crate::panel::PanelData) -> Vec<usize> {
    let len = match view {
        GitView::Files => data.changes.len(),
        GitView::Branches => data.branches.len(),
        GitView::Commits => data.commits.len(),
        GitView::Stash => data.stashes.len(),
        GitView::CommitFiles => ui.commit_files.len(),
        _ => 0,
    };
    if let Some(f) = ui
        .filter
        .as_ref()
        .filter(|f| f.view == view && !f.query.is_empty())
        && let Some(labels) = list_labels(view, data)
    {
        return fuzzy_filter(&labels, &f.query);
    }
    (0..len).collect()
}

/// Paste into the active git filter only while its panel input owns focus.
/// Returns false when the paste belongs to the terminal pane instead.
pub fn paste_filter(
    ui: &mut crate::panel::PanelUi,
    panel_visible: bool,
    panel_focused: bool,
    text: &str,
    data: &crate::panel::PanelData,
) -> bool {
    let view = {
        let Some(filter) = ui
            .git
            .filter
            .as_mut()
            .filter(|filter| panel_visible && panel_focused && filter.editing)
        else {
            return false;
        };
        filter
            .query
            .extend(text.chars().filter(|character| !character.is_control()));
        filter.view
    };
    let map = display_map(&ui.git, view, data);
    let max_cursor = map.len().saturating_sub(1);
    if let Some(filter) = ui.git.filter.as_mut() {
        filter.map = map;
    }
    let cursor = ui.git.cur.get(view).min(max_cursor);
    ui.git.cur.set(view, cursor);
    ui.cursor = cursor;
    ui.git.sel_anchor = None;
    true
}

/// Translate the focused list's display cursor to its source index.
pub fn source_at(ui: &GitUi, view: GitView, data: &crate::panel::PanelData) -> Option<usize> {
    display_map(ui, view, data).get(ui.cur.get(view)).copied()
}

/// The focused list's anchor-aware selection as SOURCE indices, in display
/// order (newest-first for commits).
pub fn selected_sources(ui: &GitUi, data: &crate::panel::PanelData) -> Vec<usize> {
    let map = display_map(ui, ui.focus, data);
    ui.selection().filter_map(|d| map.get(d).copied()).collect()
}

/// The commits selection as `(oldest_sha, target_shas)`: targets in display
/// order, oldest = the highest SOURCE index (the log is newest-first).
pub fn commit_selection(
    ui: &GitUi,
    data: &crate::panel::PanelData,
) -> Option<(String, Vec<String>)> {
    let sources = selected_sources(ui, data);
    let oldest = sources.iter().copied().max()?;
    let targets: Vec<String> = sources
        .iter()
        .filter_map(|&i| data.commits.get(i).map(|c| c.sha.clone()))
        .collect();
    let oldest = data.commits.get(oldest)?.sha.clone();
    (!targets.is_empty()).then_some((oldest, targets))
}

/// How an interactive rebase gets its starting point. An explicit `B` mark
/// overrides the ordinary selected-commit range.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PlanFrom {
    Target { oldest: String, base: String },
    Mark(String),
}

pub fn interactive_anchor(ui: &GitUi, data: &crate::panel::PanelData) -> Option<PlanFrom> {
    if let Some(mark) = &ui.mark_base {
        return Some(PlanFrom::Mark(mark.clone()));
    }
    let (oldest, _) = commit_selection(ui, data)?;
    let commit = data.commits.iter().find(|commit| commit.sha == oldest)?;
    let base = if commit.parents.is_empty() {
        "--root".to_owned()
    } else {
        format!("{}^", commit.sha)
    };
    Some(PlanFrom::Target { oldest, base })
}

/// The selected `(hunk, line)` pairs of a staging doc over an inclusive
/// flattened range (headers / context / markers drop silently).
pub fn sel_pairs(doc: &StageDoc, range: std::ops::RangeInclusive<usize>) -> Vec<(usize, usize)> {
    range
        .filter(|&i| staging::selectable(doc, i))
        .map(|i| (doc.lines[i].hunk, doc.lines[i].line))
        .collect()
}

/// Step a line cursor to the adjacent cursorable line (no-op at the ends).
pub fn step_cursor(doc: &StageDoc, cur: usize, down: bool) -> usize {
    if down {
        (cur + 1..doc.lines.len())
            .find(|&i| staging::cursorable(doc, i))
            .unwrap_or(cur)
    } else {
        (0..cur)
            .rev()
            .find(|&i| staging::cursorable(doc, i))
            .unwrap_or(cur)
    }
}

/// Retag the cursor todo (commit entries only — `exec`/`label` lines are
/// inert to the editor).
pub fn todo_retag_at(todos: &mut [TodoEntry], cursor: usize, action: TodoAction) {
    if let Some(t) = todos.get_mut(cursor)
        && t.action.is_commit()
    {
        t.action = action;
    }
}

/// Space in the rebase editor: toggle the cursor todo pick⇄drop.
pub fn todo_toggle_at(todos: &mut [TodoEntry], cursor: usize) {
    if let Some(t) = todos.get_mut(cursor) {
        match t.action {
            TodoAction::Drop => t.action = TodoAction::Pick,
            ref a if a.is_commit() => t.action = TodoAction::Drop,
            _ => {}
        }
    }
}

/// Reorder the cursor todo one step (via core's structural-safety rules);
/// returns the cursor's new position (unchanged when the move is refused).
pub fn todo_move(todos: &mut Vec<TodoEntry>, cursor: usize, up: bool) -> usize {
    let Some(sha) = todos.get(cursor).map(|t| t.sha.clone()) else {
        return cursor;
    };
    match thegn_core::rebase_todo::move_entry(todos, &sha, up) {
        Ok(moved) => {
            let at = moved.iter().position(|t| t.sha == sha).unwrap_or(cursor);
            *todos = moved;
            at
        }
        Err(_) => cursor,
    }
}

/// Mirror an externally-driven rebase (started in a shell pane, say) into
/// the flow state from the hydrated merge banner, and clear a finished one.
/// Returns the status note to show when the rebase ended. The caller must
/// skip this while one of OUR ops is pending (its result is authoritative).
pub fn sync_rebase_flow(git: &mut GitUi, banner: Option<(&str, usize)>) -> Option<&'static str> {
    match banner {
        Some(("REBASING", unresolved)) => {
            match &mut git.flow {
                GitFlow::Rebase(r) => {
                    r.running = true;
                    r.conflict = unresolved > 0;
                }
                GitFlow::None => {
                    git.flow = GitFlow::Rebase(RebaseUi {
                        running: true,
                        conflict: unresolved > 0,
                        ..RebaseUi::default()
                    });
                }
                GitFlow::Patch(p) if p.marked() == 0 => {
                    git.patch_docs.clear();
                    git.flow = GitFlow::Rebase(RebaseUi {
                        running: true,
                        conflict: unresolved > 0,
                        ..RebaseUi::default()
                    });
                }
                _ => {}
            }
            None
        }
        None if matches!(&git.flow, GitFlow::Rebase(r) if r.running) => {
            git.flow = GitFlow::None;
            if git.focus == GitView::RebaseTodo {
                git.back(GitView::Commits);
            }
            Some("rebase finished")
        }
        _ => None,
    }
}

/// Mirror an externally-driven merge / cherry-pick / revert into `git.flow`,
/// tracking the initial conflict count for the progress bar. Clears stale
/// flows when the banner disappears — AND when the banner's KIND changes
/// (e.g. `git merge --abort && git rebase main` in a shell pane flips
/// MERGING→REBASING without ever passing through `None`; the old
/// same-kind-only sync left the Merge flow wedged forever, with the help bar
/// advertising merge keys and the progress bar using the merge's stale
/// conflict denominator). Call alongside `sync_rebase_flow`.
pub fn sync_sequencer_flow(git: &mut GitUi, label: Option<&str>, onto: &str, unresolved: usize) {
    let is_sequencer_flow = |f: &GitFlow| {
        matches!(
            f,
            GitFlow::Merge(_) | GitFlow::CherryPick(_) | GitFlow::Revert(_)
        )
    };
    match label {
        Some(l @ ("MERGING" | "CHERRY-PICK" | "REVERTING")) => {
            let same_kind = matches!(
                (&git.flow, l),
                (GitFlow::Merge(_), "MERGING")
                    | (GitFlow::CherryPick(_), "CHERRY-PICK")
                    | (GitFlow::Revert(_), "REVERTING")
            );
            if same_kind {
                // Update the live flow in place.
                if let GitFlow::Merge(s) | GitFlow::CherryPick(s) | GitFlow::Revert(s) =
                    &mut git.flow
                {
                    s.conflict = unresolved > 0;
                    s.onto = onto.to_owned();
                }
                if git.merge_conflict_total.is_none() {
                    git.merge_conflict_total = Some(unresolved);
                }
            } else if matches!(git.flow, GitFlow::None)
                || is_sequencer_flow(&git.flow)
                || matches!(&git.flow, GitFlow::Patch(p) if p.marked() == 0)
            {
                // Fresh detection, or the kind changed under us: install the
                // new flavor with a fresh conflict total. A running Rebase
                // flow is left alone (that's `sync_rebase_flow`'s domain).
                let ui = SequencerUi {
                    onto: onto.to_owned(),
                    conflict: unresolved > 0,
                };
                git.flow = match l {
                    "MERGING" => GitFlow::Merge(ui),
                    "CHERRY-PICK" => GitFlow::CherryPick(ui),
                    _ => GitFlow::Revert(ui),
                };
                git.patch_docs.clear();
                git.merge_conflict_total = Some(unresolved);
            }
        }
        None => {
            // Clear any sequencer flow that has now finished.
            if is_sequencer_flow(&git.flow) {
                git.flow = GitFlow::None;
                git.merge_conflict_total = None;
            }
        }
        // An unknown / REBASING banner while a stale sequencer flow is up:
        // the kind changed (abort + rebase) — clear so `sync_rebase_flow`
        // can install the Rebase flow on this or the next pass.
        Some(_) => {
            if is_sequencer_flow(&git.flow) {
                git.flow = GitFlow::None;
                git.merge_conflict_total = None;
            }
        }
    }
}

/// Whether a chord string matches a raw key event.
fn chord_matches(chord: &str, key: &KeyCode, mods: Modifiers) -> bool {
    let ctrl = mods.contains(Modifiers::CTRL);
    let shift = mods.contains(Modifiers::SHIFT);
    match chord {
        "space" => !ctrl && matches!(key, KeyCode::Char(' ')),
        "enter" => !ctrl && matches!(key, KeyCode::Enter),
        "tab" => !ctrl && matches!(key, KeyCode::Tab),
        c if c.starts_with("C-") => {
            let target = c.chars().nth(2);
            ctrl && matches!(key, KeyCode::Char(ch) if Some(*ch) == target)
        }
        c => {
            // Single char: uppercase chords imply shift (termwiz delivers the
            // shifted char), lowercase must NOT be shifted/ctrl'd.
            let mut it = c.chars();
            match (it.next(), it.next()) {
                (Some(ch), None) => {
                    !ctrl
                        && matches!(key, KeyCode::Char(k) if *k == ch)
                        && (ch.is_uppercase() || !ch.is_alphabetic() || !shift)
                }
                _ => false,
            }
        }
    }
}

/// Which messages are pure navigation (allowed while an op is pending, and
/// at every width).
fn is_navigation(msg: &GitMsg) -> bool {
    matches!(
        msg,
        GitMsg::CursorDown
            | GitMsg::CursorUp
            | GitMsg::ToggleRangeMode
            | GitMsg::Drill
            | GitMsg::Back
            | GitMsg::NextHunk
            | GitMsg::PrevHunk
            | GitMsg::TogglePane
            | GitMsg::FilterStart
            | GitMsg::Cheatsheet
    )
}

/// Messages that need the Full layout (modal flows + marks).
fn needs_full(msg: &GitMsg) -> bool {
    matches!(
        msg,
        GitMsg::EnterInteractive
            | GitMsg::MarkBase
            | GitMsg::ToggleDiffMark
            | GitMsg::OpenMenu(MenuKind::Bisect)
            | GitMsg::OpenMenu(MenuKind::Patch)
            | GitMsg::TodoSetAction(_)
            | GitMsg::ConfirmRebase
            | GitMsg::MoveUp
            | GitMsg::MoveDown
    )
}

/// The Ctrl chords git claims (so the loop can carve them out of the global
/// no-CTRL guard) — exactly Ctrl+j/k in Commits/RebaseTodo and Ctrl+p in
/// PatchBuilding.
pub fn git_claims_ctrl(ui: &crate::panel::PanelUi, key: &KeyCode) -> bool {
    if !ui.open.is_git_family() || !ui.row_mode {
        return false;
    }
    matches!(
        (ui.git.focus, key),
        (
            GitView::Commits | GitView::RebaseTodo,
            KeyCode::Char('j' | 'k')
        ) | (GitView::PatchBuilding, KeyCode::Char('p'))
    )
}

/// Whether `msg` dispatches at `width` — the same gate [`git_key`] applies,
/// exposed so the help bar advertises only what a press will actually do.
pub fn allowed_at_width(msg: &GitMsg, width: PanelWidth) -> bool {
    match width {
        PanelWidth::Normal => is_navigation(msg),
        PanelWidth::Half => !needs_full(msg),
        PanelWidth::Full => true,
    }
}

/// Resolve a raw key against the focused git context. `None` falls through
/// to `accordion_key`, the per-section action keys, then the global keymap.
///
/// Gating, in order: row-mode only; while an op is pending only navigation;
/// at Normal width only navigation (the help bar shows `e expand` instead of
/// the action keys); Full-only messages are dropped at Half.
pub fn git_key(key: &KeyCode, mods: Modifiers, ui: &crate::panel::PanelUi) -> Option<GitMsg> {
    if !ui.open.is_git_family() || !ui.row_mode {
        return None;
    }
    let view = ui.git.focus;
    // Esc handling is the accordion's (LeaveRows) — except when there is a
    // peelable layer (filter / anchor / drill), which the loop resolves via
    // Back.
    if matches!(key, KeyCode::Escape) {
        let peelable = ui.git.filter.is_some()
            || ui.git.sel_anchor.is_some()
            || ui
                .git
                .staging
                .as_ref()
                .is_some_and(|staging| staging.anchor.is_some())
            || GitUi::is_drill(view);
        return peelable.then_some(GitMsg::Back);
    }
    // Plain cursor keys belong to the accordion in list views (it owns
    // flow-through navigation); the line-cursor views claim them.
    // CommitFiles is a drill list with its own cursor (`git.cur.commit_files`),
    // so it claims j/k/↑↓ too — otherwise the highlight would never move.
    if matches!(
        view,
        GitView::Staging
            | GitView::PatchBuilding
            | GitView::RebaseTodo
            | GitView::Blame
            | GitView::CommitFiles
    ) && !mods.contains(Modifiers::SHIFT)
        && !mods.contains(Modifiers::CTRL)
    {
        match key {
            KeyCode::DownArrow | KeyCode::Char('j') => return Some(GitMsg::CursorDown),
            KeyCode::UpArrow | KeyCode::Char('k') => return Some(GitMsg::CursorUp),
            _ => {}
        }
    }
    let table = context_keys(view);
    let hit = table.iter().find(|ck| chord_matches(ck.chord, key, mods))?;
    let msg = hit.msg.clone();
    if ui.git.pending.is_some() && !is_navigation(&msg) {
        return None;
    }
    // A paused rebase whose live-todo read hasn't landed: the editor holds
    // a list that's about to be replaced, so edits to it (retag, reorder,
    // confirm) are dropped until the sync arrives — navigation stays live.
    if view == GitView::RebaseTodo
        && matches!(&ui.git.flow, GitFlow::Rebase(r) if r.running && !r.todos_synced)
        && !is_navigation(&msg)
    {
        return None;
    }
    match ui.width {
        // A blocked key must not be a SILENT no-op — the help bar advertises
        // these keys at every width, so hand the dispatcher the message plus
        // the width it needs and let it widen-and-perform. Keys whose effect
        // is width-independent (staging the row, the commit / discard modals)
        // execute at every width without widening.
        PanelWidth::Normal if !is_navigation(&msg) && !works_at_normal(&msg) => {
            let want = if needs_full(&msg) {
                PanelWidth::Full
            } else {
                PanelWidth::Half
            };
            Some(GitMsg::WidthBlocked(Box::new(msg), want))
        }
        PanelWidth::Half if needs_full(&msg) => {
            Some(GitMsg::WidthBlocked(Box::new(msg), PanelWidth::Full))
        }
        _ => Some(msg),
    }
}

/// Effect messages that are safe (and expected) at the resting width: they
/// act on the highlighted row or open a modal, needing no wide layout.
fn works_at_normal(msg: &GitMsg) -> bool {
    matches!(
        msg,
        GitMsg::StageToggleFile | GitMsg::Commit | GitMsg::DiscardFile
    )
}

impl GitMsg {
    /// A short human name for the DWIM-widen status crumb
    /// ("widened for squash").
    pub fn dwim_label(&self) -> &'static str {
        match self {
            GitMsg::Squash => "squash",
            GitMsg::Fixup => "fixup",
            GitMsg::Drop => "drop",
            GitMsg::Edit => "edit (rebase)",
            GitMsg::Reword => "reword",
            GitMsg::Revert => "revert",
            GitMsg::EnterInteractive => "interactive rebase",
            GitMsg::AmendStaged => "amend",
            GitMsg::CopyCommits => "copy (pick)",
            GitMsg::PasteCommits => "paste (pick)",
            GitMsg::MarkBase => "mark base",
            GitMsg::ToggleDiffMark => "diff mark",
            GitMsg::ResetMenu => "reset menu",
            GitMsg::CheckoutSel => "checkout",
            GitMsg::DeleteSel => "delete",
            GitMsg::RebaseOntoSel => "rebase onto",
            GitMsg::FastForward => "fast-forward",
            GitMsg::Pull => "pull HEAD",
            GitMsg::Push => "push HEAD",
            GitMsg::NewBranch => "new branch",
            GitMsg::CreateWorktree => "worktree",
            GitMsg::StashPush => "stash",
            GitMsg::StashApply => "apply stash",
            GitMsg::StashPop => "pop stash",
            GitMsg::StashDrop => "drop stash",
            GitMsg::StageAll => "stage all",
            GitMsg::Undo => "undo",
            GitMsg::Redo => "redo",
            GitMsg::OpenMenu(_) => "menu",
            GitMsg::ConfirmRebase => "rebase",
            GitMsg::MoveUp | GitMsg::MoveDown => "reorder",
            GitMsg::TodoSetAction(_) => "todo edit",
            _ => "this action",
        }
    }
}

/// [`git_key`], but the Changes section's semantic-impact footer and its expanded
/// entity rows (cursor at or past the file-row count) are NOT git file rows —
/// suppress the git claim there so Enter/activation falls through to the accordion
/// `Select` (which toggles the breakdown / drills into the entity), matching a
/// mouse click. `chg` = short for `changes`, kept terse so the single call site in
/// the size-ratcheted `run.rs` stays one line under `max_width`.
///
/// When the cursor sits at or past the file-row count in Changes, the only rows are
/// the footer + entity rows, which the renderer emits solely when `entities.is_some()`,
/// so the row-count check alone identifies them.
pub fn git_key_chg(
    key: &KeyCode,
    mods: Modifiers,
    ui: &crate::panel::PanelUi,
    model: &crate::chrome::FrameModel,
) -> Option<GitMsg> {
    if matches!(key, KeyCode::Enter)
        && ui.open == crate::panel::Section::Changes
        && model
            .panel
            .changes
            .get(ui.cursor)
            .is_some_and(|row| row.submodule.is_some())
        && ui.cursor
            < crate::panel::visible_change_files(&model.panel, ui.width != PanelWidth::Normal)
    {
        return None;
    }
    // Suppress ONLY when footer/entity rows can actually exist (the renderer
    // emits them solely when `entities` is populated). The bare row-count
    // check also fired on a CLEAN tree (`0 >= 0`) and on a stale cursor,
    // silently killing every git key in Changes — `?`, `c` commit, `s` stash,
    // `z` undo all dead with no message.
    if ui.open == crate::panel::Section::Changes && model.panel.entities.is_some() {
        let deep = ui.width != PanelWidth::Normal;
        if ui.cursor >= crate::panel::visible_change_files(&model.panel, deep) {
            return None;
        }
    }
    git_key(key, mods, ui)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::panel::{PanelUi, Section};

    fn ui_at(view: GitView, width: PanelWidth) -> PanelUi {
        let mut ui = PanelUi {
            open: Section::Commits,
            width,
            row_mode: true,
            ..Default::default()
        };
        ui.git.focus = view;
        ui
    }

    fn change(path: &str, stage: crate::panel::Stage) -> crate::panel::ChangeRow {
        crate::panel::ChangeRow {
            path: path.into(),
            name: path.into(),
            stage,
            ..Default::default()
        }
    }

    #[test]
    fn changes_enter_is_staging_in_section_and_row_mode() {
        use crate::panel::Section;
        for width in [PanelWidth::Normal, PanelWidth::Half, PanelWidth::Full] {
            let mut ui = ui_at(GitView::Files, width);
            ui.open = Section::Changes;
            ui.row_mode = false;
            assert!(enter_enters_row_mode(Section::Changes));
            assert_eq!(
                git_key_chg(
                    &KeyCode::Enter,
                    Modifiers::NONE,
                    &ui,
                    &crate::chrome::FrameModel::default(),
                ),
                None
            );
            ui.row_mode = true;
            assert!(matches!(
                git_key_chg(
                    &KeyCode::Enter,
                    Modifiers::NONE,
                    &ui,
                    &crate::chrome::FrameModel::default(),
                ),
                Some(GitMsg::Drill | GitMsg::WidthBlocked(_, _))
            ));
        }
    }

    #[test]
    fn submodule_row_releases_enter() {
        let mut model = crate::chrome::FrameModel::default();
        model.panel.changes.push(crate::panel::ChangeRow {
            path: "sub".into(),
            submodule: Some(crate::panel::SubmoduleChange {
                diff: thegn_core::submodule::SubmoduleDiff {
                    path: "sub".into(),
                    old_sha: "old".into(),
                    new_sha: "new".into(),
                    kind: thegn_core::submodule::SubmoduleDiffKind::Changed,
                },
                state: None,
                summary: None,
            }),
            ..Default::default()
        });
        let mut ui = ui_at(GitView::Files, PanelWidth::Half);
        ui.open = crate::panel::Section::Changes;
        assert_eq!(
            git_key_chg(&KeyCode::Enter, Modifiers::NONE, &ui, &model),
            None
        );
        assert_eq!(
            git_key_chg(&KeyCode::Char(' '), Modifiers::NONE, &ui, &model),
            Some(GitMsg::StageToggleFile)
        );
    }

    #[test]
    fn untracked_drill_plans_staging_without_intent_to_add() {
        assert_eq!(
            plan_changes_drill(&change("new.txt", crate::panel::Stage::Untracked)),
            ChangesDrill::Staging {
                path: "new.txt".into(),
                pane: StagePane::Unstaged,
                untracked: true,
            }
        );
    }

    #[test]
    fn untracked_stage_plans_lazy_intent_to_add() {
        let mut staging = StagingUi::new("new.txt");
        staging.untracked = true;
        assert!(matches!(
            plan_stage_lines(
                &staging,
                StagePane::Unstaged,
                "new.txt".into(),
                "diff".into(),
                vec![]
            ),
            crate::gitmut::GitOp::StageUntrackedLines { .. }
        ));
        staging.untracked = false;
        assert!(matches!(
            plan_stage_lines(
                &staging,
                StagePane::Unstaged,
                "tracked.txt".into(),
                "diff".into(),
                vec![]
            ),
            crate::gitmut::GitOp::StageLines { .. }
        ));
        staging.untracked = true;
        assert!(matches!(
            plan_stage_lines(
                &staging,
                StagePane::Staged,
                "new.txt".into(),
                "diff".into(),
                vec![]
            ),
            crate::gitmut::GitOp::StageLines {
                target: crate::gitmut::StageTarget::Staged,
                ..
            }
        ));
    }

    #[test]
    fn staged_pane_refuses_line_discard() {
        let mut staging = StagingUi::new("f");
        staging.pane = StagePane::Staged;
        assert_eq!(line_discard_refusal(&staging), Some("unstage with space"));
        staging.pane = StagePane::Unstaged;
        staging.untracked = true;
        assert_eq!(
            line_discard_refusal(&staging),
            Some("untracked file: d on the Changes row deletes it")
        );
    }

    #[test]
    fn staging_refetch_detects_count_stage_and_absence() {
        let old = change("f", crate::panel::Stage::Unstaged);
        let mut changed = old.clone();
        changed.added = 1;
        assert_eq!(
            staging_refetch("f", std::slice::from_ref(&old), &[changed]),
            Some(false)
        );
        let staged = change("f", crate::panel::Stage::Untracked);
        assert_eq!(
            staging_refetch("f", std::slice::from_ref(&old), &[staged]),
            Some(true)
        );
        assert_eq!(
            staging_refetch("f", std::slice::from_ref(&old), &[]),
            Some(false)
        );
        assert_eq!(
            staging_refetch(
                "f",
                std::slice::from_ref(&old),
                &[old.clone(), change("other", crate::panel::Stage::Unstaged),]
            ),
            None
        );
    }

    #[test]
    fn stash_message_defaults_to_wip_on_branch() {
        assert_eq!(stash_message_or_default("", "main"), "WIP on main");
        assert_eq!(stash_message_or_default("note", "main"), "note");
        assert_eq!(stash_message_or_default("  ", ""), "WIP");
    }

    #[test]
    fn commit_precheck_refuses_with_nothing_staged() {
        let mut data = crate::panel::PanelData {
            changes: vec![
                change("new-a", crate::panel::Stage::Untracked),
                change("new-b", crate::panel::Stage::Untracked),
            ],
            ..Default::default()
        };
        assert_eq!(
            commit_precheck(&data),
            Some("nothing staged: space stages the file, a stages all")
        );
        data.changes[0].stage = crate::panel::Stage::Staged;
        assert_eq!(commit_precheck(&data), None);
        let mut partially_staged = crate::panel::PanelData {
            changes: vec![
                crate::panel::ChangeRow {
                    status: "MM".into(),
                    ..change("modified", crate::panel::Stage::Unstaged)
                },
                crate::panel::ChangeRow {
                    status: "AM".into(),
                    ..change("added", crate::panel::Stage::Unstaged)
                },
            ],
            ..Default::default()
        };
        assert_eq!(
            commit_precheck(&partially_staged),
            None,
            "partially staged MM/AM rows are allowed"
        );
        partially_staged.changes.clear();
        assert_eq!(
            commit_precheck(&partially_staged),
            Some("nothing staged: space stages the file, a stages all"),
            "empty changes list"
        );
        let merge_data = crate::panel::PanelData {
            merge: Some(crate::panel::MergeBanner {
                label: "MERGING".into(),
                onto: "main".into(),
                unresolved: 0,
                total: None,
            }),
            ..Default::default()
        };
        assert_eq!(
            commit_precheck(&merge_data),
            None,
            "merge with an empty resolved index diff"
        );
    }

    #[test]
    fn filter_paste_requires_visible_panel_focus_and_editing() {
        let data = crate::panel::PanelData::default();
        let mut ui = ui_at(GitView::Files, PanelWidth::Full);
        ui.git.filter = Some(ListFilter {
            view: GitView::Files,
            query: "old".into(),
            editing: true,
            map: Vec::new(),
        });
        ui.git.cur.set(GitView::Files, 4);
        ui.cursor = 4;
        ui.git.sel_anchor = Some(2);
        assert!(!paste_filter(&mut ui, false, true, "paste", &data));
        assert!(!paste_filter(&mut ui, true, false, "paste", &data));
        assert_eq!(ui.git.filter.as_ref().unwrap().query, "old");
        assert!(paste_filter(&mut ui, true, true, " paste", &data));
        assert_eq!(ui.git.filter.as_ref().unwrap().query, "old paste");
        assert_eq!(ui.cursor, 0);
        assert_eq!(ui.git.cur.get(GitView::Files), 0);
        assert_eq!(ui.git.sel_anchor, None);
        assert!(paste_filter(&mut ui, true, true, "a\nb\x1b", &data));
        assert_eq!(ui.git.filter.as_ref().unwrap().query, "old pasteab");
    }

    #[test]
    fn key_tables_have_unique_chords_per_context() {
        for view in [
            GitView::Files,
            GitView::Branches,
            GitView::Commits,
            GitView::Stash,
            GitView::Staging,
            GitView::CommitFiles,
            GitView::PatchBuilding,
            GitView::RebaseTodo,
            GitView::Blame,
        ] {
            let mut seen = std::collections::HashSet::new();
            for ck in context_keys(view) {
                assert!(
                    seen.insert(ck.chord),
                    "duplicate chord {:?} in {:?}",
                    ck.chord,
                    view
                );
                assert!(!ck.label.is_empty());
            }
        }
    }

    #[test]
    fn space_means_different_things_per_context() {
        let none = Modifiers::NONE;
        let full = PanelWidth::Full;
        assert_eq!(
            git_key(&KeyCode::Char(' '), none, &ui_at(GitView::Files, full)),
            Some(GitMsg::StageToggleFile)
        );
        assert_eq!(
            git_key(&KeyCode::Char(' '), none, &ui_at(GitView::Staging, full)),
            Some(GitMsg::StageLines)
        );
        assert_eq!(
            git_key(&KeyCode::Char(' '), none, &ui_at(GitView::Branches, full)),
            Some(GitMsg::CheckoutSel)
        );
        assert_eq!(
            git_key(&KeyCode::Char(' '), none, &ui_at(GitView::Stash, full)),
            Some(GitMsg::StashApply)
        );
    }

    #[test]
    fn changes_semantic_rows_release_enter_to_the_accordion() {
        use crate::chrome::FrameModel;
        use crate::panel::{ChangeRow, PanelData, Stage};
        let none = Modifiers::NONE;
        let change = |name: &str| ChangeRow {
            status: "M".into(),
            stage: Stage::Unstaged,
            dir: String::new(),
            name: name.into(),
            path: name.into(),
            added: 1,
            deleted: 0,
            count: thegn_svc::git::DiffCount::Exact(1),
            submodule: None,
            incoming: false,
        };
        // Footer/entity rows exist only when the semantic summary is
        // populated — the guard keys off that, so the fixture carries one.
        // (The old entities-free fixture accidentally asserted the CLEAN-TREE
        // bug: with `entities: None` and an empty list, `0 >= 0` swallowed
        // every git key.)
        let entities = thegn_core::semantic::EntitySummary::new(vec![(
            "a.rs".into(),
            vec![thegn_core::semantic::EntityChange {
                kind: thegn_core::semantic::EntityKind::Function,
                name: "f".into(),
                added: 1,
                deleted: 0,
                touch: thegn_core::semantic::Touch::Modified,
                start_line: 1,
            }],
        )]);
        let model = FrameModel {
            panel: PanelData {
                changes: vec![change("a.rs"), change("b.rs")],
                entities: Some(entities),
                ..Default::default()
            },
            ..Default::default()
        };
        let mut ui = ui_at(GitView::Files, PanelWidth::Half);
        ui.open = Section::Changes;
        // On a file row (cursor < 2) Enter still drills into staging.
        ui.cursor = 1;
        assert_eq!(
            git_key_chg(&KeyCode::Enter, none, &ui, &model),
            Some(GitMsg::Drill)
        );
        // On the semantic footer (cursor == 2) and entity rows (> 2) the git
        // claim is released so the accordion `Select` toggles the breakdown /
        // drills the entity, matching a mouse click.
        ui.cursor = 2;
        assert_eq!(git_key_chg(&KeyCode::Enter, none, &ui, &model), None);
        assert_eq!(git_key_chg(&KeyCode::Char(' '), none, &ui, &model), None);
        ui.cursor = 3;
        assert_eq!(git_key_chg(&KeyCode::Enter, none, &ui, &model), None);
        // A CLEAN tree (no changes, no entities) releases nothing: `?`, `c`,
        // `s`, `z` must keep working there.
        let clean = FrameModel::default();
        let mut cui = ui_at(GitView::Files, PanelWidth::Half);
        cui.open = Section::Changes;
        cui.cursor = 0;
        assert_eq!(
            git_key_chg(&KeyCode::Char('?'), none, &cui, &clean),
            Some(GitMsg::Cheatsheet)
        );
        // A non-Changes git list is unaffected: the wrapper defers to `git_key`.
        let commits = ui_at(GitView::Commits, PanelWidth::Half);
        assert_eq!(
            git_key_chg(&KeyCode::Enter, none, &commits, &model),
            git_key(&KeyCode::Enter, none, &commits)
        );
    }

    #[test]
    fn commit_files_claims_cursor_keys_and_open() {
        let none = Modifiers::NONE;
        let ui = ui_at(GitView::CommitFiles, PanelWidth::Half);
        // j/k drive the drill-list cursor rather than falling through to the
        // accordion (which would move the section cursor, not the file list).
        assert_eq!(
            git_key(&KeyCode::Char('j'), none, &ui),
            Some(GitMsg::CursorDown)
        );
        assert_eq!(
            git_key(&KeyCode::Char('k'), none, &ui),
            Some(GitMsg::CursorUp)
        );
        // `o` opens the highlighted file in the editor.
        assert_eq!(
            git_key(&KeyCode::Char('o'), none, &ui),
            Some(GitMsg::OpenFile)
        );
    }

    #[test]
    fn step_view_cursor_walks_and_clamps_commit_files() {
        let mut git = GitUi {
            focus: GitView::CommitFiles,
            commit_files: vec![("a".into(), 1, 0), ("b".into(), 0, 2), ("c".into(), 3, 3)],
            ..Default::default()
        };
        assert_eq!(git.cur.commit_files, 0);
        git.step_view_cursor(true);
        assert_eq!(git.cur.commit_files, 1);
        git.step_view_cursor(true);
        git.step_view_cursor(true); // clamps at len - 1
        assert_eq!(git.cur.commit_files, 2);
        git.step_view_cursor(false);
        assert_eq!(git.cur.commit_files, 1);
        git.step_view_cursor(false);
        git.step_view_cursor(false); // clamps at 0
        assert_eq!(git.cur.commit_files, 0);
    }

    #[test]
    fn click_drill_row_clamps_list_and_diff_cursors() {
        let mut git = GitUi {
            focus: GitView::CommitFiles,
            commit_files: vec![("a.rs".into(), 1, 0)],
            ..Default::default()
        };
        git.click_drill_row(99);
        assert_eq!(git.cur.commit_files, 0);

        git.focus = GitView::PatchBuilding;
        git.staging = Some(StagingUi::new("a.rs"));
        git.patch_doc = Some(StageDocState::default());
        git.click_drill_row(99);
        assert_eq!(git.staging.as_ref().map(|st| st.cursor), Some(0));
    }

    #[test]
    fn wheel_route_follows_the_column() {
        let mut git = GitUi {
            focus: GitView::Blame,
            ..Default::default()
        };
        assert_eq!(
            git.wheel_route(Some(PanelColumn::Main), false),
            GitWheel::DrillStep
        );
        assert_eq!(
            git.wheel_route(Some(PanelColumn::Side), false),
            GitWheel::Ignore
        );
        git.focus = GitView::Commits;
        assert_eq!(
            git.wheel_route(Some(PanelColumn::Main), true),
            GitWheel::DetailScroll
        );
        assert_eq!(
            git.wheel_route(Some(PanelColumn::Main), false),
            GitWheel::Ignore
        );
        assert_eq!(
            git.wheel_route(Some(PanelColumn::Side), true),
            GitWheel::Legacy
        );
        assert_eq!(git.wheel_route(None, true), GitWheel::Legacy);
    }

    #[test]
    fn drag_drill_to_sets_staging_range() {
        let path = "src/a.rs";
        let diff = concat!(
            "diff --git a/src/a.rs b/src/a.rs\n",
            "index 1111111..2222222 100644\n",
            "--- a/src/a.rs\n",
            "+++ b/src/a.rs\n",
            "@@ -1,1 +1,4 @@\n old\n+one\n+two\n+three\n",
        );
        let doc = staging::build(path, diff);
        let mut git = GitUi {
            focus: GitView::Staging,
            staging: Some(StagingUi::new(path)),
            stage_doc: Some(StageDocState {
                path: path.into(),
                pane: StagePane::Unstaged,
                doc,
                diff: diff.into(),
            }),
            ..Default::default()
        };
        git.drag_drill_to(1, 4);
        assert_eq!(git.staging.as_ref().and_then(|st| st.anchor), Some(1));
        assert_eq!(git.staging.as_ref().map(|st| st.cursor), Some(4));
        git.drag_drill_to(3, 3);
        assert_eq!(git.staging.as_ref().and_then(|st| st.anchor), None);

        git.focus = GitView::CommitFiles;
        git.commit_files = vec![("a.rs".into(), 1, 0), ("b.rs".into(), 0, 1)];
        git.drag_drill_to(0, 1);
        assert_eq!(git.cur.commit_files, 1);
        assert_eq!(git.staging.as_ref().and_then(|st| st.anchor), None);
    }

    #[test]
    fn shifted_letters_are_distinct_from_plain() {
        let shift = Modifiers::SHIFT;
        let none = Modifiers::NONE;
        let ui = ui_at(GitView::Commits, PanelWidth::Full);
        // C copies, c falls through (commit lives in Files/Staging).
        assert_eq!(
            git_key(&KeyCode::Char('C'), shift, &ui),
            Some(GitMsg::CopyCommits)
        );
        assert_eq!(git_key(&KeyCode::Char('c'), none, &ui), None);
        // s squashes; S falls through.
        assert_eq!(
            git_key(&KeyCode::Char('s'), none, &ui),
            Some(GitMsg::Squash)
        );
        assert_eq!(git_key(&KeyCode::Char('S'), shift, &ui), None);
    }

    #[test]
    fn width_gating_blocks_effects_not_navigation() {
        let none = Modifiers::NONE;
        // Normal: blocked effects carry the ORIGINAL message + the width they
        // need (the dispatcher widens and performs — DWIM), while navigation
        // and the width-independent effects pass through untouched.
        let normal = ui_at(GitView::Commits, PanelWidth::Normal);
        assert!(matches!(
            git_key(&KeyCode::Char('s'), none, &normal),
            Some(GitMsg::WidthBlocked(inner, PanelWidth::Half)) if *inner == GitMsg::Squash
        ));
        // A Full-only action at Normal asks straight for Full.
        assert!(matches!(
            git_key(&KeyCode::Char('i'), none, &normal),
            Some(GitMsg::WidthBlocked(inner, PanelWidth::Full))
                if *inner == GitMsg::EnterInteractive
        ));
        assert_eq!(git_key(&KeyCode::Enter, none, &normal), Some(GitMsg::Drill));
        let files_normal = ui_at(GitView::Files, PanelWidth::Normal);
        assert_eq!(
            git_key(&KeyCode::Char(' '), none, &files_normal),
            Some(GitMsg::StageToggleFile),
            "staging is width-independent"
        );
        assert_eq!(
            git_key(&KeyCode::Char('c'), none, &files_normal),
            Some(GitMsg::Commit),
            "the commit modal is width-independent"
        );
        // Half: core effects allowed, Full-only blocked (carrying Full).
        let half = ui_at(GitView::Commits, PanelWidth::Half);
        assert_eq!(
            git_key(&KeyCode::Char('s'), none, &half),
            Some(GitMsg::Squash)
        );
        assert!(matches!(
            git_key(&KeyCode::Char('i'), none, &half),
            Some(GitMsg::WidthBlocked(inner, PanelWidth::Full))
                if *inner == GitMsg::EnterInteractive
        ));
        assert!(matches!(
            git_key(&KeyCode::Char('B'), Modifiers::SHIFT, &half),
            Some(GitMsg::WidthBlocked(inner, PanelWidth::Full)) if *inner == GitMsg::MarkBase
        ));
        // Full: everything.
        let full = ui_at(GitView::Commits, PanelWidth::Full);
        assert_eq!(
            git_key(&KeyCode::Char('i'), none, &full),
            Some(GitMsg::EnterInteractive)
        );
    }

    #[test]
    fn unsynced_paused_rebase_locks_todo_edits_but_not_navigation() {
        let mut ui = ui_at(GitView::RebaseTodo, PanelWidth::Full);
        ui.git.flow = GitFlow::Rebase(RebaseUi {
            running: true,
            todos_synced: false,
            ..Default::default()
        });
        // Edits are dropped while the live read is out…
        assert_eq!(git_key(&KeyCode::Char('d'), Modifiers::NONE, &ui), None);
        assert_eq!(git_key(&KeyCode::Enter, Modifiers::NONE, &ui), None);
        // …navigation stays live…
        assert_eq!(
            git_key(&KeyCode::Char('j'), Modifiers::NONE, &ui),
            Some(GitMsg::CursorDown)
        );
        // …and everything unlocks once the sync lands.
        if let GitFlow::Rebase(r) = &mut ui.git.flow {
            r.todos_synced = true;
        }
        assert_eq!(
            git_key(&KeyCode::Char('d'), Modifiers::NONE, &ui),
            Some(GitMsg::TodoSetAction(TodoAction::Drop))
        );
        assert_eq!(
            git_key(&KeyCode::Enter, Modifiers::NONE, &ui),
            Some(GitMsg::ConfirmRebase)
        );
        // A NOT-running editor (plan being built before confirm) is never
        // locked by the sync gate.
        ui.git.flow = GitFlow::Rebase(RebaseUi::default());
        assert_eq!(
            git_key(&KeyCode::Char('d'), Modifiers::NONE, &ui),
            Some(GitMsg::TodoSetAction(TodoAction::Drop))
        );
    }

    #[test]
    fn pending_op_locks_effects() {
        let none = Modifiers::NONE;
        let mut ui = ui_at(GitView::Commits, PanelWidth::Full);
        ui.git.pending = Some(PendingOp {
            label: "rebasing".into(),
        });
        assert_eq!(git_key(&KeyCode::Char('s'), none, &ui), None);
        assert_eq!(git_key(&KeyCode::Enter, none, &ui), Some(GitMsg::Drill));
    }

    #[test]
    fn ctrl_carve_out_is_scoped() {
        let mut ui = ui_at(GitView::Commits, PanelWidth::Full);
        assert!(git_claims_ctrl(&ui, &KeyCode::Char('j')));
        assert!(git_claims_ctrl(&ui, &KeyCode::Char('k')));
        assert!(!git_claims_ctrl(&ui, &KeyCode::Char('p')));
        ui.git.focus = GitView::PatchBuilding;
        assert!(git_claims_ctrl(&ui, &KeyCode::Char('p')));
        assert!(!git_claims_ctrl(&ui, &KeyCode::Char('j')));
        ui.git.focus = GitView::Files;
        assert!(!git_claims_ctrl(&ui, &KeyCode::Char('j')));
        // Outside row mode nothing is claimed.
        let mut section_mode = ui_at(GitView::Commits, PanelWidth::Full);
        section_mode.row_mode = false;
        assert!(!git_claims_ctrl(&section_mode, &KeyCode::Char('j')));
        // Ctrl+j resolves to MoveDown at Full.
        let ui = ui_at(GitView::Commits, PanelWidth::Full);
        assert_eq!(
            git_key(&KeyCode::Char('j'), Modifiers::CTRL, &ui),
            Some(GitMsg::MoveDown)
        );
    }

    #[test]
    fn esc_peels_only_when_there_is_a_layer() {
        let none = Modifiers::NONE;
        // Plain list, nothing to peel → None (accordion's LeaveRows takes it).
        let ui = ui_at(GitView::Commits, PanelWidth::Full);
        assert_eq!(git_key(&KeyCode::Escape, none, &ui), None);
        // An anchor is peelable.
        let mut ui = ui_at(GitView::Commits, PanelWidth::Full);
        ui.git.sel_anchor = Some(2);
        assert_eq!(git_key(&KeyCode::Escape, none, &ui), Some(GitMsg::Back));
        // A drilled view is peelable.
        let ui = ui_at(GitView::Staging, PanelWidth::Full);
        assert_eq!(git_key(&KeyCode::Escape, none, &ui), Some(GitMsg::Back));
        // A filter is peelable.
        let mut ui = ui_at(GitView::Files, PanelWidth::Full);
        ui.git.filter = Some(ListFilter {
            view: GitView::Files,
            query: "x".into(),
            editing: false,
            map: vec![0],
        });
        assert_eq!(git_key(&KeyCode::Escape, none, &ui), Some(GitMsg::Back));
    }

    #[test]
    fn line_views_claim_plain_cursor_keys_lists_do_not() {
        let none = Modifiers::NONE;
        let staging = ui_at(GitView::Staging, PanelWidth::Full);
        assert_eq!(
            git_key(&KeyCode::Char('j'), none, &staging),
            Some(GitMsg::CursorDown)
        );
        // Lists leave j/k to the accordion's flow navigation.
        let commits = ui_at(GitView::Commits, PanelWidth::Full);
        assert_eq!(git_key(&KeyCode::Char('j'), none, &commits), None);
    }

    #[test]
    fn shift_arrows_are_reserved_for_accordion_section_jumps() {
        for view in [
            GitView::Files,
            GitView::Branches,
            GitView::Commits,
            GitView::Stash,
            GitView::Staging,
            GitView::CommitFiles,
            GitView::PatchBuilding,
            GitView::RebaseTodo,
            GitView::Blame,
        ] {
            let ui = ui_at(view, PanelWidth::Full);
            assert_eq!(
                git_key(&KeyCode::DownArrow, Modifiers::SHIFT, &ui),
                None,
                "{view:?} should let Shift+Down fall through to accordion_key"
            );
            assert_eq!(
                git_key(&KeyCode::UpArrow, Modifiers::SHIFT, &ui),
                None,
                "{view:?} should let Shift+Up fall through to accordion_key"
            );
        }
    }

    #[test]
    fn selection_ranges_are_anchor_aware() {
        let mut s = StagingUi::new("a.rs");
        s.cursor = 5;
        assert_eq!(s.selection(), 5..=5);
        s.anchor = Some(2);
        assert_eq!(s.selection(), 2..=5);
        s.cursor = 1;
        assert_eq!(s.selection(), 1..=2);

        let mut g = GitUi {
            focus: GitView::Commits,
            ..Default::default()
        };
        g.cur.set(GitView::Commits, 4);
        g.sel_anchor = Some(7);
        assert_eq!(g.selection(), 4..=7);
    }

    #[test]
    fn non_git_sections_never_match() {
        let ui = PanelUi {
            open: Section::Tests,
            row_mode: true,
            width: PanelWidth::Full,
            ..Default::default()
        };
        assert_eq!(git_key(&KeyCode::Char(' '), Modifiers::NONE, &ui), None);
    }

    fn commits(n: usize) -> Vec<crate::panel::CommitRow> {
        (0..n)
            .map(|i| crate::panel::CommitRow {
                sha: format!("{i:0>40x}"),
                short: format!("{i:0>7x}"),
                subject: format!("commit {i}"),
                author: "Ada".into(),
                date: 0,
                refs: String::new(),
                parents: vec!["p".into()],
            })
            .collect()
    }

    fn data_with_commits(n: usize) -> crate::panel::PanelData {
        crate::panel::PanelData {
            commits: commits(n),
            ..Default::default()
        }
    }

    #[test]
    fn display_map_is_identity_without_filter_and_filters_with_one() {
        let data = data_with_commits(3);
        let mut ui = GitUi {
            focus: GitView::Commits,
            ..Default::default()
        };
        assert_eq!(display_map(&ui, GitView::Commits, &data), vec![0, 1, 2]);
        ui.filter = Some(ListFilter {
            view: GitView::Commits,
            query: "commit 1".into(),
            editing: false,
            map: Vec::new(),
        });
        let map = display_map(&ui, GitView::Commits, &data);
        assert_eq!(map.first(), Some(&1));
        // A filter on another view leaves this one identity.
        ui.filter.as_mut().unwrap().view = GitView::Branches;
        assert_eq!(display_map(&ui, GitView::Commits, &data), vec![0, 1, 2]);
    }

    #[test]
    fn commit_selection_targets_in_display_order_oldest_by_source() {
        let data = data_with_commits(5);
        let mut ui = GitUi {
            focus: GitView::Commits,
            ..Default::default()
        };
        ui.cur.set(GitView::Commits, 1);
        ui.sel_anchor = Some(3);
        let (oldest, targets) = commit_selection(&ui, &data).unwrap();
        // Selection 1..=3; the log is newest-first so source 3 is oldest.
        assert_eq!(oldest, data.commits[3].sha);
        assert_eq!(
            targets,
            vec![
                data.commits[1].sha.clone(),
                data.commits[2].sha.clone(),
                data.commits[3].sha.clone(),
            ]
        );
        // No commits → None.
        assert!(commit_selection(&ui, &crate::panel::PanelData::default()).is_none());
    }

    #[test]
    fn sel_pairs_and_step_cursor_walk_the_doc() {
        let doc = staging::build(
            "a.rs",
            "diff --git a/a.rs b/a.rs\n--- a/a.rs\n+++ b/a.rs\n@@ -1,2 +1,2 @@\n ctx\n-old\n+new\n",
        );
        // Flattened: 0 header, 1 ctx, 2 del, 3 add.
        assert_eq!(sel_pairs(&doc, 0..=3), vec![(0, 1), (0, 2)]);
        assert_eq!(sel_pairs(&doc, 1..=1), Vec::<(usize, usize)>::new());
        // Stepping skips the header and clamps at the ends.
        assert_eq!(step_cursor(&doc, 1, true), 2);
        assert_eq!(step_cursor(&doc, 3, true), 3);
        assert_eq!(step_cursor(&doc, 2, false), 1);
        assert_eq!(step_cursor(&doc, 1, false), 1);
    }

    #[test]
    fn todo_reducers_retag_toggle_and_move() {
        let mut todos: Vec<TodoEntry> = commits(3)
            .into_iter()
            .rev()
            .map(|commit| TodoEntry {
                action: TodoAction::Pick,
                sha: commit.short,
                subject: commit.subject,
            })
            .collect();
        assert_eq!(todos.len(), 3);
        assert_eq!(todos[0].subject, "commit 2");
        assert!(todos.iter().all(|t| t.action == TodoAction::Pick));

        todo_retag_at(&mut todos, 1, TodoAction::Squash);
        assert_eq!(todos[1].action, TodoAction::Squash);
        todo_toggle_at(&mut todos, 1);
        assert_eq!(todos[1].action, TodoAction::Drop);
        todo_toggle_at(&mut todos, 1);
        assert_eq!(todos[1].action, TodoAction::Pick);

        let sha = todos[1].sha.clone();
        let at = todo_move(&mut todos, 1, true);
        assert_eq!(at, 0);
        assert_eq!(todos[0].sha, sha);
        // Refused moves keep the cursor (already at the top).
        assert_eq!(todo_move(&mut todos, 0, true), 0);
    }

    #[test]
    fn sync_rebase_flow_adopts_and_clears_external_rebases() {
        let mut git = GitUi::default();
        // An external rebase appears: flow adopts it.
        assert_eq!(sync_rebase_flow(&mut git, Some(("REBASING", 2))), None);
        match &git.flow {
            GitFlow::Rebase(r) => assert!(r.running && r.conflict),
            other => panic!("{other:?}"),
        }
        // Conflicts resolved: flag clears, flow stays.
        sync_rebase_flow(&mut git, Some(("REBASING", 0)));
        assert!(matches!(&git.flow, GitFlow::Rebase(r) if r.running && !r.conflict));
        // Banner gone: the running flow resets with a status note.
        git.focus = GitView::RebaseTodo;
        assert_eq!(sync_rebase_flow(&mut git, None), Some("rebase finished"));
        assert_eq!(git.flow, GitFlow::None);
        assert_eq!(git.focus, GitView::Commits);
        // The NOT-running todo editor is never cleared by hydration.
        git.flow = GitFlow::Rebase(RebaseUi::default());
        assert_eq!(sync_rebase_flow(&mut git, None), None);
        assert!(matches!(git.flow, GitFlow::Rebase(_)));
        // A merge banner doesn't fake a rebase.
        git.flow = GitFlow::None;
        sync_rebase_flow(&mut git, Some(("MERGING", 1)));
        assert_eq!(git.flow, GitFlow::None);
    }

    #[test]
    fn sync_sequencer_flow_mirrors_merge_cherry_revert_state() {
        let mut git = GitUi::default();

        // MERGING banner creates a Merge flow and captures total.
        sync_sequencer_flow(&mut git, Some("MERGING"), "main", 3);
        assert!(matches!(&git.flow, GitFlow::Merge(s) if s.conflict && s.onto == "main"));
        assert_eq!(git.merge_conflict_total, Some(3));

        // Subsequent calls update conflict flag but do not reset total.
        sync_sequencer_flow(&mut git, Some("MERGING"), "main", 0);
        assert!(matches!(&git.flow, GitFlow::Merge(s) if !s.conflict));
        assert_eq!(git.merge_conflict_total, Some(3), "total must not be reset");

        // None clears a finished merge.
        sync_sequencer_flow(&mut git, None, "", 0);
        assert_eq!(git.flow, GitFlow::None);
        assert_eq!(git.merge_conflict_total, None);

        // CHERRY-PICK banner.
        sync_sequencer_flow(&mut git, Some("CHERRY-PICK"), "abc1234", 1);
        assert!(matches!(&git.flow, GitFlow::CherryPick(s) if s.conflict));
        assert_eq!(git.merge_conflict_total, Some(1));

        // REVERTING banner.
        git.flow = GitFlow::None;
        git.merge_conflict_total = None;
        sync_sequencer_flow(&mut git, Some("REVERTING"), "abc1234", 2);
        assert!(matches!(&git.flow, GitFlow::Revert(s) if s.conflict && s.onto == "abc1234"));

        // A DIFFERENT banner kind while a sequencer flow is up means the kind
        // changed underneath us (`git merge --abort && git rebase …` never
        // passes through None) — the stale flow is cleared so the rebase sync
        // can install its own. The old same-kind-only sync wedged the Merge
        // flow (and its conflict denominator) forever.
        sync_sequencer_flow(&mut git, Some("REBASING"), "", 0);
        assert_eq!(git.flow, GitFlow::None);
        assert_eq!(git.merge_conflict_total, None);

        // A running REBASE flow is never stomped by a sequencer banner —
        // that hand-off belongs to `sync_rebase_flow`.
        git.flow = GitFlow::Rebase(RebaseUi {
            running: true,
            ..Default::default()
        });
        sync_sequencer_flow(&mut git, Some("MERGING"), "main", 1);
        assert!(matches!(&git.flow, GitFlow::Rebase(r) if r.running));
    }

    #[test]
    fn flow_hint_returns_correct_label_per_sequencer_state() {
        let none = flow_hint(&GitFlow::None);
        assert!(none.is_none());

        let merge_conflict = GitFlow::Merge(SequencerUi {
            onto: "main".into(),
            conflict: true,
        });
        let (chord, label) = flow_hint(&merge_conflict).unwrap();
        assert_eq!(chord, "m");
        assert!(
            label.contains("continue"),
            "expected continue/abort hint, got {label}"
        );

        let cherry_no_conflict = GitFlow::CherryPick(SequencerUi {
            onto: "abc".into(),
            conflict: false,
        });
        let (_, label2) = flow_hint(&cherry_no_conflict).unwrap();
        assert!(
            label2.contains("cherry"),
            "expected cherry label, got {label2}"
        );
    }

    #[test]
    fn files_context_has_m_for_flow_menu() {
        let ui = ui_at(GitView::Files, PanelWidth::Full);
        assert_eq!(
            git_key(&KeyCode::Char('m'), Modifiers::NONE, &ui),
            Some(GitMsg::OpenMenu(MenuKind::Rebase)),
            "m in Files must open the flow/rebase menu"
        );
    }

    #[test]
    fn cheatsheet_and_filter_reach_every_list_context() {
        for view in [
            GitView::Files,
            GitView::Branches,
            GitView::Commits,
            GitView::Stash,
        ] {
            let ui = ui_at(view, PanelWidth::Half);
            assert_eq!(
                git_key(&KeyCode::Char('?'), Modifiers::SHIFT, &ui),
                Some(GitMsg::Cheatsheet),
                "{view:?}"
            );
            // `/` filters the label-backed lists. Files has no filter labels
            // (`list_labels` has no arm for it), so advertising the key there
            // was an audited no-op and it was removed from that table.
            let want = if view == GitView::Files {
                None
            } else {
                Some(GitMsg::FilterStart)
            };
            assert_eq!(
                git_key(&KeyCode::Char('/'), Modifiers::NONE, &ui),
                want,
                "{view:?}"
            );
        }
    }

    #[test]
    fn worktree_reset_lands_on_the_open_sections_home() {
        let mut git = GitUi::default();
        git.reset_for_worktree(Some(GitView::Commits));
        assert_eq!(git.focus, GitView::Commits);
        let ui = PanelUi {
            open: Section::Commits,
            row_mode: true,
            git,
            ..Default::default()
        };
        assert_ne!(
            git_key(&KeyCode::Char(' '), Modifiers::NONE, &ui),
            Some(GitMsg::StageToggleFile)
        );
    }

    #[test]
    fn normalize_peels_drills_at_normal() {
        let mut git = GitUi {
            focus: GitView::CommitFiles,
            drilled_commit: Some("abc123".into()),
            ..GitUi::default()
        };
        assert!(git.normalize(Some(GitView::Commits), PanelWidth::Normal));
        assert_eq!(git.focus, GitView::Commits);
        assert_eq!(git.drilled_commit, None);
        let ui = PanelUi {
            open: Section::Commits,
            row_mode: true,
            git,
            ..Default::default()
        };
        assert!(
            context_keys(ui.git.focus)
                .iter()
                .any(|entry| entry.chord == "enter" && entry.label == "diff")
        );

        let mut git = GitUi {
            focus: GitView::CommitFiles,
            ..GitUi::default()
        };
        assert!(!git.normalize(Some(GitView::Commits), PanelWidth::Half));
        assert_eq!(git.focus, GitView::CommitFiles);
    }

    #[test]
    fn enter_enter_never_creates_a_patch_flow() {
        let mut git = GitUi {
            focus: GitView::CommitFiles,
            drilled_commit: Some("abc123".into()),
            commit_files: vec![("src/lib.rs".into(), 2, 1)],
            ..Default::default()
        };
        git.cur.commit_files = 0;
        git.open_commit_file("abc123", "src/lib.rs").unwrap();
        assert_eq!(git.focus, GitView::PatchBuilding);
        assert_eq!(git.flow, GitFlow::None);
    }

    #[test]
    fn first_mark_creates_the_patch_flow() {
        let mut git = GitUi {
            focus: GitView::PatchBuilding,
            drilled_commit: Some("abc123".into()),
            ..Default::default()
        };
        git.mark_patch_lines("src/lib.rs", &[2, 4]);
        assert!(matches!(&git.flow, GitFlow::Patch(p) if p.commit == "abc123" && p.marked() == 2));
    }

    #[test]
    fn stale_commit_files_result_is_dropped() {
        let mut git = GitUi {
            drilled_commit: Some("B".into()),
            commit_files: vec![("keep.rs".into(), 1, 0)],
            ..Default::default()
        };
        assert!(!git.accept_commit_files("A", Ok(vec![("stale.rs".into(), 9, 9)])));
        assert_eq!(git.commit_files[0].0, "keep.rs");
    }

    #[test]
    fn stale_patch_doc_is_dropped() {
        let mut git = GitUi {
            drilled_commit: Some("B".into()),
            ..Default::default()
        };
        let state = StageDocState {
            path: "stale.rs".into(),
            ..Default::default()
        };
        assert!(!git.accept_patch_doc("A", "new.rs", Ok(state)));
        assert!(git.patch_docs.is_empty());
    }

    #[test]
    fn patch_failure_is_retained_for_its_commit_and_path() {
        let mut git = GitUi {
            drilled_commit: Some("A".into()),
            ..Default::default()
        };
        assert!(git.accept_patch_doc("A", "a.rs", Err("git show failed".into())));
        assert_eq!(
            git.patch_failed
                .get(&("A".into(), "a.rs".into()))
                .map(String::as_str),
            Some("git show failed")
        );
        assert!(!git.patch_failed.contains_key(&("B".into(), "a.rs".into())));
    }

    #[test]
    fn stale_blame_is_dropped() {
        let mut git = GitUi {
            blame_path: Some("current.rs".into()),
            ..Default::default()
        };
        assert!(!git.accept_blame("old.rs", Ok(Vec::new())));
        assert!(git.blame_rows.is_empty());
    }

    #[test]
    fn commit_show_keyed_by_mark_and_sha() {
        let mut git = GitUi {
            flow: GitFlow::Diffing("base".into()),
            commit_show_key: Some(CommitDetailKey {
                base: Some("base".into()),
                sha: "selected".into(),
            }),
            ..Default::default()
        };
        let stale = CommitDetailKey {
            base: None,
            sha: "selected".into(),
        };
        assert!(!git.accept_commit_show(stale, Ok("stale".into())));
        let current = git.commit_show_key.clone().unwrap();
        assert!(git.accept_commit_show(current, Ok("range".into())));
        assert!(
            matches!(git.commit_show, Some((CommitDetailKey { base: Some(base), sha }, Ok(text))) if base == "base" && sha == "selected" && text == "range")
        );
    }

    #[test]
    fn branches_and_stash_enter_dispatches_drill_at_normal() {
        for view in [GitView::Branches, GitView::Stash] {
            let ui = ui_at(view, PanelWidth::Normal);
            assert_eq!(
                git_key(&KeyCode::Enter, Modifiers::NONE, &ui),
                Some(GitMsg::Drill)
            );
        }
    }

    #[test]
    fn back_returns_to_the_launching_view() {
        let mut git = GitUi {
            focus: GitView::Files,
            ..GitUi::default()
        };
        git.drill(GitView::Blame);
        git.drill(GitView::CommitFiles);
        assert!(git.back(GitView::Files));
        assert_eq!(git.focus, GitView::Blame);
        assert!(git.back(GitView::Files));
        assert_eq!(git.focus, GitView::Files);
    }

    #[test]
    fn normalize_fixes_a_foreign_list_focus() {
        let mut git = GitUi {
            focus: GitView::Commits,
            ..GitUi::default()
        };
        assert!(git.normalize(Some(GitView::Files), PanelWidth::Half));
        assert_eq!(git.focus, GitView::Files);
    }

    #[test]
    fn patch_commit_conflicts_report_marked_lines() {
        let mut git = GitUi {
            flow: GitFlow::Patch(PatchUi {
                commit: "first".into(),
                marks: std::collections::HashMap::from([(
                    "file.rs".into(),
                    std::collections::BTreeSet::from([2, 4]),
                )]),
                ..PatchUi::default()
            }),
            ..GitUi::default()
        };
        assert_eq!(git.patch_conflicts_with("second"), Some(2));
        assert_eq!(git.patch_conflicts_with("first"), None);
        if let GitFlow::Patch(patch) = &mut git.flow {
            patch.marks.clear();
        }
        assert_eq!(git.patch_conflicts_with("second"), Some(0));
    }

    #[test]
    fn back_from_commit_files_ends_only_an_unmarked_patch() {
        let mut git = GitUi {
            focus: GitView::CommitFiles,
            flow: GitFlow::Patch(PatchUi {
                commit: "abc".into(),
                ..PatchUi::default()
            }),
            ..GitUi::default()
        };
        assert!(git.back(GitView::Commits));
        assert_eq!(git.flow, GitFlow::None);

        let mut git = GitUi {
            focus: GitView::CommitFiles,
            flow: GitFlow::Patch(PatchUi {
                commit: "abc".into(),
                marks: std::collections::HashMap::from([(
                    "file.rs".into(),
                    std::collections::BTreeSet::from([1]),
                )]),
                ..PatchUi::default()
            }),
            ..GitUi::default()
        };
        assert!(git.back(GitView::Commits));
        assert!(matches!(git.flow, GitFlow::Patch(_)));
    }

    #[test]
    fn external_rebase_replaces_an_unmarked_patch_flow() {
        let mut git = GitUi {
            flow: GitFlow::Patch(PatchUi::default()),
            ..GitUi::default()
        };
        sync_rebase_flow(&mut git, Some(("REBASING", 0)));
        assert!(matches!(git.flow, GitFlow::Rebase(_)));

        let mut git = GitUi {
            flow: GitFlow::Patch(PatchUi::default()),
            ..GitUi::default()
        };
        sync_sequencer_flow(&mut git, Some("CHERRY-PICK"), "abc", 0);
        assert!(matches!(git.flow, GitFlow::CherryPick(_)));
    }

    #[test]
    fn stale_staging_in_a_list_view_does_not_swallow_esc() {
        let mut ui = ui_at(GitView::Commits, PanelWidth::Half);
        ui.git.staging = Some(StagingUi::new("stale.rs"));
        assert_eq!(git_key(&KeyCode::Escape, Modifiers::NONE, &ui), None);
    }

    #[test]
    fn retract_restores_width_after_last_drill() {
        let mut git = GitUi {
            focus: GitView::Staging,
            widened_from: Some(PanelWidth::Normal),
            ..GitUi::default()
        };
        assert_eq!(git.retract_width(), None);
        assert!(git.back(GitView::Files));
        assert_eq!(git.retract_width(), Some(PanelWidth::Normal));
        assert_eq!(git.retract_width(), None);
    }

    #[test]
    fn normalize_preserves_the_focus_invariant_for_every_home_view() {
        let homes = [
            GitView::Files,
            GitView::Commits,
            GitView::Branches,
            GitView::Stash,
        ];
        let views = [
            GitView::Files,
            GitView::Branches,
            GitView::Commits,
            GitView::Stash,
            GitView::Staging,
            GitView::CommitFiles,
            GitView::PatchBuilding,
            GitView::RebaseTodo,
            GitView::Blame,
        ];
        for home in homes {
            for view in views {
                for width in [PanelWidth::Normal, PanelWidth::Half, PanelWidth::Full] {
                    let mut git = GitUi {
                        focus: view,
                        ..GitUi::default()
                    };
                    git.normalize(Some(home), width);
                    assert!(
                        git.focus == home || GitUi::is_drill(git.focus),
                        "home={home:?} view={view:?} width={width:?} focus={:?}",
                        git.focus
                    );
                    if width == PanelWidth::Normal {
                        assert_eq!(git.focus, home);
                    }
                }
            }
        }
    }

    #[test]
    fn head_only_refusal_names_head_and_fix() {
        let data = crate::panel::PanelData {
            branch: "main".into(),
            ..Default::default()
        };
        let row = crate::panel::BranchRow {
            name: "feature".into(),
            is_head: false,
            held_by: Vec::new(),
            upstream: None,
            ahead: 0,
            behind: 0,
            upstream_gone: false,
            sha: String::new(),
            date: 0,
            subject: String::new(),
            pr: None,
        };
        let message = head_only_refusal(&data, Some(&row), "push").unwrap();
        assert!(message.contains("HEAD (main)"), "{message}");
        assert!(message.contains("space"), "{message}");
        assert!(message.contains("checks feature out"), "{message}");
        assert_eq!(head_only_refusal(&data, None, "push"), None);
        assert_eq!(
            head_only_refusal(
                &data,
                Some(&crate::panel::BranchRow {
                    is_head: true,
                    ..row
                }),
                "pull"
            ),
            None
        );
    }

    #[test]
    fn branch_keys_name_head_operations_and_open_new_branch_input() {
        let keys = context_keys(GitView::Branches);
        assert!(
            keys.iter()
                .any(|key| key.chord == "n" && key.msg == GitMsg::NewBranch)
        );
        assert!(
            keys.iter()
                .any(|key| key.chord == "p" && key.label == "pull HEAD")
        );
        assert!(
            keys.iter()
                .any(|key| key.chord == "P" && key.label == "push HEAD")
        );
    }

    #[test]
    fn interactive_anchor_prefers_the_mark_over_the_selection() {
        let data = data_with_commits(5);
        let mut ui = GitUi {
            focus: GitView::Commits,
            mark_base: Some("marked-base".into()),
            ..Default::default()
        };
        ui.cur.set(GitView::Commits, 1);
        assert_eq!(
            interactive_anchor(&ui, &data),
            Some(PlanFrom::Mark("marked-base".into()))
        );
    }

    #[test]
    fn interactive_anchor_without_mark_keeps_the_oldest_selection_rule() {
        let mut data = data_with_commits(5);
        data.commits[3].parents = vec!["parent-sha".into()];
        let mut ui = GitUi {
            focus: GitView::Commits,
            ..Default::default()
        };
        ui.cur.set(GitView::Commits, 1);
        ui.sel_anchor = Some(3);
        assert_eq!(
            interactive_anchor(&ui, &data),
            Some(PlanFrom::Target {
                oldest: data.commits[3].sha.clone(),
                base: format!("{}^", data.commits[3].sha),
            })
        );
        data.commits[3].parents.clear();
        assert_eq!(
            interactive_anchor(&ui, &data),
            Some(PlanFrom::Target {
                oldest: data.commits[3].sha.clone(),
                base: "--root".into(),
            })
        );
    }

    #[test]
    fn drag_autoscroll_walks_cursorable_lines() {
        let path = "src/a.rs";
        let diff = concat!(
            "diff --git a/src/a.rs b/src/a.rs\n",
            "index 1111111..2222222 100644\n",
            "--- a/src/a.rs\n",
            "+++ b/src/a.rs\n",
            "@@ -1,2 +1,3 @@\n one\n+two\n three\n",
            "@@ -10,1 +11,2 @@\n ten\n+eleven\n",
        );
        let doc = staging::build(path, diff);
        let cursorable: Vec<_> = (0..doc.lines.len())
            .filter(|index| staging::cursorable(&doc, *index))
            .collect();
        let edge = *cursorable.get(3).expect("second hunk body lines exist");
        let previous = *cursorable.get(2).expect("first hunk has body lines");
        assert!(
            !staging::cursorable(&doc, edge - 1),
            "hunk header is skipped"
        );
        let git = GitUi {
            focus: GitView::Staging,
            staging: Some(StagingUi::new(path)),
            stage_doc: Some(StageDocState {
                path: path.into(),
                pane: StagePane::Unstaged,
                doc: doc.clone(),
                diff: diff.into(),
            }),
            ..Default::default()
        };
        assert_eq!(
            git.drag_autoscroll_target(edge, DragOverflow::Above(1)),
            previous
        );
        let far = git.drag_autoscroll_target(edge, DragOverflow::Above(10));
        assert_eq!(far, *cursorable.first().expect("first hunk has body lines"));
        assert!(edge - far <= 3);
        let last = *cursorable.last().expect("body lines exist");
        assert_eq!(
            git.drag_autoscroll_target(last, DragOverflow::Below(1)),
            last
        );
        assert_eq!(git.drag_autoscroll_target(edge, DragOverflow::Inside), edge);
    }

    #[test]
    fn drag_autoscroll_clamps_lists() {
        let mut git = GitUi {
            focus: GitView::CommitFiles,
            commit_files: (0..5).map(|index| (format!("{index}.rs"), 0, 0)).collect(),
            ..Default::default()
        };
        assert_eq!(git.drag_autoscroll_target(1, DragOverflow::Below(2)), 3);
        assert_eq!(git.drag_autoscroll_target(3, DragOverflow::Below(9)), 4);
        git.focus = GitView::Blame;
        git.blame_rows = (0..5)
            .map(|index| crate::panel::BlameRow {
                sha: "abcdef0".into(),
                author: "Author".into(),
                date: 1_700_000_000,
                lineno: index + 1,
                content: format!("line {index}"),
            })
            .collect();
        assert_eq!(git.drag_autoscroll_target(2, DragOverflow::Above(5)), 0);
        git.focus = GitView::CommitFiles;
        git.commit_files.clear();
        assert_eq!(git.drag_autoscroll_target(3, DragOverflow::Below(2)), 0);
    }
}
