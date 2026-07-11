//! Stale-while-revalidate switch cache: the last-known per-worktree slice of
//! the frame model, painted on a worktree switch so the frame shows the
//! DESTINATION worktree's data instantly (stale-but-right-worktree) while the
//! background hydration refreshes it in place.
//!
//! Before this cache only `model.panel` was cached; the tab-bar chips
//! (sandbox backend, placement, LOC, disk) and the Timeline/Containers feeds
//! kept showing the PREVIOUS worktree's values until the ~100-500ms full
//! `build_model` landed — the visible "content pop-in" on every switch.

use crate::chrome::FrameModel;

/// How long a seeded slice counts as fresh for prefetch purposes. Before this
/// TTL existed, the prefetch loop skipped any already-cached worktree
/// **forever** — a once-warmed neighbor never re-warmed, so switching to it an
/// hour later painted hour-old data until hydration landed.
pub(crate) const FRESH_TTL: std::time::Duration = std::time::Duration::from_secs(60);

/// The path-derived fields `hydrate::build_model` computes for the ACTIVE
/// worktree — everything that must swap with it on a switch.
#[derive(Default, Clone)]
pub(crate) struct WorktreeSlice {
    pub panel: crate::panel::PanelData,
    pub sandbox_backend: String,
    pub placement_kind: Option<String>,
    pub placement_label: Option<String>,
    pub loc: Option<thegn_core::loc::LocReport>,
    pub disk: Option<u64>,
    pub container_events: Vec<thegn_core::models::ContainerEvent>,
    pub timeline: Vec<thegn_core::models::TimelineEvent>,
    /// When this slice was last seeded/refreshed (`None` = never): drives the
    /// prefetch re-warm decision via [`WorktreeSlice::is_fresh`].
    pub seeded_at: Option<std::time::Instant>,
}

impl WorktreeSlice {
    /// Capture the active worktree's slice from a freshly-hydrated model
    /// (pre LSP-merge for the panel: LSP diags are editor-global).
    pub(crate) fn seed_from(model: &FrameModel) -> Self {
        WorktreeSlice {
            panel: model.panel.clone(),
            sandbox_backend: model.active_sandbox_backend.clone(),
            placement_kind: model.active_placement_kind.clone(),
            placement_label: model.active_placement_label.clone(),
            loc: model.loc.clone(),
            disk: model.active_worktree_disk,
            container_events: model.container_events.clone(),
            timeline: model.timeline.clone(),
            seeded_at: Some(std::time::Instant::now()),
        }
    }

    /// Fresh enough that a prefetch pass can skip re-warming this worktree.
    pub(crate) fn is_fresh(&self) -> bool {
        self.seeded_at.is_some_and(|t| t.elapsed() < FRESH_TTL)
    }

    /// Paint this slice into the live model (worktree switch, cache hit).
    pub(crate) fn apply(&self, model: &mut FrameModel) {
        model.panel = self.panel.clone();
        model.active_sandbox_backend = self.sandbox_backend.clone();
        model.active_placement_kind = self.placement_kind.clone();
        model.active_placement_label = self.placement_label.clone();
        model.loc = self.loc.clone();
        model.active_worktree_disk = self.disk;
        model.container_events = self.container_events.clone();
        model.timeline = self.timeline.clone();
    }

    /// Cache miss: blank the per-worktree fields rather than leaving the
    /// PREVIOUS worktree's values on screen — wrong-worktree data is worse
    /// than empty — and raise `panel_pending` so the panel renders its
    /// skeleton (dim placeholder bars) instead of a bare void while the
    /// hydration is in flight. The next accepted hydration's model swap
    /// clears the flag (a fresh `build_model` carries `false`).
    pub(crate) fn clear(model: &mut FrameModel) {
        WorktreeSlice::default().apply(model);
        model.panel_pending = true;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn model_with(backend: &str, kind: Option<&str>, disk: Option<u64>) -> FrameModel {
        FrameModel {
            active_sandbox_backend: backend.to_string(),
            active_placement_kind: kind.map(str::to_string),
            active_placement_label: kind.map(|k| format!("label:{k}")),
            active_worktree_disk: disk,
            ..Default::default()
        }
    }

    #[test]
    fn seed_apply_round_trips_the_per_worktree_fields() {
        let src = model_with("bwrap", Some("ssh"), Some(42));
        let slice = WorktreeSlice::seed_from(&src);

        let mut dst = model_with("podman", Some("k8s"), Some(7));
        slice.apply(&mut dst);
        assert_eq!(dst.active_sandbox_backend, "bwrap");
        assert_eq!(dst.active_placement_kind.as_deref(), Some("ssh"));
        assert_eq!(dst.active_placement_label.as_deref(), Some("label:ssh"));
        assert_eq!(dst.active_worktree_disk, Some(42));
    }

    #[test]
    fn clear_blanks_stale_chips_instead_of_keeping_previous_worktree() {
        let mut model = model_with("podman", Some("k8s"), Some(7));
        model.timeline = vec![];
        WorktreeSlice::clear(&mut model);
        assert!(model.active_sandbox_backend.is_empty());
        assert!(model.active_placement_kind.is_none());
        assert!(model.active_placement_label.is_none());
        assert!(model.active_worktree_disk.is_none());
        assert!(model.container_events.is_empty());
        assert!(model.timeline.is_empty());
        assert!(model.loc.is_none());
    }
}
