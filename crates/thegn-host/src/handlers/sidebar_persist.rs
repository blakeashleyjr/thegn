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
//! | `other_open:<slug>`  | `"1"` open / `"0"` collapsed (absent=`[ui] sidebar_other_group_collapsed`) | toggle_collapse on the "Other" group |
//! | `batch_open:<key>`   | `"1"` (absent=collapsed) | toggle_collapse on a batch row (`batch:{slug}/{token}`) |
//! | `more_open:<key>`    | `"1"` (absent=truncated to the row limit) | toggle_show_more ("show more"/"show fewer") |
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
    /// Worktree paths whose destroy workers are in flight. Process-local only:
    /// groups remain in the session (and persisted layout) until completion.
    pub(crate) deleting: std::collections::HashSet<String>,
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
    /// THE-2470: worktree paths whose delete completed, with the hydration
    /// generation current when the completion was applied (`None` until stamped).
    pub(crate) recently_deleted: std::collections::HashMap<String, Option<u64>>,
    /// THE-2463 Q1: the preview project row a pending create added: (seq, repo path).
    pub(crate) create_preview: Option<(u64, String)>,
    /// THE-2737: the focused tab the user just marked unread. The loop's
    /// focus-ack skips it so the filled dot is not re-read on the very next
    /// frame; cleared once focus leaves the tab or its dot leaves `Waiting`,
    /// so focusing it again reads it as normal. Loop-side only, never persisted.
    pub(crate) unread_hold: Option<String>,
}

impl SidebarState {
    pub(crate) fn note_deleted(&mut self, path: &str) {
        self.recently_deleted.insert(path.to_string(), None);
    }

    pub(crate) fn stamp_deleted(&mut self, hydration_gen: u64) {
        for generation in self.recently_deleted.values_mut() {
            if generation.is_none() {
                *generation = Some(hydration_gen);
            }
        }
    }

    /// Drop entries a newer hydration supersedes (stamped gen < generation),
    /// then remove rows whose path is still recorded.
    pub(crate) fn filter_deleted(
        &mut self,
        generation: u64,
        rows: &mut Vec<crate::sidebar::DbWorktree>,
    ) {
        self.recently_deleted
            .retain(|_, stamped| stamped.is_none_or(|stamp| stamp >= generation));
        rows.retain(|row| !self.recently_deleted.contains_key(&row.path));
    }

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
            } else if let Some(group) = key.strip_prefix("more_open:") {
                if value == "1" {
                    self.view.more_open.insert(group.to_string());
                } else {
                    // best-effort: lazy sweep of a legacy tombstone row
                    let _ = db.del_ui_state(scope, &key);
                }
            } else if let Some(batch) = key.strip_prefix("batch_open:") {
                if value == "1" {
                    self.view.batch_open.insert(batch.to_string());
                } else {
                    // best-effort: lazy sweep of a legacy tombstone row
                    let _ = db.del_ui_state(scope, &key);
                }
            } else if let Some(slug) = key.strip_prefix("other_open:") {
                // Both values are real overrides of the config default, so
                // neither is a tombstone; anything else is junk and is swept.
                match value.as_str() {
                    "1" => {
                        self.view.other_open.insert(slug.to_string(), true);
                    }
                    "0" => {
                        self.view.other_open.insert(slug.to_string(), false);
                    }
                    _ => {
                        // best-effort: lazy sweep of an unreadable row
                        let _ = db.del_ui_state(scope, &key);
                    }
                }
            }
        }
    }

    /// Record (and persist) whether a project's derived "Other" group is
    /// open. Stored explicitly in both directions: the default it overrides
    /// is `[ui] sidebar_other_group_collapsed`, which a config edit can flip,
    /// so "absent" cannot stand for either state.
    pub(crate) fn set_other_open(&mut self, slug: &str, open: bool) {
        self.view.other_open.insert(slug.to_string(), open);
        self.persist(&format!("other_open:{slug}"), if open { "1" } else { "0" });
    }

    /// Record (and persist) whether a batch row is open. Collapsed is the
    /// default, so closing deletes the key rather than tombstoning it.
    pub(crate) fn set_batch_open(&mut self, key: &str, open: bool) {
        let ui_key = format!("batch_open:{key}");
        if open {
            self.view.batch_open.insert(key.to_string());
            self.persist(&ui_key, "1");
        } else {
            self.view.batch_open.remove(key);
            self.unpersist(&ui_key);
        }
    }

    /// Record (and persist) whether a row-limited group is expanded past its
    /// limit. `group` is the group head's collapse key. Truncated is the
    /// default, so collapsing deletes the key rather than tombstoning it.
    pub(crate) fn set_more_open(&mut self, group: &str, open: bool) {
        let key = format!("more_open:{group}");
        if open {
            self.view.more_open.insert(group.to_string());
            self.persist(&key, "1");
        } else {
            self.view.more_open.remove(group);
            self.unpersist(&key);
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

    fn db_worktree(path: &str) -> crate::sidebar::DbWorktree {
        crate::sidebar::DbWorktree {
            path: path.into(),
            slug: String::new(),
            branch: String::new(),
            repo_path: String::new(),
            tab_name: String::new(),
            folder_id: None,
            sandbox_backend: None,
            env_name: None,
            sandbox_profile: None,
            env_degraded: false,
            external: false,
        }
    }

    #[test]
    fn filter_deleted_hides_a_row_from_an_older_hydration() {
        let mut sb = SidebarState::default();
        sb.note_deleted("/gone");
        sb.stamp_deleted(7);
        let mut rows = vec![db_worktree("/gone"), db_worktree("/kept")];

        sb.filter_deleted(7, &mut rows);

        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].path, "/kept");
        assert_eq!(sb.recently_deleted.get("/gone"), Some(&Some(7)));
    }

    #[test]
    fn filter_deleted_forgets_the_path_once_a_newer_hydration_arrives() {
        let mut sb = SidebarState::default();
        sb.note_deleted("/gone");
        sb.stamp_deleted(7);
        let mut rows = vec![db_worktree("/gone")];

        sb.filter_deleted(8, &mut rows);

        assert_eq!(rows.len(), 1);
        assert!(sb.recently_deleted.is_empty());
    }

    #[test]
    fn unstamped_deletes_are_filtered() {
        let mut sb = SidebarState::default();
        sb.note_deleted("/gone");
        let mut rows = vec![db_worktree("/gone")];

        sb.filter_deleted(8, &mut rows);

        assert!(rows.is_empty());
        assert_eq!(sb.recently_deleted.get("/gone"), Some(&None));
    }

    #[test]
    fn persist_does_not_block_on_a_held_write_lock() {
        let state = crate::testenv::IsolatedState::new();
        let db = thegn_core::db::Db::open().unwrap();
        drop(db);
        let db_path = state.state_home().join("thegn/thegn.db");
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
        let _state = crate::testenv::IsolatedState::new();
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

    /// The "Other" group's per-project open state round-trips through
    /// `ui_state`: written by `set_other_open`, read back by `load`, with
    /// junk values swept rather than misread.
    #[test]
    fn other_open_round_trips_through_ui_state() {
        let _state = crate::testenv::IsolatedState::new();
        let db = thegn_core::db::Db::open().unwrap();
        drop(db);

        let mut sidebar = SidebarState::default();
        sidebar.set_other_open("app", true);
        sidebar.set_other_open("lib", false);
        assert_eq!(sidebar.view.other_open.get("app"), Some(&true));
        let rows = read_sidebar_state_from_writer().unwrap();
        assert!(
            rows.iter()
                .any(|(key, value)| key == "other_open:app" && value == "1")
        );
        assert!(
            rows.iter()
                .any(|(key, value)| key == "other_open:lib" && value == "0")
        );

        let db = thegn_core::db::Db::open_memory().unwrap();
        for (key, value) in rows {
            db.set_ui_state(SIDEBAR_SCOPE, &key, &value).unwrap();
        }
        db.set_ui_state(SIDEBAR_SCOPE, "other_open:junk", "maybe")
            .unwrap();
        let mut loaded = SidebarState::default();
        loaded.load(&db, SIDEBAR_SCOPE);
        assert_eq!(loaded.view.other_open.get("app"), Some(&true));
        assert_eq!(loaded.view.other_open.get("lib"), Some(&false));
        assert_eq!(loaded.view.other_open.get("junk"), None);
        assert!(
            !db.ui_state_in_scope(SIDEBAR_SCOPE)
                .unwrap()
                .iter()
                .any(|(key, _)| key == "other_open:junk")
        );
    }

    /// A group's "show more" expansion round-trips through `ui_state`:
    /// `set_more_open(true)` writes `more_open:<key>`, `load` restores it, and
    /// collapsing again deletes the key (truncated is the default).
    #[test]
    fn more_open_round_trips_through_ui_state() {
        let _state = crate::testenv::IsolatedState::new();
        let db = thegn_core::db::Db::open().unwrap();
        drop(db);

        let mut sidebar = SidebarState::default();
        sidebar.set_more_open("app/folder:3", true);
        sidebar.set_more_open("other:lib", true);
        sidebar.set_more_open("other:lib", false);
        assert!(sidebar.view.more_open.contains("app/folder:3"));
        assert!(!sidebar.view.more_open.contains("other:lib"));
        let rows = read_sidebar_state_from_writer().unwrap();
        assert!(
            rows.iter()
                .any(|(key, value)| key == "more_open:app/folder:3" && value == "1")
        );
        assert!(!rows.iter().any(|(key, _)| key == "more_open:other:lib"));

        let db = thegn_core::db::Db::open_memory().unwrap();
        for (key, value) in rows {
            db.set_ui_state(SIDEBAR_SCOPE, &key, &value).unwrap();
        }
        db.set_ui_state(SIDEBAR_SCOPE, "more_open:stale", "0")
            .unwrap();
        let mut loaded = SidebarState::default();
        loaded.load(&db, SIDEBAR_SCOPE);
        assert!(loaded.view.more_open.contains("app/folder:3"));
        assert!(!loaded.view.more_open.contains("stale"));
        assert!(
            !db.ui_state_in_scope(SIDEBAR_SCOPE)
                .unwrap()
                .iter()
                .any(|(key, _)| key == "more_open:stale")
        );
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

    /// A batch row's open state round-trips through `ui_state`; closing
    /// deletes the key and a stray `"0"` is swept on load.
    #[test]
    fn batch_open_round_trips_through_ui_state() {
        let _state = crate::testenv::IsolatedState::new();
        let db = thegn_core::db::Db::open().unwrap();
        drop(db);

        let mut sidebar = SidebarState::default();
        sidebar.set_batch_open("batch:app/b158", true);
        sidebar.set_batch_open("batch:app/b159", true);
        sidebar.set_batch_open("batch:app/b159", false);
        assert!(sidebar.view.batch_open.contains("batch:app/b158"));
        assert!(!sidebar.view.batch_open.contains("batch:app/b159"));
        let rows = read_sidebar_state_from_writer().unwrap();
        assert!(
            rows.iter()
                .any(|(key, value)| key == "batch_open:batch:app/b158" && value == "1")
        );
        assert!(
            !rows
                .iter()
                .any(|(key, _)| key == "batch_open:batch:app/b159")
        );

        let db = thegn_core::db::Db::open_memory().unwrap();
        for (key, value) in rows {
            db.set_ui_state(SIDEBAR_SCOPE, &key, &value).unwrap();
        }
        db.set_ui_state(SIDEBAR_SCOPE, "batch_open:batch:app/stale", "0")
            .unwrap();
        let mut loaded = SidebarState::default();
        loaded.load(&db, SIDEBAR_SCOPE);
        assert!(loaded.view.batch_open.contains("batch:app/b158"));
        assert!(!loaded.view.batch_open.contains("batch:app/stale"));
        assert!(
            !db.ui_state_in_scope(SIDEBAR_SCOPE)
                .unwrap()
                .iter()
                .any(|(key, _)| key == "batch_open:batch:app/stale")
        );
    }
}
