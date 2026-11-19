//! The background tokei count behind the bottom-bar `LOC` chip, its detail
//! table, and the Files-section footer.
//!
//! There was no background LOC scan at all. The walk lived inside
//! `hydrate::worktree_loc`, gated on the panel's Files section being open — so a
//! worktree you had never opened Files on showed no count, ever, restart
//! included — and when it did run it ran inline on the interactive hydration
//! lane, stalling model/PR/CI refresh for the length of a full-tree walk. This
//! module is the disk scan's twin: same planner, same lane, same guards.

use std::collections::HashMap;
use std::sync::atomic::AtomicBool;
use std::sync::{Mutex, OnceLock};

use termwiz::terminal::TerminalWaker;
use thegn_core::db::Db;
use thegn_core::scan_sched;
use thegn_core::scan_sched::RoundTrigger;
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
    let now = thegn_core::util::now();
    let fp = loc_fingerprint(&cfg);
    // A pump coalesced with a watch event is a Watch round; the gate stays
    // open, so the next pump still sweeps. Kick versus Pump is told apart by
    // the gate's kick latch, never by a new `RefreshKind` variant.
    let trigger = if watch {
        RoundTrigger::Watch
    } else if super::LOC_GATE.take_kick() {
        RoundTrigger::Kick
    } else {
        RoundTrigger::Pump
    };
    if !super::LOC_GATE.admit(now, fp, trigger) {
        return;
    }
    if trigger == RoundTrigger::Watch
        && let Some(active) = super::active_key(active.as_deref())
    {
        let memo = ACTIVE_COUNTED
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if watch_recently_counted(memo.as_ref(), &active, now, watch_window_secs(&cfg)) {
            return;
        }
    }

    crate::sched::report_replaceable(super::LOC_LANE.submit(Box::new(move || {
        let Some(_round) = super::begin(&INFLIGHT, "loc") else {
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
        // A background row examined and found unchanged was not re-counted,
        // so its DB stamp is old; the process memo says when it was last
        // looked at. The ACTIVE row always keeps its DB stamp, so a
        // switched-to worktree with an old count is recounted at once.
        apply_examined(&mut targets, &examined());
        if watch {
            expire_watched(&mut targets, now, cfg.watch_invalidate_secs);
        }
        let params = loc_params(&cfg);
        let shape = scan_sched::lane_shape(targets.len(), &params);
        if shape.ttl_secs > params.ttl_secs {
            super::log_stretched_once("loc", targets.len(), shape.ttl_secs);
        }
        let plan = scan_sched::plan_round(&targets, now, &shape, trigger);
        tracing::debug!(
            target: LOG,
            scan = "loc",
            ?trigger,
            known = targets.len(),
            due = plan.due.len(),
            examine_cap = shape.examine_cap,
            reaped,
            "planned round"
        );

        let mut counted = 0u32;
        let mut walked = 0usize;
        let mut unchanged = 0usize;
        let mut left_due = false;
        let mut processed: std::collections::HashSet<&str> = std::collections::HashSet::new();
        for path_s in &plan.due {
            if generation.as_ref().is_some_and(|(current, expected)| {
                current.load(std::sync::atomic::Ordering::Acquire) != *expected
            }) {
                return;
            }
            let path = std::path::Path::new(path_s);
            let is_active = active.as_deref() == Some(path_s.as_str());
            let db_stamp = stamps.get(path_s.as_str()).copied();
            if !needs_walk(is_active, db_stamp, || {
                thegn_core::disk::git_state_unchanged_since(path, db_stamp.unwrap_or(0))
            }) {
                remember_examined(path_s, now);
                processed.insert(path_s.as_str());
                unchanged += 1;
                continue;
            }
            // `0` = unlimited, matching the config convention.
            if shape.walk_budget > 0 && walked >= shape.walk_budget {
                left_due = true;
                continue;
            }
            walked += 1;
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
            if is_active {
                *ACTIVE_COUNTED
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner) =
                    Some((path_s.clone(), now));
            } else {
                // Also covers an incomplete or empty walk: it is retried on
                // the next window's examination, not on every pump.
                remember_examined(path_s, now);
            }
            processed.insert(path_s.as_str());
        }
        tracing::debug!(
            target: LOG,
            scan = "loc",
            walked,
            unchanged,
            counted,
            left_due,
            "round complete"
        );

        // A kick leaves the gate as it found it (open): the next pump sweeps.
        if trigger != RoundTrigger::Kick {
            let next = if left_due {
                Some(now)
            } else {
                let after: Vec<scan_sched::ScanTarget> = targets
                    .iter()
                    .map(|t| scan_sched::ScanTarget {
                        path: t.path.clone(),
                        measured_at: if processed.contains(t.path.as_str()) {
                            Some(now)
                        } else {
                            t.measured_at
                        },
                        active: t.active,
                    })
                    .collect();
                scan_sched::next_due_at(&after, now, shape.ttl_secs)
            };
            // An empty registry still re-checks once a window, so a worktree
            // registered by another process is noticed without a kick.
            let fallback = now.saturating_add(i64::try_from(shape.ttl_secs).unwrap_or(i64::MAX));
            super::LOC_GATE.arm(next.or(Some(fallback)), fp);
        }

        if counted > 0
            && let Some(w) = &waker
        {
            let _ = w.wake(); // best-effort: waker pulse: an input nudge must never fail the calling path
        }
    })));
}

/// Last time the ACTIVE worktree was counted, for the watch pre-gate.
static ACTIVE_COUNTED: Mutex<Option<(String, i64)>> = Mutex::new(None);

/// When each background row was last examined (probed or walked) by this
/// process. Process-local by design: the first sweep after start re-examines
/// everything, which is a few `stat`s a row.
static EXAMINED: OnceLock<Mutex<HashMap<String, i64>>> = OnceLock::new();

fn examined() -> std::sync::MutexGuard<'static, HashMap<String, i64>> {
    EXAMINED
        .get_or_init(|| Mutex::new(HashMap::new()))
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn remember_examined(path: &str, at: i64) {
    examined().insert(path.to_owned(), at);
}

/// Non-active rows take `max(db stamp, examined)`; the active row keeps its DB
/// stamp. Pure over the memo snapshot so it is testable.
fn apply_examined(targets: &mut [scan_sched::ScanTarget], memo: &HashMap<String, i64>) {
    for t in targets.iter_mut().filter(|t| !t.active) {
        if let Some(&at) = memo.get(&t.path) {
            t.measured_at = Some(t.measured_at.map_or(at, |db| db.max(at)));
        }
    }
}

/// Whether a planned row costs a tokei walk. The active row and never-counted
/// rows always do; any other row only when its git state moved since its last
/// count (`unchanged` is evaluated lazily, so they cost no `stat` here).
fn needs_walk(is_active: bool, db_stamp: Option<i64>, unchanged: impl FnOnce() -> bool) -> bool {
    is_active || db_stamp.is_none() || !unchanged()
}

/// The watch pre-gate: skip a watch submission when the same active path was
/// counted inside `window_secs`. No DB open.
fn watch_recently_counted(
    memo: Option<&(String, i64)>,
    active: &str,
    now: i64,
    window_secs: u64,
) -> bool {
    memo.is_some_and(|(path, at)| {
        path == active && thegn_core::time_policy::is_fresh(now, Some(*at), window_secs)
    })
}

/// The debounce a watch round honours: `watch_invalidate_secs`, or the TTL when
/// content-driven recounts are off (then only the TTL can make it due).
fn watch_window_secs(cfg: &thegn_core::config::LocConfig) -> u64 {
    if cfg.watch_invalidate_secs == 0 {
        cfg.scan_interval_secs
    } else {
        cfg.watch_invalidate_secs
    }
}

fn loc_params(cfg: &thegn_core::config::LocConfig) -> scan_sched::LaneParams {
    scan_sched::LaneParams {
        ttl_secs: cfg.scan_interval_secs,
        pump_secs: scan_sched::pump_secs(
            cfg.scan_interval_secs,
            crate::hydrate::LOC_PUMP_FLOOR_SECS,
        ),
        walk_budget: cfg.max_scan_per_round as usize,
    }
}

fn loc_fingerprint(cfg: &thegn_core::config::LocConfig) -> u64 {
    super::shape_fingerprint(&[
        cfg.scan_interval_secs,
        u64::from(cfg.max_scan_per_round),
        u64::from(cfg.enabled),
        cfg.watch_invalidate_secs,
    ])
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
    let live: std::collections::HashSet<&str> = live.iter().map(String::as_str).collect();
    examined().retain(|path, _| live.contains(path.as_str()));
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
    fn the_watch_memo_skips_within_the_window() {
        let memo = ("/wt/a".to_string(), NOW - 30);
        assert!(watch_recently_counted(Some(&memo), "/wt/a", NOW, 60));
        assert!(!watch_recently_counted(Some(&memo), "/wt/a", NOW + 31, 60));
        assert!(
            !watch_recently_counted(Some(&memo), "/wt/b", NOW, 60),
            "a different active path is never skipped"
        );
        assert!(!watch_recently_counted(None, "/wt/a", NOW, 60));
        assert!(!watch_recently_counted(Some(&memo), "/wt/a", NOW, 0));
    }

    #[test]
    fn the_watch_window_falls_back_to_the_ttl_when_watching_is_off() {
        let mut cfg = thegn_core::config::LocConfig::default();
        assert_eq!(watch_window_secs(&cfg), 60);
        cfg.watch_invalidate_secs = 0;
        assert_eq!(watch_window_secs(&cfg), 900);
    }

    #[test]
    fn examined_stamps_apply_to_background_rows_only() {
        let mut t = vec![
            ScanTarget::measured("/wt/active", NOW - 5_000).active(),
            ScanTarget::measured("/wt/bg", NOW - 5_000),
            ScanTarget::cold("/wt/cold"),
            ScanTarget::measured("/wt/newer-db", NOW - 10),
        ];
        let memo: HashMap<String, i64> = [
            ("/wt/active".to_string(), NOW),
            ("/wt/bg".to_string(), NOW - 20),
            ("/wt/cold".to_string(), NOW - 30),
            ("/wt/newer-db".to_string(), NOW - 400),
        ]
        .into_iter()
        .collect();
        apply_examined(&mut t, &memo);
        assert_eq!(
            t[0].measured_at,
            Some(NOW - 5_000),
            "active keeps the DB stamp"
        );
        assert_eq!(t[1].measured_at, Some(NOW - 20));
        assert_eq!(t[2].measured_at, Some(NOW - 30));
        assert_eq!(t[3].measured_at, Some(NOW - 10), "max of the two");
    }

    #[test]
    fn only_changed_background_rows_cost_a_walk() {
        let probed = std::cell::Cell::new(false);
        assert!(needs_walk(true, Some(NOW), || unreachable!(
            "active never probes"
        )));
        assert!(needs_walk(false, None, || unreachable!(
            "cold never probes"
        )));
        assert!(!needs_walk(false, Some(NOW), || {
            probed.set(true);
            true
        }));
        assert!(probed.get());
        assert!(needs_walk(false, Some(NOW), || false));
    }

    #[test]
    fn loc_params_use_the_real_pump_floor() {
        let cfg = thegn_core::config::LocConfig::default();
        let p = loc_params(&cfg);
        assert_eq!((p.ttl_secs, p.pump_secs, p.walk_budget), (900, 225, 2));
        let mut other = cfg.clone();
        other.max_scan_per_round = 3;
        assert_ne!(loc_fingerprint(&cfg), loc_fingerprint(&other));
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
