//! CI run-history refresh machinery (AV group): the ttl-guarded refresh of the
//! active worktree, a round-robin background-worktree sweep that keeps the
//! cross-worktree Across excerpts honest, exponential backoff after fetch
//! failures, and the per-worktree fetch-health note the panel surfaces
//! ("CI provider API rate limited", …). The ticker cadence and the
//! `RefreshKind` plumbing stay in [`crate::hydrate`]; everything here runs off
//! the event loop.

use std::collections::HashMap;
use std::sync::Mutex;

use termwiz::terminal::TerminalWaker;

/// The CI ticker cadence in 500ms slots, from `[ci] poll_interval_secs`.
/// Clamped to ≥ 5s — every tick is a provider subprocess (`gh run list`), so a
/// faster cadence would thrash. Pure, so it's unit-tested.
pub(crate) fn ci_every_slots(poll_interval_secs: u64) -> u64 {
    (poll_interval_secs.max(5) * 1000) / 500
}

/// Freshness guard for the CI run-history cache: a non-forced refresh
/// (ticker / on-switch backstop / sweep) is skipped while the cached row is
/// younger than `[ci] ttl_secs`. Forced refreshes (the `g` key, post-mutation)
/// bypass it. `ttl_secs == 0` disables the guard. Pure, so it's unit-tested.
pub(crate) fn ci_cache_is_fresh(fetched_at: Option<i64>, now: i64, ttl_secs: u64) -> bool {
    match fetched_at {
        Some(t) => ttl_secs > 0 && now.saturating_sub(t) < ttl_secs as i64,
        None => false,
    }
}

// --- fetch health: the panel note + failure backoff ------------------------

/// Per-worktree CI fetch health. Process-global (same pattern as hydrate's
/// glyph cache) so it needs no threading through the hydration call sites; the
/// DB stays a cache of provider *data*, not of transient fetch health.
#[derive(Clone, Default)]
struct CiHealth {
    /// Human-readable fetch problem, surfaced as the panel's `ci_note`.
    note: Option<String>,
    /// Consecutive failed fetches (reset on success).
    failures: u32,
    /// Epoch seconds before which non-forced refetches are skipped.
    backoff_until: i64,
}

fn health() -> &'static Mutex<HashMap<String, CiHealth>> {
    static HEALTH: std::sync::OnceLock<Mutex<HashMap<String, CiHealth>>> =
        std::sync::OnceLock::new();
    HEALTH.get_or_init(|| Mutex::new(HashMap::new()))
}

/// The panel-visible fetch note for a worktree ("CI provider API rate
/// limited", "CI provider CLI not installed", …). `None` while healthy.
pub(crate) fn note_for(worktree: &str) -> Option<String> {
    health().lock().ok()?.get(worktree)?.note.clone()
}

/// Exponential refetch backoff after `failures` consecutive errors: the poll
/// interval doubled per extra failure, capped at 5 minutes. Zero failures =
/// no backoff. Pure, so it's unit-tested.
pub(crate) fn backoff_secs(failures: u32, poll_secs: u64) -> u64 {
    if failures == 0 {
        return 0;
    }
    let base = poll_secs.clamp(5, 300);
    base.saturating_mul(1u64 << (failures - 1).min(6)).min(300)
}

/// Record a failed fetch: bump the failure count, arm the backoff window, and
/// set the panel note (with a retry hint once the provider is backing off).
fn record_failure(worktree: &str, message: &str, now: i64, poll_secs: u64) {
    if let Ok(mut map) = health().lock() {
        let h = map.entry(worktree.to_string()).or_default();
        h.failures += 1;
        let backoff = backoff_secs(h.failures, poll_secs);
        h.backoff_until = now + backoff as i64;
        h.note = Some(if h.failures > 1 {
            format!("{message} — retrying in {backoff}s")
        } else {
            message.to_string()
        });
    }
}

/// Record a successful fetch: clear the note and the backoff.
fn record_success(worktree: &str) {
    if let Ok(mut map) = health().lock() {
        map.remove(worktree);
    }
}

fn backoff_active(worktree: &str, now: i64) -> bool {
    health()
        .lock()
        .ok()
        .and_then(|m| m.get(worktree).map(|h| now < h.backoff_until))
        .unwrap_or(false)
}

// --- the refresh itself -----------------------------------------------------

/// Everything the loop does on a CI tick, in one call (keeps `run.rs` lean):
/// kick the off-loop cache refresh (+ the background sweep it carries) and
/// re-poll an open live drill on a still-running run so it updates in place.
pub(crate) fn on_ci_tick(
    session: &crate::session::Session,
    cfg: &superzej_core::config::CiConfig,
    refresh_tx: &tokio::sync::mpsc::UnboundedSender<crate::hydrate::RefreshKind>,
    waker: &TerminalWaker,
    force: bool,
    bar_detail: &mut Option<crate::detail::DetailOverlay>,
) {
    spawn_ci_cache_refresh(session.clone(), cfg.clone(), Some(waker.clone()), force);
    if let Some(run) = bar_detail.as_mut().and_then(|ov| ov.live_ci_repoll()) {
        crate::actions::spawn_ci_detail(session, cfg, refresh_tx, waker, run);
    }
}

/// Refresh the CI run-history cache for the active worktree (AV group), then
/// sweep one stale background worktree (round-robin) so the Across section
/// converges instead of freezing at last-visit state. Off the event loop:
/// resolves the provider from `[ci]` config + the git remote, fetches recent
/// runs for the current branch via the async `CiProvider`, writes
/// `ci_runs_cache`, and pulses the waker so the panel rehydrates. Non-`force`
/// calls are coalesced by the `[ci] ttl_secs` freshness guard and by the
/// failure backoff; fetch errors surface via [`note_for`].
pub(crate) fn spawn_ci_cache_refresh(
    session: crate::session::Session,
    cfg: superzej_core::config::CiConfig,
    waker: Option<TerminalWaker>,
    force: bool,
) {
    crate::sched::spawn_bg(move || {
        let cwd = crate::hydrate::active_tab_path(&session);
        if cwd.is_dir() {
            let loc = superzej_core::remote::GitLoc::for_worktree(&cwd);
            refresh_ci_cache_for(&loc, &cfg, waker.as_ref(), force);
        }
        // The sweep rides the same bg task so provider subprocesses stay
        // serialized (kind to rate limits) and the loop-side call stays one
        // spawn. Sweeping is never forced — the ttl guard is its rate limiter.
        sweep_one_background(&session, &cwd, &cfg, waker.as_ref());
    });
}

/// Round-robin cursor over background worktrees, advanced once per sweep so
/// every worktree gets a turn even when several are stale.
static SWEEP_CURSOR: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

/// Refresh at most ONE stale background worktree's CI cache (the ttl guard
/// decides staleness), starting from the rotating cursor. Blocking; must run
/// on the bg lane.
fn sweep_one_background(
    session: &crate::session::Session,
    active: &std::path::Path,
    cfg: &superzej_core::config::CiConfig,
    waker: Option<&TerminalWaker>,
) {
    let others: Vec<&str> = session
        .worktrees
        .iter()
        .map(|g| g.path.as_str())
        .filter(|p| std::path::Path::new(p) != active)
        .collect();
    if others.is_empty() {
        return;
    }
    let start = SWEEP_CURSOR.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    for i in 0..others.len() {
        let p = std::path::Path::new(others[(start + i) % others.len()]);
        if !p.is_dir() {
            continue;
        }
        let loc = superzej_core::remote::GitLoc::for_worktree(p);
        // First worktree that actually fetched (fresh ones are skipped by the
        // guard) ends the sweep — one provider subprocess per tick, max.
        if refresh_ci_cache_for(&loc, cfg, waker, false) {
            return;
        }
    }
}

/// The blocking body of a CI cache refresh for one worktree location. Returns
/// whether a fetch was actually attempted (`false` when skipped by the
/// freshness/backoff guards or when no provider resolves).
fn refresh_ci_cache_for(
    loc: &superzej_core::remote::GitLoc,
    cfg: &superzej_core::config::CiConfig,
    waker: Option<&TerminalWaker>,
    force: bool,
) -> bool {
    use superzej_core::store::CacheStore;
    let Ok(db) = superzej_core::db::Db::open() else {
        return false;
    };
    let key = loc.path();
    let now = superzej_core::util::now();
    if !force {
        let fetched_at = db.get_ci_cache(&key).ok().flatten().map(|(_, at)| at);
        if ci_cache_is_fresh(fetched_at, now, cfg.ttl_secs) || backoff_active(&key, now) {
            return false;
        }
    }
    let Some(client) = superzej_svc::ci::provider_for(loc, cfg) else {
        return false;
    };
    let Ok(rt) = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    else {
        return false;
    };
    let branch = loc
        .git_out(&["rev-parse", "--abbrev-ref", "HEAD"])
        .filter(|b| !b.is_empty());
    match rt.block_on(client.runs(loc, branch.as_deref(), cfg.max_runs)) {
        Ok(runs) => {
            record_success(&key);
            if let Ok(json) = serde_json::to_string(&runs) {
                let _ = db.put_ci_cache(&key, branch.as_deref().unwrap_or(""), &json);
            }
        }
        Err(e) => {
            // The stale cache stays (better than blank), but the panel gets an
            // honest note instead of silently rendering old data as current.
            record_failure(&key, &e.message(), now, cfg.poll_interval_secs);
        }
    }
    if let Some(w) = waker {
        let _ = w.wake();
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ci_cadence_honors_config_and_clamps() {
        // `[ci] poll_interval_secs` maps to 500ms ticker slots…
        assert_eq!(ci_every_slots(30), 60);
        assert_eq!(ci_every_slots(20), 40);
        // …and is clamped to ≥ 5s (a provider subprocess per tick).
        assert_eq!(ci_every_slots(0), 10);
        assert_eq!(ci_every_slots(1), 10);
    }

    #[test]
    fn ci_cache_freshness_guard() {
        // Fresh row within the ttl → skip the refetch.
        assert!(ci_cache_is_fresh(Some(100), 120, 30));
        // Aged past the ttl → refetch.
        assert!(!ci_cache_is_fresh(Some(100), 130, 30));
        assert!(!ci_cache_is_fresh(Some(100), 131, 30));
        // No cached row yet → always fetch.
        assert!(!ci_cache_is_fresh(None, 120, 30));
        // ttl 0 disables the guard entirely.
        assert!(!ci_cache_is_fresh(Some(120), 120, 0));
        // A clock that went backwards must not wedge the guard forever
        // (saturating_sub keeps the age ≥ 0, so it reads as "just fetched").
        assert!(ci_cache_is_fresh(Some(200), 120, 30));
    }

    #[test]
    fn failure_backoff_doubles_and_caps() {
        assert_eq!(backoff_secs(0, 30), 0);
        assert_eq!(backoff_secs(1, 30), 30);
        assert_eq!(backoff_secs(2, 30), 60);
        assert_eq!(backoff_secs(3, 30), 120);
        assert_eq!(backoff_secs(4, 30), 240);
        // Capped at 5 minutes, however long the streak…
        assert_eq!(backoff_secs(5, 30), 300);
        assert_eq!(backoff_secs(100, 30), 300);
        // …and a degenerate poll interval is clamped into the sane band.
        assert_eq!(backoff_secs(1, 0), 5);
        assert_eq!(backoff_secs(1, 100_000), 300);
    }

    #[test]
    fn health_notes_lifecycle() {
        let wt = format!("/tmp/ci-health-test-{}", std::process::id());
        assert_eq!(note_for(&wt), None);
        record_failure(&wt, "CI provider API rate limited", 1000, 30);
        assert_eq!(
            note_for(&wt).as_deref(),
            Some("CI provider API rate limited")
        );
        assert!(backoff_active(&wt, 1010));
        assert!(!backoff_active(&wt, 2000));
        // A second consecutive failure notes the retry horizon.
        record_failure(&wt, "CI provider API rate limited", 2000, 30);
        assert_eq!(
            note_for(&wt).as_deref(),
            Some("CI provider API rate limited — retrying in 60s")
        );
        // Success clears note + backoff.
        record_success(&wt);
        assert_eq!(note_for(&wt), None);
        assert!(!backoff_active(&wt, 2001));
    }
}
