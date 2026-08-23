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
    /// workspace, which need not be the active one).
    NewWorktreeIn { repo_root: String },
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
                    | MqStatus::NeedsHuman
            ) {
                v.push(("mq-retry", "Retry"));
            }
            v
        }
    }
}

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
            crate::sidebar::RowKind::Worktree => {
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

    /// Whether the cursor row lives in the TERMINALS region (the banner, a host
    /// group, a terminal leaf, or the empty hint).
    pub(crate) fn cursor_in_terminals(&self, model: &FrameModel) -> bool {
        self.selected_row(model)
            .map(|r| r.workspace_slug == "terminals" || r.workspace_slug.starts_with("terminals/"))
            .unwrap_or(false)
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
            RowKind::Terminal => Some(SidebarOutcome::CloseTerminal {
                name: row.label.clone(),
            }),
            RowKind::Worktree => {
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
                            "Open workspace \"{slug}\" first to close or delete this worktree"
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
            RowKind::Worktree => {
                if row.tab_target.is_some() {
                    entries.push(e("open", "Open", Some(chord_of(Id::Activate))));
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
                if !row.pin_key.is_empty() {
                    entries.push(e("pin", "Pin / unpin", Some(chord_of(Id::TogglePin))));
                }
                if row.worktree_path.is_some() {
                    entries.push(e("copy-path", "Copy path", Some(chord_of(Id::CopyPath))));
                }
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
                            "Delete branch + files…",
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
                // Workspace-wide merge-queue controls (panel `A` / clear / `D`).
                entries.push(sep());
                entries.push(e("mq-add-all", "Queue all worktrees", None));
                entries.push(e("mq-clear", "Clear merge queue", None));
                entries.push(e("mq-drain", "Drain merge queue", None));
                if row.worktree_path.is_some() {
                    entries.push(sep());
                    entries.push(
                        e(
                            "remove-workspace",
                            "Remove workspace…",
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
            RowKind::SectionHeading | RowKind::EmptyHint => return None,
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
                            crate::sidebar::RowKind::Worktree | crate::sidebar::RowKind::Terminal
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
            Id::Mark => {
                // Multi-select toggle (item 26): mark/unmark the cursor row if it
                // is a worktree or workspace. Collapse now lives solely on
                // Enter/←/→ and the caret click, so headers can be selected too.
                if let Some(row) = self.selected_row(model) {
                    if row.is_markable() {
                        let key = row.pin_key.clone();
                        if !self.marked.remove(&key) {
                            self.marked.insert(key);
                        }
                        self.sync(model);
                    } else {
                        model.status = "Only worktrees and workspaces can be marked".into();
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
                if self.cursor_in_terminals(model) {
                    return SidebarOutcome::Synthetic(crate::keymap::Action::NewTerminal);
                }
                return match self.cursor_repo_root(model) {
                    Some(repo_root) => SidebarOutcome::NewWorktreeIn { repo_root },
                    None => SidebarOutcome::Synthetic(crate::keymap::Action::NewWorktree),
                };
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
                model.status = "Folders apply to worktree and workspace rows".into();
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
                    return SidebarOutcome::Relayout;
                }
                return self.adjust_width(if id == Id::WidthDec { -2 } else { 2 });
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
            RowKind::Worktree => {
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
                    model.status =
                        format!("Open workspace \"{slug}\" first to rename this worktree");
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
            RowKind::Worktree => {
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
                format!("{skipped} workspace(s) skipped — select worktrees to close/delete");
        }
    }

    pub(crate) fn toggle_collapse(
        &mut self,
        model: &mut FrameModel,
        session: &crate::session::Session,
    ) -> SidebarOutcome {
        if let Some(row) = self.selected_row(model) {
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
        self.rebuild(model, session);
        model.status = if self.view.flat {
            // Honest about the order: flat obeys the active `s` sort (Manual —
            // the default — keeps workspace order), it is NOT always recency.
            format!(
                "Sidebar: flat — all worktrees, {} sort",
                self.view.sort.as_str()
            )
        } else {
            "Sidebar: grouped by workspace".into()
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

    pub(crate) fn adjust_width(&mut self, delta: i32) -> SidebarOutcome {
        // A fine nudge drops out of Wide so the change is visible and sticks.
        if self.expanded {
            self.expanded = false;
            self.persist("sidebar_expanded", "0");
        }
        let cur = self.width.unwrap_or(crate::layout::SIDEBAR_COLS) as i32;
        let next = (cur + delta).clamp(
            crate::layout::SIDEBAR_MIN_WIDTH as i32,
            crate::layout::SIDEBAR_MAX_WIDTH as i32,
        ) as usize;
        self.width = Some(next);
        self.persist("sidebar_cols", &next.to_string());
        SidebarOutcome::Relayout
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
            "remove-workspace" | "delete-folder" | "close-terminal" => {
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
                return match self.cursor_repo_root(model) {
                    Some(repo_root) => SidebarOutcome::NewWorktreeIn { repo_root },
                    None => SidebarOutcome::Synthetic(crate::keymap::Action::NewWorktree),
                };
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

#[cfg(test)]
mod tests {
    use super::*;
    use thegn_core::attention::MqStatus;

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
        let _ = thegn_core::util::git_ok(&dir, &["init", "-q"]);
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
        let _ = std::fs::remove_dir_all(&repo);
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
        let _ = std::fs::remove_dir_all(&repo);
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
}
