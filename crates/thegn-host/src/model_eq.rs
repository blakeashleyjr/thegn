//! The hydration idle-guard equality for [`FrameModel`] (extracted from the
//! ratchet-pinned `chrome.rs`).

use crate::chrome::FrameModel;

/// What the process tab is allowed to present from the loop-owned snapshot.
/// A sampler publication changes this to `Fresh`; visibility transitions and
/// terminal sampler errors deliberately make the old snapshot unavailable.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum ProcessViewState {
    #[default]
    Waiting,
    Fresh,
    Failed(&'static str),
}

/// Semantic revision for hydrated row caches. Exhaustion disables caching
/// instead of wrapping into an old valid key.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct ContentRevision {
    value: u64,
    exhausted: bool,
}
impl ContentRevision {
    fn advanced(self, changed: bool) -> Self {
        if !changed || self.exhausted {
            return self;
        }
        match self.value.checked_add(1) {
            Some(value) => Self {
                value,
                exhausted: false,
            },
            None => Self {
                value: self.value,
                exhausted: true,
            },
        }
    }
    pub fn same_cacheable(self, other: Self) -> bool {
        !self.exhausted && !other.exhausted && self.value == other.value
    }
}

impl FrameModel {
    /// The compositor's sole live process publisher. Reconcile visibility
    /// before final take so a sample queued across pause cannot replace the
    /// frozen model which navigation and signal confirmation read.
    pub fn take_process_publication(
        &mut self,
        control: &crate::proc_worker::Control,
        enabled: bool,
    ) -> bool {
        control.set_enabled(enabled);
        let Some(publication) = control.take_latest() else {
            return false;
        };
        self.process_revision = publication.revision;
        self.procs = publication.snapshot;
        self.process_state = ProcessViewState::Fresh;
        true
    }

    /// Drop process data when the live Processes view is no longer active.
    /// Keeping the publication revision avoids pretending a UI transition is
    /// a sampler publication; `ProcessViewState` is part of monitor identity.
    pub fn invalidate_processes(&mut self) {
        self.procs = Default::default();
        self.process_state = ProcessViewState::Waiting;
    }

    /// Replace a previously visible snapshot with an explicit sampler error.
    pub fn fail_processes(&mut self, reason: &'static str) {
        self.procs = Default::default();
        self.process_state = ProcessViewState::Failed(reason);
    }

    /// Hydration owns Git/DB state, while the process sampler owns this snapshot.
    /// Transfer it at the authoritative swap so hydration cannot blank a live
    /// process table between samples. Moving avoids copying the bounded row set.
    /// Disk row inputs receive a semantic revision without cloning either map.
    pub fn carry_monitor_state_from(&mut self, current: &mut Self) {
        let disk_changed = self.sidebar_status.disk_sizes != current.sidebar_status.disk_sizes
            || self.sidebar_status.disk_stamps != current.sidebar_status.disk_stamps;
        self.monitor_disk_revision = current.monitor_disk_revision.advanced(disk_changed);
        if self.procs_disabled {
            self.invalidate_processes();
        } else {
            self.procs = std::mem::take(&mut current.procs);
            self.process_revision = current.process_revision;
            self.process_state = current.process_state;
        }
    }

    /// True when a freshly hydrated model carries no render-affecting change
    /// versus the one on screen — i.e. the 2 s "safety" refresh tick produced
    /// byte-identical git/db data. The event loop uses this to drain the
    /// hydration result without repainting (and to carry the previous
    /// `sidebar_rows` over the model swap instead of an on-loop `build_rows`),
    /// keeping idle CPU at ~0%.
    ///
    /// Compares exactly the fields [`crate::hydrate::build_model`] populates,
    /// and nothing else (`status` is loop-owned — `handlers::status_line`): stats/metrics/containers/accent/
    /// bars/pins/app-tabs are owned by other handlers or config and have their
    /// own dirty triggers, while the session-derived tab/sidebar fields are
    /// stable during an idle period. KEEP THIS IN SYNC WITH `build_model` —
    /// every input `sidebar::build_rows` reads MUST be compared here, or the
    /// rows carry-over serves stale rows without a repaint.
    pub fn hydration_eq(&self, other: &Self) -> bool {
        self.state_db == other.state_db
            && self.worktree == other.worktree
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
            && self.panel == other.panel
            && self.disk_warn_threshold_gb == other.disk_warn_threshold_gb
            && self.procs_disabled == other.procs_disabled
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
        other.procs.total = 42;
        other.procs.enabled = true;
        // Loop-owned like `sidebar_focused`: re-derived by `SidebarState::sync`
        // after every model swap, so hydration equality must not read it.
        // Pinned here deliberately — if it ever joined `hydration_eq`, arming
        // the freeze would itself count as "hydration changed" and force the
        // repaint the idle guard exists to avoid.
        other.sidebar_sort_frozen = true;
        // Loop-owned, like `stats`: pushed by the weather task, never by
        // hydration — which is exactly why the swap in `run.rs` has to CARRY it
        // (a dropped snapshot is not visible here, it just blanks the widget
        // until the next poll, up to half an hour later).
        other.weather = Some(thegn_core::weather::WeatherSnapshot {
            provider: "wttr_in".into(),
            place: "Berlin".into(),
            sky: thegn_core::weather::Sky::Clear,
            description: "Sunny".into(),
            temp: 24.0,
            feels_like: 25.0,
            hi: 26.0,
            lo: 14.0,
            humidity_pct: 41,
            wind: 8.0,
            units: thegn_core::weather::Units::Metric,
            fetched_at: 1_700_000_000,
            forecast: Vec::new(),
        });
        other.app_tabs.push("chat".into());
        other.preview = Some(crate::chrome::PreviewView {
            worktree: "/repo".into(),
            port: 5173,
            url: "http://localhost:5173/".into(),
            source: thegn_core::preview::PortHintSource::PaneOutput,
            status: thegn_core::preview::PreviewStatus::Up,
        });
        other.sidebar_selected = 3;
        other.center_focused = !base.center_focused;
        other.status = "Copied log line".into();
        assert!(
            base.hydration_eq(&other),
            "non-hydration fields should not trip the idle guard"
        );
    }

    #[test]
    fn hydration_eq_detects_real_changes() {
        let base = FrameModel::default();
        let mut db_state_changed = base.clone();
        db_state_changed.state_db = crate::chrome::StateDbAvailability::SchemaRefused {
            observed: 68,
            build: 67,
        };
        assert!(
            !base.hydration_eq(&db_state_changed),
            "availability change must repaint persistent chrome"
        );

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
        loc_changed.loc = Some(thegn_core::loc::LocReport::total_only(42));
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
            .push(thegn_core::models::FolderRow {
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
            .push(thegn_core::models::TerminalRow {
                id: 1,
                name: "build-box".into(),
                kind: "ssh".into(),
                connection_string: "ssh build".into(),
                folder_id: None,
                created_at: 0,
                last_active: 0,
                position: 0,
                sandbox_backend: String::new(),
                observed_backend: String::new(),
                env_name: String::new(),
            });
        assert!(
            !base.hydration_eq(&term_changed),
            "terminal change must repaint"
        );
    }
}

#[cfg(test)]
mod revision_tests {
    use super::*;
    #[test]
    fn disk_revision_tracks_both_input_maps_and_never_reuses_exhausted_keys() {
        let mut prior = FrameModel::default();
        let initial = prior.monitor_disk_revision;
        let mut next = prior.clone();
        next.sidebar_status
            .disk_sizes
            .insert("fixture".into(), (1, 2));
        next.carry_monitor_state_from(&mut prior);
        assert!(!initial.same_cacheable(next.monitor_disk_revision));
        let sizes = next.monitor_disk_revision;
        let mut stamp = next.clone();
        stamp
            .sidebar_status
            .disk_stamps
            .insert("fixture".into(), 100);
        stamp.carry_monitor_state_from(&mut next);
        assert!(!sizes.same_cacheable(stamp.monitor_disk_revision));
        let exhausted = ContentRevision {
            value: u64::MAX,
            exhausted: false,
        }
        .advanced(true);
        assert!(!exhausted.same_cacheable(exhausted));
        assert_eq!(exhausted.advanced(true), exhausted);
    }

    #[test]
    fn hydration_does_not_carry_processes_when_sampling_is_disabled() {
        let mut prior = FrameModel::default();
        prior.process_state = ProcessViewState::Fresh;
        prior.procs.enabled = true;
        prior.procs.total = 1;
        let mut next = FrameModel::default();
        next.procs_disabled = true;
        next.carry_monitor_state_from(&mut prior);
        assert_eq!(next.process_state, ProcessViewState::Waiting);
        assert!(!next.procs.enabled);
        assert_eq!(next.procs.total, 0);
    }
}
