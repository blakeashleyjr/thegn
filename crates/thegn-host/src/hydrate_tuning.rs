//! Env-overridable cadence knobs for background hydration, extracted from
//! `hydrate.rs` (pinned at its god-file ratchet ceiling): the model
//! re-hydration ticker interval and the background-glyph staleness TTL. Both are
//! pure (read an env var, clamp, return a `Duration`) and unit-tested via
//! `hydrate`'s cadence-invariant test.

use std::time::Duration;

/// Default for [`model_refresh_interval`]. Matches `bg_glyph_ttl`'s 5s default
/// (the ticker's only job is refreshing background glyphs + the activity FSM);
/// must stay a multiple of the 500ms base that divides `PR_REFRESH_INTERVAL` so
/// the ticker keeps emitting `RefreshKind::Pr` (see the cadence-invariant test).
pub(crate) const DEFAULT_MODEL_REFRESH_MS: u64 = 5000;

/// Safety-net cadence for the background model re-hydration ticker. The *active*
/// worktree's panel + git glyphs already update in real time off the diff
/// fs-watcher (`retarget_diff_watcher`), so this tick exists only to (a) refresh
/// *background* worktrees' sidebar glyphs — themselves capped to the
/// `bg_glyph_ttl` (5s) staleness window, so ticking faster does no extra git
/// work — and (b) advance the activity-dot FSM (`activity::poll_and_save`, which
/// is wall-normalized and so stays correct at any cadence; dots just react up to
/// one tick later). The default therefore matches that 5s TTL.
///
/// It was 1s, which rebuilt the whole model — a ~0.3-0.4s `git` fan-out — every
/// second even when fully idle. `FrameModel::hydration_eq` drops the idle
/// *frame*, but NOT the wasted *build CPU*; on this thread that redundant rebuild
/// was the dominant idle/agent-active hydration cost. Override with
/// `THEGN_MODEL_REFRESH_MS` (lower = snappier dots/glyphs, more background git
/// work). Clamped to a multiple of the 500ms ticker base, min 500ms.
pub(crate) fn model_refresh_interval() -> Duration {
    let ms = std::env::var("THEGN_MODEL_REFRESH_MS")
        .ok()
        .and_then(|s| s.parse::<u64>().ok())
        .unwrap_or(DEFAULT_MODEL_REFRESH_MS)
        .max(500);
    Duration::from_millis((ms / 500) * 500)
}

/// Staleness window for background-worktree git glyphs. The active worktree is
/// always rescanned; others reuse the cache until this elapses. Default 5s,
/// override with `THEGN_BG_MODEL_REFRESH_MS` (`0` = always rescan, i.e. the
/// old every-worktree-every-tick behavior).
pub(crate) fn bg_glyph_ttl() -> Duration {
    let ms = std::env::var("THEGN_BG_MODEL_REFRESH_MS")
        .ok()
        .and_then(|s| s.parse::<u64>().ok())
        .unwrap_or(5000);
    Duration::from_millis(ms)
}
