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
    /// Deadline for the post-navigation grace freeze — the window after an
    /// `Alt+↑/↓` / `Alt+<digit>` jump made from a **pane**, where the sidebar
    /// never takes focus so the focus-edge freeze can't cover it. `None` = no
    /// grace pending (the freeze, if any, is held by focus instead).
    ///
    /// LOOP-SIDE ONLY, and evaluated LAZILY at the top of `rebuild` — never
    /// armed as a timer and never a wake source, so the loop still blocks on
    /// `poll_input(None)` and idles at 0%. The 500ms hydration ticker
    /// guarantees the expiry is noticed within a tick of the deadline.
    pub(crate) freeze_until: Option<std::time::Instant>,
    /// Mirror of `[ui] sidebar_freeze_sort`, seeded at startup and on config
    /// reload beside `view.workspace_sort`. Held here rather than read from the
    /// config at each call site because the arm/thaw points are spread across
    /// the key handlers, which have no config in scope.
    ///
    /// `Default` is `false` (the derive), i.e. a `SidebarState` that never saw
    /// a config simply doesn't freeze — the conservative direction: a test
    /// fixture gets today's unheld behaviour rather than a silent hold.
    pub(crate) freeze_sort: bool,
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
    /// Queue this best-effort cache write on the FIFO DB writer. The startup
    /// loader is the only production reader, and queued writes preserve the
    /// user's set/delete order against other ordered DB jobs.
    pub(crate) fn persist(&self, key: &str, value: &str) {
        let key = key.to_string();
        let value = value.to_string();
        crate::db_task::persist(move |db| {
            if let Err(error) = db.set_ui_state(SIDEBAR_SCOPE, &key, &value) {
                tracing::debug!(target: "thegn::sidebar", %error, "could not persist sidebar state");
            }
        });
    }

    /// Delete a single `ui_state` key in the global [`SIDEBAR_SCOPE`] — the
    /// counterpart of [`Self::persist`] for boolean keys returning to their
    /// default (unpinned / expanded), which are removed rather than tombstoned.
    pub(crate) fn unpersist(&self, key: &str) {
        let key = key.to_string();
        crate::db_task::persist(move |db| {
            if let Err(error) = db.del_ui_state(SIDEBAR_SCOPE, &key) {
                tracing::debug!(target: "thegn::sidebar", %error, "could not delete sidebar state");
            }
        });
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

/// FIFO barrier/read-back on the writer's own connection, so tests observe
/// queued sidebar mutations without opening a second DB connection.
#[cfg(test)]
pub(crate) fn read_ui_state_from_writer(scope: String) -> anyhow::Result<Vec<(String, String)>> {
    let (tx, rx) = std::sync::mpsc::channel();
    crate::db_task::persist(move |db| {
        if tx.send(db.ui_state_in_scope(&scope)).is_err() {
            tracing::debug!(target: "thegn::sidebar", "sidebar read-back receiver closed");
        }
    });
    rx.recv_timeout(std::time::Duration::from_secs(10))?
}

#[cfg(test)]
pub(crate) fn read_sidebar_state_from_writer() -> anyhow::Result<Vec<(String, String)>> {
    read_ui_state_from_writer(SIDEBAR_SCOPE.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn persist_does_not_block_on_a_held_write_lock() {
        let state_home = tempfile::Builder::new()
            .prefix("tg-sidebar-lock-")
            .tempdir()
            .unwrap();
        let state_path = state_home.path().to_string_lossy().into_owned();
        let _env = crate::testenv::EnvVarGuard::set(&[("XDG_STATE_HOME", &state_path)]);
        let db = thegn_core::db::Db::open().unwrap();
        drop(db);
        let db_path = state_home.path().join("thegn/thegn.db");
        let lock = rusqlite::Connection::open(db_path).unwrap();
        lock.execute_batch("BEGIN IMMEDIATE").unwrap();

        let started = std::time::Instant::now();
        SidebarState::default().persist("sidebar_cols", "42");
        assert!(started.elapsed() < std::time::Duration::from_millis(500));

        lock.execute_batch("ROLLBACK").unwrap();
        drop(lock);
        let rows = read_sidebar_state_from_writer().unwrap();
        assert!(
            rows.iter()
                .any(|(key, value)| key == "sidebar_cols" && value == "42")
        );
    }

    #[test]
    fn persist_then_unpersist_keeps_order() {
        let state_home = tempfile::Builder::new()
            .prefix("tg-sidebar-order-")
            .tempdir()
            .unwrap();
        let state_path = state_home.path().to_string_lossy().into_owned();
        let _env = crate::testenv::EnvVarGuard::set(&[("XDG_STATE_HOME", &state_path)]);
        let db = thegn_core::db::Db::open().unwrap();
        drop(db);

        let sidebar = SidebarState::default();
        sidebar.persist("pin:ordered", "1");
        sidebar.unpersist("pin:ordered");

        let rows = read_sidebar_state_from_writer().unwrap();
        assert!(!rows.iter().any(|(key, _)| key == "pin:ordered"));
    }

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
