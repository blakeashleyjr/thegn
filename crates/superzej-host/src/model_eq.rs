//! The hydration idle-guard equality for [`FrameModel`] (extracted from the
//! ratchet-pinned `chrome.rs`).

use crate::chrome::FrameModel;

impl FrameModel {
    /// True when a freshly hydrated model carries no render-affecting change
    /// versus the one on screen — i.e. the 2 s "safety" refresh tick produced
    /// byte-identical git/db data. The event loop uses this to drain the
    /// hydration result without repainting (and to carry the previous
    /// `sidebar_rows` over the model swap instead of an on-loop `build_rows`),
    /// keeping idle CPU at ~0%.
    ///
    /// Compares exactly the fields [`crate::hydrate::build_model`] populates
    /// (plus `status`), and nothing else: stats/metrics/containers/accent/
    /// bars/pins/app-tabs are owned by other handlers or config and have their
    /// own dirty triggers, while the session-derived tab/sidebar fields are
    /// stable during an idle period. KEEP THIS IN SYNC WITH `build_model` —
    /// every input `sidebar::build_rows` reads MUST be compared here, or the
    /// rows carry-over serves stale rows without a repaint.
    pub fn hydration_eq(&self, other: &Self) -> bool {
        self.worktree == other.worktree
            && self.tabs == other.tabs
            && self.active_tab == other.active_tab
            && self.sidebar_workspaces == other.sidebar_workspaces
            && self.sidebar_db_worktrees == other.sidebar_db_worktrees
            && self.sidebar_db_folders == other.sidebar_db_folders
            && self.sidebar_db_terminals == other.sidebar_db_terminals
            && self.sidebar_status == other.sidebar_status
            && self.loc == other.loc
            && self.active_container_name == other.active_container_name
            && self.active_sandbox_backend == other.active_sandbox_backend
            && self.active_placement_kind == other.active_placement_kind
            && self.active_placement_label == other.active_placement_label
            && self.container_events == other.container_events
            && self.timeline == other.timeline
            && self.status == other.status
            && self.panel == other.panel
            && self.disk_warn_threshold_gb == other.disk_warn_threshold_gb
            && self.active_worktree_disk == other.active_worktree_disk
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hydration_eq_ignores_non_hydration_fields() {
        let base = FrameModel::default();
        // Fields owned by other handlers / config must NOT count as a change,
        // or the idle guard would still repaint on every safety tick.
        let mut other = base.clone();
        other.accent = "ff0000".into();
        other.pins.push(crate::pins::PinChip {
            index: 0,
            label: "p".into(),
            glyph: '●',
        });
        other.stats.cpu_pct = Some(99);
        other.app_tabs.push("chat".into());
        other.sidebar_selected = 3;
        other.center_focused = !base.center_focused;
        assert!(
            base.hydration_eq(&other),
            "non-hydration fields should not trip the idle guard"
        );
    }

    #[test]
    fn hydration_eq_detects_real_changes() {
        let base = FrameModel::default();
        let mut panel_changed = base.clone();
        panel_changed.panel.branch = "feature".into();
        assert!(
            !base.hydration_eq(&panel_changed),
            "panel change must repaint"
        );

        let mut sidebar_changed = base.clone();
        sidebar_changed
            .sidebar_status
            .activity
            .insert("tab".into(), crate::sidebar::ActivityState::Active);
        assert!(
            !base.hydration_eq(&sidebar_changed),
            "sidebar status change must repaint"
        );

        let mut loc_changed = base.clone();
        loc_changed.loc = Some(superzej_core::loc::LocReport::total_only(42));
        assert!(!base.hydration_eq(&loc_changed), "loc change must repaint");
    }

    /// Regression: folders + terminals ARE build_rows inputs that build_model
    /// populates — a hydration changing only them must repaint (and must not
    /// take the rows carry-over path). This was a silent gap before the
    /// carry-over existed.
    #[test]
    fn hydration_eq_detects_folder_and_terminal_changes() {
        let base = FrameModel::default();

        let mut folder_changed = base.clone();
        folder_changed
            .sidebar_db_folders
            .push(superzej_core::models::FolderRow {
                folder_id: 1,
                repo_path: "/tmp/app".into(),
                name: "wip".into(),
                position: 0,
                created_at: 0,
            });
        assert!(
            !base.hydration_eq(&folder_changed),
            "folder change must repaint"
        );

        let mut term_changed = base.clone();
        term_changed
            .sidebar_db_terminals
            .push(superzej_core::models::TerminalRow {
                id: 1,
                name: "build-box".into(),
                kind: "ssh".into(),
                connection_string: "ssh build".into(),
                folder_id: None,
                created_at: 0,
                last_active: 0,
                position: 0,
                sandbox_backend: String::new(),
                env_name: String::new(),
            });
        assert!(
            !base.hydration_eq(&term_changed),
            "terminal change must repaint"
        );
    }
}
