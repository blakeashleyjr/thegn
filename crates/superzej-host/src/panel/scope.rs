//! Panel view-scope toggles — global, render-time flags for the two "show
//! everything" escape hatches in the right-hand panel.
//!
//! By default the panel is scoped to the **active worktree's repo**: the "My
//! Work" (Mine) feed shows only that repo's issues/PRs, and the System tab shows
//! only that repo's notifications / containers. A one-key toggle flips each back
//! to the platform-wide view. There is exactly one panel per session, so — like
//! the render-cap holder (`caps.rs`) and the chrome palette — the flags live in
//! process-global atomics read at hydrate time rather than threaded through
//! every `HydrateHints` / `build_panel` call site.

use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};

/// "My Work" (Mine): when set, load the cross-repo `ALL_SCOPE` feed instead of
/// the active repo's scoped feed.
static MINE_ALL: AtomicBool = AtomicBool::new(false);
/// System tab: when set, show platform-wide notifications / containers instead
/// of only the active repo's.
static SYSTEM_ALL: AtomicBool = AtomicBool::new(false);

/// Whether the "My Work" feed is showing every repo (the toggle is on).
pub fn mine_all() -> bool {
    MINE_ALL.load(Ordering::Relaxed)
}

/// Set the "My Work" all-repos toggle; returns the new value.
pub fn set_mine_all(on: bool) -> bool {
    MINE_ALL.store(on, Ordering::Relaxed);
    on
}

/// Flip the "My Work" all-repos toggle; returns the new value.
pub fn toggle_mine_all() -> bool {
    set_mine_all(!mine_all())
}

/// Whether the System tab is showing platform-wide data (the toggle is on).
pub fn system_all() -> bool {
    SYSTEM_ALL.load(Ordering::Relaxed)
}

/// Set the System-tab all toggle; returns the new value.
pub fn set_system_all(on: bool) -> bool {
    SYSTEM_ALL.store(on, Ordering::Relaxed);
    on
}

/// Flip the System-tab all toggle; returns the new value.
pub fn toggle_system_all() -> bool {
    set_system_all(!system_all())
}

/// The active worktree's log tag (`log_trace::wt_slug` of its path). Set once per
/// active-model hydration (`build_model`) and read by the Logs section to keep
/// only this worktree's + host-global lines by default. There is one active
/// worktree, so — like the toggles above — it lives in a process-global holder
/// rather than threading through every `FrameModel` construction site.
static ACTIVE_WT_TAG: Mutex<String> = Mutex::new(String::new());

/// Record the active worktree's log tag (no-op if unchanged).
pub fn set_active_wt_tag(tag: &str) {
    if let Ok(mut g) = ACTIVE_WT_TAG.lock()
        && *g != tag
    {
        *g = tag.to_string();
    }
}

/// The active worktree's log tag, or empty when none is set.
pub fn active_wt_tag() -> String {
    ACTIVE_WT_TAG.lock().map(|g| g.clone()).unwrap_or_default()
}

#[cfg(test)]
mod spec {
    use super::*;

    #[test]
    fn toggles_flip_and_report() {
        // Independent flags; default off. (Serialized within one test to avoid
        // cross-test races on the process-global atomics.)
        set_mine_all(false);
        set_system_all(false);
        assert!(!mine_all() && !system_all());
        assert!(toggle_mine_all());
        assert!(mine_all());
        assert!(!system_all()); // system unaffected by the mine toggle
        assert!(!toggle_mine_all());
        assert!(toggle_system_all());
        assert!(system_all());
        set_mine_all(false);
        set_system_all(false);
    }
}
