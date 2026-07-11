//! The sidebar's interaction state ([`SidebarState`]) and its persisted view
//! state. Extracted from ratchet-pinned `run.rs`; loop-coupled methods
//! (`rebuild`/`sync`/`effective_cols`) stay in `run.rs` impl blocks, key
//! handling lives in [`crate::handlers::sidebar_keys`].
//!
//! ## Persisted key inventory (`ui_state`, scope [`SIDEBAR_SCOPE`])
//!
//! | key                  | value                | written by                        |
//! |----------------------|----------------------|-----------------------------------|
//! | `collapse:<key>`     | `"1"` (absent=open)  | toggle_collapse / collapse_parent |
//! | `pin:<key>`          | `"1"` (absent=unpinned) | toggle_pin                     |
//! | `sort_mode`          | `SortMode::as_str()` | sort menu                         |
//! | `sidebar_cols`       | width in columns     | adjust_width (`<`/`>`)            |
//! | `sidebar_expanded`   | `"1"`/`"0"`          | `e` wide toggle                   |
//! | `sidebar_mode`       | `SidebarMode::as_key()` | ToggleSidebar cycle            |
//!
//! `<key>` is the row's stable identity: a workspace slug, `{slug}/{branch}`
//! for worktrees, `{slug}/folder:{id}` for folders, `terminals/host:{key}` for
//! terminal host groups. Boolean keys are DELETED when they return to their
//! default state (never tombstoned with `"0"`); `load` sweeps legacy `"0"`
//! rows as it reads. Entity removal prunes its keys by prefix (see
//! `del_ui_state_prefix` call sites).
//!
//! Scope contract: this is process-global view state — the sidebar is a single
//! global tree showing every workspace at once, so it is NOT keyed by the
//! active workspace. Two *separate* stores it must not be confused with: the
//! `""`-scope `active_workspace` pointer (which workspace hydrates first) and
//! per-session `session_state.active_tab` (which worktree tab focus restores
//! to).

use superzej_core::store::WorkspaceStore;

use crate::chrome::FrameModel;

/// `ui_state` scope for the sidebar's persisted view state. The sidebar is a
/// single global tree showing every workspace at once, so its view state
/// (pins, collapse, sort, width, expand) is process-global — NOT keyed by the
/// active workspace. (Mirrors the right panel's `"panel"` scope. Keying this by
/// `session.id`, which is the active workspace's repo path, stranded pins in
/// per-workspace scopes so they never reloaded.)
pub(crate) const SIDEBAR_SCOPE: &str = "sidebar";

/// Interaction + persisted view state for the workspace tree (items 16–27).
/// The single source of truth the event loop mutates; `SidebarState::rebuild`
/// (in `run.rs`) derives `FrameModel`'s sidebar fields from it plus the
/// model's data carriers.
#[derive(Default)]
pub(crate) struct SidebarState {
    pub(crate) view: crate::sidebar::ViewState,
    pub(crate) focused: bool,
    /// Cursor over the *visible* rows.
    pub(crate) cursor: usize,
    pub(crate) filtering: bool,
    /// Marked rows for bulk actions (item 26), keyed by the stable per-row
    /// `pin_key` so the selection survives rebuilds (collapse/sort/filter/
    /// hydration/reorder) instead of drifting when row indices shift.
    pub(crate) marked: std::collections::HashSet<String>,
    /// Open context menu, if any (item 27).
    pub(crate) menu: Option<crate::sidebar_view::RowMenu>,
    /// Adjustable bar width in columns (item 25); `None` = layout default.
    pub(crate) width: Option<usize>,
    /// Wide expand toggle (`e`): mirrors the panel's expand affordance. When
    /// set, the sidebar claims ~half the window, ignoring `width`.
    pub(crate) expanded: bool,
    /// Display mode cycled by `ToggleSidebar`: full panel, slim rail, hidden.
    pub(crate) mode: crate::layout::SidebarMode,
    /// Desired top visible-row index of the scroll window; `build_sidebar`
    /// clamps it each frame so the cursor row stays in view.
    pub(crate) scroll: usize,
    /// Group names of worktrees mid-creation; `rebuild` overlays a loading dot
    /// on their rows (a build in flight has no CPU-based activity yet).
    pub(crate) creating: std::collections::HashSet<String>,
}

impl SidebarState {
    /// Load persisted collapse/sort/pins/width from `ui_state` for this session.
    /// Legacy tombstone rows (`"0"` for the boolean `collapse:`/`pin:` keys,
    /// written before deletes replaced tombstones) are swept as they're read.
    pub(crate) fn load(&mut self, db: &superzej_core::db::Db, scope: &str) {
        for (key, value) in db.ui_state_in_scope(scope).unwrap_or_default() {
            if let Some(slug) = key.strip_prefix("collapse:") {
                if value == "1" {
                    self.view.collapsed.insert(slug.to_string());
                } else {
                    // best-effort: lazy sweep of a legacy tombstone row
                    let _ = db.del_ui_state(scope, &key);
                }
            } else if let Some(slug) = key.strip_prefix("pin:") {
                if value == "1" {
                    if !self.view.pins.contains(&slug.to_string()) {
                        self.view.pins.push(slug.to_string());
                    }
                } else {
                    // best-effort: lazy sweep of a legacy tombstone row
                    let _ = db.del_ui_state(scope, &key);
                }
            } else if key == "sort_mode" {
                self.view.sort = crate::sidebar::SortMode::from_str(&value);
                // Normalize legacy spellings ("activity") to the canonical
                // string once, so the stored value always round-trips.
                if value != self.view.sort.as_str() {
                    // best-effort: DB is a cache; worst case we normalize again
                    let _ = db.set_ui_state(scope, "sort_mode", self.view.sort.as_str());
                }
            } else if key == "sidebar_cols" {
                self.width = value.parse().ok();
            } else if key == "sidebar_expanded" {
                self.expanded = value == "1";
            } else if key == "sidebar_mode" {
                self.mode = crate::layout::SidebarMode::from_key(&value);
            }
        }
    }

    /// Persist a single `ui_state` key in the global [`SIDEBAR_SCOPE`].
    pub(crate) fn persist(&self, key: &str, value: &str) {
        if let Ok(db) = superzej_core::db::Db::open() {
            // best-effort: the DB is a cache; a failed persist only loses a
            // view preference, never sidebar correctness
            let _ = db.set_ui_state(SIDEBAR_SCOPE, key, value);
        }
    }

    /// Delete a single `ui_state` key in the global [`SIDEBAR_SCOPE`] — the
    /// counterpart of [`Self::persist`] for boolean keys returning to their
    /// default (unpinned / expanded), which are removed rather than tombstoned.
    pub(crate) fn unpersist(&self, key: &str) {
        if let Ok(db) = superzej_core::db::Db::open() {
            // best-effort: same cache rule as `persist`
            let _ = db.del_ui_state(SIDEBAR_SCOPE, key);
        }
    }

    /// The currently-selected visible row, if any.
    pub(crate) fn selected_row<'a>(
        &self,
        model: &'a FrameModel,
    ) -> Option<&'a crate::sidebar::SidebarRow> {
        model
            .sidebar_rows
            .iter()
            .filter(|r| r.visible)
            .nth(self.cursor)
    }

    /// Number of currently-visible rows.
    pub(crate) fn visible_len(model: &FrameModel) -> usize {
        model.sidebar_rows.iter().filter(|r| r.visible).count()
    }
}
