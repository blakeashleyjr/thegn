//! Sidebar reorder: move worktrees / folders / workspaces / terminals up or
//! down one slot and persist the new order. Extracted from `run.rs` (pinned by
//! the keep-god-files-flat guidance).
//!
//! Two entry points share the same primitives:
//! - **Ctrl+Alt+↑/↓** (`move_cursor_worktree` / `move_folder_id` /
//!   `move_selected_workspace` / `move_cursor_terminal`) move the single item
//!   under the sidebar cursor — or the active worktree when the sidebar isn't
//!   focused.
//! - **Shift+↑/↓** (`reorder_selection`) move the whole multi-select: every
//!   marked row of the cursor row's kind (or the cursor row alone when nothing
//!   is marked), one slot, as a block. Terminals are never markable
//!   (`SidebarRow::is_markable`), so a terminal "block" is always the cursor
//!   row alone.
//!
//! Worktree motion is confined to the row's **sibling run** — the loose list or
//! the folder it is filed into — as resolved by [`crate::sidebar_order`];
//! pushing past a run's edge crosses into the adjacent run and re-files. `home`
//! anchors the head of the loose run. A move under a computed sort first flips
//! the workspace back to Manual so the move is visible and sticks.
//!
//! Persistence writes the workspace's exact new order (`position = index`) once
//! per move rather than swapping two positions, for the same reason
//! `set_workspace_order` exists: a swap leans on the normalize pass and can seed
//! a sequence that differs from the tree.

use std::collections::HashSet;

use thegn_core::store::WorkspaceStore;

use crate::chrome::FrameModel;
use crate::run::{SidebarState, visible_index_of_active, visible_index_of_workspace};

/// The visible-row index of the row with this `pin_key`, if present. The cursor
/// travels with the item it moved by re-resolving this after the rebuild.
fn visible_index_of_pin_key(model: &FrameModel, key: &str) -> Option<usize> {
    model
        .sidebar_rows
        .iter()
        .filter(|r| r.visible)
        .position(|r| r.pin_key == key)
}

/// The visible-row index of the worktree row for `path`. A move that crosses a
/// folder boundary re-keys the row (`pin_key` embeds the folder), so the cursor
/// falls back to the path, which is stable across a re-file.
fn visible_index_of_worktree_path(model: &FrameModel, path: &str) -> Option<usize> {
    model
        .sidebar_rows
        .iter()
        .filter(|r| r.visible)
        .position(|r| {
            r.kind == crate::sidebar::RowKind::Worktree && r.worktree_path.as_deref() == Some(path)
        })
}

/// The workspace slug owning the worktree row at `path`.
fn workspace_slug_of_path(model: &FrameModel, path: &str) -> Option<String> {
    model
        .sidebar_rows
        .iter()
        .find(|r| {
            r.kind == crate::sidebar::RowKind::Worktree && r.worktree_path.as_deref() == Some(path)
        })
        .map(|r| r.workspace_slug.clone())
}

/// Run a best-effort DB write off the event loop. Falls back to running inline
/// when there is no tokio runtime (unit tests) — the DB is a cache, so the
/// in-memory reorder is the user-visible change either way.
fn off_loop(job: impl FnOnce() + Send + 'static) {
    match tokio::runtime::Handle::try_current() {
        Ok(_) => {
            tokio::task::spawn_blocking(job);
        }
        Err(_) => job(),
    }
}

/// Persist a worktree reorder: the membership change (when the move crossed a
/// run) and then the workspace's exact new order as `position = index`.
///
/// Writing the whole order rather than swapping two positions is the same
/// reasoning as `set_workspace_order` (see [`SidebarState::move_workspace_by_slug`]):
/// a swap leans on `normalize_worktree_positions` to heal NULL/tied values and
/// can seed a different sequence than the tree is showing.
fn persist_worktree_order(order: Vec<String>, refile: Option<(String, Option<i64>)>) {
    off_loop(move || {
        // best-effort beyond the warn: the DB is a cache — the optimistic
        // model regroup is the user-visible change; a failed write only loses
        // it on restart. But log it, or the "next tick snapped my order back"
        // failure mode is undiagnosable.
        let Ok(db) = thegn_core::db::Db::open() else {
            tracing::warn!(target: "thegn::sidebar", "worktree reorder not persisted: DB unavailable");
            return;
        };
        if let Some((path, folder)) = refile
            && let Err(e) = db.set_worktree_folder(&path, folder)
        {
            tracing::warn!(target: "thegn::sidebar", error = %e, "worktree re-file not persisted");
        }
        if let Err(e) = db.set_worktree_order(&order) {
            tracing::warn!(target: "thegn::sidebar", error = %e, "worktree order not persisted");
        }
    });
}

/// Persist a folder reorder as `position = index` within one workspace.
fn persist_folder_order(repo_path: String, order: Vec<i64>) {
    off_loop(move || {
        // best-effort beyond the warn: see `persist_worktree_order`.
        match thegn_core::db::Db::open() {
            Ok(db) => {
                if let Err(e) = db.set_folder_order(&repo_path, &order) {
                    tracing::warn!(target: "thegn::sidebar", error = %e, "folder order not persisted");
                }
            }
            Err(e) => {
                tracing::warn!(target: "thegn::sidebar", error = %e, "folder order not persisted: DB unavailable");
            }
        }
    });
}

impl SidebarState {
    /// Move the active worktree one slot within its run (Ctrl+Alt+↑/↓ with the
    /// sidebar unfocused), keeping the highlight on the moved (still active)
    /// group.
    pub(crate) fn move_active_worktree(
        &mut self,
        model: &mut FrameModel,
        session: &mut crate::session::Session,
        up: bool,
    ) -> bool {
        let Some(path) = session
            .worktrees
            .get(session.active)
            .map(|g| g.path.clone())
        else {
            return false;
        };
        if self.move_worktree_path(model, session, &path, up) {
            // Keep the highlight on the worktree that just moved (now the active
            // group), the way workspace reorders already do.
            self.cursor = visible_index_of_active(model);
            self.sync(model);
            true
        } else {
            false
        }
    }

    /// Move the worktree under the sidebar cursor one slot within its run,
    /// keeping the cursor on it — the worktree analogue of
    /// [`Self::move_cursor_terminal`] / [`Self::move_selected_workspace`].
    ///
    /// The Ctrl+Alt dispatch used to fall through to [`Self::move_active_worktree`]
    /// for every non-workspace, non-terminal row, so parking the cursor on an
    /// inactive worktree and pressing the key moved a *different* row than the
    /// highlighted one. This is the cursor-based path the other two row kinds
    /// already had.
    pub(crate) fn move_cursor_worktree(
        &mut self,
        model: &mut FrameModel,
        session: &mut crate::session::Session,
        up: bool,
    ) -> bool {
        let Some(row) = self.selected_row(model) else {
            return false;
        };
        let cursor_key = row.pin_key.clone();
        let Some(path) = row.worktree_path.clone() else {
            return false;
        };
        if self.move_worktree_path(model, session, &path, up) {
            // The pin key of a filed worktree embeds its folder, so a move that
            // crosses runs re-keys the row; fall back to the path.
            self.cursor = visible_index_of_pin_key(model, &cursor_key)
                .or_else(|| visible_index_of_worktree_path(model, &path))
                .unwrap_or(self.cursor);
            self.sync(model);
            true
        } else {
            false
        }
    }

    /// Move the worktree at `path` one slot within its **sibling run** — the
    /// loose list, or the folder it is filed into — crossing into the adjacent
    /// run (and re-filing) when pushed past an edge. `home` is a fixed anchor at
    /// the head of the loose run: it never moves and nothing lands above it.
    /// Rebuilds the tree; the caller places the cursor. Returns whether it moved.
    pub(crate) fn move_worktree_path(
        &mut self,
        model: &mut FrameModel,
        session: &mut crate::session::Session,
        path: &str,
        up: bool,
    ) -> bool {
        let Some(slug) = workspace_slug_of_path(model, path) else {
            return false;
        };
        let Some(plan) = crate::sidebar_order::step(&model.sidebar_rows, &slug, path, up) else {
            return false;
        };
        self.apply_order_plan(model, session, &slug, plan)
    }

    /// Apply a resolved [`crate::sidebar_order::Plan`]: optimistically regroup
    /// and reorder the model so the tree shows the move on this frame, permute
    /// the live session groups to match, then persist off-loop.
    ///
    /// The optimistic-model + deferred-write shape mirrors
    /// [`crate::handlers::sidebar_folder::file_worktree_path`]; unlike that path
    /// it does **not** fire a `RefreshKind::Model` afterwards, because a reorder
    /// only ever targets folders that already exist (no synthetic id to
    /// reconcile) and a held-down arrow key must not queue a full hydration per
    /// repeat.
    pub(crate) fn apply_order_plan(
        &mut self,
        model: &mut FrameModel,
        session: &mut crate::session::Session,
        slug: &str,
        plan: crate::sidebar_order::Plan,
    ) -> bool {
        // 1. Membership, optimistically — this is what moves a row between a
        //    folder and the loose list on the same frame.
        if let Some(folder) = plan.refile
            && let Some(w) = model
                .sidebar_db_worktrees
                .iter_mut()
                .find(|w| w.path == plan.path)
        {
            w.folder_id = folder;
        }

        // 2. Order, optimistically. A *dormant* workspace's rows are rebuilt
        //    from this list (in `db.worktrees()` position order), so sorting it
        //    here is what makes reorder work without a live session group.
        let rank = |p: &str| plan.order.iter().position(|q| q == p).unwrap_or(usize::MAX);
        model
            .sidebar_db_worktrees
            .sort_by_key(|w| (w.slug != slug, rank(&w.path)));

        // 3. The live session order: `gather_groups` reads `session.worktrees`
        //    slot order for a loaded workspace, so permute this workspace's
        //    slots in place (other workspaces' groups keep their slots).
        let slots: Vec<usize> = session
            .worktrees
            .iter()
            .enumerate()
            .filter(|(_, g)| {
                crate::sidebar::split_tab(&g.name)
                    .map(|(s, _)| s)
                    .as_deref()
                    == Some(slug)
            })
            .map(|(i, _)| i)
            .collect();
        if !slots.is_empty() {
            // Track the active group by name: a permutation moves indices
            // around in a way the old pairwise index fixup can't express.
            let active_name = session
                .worktrees
                .get(session.active)
                .map(|g| g.name.clone());
            let mut taken: Vec<crate::session::WorktreeGroup> = slots
                .iter()
                .map(|&i| session.worktrees[i].clone())
                .collect();
            // Stable sort: a group missing from `order` (not yet hydrated into
            // the DB list) keeps its relative slot at the end of the run.
            taken.sort_by_key(|g| rank(&g.path));
            for (&slot, g) in slots.iter().zip(taken) {
                session.worktrees[slot] = g;
            }
            if let Some(name) = active_name
                && let Some(i) = session.worktrees.iter().position(|g| g.name == name)
            {
                session.active = i;
            }
        }

        // 4. A manual move only makes sense under Manual order; flip + persist
        //    if a computed sort was active so the move is visible and sticks.
        if self.view.sort != crate::sidebar::SortMode::Manual {
            self.view.sort = crate::sidebar::SortMode::Manual;
            self.persist("sort_mode", self.view.sort.as_str());
        }
        // Stamp the optimistic edit so the model swap keeps these lists while
        // the deferred write is in flight (see `optimistic_db_edit_at`).
        self.optimistic_db_edit_at = Some(std::time::Instant::now());
        self.rebuild(model, session);

        persist_worktree_order(plan.order, plan.refile.map(|f| (plan.path, f)));
        true
    }

    /// Move folder `fid` one slot among its workspace's folders (its worktrees
    /// travel with the header, so no worktree position changes). Rebuilds; the
    /// caller places the cursor. Returns whether it moved.
    pub(crate) fn move_folder_id(
        &mut self,
        model: &mut FrameModel,
        session: &crate::session::Session,
        slug: &str,
        fid: i64,
        up: bool,
    ) -> bool {
        let Some(order) = crate::sidebar_order::step_folder(&model.sidebar_rows, slug, fid, up)
        else {
            return false;
        };
        self.apply_folder_order(model, session, slug, order)
    }

    /// Apply a new folder id order for `slug`: renumber the in-model
    /// `FolderRow`s so the tree shows it now, then persist off-loop.
    pub(crate) fn apply_folder_order(
        &mut self,
        model: &mut FrameModel,
        session: &crate::session::Session,
        slug: &str,
        order: Vec<i64>,
    ) -> bool {
        let Some(repo_path) = model
            .sidebar_workspaces
            .iter()
            .find(|(s, ..)| s == slug)
            .map(|(_, _, _, p)| p.clone())
            .filter(|p| !p.is_empty())
        else {
            return false;
        };
        for f in model.sidebar_db_folders.iter_mut() {
            if f.repo_path == repo_path
                && let Some(i) = order.iter().position(|id| *id == f.folder_id)
            {
                f.position = i as i64;
            }
        }
        // Stamp the optimistic edit so the model swap keeps these lists while
        // the deferred write is in flight (see `optimistic_db_edit_at`).
        self.optimistic_db_edit_at = Some(std::time::Instant::now());
        self.rebuild(model, session);

        // A folder created optimistically carries a synthetic negative id that
        // has no DB row yet; the deferred filing write will assign the real one,
        // so skip those rather than renumber a row that doesn't exist.
        let real: Vec<i64> = order.into_iter().filter(|id| *id > 0).collect();
        persist_folder_order(repo_path, real);
        true
    }

    /// Move the terminal named `name` one slot within its **host group**,
    /// swapping the durable `position` with its adjacent same-host sibling in
    /// display order. Terminals live in the DB registry (not `session.worktrees`
    /// as orderable groups), so this swaps `terminals.position` and re-reads the
    /// registry snapshot before rebuilding. Never crosses a host boundary; the
    /// local/remote host grouping itself is fixed (see `terminal_hosts_ordered`).
    /// Returns whether it moved.
    pub(crate) fn move_terminal_row(
        &mut self,
        model: &mut FrameModel,
        session: &crate::session::Session,
        name: &str,
        up: bool,
    ) -> bool {
        use crate::sidebar::RowKind;
        // The cursor terminal's host slug (`terminals/host:{key}`) confines the
        // motion to same-host siblings.
        let Some(host_slug) = model
            .sidebar_rows
            .iter()
            .find(|r| r.kind == RowKind::Terminal && r.worktree_path.as_deref() == Some(name))
            .map(|r| r.workspace_slug.clone())
        else {
            return false;
        };
        // In-host display order (terminal names), top to bottom as rendered.
        let order = |model: &FrameModel| -> Vec<String> {
            model
                .sidebar_rows
                .iter()
                .filter(|r| {
                    r.visible && r.kind == RowKind::Terminal && r.workspace_slug == host_slug
                })
                .filter_map(|r| r.worktree_path.clone())
                .collect()
        };
        let cur = order(model);
        let Some(p) = cur.iter().position(|n| n == name) else {
            return false;
        };
        let neighbor = if up {
            p.checked_sub(1)
        } else {
            (p + 1 < cur.len()).then_some(p + 1)
        };
        let Some(np) = neighbor else { return false };
        let other = cur[np].clone();

        // Optimistic: swap the two entries in the registry snapshot the tree
        // renders from, then persist off-loop — a fresh `Db::open()` + write
        // ON the loop can stall up to the WAL `busy_timeout` (the same hazard
        // every other reorder persist already routes around via `off_loop`).
        let idx_of = |terms: &[thegn_core::models::TerminalRow], n: &str| {
            terms.iter().position(|t| t.name == n)
        };
        if let (Some(ia), Some(ib)) = (
            idx_of(&model.sidebar_db_terminals, name),
            idx_of(&model.sidebar_db_terminals, &other),
        ) {
            model.sidebar_db_terminals.swap(ia, ib);
        }
        self.optimistic_db_edit_at = Some(std::time::Instant::now());
        let (a, b) = (name.to_string(), other);
        off_loop(move || {
            let Ok(db) = thegn_core::db::Db::open() else {
                tracing::warn!(target: "thegn::sidebar", "terminal reorder not persisted: DB unavailable");
                return;
            };
            if let Err(e) = db.swap_terminal_positions(&a, &b) {
                tracing::warn!(target: "thegn::sidebar", error = %e, "terminal reorder not persisted");
            }
        });
        self.rebuild(model, session);
        true
    }

    /// Move the terminal under the sidebar cursor one slot within its host
    /// (Ctrl+Alt+↑/↓), keeping the cursor on the moved terminal — the terminal
    /// analogue of [`Self::move_active_worktree`] / [`Self::move_selected_workspace`].
    pub(crate) fn move_cursor_terminal(
        &mut self,
        model: &mut FrameModel,
        session: &crate::session::Session,
        up: bool,
    ) -> bool {
        let Some(row) = self.selected_row(model) else {
            return false;
        };
        let cursor_key = row.pin_key.clone();
        let Some(name) = row.worktree_path.clone() else {
            return false;
        };
        if self.move_terminal_row(model, session, &name, up) {
            self.cursor = visible_index_of_pin_key(model, &cursor_key).unwrap_or(self.cursor);
            self.sync(model);
            true
        } else {
            false
        }
    }

    /// Reorder the workspace under the sidebar cursor one slot (Ctrl+Alt+↑/↓),
    /// keeping the cursor on the moved workspace's header.
    pub(crate) fn move_selected_workspace(
        &mut self,
        model: &mut FrameModel,
        session: &crate::session::Session,
        up: bool,
    ) -> bool {
        let Some(slug) = self.selected_row(model).map(|r| r.workspace_slug.clone()) else {
            return false;
        };
        if self.move_workspace_by_slug(model, session, &slug, up) {
            if let Some(idx) = visible_index_of_workspace(model, &slug) {
                self.cursor = idx;
                self.sync(model);
            }
            true
        } else {
            false
        }
    }

    /// Move the workspace with this `slug` one slot in the **visible** order:
    /// swap it with its on-screen neighbor, rewrite `model.sidebar_workspaces`
    /// to match so it shows at once, then persist the **entire** new order via
    /// `set_workspace_order`. Live-only workspaces (no DB row) are skipped.
    /// Rebuilds; the caller places the cursor. Returns whether it moved.
    ///
    /// Both the neighbor AND the applied order come from the visible workspace
    /// order (`sidebar_rows`) — the order the user sees and the Shift+Alt nav
    /// ring walks. Moves the raw order can't express are refused with a status
    /// instead: pinned blocks always float first (`apply_pins`), and the
    /// `attention` workspace-sort recomputes the order every rebuild.
    ///
    /// Persisting the whole order (not a two-position swap) is deliberate: the
    /// nav ring is rebuilt from `db.workspaces()` on the next hydration, and a
    /// swap that relies on `normalize_workspace_positions` can seed positions in
    /// a different order than the tree shows when positions are NULL/tied — so
    /// the ring would walk an order the user never arranged. Writing
    /// `position = index` over the current order makes the reload verbatim.
    pub(crate) fn move_workspace_by_slug(
        &mut self,
        model: &mut FrameModel,
        session: &crate::session::Session,
        slug: &str,
        up: bool,
    ) -> bool {
        // Reorderable (DB-backed) workspaces in visible order. A workspace
        // header carries `worktree_path: Some(_)` only when it has a DB row;
        // live-only fallbacks (no position) are skipped, as before.
        let visible: Vec<String> = model
            .sidebar_rows
            .iter()
            .filter(|r| {
                r.visible
                    && r.kind == crate::sidebar::RowKind::Workspace
                    && r.worktree_path.is_some()
            })
            .map(|r| r.workspace_slug.clone())
            .collect();
        let Some(p) = visible.iter().position(|s| s == slug) else {
            return false;
        };
        let neighbor = if up {
            p.checked_sub(1)
        } else {
            (p + 1 < visible.len()).then_some(p + 1)
        };
        let Some(np) = neighbor else { return false };

        // Target VISIBLE order = the on-screen order with the two swapped.
        let mut target = visible.clone();
        target.swap(p, np);
        self.apply_workspace_order(model, session, target)
    }

    /// Apply a resolved workspace slug order — the mouse-drop counterpart of the
    /// one-slot [`Self::move_workspace_by_slug`], which now delegates here.
    ///
    /// A drop used to step-walk `move_workspace_by_slug` up to `len + 1` times
    /// (a full rebuild and an off-loop DB write per step), and could bail out
    /// half way on a pinned neighbour — parking the workspace between source and
    /// target. One resolved order applied once is atomic: it either happens or
    /// it doesn't.
    pub(crate) fn apply_workspace_order(
        &mut self,
        model: &mut FrameModel,
        session: &crate::session::Session,
        target: Vec<String>,
    ) -> bool {
        let visible: Vec<String> = model
            .sidebar_rows
            .iter()
            .filter(|r| {
                r.visible
                    && r.kind == crate::sidebar::RowKind::Workspace
                    && r.worktree_path.is_some()
            })
            .map(|r| r.workspace_slug.clone())
            .collect();
        if target == visible {
            return false; // nothing moved
        }

        // Refuse moves the renderer cannot express in the raw order, instead
        // of silently rewriting `sidebar_workspaces` (and persisting it) with
        // no on-screen effect — or worse, a jump in the wrong direction:
        // `apply_pins` always floats pinned blocks first (in pin order), and
        // the attention workspace-sort recomputes the order every rebuild,
        // using the raw order only as a tiebreak.
        //
        // Attention sort REFUSES rather than flipping to manual the way
        // `apply_order_plan` flips `view.sort`: that one is persisted UI state
        // (`persist("sort_mode", …)`), whereas `view.workspace_sort` is mirrored
        // from `[ui] sidebar_workspace_sort` with no persistence hook, so a flip
        // would be silently reverted on the next config reload.
        if self.view.workspace_sort == thegn_core::config::WorkspaceSort::Attention {
            model.status =
                "Workspace order is by attention — set [ui] sidebar_workspace_sort = \"manual\" to reorder"
                    .into();
            return false;
        }
        let pinned = |s: &str| self.view.pins.iter().any(|k| k == s);
        // Any workspace whose slot actually CHANGED must be unpinned — a pinned
        // block is floated to the front by `apply_pins`, so its position is not
        // expressible by rewriting `sidebar_workspaces` at all.
        if visible
            .iter()
            .zip(&target)
            .any(|(a, b)| a != b && (pinned(a) || pinned(b)))
        {
            model.status = "Pinned workspaces stay on top — unpin to reorder".into();
            return false;
        }

        // Rewrite the reorderable entries of `sidebar_workspaces` into that
        // order in place — a raw two-slot swap is only equivalent when the
        // raw and visible orders already coincide. Entries not in the visible
        // list (live-only fallbacks, filter-hidden rows) keep their slots.
        let rank: std::collections::HashMap<&str, usize> = target
            .iter()
            .enumerate()
            .map(|(i, s)| (s.as_str(), i))
            .collect();
        let slots: Vec<usize> = model
            .sidebar_workspaces
            .iter()
            .enumerate()
            .filter(|(_, (ws, _, _, _))| rank.contains_key(ws.as_str()))
            .map(|(i, _)| i)
            .collect();
        let mut entries: Vec<_> = slots
            .iter()
            .map(|&i| model.sidebar_workspaces[i].clone())
            .collect();
        entries.sort_by_key(|(ws, _, _, _)| rank[ws.as_str()]);
        for (slot, entry) in slots.into_iter().zip(entries) {
            model.sidebar_workspaces[slot] = entry;
        }
        // Persist the ENTIRE new on-screen order (not a two-position swap): the
        // nav ring is rebuilt from `db.workspaces()` on the next hydration, and
        // a swap that leans on `normalize_workspace_positions` can seed a
        // different order than the tree shows when positions are NULL/tied
        // (a different tiebreak) — so the ring would walk an order the user
        // never arranged. Writing `position = index` over the current order
        // makes the reload reproduce exactly what's on screen.
        let order: Vec<String> = model
            .sidebar_workspaces
            .iter()
            .filter(|(_, _, _, path)| !path.is_empty())
            .map(|(_, _, _, path)| path.clone())
            .collect();
        // Persist off-loop: a fresh `Db::open()` + write on the loop can stall
        // up to the WAL `busy_timeout`. Best-effort beyond the warn — the DB
        // is a cache; the in-memory reorder above is the user-visible move.
        self.optimistic_db_edit_at = Some(std::time::Instant::now());
        off_loop(move || {
            let Ok(db) = thegn_core::db::Db::open() else {
                tracing::warn!(target: "thegn::sidebar", "workspace reorder not persisted: DB unavailable");
                return;
            };
            if let Err(e) = db.set_workspace_order(&order) {
                tracing::warn!(target: "thegn::sidebar", error = %e, "workspace reorder not persisted");
            }
        });
        self.rebuild(model, session);
        true
    }

    /// Reorder the current selection (Shift+↑/↓) one slot. The selection is
    /// homogeneous by the **cursor row's kind**: worktrees or workspaces. Marks
    /// of the other kind are ignored; with nothing marked, the cursor row moves
    /// alone (matching the single-item Ctrl+Alt behaviour). A worktree selection
    /// spanning >1 workspace is refused (worktrees only reorder within their
    /// own workspace). Returns whether anything moved.
    pub(crate) fn reorder_selection(
        &mut self,
        model: &mut FrameModel,
        session: &mut crate::session::Session,
        up: bool,
    ) -> bool {
        use crate::sidebar::RowKind;
        let Some(cursor_row) = self.selected_row(model) else {
            return false;
        };
        // Owned up front: the arms below mutate `model`, which ends the borrow.
        let cursor_kind = cursor_row.kind;
        let cursor_key = cursor_row.pin_key.clone();
        let cursor_path = cursor_row.worktree_path.clone();
        let cursor_folder = cursor_row.folder_id;
        let cursor_slug = cursor_row.workspace_slug.clone();

        match cursor_kind {
            RowKind::Worktree => {
                // Selected worktrees (marked rows of this kind, else the cursor
                // row), keyed by **path** — stable across a re-file, unlike the
                // pin key, and available on dormant rows that have no session
                // group.
                let mut sel_paths: Vec<String> = model
                    .sidebar_rows
                    .iter()
                    .filter(|r| {
                        r.visible && r.kind == RowKind::Worktree && self.marked.contains(&r.pin_key)
                    })
                    .filter_map(|r| r.worktree_path.clone())
                    .collect();
                if sel_paths.is_empty()
                    && let Some(p) = cursor_path.clone()
                {
                    sel_paths.push(p);
                }
                if sel_paths.is_empty() {
                    return false;
                }
                let sel: HashSet<String> = sel_paths.iter().cloned().collect();
                // Worktrees only reorder within their own workspace.
                let slugs: HashSet<String> = model
                    .sidebar_rows
                    .iter()
                    .filter(|r| {
                        r.kind == RowKind::Worktree
                            && r.worktree_path.as_deref().is_some_and(|p| sel.contains(p))
                    })
                    .map(|r| r.workspace_slug.clone())
                    .collect();
                if slugs.len() > 1 {
                    model.status = "Can't move a selection across workspaces".into();
                    return false;
                }
                let Some(slug) = slugs.into_iter().next() else {
                    return false;
                };
                // Process in display order — top-first for up, bottom-first for
                // down — so a block moves as a unit and two selected neighbours
                // never swap with each other.
                let mut ordered: Vec<String> = model
                    .sidebar_rows
                    .iter()
                    .filter(|r| r.kind == RowKind::Worktree)
                    .filter_map(|r| r.worktree_path.clone())
                    .filter(|p| sel.contains(p))
                    .collect();
                if !up {
                    ordered.reverse();
                }
                let mut moved = false;
                for path in &ordered {
                    // Don't swap two selected items past each other. At a run
                    // edge there is no in-run neighbour, so the block crosses
                    // into the adjacent run one member at a time.
                    if let Some(nb) =
                        crate::sidebar_order::in_run_neighbor(&model.sidebar_rows, &slug, path, up)
                        && sel.contains(&nb)
                    {
                        continue;
                    }
                    if self.move_worktree_path(model, session, path, up) {
                        moved = true;
                    }
                }
                if moved {
                    self.cursor = visible_index_of_pin_key(model, &cursor_key)
                        .or_else(|| {
                            cursor_path
                                .as_deref()
                                .and_then(|p| visible_index_of_worktree_path(model, p))
                        })
                        .unwrap_or(self.cursor);
                    self.sync(model);
                }
                moved
            }
            RowKind::Folder => {
                // Folders aren't markable, so this is always the cursor row.
                let Some(fid) = cursor_folder else {
                    return false;
                };
                if self.move_folder_id(model, session, &cursor_slug, fid, up) {
                    self.cursor =
                        visible_index_of_pin_key(model, &cursor_key).unwrap_or(self.cursor);
                    self.sync(model);
                    true
                } else {
                    false
                }
            }
            RowKind::Workspace => {
                // Selected workspace slugs (marked headers, else the cursor's).
                let mut sel_slugs: HashSet<String> = model
                    .sidebar_rows
                    .iter()
                    .filter(|r| {
                        r.visible
                            && r.kind == RowKind::Workspace
                            && self.marked.contains(&r.pin_key)
                    })
                    .map(|r| r.workspace_slug.clone())
                    .collect();
                if sel_slugs.is_empty()
                    && let Some(row) = self.selected_row(model)
                {
                    sel_slugs.insert(row.workspace_slug.clone());
                }
                if sel_slugs.is_empty() {
                    return false;
                }
                let display_order = |model: &FrameModel| -> Vec<String> {
                    model
                        .sidebar_rows
                        .iter()
                        .filter(|r| r.visible && r.kind == RowKind::Workspace)
                        .map(|r| r.workspace_slug.clone())
                        .collect::<Vec<_>>()
                };
                let mut ordered: Vec<String> = display_order(model)
                    .into_iter()
                    .filter(|s| sel_slugs.contains(s))
                    .collect();
                if !up {
                    ordered.reverse();
                }
                let mut moved = false;
                for slug in &ordered {
                    let disp = display_order(model);
                    let Some(p) = disp.iter().position(|s| s == slug) else {
                        continue;
                    };
                    let neighbor = if up {
                        p.checked_sub(1)
                    } else {
                        (p + 1 < disp.len()).then_some(p + 1)
                    };
                    if let Some(np) = neighbor
                        && sel_slugs.contains(&disp[np])
                    {
                        continue;
                    }
                    if self.move_workspace_by_slug(model, session, slug, up) {
                        moved = true;
                    }
                }
                if moved {
                    self.cursor =
                        visible_index_of_pin_key(model, &cursor_key).unwrap_or(self.cursor);
                    self.sync(model);
                }
                moved
            }
            RowKind::Terminal => {
                // The cursor terminal, confined to its host group — terminals
                // only reorder within their own host, mirroring
                // worktrees-within-a-workspace. (No marked set here: terminals
                // are not markable — `SidebarRow::is_markable` — so the
                // "block" is always exactly the cursor row. The Vec shape is
                // kept so the block machinery below stays shared.)
                let host_slug = cursor_row.workspace_slug.clone();
                let mut sel_names: Vec<String> = Vec::new();
                if let Some(n) = self
                    .selected_row(model)
                    .and_then(|r| r.worktree_path.clone())
                {
                    sel_names.push(n);
                }
                if sel_names.is_empty() {
                    return false;
                }
                let sel: HashSet<String> = sel_names.iter().cloned().collect();
                // In-host display order of the selected terminals — top-first for
                // up, bottom-first for down — so the block moves as a unit.
                let in_host = |model: &FrameModel| -> Vec<String> {
                    model
                        .sidebar_rows
                        .iter()
                        .filter(|r| {
                            r.visible
                                && r.kind == RowKind::Terminal
                                && r.workspace_slug == host_slug
                        })
                        .filter_map(|r| r.worktree_path.clone())
                        .collect()
                };
                let mut ordered: Vec<String> = in_host(model)
                    .into_iter()
                    .filter(|n| sel.contains(n))
                    .collect();
                if !up {
                    ordered.reverse();
                }
                let mut moved = false;
                for name in &ordered {
                    let disp = in_host(model);
                    let Some(p) = disp.iter().position(|n| n == name) else {
                        continue;
                    };
                    let neighbor = if up {
                        p.checked_sub(1)
                    } else {
                        (p + 1 < disp.len()).then_some(p + 1)
                    };
                    // Don't swap two selected terminals past each other.
                    if let Some(np) = neighbor
                        && sel.contains(&disp[np])
                    {
                        continue;
                    }
                    if self.move_terminal_row(model, session, name, up) {
                        moved = true;
                    }
                }
                if moved {
                    self.cursor =
                        visible_index_of_pin_key(model, &cursor_key).unwrap_or(self.cursor);
                    self.sync(model);
                }
                moved
            }
            _ => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hydrate::build_initial_model;
    use crate::run::{SidebarOutcome, now_secs};
    use crate::session::{GroupKind, Session, WorktreeGroup};
    use crate::sidebar::SortMode;
    use crate::testenv::ENV_LOCK;
    use termwiz::input::{KeyCode, Modifiers};

    /// Isolate the user DB: the move helpers open it to persist `position`
    /// swaps. The swap no-ops on the throwaway `/tmp` paths — the tests assert
    /// the in-memory reorder, which is the user-visible part.
    struct DbGuard {
        home: std::path::PathBuf,
        _lock: std::sync::MutexGuard<'static, ()>,
    }
    impl DbGuard {
        fn new(tag: &str) -> Self {
            let lock = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
            let home = std::env::temp_dir().join(format!(
                "thegn-reorder-{tag}-{}-{}",
                std::process::id(),
                now_secs()
            ));
            // SAFETY: guarded by ENV_LOCK; cleared on drop.
            unsafe { std::env::set_var("XDG_STATE_HOME", &home) };
            Self { home, _lock: lock }
        }
    }
    impl Drop for DbGuard {
        fn drop(&mut self) {
            // SAFETY: still under ENV_LOCK for this guard's lifetime.
            unsafe { std::env::remove_var("XDG_STATE_HOME") };
            let _ = std::fs::remove_dir_all(&self.home);
        }
    }

    /// A single-workspace session: the first label is home, the rest branches.
    fn app_session(labels: &[&str]) -> Session {
        let worktrees = labels
            .iter()
            .enumerate()
            .map(|(i, l)| {
                let kind = if i == 0 {
                    GroupKind::Home
                } else {
                    GroupKind::Branch
                };
                WorktreeGroup::new(format!("app/{l}"), kind, format!("/tmp/app-{l}"))
            })
            .collect();
        Session {
            id: "s1".into(),
            worktrees,
            active: 0,
        }
    }

    fn one_ws_model(session: &Session) -> FrameModel {
        let mut m = build_initial_model(session, None);
        m.sidebar_workspaces = vec![("app".into(), "app".into(), "repo".into(), String::new())];
        m
    }

    fn focused(model: &mut FrameModel, session: &Session) -> SidebarState {
        let mut sb = SidebarState {
            focused: true,
            ..Default::default()
        };
        sb.rebuild(model, session);
        sb
    }

    fn key_of(model: &FrameModel, label: &str) -> String {
        model
            .sidebar_rows
            .iter()
            .find(|r| r.label == label)
            .map(|r| r.pin_key.clone())
            .expect("row present")
    }
    fn vidx(model: &FrameModel, label: &str) -> usize {
        model
            .sidebar_rows
            .iter()
            .filter(|r| r.visible)
            .position(|r| r.label == label)
            .expect("row visible")
    }
    fn names(session: &Session) -> Vec<String> {
        session.worktrees.iter().map(|g| g.name.clone()).collect()
    }

    #[test]
    fn marks_survive_rebuild_across_collapse_and_sort() {
        let session = app_session(&["home", "alpha", "beta"]);
        let mut model = one_ws_model(&session);
        let mut sb = focused(&mut model, &session);
        sb.marked.insert(key_of(&model, "alpha"));
        sb.sync(&mut model);
        assert!(model.sidebar_marked.contains(&vidx(&model, "alpha")));

        // Collapse the workspace: alpha's row is hidden but still emitted, so the
        // identity mark is retained (not pruned).
        sb.view.collapsed.insert("app".into());
        sb.rebuild(&mut model, &session);
        assert!(sb.marked.contains("app/alpha"));

        // Expand again: the mark re-projects onto alpha's current visible index.
        sb.view.collapsed.remove("app");
        sb.rebuild(&mut model, &session);
        assert!(model.sidebar_marked.contains(&vidx(&model, "alpha")));

        // A sort change reshuffles indices; the identity mark still lands right.
        sb.view.sort = SortMode::Name;
        sb.rebuild(&mut model, &session);
        assert!(model.sidebar_marked.contains(&vidx(&model, "alpha")));
    }

    #[test]
    fn stale_mark_pruned_when_row_removed() {
        let session = app_session(&["home", "alpha", "beta"]);
        let mut model = one_ws_model(&session);
        let mut sb = focused(&mut model, &session);
        sb.marked.insert(key_of(&model, "alpha"));
        sb.marked.insert(key_of(&model, "beta"));
        sb.sync(&mut model);

        // Rebuild against a session that no longer has beta.
        let session2 = app_session(&["home", "alpha"]);
        sb.rebuild(&mut model, &session2);
        assert!(sb.marked.contains("app/alpha"));
        assert!(!sb.marked.contains("app/beta"), "gone row's mark is pruned");
    }

    #[test]
    fn space_marks_workspace_header_without_collapsing() {
        let session = app_session(&["home", "alpha"]);
        let mut model = one_ws_model(&session);
        let mut sb = focused(&mut model, &session);
        sb.cursor = vidx(&model, "app"); // the workspace header row
        let was_collapsed = sb.view.collapsed.contains("app");

        sb.handle_key(&KeyCode::Char(' '), Modifiers::NONE, &mut model, &session);
        assert!(sb.marked.contains("app"), "workspace header is now marked");
        assert_eq!(
            sb.view.collapsed.contains("app"),
            was_collapsed,
            "Space marks, it no longer collapses the header"
        );
        assert!(model.sidebar_marked.contains(&vidx(&model, "app")));
    }

    #[test]
    fn shift_arrow_returns_reorder_outcome() {
        let session = app_session(&["home", "alpha"]);
        let mut model = one_ws_model(&session);
        let mut sb = focused(&mut model, &session);
        let out = sb.handle_key(&KeyCode::UpArrow, Modifiers::SHIFT, &mut model, &session);
        assert!(matches!(out, SidebarOutcome::ReorderSelection { up: true }));
        let out = sb.handle_key(&KeyCode::DownArrow, Modifiers::SHIFT, &mut model, &session);
        assert!(matches!(
            out,
            SidebarOutcome::ReorderSelection { up: false }
        ));
    }

    #[test]
    fn reorder_moves_marked_worktree_block_as_a_unit() {
        let _db = DbGuard::new("block");
        let mut session = app_session(&["home", "a", "b", "c"]);
        let mut model = one_ws_model(&session);
        let mut sb = focused(&mut model, &session);
        sb.cursor = vidx(&model, "b");
        sb.marked.insert(key_of(&model, "b"));
        sb.marked.insert(key_of(&model, "c"));
        sb.sync(&mut model);

        assert!(sb.reorder_selection(&mut model, &mut session, true));
        assert_eq!(
            names(&session),
            vec!["app/home", "app/b", "app/c", "app/a"],
            "the {{b,c}} block moved up one slot, 'a' fell through"
        );
        // Both stay marked and the cursor rides with the item it was on.
        assert!(sb.marked.contains("app/b") && sb.marked.contains("app/c"));
        assert_eq!(sb.cursor, vidx(&model, "b"));
    }

    #[test]
    fn reorder_block_against_home_anchors_without_leapfrog() {
        let _db = DbGuard::new("anchor");
        let mut session = app_session(&["home", "a", "b"]);
        let mut model = one_ws_model(&session);
        let mut sb = focused(&mut model, &session);
        sb.cursor = vidx(&model, "a");
        sb.marked.insert(key_of(&model, "a"));
        sb.marked.insert(key_of(&model, "b"));
        sb.sync(&mut model);

        // The block is already flush against home: nothing moves, and the two
        // selected rows must not swap past each other.
        assert!(!sb.reorder_selection(&mut model, &mut session, true));
        assert_eq!(names(&session), vec!["app/home", "app/a", "app/b"]);
    }

    #[test]
    fn reorder_single_cursor_item_with_no_marks() {
        let _db = DbGuard::new("single");
        let mut session = app_session(&["home", "a", "b"]);
        let mut model = one_ws_model(&session);
        let mut sb = focused(&mut model, &session);
        sb.cursor = vidx(&model, "a");

        // Nothing marked → move the cursor's worktree down one slot.
        assert!(sb.reorder_selection(&mut model, &mut session, false));
        assert_eq!(names(&session), vec!["app/home", "app/b", "app/a"]);
        assert_eq!(sb.cursor, vidx(&model, "a"));
    }

    #[test]
    fn reorder_refuses_worktrees_across_workspaces() {
        let session_owned = Session {
            id: "s1".into(),
            worktrees: vec![
                WorktreeGroup::new("app/home", GroupKind::Home, "/tmp/app"),
                WorktreeGroup::new("app/a", GroupKind::Branch, "/tmp/app-a"),
                WorktreeGroup::new("lib/home", GroupKind::Home, "/tmp/lib"),
                WorktreeGroup::new("lib/x", GroupKind::Branch, "/tmp/lib-x"),
            ],
            active: 0,
        };
        let mut session = session_owned;
        let mut model = build_initial_model(&session, None);
        model.sidebar_workspaces = vec![
            ("app".into(), "app".into(), "repo".into(), String::new()),
            ("lib".into(), "lib".into(), "repo".into(), String::new()),
        ];
        let mut sb = focused(&mut model, &session);
        sb.cursor = vidx(&model, "a");
        sb.marked.insert(key_of(&model, "a")); // app/a
        sb.marked.insert(key_of(&model, "x")); // lib/x
        sb.sync(&mut model);

        assert!(!sb.reorder_selection(&mut model, &mut session, true));
        assert!(model.status.contains("across workspaces"));
        assert_eq!(
            names(&session),
            vec!["app/home", "app/a", "lib/home", "lib/x"],
            "nothing moved"
        );
    }

    #[test]
    fn workspace_reorder_uses_visible_order_under_pins() {
        // A pinned workspace floats to the top of the tree, so the visible
        // order differs from the raw `sidebar_workspaces` order. Moving a
        // workspace must swap it past its *visible* neighbor — the order the
        // Shift+Alt nav ring walks — not the raw-order neighbor (which, before
        // this, could pick the pinned row and produce no visible move).
        let _db = DbGuard::new("ws-pins");
        let session = app_session(&["home"]);
        let mut model = build_initial_model(&session, None);
        model.sidebar_workspaces = vec![
            ("app".into(), "app".into(), "repo".into(), "/tmp/app".into()),
            ("lib".into(), "lib".into(), "repo".into(), "/tmp/lib".into()),
            ("zed".into(), "zed".into(), "repo".into(), "/tmp/zed".into()),
        ];
        let mut sb = SidebarState {
            focused: true,
            ..Default::default()
        };
        sb.view.pins = vec!["lib".into()];
        sb.rebuild(&mut model, &session);

        let visible = |model: &FrameModel| -> Vec<String> {
            model
                .sidebar_rows
                .iter()
                .filter(|r| r.visible && r.kind == crate::sidebar::RowKind::Workspace)
                .map(|r| r.workspace_slug.clone())
                .collect::<Vec<_>>()
        };
        // Pinned lib floats first: visible order is [lib, app, zed].
        assert_eq!(visible(&model), vec!["lib", "app", "zed"]);

        // Move zed up: its visible neighbor is app (lib is pinned above). It
        // must swap past app, giving visible [lib, zed, app]. The raw-order
        // neighbor would have been lib and left the tree unchanged.
        assert!(sb.move_workspace_by_slug(&mut model, &session, "zed", true));
        assert_eq!(visible(&model), vec!["lib", "zed", "app"]);

        // Move zed up again: its visible neighbor is now the pinned lib. The
        // raw order can't express a move past a pinned block (`apply_pins`
        // always floats it first), so the move is refused with a status —
        // before this, the raw vec was silently rewritten (and persisted)
        // with no visible effect, or a jump in the wrong direction.
        let raw_before = model.sidebar_workspaces.clone();
        assert!(!sb.move_workspace_by_slug(&mut model, &session, "zed", true));
        assert!(model.status.contains("Pinned"));
        assert_eq!(model.sidebar_workspaces, raw_before, "raw order untouched");
        assert_eq!(visible(&model), vec!["lib", "zed", "app"]);
    }

    #[test]
    fn workspace_reorder_refused_under_attention_sort() {
        // The attention workspace-sort recomputes the order every rebuild
        // (raw order is only a tiebreak), so a manual move can't show — it
        // must be refused with a status instead of silently rewriting and
        // persisting an order the tree doesn't display.
        let _db = DbGuard::new("ws-attn");
        let session = app_session(&["home"]);
        let mut model = build_initial_model(&session, None);
        model.sidebar_workspaces = vec![
            ("app".into(), "app".into(), "repo".into(), "/tmp/app".into()),
            ("lib".into(), "lib".into(), "repo".into(), "/tmp/lib".into()),
        ];
        let mut sb = SidebarState {
            focused: true,
            ..Default::default()
        };
        sb.view.workspace_sort = thegn_core::config::WorkspaceSort::Attention;
        sb.rebuild(&mut model, &session);

        let raw_before = model.sidebar_workspaces.clone();
        assert!(!sb.move_workspace_by_slug(&mut model, &session, "lib", true));
        assert!(model.status.contains("attention"));
        assert_eq!(model.sidebar_workspaces, raw_before);
    }

    #[test]
    fn reorder_marked_workspace_block() {
        let _db = DbGuard::new("ws");
        // DB-backed workspaces (non-empty repo_path) are the reorderable ones.
        let session = app_session(&["home"]);
        let mut model = build_initial_model(&session, None);
        model.sidebar_workspaces = vec![
            ("app".into(), "app".into(), "repo".into(), "/tmp/app".into()),
            ("lib".into(), "lib".into(), "repo".into(), "/tmp/lib".into()),
            ("zed".into(), "zed".into(), "repo".into(), "/tmp/zed".into()),
        ];
        let mut session = session;
        let mut sb = focused(&mut model, &session);
        sb.cursor = vidx(&model, "app"); // a workspace header ⇒ workspace kind
        sb.marked.insert("app".into());
        sb.marked.insert("lib".into());
        sb.sync(&mut model);

        assert!(sb.reorder_selection(&mut model, &mut session, false));
        let order: Vec<String> = model
            .sidebar_workspaces
            .iter()
            .map(|(s, _, _, _)| s.clone())
            .collect();
        assert_eq!(
            order,
            vec!["zed", "app", "lib"],
            "the app+lib block moved down"
        );
    }

    #[test]
    fn workspace_reorder_persisted_order_matches_tree_after_db_reload() {
        // The Shift+Alt nav ring is rebuilt from `db.workspaces()` on the next
        // hydration, so the persisted order MUST equal what the tree shows after
        // a manual reorder. Regression: with NULL/tied positions (migrate_brand
        // / db_zones inserts), the swap+`normalize_workspace_positions` path
        // seeded positions by a `repo_path` tiebreak that differs from the
        // on-screen order, so the ring walked an order the user never arranged.
        // Persisting the whole visible order fixes it. Register real DB rows so
        // the persist + reload actually round-trips (the other reorder tests use
        // throwaway paths where the DB write no-ops).
        let _db = DbGuard::new("ws-reload");
        let db = thegn_core::db::Db::open().unwrap();
        // Tie every position to reproduce the hazard; insertion order (wc, wb,
        // wa) is deliberately the reverse of `repo_path` ASC so the old
        // normalize tiebreak would diverge from the on-screen order.
        for (path, name) in [("/tmp/wc", "wc"), ("/tmp/wb", "wb"), ("/tmp/wa", "wa")] {
            db.put_workspace(path, name, "repo").unwrap();
            db.set_workspace_position(path, 0).unwrap();
        }
        // Home group belongs to `wa` so its live-fallback slug is already in the
        // DB list — no extra trailing workspace to skew the reload assertion.
        let session = Session {
            id: "/tmp/wa".into(),
            worktrees: vec![WorktreeGroup::new("wa/home", GroupKind::Home, "/tmp/wa")],
            active: 0,
        };
        let mut model = build_initial_model(&session, None);
        // Explicit on-screen order [wc, wb, wa] (independent of any SQLite
        // tie-order), matching the registered DB rows.
        model.sidebar_workspaces = vec![
            ("wc".into(), "wc".into(), "repo".into(), "/tmp/wc".into()),
            ("wb".into(), "wb".into(), "repo".into(), "/tmp/wb".into()),
            ("wa".into(), "wa".into(), "repo".into(), "/tmp/wa".into()),
        ];
        let mut sb = focused(&mut model, &session);

        let visible = |model: &FrameModel| -> Vec<String> {
            model
                .sidebar_rows
                .iter()
                .filter(|r| r.visible && r.kind == crate::sidebar::RowKind::Workspace)
                .map(|r| r.workspace_slug.clone())
                .collect::<Vec<_>>()
        };

        // Move `wa` (bottom) up one → on-screen order [wc, wa, wb].
        assert!(sb.move_workspace_by_slug(&mut model, &session, "wa", true));
        assert_eq!(visible(&model), vec!["wc", "wa", "wb"]);

        // The reload the nav ring rebuilds from must equal the on-screen order.
        let reload: Vec<String> = crate::hydrate::workspace_list(&session, Some(&db))
            .into_iter()
            .map(|(slug, ..)| slug)
            .collect();
        assert_eq!(
            reload,
            vec!["wc", "wa", "wb"],
            "db.workspaces() reload must match the reordered tree, not a \
             normalize-tiebreak order"
        );
    }

    #[test]
    fn reorder_terminal_within_host_swaps_and_persists() {
        use thegn_core::store::WorkspaceStore;
        let _g = DbGuard::new("terminal-reorder");
        let db = thegn_core::db::Db::open().unwrap();
        // Three local terminals; insert order is the initial position order.
        db.put_terminal("a", "local", "", None).unwrap();
        db.put_terminal("b", "local", "", None).unwrap();
        db.put_terminal("c", "local", "", None).unwrap();

        let mut session = app_session(&["home"]);
        let mut model = one_ws_model(&session);
        model.sidebar_db_terminals = db.terminals().unwrap();
        let mut sb = focused(&mut model, &session);

        let term_order = |model: &FrameModel| -> Vec<String> {
            model
                .sidebar_rows
                .iter()
                .filter(|r| r.visible && r.kind == crate::sidebar::RowKind::Terminal)
                .map(|r| r.label.clone())
                .collect::<Vec<_>>()
        };
        assert_eq!(term_order(&model), vec!["a", "b", "c"]);

        // Cursor on "b", move up → swaps with its same-host sibling "a".
        sb.cursor = vidx(&model, "b");
        sb.sync(&mut model);
        assert!(sb.reorder_selection(&mut model, &mut session, true));
        assert_eq!(term_order(&model), vec!["b", "a", "c"]);
        // Persisted: a fresh registry read reflects the new order.
        let persisted: Vec<String> = db
            .terminals()
            .unwrap()
            .into_iter()
            .map(|t| t.name)
            .collect();
        assert_eq!(persisted, vec!["b", "a", "c"]);

        // The top terminal can't move up past the host boundary — no-op.
        sb.cursor = vidx(&model, "b");
        sb.sync(&mut model);
        assert!(!sb.reorder_selection(&mut model, &mut session, true));
        assert_eq!(term_order(&model), vec!["b", "a", "c"]);
    }

    #[test]
    fn reorder_terminals_do_not_cross_host_boundaries() {
        use thegn_core::store::WorkspaceStore;
        let _g = DbGuard::new("terminal-reorder-hosts");
        let db = thegn_core::db::Db::open().unwrap();
        // One local + one ssh terminal: distinct host groups (local sorts first).
        db.put_terminal("loc", "local", "", None).unwrap();
        db.put_terminal("rem", "ssh", "ssh dev@prod", None).unwrap();

        let mut session = app_session(&["home"]);
        let mut model = one_ws_model(&session);
        model.sidebar_db_terminals = db.terminals().unwrap();
        let mut sb = focused(&mut model, &session);

        // Cursor on the sole local terminal; moving down must NOT pull the remote
        // terminal into the local group — there's no same-host sibling below.
        sb.cursor = vidx(&model, "loc");
        sb.sync(&mut model);
        assert!(!sb.reorder_selection(&mut model, &mut session, false));
    }

    // --- folder-aware ordering ------------------------------------------

    const REPO: &str = "/repo/app";

    /// A model whose workspace has a real repo path plus DB folder/worktree
    /// rows, so `build_rows` renders folder headers and files worktrees under
    /// them (the plain `one_ws_model` has an empty repo path and no folders).
    fn foldered_model(
        session: &Session,
        folders: &[(i64, &str)],
        filed: &[(&str, Option<i64>)],
    ) -> FrameModel {
        let mut m = build_initial_model(session, None);
        m.sidebar_workspaces = vec![("app".into(), "app".into(), "repo".into(), REPO.into())];
        m.sidebar_db_folders = folders
            .iter()
            .enumerate()
            .map(|(i, (id, name))| thegn_core::models::FolderRow {
                folder_id: *id,
                repo_path: REPO.into(),
                name: (*name).into(),
                position: i as i64,
                created_at: 0,
            })
            .collect();
        m.sidebar_db_worktrees = filed
            .iter()
            .map(|(label, fid)| crate::sidebar::DbWorktree {
                slug: "app".into(),
                branch: (*label).into(),
                repo_path: REPO.into(),
                tab_name: format!("app/{label}"),
                path: format!("/tmp/app-{label}"),
                folder_id: *fid,
                sandbox_backend: None,
                env_name: None,
                env_degraded: false,
            })
            .collect();
        m
    }

    /// The visible workspaces tree as `(depth, label)` pairs — the shape the
    /// user sees. The TERMINALS section (and its "no terminals" hint) is
    /// dropped: it's a static peer section, not part of the ordering under test.
    fn tree(model: &FrameModel) -> Vec<(u8, String)> {
        model
            .sidebar_rows
            .iter()
            .filter(|r| {
                r.visible
                    && r.kind != crate::sidebar::RowKind::SectionHeading
                    && !r.workspace_slug.starts_with("terminals")
            })
            .map(|r| (r.depth, r.label.clone()))
            .collect()
    }

    fn cursor_on(sb: &mut SidebarState, model: &mut FrameModel, label: &str) {
        sb.cursor = vidx(model, label);
        sb.sync(model);
    }

    #[test]
    fn reorder_inside_a_folder_does_not_disturb_the_loose_run() {
        let _g = DbGuard::new("folder-inner");
        let mut session = app_session(&["home", "alpha", "beta", "gamma", "delta"]);
        let mut model = foldered_model(
            &session,
            &[(1, "One")],
            &[
                ("home", None),
                ("alpha", None),
                ("beta", None),
                ("gamma", Some(1)),
                ("delta", Some(1)),
            ],
        );
        let mut sb = focused(&mut model, &session);
        assert_eq!(
            tree(&model),
            vec![
                (0, "app".into()),
                (1, "home".into()),
                (1, "alpha".into()),
                (1, "beta".into()),
                (1, "One".into()),
                (2, "gamma".into()),
                (2, "delta".into()),
            ]
        );

        cursor_on(&mut sb, &mut model, "delta");
        assert!(sb.reorder_selection(&mut model, &mut session, true));
        assert_eq!(
            tree(&model),
            vec![
                (0, "app".into()),
                (1, "home".into()),
                (1, "alpha".into()),
                (1, "beta".into()),
                (1, "One".into()),
                (2, "delta".into()),
                (2, "gamma".into()),
            ],
            "delta rose within the folder; the loose run is untouched"
        );
    }

    #[test]
    fn stepping_up_off_a_folder_head_unfiles_into_the_loose_run() {
        let _g = DbGuard::new("folder-cross-up");
        let mut session = app_session(&["home", "alpha", "beta"]);
        let mut model = foldered_model(
            &session,
            &[(1, "One")],
            &[("home", None), ("alpha", None), ("beta", Some(1))],
        );
        let mut sb = focused(&mut model, &session);

        cursor_on(&mut sb, &mut model, "beta");
        assert!(sb.reorder_selection(&mut model, &mut session, true));
        assert_eq!(
            tree(&model),
            vec![
                (0, "app".into()),
                (1, "home".into()),
                (1, "alpha".into()),
                (1, "beta".into()),
                (1, "One".into()),
            ],
            "beta left the folder and landed at the end of the loose run"
        );
        let beta = model
            .sidebar_db_worktrees
            .iter()
            .find(|w| w.branch == "beta")
            .unwrap();
        assert_eq!(
            beta.folder_id, None,
            "membership was re-filed, not just moved"
        );
    }

    #[test]
    fn stepping_down_off_the_loose_tail_files_into_the_first_folder() {
        let _g = DbGuard::new("folder-cross-down");
        let mut session = app_session(&["home", "alpha", "beta"]);
        let mut model = foldered_model(
            &session,
            &[(1, "One")],
            &[("home", None), ("alpha", None), ("beta", Some(1))],
        );
        let mut sb = focused(&mut model, &session);

        cursor_on(&mut sb, &mut model, "alpha");
        assert!(sb.reorder_selection(&mut model, &mut session, false));
        assert_eq!(
            tree(&model),
            vec![
                (0, "app".into()),
                (1, "home".into()),
                (1, "One".into()),
                (2, "alpha".into()),
                (2, "beta".into()),
            ]
        );
        let alpha = model
            .sidebar_db_worktrees
            .iter()
            .find(|w| w.branch == "alpha")
            .unwrap();
        assert_eq!(alpha.folder_id, Some(1));
    }

    #[test]
    fn a_collapsed_folder_is_hopped_over_not_entered() {
        let _g = DbGuard::new("folder-collapsed");
        let mut session = app_session(&["home", "alpha", "beta", "gamma"]);
        let mut model = foldered_model(
            &session,
            &[(1, "One"), (2, "Two")],
            &[
                ("home", None),
                ("alpha", None),
                ("beta", Some(1)),
                ("gamma", Some(2)),
            ],
        );
        let mut sb = focused(&mut model, &session);
        sb.view.collapsed.insert("app/folder:1".into());
        sb.rebuild(&mut model, &session);

        cursor_on(&mut sb, &mut model, "alpha");
        assert!(sb.reorder_selection(&mut model, &mut session, false));
        let alpha = model
            .sidebar_db_worktrees
            .iter()
            .find(|w| w.branch == "alpha")
            .unwrap();
        assert_eq!(
            alpha.folder_id,
            Some(2),
            "skipped the collapsed folder rather than hiding alpha inside it"
        );
    }

    #[test]
    fn the_active_worktree_stays_active_across_a_reorder() {
        let _g = DbGuard::new("folder-active");
        let mut session = app_session(&["home", "alpha", "beta", "gamma"]);
        session.active = 3; // gamma
        let mut model = foldered_model(
            &session,
            &[],
            &[
                ("home", None),
                ("alpha", None),
                ("beta", None),
                ("gamma", None),
            ],
        );
        let mut sb = focused(&mut model, &session);

        cursor_on(&mut sb, &mut model, "gamma");
        assert!(sb.reorder_selection(&mut model, &mut session, true));
        assert_eq!(names(&session)[1..], ["app/alpha", "app/gamma", "app/beta"]);
        assert_eq!(
            session.worktrees[session.active].name, "app/gamma",
            "the active index followed the permuted group"
        );
    }

    #[test]
    fn ctrl_alt_moves_the_cursor_row_not_the_active_worktree() {
        let _g = DbGuard::new("cursor-not-active");
        let mut session = app_session(&["home", "alpha", "beta"]);
        session.active = 1; // alpha is active…
        let mut model = foldered_model(
            &session,
            &[],
            &[("home", None), ("alpha", None), ("beta", None)],
        );
        let mut sb = focused(&mut model, &session);

        // …but the cursor sits on beta, which is the row that must move.
        cursor_on(&mut sb, &mut model, "beta");
        assert!(sb.move_cursor_worktree(&mut model, &mut session, true));
        assert_eq!(names(&session)[1..], ["app/beta", "app/alpha"]);
    }

    #[test]
    fn folders_reorder_and_carry_their_worktrees() {
        let _g = DbGuard::new("folder-move");
        let session = app_session(&["home", "alpha", "beta"]);
        let mut model = foldered_model(
            &session,
            &[(1, "One"), (2, "Two")],
            &[("home", None), ("alpha", Some(1)), ("beta", Some(2))],
        );
        let mut sb = focused(&mut model, &session);

        assert!(sb.move_folder_id(&mut model, &session, "app", 2, true));
        assert_eq!(
            tree(&model),
            vec![
                (0, "app".into()),
                (1, "home".into()),
                (1, "Two".into()),
                (2, "beta".into()),
                (1, "One".into()),
                (2, "alpha".into()),
            ],
            "the folder moved and its worktree travelled with it"
        );
        // Already first: no further move.
        assert!(!sb.move_folder_id(&mut model, &session, "app", 2, true));
    }

    #[test]
    fn a_dormant_workspace_reorders_without_a_session_group() {
        let _g = DbGuard::new("folder-dormant");
        // No session groups for "app" at all — the rows come from the DB list.
        let mut session = Session {
            id: "s1".into(),
            worktrees: vec![],
            active: 0,
        };
        let mut model = foldered_model(
            &session,
            &[],
            &[("home", None), ("alpha", None), ("beta", None)],
        );
        let mut sb = focused(&mut model, &session);
        assert_eq!(
            tree(&model),
            vec![
                (0, "app".into()),
                (1, "home".into()),
                (1, "alpha".into()),
                (1, "beta".into()),
            ]
        );

        cursor_on(&mut sb, &mut model, "beta");
        assert!(sb.reorder_selection(&mut model, &mut session, true));
        assert_eq!(
            tree(&model),
            vec![
                (0, "app".into()),
                (1, "home".into()),
                (1, "beta".into()),
                (1, "alpha".into()),
            ]
        );
    }

    #[test]
    fn reorder_persists_the_exact_on_screen_order() {
        use thegn_core::store::WorkspaceStore;
        let _g = DbGuard::new("folder-persist");
        let db = thegn_core::db::Db::open().unwrap();
        db.put_workspace(REPO, "app", "repo").unwrap();
        for l in ["home", "alpha", "beta"] {
            db.put_worktree(
                &format!("app/{l}"),
                REPO,
                &format!("/tmp/app-{l}"),
                l,
                None,
                None,
            )
            .unwrap();
        }

        let mut session = app_session(&["home", "alpha", "beta"]);
        let mut model = foldered_model(
            &session,
            &[],
            &[("home", None), ("alpha", None), ("beta", None)],
        );
        let mut sb = focused(&mut model, &session);
        cursor_on(&mut sb, &mut model, "beta");
        assert!(sb.reorder_selection(&mut model, &mut session, true));

        // The durable order must reproduce the tree, not a normalized tiebreak.
        let on_screen: Vec<String> = model
            .sidebar_rows
            .iter()
            .filter(|r| r.visible && r.kind == crate::sidebar::RowKind::Worktree)
            .filter_map(|r| r.worktree_path.clone())
            .collect();
        let persisted: Vec<String> = db
            .worktrees()
            .unwrap()
            .into_iter()
            .map(|w| w.worktree)
            .collect();
        assert_eq!(persisted, on_screen);
    }

    // --- the MOUSE path ---------------------------------------------------
    //
    // The keyboard path has `reorder_persists_the_exact_on_screen_order`; the
    // mouse path had nothing, because `perform_drop` demands a `TerminalWaker`
    // (private field, no public constructor). These drive the waker-free seam
    // `apply_reorder_drop`, which is the same code `perform_drop` delegates to.

    /// Drive a drop by hovering `target`'s row, exactly as the loop would.
    fn drop_on(
        sb: &mut SidebarState,
        model: &mut FrameModel,
        session: &mut Session,
        src_label: &str,
        target_label: &str,
    ) -> bool {
        use crate::handlers::sidebar_mouse::{DragSrc, apply_reorder_drop, spot_for_hover};
        let row = |m: &FrameModel, label: &str| {
            m.sidebar_rows
                .iter()
                .filter(|r| r.visible)
                .position(|r| r.label == label)
                .unwrap_or_else(|| panic!("no visible row {label}"))
        };
        let src_row = model
            .sidebar_rows
            .iter()
            .find(|r| r.label == src_label)
            .unwrap()
            .clone();
        let src = DragSrc::Worktree {
            pin_key: src_row.pin_key.clone(),
            slug: src_row.workspace_slug.clone(),
            path: src_row.worktree_path.clone().unwrap(),
        };
        let spot = spot_for_hover(&model.sidebar_rows, row(model, target_label), &src);
        apply_reorder_drop(sb, model, session, &src, &spot)
    }

    fn on_screen_paths(model: &FrameModel) -> Vec<String> {
        model
            .sidebar_rows
            .iter()
            .filter(|r| r.visible && r.kind == crate::sidebar::RowKind::Worktree)
            .filter_map(|r| r.worktree_path.clone())
            .collect()
    }

    #[test]
    fn a_mouse_drop_persists_the_exact_on_screen_order() {
        use thegn_core::store::WorkspaceStore;
        let _g = DbGuard::new("mouse-persist");
        let db = thegn_core::db::Db::open().unwrap();
        db.put_workspace(REPO, "app", "repo").unwrap();
        for l in ["home", "alpha", "beta", "gamma"] {
            db.put_worktree(
                &format!("app/{l}"),
                REPO,
                &format!("/tmp/app-{l}"),
                l,
                None,
                None,
            )
            .unwrap();
        }
        let mut session = app_session(&["home", "alpha", "beta", "gamma"]);
        let mut model = foldered_model(
            &session,
            &[],
            &[
                ("home", None),
                ("alpha", None),
                ("beta", None),
                ("gamma", None),
            ],
        );
        let mut sb = focused(&mut model, &session);

        // Drop `alpha` on `gamma`, the LAST row: it must land last. Under the
        // old rule this was unreachable — the tail had no anchor to name.
        assert!(drop_on(&mut sb, &mut model, &mut session, "alpha", "gamma"));
        assert_eq!(
            tree(&model),
            vec![
                (0, "app".into()),
                (1, "home".into()),
                (1, "beta".into()),
                (1, "gamma".into()),
                (1, "alpha".into()),
            ]
        );

        let persisted: Vec<String> = db
            .worktrees()
            .unwrap()
            .into_iter()
            .map(|w| w.worktree)
            .collect();
        assert_eq!(persisted, on_screen_paths(&model));
    }

    #[test]
    fn a_mouse_drop_into_a_folder_persists_membership_and_position() {
        use thegn_core::store::WorkspaceStore;
        let _g = DbGuard::new("mouse-file");
        let db = thegn_core::db::Db::open().unwrap();
        db.put_workspace(REPO, "app", "repo").unwrap();
        for l in ["home", "alpha", "beta", "gamma"] {
            db.put_worktree(
                &format!("app/{l}"),
                REPO,
                &format!("/tmp/app-{l}"),
                l,
                None,
                None,
            )
            .unwrap();
        }
        let mut session = app_session(&["home", "alpha", "beta", "gamma"]);
        let mut model = foldered_model(
            &session,
            &[(1, "One")],
            &[
                ("home", None),
                ("alpha", None),
                ("beta", Some(1)),
                ("gamma", Some(1)),
            ],
        );
        let mut sb = focused(&mut model, &session);

        // Loose `alpha` onto `gamma`, the last member of folder 1: it is filed
        // into folder 1 AND takes gamma's slot.
        assert!(drop_on(&mut sb, &mut model, &mut session, "alpha", "gamma"));
        assert_eq!(
            tree(&model),
            vec![
                (0, "app".into()),
                (1, "home".into()),
                (1, "One".into()),
                (2, "beta".into()),
                (2, "alpha".into()),
                (2, "gamma".into()),
            ]
        );
        let alpha = model
            .sidebar_db_worktrees
            .iter()
            .find(|w| w.branch == "alpha")
            .unwrap();
        assert_eq!(alpha.folder_id, Some(1), "the drop filed it");

        // …and the tree survives the next rebuild rather than snapping back.
        sb.rebuild(&mut model, &session);
        assert_eq!(
            tree(&model).into_iter().map(|(_, l)| l).collect::<Vec<_>>(),
            vec!["app", "home", "One", "beta", "alpha", "gamma"],
        );
    }

    #[test]
    fn a_workspace_drop_applies_one_resolved_order() {
        let _g = DbGuard::new("mouse-ws-order");
        let session = app_session(&["home"]);
        let mut model = foldered_model(&session, &[], &[("home", None)]);
        // Three orderable workspaces.
        model.sidebar_workspaces = ["app", "lib", "cli"]
            .iter()
            .map(|s| {
                (
                    (*s).to_string(),
                    (*s).to_string(),
                    "repo".into(),
                    format!("/repos/{s}"),
                )
            })
            .collect();
        let mut sb = focused(&mut model, &session);
        sb.view.sort = SortMode::Manual;
        sb.rebuild(&mut model, &session);

        let order = |m: &FrameModel| -> Vec<String> {
            crate::sidebar_order::workspace_order(&m.sidebar_rows)
        };
        let before = order(&model);
        assert_eq!(before.len(), 3, "three orderable headers");

        // First onto last, in ONE apply. The old step-walk did this as a bounded
        // loop of one-slot swaps, each with its own rebuild and DB write.
        let target = vec![before[1].clone(), before[2].clone(), before[0].clone()];
        assert!(sb.apply_workspace_order(&mut model, &session, target.clone()));
        assert_eq!(order(&model), target);
    }

    #[test]
    fn a_refused_workspace_move_leaves_the_order_exactly_as_it_was() {
        let _g = DbGuard::new("mouse-ws-refuse");
        let session = app_session(&["home"]);
        let mut model = foldered_model(&session, &[], &[("home", None)]);
        model.sidebar_workspaces = ["app", "lib", "cli"]
            .iter()
            .map(|s| {
                (
                    (*s).to_string(),
                    (*s).to_string(),
                    "repo".into(),
                    format!("/repos/{s}"),
                )
            })
            .collect();
        let mut sb = focused(&mut model, &session);
        sb.view.sort = SortMode::Manual;
        sb.rebuild(&mut model, &session);
        let before = crate::sidebar_order::workspace_order(&model.sidebar_rows);
        let raw_before = model.sidebar_workspaces.clone();
        let target = vec![before[1].clone(), before[2].clone(), before[0].clone()];

        // A pinned participant: refuse, and change NOTHING. The step-walk used
        // to bail here only after having already moved the workspace part way.
        sb.view.pins = vec![before[2].clone()];
        assert!(!sb.apply_workspace_order(&mut model, &session, target.clone()));
        assert_eq!(model.sidebar_workspaces, raw_before, "an atomic refusal");
        sb.view.pins.clear();

        // Same under the attention sort, which recomputes the order every
        // rebuild and would silently discard a manual move.
        sb.view.workspace_sort = thegn_core::config::WorkspaceSort::Attention;
        assert!(!sb.apply_workspace_order(&mut model, &session, target));
        assert_eq!(model.sidebar_workspaces, raw_before, "an atomic refusal");
    }

    #[test]
    fn flat_mode_reorders_without_dissolving_folders() {
        let _g = DbGuard::new("folder-flat");
        let mut session = app_session(&["home", "alpha", "beta"]);
        let mut model = foldered_model(
            &session,
            &[(1, "One")],
            &[("home", None), ("alpha", None), ("beta", Some(1))],
        );
        let mut sb = focused(&mut model, &session);
        sb.view.flat = true;
        sb.view.sort = SortMode::Manual;
        sb.rebuild(&mut model, &session);

        cursor_on(&mut sb, &mut model, "beta");
        assert!(sb.reorder_selection(&mut model, &mut session, true));
        let beta = model
            .sidebar_db_worktrees
            .iter()
            .find(|w| w.branch == "beta")
            .unwrap();
        assert_eq!(
            beta.folder_id,
            Some(1),
            "flat mode hides folders; it must not dissolve them"
        );
    }
}
