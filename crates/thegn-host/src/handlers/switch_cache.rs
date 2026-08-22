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
    /// The hydration-resolved container name (profile-aware, in-env path for
    /// remote/provider worktrees). Loop-side switch paths used to recompute
    /// this profile-blind from the local path, flipping System ▸ Sandbox to
    /// "not sandboxed" on every switch under a `[profile]` and defeating the
    /// idle guard for remote worktrees; it now travels with the slice.
    pub container_name: String,
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
    /// (pre LSP-merge for the panel: LSP diags live in their own per-root
    /// store and are re-merged on every paint).
    pub(crate) fn seed_from(model: &FrameModel) -> Self {
        WorktreeSlice {
            panel: model.panel.clone(),
            sandbox_backend: model.active_sandbox_backend.clone(),
            container_name: model.active_container_name.clone(),
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
        // Now-playing is loop-owned and player-global, not per-worktree —
        // carry it across the panel swap or a switch blinks the badge off
        // until the next media push.
        let media = model.panel.media.take();
        model.panel = self.panel.clone();
        model.panel.media = media;
        model.active_sandbox_backend = self.sandbox_backend.clone();
        model.active_container_name = self.container_name.clone();
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

/// Cache-miss switch: blank the stale per-worktree fields (skeleton), then kick
/// a fast interactive-lane panel-only build for `cwd`. It's the same cheap
/// `build_panel` the neighbor prefetch uses (no sidebar rebuild / `git log` /
/// LOC / disk — the ~1s tail of the full `build_model`), but on the interactive
/// lane so the cold worktree's changes list lands ASAP and replaces the
/// skeleton. Ships on the prefetch channel; [`drain_prefetch_results`] applies
/// it to the live frame the moment it arrives, while still active + pending.
pub(crate) fn clear_and_fill(
    model: &mut FrameModel,
    cwd: &std::path::Path,
    tx: &tokio::sync::mpsc::UnboundedSender<(std::path::PathBuf, crate::panel::PanelData)>,
    hints: &crate::hydrate::HydrateHints,
    waker: &termwiz::terminal::TerminalWaker,
) {
    WorktreeSlice::clear(model);
    let (tx, cwd, hints, waker) = (tx.clone(), cwd.to_path_buf(), hints.clone(), waker.clone());
    tokio::task::spawn_blocking(move || {
        if !cwd.is_dir() {
            return;
        }
        let Ok(db) = thegn_core::db::Db::open() else {
            return;
        };
        let cfg = crate::hydrate::load_hydration_config();
        let panel = {
            let _g = crate::perf::measure(crate::perf::Subsys::Hydrate);
            crate::hydrate::build_panel(&cwd, &db, &hints, &cfg)
        };
        if tx.send((cwd, panel)).is_ok() {
            let _ = waker.wake();
        }
    });
}

/// Drain the prefetch/fast-fill channel: every result seeds the switch cache;
/// a fast-fill for the still-active, still-`panel_pending` worktree also paints
/// the live frame (skeleton → real changes list), leaving neighbor warms
/// repaint-free. Returns whether the live frame changed (caller repaints).
pub(crate) fn drain_prefetch_results(
    rx: &mut tokio::sync::mpsc::UnboundedReceiver<(std::path::PathBuf, crate::panel::PanelData)>,
    cache: &mut std::collections::HashMap<std::path::PathBuf, WorktreeSlice>,
    inflight: &mut crate::handlers::prefetch_policy::PrefetchInflight,
    model: &mut FrameModel,
    session: &crate::session::Session,
    lsp: &crate::lsp::LspDiagnostics,
    loop_perf: &mut crate::perf::LoopPerf,
) -> bool {
    let mut painted = false;
    while let Ok((path, panel)) = rx.try_recv() {
        loop_perf.tick(crate::perf::WakeSource::Prefetch);
        // Release the dedupe guard: this path may warm again after its TTL.
        inflight.finish(&path);
        // Keyed like the switch detection: a terminal never matches a dir's
        // prefetch, so the launch-dir worktree's panel can't paint onto it.
        let is_active = path == crate::hydrate::active_slice_key(session);
        let slice = cache.entry(path).or_default();
        slice.panel = panel.clone();
        // A prefetched panel is fresh — stamps the re-warm TTL.
        slice.seeded_at = Some(std::time::Instant::now());
        // A fast-fill for the worktree the user just cold-switched to (still
        // active + pending) paints the live frame: preserve loop-owned
        // now-playing media and re-merge this worktree's LSP diags (the fresh
        // panel carries only git/db diags), mirroring the full model swap.
        if is_active && model.panel_pending {
            let media = model.panel.media.take();
            model.panel = panel;
            model.panel.media = media;
            if !lsp.is_empty() {
                lsp.merge_into(
                    &crate::hydrate::active_tab_path(session),
                    &mut model.panel.diagnostics,
                );
            }
            model.panel_pending = false;
            painted = true;
        }
    }
    painted
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
