//! Env-overridable cadence knobs for background hydration, extracted from
//! `hydrate.rs` (pinned at its god-file ratchet ceiling). The ticker interval,
//! background-glyph policy, active-row floor, and terminal resync cadence are
//! pure env readers/builders and are unit-tested without mutating the process env.

use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

/// Default for [`model_refresh_interval`]. It must stay a multiple of the
/// 500ms base that divides `PR_REFRESH_INTERVAL` so the ticker keeps emitting
/// `RefreshKind::Pr` (see the cadence-invariant test).
pub(crate) const DEFAULT_MODEL_REFRESH_MS: u64 = 5000;
const DEFAULT_BG_BUSY_GLYPH_TTL_MS: u64 = 10_000;
const DEFAULT_BG_IDLE_GLYPH_TTL_MS: u64 = 120_000;

/// Background glyph policy windows and the cadence used to interpret hints.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct GlyphTtls {
    pub(crate) busy: Duration,
    pub(crate) idle: Duration,
    pub(crate) floor: Duration,
    pub(crate) tick: Duration,
}

/// Build policy windows from explicit values. A zero busy TTL deliberately
/// preserves the always-rescan opt-out; otherwise the busy window stays above
/// one model tick and idle work is never checked more frequently than busy work.
pub(crate) fn glyph_ttls_from(
    busy_ms: Option<u64>,
    idle_ms: Option<u64>,
    floor: Duration,
    tick: Duration,
) -> GlyphTtls {
    let busy_ms = busy_ms.unwrap_or(DEFAULT_BG_BUSY_GLYPH_TTL_MS);
    let idle_ms = idle_ms.unwrap_or(DEFAULT_BG_IDLE_GLYPH_TTL_MS);
    let busy = if busy_ms == 0 {
        Duration::ZERO
    } else {
        Duration::from_millis(busy_ms).max(tick.saturating_add(Duration::from_millis(500)))
    };
    let idle = if busy.is_zero() {
        Duration::from_millis(idle_ms)
    } else {
        Duration::from_millis(idle_ms).max(busy)
    };
    GlyphTtls {
        busy,
        idle,
        floor,
        tick,
    }
}

/// Current policy. `THEGN_BG_MODEL_REFRESH_MS` defaults to 10s (`0` always
/// rescans background rows); `THEGN_BG_IDLE_GLYPH_TTL_MS` defaults to 120s.
/// Both are env-only knobs, and the idle window is raised to at least busy.
pub(crate) fn glyph_ttls() -> GlyphTtls {
    let busy_ms = std::env::var("THEGN_BG_MODEL_REFRESH_MS")
        .ok()
        .and_then(|value| value.parse::<u64>().ok());
    let idle_ms = std::env::var("THEGN_BG_IDLE_GLYPH_TTL_MS")
        .ok()
        .and_then(|value| value.parse::<u64>().ok());
    glyph_ttls_from(
        busy_ms,
        idle_ms,
        active_glyph_floor(),
        model_refresh_interval(),
    )
}

/// Stable FNV-1a path hash mapped to a half-open [0, 500) permille range.
pub(crate) fn path_jitter_permille(path: &Path) -> u32 {
    let mut hash = 0xcbf29ce484222325_u64;
    for byte in path.as_os_str().as_encoded_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x100000001b3);
    }
    (hash % 500) as u32
}

/// Add deterministic per-path jitter in [ttl, 1.5 * ttl).
pub(crate) fn jittered(ttl: Duration, path: &Path) -> Duration {
    let extra_ms = ttl
        .as_millis()
        .saturating_mul(u128::from(path_jitter_permille(path)))
        / 1000;
    ttl.saturating_add(Duration::from_millis(
        extra_ms.min(u128::from(u64::MAX)) as u64
    ))
}

/// Whether a recent activity/ref observation happened after this row's scan.
pub(crate) fn touched_since_scan(
    cached_age: Duration,
    since_active: Option<Duration>,
    since_ref_move: Option<Duration>,
    slop: Duration,
) -> bool {
    since_active.is_some_and(|age| age <= cached_age.saturating_add(slop))
        || since_ref_move.is_some_and(|age| age < cached_age)
}

/// Select the rescan window for one row from its activity hints.
pub(crate) fn glyph_window(
    is_active: bool,
    touched: bool,
    path: &Path,
    ttls: &GlyphTtls,
) -> Duration {
    if is_active {
        ttls.floor
    } else if ttls.busy.is_zero() {
        Duration::ZERO
    } else if touched {
        jittered(ttls.busy, path)
    } else {
        jittered(ttls.idle, path)
    }
}

/// Ref-generation observation kept per watched path.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct RefObs {
    pub(crate) path: PathBuf,
    pub(crate) generation: u64,
    pub(crate) changed_at: Option<Instant>,
}

/// Advance a ref-generation observation without counting first sight or a path switch as a move.
pub(crate) fn next_ref_obs(
    prev: Option<RefObs>,
    path: &Path,
    generation: Option<u64>,
    now: Instant,
) -> Option<RefObs> {
    let generation = generation?;
    match prev {
        Some(prev) if prev.path == path && prev.generation == generation => Some(prev),
        Some(prev) if prev.path == path => Some(RefObs {
            path: path.to_path_buf(),
            generation,
            changed_at: Some(now),
        }),
        _ => Some(RefObs {
            path: path.to_path_buf(),
            generation,
            changed_at: None,
        }),
    }
}

/// Observe the active watcher's ref generation and return the last move time.
pub(crate) fn observe_ref_generation(
    path: &Path,
    generation: Option<u64>,
    now: Instant,
) -> Option<Instant> {
    static OBS: OnceLock<Mutex<Option<RefObs>>> = OnceLock::new();
    let mut obs = OBS
        .get_or_init(|| Mutex::new(None))
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    *obs = next_ref_obs(obs.take(), path, generation, now);
    obs.as_ref().and_then(|current| current.changed_at)
}

/// Stamp a warm-cache row so its idle window expires in a staggered first wave.
pub(crate) fn warm_stamp(now: Instant, path: &Path, ttls: &GlyphTtls) -> Instant {
    let stagger = ttls
        .busy
        .as_millis()
        .saturating_mul(u128::from(path_jitter_permille(path)))
        / 1000;
    warm_stamp_from_age(
        now,
        jittered(ttls.idle, path).saturating_sub(Duration::from_millis(
            stagger.min(u128::from(u64::MAX)) as u64,
        )),
    )
}

fn warm_stamp_from_age(now: Instant, age: Duration) -> Instant {
    now.checked_sub(age).unwrap_or(now)
}

/// Safety-net cadence for the background model re-hydration ticker. This tick
/// advances the activity-dot FSM and checks background glyphs against their
/// independent busy/idle windows. The active worktree's panel + git glyphs
/// update in real time off the diff fs-watcher (`retarget_diff_watcher`).
///
/// It was 1s, which rebuilt the whole model — a ~0.3-0.4s `git` fan-out — every
/// second even when fully idle. `FrameModel::hydration_eq` drops the idle
/// *frame*, but NOT the wasted *build CPU*; on this thread that redundant rebuild
/// was the dominant idle/agent-active hydration cost. Override with
/// `THEGN_MODEL_REFRESH_MS` (lower = snappier dots/activity, more hydration
/// work). Clamped to a multiple of the 500ms ticker base, min 500ms.
pub(crate) fn model_refresh_interval() -> Duration {
    let ms = std::env::var("THEGN_MODEL_REFRESH_MS")
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .unwrap_or(DEFAULT_MODEL_REFRESH_MS)
        .max(500);
    Duration::from_millis((ms / 500) * 500)
}

/// Minimum age before the ACTIVE worktree's git glyphs are rescanned.
///
/// The active row used to be exempt from staleness entirely — `is_active` meant
/// "always rescan" — justified by the fact that its diff fs-watcher already
/// forces immediate refreshes. That reasoning is circular: the watcher is the
/// thing firing, and (before the watch registration was pruned) it fired on
/// build output, so every 500ms debounce window spent a full git fan-out
/// including a `<base>...HEAD` three-dot diff. On a large repo that measured at
/// ~0.65 spawns/second, each taking most of a core.
///
/// A floor, not a TTL: it never *delays* a scan past this window, it only stops
/// two scans inside it. Small enough to stay imperceptible on a real edit —
/// the watcher's own debounce is 500ms, so this is the tighter of the two only
/// when several watchers fire at once. Override with `THEGN_ACTIVE_GLYPH_FLOOR_MS`
/// (`0` restores the old always-rescan behavior).
pub(crate) fn active_glyph_floor() -> Duration {
    let ms = std::env::var("THEGN_ACTIVE_GLYPH_FLOOR_MS")
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .unwrap_or(300);
    Duration::from_millis(ms)
}

/// Cadence for the flash-free full-screen "resync" heal. thegn's diff renderer
/// assumes `front` (its baseline) mirrors the physical screen EXACTLY; every
/// frame it emits only the `front`→`scratch` delta. If the outer terminal
/// drifts out-of-band — the classic case is Ghostty (and other terminals that
/// send NO same-size SIGWINCH on window refocus, see the resize handler in
/// `run.rs`) repainting its alt-screen imperfectly when you focus back — the
/// bounded diff can't know, so it leaves the orphaned rows stale forever: the
/// "doubled lines" bug. Periodically we reset the baseline and let the normal
/// `diff_screens` re-emit every cell IN PLACE — no `ClearScreen`, so no flash;
/// unchanged cells are overwritten with themselves while any drifted cell
/// heals. This only ever runs on a frame that is ALREADY being produced (it
/// piggybacks inside `should_render`), so an idle loop stays idle — the 0%-idle
/// invariant holds. Default 3s; `THEGN_RESYNC_MS=0` disables it entirely.
pub(crate) fn resync_interval() -> Duration {
    let ms = std::env::var("THEGN_RESYNC_MS")
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .unwrap_or(3000);
    Duration::from_millis(ms)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn busy_ttl_is_strictly_greater_than_the_tick_and_zero_still_opts_out() {
        let tick = Duration::from_secs(5);
        let clamped = glyph_ttls_from(Some(5000), Some(1000), Duration::from_millis(300), tick);
        assert_eq!(clamped.busy, Duration::from_millis(5500));
        assert_eq!(clamped.idle, clamped.busy);

        let disabled = glyph_ttls_from(Some(0), Some(120_000), Duration::ZERO, tick);
        let window = glyph_window(false, false, Path::new("/wt/a"), &disabled);
        assert!(window.is_zero());
    }

    #[test]
    fn jitter_is_deterministic_and_bounded() {
        let path = Path::new("/worktrees/alpha");
        let permille = path_jitter_permille(path);
        let ttl = Duration::from_secs(10);
        assert_eq!(permille, path_jitter_permille(path));
        assert!(permille < 500);
        let window = jittered(ttl, path);
        assert!(window >= ttl);
        assert!(window < ttl + ttl / 2);
    }

    #[test]
    fn activity_since_scan_uses_the_busy_window() {
        let ttls = glyph_ttls_from(
            Some(10_000),
            Some(120_000),
            Duration::from_millis(300),
            Duration::from_secs(5),
        );
        let path = Path::new("/worktrees/active-recently");
        let age = Duration::from_secs(16);
        let touched = touched_since_scan(age, Some(age + ttls.tick), None, ttls.tick);
        let busy_window = glyph_window(false, touched, path, &ttls);
        let quiet_window = glyph_window(false, false, path, &ttls);
        assert!(age >= busy_window);
        assert!(age < quiet_window);
        assert!(!touched_since_scan(
            age,
            Some(age + ttls.tick + Duration::from_millis(1)),
            None,
            ttls.tick
        ));
    }

    #[test]
    fn a_ref_move_after_the_scan_touches_rows_but_first_sight_and_path_switch_do_not() {
        let now = Instant::now();
        let first_path = Path::new("/repo/one");
        let other_path = Path::new("/repo/two");
        let first = next_ref_obs(None, first_path, Some(1), now).unwrap();
        assert_eq!(first.changed_at, None);
        assert!(!touched_since_scan(
            Duration::from_secs(10),
            None,
            first
                .changed_at
                .map(|changed_at| now.saturating_duration_since(changed_at)),
            Duration::from_secs(5),
        ));
        let moved = next_ref_obs(Some(first.clone()), first_path, Some(2), now).unwrap();
        assert_eq!(moved.changed_at, Some(now));
        assert!(touched_since_scan(
            Duration::from_secs(10),
            None,
            moved
                .changed_at
                .map(|changed_at| now.saturating_duration_since(changed_at)),
            Duration::from_secs(5),
        ));
        let unchanged = next_ref_obs(
            Some(moved.clone()),
            first_path,
            Some(2),
            now + Duration::from_secs(1),
        )
        .unwrap();
        assert_eq!(unchanged, moved);
        let switched = next_ref_obs(Some(moved), other_path, Some(2), now).unwrap();
        assert_eq!(switched.changed_at, None);
        assert!(!touched_since_scan(
            Duration::from_secs(10),
            None,
            switched
                .changed_at
                .map(|changed_at| now.saturating_duration_since(changed_at)),
            Duration::from_secs(5),
        ));
        assert!(next_ref_obs(Some(switched), other_path, None, now).is_none());
    }

    #[test]
    fn warm_rows_come_due_within_half_a_busy_window() {
        let now = Instant::now() + Duration::from_secs(300);
        let ttls = glyph_ttls_from(
            None,
            None,
            Duration::from_millis(300),
            Duration::from_secs(5),
        );
        let path = Path::new("/worktrees/warm");
        let stamp = warm_stamp(now, path, &ttls);
        let age = now.duration_since(stamp);
        let due_after = jittered(ttls.idle, path).saturating_sub(age);
        assert!(due_after <= ttls.busy / 2);
        assert_eq!(
            age,
            jittered(ttls.idle, path).saturating_sub(Duration::from_millis(
                ttls.busy.as_millis() as u64 * u64::from(path_jitter_permille(path)) / 1000,
            )),
        );
        assert_eq!(warm_stamp_from_age(now, Duration::MAX), now);

        let offsets: std::collections::HashSet<Duration> = (0..35)
            .map(|index| {
                let path = PathBuf::from(format!("/worktrees/warm-{index}"));
                let stamp = warm_stamp(now, &path, &ttls);
                jittered(ttls.idle, &path).saturating_sub(now.duration_since(stamp))
            })
            .collect();
        assert!(offsets.len() >= 6);
        assert!(offsets.iter().all(|offset| *offset <= ttls.busy / 2));
    }
}
