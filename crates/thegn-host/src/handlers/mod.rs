//! Event-loop drain handlers extracted from `run.rs` (which is pinned by the
//! file-size ratchet): each submodule owns the loop-side handling of one
//! off-thread producer's channel, taking the loop locals it mutates as a
//! context struct. The loop calls one `drain_*` per wake; everything here runs
//! ON the loop and must stay I/O-free.

pub(crate) mod attention;
pub(crate) mod close;
pub(crate) mod crash;
pub(crate) mod creating;
pub(crate) mod daemon_lifecycle;
pub(crate) mod host;
pub(crate) mod host_heal;
pub(crate) mod materialize;
pub(crate) mod merge_queue;
pub(crate) mod overlay;
pub(crate) mod pane_zoom;
pub(crate) mod panel_changes;
pub(crate) mod provision;
pub(crate) mod repo_trust;
pub(crate) mod sidebar_actions;
pub(crate) mod sidebar_activate;
pub(crate) mod sidebar_collapse;
pub(crate) mod sidebar_folder;
pub(crate) mod sidebar_keys;
pub(crate) mod sidebar_mouse;
pub(crate) mod sidebar_persist;
pub(crate) mod sidebar_reorder;
pub(crate) mod startup;
pub(crate) mod switch;
pub(crate) mod switch_cache;
pub(crate) mod terminal;
pub(crate) mod tracker;
pub(crate) mod wizard;
pub(crate) mod worktree_delete;

/// Persist a first-launch keymap-preset choice (item 621) to `ui_state` and
/// record it on `cfg`, returning the status line to show. The caller rebuilds
/// the live keymap from `cfg` (that reassignment stays on the loop). Extracted
/// from the pinned `run.rs`.
pub(crate) fn apply_keymap_preset(preset: &str, cfg: &mut thegn_core::config::Config) -> String {
    use thegn_core::store::WorkspaceStore;
    // best-effort: the preset also rides `cfg`; a failed persist just re-asks.
    if let Ok(db) = thegn_core::db::Db::open() {
        let _ = db.set_ui_state("", "keymap_preset", preset);
    }
    cfg.keymap_preset = preset.to_string();
    if preset == "default" {
        "Keymap: thegn defaults".into()
    } else {
        format!("Keymap preset: {preset}")
    }
}
