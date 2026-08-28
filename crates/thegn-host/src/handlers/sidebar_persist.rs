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
//! | `sidebar_flat`       | `"1"` (absent=grouped) | toggle_flat (`g`)               |
//! | `sidebar_focus_detail` | `FocusDetail::as_str()` (absent=`[ui]` config) | cycle_focus_detail (`i`) |
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

use thegn_core::store::WorkspaceStore;

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
    /// When the last OPTIMISTIC edit to the model's sidebar DB lists
    /// (`sidebar_db_worktrees` / `sidebar_db_folders`) happened — reorder,
    /// re-file, folder create/delete all mutate the model on the gesture's
    /// frame and defer the durable write off-loop. While fresh, the model swap
    /// keeps the loop's lists instead of adopting a hydration whose DB read
    /// predates the write (which visibly snapped the row back, then re-moved
    /// it when the write's own refresh landed).
    pub(crate) optimistic_db_edit_at: Option<std::time::Instant>,
    /// Adjustable bar width in columns (item 25); `None` = layout default.
    pub(crate) width: Option<usize>,
    /// Wide expand toggle (`e`): mirrors the panel's expand affordance. When
    /// set, the sidebar claims ~half the window, ignoring `width`.
    pub(crate) expanded: bool,
    /// Runtime override for `[ui] sidebar_focus_detail`, cycled by `i`.
    /// `None` = follow config. Held separately from `view.display` because a
    /// config reload rebuilds that struct from `[ui]` and would drop it.
    pub(crate) focus_detail_override: Option<thegn_core::config::FocusDetail>,
    /// Display mode cycled by `ToggleSidebar`: full panel, slim rail, hidden.
    pub(crate) mode: crate::layout::SidebarMode,
    /// Top visible-row index of the scroll window. FIRST-CLASS state,
    /// independent of the cursor: `build_sidebar` only clamps it to
    /// `[0, max_sidebar_scroll]`, and revealing the cursor is
    /// `handlers::sidebar_scroll`'s job. Deliberately NOT persisted — row
    /// identity isn't stable across restarts (hydration order, repos resolving
    /// late, out-of-band worktree add/prune), so a restored *index* would point
    /// at a different row; see the key inventory above.
    pub(crate) scroll: usize,
    /// The cursor position `settle_scroll` last revealed. Lets the pre-render
    /// settle be skipped in O(1) when nothing moved, which is what keeps a
    /// pane-output frame from paying for a heights pass.
    pub(crate) revealed_cursor: usize,
    /// Window cache stamped by `scroll_by`/`settle_scroll`, for the O(1)
    /// `reanchor_cursor` check and the PageUp/Down step. Frame-derived and
    /// LOOP-SIDE ONLY — `build_sidebar` must never read these, or paint and
    /// hit-test could disagree about the same frame.
    pub(crate) first_visible: usize,
    pub(crate) last_visible: usize,
    pub(crate) last_list_rows: usize,
    /// Group names of worktrees mid-creation; `rebuild` overlays a loading dot
    /// on their rows (a build in flight has no CPU-based activity yet).
    pub(crate) creating: std::collections::HashSet<String>,
    /// Group names of worktrees whose env bring-up failed (mirrors the loop's
    /// `materialize_failed`/`prewarm_failed`); `rebuild` overlays a red error
    /// dot so the failure stays visible after the halt modal is dismissed.
    pub(crate) env_failed: std::collections::HashSet<String>,
}

impl SidebarState {
    /// Load persisted collapse/sort/pins/width from `ui_state` for this session.
    /// Legacy tombstone rows (`"0"` for the boolean `collapse:`/`pin:` keys,
    /// written before deletes replaced tombstones) are swept as they're read.
    pub(crate) fn load(&mut self, db: &thegn_core::db::Db, scope: &str) {
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
                self.width = value.parse().ok(); // best-effort: optional input: an unparseable persisted width means 'no override'
            } else if key == "sidebar_expanded" {
                self.expanded = value == "1";
            } else if key == "sidebar_flat" {
                self.view.flat = value == "1";
            } else if key == "sidebar_focus_detail" {
                // best-effort: an unparseable stored value just falls back to
                // the `[ui]` config value rather than failing the load.
                self.focus_detail_override =
                    thegn_core::config::FocusDetail::from_str_validated(&value).ok();
            } else if key == "sidebar_mode" {
                self.mode = crate::layout::SidebarMode::from_key(&value);
            }
        }
    }

    /// The effective per-row detail mode: the runtime override (`i`) if the
    /// user has cycled it this install, else whatever `[ui]` config resolved to.
    pub(crate) fn focus_detail(&self) -> thegn_core::config::FocusDetail {
        self.focus_detail_override
            .unwrap_or(self.view.display.focus_detail)
    }

    /// Persist a single `ui_state` key in the global [`SIDEBAR_SCOPE`].
    ///
    /// Kept synchronous: a view-preference toggle is rare and the write is tiny,
    /// and callers (+ tests) rely on read-after-write within the same session
    /// (re-loading a fresh `SidebarState` immediately reflects the change). The
    /// off-loop background writer would break that ordering for negligible gain.
    pub(crate) fn persist(&self, key: &str, value: &str) {
        if let Ok(db) = thegn_core::db::Db::open() {
            // best-effort: the DB is a cache; a failed persist only loses a
            // view preference, never sidebar correctness
            let _ = db.set_ui_state(SIDEBAR_SCOPE, key, value); // best-effort: cache write: the DB is a cache; git/forge stays the source of truth
        }
    }

    /// Delete a single `ui_state` key in the global [`SIDEBAR_SCOPE`] — the
    /// counterpart of [`Self::persist`] for boolean keys returning to their
    /// default (unpinned / expanded), which are removed rather than tombstoned.
    pub(crate) fn unpersist(&self, key: &str) {
        if let Ok(db) = thegn_core::db::Db::open() {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn load_reads_sidebar_flat() {
        // The `sidebar_flat` ui_state key restores the flat layout on startup.
        let db = thegn_core::db::Db::open_memory().unwrap();
        db.set_ui_state(SIDEBAR_SCOPE, "sidebar_flat", "1").unwrap();
        let mut sb = SidebarState::default();
        sb.load(&db, SIDEBAR_SCOPE);
        assert!(sb.view.flat);

        // Absent key → grouped (the default).
        let db2 = thegn_core::db::Db::open_memory().unwrap();
        let mut sb2 = SidebarState::default();
        sb2.load(&db2, SIDEBAR_SCOPE);
        assert!(!sb2.view.flat);
    }

    #[test]
    fn load_reads_sidebar_focus_detail() {
        use thegn_core::config::FocusDetail;
        let db = thegn_core::db::Db::open_memory().unwrap();
        db.set_ui_state(SIDEBAR_SCOPE, "sidebar_focus_detail", "cursor")
            .unwrap();
        let mut sb = SidebarState::default();
        sb.load(&db, SIDEBAR_SCOPE);
        assert_eq!(sb.focus_detail_override, Some(FocusDetail::Cursor));
        assert_eq!(sb.focus_detail(), FocusDetail::Cursor);
    }

    /// With no stored override the effective mode follows `[ui]` config, and a
    /// junk stored value degrades to config rather than failing the load.
    #[test]
    fn focus_detail_falls_back_to_config() {
        use thegn_core::config::FocusDetail;
        let db = thegn_core::db::Db::open_memory().unwrap();
        db.set_ui_state(SIDEBAR_SCOPE, "sidebar_focus_detail", "nonsense")
            .unwrap();
        let mut sb = SidebarState::default();
        sb.view.display.focus_detail = FocusDetail::Off;
        sb.load(&db, SIDEBAR_SCOPE);
        assert_eq!(sb.focus_detail_override, None);
        assert_eq!(sb.focus_detail(), FocusDetail::Off, "config wins");
    }
}
