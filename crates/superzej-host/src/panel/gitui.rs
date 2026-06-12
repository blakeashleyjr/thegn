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
use crate::panel::staging::{self, StageDoc};
use superzej_core::rebase_todo::{TodoAction, TodoEntry};
use termwiz::input::{KeyCode, Modifiers};

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
/// `superzej_core::patch::parse_patch` over the SAME diff text the apply
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
}

impl StagingUi {
    pub fn new(path: &str) -> Self {
        StagingUi {
            path: path.to_string(),
            pane: StagePane::Unstaged,
            cursor: 0,
            anchor: None,
            scroll: 0,
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
    /// The patch-building view's fetched document (the drilled commit's diff
    /// limited to the drilled path).
    pub patch_doc: Option<StageDocState>,
    /// Every patch doc fetched this flow, by path — custom-patch rendering
    /// needs the diff of EVERY marked file, not just the one on screen.
    pub patch_docs: std::collections::HashMap<String, StageDocState>,
    /// Acceptance cutoff for arriving git-op results (same idiom as
    /// `hunks_gen`).
    pub op_gen: u64,
}

impl GitUi {
    /// The focused list's selected inclusive range (anchor-aware).
    pub fn selection(&self) -> std::ops::RangeInclusive<usize> {
        let cur = self.cur.get(self.focus);
        match self.sel_anchor {
            Some(a) => a.min(cur)..=a.max(cur),
            None => cur..=cur,
        }
    }

    /// Reset transient state when the worktree changes underneath.
    pub fn reset_for_worktree(&mut self) {
        *self = GitUi {
            op_gen: self.op_gen + 1,
            ..GitUi::default()
        };
    }
}

/// A decoded git intent. Everything that mutates the repo flows through the
/// loop's mutation runner; navigation mutates [`GitUi`] directly.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GitMsg {
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
            k("enter", "diff", GitMsg::Drill),
            k("c", "commit", GitMsg::Commit),
            k("d", "discard", GitMsg::DiscardFile),
            k("a", "stage all", GitMsg::StageAll),
            k("s", "stash", GitMsg::StashPush),
            k("D", "reset menu", GitMsg::ResetMenu),
            k("v", "range", GitMsg::ToggleRangeMode),
            k("x", "commands", GitMsg::OpenMenu(MenuKind::CustomCommands)),
            k("z", "undo", GitMsg::Undo),
            k("Z", "redo", GitMsg::Redo),
            k("/", "filter", GitMsg::FilterStart),
            k("?", "keys", GitMsg::Cheatsheet),
        ],
        GitView::Branches => vec![
            k("space", "checkout", GitMsg::CheckoutSel),
            k("enter", "log", GitMsg::Drill),
            k("n", "new branch", GitMsg::OpenMenu(MenuKind::BranchActions)),
            k("d", "delete", GitMsg::DeleteSel),
            k("r", "rebase onto", GitMsg::RebaseOntoSel),
            k("f", "fast-forward", GitMsg::FastForward),
            k("p", "pull", GitMsg::Pull),
            k("P", "push", GitMsg::Push),
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
            k("enter", "files", GitMsg::Drill),
            k("s", "squash", GitMsg::Squash),
            k("f", "fixup", GitMsg::Fixup),
            k("d", "drop", GitMsg::Drop),
            k("e", "edit", GitMsg::Edit),
            k("r", "reword", GitMsg::Reword),
            k("t", "revert", GitMsg::Revert),
            k("i", "interactive", GitMsg::EnterInteractive),
            k("m", "rebase menu", GitMsg::OpenMenu(MenuKind::Rebase)),
            k("A", "amend staged", GitMsg::AmendStaged),
            k("C", "copy", GitMsg::CopyCommits),
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
            k("tab", "staged⇄unstaged", GitMsg::TogglePane),
            k("[", "prev hunk", GitMsg::PrevHunk),
            k("]", "next hunk", GitMsg::NextHunk),
            k("c", "commit", GitMsg::Commit),
            k("z", "undo", GitMsg::Undo),
            k("?", "keys", GitMsg::Cheatsheet),
        ],
        GitView::CommitFiles => vec![
            k("enter", "patch lines", GitMsg::Drill),
            k("z", "undo", GitMsg::Undo),
            k("/", "filter", GitMsg::FilterStart),
            k("?", "keys", GitMsg::Cheatsheet),
        ],
        GitView::PatchBuilding => vec![
            k("space", "mark line", GitMsg::StageLines),
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
            k("space", "pick⇄drop", GitMsg::StageToggleFile),
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
/// score order (nucleo, same engine as the palette). Empty query → identity.
pub fn fuzzy_filter(labels: &[String], query: &str) -> Vec<usize> {
    use nucleo_matcher::pattern::{CaseMatching, Normalization, Pattern};
    use nucleo_matcher::{Config, Matcher, Utf32Str};
    if query.is_empty() {
        return (0..labels.len()).collect();
    }
    let mut matcher = Matcher::new(Config::DEFAULT);
    let pattern = Pattern::parse(query, CaseMatching::Ignore, Normalization::Smart);
    let mut scored: Vec<(u32, usize)> = labels
        .iter()
        .enumerate()
        .filter_map(|(i, l)| {
            let mut buf = Vec::new();
            pattern
                .score(Utf32Str::new(l, &mut buf), &mut matcher)
                .map(|s| (s, i))
        })
        .collect();
    scored.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
    scored.into_iter().map(|(_, i)| i).collect()
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
    match superzej_core::rebase_todo::move_entry(todos, &sha, up) {
        Ok(moved) => {
            let at = moved.iter().position(|t| t.sha == sha).unwrap_or(cursor);
            *todos = moved;
            at
        }
        Err(_) => cursor,
    }
}

/// Build the pick-everything todo PURELY from the loaded commit list
/// (newest-first), oldest first as `git rebase -i` expects; the base is the
/// oldest commit's parent (`<sha>^`, or `--root` for a root commit).
pub fn todo_from_commits(commits: &[crate::panel::CommitRow]) -> Option<(String, Vec<TodoEntry>)> {
    let oldest = commits.last()?;
    let base = if oldest.parents.is_empty() {
        "--root".to_string()
    } else {
        format!("{}^", oldest.sha)
    };
    let todos: Vec<TodoEntry> = commits
        .iter()
        .rev()
        .map(|c| TodoEntry {
            action: TodoAction::Pick,
            sha: c.short.clone(),
            subject: c.subject.clone(),
        })
        .collect();
    Some((base, todos))
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
                _ => {}
            }
            None
        }
        None if matches!(&git.flow, GitFlow::Rebase(r) if r.running) => {
            git.flow = GitFlow::None;
            if git.focus == GitView::RebaseTodo {
                git.focus = GitView::Commits;
            }
            Some("rebase finished")
        }
        _ => None,
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

/// Resolve a raw key against the focused git context. `None` falls through
/// to `accordion_key`, the per-section action keys, then the global keymap.
///
/// Gating, in order: row-mode only; while an op is pending only navigation;
/// at Normal width only navigation (the panel hints to widen); Full-only
/// messages are dropped at Half.
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
            || ui.git.staging.is_some()
            || !matches!(
                view,
                GitView::Files | GitView::Branches | GitView::Commits | GitView::Stash
            );
        return peelable.then_some(GitMsg::Back);
    }
    // Plain cursor keys belong to the accordion in list views (it owns
    // flow-through navigation); the line-cursor views claim them.
    if matches!(
        view,
        GitView::Staging | GitView::PatchBuilding | GitView::RebaseTodo
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
        PanelWidth::Normal if !is_navigation(&msg) => None,
        PanelWidth::Half if needs_full(&msg) => None,
        _ => Some(msg),
    }
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
        // Normal: effects blocked, navigation allowed.
        let normal = ui_at(GitView::Commits, PanelWidth::Normal);
        assert_eq!(git_key(&KeyCode::Char('s'), none, &normal), None);
        assert_eq!(git_key(&KeyCode::Enter, none, &normal), Some(GitMsg::Drill));
        // Half: core effects allowed, Full-only blocked.
        let half = ui_at(GitView::Commits, PanelWidth::Half);
        assert_eq!(
            git_key(&KeyCode::Char('s'), none, &half),
            Some(GitMsg::Squash)
        );
        assert_eq!(git_key(&KeyCode::Char('i'), none, &half), None);
        assert_eq!(git_key(&KeyCode::Char('B'), Modifiers::SHIFT, &half), None);
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
                // The oldest (last) commit is a root commit when i == n-1
                // only if the test wants it; default: every commit has a
                // parent so `todo_from_commits` uses `<sha>^`.
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
        let (base, mut todos) = todo_from_commits(&commits(3)).unwrap();
        // Oldest-first picks; base is the oldest sha + "^".
        assert_eq!(todos.len(), 3);
        assert!(base.ends_with('^'));
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

        // A root commit rebases from --root.
        let mut root = commits(2);
        root.last_mut().unwrap().parents.clear();
        let (base, _) = todo_from_commits(&root).unwrap();
        assert_eq!(base, "--root");
        assert!(todo_from_commits(&[]).is_none());
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
            assert_eq!(
                git_key(&KeyCode::Char('/'), Modifiers::NONE, &ui),
                Some(GitMsg::FilterStart),
                "{view:?}"
            );
        }
    }
}
