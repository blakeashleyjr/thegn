//! Sidebar key handling + the row context menu: everything that happens while
//! the sidebar owns keyboard focus. Extracted from ratchet-pinned `run.rs`;
//! the loop dispatches on the returned [`SidebarOutcome`].
//!
//! ## The sidebar key surface
//!
//! The key surface itself is declared **once**, in
//! [`crate::sidebar_keytable::SIDEBAR_KEYS`] — chord, help label, and dispatch
//! id as a single datum. This module owns only the behaviour behind each id;
//! [`crate::sidebar_keytable::resolve`] does the key matching, and the same
//! table feeds the statusbar strip, the sidebar's NAVIGATE footer, and the
//! drift test against `docs/help/sidebar.md`. Do not add a bare `KeyCode` arm
//! here — add a table row, then handle its id.

use termwiz::input::{KeyCode, Modifiers};

use crate::chrome::FrameModel;
use crate::handlers::sidebar_persist::SidebarState;
use crate::sidebar_keytable::{SidebarKeyId as Id, chord_of};
use crate::sidebar_view::{RowMenuEntry, menu_step};

/// What the event loop should do after a sidebar key was handled.
pub(crate) enum SidebarOutcome {
    /// Key wasn't for the sidebar; let normal dispatch handle it.
    NotHandled,
    /// Handled; just redraw.
    Redraw,
    /// Leave sidebar focus (return input to the pane).
    Defocus,
    /// Activate this `(worktree group, tab)` target.
    Activate(crate::sidebar::RowTarget),
    /// The layout changed (bar width); recompute chrome.
    Relayout,
    /// Reorder the current selection (marked rows, else the cursor row) one slot
    /// (Shift+↑/↓). Needs `&mut Session`, so the loop performs it.
    ReorderSelection { up: bool },
    /// Close the worktree groups at these session indices (bulk action) — the
    /// non-destructive forget that keeps branch + files.
    CloseGroups(Vec<usize>),
    /// DELETE these worktree groups from disk (`git worktree remove`) and
    /// close them — destructive; the loop interposes the confirm flow.
    DeleteGroups(Vec<usize>),
    /// Open the close-or-delete chooser (`d`) for these worktree groups: a
    /// disambiguation modal (close = safe default, delete = danger arm).
    ConfirmCloseOrDelete(Vec<usize>),
    /// Forget a whole workspace: close its live groups and prune its DB rows,
    /// WITHOUT touching the worktree files on disk. Always confirmed.
    RemoveWorkspace {
        repo_path: String,
        slug: String,
        display: String,
    },
    /// Copy this text (a worktree path) to the system clipboard via OSC-52.
    CopyText(String),
    /// Hand the selected worktree row to the configured IDE without activating
    /// it first. Pane-only editors degrade if that dormant row is not focused.
    OpenInIde {
        target: thegn_core::editor::EditorTarget,
        workspace_slug: String,
    },
    /// Prompt to rename the worktree group at this session index (its current
    /// branch seeds the input). Item 53.
    PromptRename { gi: usize, branch: String },
    /// Fork a new worktree branching from this source branch (item 52). The
    /// loop launches the new-worktree wizard with the base overridden.
    Fork {
        base_branch: String,
        repo_root: String,
    },
    /// Run a global keymap action from a sidebar key (`n`/`N` create): the
    /// loop's action dispatcher handles it exactly as if the palette fired it.
    Synthetic(crate::keymap::Action),
    /// Open the new-worktree wizard rooted at this repo (the cursor row's
    /// workspace, which need not be the active one). `folder` is the sidebar
    /// folder the cursor row sits in (a folder header, or a filed worktree):
    /// the finished worktree is filed there.
    NewWorktreeIn {
        repo_root: String,
        folder: Option<SidebarFolderIntent>,
    },
    /// Open the move-to-folder picker targeting this worktree row (`f`).
    MoveToFolder {
        worktree_path: String,
        repo_path: String,
    },
    /// Prompt for a new (empty) folder in this workspace (`f` on a
    /// workspace/folder row).
    NewFolderPrompt { repo_path: String },
    /// Prompt to rename this folder (`r` on a folder row).
    RenameFolder { folder_id: i64, name: String },
    /// Confirm deleting this folder — its worktrees move back to the
    /// workspace root (never touches disk).
    DeleteFolder { folder_id: i64, name: String },
    /// Confirm closing this terminal (`d` on a terminal row).
    CloseTerminal { name: String },
    /// Open the sort-mode menu (`s`).
    SortMenu,
    /// Open the existing Work → Merge queue section for this workspace.
    OpenMergeQueue { repo_path: String },
    /// Show the sidebar help overlay (`?`).
    ShowHelp,
    /// A merge-queue action fired from the row/workspace context menu (mirrors
    /// the panel's `a/A/x/l/r/c/D`). `path` is the target worktree (per-row
    /// actions) or any path inside the repo (workspace-wide actions). The loop
    /// runs the mutation off-thread via `handlers::merge_queue`.
    Mq {
        action: crate::handlers::merge_queue::SidebarMq,
        path: String,
    },
}

/// The durable identity captured for a folder-scoped worktree creation. The
/// name is only the display/fallback value; the id lets completion follow a
/// rename while refusing a deleted or replacement folder.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SidebarFolderIntent {
    pub(crate) folder_id: i64,
    pub(crate) name: String,
}

/// The merge-queue context-menu entries (`(id, label)`, in render order) for a
/// worktree row given its queue status. Pure so the status→entry matrix is
/// unit-testable: not-queued ⇒ Add; queued ⇒ Remove, plus Land when `ready` and
/// Retry when blocked (deferred / gate-failed / needs-human). Mirrors the
/// panel's `a/x/l/r` availability rules (`handlers::merge_queue::row_action_for`).
fn worktree_mq_entries(
    mq_status: Option<thegn_core::attention::MqStatus>,
) -> Vec<(&'static str, &'static str)> {
    use thegn_core::attention::MqStatus;
    match mq_status {
        None => vec![("mq-add", "Add to merge queue")],
        Some(status) => {
            let mut v = vec![("mq-remove", "Remove from merge queue")];
            if status == MqStatus::Ready {
                v.push(("mq-land", "Land branch"));
            }
            if matches!(
                status,
                MqStatus::Deferred
                    | MqStatus::GateFailed
                    | MqStatus::GateError
                    | MqStatus::AgentBlocked
                    | MqStatus::NeedsHuman
            ) {
                v.push(("mq-retry", "Retry"));
            }
            v
        }
    }
}

/// What `Action::NewWorktree` should do, given the sidebar cursor.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum NewWorktreeTarget {
    /// The cursor row's repo resolved — open the wizard there.
    Root(String),
    /// The cursor row names a workspace/worktree/folder but its repo is
    /// unresolvable (a live fallback whose registry heal also failed).
    /// Refuse — NEVER silently build in another workspace's repo.
    Refuse(&'static str),
    /// No sidebar row in play (no sidebar focus, no selected row, or a
    /// terminals-region row): the active tab's repo is the intent, as before.
    ActiveFallback,
}

/// The refusal message displayed when `NewWorktreeTarget::Refuse` fires.
pub(crate) const NEW_WORKTREE_REFUSAL: &str =
    "No repo path for this project yet — it registers on the next refresh";

impl SidebarState {
    /// What the cursor row activates, if anything.
    pub(crate) fn cursor_target(&self, model: &FrameModel) -> Option<crate::sidebar::RowTarget> {
        self.selected_row(model).and_then(|r| r.tab_target.clone())
    }

    /// The repo path backing a workspace slug, from the model's workspace list
    /// (`(slug, display, kind, repo_path)`); `None` for live fallbacks with no
    /// DB row yet.
    fn workspace_repo_path(model: &FrameModel, slug: &str) -> Option<String> {
        model
            .sidebar_workspaces
            .iter()
            .find(|(s, ..)| s == slug)
            .map(|(_, _, _, p)| p.clone())
            .filter(|p| !p.is_empty())
    }

    /// The repo root to open a create-worktree wizard in, for the cursor row:
    /// a workspace row's repo path, or a worktree row's main checkout (via its
    /// own path, falling back to the workspace list).
    fn cursor_repo_root(&self, model: &FrameModel) -> Option<String> {
        let row = self.selected_row(model)?;
        match row.kind {
            crate::sidebar::RowKind::Workspace => row.worktree_path.clone(),
            // A worktree row's repo root is its workspace's already-hydrated
            // repo path (the main checkout). Prefer that over spawning
            // `git rev-parse` here: this runs on the compositor loop, and a
            // no-timeout git subprocess against a stalled mount / wedged .git
            // lock would freeze the whole UI (event-loop-blocking invariant).
            // Fall back to `main_worktree` only when the model has no workspace
            // row for this slug (a live worktree with no persisted workspace).
            crate::sidebar::RowKind::Worktree | crate::sidebar::RowKind::PipelineWorktree => {
                Self::workspace_repo_path(model, &row.workspace_slug).or_else(|| {
                    row.worktree_path.as_deref().and_then(|p| {
                        thegn_core::repo::main_worktree(std::path::Path::new(p))
                            .map(|p| p.to_string_lossy().into_owned())
                    })
                })
            }
            crate::sidebar::RowKind::Folder => {
                Self::workspace_repo_path(model, &row.workspace_slug)
            }
            _ => None,
        }
    }

    /// The sidebar folder the cursor row belongs to, with its stable DB id: a
    /// folder header itself, or the folder a worktree row is filed into.
    /// `None` for loose worktrees and every other row kind. Pure over the
    /// hydrated model.
    fn cursor_folder_intent(&self, model: &FrameModel) -> Option<SidebarFolderIntent> {
        use crate::sidebar::RowKind;
        let row = self.selected_row(model)?;
        let folder_id = match row.kind {
            RowKind::Folder => row.folder_id,
            RowKind::Worktree | RowKind::PipelineWorktree => {
                let path = row.worktree_path.as_deref()?;
                model
                    .sidebar_db_worktrees
                    .iter()
                    .find(|w| w.path == path)?
                    .folder_id
            }
            _ => return None,
        };
        let repo_root = self.cursor_repo_root(model)?;
        folder_id.and_then(|fid| {
            model
                .sidebar_db_folders
                .iter()
                .find(|f| f.folder_id == fid && f.repo_path == repo_root)
                .map(|f| SidebarFolderIntent {
                    folder_id: fid,
                    name: f.name.clone(),
                })
        })
    }

    /// Whether the cursor row lives in the TERMINALS region (the banner, a host
    /// group, a terminal leaf, or the empty hint).
    pub(crate) fn cursor_in_terminals(&self, model: &FrameModel) -> bool {
        self.selected_row(model)
            .map(|r| r.workspace_slug == "terminals" || r.workspace_slug.starts_with("terminals/"))
            .unwrap_or(false)
    }

    /// Determine the new-worktree target from sidebar focus + cursor position.
    pub(crate) fn new_worktree_target(
        &self,
        model: &FrameModel,
        sidebar_focus: bool,
    ) -> NewWorktreeTarget {
        if !sidebar_focus || self.selected_row(model).is_none() {
            return NewWorktreeTarget::ActiveFallback;
        }
        if self.cursor_in_terminals(model) {
            return NewWorktreeTarget::ActiveFallback;
        }
        match self.cursor_repo_root(model) {
            Some(root) => NewWorktreeTarget::Root(root),
            None => NewWorktreeTarget::Refuse(NEW_WORKTREE_REFUSAL),
        }
    }

    /// The sidebar outcome for `Id::NewWorktree` and the `"new-worktree"`
    /// context-menu entry, factoring refusal + redraw vs unresolved rows.
    pub(crate) fn new_worktree_outcome(&self, model: &mut FrameModel) -> SidebarOutcome {
        if self.cursor_in_terminals(model) {
            return SidebarOutcome::Synthetic(crate::keymap::Action::NewTerminal);
        }
        match self.cursor_repo_root(model) {
            Some(repo_root) => SidebarOutcome::NewWorktreeIn {
                repo_root,
                folder: self.cursor_folder_intent(model),
            },
            None => {
                model.status = NEW_WORKTREE_REFUSAL.into();
                self.sync(model);
                SidebarOutcome::Redraw
            }
        }
    }

    /// The remove-workspace outcome for the cursor row, when it is a Workspace
    /// row backed by a DB repo path. `None` for worktree rows or live fallbacks
    /// with no persisted workspace yet.
    fn remove_workspace_target(&self, model: &FrameModel) -> Option<SidebarOutcome> {
        let row = self.selected_row(model)?;
        if row.kind != crate::sidebar::RowKind::Workspace {
            return None;
        }
        let repo_path = row.worktree_path.clone()?;
        Some(SidebarOutcome::RemoveWorkspace {
            repo_path,
            slug: row.workspace_slug.clone(),
            display: row.label.clone(),
        })
    }

    /// The project the sidebar cursor is in — its own project row, or the
    /// project row a worktree/folder/lane under the cursor belongs to — as a
    /// `RemoveWorkspace` outcome. Used by the palette's "Remove project" so it
    /// acts on the project the user selected, not the active one. `None` when
    /// the cursor is not inside a removable project (terminals, headings).
    pub(crate) fn selected_project_removal(&self, model: &FrameModel) -> Option<SidebarOutcome> {
        use crate::sidebar::RowKind;
        let visible: Vec<&crate::sidebar::SidebarRow> =
            model.sidebar_rows.iter().filter(|r| r.visible).collect();
        let row = *visible.get(self.cursor)?;
        if row.workspace_slug.is_empty() {
            return None;
        }
        let project = visible[..=self.cursor]
            .iter()
            .rev()
            .find(|r| r.kind == RowKind::Workspace && r.workspace_slug == row.workspace_slug)?;
        Some(SidebarOutcome::RemoveWorkspace {
            repo_path: project.worktree_path.clone()?,
            slug: project.workspace_slug.clone(),
            display: project.label.clone(),
        })
    }

    /// Whether the cursor row is the workspace's home worktree (undeletable,
    /// unrenamable).
    fn cursor_is_home(&self, model: &FrameModel, session: &crate::session::Session) -> bool {
        self.selected_row(model).is_some_and(|row| {
            matches!(
                row.tab_target,
                Some(crate::sidebar::RowTarget::Tab(gi, _))
                    if session.worktrees.get(gi).map(|g| g.kind)
                        == Some(crate::session::GroupKind::Home)
            )
        })
    }

    /// The row-kind-aware close/delete outcome for `d` / `Delete` / the menu.
    fn delete_outcome(
        &self,
        model: &mut FrameModel,
        session: &crate::session::Session,
    ) -> Option<SidebarOutcome> {
        use crate::sidebar::RowKind;
        let row = self.selected_row(model)?;
        match row.kind {
            RowKind::Workspace => self.remove_workspace_target(model),
            RowKind::Folder => Some(SidebarOutcome::DeleteFolder {
                folder_id: row.folder_id?,
                name: row.label.clone(),
            }),
            RowKind::Terminal
                if row
                    .worktree_path
                    .as_deref()
                    .is_some_and(crate::lead::group::is_lead_group) =>
            {
                model.status = "The Lead can't be closed from the sidebar".into();
                Some(SidebarOutcome::Redraw)
            }
            RowKind::Terminal => Some(SidebarOutcome::CloseTerminal {
                name: row.label.clone(),
            }),
            RowKind::Worktree | RowKind::PipelineWorktree => {
                if self.cursor_is_home(model, session) && self.marked.is_empty() {
                    model.status = "The home worktree can't be closed or deleted".into();
                    return Some(SidebarOutcome::Redraw);
                }
                let dormant_ws = matches!(
                    row.tab_target,
                    Some(crate::sidebar::RowTarget::Workspace { .. })
                )
                .then(|| row.workspace_slug.clone());
                let targets = self.action_targets(model);
                if targets.is_empty() {
                    // A dormant workspace's worktree has no live session group
                    // for the close/delete flow to act on. Say so — `d` is an
                    // Essential-tier key and must never silently no-op.
                    if let Some(slug) = dormant_ws {
                        model.status = format!(
                            "Open project \"{slug}\" first to close or delete this worktree"
                        );
                        return Some(SidebarOutcome::Redraw);
                    }
                    return None;
                }
                self.hint_skipped_workspace_marks(model);
                Some(SidebarOutcome::ConfirmCloseOrDelete(targets))
            }
            _ => None,
        }
    }

    /// Build the context-menu entries for the cursor row (item 27). The menu is
    /// the canonical action catalog: every keyboard action appears here with
    /// its key chip, so it doubles as key discovery.
    pub(crate) fn menu_for_cursor(
        &self,
        model: &FrameModel,
        session: &crate::session::Session,
    ) -> Option<crate::sidebar_view::RowMenu> {
        use crate::sidebar::RowKind;
        let row = self.selected_row(model)?;
        let e = RowMenuEntry::new;
        let sep = RowMenuEntry::separator;
        let mut entries: Vec<RowMenuEntry> = Vec::new();
        match row.kind {
            RowKind::Worktree | RowKind::PipelineWorktree => {
                if row.tab_target.is_some() {
                    entries.push(e("open", "Open", Some(chord_of(Id::Activate))));
                }
                if row.worktree_path.is_some() {
                    entries.push(e("open-in-ide", "Open in IDE", None));
                }
                entries.push(sep());
                entries.push(e(
                    "new-worktree",
                    "New worktree here…",
                    Some(chord_of(Id::NewWorktree)),
                ));
                if row.worktree_path.is_some() {
                    entries.push(e("fork", "Branch from this…", Some(chord_of(Id::Fork))));
                }
                // Rename/close/delete run through the live session, so a
                // dormant workspace's rows (RowTarget::Workspace) must not
                // offer them — the entries would silently no-op.
                let is_live = matches!(row.tab_target, Some(crate::sidebar::RowTarget::Tab(_, _)));
                let is_home = self.cursor_is_home(model, session);
                if !is_home && is_live {
                    entries.push(e("rename", "Rename…", Some(chord_of(Id::Rename))));
                }
                entries.push(sep());
                if row.worktree_path.is_some() {
                    entries.push(e(
                        "move-to-folder",
                        "Move to folder…",
                        Some(chord_of(Id::Folder)),
                    ));
                }
                if row.is_pinnable() {
                    entries.push(e("pin", "Pin / unpin", Some(chord_of(Id::TogglePin))));
                }
                if row.worktree_path.is_some() {
                    entries.push(e("copy-path", "Copy path", Some(chord_of(Id::CopyPath))));
                }
                entries.push(e("info", "Info…", Some(chord_of(Id::Info))));
                // Merge-queue controls (status-aware, mirroring the panel keys).
                // Skipped for the home row (it sits on the target branch).
                if !is_home && row.worktree_path.is_some() {
                    entries.push(sep());
                    for (id, label) in worktree_mq_entries(row.mq_status) {
                        entries.push(e(id, label, None));
                    }
                }
                if !is_home && is_live {
                    entries.push(sep());
                    entries.push(e("close", "Close — keep files on disk", None));
                    entries.push(
                        e(
                            "delete",
                            "Delete worktree files… (branch kept)",
                            Some(chord_of(Id::Delete)),
                        )
                        .danger(),
                    );
                }
            }
            RowKind::Workspace => {
                // Enter toggles collapse on headers, so "Open" carries no chip.
                if row.tab_target.is_some() {
                    entries.push(e("open", "Open", None));
                }
                entries.push(e(
                    "toggle",
                    "Collapse / expand",
                    Some(chord_of(Id::Activate)),
                ));
                entries.push(sep());
                entries.push(e(
                    "new-worktree",
                    "New worktree…",
                    Some(chord_of(Id::NewWorktree)),
                ));
                entries.push(e("new-folder", "New folder…", Some(chord_of(Id::Folder))));
                if !row.pin_key.is_empty() {
                    entries.push(e("pin", "Pin / unpin", Some(chord_of(Id::TogglePin))));
                }
                entries.push(e(
                    "sort",
                    "Sort worktrees by…",
                    Some(chord_of(Id::SortMenu)),
                ));
                entries.push(e("info", "Info…", Some(chord_of(Id::Info))));
                entries.push(e(
                    "toggle-flat",
                    "Flat / grouped view",
                    Some(chord_of(Id::ToggleFlat)),
                ));
                entries.push(e(
                    "cycle-detail",
                    "Row detail: all / cursor / off",
                    Some(chord_of(Id::CycleDetail)),
                ));
                if row.worktree_path.is_some() {
                    entries.push(e("open-merge-queue", "Open merge queue", None));
                }
                // Workspace-wide merge-queue controls (panel `A` / clear / `D`).
                entries.push(sep());
                entries.push(e("mq-add-all", "Queue all worktrees", None));
                entries.push(e("mq-clear", "Clear merge queue", None));
                entries.push(e("mq-drain", "Drain merge queue", None));
                if row.worktree_path.is_some() {
                    entries.push(sep());
                    entries.push(
                        e(
                            "remove-project",
                            "Remove project…",
                            Some(chord_of(Id::Delete)),
                        )
                        .danger(),
                    );
                }
            }
            RowKind::Folder => {
                entries.push(e(
                    "toggle",
                    "Collapse / expand",
                    Some(chord_of(Id::Activate)),
                ));
                entries.push(e(
                    "rename-folder",
                    "Rename folder…",
                    Some(chord_of(Id::Rename)),
                ));
                entries.push(e(
                    "new-worktree",
                    "New worktree here…",
                    Some(chord_of(Id::NewWorktree)),
                ));
                entries.push(sep());
                entries.push(
                    e(
                        "delete-folder",
                        "Delete folder (keeps worktrees)",
                        Some(chord_of(Id::Delete)),
                    )
                    .danger(),
                );
            }
            RowKind::TerminalHost => {
                entries.push(e(
                    "toggle",
                    "Collapse / expand",
                    Some(chord_of(Id::Activate)),
                ));
                entries.push(e(
                    "new-terminal",
                    "New terminal here…",
                    Some(chord_of(Id::NewWorktree)),
                ));
            }
            RowKind::Terminal => {
                if row.tab_target.is_some() {
                    entries.push(e("open", "Open", Some(chord_of(Id::Activate))));
                }
                if !row.pin_key.is_empty() {
                    entries.push(e("pin", "Pin / unpin", Some(chord_of(Id::TogglePin))));
                }
                entries.push(e(
                    "new-terminal",
                    "New terminal…",
                    Some(chord_of(Id::NewWorktree)),
                ));
                entries.push(sep());
                entries.push(
                    e(
                        "close-terminal",
                        "Close terminal…",
                        Some(chord_of(Id::Delete)),
                    )
                    .danger(),
                );
            }
            // A derived `Pipelines` group / lane collapses and expands, and
            // that is the whole of what it can do: it is derived from the
            // roster, so renaming, filing, pinning or deleting one has nothing
            // to write to.
            // The derived "Other" group of external worktrees is the same: a
            // view-side grouping with no folder row to rename or delete, and
            // so is a derived batch row.
            RowKind::PipelineGroup
            | RowKind::PipelineLane
            | RowKind::OtherGroup
            | RowKind::BatchGroup => {
                entries.push(e(
                    "toggle",
                    "Collapse / expand",
                    Some(chord_of(Id::Activate)),
                ));
            }
            // The "show more"/"show fewer" toggle does one thing (Enter,
            // Space or a click); a menu would only repeat it.
            RowKind::SectionHeading | RowKind::EmptyHint | RowKind::ShowMore => {
                return None;
            }
        }
        // Drop leading/trailing separators (rows above may not have emitted).
        while entries.first().is_some_and(|x| x.is_separator()) {
            entries.remove(0);
        }
        while entries.last().is_some_and(|x| x.is_separator()) {
            entries.pop();
        }
        if entries.is_empty() {
            return None;
        }
        let cursor = entries.iter().position(|x| !x.is_separator())?;
        Some(crate::sidebar_view::RowMenu {
            anchor: self.cursor,
            target_pin_key: row.pin_key.clone(),
            entries,
            cursor,
        })
    }

    /// Handle a key while the sidebar owns focus. Mutates view/interaction
    /// state, rebuilds rows, and returns what the loop must do.
    pub(crate) fn handle_key(
        &mut self,
        key: &KeyCode,
        mods: Modifiers,
        model: &mut FrameModel,
        session: &crate::session::Session,
    ) -> SidebarOutcome {
        // Filter input sub-mode captures text (item 21).
        if self.filtering {
            let mut committed = false;
            match key {
                key if crate::input::is_escape_key(key) => {
                    self.filtering = false;
                    self.view.filter.clear();
                }
                KeyCode::Enter => {
                    self.filtering = false;
                    committed = true;
                }
                KeyCode::Backspace => {
                    self.view.filter.pop();
                }
                KeyCode::Char(c) if !mods.contains(Modifiers::CTRL) => {
                    self.view.filter.push(*c);
                }
                _ => return SidebarOutcome::Redraw,
            }
            self.cursor = 0;
            // The visible set just changed shape, so a snapshot of the old
            // order means nothing — re-snapshot the filtered one.
            crate::sidebar_freeze::rearm(self, &model.sidebar_status);
            self.rebuild(model, session);
            // Committing lands the cursor on the first actionable MATCH (a
            // worktree/terminal row), not on row 0 — which is the first
            // workspace *header*, so Enter-then-Enter used to fold a header
            // instead of opening the row the user filtered for.
            if committed
                && let Some(idx) = model
                    .sidebar_rows
                    .iter()
                    .filter(|r| r.visible)
                    .position(|r| {
                        matches!(
                            r.kind,
                            crate::sidebar::RowKind::Worktree
                                | crate::sidebar::RowKind::PipelineWorktree
                                | crate::sidebar::RowKind::Terminal
                        )
                    })
            {
                self.cursor = idx;
                self.sync(model);
            }
            return SidebarOutcome::Redraw;
        }

        // Open context menu captures navigation (item 27).
        if let Some(menu) = &mut self.menu {
            match key {
                key if crate::input::is_escape_key(key) => {
                    self.menu = None;
                }
                KeyCode::UpArrow | KeyCode::Char('k') => {
                    menu.cursor = menu_step(&menu.entries, menu.cursor, -1);
                }
                KeyCode::DownArrow | KeyCode::Char('j') => {
                    menu.cursor = menu_step(&menu.entries, menu.cursor, 1);
                }
                KeyCode::Enter => {
                    let id = menu.entries.get(menu.cursor).map(|e| e.id.clone());
                    let target_key = menu.target_pin_key.clone();
                    self.menu = None;
                    if let Some(id) = id.filter(|id| !id.is_empty()) {
                        // The action runs against the row the menu was OPENED
                        // on. If that row vanished while the menu was up
                        // (hydration prune, re-file re-keying it), bail —
                        // falling through would fire the entry (possibly
                        // Delete) at whatever row the cursor happens to be on.
                        let Some(idx) = model
                            .sidebar_rows
                            .iter()
                            .filter(|r| r.visible)
                            .position(|r| r.pin_key == target_key)
                        else {
                            model.status = "That row is gone — menu closed".into();
                            self.sync(model);
                            return SidebarOutcome::Redraw;
                        };
                        self.cursor = idx;
                        return self.run_menu_action(&id, model, session);
                    }
                }
                _ => {}
            }
            self.sync(model);
            return SidebarOutcome::Redraw;
        }

        let visible = Self::visible_len(model);
        // Every key below is declared once in `sidebar_keytable::SIDEBAR_KEYS`,
        // which also feeds the statusbar strip, the NAVIGATE footer, and the
        // help-page drift test — so a key can't exist without surfacing.
        let Some(id) = crate::sidebar_keytable::resolve(key, mods) else {
            return SidebarOutcome::NotHandled;
        };
        // The wheel scrolls the viewport without moving the cursor, so the
        // cursor can be off-screen when a key arrives. Snap it back into the
        // window first: an action key must never target a row you can't see,
        // and a relative move must start from where you are looking. O(1) —
        // it reads the window `settle_scroll` stamped, no layout pass.
        if id.is_cursor_relative() {
            self.reanchor_cursor();
        }
        match id {
            Id::Defocus => {
                // First Esc/q with a COMMITTED filter clears it (the only
                // other clear site is inside the filtering sub-mode, so a
                // committed filter used to silently hide most of the tree for
                // the rest of the session); the next one defocuses.
                if !self.view.filter.is_empty() {
                    self.view.filter.clear();
                    self.cursor = 0;
                    // Same as the filter sub-mode: the visible set changed.
                    crate::sidebar_freeze::rearm(self, &model.sidebar_status);
                    self.rebuild(model, session);
                    model.status = "Sidebar filter cleared".into();
                    return SidebarOutcome::Redraw;
                }
                return SidebarOutcome::Defocus;
            }
            // Shift+↑/↓ reorders the selection (the loop has `&mut Session`).
            // Only the arrows carry Shift here — Shift+j/k normalise to J/K.
            Id::ReorderUp => {
                return SidebarOutcome::ReorderSelection { up: true };
            }
            Id::ReorderDown => {
                return SidebarOutcome::ReorderSelection { up: false };
            }
            Id::CursorDown => {
                if visible > 0 {
                    self.cursor = (self.cursor + 1).min(visible - 1);
                }
            }
            Id::CursorUp => {
                self.cursor = self.cursor.saturating_sub(1);
            }
            // A page is the window the last frame laid out, not a fixed 10 —
            // `last_list_rows` is stamped by `settle_scroll`. One frame of
            // staleness after a resize is harmless: it only scales a cursor
            // step. Falls back to 10 before the first layout pass.
            Id::PageDown => {
                if visible > 0 {
                    self.cursor = (self.cursor + self.page_step()).min(visible - 1);
                }
            }
            Id::PageUp => {
                self.cursor = self.cursor.saturating_sub(self.page_step());
            }
            Id::CursorHome => {
                self.cursor = 0;
            }
            Id::CursorEnd => {
                self.cursor = visible.saturating_sub(1);
            }
            Id::Activate => {
                // On a collapsible header (workspace or terminal host), Enter
                // toggles collapse; on an EmptyHint it runs the hinted action;
                // elsewhere it opens the row.
                if let Some(row) = self.selected_row(model) {
                    if row.kind.is_collapsible() {
                        return self.toggle_collapse(model, session);
                    }
                    if row.kind == crate::sidebar::RowKind::EmptyHint {
                        return SidebarOutcome::Synthetic(crate::keymap::Action::NewTerminal);
                    }
                    if row.kind == crate::sidebar::RowKind::ShowMore {
                        return self.toggle_show_more(model, session);
                    }
                    if let Some(t) = row.tab_target.clone() {
                        return SidebarOutcome::Activate(t);
                    }
                }
            }
            Id::Expand => {
                // Expand a collapsed header.
                if let Some(row) = self.selected_row(model)
                    && row.kind.is_collapsible()
                    && row.collapsed
                {
                    return self.toggle_collapse(model, session);
                }
            }
            Id::Collapse => {
                // On an expanded collapsible header: collapse it. Otherwise (a
                // leaf sub-item, or an already-collapsed header): collapse the
                // nearest collapsible ancestor and move the cursor onto it.
                if let Some(row) = self.selected_row(model) {
                    if row.kind.is_collapsible() && !row.collapsed {
                        return self.toggle_collapse(model, session);
                    }
                    return self.collapse_parent(model, session);
                }
            }
            Id::Filter => {
                // The rail paints no filter echo (and no header at all), so
                // typing into an invisible query field just makes rows vanish
                // mysteriously. Refuse with a pointer instead.
                if model.sidebar_rail {
                    model.status = "Filter needs the full sidebar — Alt-s to expand".into();
                    self.sync(model);
                    return SidebarOutcome::Redraw;
                }
                self.filtering = true;
                self.sync(model);
            }
            Id::SortMenu => return SidebarOutcome::SortMenu,
            Id::ToggleFlat => return self.toggle_flat(model, session),
            Id::TogglePin => return self.toggle_pin(model, session),
            Id::CycleDetail => return self.cycle_focus_detail(model),
            Id::Info => {
                if self.selected_row(model).is_some_and(|r| {
                    matches!(
                        r.kind,
                        crate::sidebar::RowKind::Worktree
                            | crate::sidebar::RowKind::PipelineWorktree
                            | crate::sidebar::RowKind::Workspace
                    )
                }) {
                    return SidebarOutcome::Synthetic(crate::keymap::Action::OpenInfo);
                }
                model.status = "Info covers project and worktree rows".into();
            }
            Id::Mark => {
                // Multi-select toggle (item 26): mark/unmark the cursor row if it
                // is a worktree or workspace. Collapse now lives solely on
                // Enter/←/→ and the caret click, so headers can be selected too.
                if let Some(row) = self.selected_row(model) {
                    // Space on the "show more" row toggles it, like Enter:
                    // it is not markable, and refusing would read as broken.
                    if row.kind == crate::sidebar::RowKind::ShowMore {
                        return self.toggle_show_more(model, session);
                    }
                    if row.is_markable() {
                        let key = row.pin_key.clone();
                        if !self.marked.remove(&key) {
                            self.marked.insert(key);
                        }
                        self.sync(model);
                    } else {
                        model.status = "Only worktrees and projects can be marked".into();
                    }
                }
            }
            Id::RowMenu => {
                // The rail paints no menu overlay, but an open menu still
                // captures every key — an invisible modal whose Enter can hit
                // the danger delete arm. Refuse in rail mode.
                if model.sidebar_rail {
                    model.status = "The menu needs the full sidebar — Alt-s to expand".into();
                    self.sync(model);
                    return SidebarOutcome::Redraw;
                }
                self.menu = self.menu_for_cursor(model, session);
                self.sync(model);
            }
            Id::Delete => {
                if let Some(out) = self.delete_outcome(model, session) {
                    return out;
                }
                // An Essential-tier key must never silently no-op.
                model.status = "Nothing to close or delete on this row".into();
            }
            Id::Rename => {
                if let Some(out) = self.rename_outcome(model, session) {
                    return out;
                }
                model.status = "Only worktrees and folders can be renamed".into();
            }
            Id::NewWorktree => {
                return self.new_worktree_outcome(model);
            }
            Id::NewWorkspace => {
                return SidebarOutcome::Synthetic(crate::keymap::Action::NewWorkspace);
            }
            Id::Fork => {
                if let Some(out) = self.fork_outcome(model) {
                    return out;
                }
                model.status = "Branch-from needs a worktree row with a branch".into();
            }
            Id::Folder => {
                if let Some(out) = self.folder_outcome(model) {
                    return out;
                }
                model.status = "Folders apply to worktree and project rows".into();
            }
            Id::CopyPath => {
                if let Some(p) = self
                    .selected_row(model)
                    .and_then(|r| r.worktree_path.clone())
                {
                    return SidebarOutcome::CopyText(p);
                }
                model.status = "This row has no path to copy".into();
            }
            Id::Help => return SidebarOutcome::ShowHelp,
            Id::WidthDec | Id::WidthInc | Id::ToggleWide => {
                // Rail mode ignores `width`/`expanded` entirely
                // (`effective_cols` matches Rail first), so nudging there is
                // an invisible no-op that still PERSISTS — cycle back to Full
                // (or restart) and the sidebar sits at a width you never saw
                // yourself set. Refuse with a pointer instead.
                if model.sidebar_rail {
                    model.status = "Width applies to the full sidebar — Alt-s to expand".into();
                    self.sync(model);
                    return SidebarOutcome::Redraw;
                }
                if id == Id::ToggleWide {
                    // Toggle the Wide expand (mirrors the panel's `e`): ~half
                    // the window vs. the fine-nudged width.
                    self.expanded = !self.expanded;
                    self.persist("sidebar_expanded", if self.expanded { "1" } else { "0" });
                    model.status = if self.expanded {
                        "sidebar: wide".into()
                    } else {
                        "sidebar: resting width".into()
                    };
                    return SidebarOutcome::Relayout;
                }
                // Report the landing width: silent nudges read as a dead key
                // once the clamp is reached.
                let w = self.adjust_width(if id == Id::WidthDec { -2 } else { 2 });
                model.status = format!("sidebar width: {w} cols");
                return SidebarOutcome::Relayout;
            }
        }
        self.sync(model);
        SidebarOutcome::Redraw
    }

    /// The rename outcome for the cursor row (`r` / F2 / menu): a worktree's
    /// branch, or a folder's name.
    fn rename_outcome(
        &self,
        model: &mut FrameModel,
        session: &crate::session::Session,
    ) -> Option<SidebarOutcome> {
        use crate::sidebar::RowKind;
        let row = self.selected_row(model)?;
        match row.kind {
            RowKind::Folder => Some(SidebarOutcome::RenameFolder {
                folder_id: row.folder_id?,
                name: row.label.clone(),
            }),
            RowKind::Worktree | RowKind::PipelineWorktree => {
                if self.cursor_is_home(model, session) {
                    model.status = "The home worktree can't be renamed".into();
                    return Some(SidebarOutcome::Redraw);
                }
                let dormant_ws = matches!(
                    row.tab_target,
                    Some(crate::sidebar::RowTarget::Workspace { .. })
                )
                .then(|| row.workspace_slug.clone());
                if let Some(crate::sidebar::RowTarget::Tab(gi, _)) = row.tab_target.clone()
                    && let Some(branch) = row.branch.clone()
                {
                    return Some(SidebarOutcome::PromptRename { gi, branch });
                }
                // Dormant workspace's worktree: rename runs through the live
                // session; surface why nothing happened instead of no-opping.
                if let Some(slug) = dormant_ws {
                    model.status = format!("Open project \"{slug}\" first to rename this worktree");
                    return Some(SidebarOutcome::Redraw);
                }
                None
            }
            _ => None,
        }
    }

    /// The branch-from-this outcome (`b` / menu "fork"): a new worktree based
    /// on the cursor row's branch.
    fn fork_outcome(&self, model: &FrameModel) -> Option<SidebarOutcome> {
        let row = self.selected_row(model)?;
        let branch = row.branch.clone().filter(|b| !b.is_empty())?;
        let path = row.worktree_path.clone()?;
        // Prefer the workspace's already-hydrated repo path over a loop-side
        // `git rev-parse` (see `cursor_repo_root`: a no-timeout subprocess on
        // the compositor loop can freeze the UI on a stalled mount). Fall back
        // to `main_worktree`, then the row's own path.
        let repo_root = Self::workspace_repo_path(model, &row.workspace_slug)
            .or_else(|| {
                thegn_core::repo::main_worktree(std::path::Path::new(&path))
                    .map(|p| p.to_string_lossy().into_owned())
            })
            .unwrap_or(path);
        Some(SidebarOutcome::Fork {
            base_branch: branch,
            repo_root,
        })
    }

    /// The folder outcome for `f`: move-to-folder on a worktree row, new-folder
    /// on a workspace/folder row.
    fn folder_outcome(&self, model: &FrameModel) -> Option<SidebarOutcome> {
        use crate::sidebar::RowKind;
        let row = self.selected_row(model)?;
        match row.kind {
            RowKind::Worktree | RowKind::PipelineWorktree => {
                let worktree_path = row.worktree_path.clone()?;
                let repo_path = Self::workspace_repo_path(model, &row.workspace_slug)?;
                Some(SidebarOutcome::MoveToFolder {
                    worktree_path,
                    repo_path,
                })
            }
            RowKind::Workspace | RowKind::Folder => {
                let repo_path = match row.kind {
                    RowKind::Workspace => row.worktree_path.clone(),
                    _ => Self::workspace_repo_path(model, &row.workspace_slug),
                }?;
                Some(SidebarOutcome::NewFolderPrompt { repo_path })
            }
            _ => None,
        }
    }

    /// The groups a bulk action applies to: every marked row's group, or the
    /// cursor row's group when nothing is marked.
    pub(crate) fn action_targets(&self, model: &FrameModel) -> Vec<usize> {
        let marked = self.marked_group_targets(model);
        if !marked.is_empty() {
            return marked;
        }
        match self.cursor_target(model) {
            Some(crate::sidebar::RowTarget::Tab(g, _)) => vec![g],
            _ => Vec::new(),
        }
    }

    /// Marked rows resolved to worktree-group indices (close acts per group).
    /// Marks that aren't worktree rows (e.g. workspace headers) carry no group
    /// target and are dropped here; [`Self::marked_nonworktree_count`] reports
    /// them so the caller can hint the user.
    ///
    /// Deliberately NOT filtered on `r.visible`: marks are identity-keyed and
    /// survive a collapse (`rebuild` keeps them so re-expanding restores the
    /// selection) — a mark inside a collapsed folder/workspace must still act,
    /// or a bulk delete silently skips rows the user explicitly selected. The
    /// confirm modal names every target, so nothing acts sight-unseen.
    fn marked_group_targets(&self, model: &FrameModel) -> Vec<usize> {
        let mut targets: Vec<usize> = model
            .sidebar_rows
            .iter()
            .filter(|r| self.marked.contains(&r.pin_key))
            .filter_map(|r| match r.tab_target {
                Some(crate::sidebar::RowTarget::Tab(g, _)) => Some(g),
                _ => None,
            })
            .collect();
        targets.sort_unstable();
        targets.dedup();
        targets
    }

    /// How many marked rows are *not* worktree groups (workspace headers), which
    /// bulk close/delete can't act on. Used to surface a "N workspaces skipped"
    /// hint rather than silently ignoring them.
    fn marked_nonworktree_count(&self, model: &FrameModel) -> usize {
        model
            .sidebar_rows
            .iter()
            .filter(|r| self.marked.contains(&r.pin_key))
            .filter(|r| !matches!(r.tab_target, Some(crate::sidebar::RowTarget::Tab(_, _))))
            .count()
    }

    /// Warn when a bulk close/delete silently skips marked workspace headers,
    /// which those actions can't operate on (worktrees only).
    fn hint_skipped_workspace_marks(&self, model: &mut FrameModel) {
        let skipped = self.marked_nonworktree_count(model);
        if skipped > 0 {
            model.status =
                format!("{skipped} project(s) skipped — select worktrees to close/delete");
        }
    }

    pub(crate) fn toggle_collapse(
        &mut self,
        model: &mut FrameModel,
        session: &crate::session::Session,
    ) -> SidebarOutcome {
        if let Some(row) = self.selected_row(model) {
            // The derived "Other" group's open state is a per-project
            // override of a config default, not a collapse key.
            if row.kind == crate::sidebar::RowKind::OtherGroup {
                let slug = row.workspace_slug.clone();
                let open = row.collapsed;
                self.set_other_open(&slug, open);
                self.rebuild(model, session);
                return SidebarOutcome::Redraw;
            }
            // A batch row's open state is `batch_open:<key>` (absent =
            // collapsed), keyed on its stable `pin_key`.
            if row.kind == crate::sidebar::RowKind::BatchGroup {
                let key = row.pin_key.clone();
                let open = row.collapsed;
                self.set_batch_open(&key, open);
                self.rebuild(model, session);
                return SidebarOutcome::Redraw;
            }
            // Per-kind collapse key: folders key on their `pin_key`
            // (`{slug}/folder:{id}`), everything else on `workspace_slug`.
            let slug = row.collapse_key().to_string();
            if self.view.collapsed.contains(&slug) {
                self.view.collapsed.remove(&slug);
                // Expanded is the default state: delete the key, don't tombstone.
                self.unpersist(&format!("collapse:{slug}"));
            } else {
                self.view.collapsed.insert(slug.clone());
                self.persist(&format!("collapse:{slug}"), "1");
            }
            self.rebuild(model, session);
        }
        SidebarOutcome::Redraw
    }

    /// Flip the cursor row's "show more" / "show fewer" toggle: expand or
    /// re-truncate its group and persist the choice (`more_open:<group>`).
    /// The rebuild re-seeks the cursor by `pin_key`, and the toggle row's key
    /// is stable across both states, so the cursor stays on it.
    pub(crate) fn toggle_show_more(
        &mut self,
        model: &mut FrameModel,
        session: &crate::session::Session,
    ) -> SidebarOutcome {
        if let Some(group) = self
            .selected_row(model)
            .and_then(crate::sidebar::show_more_group)
            .map(str::to_string)
        {
            let open = !self.view.more_open.contains(&group);
            self.set_more_open(&group, open);
            self.rebuild(model, session);
        }
        SidebarOutcome::Redraw
    }

    pub(crate) fn toggle_pin(
        &mut self,
        model: &mut FrameModel,
        session: &crate::session::Session,
    ) -> SidebarOutcome {
        // Bulk: every marked row's pin key (hidden-by-collapse marks
        // included — see `marked_group_targets`), else the cursor row's.
        let mut keys: Vec<String> = model
            .sidebar_rows
            .iter()
            .filter(|r| self.marked.contains(&r.pin_key))
            .map(|r| r.pin_key.clone())
            .collect();
        if keys.is_empty()
            && let Some(row) = self.selected_row(model)
            // Derived pipeline group/lane keys hold collapse state. Pipeline
            // worktree leaves are real worktree actions and may be pinned
            // within their lane using the path-stable key.
            && row.is_pinnable()
        {
            keys.push(row.pin_key.clone());
        }
        self.toggle_pin_keys(keys, model, session)
    }

    /// Toggle the pin state of these `pin_key`s and persist each flip. Shared
    /// by the bulk `p` path above and the single-row context-menu entry.
    fn toggle_pin_keys(
        &mut self,
        keys: Vec<String>,
        model: &mut FrameModel,
        session: &crate::session::Session,
    ) -> SidebarOutcome {
        for key in keys {
            if key.is_empty() {
                continue;
            }
            if let Some(pos) = self.view.pins.iter().position(|k| *k == key) {
                self.view.pins.remove(pos);
                // Unpinned is the default state: delete the key, don't tombstone.
                self.unpersist(&format!("pin:{key}"));
            } else {
                self.view.pins.push(key.clone());
                self.persist(&format!("pin:{key}"), "1");
            }
        }
        self.rebuild(model, session);
        SidebarOutcome::Redraw
    }

    /// Toggle the flat cross-workspace layout (`g` / context menu): one list
    /// of every worktree across all repos, ordered by the active `s` sort
    /// (Manual keeps workspace order — flat is NOT inherently recency), vs
    /// the per-workspace grouping. Persisted as `sidebar_flat` (grouped is
    /// the default, so the key is deleted rather than tombstoned when off).
    pub(crate) fn toggle_flat(
        &mut self,
        model: &mut FrameModel,
        session: &crate::session::Session,
    ) -> SidebarOutcome {
        self.view.flat = !self.view.flat;
        if self.view.flat {
            self.persist("sidebar_flat", "1");
        } else {
            self.unpersist("sidebar_flat");
        }
        // The row set changes shape (banner + interleaved rows); land at the
        // top so the cursor and scroll window don't dangle past the new list.
        self.cursor = 0;
        self.scroll = 0;
        // Grouped and flat are different sort domains (`sort_groups` vs
        // `sort_groups_flat`), so re-snapshot rather than hold the old one.
        crate::sidebar_freeze::rearm(self, &model.sidebar_status);
        self.rebuild(model, session);
        model.status = if self.view.flat {
            // Honest about the order: flat obeys the active `s` sort (Manual —
            // the default — keeps workspace order), it is NOT always recency.
            format!(
                "Sidebar: flat — all worktrees, {} sort",
                self.view.sort.as_str()
            )
        } else {
            "Sidebar: grouped by project".into()
        };
        SidebarOutcome::Redraw
    }

    /// Cycle how much per-row detail the focused sidebar shows (`i`):
    /// `all` → `cursor` → `off` → `all`.
    ///
    /// This is the runtime toggle for `[ui] sidebar_focus_detail`, which was
    /// previously reachable only by editing config. The choice is held as an
    /// *override* rather than written into `self.view.display`, because a
    /// config reload rebuilds `view.display` wholesale from `[ui]`
    /// (`SidebarDisplay::from_ui`) and would silently discard it.
    pub(crate) fn cycle_focus_detail(&mut self, model: &mut FrameModel) -> SidebarOutcome {
        use thegn_core::config::FocusDetail;
        let next = match self.focus_detail() {
            FocusDetail::All => FocusDetail::Cursor,
            FocusDetail::Cursor => FocusDetail::Off,
            FocusDetail::Off => FocusDetail::All,
        };
        self.focus_detail_override = Some(next);
        self.persist("sidebar_focus_detail", next.as_str());
        model.sidebar_display.focus_detail = next;
        model.status = match next {
            FocusDetail::All => "Sidebar detail: all rows".into(),
            FocusDetail::Cursor => "Sidebar detail: cursor row only".into(),
            FocusDetail::Off => "Sidebar detail: off".into(),
        };
        SidebarOutcome::Redraw
    }

    /// Drop out of the Wide expand back to the resting width (mirrors the
    /// panel's Esc collapse). Returns whether anything changed so the caller can
    /// gate a relayout. Persists "0" so an unfocused bar doesn't re-expand on
    /// restart, matching `adjust_width`'s "drops out of Wide + sticks" rule.
    pub(crate) fn collapse_wide(&mut self) -> bool {
        if !self.expanded {
            return false;
        }
        self.expanded = false;
        self.persist("sidebar_expanded", "0");
        true
    }

    /// Nudge the stored width by `delta` columns, clamped to
    /// [`SIDEBAR_MIN_WIDTH`](crate::layout::SIDEBAR_MIN_WIDTH) and the live
    /// [`sidebar_max_width`](crate::layout::sidebar_max_width) (~half the
    /// window). Returns the width it settled on so the caller can report it.
    pub(crate) fn adjust_width(&mut self, delta: i32) -> usize {
        // A fine nudge drops out of Wide so the change is visible and sticks.
        if self.expanded {
            self.expanded = false;
            self.persist("sidebar_expanded", "0");
        }
        let cur = self
            .width
            .unwrap_or_else(crate::layout::sidebar_default_cols) as i32;
        let next = (cur + delta).clamp(
            crate::layout::SIDEBAR_MIN_WIDTH as i32,
            crate::layout::sidebar_max_width() as i32,
        ) as usize;
        self.width = Some(next);
        self.persist("sidebar_cols", &next.to_string());
        next
    }

    pub(crate) fn run_menu_action(
        &mut self,
        id: &str,
        model: &mut FrameModel,
        session: &crate::session::Session,
    ) -> SidebarOutcome {
        match id {
            "open" => {
                if let Some(t) = self.cursor_target(model) {
                    return SidebarOutcome::Activate(t);
                }
            }
            "toggle" => return self.toggle_collapse(model, session),
            "toggle-flat" => return self.toggle_flat(model, session),
            "cycle-detail" => return self.cycle_focus_detail(model),
            // The menu is anchored to ONE row (the caller re-lands the cursor
            // on it), so pin/close/delete act on that row alone — never the
            // marked set. A menu entry labelled for the row under the pointer
            // must not delete rows marked elsewhere; `Space` + `d`/`p` remain
            // the bulk gestures.
            "pin" => {
                let key = self
                    .selected_row(model)
                    .map(|r| r.pin_key.clone())
                    .filter(|k| !k.is_empty());
                if let Some(key) = key {
                    return self.toggle_pin_keys(vec![key], model, session);
                }
            }
            "close" => {
                if let Some(crate::sidebar::RowTarget::Tab(g, _)) = self.cursor_target(model) {
                    return SidebarOutcome::CloseGroups(vec![g]);
                }
            }
            "delete" => {
                if let Some(crate::sidebar::RowTarget::Tab(g, _)) = self.cursor_target(model) {
                    return SidebarOutcome::DeleteGroups(vec![g]);
                }
            }
            "remove-project" | "remove-workspace" | "delete-folder" | "close-terminal" => {
                if let Some(out) = self.delete_outcome(model, session) {
                    return out;
                }
            }
            "copy-path" => {
                if let Some(p) = self
                    .selected_row(model)
                    .and_then(|r| r.worktree_path.clone())
                {
                    return SidebarOutcome::CopyText(p);
                }
            }
            "info" => return SidebarOutcome::Synthetic(crate::keymap::Action::OpenInfo),
            "open-in-ide" => {
                if let Some(row) = self.selected_row(model)
                    && let Some(path) = row.worktree_path.as_deref()
                {
                    match thegn_core::editor::EditorTarget::project(path) {
                        Ok(target) => {
                            return SidebarOutcome::OpenInIde {
                                target,
                                workspace_slug: row.workspace_slug.clone(),
                            };
                        }
                        Err(error) => model.status = error.to_string(),
                    }
                } else {
                    model.status = "This worktree row has no IDE target".into();
                }
            }
            "fork" => {
                if let Some(out) = self.fork_outcome(model) {
                    return out;
                }
            }
            "rename" | "rename-folder" => {
                if let Some(out) = self.rename_outcome(model, session) {
                    return out;
                }
            }
            "new-worktree" => {
                return self.new_worktree_outcome(model);
            }
            "new-folder" => {
                if let Some(out) = self.folder_outcome(model) {
                    return out;
                }
            }
            "new-terminal" => {
                return SidebarOutcome::Synthetic(crate::keymap::Action::NewTerminal);
            }
            "move-to-folder" => {
                if let Some(out) = self.folder_outcome(model) {
                    return out;
                }
            }
            "sort" => return SidebarOutcome::SortMenu,
            "open-merge-queue" => {
                if let Some(repo_path) = self
                    .selected_row(model)
                    .filter(|row| row.kind == crate::sidebar::RowKind::Workspace)
                    .and_then(|row| row.worktree_path.clone())
                {
                    return SidebarOutcome::OpenMergeQueue { repo_path };
                }
            }
            "mq-add" | "mq-remove" | "mq-land" | "mq-retry" => {
                use crate::handlers::merge_queue::SidebarMq;
                if let Some(path) = self
                    .selected_row(model)
                    .and_then(|r| r.worktree_path.clone())
                {
                    let action = match id {
                        "mq-add" => SidebarMq::Add,
                        "mq-remove" => SidebarMq::Remove,
                        "mq-land" => SidebarMq::Land,
                        _ => SidebarMq::Retry,
                    };
                    return SidebarOutcome::Mq { action, path };
                }
            }
            "mq-add-all" | "mq-clear" | "mq-drain" => {
                use crate::handlers::merge_queue::SidebarMq;
                if let Some(path) = self.cursor_repo_root(model) {
                    let action = match id {
                        "mq-add-all" => SidebarMq::AddAll,
                        "mq-clear" => SidebarMq::Clear,
                        _ => SidebarMq::Drain,
                    };
                    return SidebarOutcome::Mq { action, path };
                }
            }
            _ => {}
        }
        SidebarOutcome::Redraw
    }
}

/// The hydrated model's knowledge of the git repo owning a path.
pub(crate) enum ModelRepoRoot {
    /// A git workspace's main checkout, from the workspace list or a DB worktree row.
    Git(std::path::PathBuf),
    /// A plain-directory workspace: there is no git repo to create a worktree in.
    NotGit,
    /// The model has no row for this path; the caller may fall back to `main_worktree`.
    Unknown,
}

/// Resolve a repo root from hydrated state before considering a git subprocess.
///
/// New-worktree actions run on the compositor loop, where a blocking git call
/// can freeze input and rendering. Use model facts when available; only paths
/// the model does not know may fall back to `main_worktree`.
pub(crate) fn model_repo_root(model: &FrameModel, path: &str) -> ModelRepoRoot {
    if path.is_empty() {
        return ModelRepoRoot::Unknown;
    }

    let workspace_root = |repo_path: &str| {
        model
            .sidebar_workspaces
            .iter()
            .find(|(_, _, _, workspace_path)| workspace_path == repo_path)
            .map(|(_, _, kind, workspace_path)| {
                if kind == "dir" {
                    ModelRepoRoot::NotGit
                } else {
                    ModelRepoRoot::Git(std::path::PathBuf::from(workspace_path))
                }
            })
    };

    if let Some((_, _, kind, repo_path)) = model
        .sidebar_workspaces
        .iter()
        .find(|(_, _, _, repo_path)| !repo_path.is_empty() && repo_path == path)
    {
        return if kind == "dir" {
            ModelRepoRoot::NotGit
        } else {
            ModelRepoRoot::Git(std::path::PathBuf::from(repo_path))
        };
    }

    if let Some(worktree) = model
        .sidebar_db_worktrees
        .iter()
        .find(|worktree| worktree.path == path && !worktree.repo_path.is_empty())
    {
        return workspace_root(&worktree.repo_path)
            .unwrap_or_else(|| ModelRepoRoot::Git(std::path::PathBuf::from(&worktree.repo_path)));
    }

    ModelRepoRoot::Unknown
}

/// Resolve from hydrated state, falling back to git only for unknown paths.
pub(crate) fn resolve_repo_root(
    model: &FrameModel,
    path: &str,
    git: impl FnOnce(&std::path::Path) -> Option<std::path::PathBuf>,
) -> Option<std::path::PathBuf> {
    match model_repo_root(model, path) {
        ModelRepoRoot::Git(path) => Some(path),
        ModelRepoRoot::NotGit => None,
        ModelRepoRoot::Unknown => git(std::path::Path::new(path)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use thegn_core::attention::MqStatus;

    #[test]
    fn model_repo_root_git_workspace() {
        let model = FrameModel {
            sidebar_workspaces: vec![("app".into(), "app".into(), "repo".into(), "/r/app".into())],
            ..FrameModel::default()
        };

        assert!(matches!(
            model_repo_root(&model, "/r/app"),
            ModelRepoRoot::Git(path) if path == std::path::Path::new("/r/app")
        ));
    }

    #[test]
    fn model_repo_root_dir_workspace_is_not_git() {
        let model = FrameModel {
            sidebar_workspaces: vec![(
                "docs".into(),
                "Docs".into(),
                "dir".into(),
                "/r/docs".into(),
            )],
            ..FrameModel::default()
        };
        let git_called = std::cell::Cell::new(false);

        assert!(matches!(
            model_repo_root(&model, "/r/docs"),
            ModelRepoRoot::NotGit
        ));
        assert_eq!(
            resolve_repo_root(&model, "/r/docs", |_| {
                git_called.set(true);
                Some(std::path::PathBuf::from("/unexpected"))
            }),
            None
        );
        assert!(!git_called.get());
    }

    #[test]
    fn model_repo_root_known_worktree_maps_to_its_workspace() {
        let model = FrameModel {
            sidebar_workspaces: vec![("app".into(), "app".into(), "repo".into(), "/r/app".into())],
            sidebar_db_worktrees: vec![crate::sidebar::DbWorktree {
                slug: "app".into(),
                branch: "feat".into(),
                repo_path: "/r/app".into(),
                tab_name: "app/feat".into(),
                path: "/r/app-wt/feat".into(),
                folder_id: None,
                sandbox_backend: None,
                env_name: None,
                env_degraded: false,
                sandbox_profile: None,
                external: false,
            }],
            ..FrameModel::default()
        };
        let git_called = std::cell::Cell::new(false);

        assert!(matches!(
            model_repo_root(&model, "/r/app-wt/feat"),
            ModelRepoRoot::Git(path) if path == std::path::Path::new("/r/app")
        ));
        assert_eq!(
            resolve_repo_root(&model, "/r/app-wt/feat", |_| {
                git_called.set(true);
                Some(std::path::PathBuf::from("/unexpected"))
            }),
            Some(std::path::PathBuf::from("/r/app"))
        );
        assert!(!git_called.get());
    }

    #[test]
    fn resolve_repo_root_falls_back_to_git_only_for_unknown_paths() {
        let model = FrameModel::default();
        let git_calls = std::cell::Cell::new(0);

        assert_eq!(
            resolve_repo_root(&model, "/unknown/path", |path| {
                git_calls.set(git_calls.get() + 1);
                assert_eq!(path, std::path::Path::new("/unknown/path"));
                Some(std::path::PathBuf::from("/git/root"))
            }),
            Some(std::path::PathBuf::from("/git/root"))
        );
        assert_eq!(git_calls.get(), 1);
    }

    #[test]
    fn model_repo_root_ignores_live_fallback_rows() {
        let model = FrameModel {
            sidebar_workspaces: vec![("live".into(), "Live".into(), "repo".into(), String::new())],
            ..FrameModel::default()
        };

        assert!(matches!(
            model_repo_root(&model, "/live/path"),
            ModelRepoRoot::Unknown
        ));
        assert!(matches!(
            model_repo_root(&model, ""),
            ModelRepoRoot::Unknown
        ));
    }

    /// Create a real git repo dir so `main_worktree` (the OLD code path) would
    /// resolve it to a concrete, DIFFERENT value than the hydrated workspace
    /// path — letting the test distinguish new (workspace-first) from old
    /// (git-subprocess-first) behavior.
    fn temp_git_repo(tag: &str) -> std::path::PathBuf {
        let mut dir = std::env::temp_dir();
        let uniq = format!(
            "thegn-sbkeys-{tag}-{}-{:?}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        );
        dir.push(uniq);
        std::fs::create_dir_all(&dir).unwrap();
        let _ = thegn_core::util::git_ok(&dir, &["init", "-q"]); // best-effort: test setup: a failed init fails the asserted git calls below
        dir
    }

    /// A Worktree row's repo root resolves from the already-hydrated workspace
    /// list — NOT a loop-side `git rev-parse`. Regression for the event-loop-
    /// blocking subprocess: `worktree_path` points at a REAL git repo, so the
    /// OLD code (git-first) would resolve it to that repo's main-worktree path;
    /// the fixed code must instead return the hydrated workspace path without
    /// consulting git at all.
    #[test]
    #[allow(clippy::field_reassign_with_default)]
    fn cursor_repo_root_uses_hydrated_workspace_path() {
        use crate::sidebar::{RowKind, SidebarRow};
        let repo = temp_git_repo("cursor");
        let mut model = FrameModel::default();
        model.sidebar_workspaces = vec![(
            "myrepo".to_string(),
            "myrepo".to_string(),
            "git".to_string(),
            "/hydrated/root".to_string(),
        )];
        let mut row = SidebarRow::base(RowKind::Worktree, 1, "feature", "myrepo");
        row.worktree_path = Some(repo.to_string_lossy().into_owned());
        model.sidebar_rows = vec![row];

        let sb = SidebarState::default();
        assert_eq!(
            sb.cursor_repo_root(&model),
            Some("/hydrated/root".to_string()),
            "repo root must come from the hydrated workspace, not a git subprocess \
             on the loop (old code would return the git-resolved main worktree)",
        );
        let _ = std::fs::remove_dir_all(&repo); // best-effort: cleanup: the target may already be gone; a failed removal never fails the caller
    }

    /// The fork outcome resolves its repo root from the hydrated workspace list
    /// too (same no-git-on-loop rule), keeping the base branch. `worktree_path`
    /// is a REAL repo so the old git-first path would have returned its
    /// main-worktree instead of the hydrated value.
    #[test]
    #[allow(clippy::field_reassign_with_default)]
    fn fork_outcome_uses_hydrated_workspace_path() {
        use crate::sidebar::{RowKind, SidebarRow};
        let repo = temp_git_repo("fork");
        let mut model = FrameModel::default();
        model.sidebar_workspaces = vec![(
            "myrepo".to_string(),
            "myrepo".to_string(),
            "git".to_string(),
            "/hydrated/root".to_string(),
        )];
        let mut row = SidebarRow::base(RowKind::Worktree, 1, "feature", "myrepo");
        row.worktree_path = Some(repo.to_string_lossy().into_owned());
        row.branch = Some("feature".to_string());
        model.sidebar_rows = vec![row];

        let sb = SidebarState::default();
        match sb.fork_outcome(&model) {
            Some(SidebarOutcome::Fork {
                base_branch,
                repo_root,
            }) => {
                assert_eq!(base_branch, "feature");
                assert_eq!(
                    repo_root, "/hydrated/root",
                    "fork repo root must come from the hydrated workspace, not git",
                );
            }
            _ => panic!("expected Fork outcome"),
        }
        let _ = std::fs::remove_dir_all(&repo); // best-effort: cleanup: the target may already be gone; a failed removal never fails the caller
    }

    /// A Workspace row whose `sidebar_workspaces` entry has an empty repo path
    /// resolves to `None` — the precondition for `new_worktree_outcome`
    /// returning a refusal and for `NewWorktreeTarget::Refuse`.
    #[test]
    fn cursor_repo_root_is_none_for_a_live_fallback_workspace_row() {
        use crate::sidebar::{RowKind, SidebarRow};
        let mut model = FrameModel {
            sidebar_workspaces: vec![(
                "live-fb".to_string(),
                "live-fb".to_string(),
                "git".to_string(),
                "".to_string(),
            )],
            ..Default::default()
        };
        // Empty repo_path (4th tuple element) simulates a live fallback whose
        // registry heal also failed.
        let row = SidebarRow::base(RowKind::Workspace, 1, "live-fb", "live-fb");
        model.sidebar_rows = vec![row];

        let sb = SidebarState::default();
        assert_eq!(
            sb.cursor_repo_root(&model),
            None,
            "cursor_repo_root must be None for a live-fallback workspace with empty repo path"
        );
    }

    /// Pressing the new-worktree key on a live-fallback workspace row MUST
    /// set a non-empty status message and return `Redraw` — it must NOT
    /// return `Synthetic(Action::NewWorktree)` (which would silently cross
    /// workspaces via the old active-group fallback).
    #[test]
    fn new_worktree_key_refuses_a_live_fallback_workspace_row() {
        use crate::sidebar::{RowKind, SidebarRow};
        let mut model = FrameModel {
            sidebar_workspaces: vec![(
                "live-fb".to_string(),
                "live-fb".to_string(),
                "git".to_string(),
                "".to_string(),
            )],
            ..Default::default()
        };
        let row = SidebarRow::base(RowKind::Workspace, 1, "live-fb", "live-fb");
        model.sidebar_rows = vec![row];

        let sb = SidebarState::default();
        let outcome = sb.new_worktree_outcome(&mut model);
        assert!(
            !model.status.is_empty(),
            "model.status must be non-empty after refusal"
        );
        // The return type must be Redraw — never Synthetic(NewWorktree).
        match outcome {
            SidebarOutcome::Redraw => {}
            _ => panic!("expected Redraw from new_worktree_outcome on unresolvable row"),
        }
    }

    /// "New worktree here…" carries the cursor row's folder so the created
    /// worktree is filed there: a folder header names itself, a filed worktree
    /// names its folder, and a loose worktree carries none.
    #[test]
    fn new_worktree_outcome_carries_the_cursor_rows_folder() {
        use crate::sidebar::{DbWorktree, RowKind, SidebarRow};
        let mut model = FrameModel {
            sidebar_workspaces: vec![(
                "myrepo".to_string(),
                "myrepo".to_string(),
                "git".to_string(),
                "/repo".to_string(),
            )],
            sidebar_db_folders: vec![thegn_core::models::FolderRow {
                folder_id: 10,
                repo_path: "/repo".into(),
                name: "Waiting".into(),
                position: 0,
                created_at: 0,
            }],
            sidebar_db_worktrees: ["filed", "loose"]
                .iter()
                .map(|label| DbWorktree {
                    slug: "myrepo".into(),
                    branch: (*label).into(),
                    repo_path: "/repo".into(),
                    tab_name: format!("myrepo/{label}"),
                    path: format!("/wt/{label}"),
                    folder_id: (*label == "filed").then_some(10),
                    sandbox_backend: None,
                    env_name: None,
                    env_degraded: false,
                    sandbox_profile: None,
                    external: false,
                })
                .collect(),
            ..Default::default()
        };
        let sb = SidebarState::default();
        let folder_of = |model: &mut FrameModel, row: SidebarRow| {
            model.sidebar_rows = vec![row];
            match sb.new_worktree_outcome(model) {
                SidebarOutcome::NewWorktreeIn { repo_root, folder } => {
                    assert_eq!(repo_root, "/repo");
                    folder.map(|intent| intent.name)
                }
                _ => panic!("expected NewWorktreeIn"),
            }
        };

        let mut header = SidebarRow::base(RowKind::Folder, 1, "Waiting", "myrepo");
        header.folder_id = Some(10);
        assert_eq!(folder_of(&mut model, header), Some("Waiting".into()));

        let mut filed = SidebarRow::base(RowKind::Worktree, 2, "filed", "myrepo");
        filed.worktree_path = Some("/wt/filed".into());
        assert_eq!(folder_of(&mut model, filed), Some("Waiting".into()));

        let mut loose = SidebarRow::base(RowKind::Worktree, 1, "loose", "myrepo");
        loose.worktree_path = Some("/wt/loose".into());
        assert_eq!(folder_of(&mut model, loose), None);
    }

    /// `NewWorktreeTarget` mapping: focus/selection semantics, and defensive
    /// terminals-row handling.
    #[test]
    fn new_worktree_target_maps_focus_rows_and_fallbacks() {
        use crate::sidebar::{RowKind, SidebarRow};

        // Resolvable workspace row => Root
        let mut model = FrameModel {
            sidebar_workspaces: vec![(
                "ok".to_string(),
                "ok".to_string(),
                "git".to_string(),
                "/repo".to_string(),
            )],
            ..Default::default()
        };
        let mut row = SidebarRow::base(RowKind::Workspace, 0, "ok", "ok");
        row.worktree_path = Some("/repo".to_string());
        model.sidebar_rows = vec![row];
        let sb = SidebarState::default();
        match sb.new_worktree_target(&model, true) {
            crate::handlers::sidebar_keys::NewWorktreeTarget::Root(p) => {
                assert_eq!(p, "/repo");
            }
            other => panic!("expected Root for focused resolvable workspace, got {other:?}"),
        }

        // Unresolvable workspace row => Refuse
        let model2 = FrameModel {
            sidebar_workspaces: vec![(
                "live-fb".to_string(),
                "live-fb".to_string(),
                "git".to_string(),
                "".to_string(),
            )],
            sidebar_rows: vec![SidebarRow::base(
                RowKind::Workspace,
                1,
                "live-fb",
                "live-fb",
            )],
            ..Default::default()
        };
        match sb.new_worktree_target(&model2, true) {
            crate::handlers::sidebar_keys::NewWorktreeTarget::Refuse(msg) => {
                assert!(!msg.is_empty());
            }
            other => panic!("expected Refuse for focused unresolvable row, got {other:?}"),
        }

        // No sidebar focus => ActiveFallback
        match sb.new_worktree_target(&model, false) {
            crate::handlers::sidebar_keys::NewWorktreeTarget::ActiveFallback => {}
            other => panic!("expected ActiveFallback when sidebar_focus=false, got {other:?}"),
        }

        // Terminals row => ActiveFallback (defensive)
        let model_terms = FrameModel {
            sidebar_rows: vec![SidebarRow::base(
                RowKind::SectionHeading,
                0,
                "TERMINALS",
                "terminals",
            )],
            ..Default::default()
        };
        match sb.new_worktree_target(&model_terms, true) {
            crate::handlers::sidebar_keys::NewWorktreeTarget::ActiveFallback => {}
            other => panic!("expected ActiveFallback for terminals row, got {other:?}"),
        }
    }

    fn ids(status: Option<MqStatus>) -> Vec<&'static str> {
        worktree_mq_entries(status)
            .into_iter()
            .map(|(id, _)| id)
            .collect()
    }

    #[test]
    fn not_queued_offers_only_add() {
        assert_eq!(ids(None), vec!["mq-add"]);
    }

    #[test]
    fn queued_offers_remove_without_land_or_retry() {
        for s in [MqStatus::Queued, MqStatus::Folding, MqStatus::Verifying] {
            assert_eq!(ids(Some(s)), vec!["mq-remove"], "{s:?}");
        }
    }

    #[test]
    fn ready_adds_land() {
        assert_eq!(ids(Some(MqStatus::Ready)), vec!["mq-remove", "mq-land"]);
    }

    #[test]
    fn blocked_statuses_add_retry() {
        for s in [
            MqStatus::Deferred,
            MqStatus::GateFailed,
            MqStatus::GateError,
            MqStatus::AgentBlocked,
            MqStatus::NeedsHuman,
        ] {
            assert_eq!(ids(Some(s)), vec!["mq-remove", "mq-retry"], "{s:?}");
        }
    }

    #[test]
    fn landed_and_agent_running_offer_only_remove() {
        // Landed rows are shown in the panel, not the sidebar chip, but if the
        // status is present the menu still offers a plain remove.
        for s in [MqStatus::Landed, MqStatus::AgentRunning] {
            assert_eq!(ids(Some(s)), vec!["mq-remove"], "{s:?}");
        }
    }

    #[test]
    fn workspace_menu_opens_merge_queue_without_a_new_keybind() {
        use crate::sidebar::{RowKind, SidebarRow};

        let mut model = FrameModel {
            sidebar_rows: vec![SidebarRow {
                worktree_path: Some("/repos/app".into()),
                pin_key: "app".into(),
                ..SidebarRow::base(RowKind::Workspace, 0, "app", "app")
            }],
            ..Default::default()
        };
        let sb = SidebarState::default();
        let menu = sb
            .menu_for_cursor(&model, &crate::session::Session::default())
            .expect("workspace menu");
        assert!(
            menu.entries
                .iter()
                .any(|entry| entry.id == "open-merge-queue")
        );
        assert!(menu.entries.iter().any(|entry| entry.id == "info"));

        let mut sb = sb;
        assert!(matches!(
            sb.run_menu_action("info", &mut model, &crate::session::Session::default(),),
            SidebarOutcome::Synthetic(crate::keymap::Action::OpenInfo)
        ));
        let out = sb.run_menu_action(
            "open-merge-queue",
            &mut model,
            &crate::session::Session::default(),
        );
        assert!(matches!(
            out,
            SidebarOutcome::OpenMergeQueue { ref repo_path }
                if repo_path == "/repos/app"
        ));
    }

    #[test]
    fn worktree_menu_hands_the_selected_path_to_the_core_target_policy() {
        use crate::sidebar::{RowKind, SidebarRow};

        let mut model = FrameModel {
            sidebar_rows: vec![SidebarRow {
                worktree_path: Some("/repos/app-feature".into()),
                pin_key: "app-feature".into(),
                ..SidebarRow::base(RowKind::Worktree, 1, "feature", "app")
            }],
            ..Default::default()
        };
        let mut sb = SidebarState::default();
        let menu = sb
            .menu_for_cursor(&model, &crate::session::Session::default())
            .expect("worktree menu");
        assert!(menu.entries.iter().any(|entry| entry.id == "open-in-ide"));
        assert!(menu.entries.iter().any(|entry| entry.id == "info"));

        assert!(matches!(
            sb.run_menu_action(
                "open-in-ide",
                &mut model,
                &crate::session::Session::default(),
            ),
            SidebarOutcome::OpenInIde { target, workspace_slug }
                if target.worktree() == std::path::Path::new("/repos/app-feature")
                    && workspace_slug == "app"
        ));
    }

    #[test]
    fn pipeline_worktree_menu_keeps_regular_worktree_actions() {
        use crate::sidebar::{RowKind, RowTarget, SidebarRow};

        let model = FrameModel {
            sidebar_workspaces: vec![(
                "app".into(),
                "app".into(),
                "repo".into(),
                "/repos/app".into(),
            )],
            sidebar_rows: vec![
                SidebarRow {
                    worktree_path: Some("/repos/app".into()),
                    pin_key: "app".into(),
                    ..SidebarRow::base(RowKind::Workspace, 0, "app", "app")
                },
                SidebarRow {
                    worktree_path: Some("/repos/app-pipeline".into()),
                    pin_key: "app/app-pipeline".into(),
                    branch: Some("feature".into()),
                    tab_target: Some(RowTarget::Tab(0, 0)),
                    ..SidebarRow::base(RowKind::PipelineWorktree, 3, "app-pipeline", "app")
                },
            ],
            ..Default::default()
        };
        let mut sb = SidebarState::default();
        sb.cursor = model
            .sidebar_rows
            .iter()
            .filter(|row| row.visible)
            .position(|row| {
                row.kind == RowKind::PipelineWorktree && row.pin_key == "app/app-pipeline"
            })
            .expect("pipeline leaf is visible");
        assert_eq!(
            sb.selected_row(&model).map(|row| row.pin_key.as_str()),
            Some("app/app-pipeline")
        );
        let menu = sb
            .menu_for_cursor(&model, &crate::session::Session::default())
            .expect("pipeline worktree menu");
        for id in [
            "open",
            "open-in-ide",
            "fork",
            "move-to-folder",
            "copy-path",
            "info",
        ] {
            assert!(
                menu.entries.iter().any(|entry| entry.id == id),
                "missing {id}"
            );
        }
        assert!(menu.entries.iter().any(|entry| entry.id == "pin"));
        assert!(matches!(
            sb.folder_outcome(&model),
            Some(SidebarOutcome::MoveToFolder {
                worktree_path,
                repo_path,
            }) if worktree_path == "/repos/app-pipeline" && repo_path == "/repos/app"
        ));
    }

    /// The "show more" row toggles on Enter and on Space, `j` lands on it,
    /// and the cursor stays on it across both toggles (the rebuild re-seeks
    /// it by its stable `pin_key`). The toggle persists, so isolate
    /// `XDG_STATE_HOME`.
    #[test]
    fn show_more_toggles_on_enter_and_space_and_keeps_the_cursor() {
        use crate::session::{GroupKind, Session, WorktreeGroup};
        let _state = crate::testenv::IsolatedState::new();
        let worktrees = (0..21)
            .map(|i| {
                let kind = if i == 0 {
                    GroupKind::Home
                } else {
                    GroupKind::Branch
                };
                WorktreeGroup::new(format!("app/b{i:02}"), kind, format!("/tmp/app-b{i:02}"))
            })
            .collect();
        let session = Session {
            id: "s1".into(),
            worktrees,
            active: 0,
        };
        let mut model = crate::hydrate::build_initial_model(&session, None);
        model.sidebar_workspaces = vec![("app".into(), "app".into(), "repo".into(), String::new())];
        let mut sb = SidebarState {
            focused: true,
            ..Default::default()
        };
        sb.view.display.group_row_limit = 15;
        sb.rebuild(&mut model, &session);

        let more_key = crate::sidebar::show_more_key("app");
        let visible_kinds = |model: &FrameModel| -> Vec<crate::sidebar::RowKind> {
            model
                .sidebar_rows
                .iter()
                .filter(|r| r.visible)
                .map(|r| r.kind)
                .collect()
        };
        let more_idx = |model: &FrameModel| {
            model
                .sidebar_rows
                .iter()
                .filter(|r| r.visible)
                .position(|r| r.pin_key == more_key)
                .expect("the show-more row is visible")
        };
        let worktrees_shown = |model: &FrameModel| {
            visible_kinds(model)
                .iter()
                .filter(|k| **k == crate::sidebar::RowKind::Worktree)
                .count()
        };
        assert_eq!(worktrees_shown(&model), 15);

        // `j` from the last shown entry lands on the toggle row.
        let idx = more_idx(&model);
        sb.cursor = idx - 1;
        sb.handle_key(&KeyCode::Char('j'), Modifiers::NONE, &mut model, &session);
        assert_eq!(sb.cursor, idx);

        // Enter expands; the cursor follows the row to its new slot.
        sb.handle_key(&KeyCode::Enter, Modifiers::NONE, &mut model, &session);
        assert!(sb.view.more_open.contains("app"));
        assert_eq!(worktrees_shown(&model), 21);
        assert_eq!(sb.cursor, more_idx(&model), "cursor stays on the toggle");
        assert!(
            !sb.selected_row(&model).unwrap().collapsed,
            "now reads `show fewer`"
        );

        // Space truncates again, and the cursor is still on the toggle.
        sb.handle_key(&KeyCode::Char(' '), Modifiers::NONE, &mut model, &session);
        assert!(!sb.view.more_open.contains("app"));
        assert_eq!(worktrees_shown(&model), 15);
        assert_eq!(sb.cursor, more_idx(&model));
        assert!(sb.marked.is_empty(), "Space toggles, it does not mark");
        assert!(sb.menu_for_cursor(&model, &session).is_none());
    }

    /// Home plus three `b158` lanes: a collapsed-by-default batch row.
    fn batch_fixture() -> (crate::session::Session, FrameModel, SidebarState) {
        use crate::session::{GroupKind, Session, WorktreeGroup};
        let mut worktrees = vec![WorktreeGroup::new(
            "app/main".to_string(),
            GroupKind::Home,
            "/tmp/app-main".to_string(),
        )];
        for n in ["a", "b", "c"] {
            worktrees.push(WorktreeGroup::new(
                format!("app/b158-{n}"),
                GroupKind::Branch,
                format!("/tmp/app-b158-{n}"),
            ));
        }
        let session = Session {
            id: "s1".into(),
            worktrees,
            active: 0,
        };
        let mut model = crate::hydrate::build_initial_model(&session, None);
        model.sidebar_workspaces = vec![("app".into(), "app".into(), "repo".into(), String::new())];
        let mut sb = SidebarState {
            focused: true,
            ..Default::default()
        };
        sb.view.display.batch_group_min = 2;
        sb.rebuild(&mut model, &session);
        (session, model, sb)
    }

    fn batch_idx(model: &FrameModel) -> usize {
        model
            .sidebar_rows
            .iter()
            .filter(|r| r.visible)
            .position(|r| r.kind == crate::sidebar::RowKind::BatchGroup)
            .expect("a visible batch row")
    }

    #[test]
    fn enter_on_a_batch_row_opens_then_closes_it() {
        let _state = crate::testenv::IsolatedState::new();
        let (session, mut model, mut sb) = batch_fixture();
        let idx = batch_idx(&model);
        sb.cursor = idx;
        assert!(sb.selected_row(&model).unwrap().collapsed);
        let key = sb.selected_row(&model).unwrap().pin_key.clone();

        sb.handle_key(&KeyCode::Enter, Modifiers::NONE, &mut model, &session);
        assert!(sb.view.batch_open.contains(&key));
        assert_eq!(
            sb.cursor,
            batch_idx(&model),
            "cursor stays on the batch row"
        );
        assert!(!sb.selected_row(&model).unwrap().collapsed);

        sb.handle_key(&KeyCode::Enter, Modifiers::NONE, &mut model, &session);
        assert!(!sb.view.batch_open.contains(&key));
        assert!(sb.selected_row(&model).unwrap().collapsed);
    }

    #[test]
    fn left_on_a_member_collapses_its_batch_and_lands_on_it() {
        let _state = crate::testenv::IsolatedState::new();
        let (session, mut model, mut sb) = batch_fixture();
        sb.cursor = batch_idx(&model);
        sb.handle_key(&KeyCode::Enter, Modifiers::NONE, &mut model, &session);
        let idx = batch_idx(&model);
        sb.cursor = idx + 1;
        assert_eq!(
            sb.selected_row(&model).unwrap().kind,
            crate::sidebar::RowKind::Worktree
        );

        sb.handle_key(&KeyCode::LeftArrow, Modifiers::NONE, &mut model, &session);
        assert!(sb.view.batch_open.is_empty());
        assert_eq!(sb.cursor, batch_idx(&model));
        assert!(sb.selected_row(&model).unwrap().collapsed);
    }

    #[test]
    fn the_menu_offers_only_collapse_expand_on_a_batch_row() {
        let _state = crate::testenv::IsolatedState::new();
        let (session, model, mut sb) = batch_fixture();
        sb.cursor = batch_idx(&model);
        let menu = sb.menu_for_cursor(&model, &session).expect("batch menu");
        let ids: Vec<&str> = menu
            .entries
            .iter()
            .filter(|e| !e.is_separator())
            .map(|e| e.id.as_str())
            .collect();
        assert_eq!(ids, vec!["toggle"]);
    }
}

#[cfg(test)]
mod selected_project_removal_tests {
    use super::*;

    /// The palette's "Remove project" must act on the project the sidebar
    /// cursor is in, never the first (top) project or the active one.
    #[test]
    fn resolves_the_project_owning_the_cursor_row() {
        let _state = crate::testenv::IsolatedState::new();
        let session = crate::session::Session::default();
        let mut model = crate::hydrate::build_initial_model(&session, None);
        model.sidebar_workspaces = vec![
            (
                "alpha".into(),
                "alpha".into(),
                "repo".into(),
                "/r/alpha".into(),
            ),
            (
                "beta".into(),
                "beta".into(),
                "repo".into(),
                "/r/beta".into(),
            ),
            (
                "gamma".into(),
                "gamma".into(),
                "repo".into(),
                "/r/gamma".into(),
            ),
        ];
        let mut sb = SidebarState {
            focused: true,
            ..Default::default()
        };
        sb.rebuild(&mut model, &session);
        let visible: Vec<_> = model.sidebar_rows.iter().filter(|r| r.visible).collect();
        let at = |kind, slug: &str| {
            visible
                .iter()
                .position(|r| r.kind == kind && r.workspace_slug == slug)
                .unwrap()
        };
        let target = |sb: &SidebarState| match sb.selected_project_removal(&model) {
            Some(SidebarOutcome::RemoveWorkspace {
                repo_path, slug, ..
            }) => Some((repo_path, slug)),
            _ => None,
        };
        use crate::sidebar::RowKind;
        // On the project row itself.
        sb.cursor = at(RowKind::Workspace, "beta");
        assert_eq!(target(&sb), Some(("/r/beta".into(), "beta".into())));
        // On a worktree under the project.
        sb.cursor = at(RowKind::Worktree, "gamma");
        assert_eq!(target(&sb), Some(("/r/gamma".into(), "gamma".into())));
        // Outside any project (terminals section): fall back to the caller.
        sb.cursor = visible
            .iter()
            .position(|r| r.kind == RowKind::SectionHeading)
            .unwrap();
        assert_eq!(target(&sb), None);
    }
}
