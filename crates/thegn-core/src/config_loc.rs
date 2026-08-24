//! The `[loc]` config family — lines-of-code counting (tokei) behind the
//! bottom-bar `LOC` chip, its detail table, and the Files-section footer. Kept
//! in a sibling module rather than the god-file `config.rs`; `config.rs`
//! re-exports it.
//!
//! Counting used to have no configuration at all: a hardcoded 5-minute TTL, no
//! off switch, and a walk that only ever ran while the Files panel section
//! happened to be open — so a freshly created worktree never showed a count.
//! The walk now runs on the background measurement lane
//! ([`crate::scan_sched`]), which makes cadence and cost worth exposing.

use serde::{Deserialize, Serialize};

/// `[loc]` — per-worktree line counting. The walk is a full-tree tokei scan, so
/// it runs off the event loop on the background lane, is bounded per round, and
/// is painted from a DB cache; nothing here is ever on the render path.
#[derive(Debug, Clone, Deserialize, Serialize, schemars::JsonSchema)]
#[serde(default)]
pub struct LocConfig {
    /// Count lines of code at all. `false` hides the LOC chip, its detail table
    /// and the Files-footer count, and runs no tokei walk — the existing cache
    /// is ignored rather than merely frozen.
    pub enabled: bool,
    /// Per-worktree re-count TTL (seconds). The background scanner skips rows
    /// younger than this, and pumps at a quarter of it so a budget-bounded round
    /// still sweeps every worktree inside one window. `0` counts every pump.
    pub scan_interval_secs: u64,
    /// Worktrees counted per background round. tokei walks the whole tree, so a
    /// bounded round keeps one background-lane permit from being held for
    /// minutes on a large registry; the next pump resumes where this one
    /// stopped. `0` = unlimited.
    pub max_scan_per_round: u32,
    /// Shortest gap (seconds) between content-driven recounts of the ACTIVE
    /// worktree. Editing files is the one case where the long
    /// `scan_interval_secs` is visibly wrong, so the diff filesystem watcher may
    /// bypass it for that single path — but no more often than this, or a save
    /// storm would re-walk the tree continuously. `0` disables content-driven
    /// recounts entirely (the TTL alone governs).
    pub watch_invalidate_secs: u64,
}

impl Default for LocConfig {
    fn default() -> Self {
        LocConfig {
            enabled: true,
            // Generous by design: this is now a *background* count, so it no
            // longer needs a short TTL to feel fresh on the frame that asks for
            // it, and line counts move slowly. The active worktree's edits still
            // shorten the wait via the diff watcher's invalidation.
            scan_interval_secs: 900,
            max_scan_per_round: 2,
            watch_invalidate_secs: 60,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_enabled_and_bounded() {
        let c = LocConfig::default();
        assert!(c.enabled);
        assert_eq!(c.scan_interval_secs, 900);
        assert_eq!(c.max_scan_per_round, 2);
        assert_eq!(c.watch_invalidate_secs, 60);
    }

    #[test]
    fn partial_toml_keeps_the_other_defaults() {
        let c: LocConfig = toml::from_str("enabled = false").unwrap();
        assert!(!c.enabled);
        assert_eq!(c.scan_interval_secs, 900, "untouched key keeps its default");
    }

    #[test]
    fn round_trips_through_toml() {
        let c = LocConfig {
            enabled: false,
            scan_interval_secs: 60,
            max_scan_per_round: 7,
            watch_invalidate_secs: 5,
        };
        let back: LocConfig = toml::from_str(&toml::to_string(&c).unwrap()).unwrap();
        assert_eq!(back.scan_interval_secs, 60);
        assert_eq!(back.max_scan_per_round, 7);
        assert_eq!(back.watch_invalidate_secs, 5);
        assert!(!back.enabled);
    }
}
