//! The background tokei count behind the bottom-bar `LOC` chip, its detail
//! table, and the Files-section footer.
//!
//! There was no background LOC scan at all. The walk lived inside
//! `hydrate::worktree_loc`, gated on the panel's Files section being open — so a
//! worktree you had never opened Files on showed no count, ever, restart
//! included — and when it did run it ran inline on the interactive hydration
//! lane, stalling model/PR/CI refresh for the length of a full-tree walk. This
//! module is the disk scan's twin: same planner, same lane, same guards.

use std::sync::atomic::AtomicBool;

use termwiz::terminal::TerminalWaker;
use thegn_core::db::Db;
use thegn_core::scan_sched;
use thegn_core::store::CacheStore;

use super::LOG;

static INFLIGHT: AtomicBool = AtomicBool::new(false);

/// Background per-worktree LOC count. Same priority rules as the size scan: the
/// active worktree and any never-counted one first, so a freshly created
/// worktree gets a count immediately rather than after a full sweep.
///
/// `watch` marks a content-driven round (the diff filesystem watcher saw the
/// active worktree change): that one path may bypass the long TTL, bounded by
/// `[loc] watch_invalidate_secs`.
#[allow(dead_code)] // retained as the untagged/event-driven entry point
pub(crate) fn spawn_scan(
    cfg: thegn_core::config::LocConfig,
    active: Option<std::path::PathBuf>,
    watch: bool,
    waker: Option<TerminalWaker>,
) {
    spawn_scan_with_generation(cfg, active, watch, waker, None);
}

pub(crate) fn spawn_scan_with_generation(
    cfg: thegn_core::config::LocConfig,
    active: Option<std::path::PathBuf>,
    watch: bool,
    waker: Option<TerminalWaker>,
    generation: Option<(std::sync::Arc<std::sync::atomic::AtomicU64>, u64)>,
) {
    tokio::task::spawn_blocking(move || {
        let Some(_round) = super::begin(&INFLIGHT, "loc") else {
            return;
        };
        let Some(_permit) = super::permit("loc") else {
            return;
        };
        let Ok(db) = Db::open() else {
            return;
        };

        // Sweep before the `enabled` gate, for the same reason the size scan
        // does: reclaiming rows for worktrees that no longer exist is not part
        // of the feature you switched off. This is also what self-heals the
        // drift the cache accumulated while it had no GC at all.
        let reaped = sweep_orphans(&db);

        if !cfg.enabled {
            return;
        }

        let stamps = db.all_loc_cache_stamps().unwrap_or_default();
        let active = super::active_key(active.as_deref());
        let now = thegn_core::util::now();
        let mut targets = super::targets(&db, &stamps, active.as_deref());
        if watch {
            expire_watched(&mut targets, now, cfg.watch_invalidate_secs);
        }
        let due = scan_sched::plan(
            &targets,
            now,
            cfg.scan_interval_secs,
            cfg.max_scan_per_round as usize,
        );
        tracing::debug!(
            target: LOG,
            scan = "loc",
            known = targets.len(),
            due = due.len(),
            reaped,
            "planned round"
        );

        let mut counted = 0u32;
        for path_s in &due {
            if generation.as_ref().is_some_and(|(current, expected)| {
                current.load(std::sync::atomic::Ordering::Acquire) != *expected
            }) {
                return;
            }
            let path = std::path::Path::new(path_s);
            // loc_scan owns the repository boundary rule: a gitlink's checked
            // out source is not part of the superproject's LOC total.
            if apply_scan_result(
                &db,
                path_s,
                crate::loc_scan::scan(path),
                generation.as_ref(),
            ) {
                counted += 1;
            }
        }

        if counted > 0
            && let Some(w) = &waker
        {
            let _ = w.wake(); // best-effort: waker pulse: an input nudge must never fail the calling path
        }
    });
}

/// Apply one scan without allowing an incomplete report to advance the cache's
/// freshness timestamp. Returns whether the visible cache changed and should
/// wake hydration.
fn apply_scan_result(
    db: &Db,
    path: &str,
    outcome: crate::loc_scan::ScanOutcome,
    generation: Option<&crate::hydrate_schedule::ScheduleFence>,
) -> bool {
    match outcome {
        crate::loc_scan::ScanOutcome::Unavailable
        | crate::loc_scan::ScanOutcome::Complete(None) => {
            // An absent root or a completed empty walk means the old count no
            // longer describes this target. Keep the prior deletion behavior.
            if !crate::hydrate_schedule::generation_is_current(generation) {
                return false;
            }
            let _ = db.delete_loc_cache(path); // best-effort: cache write: the DB is a cache; git/forge stays the source of truth
            true
        }
        crate::loc_scan::ScanOutcome::Complete(Some(report)) => {
            if let Ok(json) = serde_json::to_string(&report) {
                if !crate::hydrate_schedule::generation_is_current(generation) {
                    return false;
                }
                let _ = db.put_loc_cache(path, report.total_code, &json); // best-effort: cache write: the DB is a cache; git/forge stays the source of truth
                true
            } else {
                false
            }
        }
        crate::loc_scan::ScanOutcome::Incomplete(report) => {
            // Tokei found usable rows but at least one language read failed.
            // Do not turn a partial result into a fresh cache stamp: retain the
            // last complete row and let the normal bounded scheduler revisit
            // this target later. There is no retry loop or extra wakeup here.
            tracing::debug!(
                target: LOG,
                path,
                partial_code = report.total_code,
                "LOC scan incomplete; retaining previous cache"
            );
            false
        }
    }
}

/// Content-driven invalidation for the ACTIVE target: if its last count is
/// older than `window_secs`, clear the stamp so the planner sees it as never
/// counted — which makes it `ActiveCold`, the very first thing measured this
/// round. A count inside the window is left alone, so a save storm re-walks the
/// tree at most once per window rather than continuously.
///
/// `window_secs == 0` disables content-driven recounts (TTL alone governs).
/// Pure, so the debounce is testable without a filesystem or a DB.
fn expire_watched(targets: &mut [thegn_core::scan_sched::ScanTarget], now: i64, window_secs: u64) {
    if window_secs == 0 {
        return;
    }
    for t in targets.iter_mut().filter(|t| t.active) {
        if t.measured_at
            .is_some_and(|at| !thegn_core::time_policy::is_fresh(now, Some(at), window_secs))
        {
            t.measured_at = None;
        }
    }
}

/// Delete LOC rows whose worktree has left the registry. Returns how many.
fn sweep_orphans(db: &Db) -> usize {
    let Ok(cached) = db.all_loc_cache_stamps() else {
        return 0;
    };
    let live = super::live_paths(db);
    let gone = scan_sched::orphans(
        cached.keys().map(String::as_str),
        live.iter().map(String::as_str),
    );
    for path in &gone {
        let _ = db.delete_loc_cache(path); // best-effort: cache write: the DB is a cache; git/forge stays the source of truth
    }
    gone.len()
}

#[cfg(test)]
mod tests {
    use super::*;
    use thegn_core::scan_sched::ScanTarget;
    use thegn_core::store::CacheStore;

    const NOW: i64 = 1_000_000;

    fn targets() -> Vec<ScanTarget> {
        vec![
            ScanTarget::measured("/wt/active", NOW - 300).active(),
            ScanTarget::measured("/wt/other", NOW - 300),
        ]
    }

    /// An edit to the worktree you are looking at must be able to jump the long
    /// `scan_interval_secs` — that is the whole point of watching the tree.
    #[test]
    fn a_watched_edit_expires_only_the_active_target() {
        let mut t = targets();
        expire_watched(&mut t, NOW, 60);
        assert!(t[0].measured_at.is_none(), "active target is recounted");
        assert!(t[1].measured_at.is_some(), "others keep their TTL");

        // …and being cold + active puts it first in the round.
        let due = scan_sched::plan(&t, NOW, 900, 0);
        assert_eq!(due.first().map(String::as_str), Some("/wt/active"));
    }

    /// A save storm must not re-walk the tree on every keystroke.
    #[test]
    fn a_recent_count_is_not_re_expired_within_the_window() {
        let mut t = vec![ScanTarget::measured("/wt/active", NOW - 5).active()];
        expire_watched(&mut t, NOW, 60);
        assert!(t[0].measured_at.is_some(), "inside the debounce window");
        assert!(scan_sched::plan(&t, NOW, 900, 0).is_empty());
    }

    #[test]
    fn a_zero_window_disables_content_driven_recounts() {
        let mut t = targets();
        expire_watched(&mut t, NOW, 0);
        assert!(t.iter().all(|t| t.measured_at.is_some()));
    }

    /// A never-counted target is already cold; expiry must not disturb it.
    #[test]
    fn expiry_is_a_noop_for_an_uncounted_target() {
        let mut t = vec![ScanTarget::cold("/wt/new").active()];
        expire_watched(&mut t, NOW, 60);
        assert!(t[0].measured_at.is_none());
    }

    #[test]
    fn incomplete_scan_retains_last_complete_cache_until_recovery() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("loc.db");
        let db = Db::open_at(&db_path).unwrap();
        let complete = thegn_core::loc::LocReport::total_only(12);
        let complete_json = serde_json::to_string(&complete).unwrap();
        assert!(apply_scan_result(
            &db,
            "/wt",
            crate::loc_scan::ScanOutcome::Complete(Some(complete.clone())),
            None,
        ));
        drop(db);
        // Move the fixture into the past so recovery must advance the stamp;
        // two fast writes can otherwise legitimately share the same second.
        let sql = rusqlite::Connection::open(&db_path).unwrap();
        sql.execute(
            "UPDATE loc_cache SET fetched_at=123 WHERE worktree='/wt'",
            [],
        )
        .unwrap();
        drop(sql);
        let db = Db::open_at(&db_path).unwrap();
        let (before_json, before_at) = db.get_loc_cache_entry("/wt").unwrap().unwrap();
        assert_eq!(before_json, complete_json);
        assert_eq!(before_at, 123);

        let partial = thegn_core::loc::LocReport::total_only(99);
        assert!(!apply_scan_result(
            &db,
            "/wt",
            crate::loc_scan::ScanOutcome::Incomplete(partial),
            None,
        ));
        drop(db);
        let db = Db::open_at(&db_path).unwrap();
        let (retained_json, retained_at) = db.get_loc_cache_entry("/wt").unwrap().unwrap();
        assert_eq!(retained_json, before_json);
        assert_eq!(retained_at, before_at);

        let recovered = thegn_core::loc::LocReport::total_only(21);
        assert!(apply_scan_result(
            &db,
            "/wt",
            crate::loc_scan::ScanOutcome::Complete(Some(recovered.clone())),
            None,
        ));
        let (recovered_json, recovered_at) = db.get_loc_cache_entry("/wt").unwrap().unwrap();
        assert_eq!(
            serde_json::from_str::<thegn_core::loc::LocReport>(&recovered_json).unwrap(),
            recovered
        );
        assert!(recovered_at > retained_at);
    }

    #[test]
    fn unavailable_or_empty_scan_keeps_the_delete_policy() {
        let db = Db::open_memory().unwrap();
        let report = thegn_core::loc::LocReport::total_only(12);
        db.put_loc_cache(
            "/wt",
            report.total_code,
            &serde_json::to_string(&report).unwrap(),
        )
        .unwrap();
        assert!(apply_scan_result(
            &db,
            "/wt",
            crate::loc_scan::ScanOutcome::Complete(None),
            None,
        ));
        assert!(db.get_loc_cache_entry("/wt").unwrap().is_none());

        let report = thegn_core::loc::LocReport::total_only(12);
        db.put_loc_cache(
            "/wt",
            report.total_code,
            &serde_json::to_string(&report).unwrap(),
        )
        .unwrap();
        assert!(apply_scan_result(
            &db,
            "/wt",
            crate::loc_scan::ScanOutcome::Unavailable,
            None,
        ));
        assert!(db.get_loc_cache_entry("/wt").unwrap().is_none());

        let fresh = Db::open_memory().unwrap();
        assert!(!apply_scan_result(
            &fresh,
            "/never-cached",
            crate::loc_scan::ScanOutcome::Incomplete(thegn_core::loc::LocReport::total_only(99),),
            None,
        ));
        assert!(
            fresh
                .get_loc_cache_entry("/never-cached")
                .unwrap()
                .is_none()
        );
    }
}
