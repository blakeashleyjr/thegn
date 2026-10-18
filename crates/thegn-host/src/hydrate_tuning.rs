//! Env-overridable cadence knobs for background hydration, extracted from
//! `hydrate.rs` (pinned at its god-file ratchet ceiling). The ticker interval,
//! background-glyph policy, active-row floor, and terminal resync cadence are
//! pure env readers/builders and are unit-tested without mutating the process env.

use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct GitFileStamp {
    modified: Option<std::time::SystemTime>,
    len: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct GlyphFingerprint {
    index: Option<GitFileStamp>,
    head: Option<GitFileStamp>,
}

/// Read only the cached gitdir's index and HEAD metadata. The `.git` marker is
/// resolved once per worktree and again only if one of those stats is missing.
pub(crate) fn glyph_fingerprint(path: &str, worktree: &Path) -> Option<GlyphFingerprint> {
    let mut fingerprints = glyph_fingerprints()
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    let previous = fingerprints.get(path).cloned();
    let mut gitdir = previous
        .map(|(gitdir, _)| gitdir)
        .or_else(|| resolve_gitdir(worktree))?;
    let (mut current, index_missing, head_missing) = fingerprint_at(&gitdir);
    if index_missing || head_missing {
        if let Some(resolved) = resolve_gitdir(worktree) {
            if resolved != gitdir {
                gitdir = resolved;
                current = fingerprint_at(&gitdir).0;
            }
        }
    }
    let baseline = previous
        .map(|(_, fingerprint)| fingerprint)
        .unwrap_or(current);
    fingerprints.insert(path.to_string(), (gitdir, baseline));
    Some(current)
}

fn resolve_gitdir(worktree: &Path) -> Option<PathBuf> {
    let marker = worktree.join(".git");
    if let Ok(contents) = std::fs::read_to_string(&marker) {
        let target = contents.trim().strip_prefix("gitdir:")?.trim();
        let target = Path::new(target);
        Some(if target.is_absolute() {
            target.to_path_buf()
        } else {
            worktree.join(target)
        })
    } else {
        Some(marker)
    }
}

fn fingerprint_at(gitdir: &Path) -> (GlyphFingerprint, bool, bool) {
    let (index, index_missing) = file_stamp(&gitdir.join("index"));
    let (head, head_missing) = file_stamp(&gitdir.join("HEAD"));
    (
        GlyphFingerprint { index, head },
        index_missing,
        head_missing,
    )
}

fn file_stamp(path: &Path) -> (Option<GitFileStamp>, bool) {
    match std::fs::metadata(path) {
        Ok(metadata) => (
            Some(GitFileStamp {
                modified: metadata.modified().ok(),
                len: metadata.len(),
            }),
            false,
        ),
        Err(error) => (None, error.kind() == std::io::ErrorKind::NotFound),
    }
}

/// Compare and remember a gitdir fingerprint when its row is selected for a
/// scan. Rows without a resolvable gitdir rely on the safety interval.
pub(crate) fn glyph_fingerprint_changed(path: &str, current: Option<GlyphFingerprint>) -> bool {
    let previous = glyph_fingerprints()
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .get(path)
        .map(|(_, fingerprint)| *fingerprint);
    matches!((previous, current), (Some(previous), Some(current)) if previous != current)
}

pub(crate) fn record_glyph_fingerprint(path: &str, current: Option<GlyphFingerprint>) {
    if let Some(current) = current {
        let mut fingerprints = glyph_fingerprints()
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if let Some((_, fingerprint)) = fingerprints.get_mut(path) {
            *fingerprint = current;
        }
    }
}

pub(crate) fn retain_glyph_fingerprints(keep: &std::collections::HashSet<String>) {
    glyph_fingerprints()
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .retain(|path, _| keep.contains(path));
}

fn glyph_fingerprints()
-> &'static Mutex<std::collections::HashMap<String, (PathBuf, GlyphFingerprint)>> {
    static SEEN: OnceLock<Mutex<std::collections::HashMap<String, (PathBuf, GlyphFingerprint)>>> =
        OnceLock::new();
    SEEN.get_or_init(|| Mutex::new(std::collections::HashMap::new()))
}

/// Default for [`model_refresh_interval`]. It must stay a multiple of the
/// 500ms base that divides `PR_REFRESH_INTERVAL` so the ticker keeps emitting
/// `RefreshKind::Pr` (see the cadence-invariant test).
pub(crate) const DEFAULT_MODEL_REFRESH_MS: u64 = 5000;
const DEFAULT_BG_BUSY_GLYPH_TTL_MS: u64 = 10_000;
const DEFAULT_BG_SAFETY_GLYPH_TTL_MS: u64 = 600_000;

/// Background glyph policy windows and the cadence used to interpret hints.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct GlyphTtls {
    pub(crate) busy: Duration,
    pub(crate) safety: Duration,
    pub(crate) floor: Duration,
    pub(crate) tick: Duration,
}

/// Build policy windows from explicit values. A zero busy TTL deliberately
/// preserves the always-rescan opt-out; otherwise the busy window stays above
/// one model tick; quiet rows use the longer safety interval unless their
/// gitdir fingerprint changes.
pub(crate) fn glyph_ttls_from(
    busy_ms: Option<u64>,
    safety_ms: Option<u64>,
    floor: Duration,
    tick: Duration,
) -> GlyphTtls {
    let busy_ms = busy_ms.unwrap_or(DEFAULT_BG_BUSY_GLYPH_TTL_MS);
    let safety_ms = safety_ms.unwrap_or(DEFAULT_BG_SAFETY_GLYPH_TTL_MS);
    let busy = if busy_ms == 0 {
        Duration::ZERO
    } else {
        Duration::from_millis(busy_ms).max(tick.saturating_add(Duration::from_millis(500)))
    };
    let safety = Duration::from_millis(safety_ms);
    let safety = if busy.is_zero() {
        safety
    } else {
        safety.max(busy)
    };
    GlyphTtls {
        busy,
        safety,
        floor,
        tick,
    }
}

/// Current policy. `THEGN_BG_MODEL_REFRESH_MS` defaults to 10s (`0` always
/// rescans background rows); `THEGN_BG_SAFETY_GLYPH_TTL_MS` defaults to 10m.
/// Both are env-only knobs.
pub(crate) fn glyph_ttls() -> GlyphTtls {
    let busy_ms = std::env::var("THEGN_BG_MODEL_REFRESH_MS")
        .ok()
        .and_then(|value| value.parse::<u64>().ok());
    let safety_ms = std::env::var("THEGN_BG_SAFETY_GLYPH_TTL_MS")
        .ok()
        .and_then(|value| value.parse::<u64>().ok());
    let safety_ms = safety_ms.or_else(|| {
        std::env::var("THEGN_BG_IDLE_GLYPH_TTL_MS")
            .ok()
            .and_then(|value| value.parse::<u64>().ok())
    });
    glyph_ttls_from(
        busy_ms,
        safety_ms,
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

/// Convert an activity timestamp into its age, treating future timestamps as
/// activity observed just now. Non-finite values cannot identify an age.
pub(crate) fn since_active_age(now_unix: f64, last_active_at: f64) -> Option<Duration> {
    let age_seconds = now_unix - last_active_at;
    if age_seconds.is_finite() {
        Duration::try_from_secs_f64(age_seconds.max(0.0)).ok()
    } else {
        None
    }
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
        jittered(ttls.safety, path)
    }
}

#[cfg(test)]
pub(crate) fn glyph_should_rescan(
    cached_age: Option<Duration>,
    is_active: bool,
    touched: bool,
    fingerprint_changed: bool,
    path: &Path,
    ttls: &GlyphTtls,
) -> bool {
    let window = fingerprint_adjusted_window(
        fingerprint_changed,
        glyph_window(is_active, touched, path, ttls),
    );
    match cached_age {
        None => true,
        Some(age) => age >= window,
    }
}

pub(crate) fn fingerprint_adjusted_window(
    fingerprint_changed: bool,
    normal_window: Duration,
) -> Duration {
    if fingerprint_changed {
        Duration::ZERO
    } else {
        normal_window
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

/// Stamp a warm-cache row so its safety window expires in a staggered first wave.
pub(crate) fn warm_stamp(now: Instant, path: &Path, ttls: &GlyphTtls) -> Instant {
    let stagger = ttls
        .busy
        .as_millis()
        .saturating_mul(u128::from(path_jitter_permille(path)))
        / 1000;
    warm_stamp_from_age(
        now,
        jittered(ttls.safety, path).saturating_sub(Duration::from_millis(
            stagger.min(u128::from(u64::MAX)) as u64,
        )),
    )
}

fn warm_stamp_from_age(now: Instant, age: Duration) -> Instant {
    now.checked_sub(age).unwrap_or(now)
}

/// Safety-net cadence for the background model re-hydration ticker. This tick
/// advances the activity-dot FSM and checks background glyphs against their
/// busy window or quiet safety interval. The active worktree's panel + git glyphs
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
        assert_eq!(clamped.safety, clamped.busy);

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
    fn future_activity_timestamp_saturates_to_zero_and_touches_row() {
        assert_eq!(since_active_age(1000.0, 1000.7), Some(Duration::ZERO));
        assert!(touched_since_scan(
            Duration::ZERO,
            since_active_age(1000.0, 1000.7),
            None,
            Duration::from_secs(5),
        ));
    }

    #[test]
    fn idle_rows_wait_for_safety_unless_the_gitdir_fingerprint_changes() {
        let ttls = glyph_ttls_from(
            Some(10_000),
            Some(600_000),
            Duration::from_millis(300),
            Duration::from_secs(5),
        );
        let path = Path::new("/worktrees/quiet");
        assert!(!glyph_should_rescan(
            Some(Duration::from_secs(121)),
            false,
            false,
            false,
            path,
            &ttls
        ));
        assert!(!glyph_should_rescan(
            Some(ttls.safety - Duration::from_secs(1)),
            false,
            false,
            false,
            path,
            &ttls
        ));
        assert!(glyph_should_rescan(
            Some(jittered(ttls.safety, path) + Duration::from_millis(1)),
            false,
            false,
            false,
            path,
            &ttls,
        ));
        assert!(glyph_should_rescan(
            Some(Duration::from_secs(121)),
            false,
            false,
            true,
            path,
            &ttls
        ));
        let touched_window = glyph_window(false, true, path, &ttls);
        assert!(glyph_should_rescan(
            Some(touched_window + Duration::from_millis(1)),
            false,
            true,
            false,
            path,
            &ttls,
        ));
        let active_window = glyph_window(true, false, path, &ttls);
        assert!(glyph_should_rescan(
            Some(active_window + Duration::from_millis(1)),
            true,
            false,
            false,
            path,
            &ttls,
        ));
        let always = glyph_ttls_from(
            Some(0),
            Some(600_000),
            Duration::ZERO,
            Duration::from_secs(5),
        );
        assert!(glyph_should_rescan(
            Some(Duration::from_millis(1)),
            false,
            false,
            false,
            path,
            &always
        ));
    }

    #[test]
    fn gitdir_fingerprint_tracks_index_metadata_without_git() {
        let dir = tempfile::tempdir().unwrap();
        thegn_core::fsperm::restrict_dir_to_owner(dir.path()).unwrap();
        let gitdir = dir.path().join(".git");
        std::fs::create_dir(&gitdir).unwrap();
        std::fs::write(gitdir.join("HEAD"), "ref: refs/heads/main\n").unwrap();
        std::fs::write(gitdir.join("index"), "one").unwrap();
        let key = dir.path().to_string_lossy().into_owned();
        let before = glyph_fingerprint(&key, dir.path()).unwrap();
        record_glyph_fingerprint(&key, Some(before));
        assert!(!glyph_fingerprint_changed(&key, Some(before)));
        std::thread::sleep(Duration::from_millis(2));
        std::fs::write(gitdir.join("index"), "two longer").unwrap();
        let after = glyph_fingerprint(&key, dir.path()).unwrap();
        assert_ne!(before, after);
        assert!(glyph_fingerprint_changed(&key, Some(after)));

        let linked = dir.path().join("linked");
        let linked_gitdir = dir.path().join("linked-gitdir");
        std::fs::create_dir(&linked).unwrap();
        std::fs::create_dir(&linked_gitdir).unwrap();
        std::fs::write(
            linked.join(".git"),
            format!("gitdir: {}\n", linked_gitdir.display()),
        )
        .unwrap();
        std::fs::write(linked_gitdir.join("HEAD"), "ref: refs/heads/linked\n").unwrap();
        std::fs::write(linked_gitdir.join("index"), "linked index").unwrap();
        let linked_key = linked.to_string_lossy().into_owned();
        assert!(
            glyph_fingerprint(&linked_key, &linked)
                .unwrap()
                .index
                .is_some()
        );

        let relative = dir.path().join("relative");
        let relative_gitdir = dir.path().join("relative-gitdir");
        std::fs::create_dir(&relative).unwrap();
        std::fs::create_dir(&relative_gitdir).unwrap();
        std::fs::write(relative.join(".git"), "gitdir: ../relative-gitdir\n").unwrap();
        std::fs::write(relative_gitdir.join("HEAD"), "ref: refs/heads/relative\n").unwrap();
        std::fs::write(relative_gitdir.join("index"), "relative index").unwrap();
        let relative_key = relative.to_string_lossy().into_owned();
        assert!(
            glyph_fingerprint(&relative_key, &relative)
                .unwrap()
                .index
                .is_some()
        );
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
    fn warm_rows_come_due_staggered_within_half_a_busy_window() {
        let now = Instant::now() + Duration::from_secs(300);
        let ttls = glyph_ttls_from(
            None,
            None,
            Duration::from_millis(300),
            Duration::from_secs(5),
        );
        assert_eq!(ttls.safety, Duration::from_secs(600));
        let path = Path::new("/worktrees/warm");
        let stamp = warm_stamp(now, path, &ttls);
        let age = now.duration_since(stamp);
        let due_after = jittered(ttls.safety, path).saturating_sub(age);
        assert!(due_after <= ttls.busy / 2);
        assert_eq!(
            age,
            jittered(ttls.safety, path).saturating_sub(Duration::from_millis(
                ttls.busy.as_millis() as u64 * u64::from(path_jitter_permille(path)) / 1000,
            )),
        );
        assert_eq!(warm_stamp_from_age(now, Duration::MAX), now);

        let offsets: std::collections::HashSet<Duration> = (0..35)
            .map(|index| {
                let path = PathBuf::from(format!("/worktrees/warm-{index}"));
                let stamp = warm_stamp(now, &path, &ttls);
                jittered(ttls.safety, &path).saturating_sub(now.duration_since(stamp))
            })
            .collect();
        assert!(offsets.len() >= 6);
        assert!(offsets.iter().all(|offset| *offset <= ttls.busy / 2));
    }
}
