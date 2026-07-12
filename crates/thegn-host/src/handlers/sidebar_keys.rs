//! Sidebar key handling + the row context menu: everything that happens while
//! the sidebar owns keyboard focus. Extracted from ratchet-pinned `run.rs`;
//! the loop dispatches on the returned [`SidebarOutcome`].
//!
//! ## The sidebar key surface
//!
//! | key            | action                                                  |
//! |----------------|---------------------------------------------------------|
//! | j/k/↑/↓        | move cursor                                             |
//! | Enter          | open row / toggle header / run an EmptyHint's action    |
//! | h/l/←/→        | collapse/expand (h on a leaf folds its parent)          |
//! | `/`            | filter                                                  |
//! | Space          | mark for bulk actions                                   |
//! | m              | context menu (the canonical action catalog)             |
//! | d / Delete     | close-or-delete chooser (row-kind aware, bulk-aware)    |
//! | r / F2         | rename (worktree branch, folder)                        |
//! | n              | new worktree here (terminals region: new terminal)      |
//! | N              | new workspace                                           |
//! | b              | branch a new worktree from this one                     |
//! | f              | move to folder… (workspace/folder row: new folder…)     |
//! | c              | copy path                                               |
//! | p              | pin / unpin                                             |
//! | s              | sort menu                                               |
//! | Shift+↑/↓      | reorder selection                                       |
//! | `<`/`>`, e     | resize / wide toggle                                    |
//! | ?              | help overlay                                            |
//! | q / Esc        | back to the terminal                                    |

use termwiz::input::{KeyCode, Modifiers};

use crate::chrome::FrameModel;
use crate::handlers::sidebar_persist::SidebarState;
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
            crate::sidebar::RowKind::Worktree => row
                .worktree_path
                .as_deref()
                .and_then(|p| {
                    thegn_core::repo::main_worktree(std::path::Path::new(p))
                        .map(|p| p.to_string_lossy().into_owned())
                })
                .or_else(|| Self::workspace_repo_path(model, &row.workspace_slug)),
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
                let targets = self.action_targets(model);
                if targets.is_empty() {
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
                    entries.push(e("open", "Open", Some("↵")));
                }
                entries.push(sep());
                entries.push(e("new-worktree", "New worktree here…", Some("n")));
                if row.worktree_path.is_some() {
                    entries.push(e("fork", "Branch from this…", Some("b")));
                }
                let is_home = self.cursor_is_home(model, session);
                if !is_home {
                    entries.push(e("rename", "Rename…", Some("r")));
                }
                entries.push(sep());
                if row.worktree_path.is_some() {
                    entries.push(e("move-to-folder", "Move to folder…", Some("f")));
                }
                if !row.pin_key.is_empty() {
                    entries.push(e("pin", "Pin / unpin", Some("p")));
                }
                if row.worktree_path.is_some() {
                    entries.push(e("copy-path", "Copy path", Some("c")));
                }
                if !is_home {
                    entries.push(sep());
                    entries.push(e("close", "Close — keep files on disk", None));
                    entries.push(e("delete", "Delete branch + files…", Some("d")).danger());
                }
            }
            RowKind::Workspace => {
                // Enter toggles collapse on headers, so "Open" carries no chip.
                if row.tab_target.is_some() {
                    entries.push(e("open", "Open", None));
                }
                entries.push(e("toggle", "Collapse / expand", Some("↵")));
                entries.push(sep());
                entries.push(e("new-worktree", "New worktree…", Some("n")));
                entries.push(e("new-folder", "New folder…", Some("f")));
                if !row.pin_key.is_empty() {
                    entries.push(e("pin", "Pin / unpin", Some("p")));
                }
                entries.push(e("sort", "Sort worktrees by…", Some("s")));
                if row.worktree_path.is_some() {
                    entries.push(sep());
                    entries.push(e("remove-workspace", "Remove workspace…", Some("d")).danger());
                }
            }
            RowKind::Folder => {
                entries.push(e("toggle", "Collapse / expand", Some("↵")));
                entries.push(e("rename-folder", "Rename folder…", Some("r")));
                entries.push(e("new-worktree", "New worktree here…", Some("n")));
                entries.push(sep());
                entries.push(
                    e(
                        "delete-folder",
                        "Delete folder (keeps worktrees)",
                        Some("d"),
                    )
                    .danger(),
                );
            }
            RowKind::TerminalHost => {
                entries.push(e("toggle", "Collapse / expand", Some("↵")));
                entries.push(e("new-terminal", "New terminal here…", Some("n")));
            }
            RowKind::Terminal => {
                if row.tab_target.is_some() {
                    entries.push(e("open", "Open", Some("↵")));
                }
                if !row.pin_key.is_empty() {
                    entries.push(e("pin", "Pin / unpin", Some("p")));
                }
                entries.push(e("new-terminal", "New terminal…", Some("n")));
                entries.push(sep());
                entries.push(e("close-terminal", "Close terminal…", Some("d")).danger());
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
            match key {
                key if crate::input::is_escape_key(key) => {
                    self.filtering = false;
                    self.view.filter.clear();
                }
                KeyCode::Enter => self.filtering = false,
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
                        if let Some(idx) = model
                            .sidebar_rows
                            .iter()
                            .filter(|r| r.visible)
                            .position(|r| r.pin_key == target_key)
                        {
                            self.cursor = idx;
                        }
                        return self.run_menu_action(&id, model, session);
                    }
                }
                _ => {}
            }
            self.sync(model);
            return SidebarOutcome::Redraw;
        }

        let visible = Self::visible_len(model);
        match key {
            key if crate::input::is_escape_key(key) => return SidebarOutcome::Defocus,
            KeyCode::Char('q') => return SidebarOutcome::Defocus,
            // Shift+↑/↓ reorders the selection (the loop has `&mut Session`).
            // Only the arrows carry Shift here — Shift+j/k normalise to J/K.
            KeyCode::UpArrow if mods.contains(Modifiers::SHIFT) => {
                return SidebarOutcome::ReorderSelection { up: true };
            }
            KeyCode::DownArrow if mods.contains(Modifiers::SHIFT) => {
                return SidebarOutcome::ReorderSelection { up: false };
            }
            KeyCode::DownArrow | KeyCode::Char('j') => {
                if visible > 0 {
                    self.cursor = (self.cursor + 1).min(visible - 1);
                }
            }
            KeyCode::UpArrow | KeyCode::Char('k') => {
                self.cursor = self.cursor.saturating_sub(1);
            }
            KeyCode::Enter => {
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
            KeyCode::Char('l') | KeyCode::RightArrow => {
                // Expand a collapsed header.
                if let Some(row) = self.selected_row(model)
                    && row.kind.is_collapsible()
                    && row.collapsed
                {
                    return self.toggle_collapse(model, session);
                }
            }
            KeyCode::Char('h') | KeyCode::LeftArrow => {
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
            KeyCode::Char('/') => {
                self.filtering = true;
                self.sync(model);
            }
            KeyCode::Char('s') => return SidebarOutcome::SortMenu,
            KeyCode::Char('p') => return self.toggle_pin(model, session),
            KeyCode::Char(' ') => {
                // Multi-select toggle (item 26): mark/unmark the cursor row if it
                // is a worktree or workspace. Collapse now lives solely on
                // Enter/←/→ and the caret click, so headers can be selected too.
                if let Some(row) = self.selected_row(model)
                    && row.is_markable()
                {
                    let key = row.pin_key.clone();
                    if !self.marked.remove(&key) {
                        self.marked.insert(key);
                    }
                    self.sync(model);
                }
            }
            KeyCode::Char('m') => {
                self.menu = self.menu_for_cursor(model, session);
                self.sync(model);
            }
            KeyCode::Char('d') | KeyCode::Delete => {
                if let Some(out) = self.delete_outcome(model, session) {
                    return out;
                }
            }
            KeyCode::Char('r') | KeyCode::Function(2) => {
                if let Some(out) = self.rename_outcome(model, session) {
                    return out;
                }
            }
            KeyCode::Char('n') => {
                if self.cursor_in_terminals(model) {
                    return SidebarOutcome::Synthetic(crate::keymap::Action::NewTerminal);
                }
                return match self.cursor_repo_root(model) {
                    Some(repo_root) => SidebarOutcome::NewWorktreeIn { repo_root },
                    None => SidebarOutcome::Synthetic(crate::keymap::Action::NewWorktree),
                };
            }
            KeyCode::Char('N') => {
                return SidebarOutcome::Synthetic(crate::keymap::Action::NewWorkspace);
            }
            KeyCode::Char('b') => {
                if let Some(out) = self.fork_outcome(model) {
                    return out;
                }
            }
            KeyCode::Char('f') => {
                if let Some(out) = self.folder_outcome(model) {
                    return out;
                }
            }
            KeyCode::Char('c') => {
                if let Some(p) = self
                    .selected_row(model)
                    .and_then(|r| r.worktree_path.clone())
                {
                    return SidebarOutcome::CopyText(p);
                }
            }
            KeyCode::Char('?') => return SidebarOutcome::ShowHelp,
            KeyCode::Char('<') | KeyCode::Char(',') => {
                return self.adjust_width(-2);
            }
            KeyCode::Char('>') | KeyCode::Char('.') => {
                return self.adjust_width(2);
            }
            KeyCode::Char('e') => {
                // Toggle the Wide expand (mirrors the panel's `e`): ~half the
                // window vs. the fine-nudged width.
                self.expanded = !self.expanded;
                self.persist("sidebar_expanded", if self.expanded { "1" } else { "0" });
                return SidebarOutcome::Relayout;
            }
            _ => return SidebarOutcome::NotHandled,
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
                if let Some(crate::sidebar::RowTarget::Tab(gi, _)) = row.tab_target.clone()
                    && let Some(branch) = row.branch.clone()
                {
                    return Some(SidebarOutcome::PromptRename { gi, branch });
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
        let repo_root = thegn_core::repo::main_worktree(std::path::Path::new(&path))
            .map(|p| p.to_string_lossy().into_owned())
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
    fn marked_group_targets(&self, model: &FrameModel) -> Vec<usize> {
        let mut targets: Vec<usize> = model
            .sidebar_rows
            .iter()
            .filter(|r| r.visible && self.marked.contains(&r.pin_key))
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
            .filter(|r| r.visible && self.marked.contains(&r.pin_key))
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
        // Bulk: every marked row's pin key, else the cursor row's.
        let mut keys: Vec<String> = model
            .sidebar_rows
            .iter()
            .filter(|r| r.visible && self.marked.contains(&r.pin_key))
            .map(|r| r.pin_key.clone())
            .collect();
        if keys.is_empty()
            && let Some(row) = self.selected_row(model)
        {
            keys.push(row.pin_key.clone());
        }
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
            "pin" => return self.toggle_pin(model, session),
            "close" => {
                let targets = self.action_targets(model);
                if !targets.is_empty() {
                    return SidebarOutcome::CloseGroups(targets);
                }
            }
            "delete" => {
                let targets = self.action_targets(model);
                if !targets.is_empty() {
                    return SidebarOutcome::DeleteGroups(targets);
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
            _ => {}
        }
        SidebarOutcome::Redraw
    }
}
