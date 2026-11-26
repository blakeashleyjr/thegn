//! The background `du` size scan behind the sidebar size badges, the bottom-bar
//! `disk` chip, and the statusbar's disk-warning rollup. Was
//! `hydrate::spawn_disk_scan`.

use std::sync::atomic::AtomicBool;
use std::sync::{Mutex, OnceLock};

use termwiz::terminal::TerminalWaker;
use thegn_core::db::Db;
use thegn_core::scan_sched;
use thegn_core::scan_sched::RoundTrigger;
use thegn_core::store::{WorkspaceStore, WorktreeAuxStore};

use super::LOG;

static INFLIGHT: AtomicBool = AtomicBool::new(false);

type GenerationMemo = std::collections::HashMap<std::path::PathBuf, (u128, u64, Option<u64>)>;
static GENERATION_MEMO: OnceLock<Mutex<GenerationMemo>> = OnceLock::new();
static LAST_WALK: OnceLock<Mutex<std::collections::HashMap<std::path::PathBuf, i64>>> =
    OnceLock::new();

#[cfg(test)]
thread_local! {
    static GENERATION_INVENTORY_CALLS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

fn fingerprint_mtime(profile_dir: &std::path::Path) -> Option<u128> {
    let fingerprint = profile_dir.join(".fingerprint");
    let metadata = match std::fs::symlink_metadata(fingerprint) {
        Ok(metadata) => metadata,
        Err(_) => return None,
    };
    if !metadata.is_dir() || metadata.file_type().is_symlink() {
        return None;
    }
    let modified = match metadata.modified() {
        Ok(modified) => modified,
        Err(_) => return None,
    };
    let since_epoch = match modified.duration_since(std::time::UNIX_EPOCH) {
        Ok(duration) => duration,
        Err(_) => return None,
    };
    Some(since_epoch.as_nanos())
}

fn generation_memoized(profile_dir: &std::path::Path, min_age_secs: u64, now_secs: u64) -> bool {
    let Some(mtime) = fingerprint_mtime(profile_dir) else {
        return false;
    };
    let memo = GENERATION_MEMO
        .get_or_init(|| Mutex::new(GenerationMemo::new()))
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    memo.get(profile_dir)
        .is_some_and(|(recorded_mtime, recorded_min_age_secs, recheck)| {
            *recorded_mtime == mtime
                && *recorded_min_age_secs == min_age_secs
                && recheck.is_none_or(|at| now_secs < at)
        })
}

fn remember_generation_noop(
    profile_dir: &std::path::Path,
    inventory: &[thegn_core::disk_reclaim::Generation],
    policy: thegn_core::disk_reclaim::GenerationPolicy,
    orphan_recheck: Option<u64>,
) {
    let Some(mtime) = fingerprint_mtime(profile_dir) else {
        return;
    };
    // Wake for whichever comes first: an old generation or a young orphan
    // reaching the age floor. Neither moves the `.fingerprint` mtime.
    let recheck = [
        thegn_core::disk_reclaim::generation_recheck_after(inventory, policy),
        orphan_recheck,
    ]
    .into_iter()
    .flatten()
    .min();
    GENERATION_MEMO
        .get_or_init(|| Mutex::new(GenerationMemo::new()))
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .insert(
            profile_dir.to_path_buf(),
            (mtime, policy.min_age_secs, recheck),
        );
}

/// Walk a worktree unconditionally once this long has passed since its last
/// real measurement, however quiet its change sentinels look.
///
/// This is the bound on [`thegn_core::disk::likely_unchanged_since`]'s
/// imprecision: that probe can miss an edit that moves no sentinel, so trusting
/// it forever would let a reported size drift permanently.
///
/// A day, not an hour. The sentinels already catch the case that actually moves
/// these numbers — every `cargo` invocation touches
/// `target/<profile>/.cargo-lock` — so the ceiling exists only for the residual
/// deep-edit case, where being a day stale costs a slightly wrong badge. An
/// hourly ceiling would instead re-walk every worktree every hour whatever
/// happened: on the machine this was diagnosed on that is 490 GiB, 368 GiB of it
/// build output, which is the cost this whole gate exists to remove.
const FORCE_WALK_AFTER_SECS: i64 = 86_400;

fn lazy_skip(cached_at: i64, last_walk: i64, now: i64, quiet: bool) -> bool {
    if cached_at <= 0 {
        return false;
    }
    quiet && now.saturating_sub(last_walk) < FORCE_WALK_AFTER_SECS
}

fn last_walk_at(path: &std::path::Path, cached_at: i64) -> i64 {
    let mut last_walk = LAST_WALK
        .get_or_init(|| Mutex::new(std::collections::HashMap::new()))
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    *last_walk.entry(path.to_path_buf()).or_insert(cached_at)
}

fn forget_last_walk(path: &std::path::Path) {
    LAST_WALK
        .get_or_init(|| Mutex::new(std::collections::HashMap::new()))
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .remove(path);
}

fn remember_walk(path: &std::path::Path, walked_at: i64) {
    LAST_WALK
        .get_or_init(|| Mutex::new(std::collections::HashMap::new()))
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .insert(path.to_path_buf(), walked_at);
}

/// The active worktree is `du`d at most this often; inside the floor its row is
/// re-stamped from cache like a lazy skip.
const ACTIVE_WALK_FLOOR_SECS: i64 = 120;

/// Hook-log pruning runs at most this often from the disk round (it also
/// throttles itself hourly; this keeps an idle round from even resolving the
/// state path).
const HOOK_LOG_PRUNE_EVERY_SECS: i64 = 600;
static HOOK_LOG_PRUNED_AT: Mutex<Option<i64>> = Mutex::new(None);

/// The active-path generation-prune fallback runs at most this often per path.
const ACTIVE_GENERATION_EVERY_SECS: i64 = 300;
static ACTIVE_GENERATION_AT: OnceLock<Mutex<std::collections::HashMap<String, i64>>> =
    OnceLock::new();

/// `true` once `period` seconds have passed since `prev` (or there is none).
fn elapsed_at_least(prev: Option<i64>, now: i64, period: i64) -> bool {
    prev.is_none_or(|at| now.saturating_sub(at) >= period)
}

/// Whether the active worktree's floor allows a real walk now.
fn active_walk_due(last_walk: i64, now: i64) -> bool {
    elapsed_at_least(Some(last_walk), now, ACTIVE_WALK_FLOOR_SECS)
}

/// Claim a once-per-`period` slot recorded in `memo`.
fn take_slot(memo: &Mutex<Option<i64>>, now: i64, period: i64) -> bool {
    let mut last = memo
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if !elapsed_at_least(*last, now, period) {
        return false;
    }
    *last = Some(now);
    true
}

/// Claim the active path's generation-prune fallback slot (once per 5 min).
fn take_active_generation_slot(path: &str, now: i64) -> bool {
    let mut memo = ACTIVE_GENERATION_AT
        .get_or_init(|| Mutex::new(std::collections::HashMap::new()))
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if !elapsed_at_least(memo.get(path).copied(), now, ACTIVE_GENERATION_EVERY_SECS) {
        return false;
    }
    memo.insert(path.to_owned(), now);
    true
}

fn disk_fingerprint(
    cfg: &thegn_core::config::DiskConfig,
    policy: &thegn_core::disk_reclaim::Policy,
) -> u64 {
    super::shape_fingerprint(&[
        cfg.scan_interval_secs,
        u64::from(cfg.max_scan_per_round),
        u64::from(cfg.show_sizes),
        u64::from(cfg.generation_min_age_days),
        u64::from(policy.idle_days),
        u64::from(policy.on_low_disk),
        u64::from(policy.free_warn_pct),
        u64::from(policy.free_critical_pct),
    ])
}

/// Background per-worktree disk scan.
///
/// Ordered by [`scan_sched::plan`], so the ACTIVE worktree and any
/// never-measured one are `du`d before every stale multi-GB one. That ordering
/// is the whole point: the previous version walked the registry in
/// `ORDER BY position, created_at`, which measured a brand-new worktree **last**
/// — the reported "sizes take forever to show up on a new worktree".
///
/// Bounded to `max_scan_per_round`, one round at a time, and never on the loop.
pub(crate) fn spawn_scan(
    cfg: thegn_core::config::DiskConfig,
    policy: thegn_core::disk_reclaim::Policy,
    active: Option<std::path::PathBuf>,
    waker: Option<TerminalWaker>,
) {
    let fp = disk_fingerprint(&cfg, &policy);
    // The disk lane has no Watch trigger: a kick or the pump.
    let trigger = if super::DISK_GATE.take_kick() {
        RoundTrigger::Kick
    } else {
        RoundTrigger::Pump
    };
    if !super::DISK_GATE.admit(thegn_core::util::now(), fp, trigger) {
        return;
    }
    crate::sched::report_replaceable(super::DISK_LANE.submit(Box::new(move || {
        let Some(_round) = super::begin(&INFLIGHT, "disk") else {
            return;
        };
        let Ok(db) = Db::open() else {
            return;
        };
        let now = thegn_core::util::now();
        // When a kick round ends the gate stays as the kick left it (open);
        // otherwise it sleeps until the earliest row falls due.
        let arm = |next: Option<i64>, ttl: u64| {
            if trigger != RoundTrigger::Kick {
                let fallback = now.saturating_add(i64::try_from(ttl).unwrap_or(i64::MAX));
                super::DISK_GATE.arm(next.or(Some(fallback)), fp);
            }
        };

        // The orphan sweep runs BEFORE the `show_sizes` gate. A row for a
        // removed worktree is never re-measured by the loop below and would
        // otherwise inflate the statusbar total forever — and turning badges off
        // is not a reason to stop reclaiming them.
        let reaped = sweep_orphans(&db);
        crate::db_maintain::run_round(&db);
        if take_slot(&HOOK_LOG_PRUNED_AT, now, HOOK_LOG_PRUNE_EVERY_SECS) {
            crate::hook_run::prune_logs(&crate::hook_run::log_root(), now);
        }

        // `show_sizes` hides the badges; it is not a reason to stop reclaiming
        // disk. The round still runs when a reclaim rule is on, because the
        // reclaim decision is made from the very measurements this round takes
        // (see `reclaim`). With badges off AND both rules off there is nothing
        // to do beyond the orphan sweep above.
        let reclaim_on = policy.idle_days > 0 || policy.on_low_disk;
        if !cfg.show_sizes && !reclaim_on {
            arm(None, cfg.scan_interval_secs);
            if reaped > 0
                && let Some(w) = &waker
            {
                let _ = w.wake(); // best-effort: waker pulse: an input nudge must never fail the calling path
            }
            return;
        }

        let stamps = db.all_worktree_disk_stamps().unwrap_or_default();
        let active = super::active_key(active.as_deref());
        let targets = super::targets(&db, &stamps, active.as_deref());
        // TWO budgets, because this lane has two costs. `max_scan_per_round`
        // bounds the `du` below — the expensive thing. The examine cap bounds
        // how many rows the round may *look at*, which after the lazy gate is
        // mostly six `stat`s apiece, and `lane_shape` sizes it to the registry
        // so the lane always reaches an idle round (stretching the effective
        // TTL only past `scan_sched::EXAMINE_CEILING`).
        let walk_budget = cfg.max_scan_per_round as usize;
        let params = scan_sched::LaneParams {
            ttl_secs: cfg.scan_interval_secs,
            pump_secs: scan_sched::pump_secs(
                cfg.scan_interval_secs,
                crate::hydrate::DISK_PUMP_FLOOR_SECS,
            ),
            walk_budget,
        };
        let shape = scan_sched::lane_shape(targets.len(), &params);
        if shape.ttl_secs > params.ttl_secs {
            super::log_stretched_once("disk", targets.len(), shape.ttl_secs);
        }
        let plan = scan_sched::plan_round(&targets, now, &shape, trigger);
        let due = plan.due;
        tracing::debug!(
            target: LOG,
            scan = "disk",
            ?trigger,
            known = targets.len(),
            due = due.len(),
            examine_cap = shape.examine_cap,
            reaped,
            "planned round"
        );

        // One bulk read for the lazy gate below, so a skipped row costs a map
        // lookup rather than a query.
        let cached_all = db.all_worktree_disk().unwrap_or_default();
        let mut measured = 0u32;
        // Rows re-stamped from cache without walking (the lazy gate below).
        let mut skipped = 0u32;
        // Walks actually performed. This — not the row count — is what
        // `max_scan_per_round` bounds, so that a round of cheap skips can drain
        // the whole due list and the lane can reach an idle round.
        let mut walked = 0usize;
        // Freshly measured this round, for the reclaim pass below.
        let mut fresh: Vec<(String, thegn_core::disk::DiskUsage)> = Vec::new();
        // Rows this round stamped (walked, re-stamped or cleared), for arming
        // the gate; `left_due` = the walk budget ran out with rows still due.
        let mut processed: std::collections::HashSet<&str> = std::collections::HashSet::new();
        let mut left_due = false;
        for path_s in &due {
            // `0` means unlimited, matching `plan`'s budget convention.
            if walk_budget > 0 && walked >= walk_budget {
                left_due = true;
                break;
            }
            processed.insert(path_s.as_str());
            let path = std::path::Path::new(path_s);
            if !path.is_dir() {
                // Vanished since the registry row was written — drop any stale
                // size so the badge clears instead of freezing at its last value.
                let _ = db.delete_worktree_disk(path_s); // best-effort: cache write: the DB is a cache; git/forge stays the source of truth
                forget_last_walk(path);
                measured += 1;
                continue;
            }
            // LAZY GATE. The walk below is the expensive thing in this file:
            // all ignore rules off, so it recurses through every build artifact
            // on purpose. For an idle worktree it re-derives a number that has
            // not moved.
            //
            // Skip it when no change sentinel has moved since the cached
            // measurement — but only up to `FORCE_WALK_AFTER_SECS`, because the
            // sentinels can miss a deep edit and a size must not be allowed to
            // drift indefinitely. Past the ceiling we always walk.
            let cached_at = stamps.get(path_s.as_str()).copied().unwrap_or(0);
            let last_walk = last_walk_at(path, cached_at);
            let quiet = thegn_core::disk::likely_unchanged_since(path, cached_at)
                && cached_all.contains_key(path_s.as_str());
            if lazy_skip(cached_at, last_walk, now, quiet)
                && let Some(&(total, target)) = cached_all.get(path_s.as_str())
            {
                // Re-stamp so the scheduler treats the row as fresh; without
                // this it stays due forever and the round never idles.
                let _ = db.put_worktree_disk(path_s, total, target); // best-effort: cache write: the DB is a cache
                skipped += 1;
                continue;
            }
            // ACTIVE WALK FLOOR. The on-screen worktree is the one being
            // built in, so its sentinels move constantly; without a floor it
            // would be `du`d on every 45 s TTL. Inside the floor, re-stamp it
            // from cache exactly like a lazy skip.
            if active.as_deref() == Some(path_s.as_str())
                && !active_walk_due(last_walk, now)
                && let Some(&(total, target)) = cached_all.get(path_s.as_str())
            {
                let _ = db.put_worktree_disk(path_s, total, target); // best-effort: cache write: the DB is a cache
                skipped += 1;
                continue;
            }
            let usage = thegn_core::disk::measure_worktree(path);
            remember_walk(path, thegn_core::util::now());
            // A repo root's `du` already includes any worktree checked out
            // beneath it (`worktree_mode = "in_repo"` puts them at
            // `<root>/.worktrees/<slug>`), so subtract the children we have
            // already measured or the same bytes get counted twice.
            //
            // Re-read per target rather than once per round: the planner orders
            // by staleness, not by nesting, so a child measured earlier in THIS
            // round must already be visible when its parent is folded. (A child
            // not yet measured at all simply isn't subtracted — the root reads
            // high for one round and self-corrects on the next.) An indexed
            // scan of a table this small is free next to the `du` above.
            let known = cached_sizes(&db);
            let known: Vec<(&std::path::Path, u64)> =
                known.iter().map(|(p, b)| (p.as_path(), *b)).collect();
            let total = thegn_core::disk::net_root_bytes(path, usage.total_bytes, &known);
            let _ = db.put_worktree_disk(path_s, total as i64, usage.target_bytes as i64); // best-effort: cache write: the DB is a cache; git/forge stays the source of truth
            fresh.push((path_s.clone(), usage));
            measured += 1;
            walked += 1;
        }

        let reclaimed = reclaim(&db, &policy, &fresh, active.as_deref());
        // The active checkout is marked due whenever its cache stamp is stale,
        // but a recently stamped checkout can pass the lazy gate. Generation
        // pruning is independent of the size measurement, so add the active
        // path only to the generation set as a direct fallback. Keep `fresh`
        // untouched for whole-target reclaim: a fabricated zero measurement
        // would make that path do unnecessary DB, UI-state, and statvfs work.
        let active_fallback = active
            .as_deref()
            .filter(|path| take_active_generation_slot(path, now));
        let generation_rows = generation_prune_rows(&fresh, active_fallback);
        let generation_reclaimed = reap_generation_footprints(
            &generation_rows,
            cfg.generation_min_age_days,
            thegn_core::util::now().max(0) as u64,
        )
        .saturating_add(reap_stale_live_builds(
            &generation_rows,
            policy.idle_days,
            thegn_core::util::now().max(0) as u64,
        ));

        // `skipped` is the whole point of the lazy gate, so make it visible:
        // a healthy steady state is a round that is nearly all skips. A round
        // that is nearly all walks on a quiet machine means the sentinels are
        // not catching something, which is what you would want to see here.
        tracing::debug!(
            target: LOG,
            scan = "disk",
            measured,
            skipped,
            walked,
            walk_budget,
            reclaimed,
            left_due,
            "round complete"
        );

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
        arm(next, shape.ttl_secs);

        if (measured > 0 || reaped > 0 || reclaimed > 0 || generation_reclaimed > 0)
            && let Some(w) = &waker
        {
            let _ = w.wake(); // best-effort: waker pulse: an input nudge must never fail the calling path
        }
    })));
}

/// Every cached total, as owned paths for [`thegn_core::disk::net_root_bytes`].
fn cached_sizes(db: &Db) -> Vec<(std::path::PathBuf, u64)> {
    // This table contains registered worktrees only. Submodule directories are
    // deliberately never inserted as synthetic rows, so their physical bytes
    // remain inside the owning worktree and are counted exactly once.
    db.all_worktree_disk()
        .unwrap_or_default()
        .into_iter()
        .map(|(p, (total, _))| (std::path::PathBuf::from(p), total.max(0) as u64))
        .collect()
}

/// Delete size-cache rows whose worktree has left the registry. Returns how many.
fn sweep_orphans(db: &Db) -> usize {
    let Ok(cached) = db.all_worktree_disk() else {
        return 0;
    };
    let live = super::live_paths(db);
    let gone = scan_sched::orphans(
        cached.keys().map(String::as_str),
        live.iter().map(String::as_str),
    );
    for path in &gone {
        let _ = db.delete_worktree_disk(path); // best-effort: cache write: the DB is a cache; git/forge stays the source of truth
    }
    gone.len()
}

/// Idle / low-disk `target/` reclaim, run at the tail of a size-scan round.
///
/// Only the worktrees measured **this round** are considered, because
/// `newest_mtime` comes out of the walk that just happened and is not cached in
/// the DB (a size row carries bytes, not mtimes). That is a deliberate
/// simplification, and it degrades well: the scheduler sweeps every worktree
/// inside a couple of `[disk] scan_interval_secs` windows, and the low-disk rule
/// re-reads free space each round — so it is a control loop that converges on
/// the free-space target rather than a single global LRU sort. It also means no
/// schema change, and no reclaim decision is ever made from stale measurements.
///
/// Returns the bytes reclaimed. Best-effort throughout: this runs on the
/// background lane and must never take down a scan round.
fn reclaim(
    db: &Db,
    policy: &thegn_core::disk_reclaim::Policy,
    fresh: &[(String, thegn_core::disk::DiskUsage)],
    active: Option<&str>,
) -> u64 {
    use thegn_core::disk_reclaim as rc;

    if fresh.is_empty() || (policy.idle_days == 0 && !policy.on_low_disk) {
        return 0;
    }
    let now = thegn_core::util::now().max(0) as u64;
    let idle_threshold = rc::idle_threshold_secs(policy);

    // Worktrees whose pipeline work nobody has closed yet. Reclaiming one costs
    // the next stage a cold rebuild of work that is still mid-flight, so they
    // are exempt from both rules. One query for the whole round.
    // best-effort: a roster read failure must not disable the whole reclaim —
    // it only means this round loses the exemption, so fall back to "none".
    let awaiting = db.worktrees_with_active_dispatch().unwrap_or_default();

    let candidates: Vec<rc::Candidate> = fresh
        .iter()
        .map(|(path, usage)| {
            let p = std::path::Path::new(path);
            let idle_secs = now.saturating_sub(usage.newest_mtime);
            // `git status` is the only per-candidate subprocess here, so it is
            // deferred to the few that could possibly be picked by the idle rule
            // (the pressure rule ignores dirtiness anyway).
            let dirty = idle_threshold.is_some_and(|t| idle_secs >= t)
                && usage.target_bytes >= rc::MIN_RECLAIM_BYTES
                && thegn_core::util::git_out(p, &["status", "--porcelain"])
                    .is_none_or(|out| !out.trim().is_empty());
            rc::Candidate {
                path: path.clone(),
                target_bytes: usage.target_bytes,
                idle_secs,
                active: active == Some(path.as_str()),
                building: crate::task::slot_active(p),
                dirty,
                awaiting_verification: awaiting.iter().any(|w| w == path),
                reclaimed_secs_ago: last_reclaim_secs_ago(db, path, now),
            }
        })
        .collect();

    // Free space on the filesystem the worktrees live on. Absent (non-unix, or
    // a statvfs error) simply disables the pressure rule for this round.
    let pressure = fresh
        .first()
        .and_then(|(path, _)| thegn_metrics::disk_space(std::path::Path::new(path)))
        .map(|(total_bytes, free_bytes, free_pct)| rc::Pressure {
            free_pct,
            total_bytes,
            free_bytes,
        });

    let plan = rc::plan(&candidates, policy, pressure);
    let mut total = 0u64;
    for item in &plan {
        let path = std::path::Path::new(&item.path);
        match thegn_core::worktree::clean_target(path) {
            Ok(bytes) if bytes > 0 => {
                total += bytes;
                // best-effort: the size cache is a cache; a failed delete just
                // means the badge shows a stale number until the next round.
                let _ = db.delete_worktree_disk(&item.path); // best-effort: cache write: the DB is a cache; git/forge stays the source of truth
                // Stamp the reclaim so the cooldown can see it next round. Without
                // this the pressure rule re-picks the same worktree as soon as a
                // build repopulates `target/` — delete, rebuild, delete.
                // best-effort: losing the stamp costs hysteresis, never correctness.
                let _ = db.set_ui_state(RECLAIM_SCOPE, &item.path, &now.to_string()); // best-effort: cache write: the DB is a cache; git/forge stays the source of truth
                let branch = branch_of(db, &item.path);
                let msg = format!(
                    "target/ reclaimed ({} — {})",
                    thegn_core::disk::human(bytes),
                    item.reason.note()
                );
                // The marker the next attach reads: a cold rebuild is coming,
                // and this says why rather than looking like a broken cache.
                // best-effort: the reclaim already happened and is logged; a
                // failed insert must not abort the rest of the round.
                let _ =
                    crate::automation_events::emit(db, "disk_cleaned", &branch, &msg, &item.path); // best-effort: cache write: the DB is a cache; git/forge stays the source of truth
                tracing::info!(
                    target: LOG,
                    worktree = %item.path,
                    bytes,
                    reason = %item.reason.note(),
                    "reclaimed target/"
                );
            }
            Ok(_) => {}
            Err(e) => {
                tracing::warn!(target: LOG, worktree = %item.path, error = %e, "reclaim failed");
            }
        }
    }
    total
}

#[derive(Debug)]
struct GenerationFootprint {
    package_name: String,
    generation_id: String,
    fingerprint: std::path::PathBuf,
    deps: Vec<std::path::PathBuf>,
    incremental: Vec<std::path::PathBuf>,
    newest_mtime_secs: u64,
    bytes: u64,
}

/// Tracing target for the per-profile prune outcome lines
/// (`THEGN_LOG=thegn::disk=info`).
const DISK_LOG: &str = "thegn::disk";

/// What one profile's prune round did, for its single `thegn::disk` log line.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
struct ProfileRound {
    generations: usize,
    orphans: usize,
    before_bytes: u64,
    planned_bytes: u64,
    removed_bytes: u64,
    failures: usize,
}

/// `outcome` is `pruned` or `skipped`; `reason` is one of `lock_held`,
/// `not_due`, `exempt`, `unsafe_tree` (empty for `pruned`).
fn log_profile_round(profile: &std::path::Path, outcome: &str, reason: &str, round: ProfileRound) {
    // "skipped" / "not_due" fire on every round for every quiet profile, so they
    // are DEBUG; only a prune that did something is worth an INFO line.
    if outcome != "pruned" {
        tracing::debug!(
            target: DISK_LOG,
            profile = %profile.display(),
            outcome,
            reason,
            "cargo profile prune"
        );
        return;
    }
    tracing::info!(
        target: DISK_LOG,
        profile = %profile.display(),
        outcome,
        reason,
        generations = round.generations,
        orphans = round.orphans,
        before_bytes = round.before_bytes,
        planned_bytes = round.planned_bytes,
        removed_bytes = round.removed_bytes,
        after_bytes = round.before_bytes.saturating_sub(round.removed_bytes),
        failures = round.failures,
        "cargo profile prune"
    );
}

/// Cargo output roots whose whole size is the honest `before_bytes`
/// denominator: everything the generation and orphan passes can remove from.
const PROFILE_OUTPUT_ROOTS: [&str; 4] = [".fingerprint", "deps", "incremental", "build"];

/// Bytes under `.fingerprint/`, `deps/`, `incremental/` and `build/` (absent
/// roots count zero). Any symlink or out-of-profile path is an error.
fn profile_output_bytes(profile: &std::path::Path) -> std::io::Result<u64> {
    let mut total = 0u64;
    for root in PROFILE_OUTPUT_ROOTS {
        let path = profile.join(root);
        if std::fs::symlink_metadata(&path).is_err() {
            continue;
        }
        total = total.saturating_add(measure_safe_tree(&path, profile)?.0);
    }
    Ok(total)
}

/// Prune only generation records in profiles measured during this scan. The
/// lock is held from before inventory through the final unlink so Cargo cannot
/// begin a profile build midway through pruning. The same window then removes
/// orphan artifacts: `deps/`, `incremental/` and `build/` entries whose unit
/// hash names no live fingerprint (see [`orphan_inventory`]).
fn reap_generation_footprints(
    fresh: &[(String, thegn_core::disk::DiskUsage)],
    min_age_days: u32,
    now_secs: u64,
) -> u64 {
    use std::fs::OpenOptions;
    use thegn_core::disk_reclaim::{Generation, GenerationPolicy};

    let mut all_removed = 0u64;
    for (worktree, _) in fresh {
        let worktree_path = std::path::Path::new(worktree);
        let target = worktree_path.join("target");
        let profiles = [target.join("debug"), target.join("release")];
        let min_age_secs = u64::from(min_age_days).saturating_mul(86_400);
        if crate::task::slot_active(worktree_path) {
            for profile_dir in profiles.iter().filter(|dir| dir.is_dir()) {
                log_profile_round(profile_dir, "skipped", "exempt", ProfileRound::default());
            }
            continue;
        }
        for profile_dir in profiles {
            let lock_path = profile_dir.join(".cargo-lock");
            // Not a profile at all (no dir, no Cargo lock, no fingerprints):
            // nothing to report per round. Do not create a missing lock file.
            if !profile_dir.is_dir() || std::fs::symlink_metadata(&lock_path).is_err() {
                tracing::debug!(
                    target: LOG,
                    profile = %profile_dir.display(),
                    reason = "not_cargo_profile",
                    "generation prune skipped"
                );
                continue;
            }
            if !safe_directory(&profile_dir) || !safe_regular_file(&lock_path) {
                log_profile_round(
                    &profile_dir,
                    "skipped",
                    "unsafe_tree",
                    ProfileRound::default(),
                );
                continue;
            }
            if fingerprint_mtime(&profile_dir).is_none() {
                tracing::debug!(
                    target: LOG,
                    profile = %profile_dir.display(),
                    reason = "not_cargo_profile",
                    "generation prune skipped"
                );
                continue;
            }
            if generation_memoized(&profile_dir, min_age_secs, now_secs) {
                log_profile_round(&profile_dir, "skipped", "not_due", ProfileRound::default());
                continue;
            }
            let Ok(lock) = OpenOptions::new().read(true).write(true).open(&lock_path) else {
                log_profile_round(
                    &profile_dir,
                    "skipped",
                    "unsafe_tree",
                    ProfileRound::default(),
                );
                continue;
            };
            // `File::try_lock` returns `Ok(())` only after acquiring the lock;
            // both contention and lock errors skip this profile.
            if !matches!(lock.try_lock(), Ok(())) {
                log_profile_round(
                    &profile_dir,
                    "skipped",
                    "lock_held",
                    ProfileRound::default(),
                );
                continue;
            }

            let footprints = match generation_inventory(&profile_dir) {
                Ok(value) => value,
                Err(error) => {
                    tracing::warn!(
                        target: LOG,
                        profile = %profile_dir.display(),
                        error = %error,
                        "generation inventory failed"
                    );
                    log_profile_round(
                        &profile_dir,
                        "skipped",
                        "unsafe_tree",
                        ProfileRound {
                            failures: 1,
                            ..ProfileRound::default()
                        },
                    );
                    continue;
                }
            };
            let inventory: Vec<_> = footprints
                .iter()
                .map(|footprint| Generation {
                    package_name: footprint.package_name.clone(),
                    generation_id: footprint.generation_id.clone(),
                    newest_mtime_secs: footprint.newest_mtime_secs,
                    bytes: footprint.bytes,
                })
                .collect();
            // Honest denominator: the whole of the removable output roots,
            // orphans included, not just the fingerprinted footprints. An
            // unmeasurable tree (a symlink somewhere) falls back to the
            // inventory sum and disables the orphan pass below.
            let measured_before = profile_output_bytes(&profile_dir);
            let mut round = ProfileRound {
                before_bytes: measured_before
                    .as_ref()
                    .copied()
                    .unwrap_or_else(|_| inventory.iter().map(|entry| entry.bytes).sum::<u64>()),
                ..ProfileRound::default()
            };
            let generation_policy = GenerationPolicy { min_age_secs };
            let plan =
                thegn_core::disk_reclaim::plan_generations(&inventory, generation_policy, now_secs);
            for eviction in plan {
                let Some(footprint) = footprints.iter().find(|candidate| {
                    candidate.package_name == eviction.package_name
                        && candidate.generation_id == eviction.generation_id
                }) else {
                    round.failures += 1;
                    continue;
                };
                round.planned_bytes = round.planned_bytes.saturating_add(footprint.bytes);
                round.generations += 1;
                // Cargo treats a missing fingerprint as stale. Delete it
                // before artifacts: if interrupted, leftovers are rebuildable;
                // deleting outputs first while freshness metadata survives is
                // the unsafe ordering.
                if remove_tree_counted(
                    &footprint.fingerprint,
                    &profile_dir,
                    &mut round.removed_bytes,
                )
                .is_err()
                {
                    round.failures += 1;
                    continue;
                }
                for path in footprint.deps.iter().chain(&footprint.incremental) {
                    if remove_tree_counted(path, &profile_dir, &mut round.removed_bytes).is_err() {
                        round.failures += 1;
                    }
                }
            }

            // Orphans, still inside the lock. The live set is every
            // fingerprint the inventory saw (including any just evicted: their
            // own artifacts went with them, and a failed eviction keeps its
            // leftovers protected rather than half-removed twice).
            let live: std::collections::HashSet<&str> = footprints
                .iter()
                .map(|footprint| footprint.generation_id.as_str())
                .collect();
            let mut orphan_unsafe = measured_before.is_err();
            let mut orphan_recheck = None;
            if !orphan_unsafe {
                match orphan_inventory(&profile_dir, &live) {
                    Ok(orphans) => {
                        let rows: Vec<_> = orphans.iter().map(|(_, row)| row.clone()).collect();
                        orphan_recheck = thegn_core::disk_reclaim::orphan_recheck_after(
                            &rows,
                            now_secs,
                            min_age_secs,
                        );
                        for index in thegn_core::disk_reclaim::plan_orphan_artifacts(
                            &rows,
                            now_secs,
                            min_age_secs,
                        ) {
                            let (path, row) = &orphans[index];
                            round.planned_bytes = round.planned_bytes.saturating_add(row.bytes);
                            round.orphans += 1;
                            if remove_tree_counted(path, &profile_dir, &mut round.removed_bytes)
                                .is_err()
                            {
                                round.failures += 1;
                            }
                        }
                    }
                    Err(error) => {
                        tracing::debug!(
                            target: LOG,
                            profile = %profile_dir.display(),
                            error = %error,
                            "orphan inventory aborted"
                        );
                        orphan_unsafe = true;
                    }
                }
            }

            // `planned_bytes` is the measured complete selection; `removed_bytes`
            // counts only successful files actually unlinked. Keep the two
            // metrics distinct when permission or I/O errors cause partial work.
            let (outcome, reason) = if round.planned_bytes > 0 || round.removed_bytes > 0 {
                ("pruned", "")
            } else if orphan_unsafe {
                ("skipped", "unsafe_tree")
            } else {
                ("skipped", "not_due")
            };
            log_profile_round(&profile_dir, outcome, reason, round);
            if round.planned_bytes == 0 && round.generations == 0 && round.orphans == 0 {
                remember_generation_noop(
                    &profile_dir,
                    &inventory,
                    generation_policy,
                    orphan_recheck,
                );
            } else {
                GENERATION_MEMO
                    .get_or_init(|| Mutex::new(GenerationMemo::new()))
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .remove(&profile_dir);
            }
            all_removed = all_removed.saturating_add(round.removed_bytes);
            drop(lock);
        }
    }
    all_removed
}

/// Orphan Cargo outputs in a profile: `deps/` regular files and
/// `incremental/` / `build/` directories whose name ends in a 16-hex unit hash
/// that no live fingerprint carries. Names without such a suffix are ignored.
/// Any symlink directly under one of the three roots aborts the whole scan
/// (the caller then removes nothing), and every candidate is measured with
/// [`measure_safe_tree`], which also refuses symlinks and out-of-profile paths.
fn orphan_inventory(
    profile: &std::path::Path,
    live: &std::collections::HashSet<&str>,
) -> std::io::Result<Vec<(std::path::PathBuf, thegn_core::disk_reclaim::Orphan)>> {
    use thegn_core::disk_reclaim::{Orphan, cargo_unit_hash};

    let mut out = Vec::new();
    for (root, files) in [("deps", true), ("incremental", false), ("build", false)] {
        let root = profile.join(root);
        if std::fs::symlink_metadata(&root).is_err() {
            continue;
        }
        if !safe_directory(&root) {
            return Err(std::io::Error::other("unsafe orphan root"));
        }
        let mut entries = std::fs::read_dir(&root)?.collect::<Result<Vec<_>, _>>()?;
        entries.sort_by_key(std::fs::DirEntry::file_name);
        for entry in entries {
            let ty = entry.file_type()?;
            if ty.is_symlink() {
                return Err(std::io::Error::other("symlink in orphan root"));
            }
            // `deps/` holds files; `incremental/` and `build/` hold one
            // directory per unit. Anything else is not ours to judge.
            if (files && !ty.is_file()) || (!files && !ty.is_dir()) {
                continue;
            }
            let name = entry.file_name();
            let Some(name) = name.to_str() else {
                continue;
            };
            let stem = if files {
                name.split_once('.').map_or(name, |(stem, _)| stem)
            } else {
                name
            };
            let Some(hash) = cargo_unit_hash(stem) else {
                continue;
            };
            if live.contains(hash) {
                continue;
            }
            let path = entry.path();
            let (bytes, newest) = measure_safe_tree(&path, profile)?;
            // A unit directory's own mtime counts too, so a freshly created
            // but still empty one is young rather than epoch-old.
            let mtime_secs = newest.max(mtime_secs_of(&entry.metadata()?));
            out.push((
                path,
                Orphan {
                    hash: hash.to_string(),
                    mtime_secs,
                    bytes,
                },
            ));
        }
    }
    Ok(out)
}

/// Modification time in whole seconds since the epoch; `0` when unknown.
fn mtime_secs_of(meta: &std::fs::Metadata) -> u64 {
    meta.modified()
        .ok()
        .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
        .map_or(0, |since| since.as_secs())
}

/// `just live` staging directories (`scripts/live.py` `LIVE_STAGE_PREFIX`).
const LIVE_BUILD_PREFIX: &str = ".thegn-live-build-";

/// Lock files whose holder marks a stage as in use: Cargo's profile lock and
/// the stage lock `scripts/live.py` holds for the whole staged build.
const LIVE_BUILD_LOCK_NAMES: [&str; 2] = [".cargo-lock", ".thegn-live-build.lock"];

/// Remove `target/.thegn-live-build-*` stages untouched for `idle_days`
/// (`[disk] idle_clean_days`; `0` = off) in the worktrees of this round.
/// Returns bytes removed.
fn reap_stale_live_builds(
    rows: &[(String, thegn_core::disk::DiskUsage)],
    idle_days: u32,
    now_secs: u64,
) -> u64 {
    if idle_days == 0 {
        return 0;
    }
    rows.iter()
        .map(|(worktree, _)| std::path::Path::new(worktree).join("target"))
        .filter(|target| safe_directory(target))
        .map(|target| reap_live_builds_in(&target, idle_days, now_secs))
        .fold(0u64, u64::saturating_add)
}

/// Every stage lock file inside `stage`, refusing symlinks anywhere.
fn live_build_lock_files(stage: &std::path::Path) -> std::io::Result<Vec<std::path::PathBuf>> {
    let mut out = Vec::new();
    let mut stack = vec![stage.to_path_buf()];
    while let Some(dir) = stack.pop() {
        for entry in std::fs::read_dir(&dir)? {
            let entry = entry?;
            let ty = entry.file_type()?;
            if ty.is_symlink() {
                return Err(std::io::Error::other("symlink in live build stage"));
            }
            if ty.is_dir() {
                stack.push(entry.path());
            } else if ty.is_file()
                && entry
                    .file_name()
                    .to_str()
                    .is_some_and(|name| LIVE_BUILD_LOCK_NAMES.contains(&name))
            {
                out.push(entry.path());
            }
        }
    }
    Ok(out)
}

/// Try-lock every lock file. Returns the held handles (kept open until the
/// removal finishes) and whether any lock is taken by someone else. A lock
/// that cannot be opened or tried counts as held: keep rather than guess.
fn try_lock_all(paths: &[std::path::PathBuf]) -> (Vec<std::fs::File>, bool) {
    let mut held = Vec::new();
    for path in paths {
        let Ok(file) = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(path)
        else {
            return (held, true);
        };
        if !matches!(file.try_lock(), Ok(())) {
            return (held, true);
        }
        held.push(file);
    }
    (held, false)
}

fn reap_live_builds_in(target: &std::path::Path, idle_days: u32, now_secs: u64) -> u64 {
    use thegn_core::disk_reclaim::LiveBuild;

    let Ok(entries) = std::fs::read_dir(target) else {
        return 0;
    };
    let mut stages = Vec::new();
    let mut unsafe_trees = 0usize;
    for entry in entries.flatten() {
        let name = entry.file_name();
        if !name.to_str().is_some_and(|name| {
            name.len() > LIVE_BUILD_PREFIX.len() && name.starts_with(LIVE_BUILD_PREFIX)
        }) {
            continue;
        }
        let Ok(ty) = entry.file_type() else {
            unsafe_trees += 1;
            continue;
        };
        if ty.is_symlink() || !ty.is_dir() {
            unsafe_trees += 1;
            continue;
        }
        let path = entry.path();
        // The stage root's own mtime counts: `live.py` creates the stage with
        // `mkdtemp` before any file exists, and that empty dir must read young.
        let inspected = live_build_lock_files(&path).and_then(|locks| {
            let (_, newest) = measure_safe_tree(&path, target)?;
            Ok((locks, newest.max(mtime_secs_of(&entry.metadata()?))))
        });
        let Ok((locks, newest_mtime_secs)) = inspected else {
            unsafe_trees += 1;
            continue;
        };
        let (handles, lock_held) = try_lock_all(&locks);
        stages.push((
            path,
            LiveBuild {
                newest_mtime_secs,
                lock_held,
            },
            handles,
        ));
    }
    if stages.is_empty() && unsafe_trees == 0 {
        return 0;
    }
    let rows: Vec<_> = stages.iter().map(|(_, row, _)| row.clone()).collect();
    let plan = thegn_core::disk_reclaim::plan_stale_live_builds(&rows, now_secs, idle_days);
    let lock_held = rows.iter().filter(|row| row.lock_held).count();
    // Release the locks of every stage that stays, so a build waiting on one
    // is not held up for the rest of this pass.
    for (index, stage) in stages.iter_mut().enumerate() {
        if !plan.contains(&index) {
            stage.2.clear();
        }
    }
    let mut removed_bytes = 0u64;
    let mut failures = 0usize;
    for &index in &plan {
        // The stage's locks stay held (in `stages`) across the removal.
        if remove_tree_counted(&stages[index].0, target, &mut removed_bytes).is_err() {
            failures += 1;
        }
    }
    drop(stages);
    tracing::info!(
        target: DISK_LOG,
        target_dir = %target.display(),
        outcome = if plan.is_empty() { "skipped" } else { "pruned" },
        pruned = plan.len(),
        lock_held,
        not_due = rows.len().saturating_sub(plan.len()).saturating_sub(lock_held),
        unsafe_tree = unsafe_trees,
        removed_bytes,
        failures,
        "live build prune"
    );
    removed_bytes
}

fn generation_prune_rows(
    fresh: &[(String, thegn_core::disk::DiskUsage)],
    active: Option<&str>,
) -> Vec<(String, thegn_core::disk::DiskUsage)> {
    let mut rows = fresh.to_vec();
    if let Some(active_path) = active
        && !rows.iter().any(|(path, _)| path == active_path)
    {
        rows.push((
            active_path.to_owned(),
            thegn_core::disk::DiskUsage::default(),
        ));
    }
    rows
}

/// Inventory `<profile>/.fingerprint/<package>-<id>` records and associate only
/// Cargo outputs for that package and opaque id. Cargo library outputs start
/// with `lib`, while dep-info files and executables do not. Package/target
/// separators admit target names without confusing `foo` with `foobar`.
/// Any unreadable or symlinked path rejects the whole profile/candidate rather
/// than guessing.
fn generation_inventory(profile: &std::path::Path) -> std::io::Result<Vec<GenerationFootprint>> {
    use std::fs;

    #[cfg(test)]
    GENERATION_INVENTORY_CALLS.with(|calls| calls.set(calls.get() + 1));

    fn dirs(path: &std::path::Path) -> std::io::Result<Vec<std::path::PathBuf>> {
        let mut out = Vec::new();
        for entry in fs::read_dir(path)? {
            let entry = entry?;
            let ty = entry.file_type()?;
            if ty.is_symlink() {
                return Err(std::io::Error::other("symlink in generation profile"));
            }
            if ty.is_dir() {
                out.push(entry.path());
            }
        }
        out.sort();
        Ok(out)
    }

    let fingerprint_root = profile.join(".fingerprint");
    let deps_root = profile.join("deps");
    let incremental_root = profile.join("incremental");
    if !safe_directory(profile)
        || !safe_directory(&fingerprint_root)
        || (deps_root.exists() && !safe_directory(&deps_root))
        || (incremental_root.exists() && !safe_directory(&incremental_root))
    {
        return Err(std::io::Error::other("unsafe generation profile directory"));
    }
    let fingerprints = dirs(&fingerprint_root)?;
    let deps = if deps_root.is_dir() {
        fs::read_dir(&deps_root)?.collect::<Result<Vec<_>, _>>()?
    } else {
        Vec::new()
    };
    let incremental = if incremental_root.is_dir() {
        dirs(&incremental_root)?
    } else {
        Vec::new()
    };
    let mut result = Vec::new();
    for fingerprint in fingerprints {
        let Some(dirname) = fingerprint.file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        let Some((package_name, generation_id)) = dirname.rsplit_once('-') else {
            continue;
        };
        if package_name.is_empty() || generation_id.is_empty() {
            continue;
        }
        let marker = format!("-{generation_id}");
        let unit_kind = fingerprint_unit_kind(&fingerprint)?;
        let mut dep_paths = Vec::new();
        for entry in &deps {
            let name = entry.file_name();
            let Some(name) = name.to_str() else {
                continue;
            };
            let stem = name.split_once('.').map_or(name, |(stem, _)| stem);
            if !artifact_matches_generation(stem, package_name, &marker) {
                continue;
            }
            let ty = entry.file_type()?;
            if ty.is_symlink() {
                return Err(std::io::Error::other("symlink in generation deps"));
            }
            if ty.is_file() {
                dep_paths.push(entry.path());
            }
        }
        let incremental_paths: Vec<_> = incremental
            .iter()
            .filter(|path| {
                path.file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(|name| artifact_matches_generation(name, package_name, &marker))
            })
            .cloned()
            .collect();
        let paths = std::iter::once(&fingerprint)
            .chain(dep_paths.iter())
            .chain(incremental_paths.iter());
        let mut bytes = 0u64;
        let mut newest = 0u64;
        for path in paths {
            let (path_bytes, path_newest) = measure_safe_tree(path, profile)?;
            bytes = bytes.saturating_add(path_bytes);
            newest = newest.max(path_newest);
        }
        result.push(GenerationFootprint {
            package_name: format!("{package_name}/{unit_kind}"),
            generation_id: generation_id.to_string(),
            fingerprint,
            deps: dep_paths,
            incremental: incremental_paths,
            newest_mtime_secs: newest,
            bytes,
        });
    }
    Ok(result)
}

/// Cargo names a unit's hash file `<kind>-<target>` (`lib-serde`,
/// `build-script-build-script-build`, `run-build-script-build-script-build`,
/// `test-lib-foo`, `bin-foo`). Generations are only comparable within one kind.
fn fingerprint_unit_kind(dir: &std::path::Path) -> std::io::Result<String> {
    let mut names = Vec::new();
    for entry in std::fs::read_dir(dir)? {
        let name = entry?.file_name();
        let Some(name) = name.to_str() else { continue };
        if name == "invoked.timestamp"
            || name.starts_with("dep-")
            || name.starts_with("output-")
            || name.ends_with(".json")
        {
            continue;
        }
        names.push(name.to_string());
    }
    names.sort();
    Ok(names.join(","))
}

/// Match `<package>-<id>` and `<package>-<target>-<id>` Cargo output names,
/// accepting the `lib` prefix used by Rust library artifacts.
fn artifact_matches_generation(stem: &str, package_name: &str, marker: &str) -> bool {
    let normalized_package = package_name.replace('-', "_");
    let matches_name = |name: &str, package: &str| {
        name.strip_suffix(marker).is_some_and(|base| {
            base == package
                || base
                    .strip_prefix(package)
                    .is_some_and(|suffix| suffix.starts_with('-') || suffix.starts_with('_'))
        })
    };
    let matches_package =
        |name: &str| matches_name(name, package_name) || matches_name(name, &normalized_package);
    matches_package(stem) || stem.strip_prefix("lib").is_some_and(matches_package)
}

fn measure_safe_tree(
    path: &std::path::Path,
    profile: &std::path::Path,
) -> std::io::Result<(u64, u64)> {
    use std::fs;
    let root = profile.canonicalize()?;
    let mut bytes = 0u64;
    let mut newest = 0u64;
    let mut stack = vec![path.to_path_buf()];
    while let Some(current) = stack.pop() {
        let meta = fs::symlink_metadata(&current)?;
        if meta.file_type().is_symlink() || !current.canonicalize()?.starts_with(&root) {
            return Err(std::io::Error::other("unsafe generation footprint path"));
        }
        if meta.is_dir() {
            for entry in fs::read_dir(&current)? {
                stack.push(entry?.path());
            }
        } else if meta.is_file() {
            bytes = bytes.saturating_add(meta.len());
            if let Ok(time) = meta.modified()
                && let Ok(time) = time.duration_since(std::time::UNIX_EPOCH)
            {
                newest = newest.max(time.as_secs());
            }
            if let Ok(time) = meta.accessed()
                && let Ok(time) = time.duration_since(std::time::UNIX_EPOCH)
            {
                newest = newest.max(time.as_secs());
            }
        }
    }
    Ok((bytes, newest))
}

fn safe_directory(path: &std::path::Path) -> bool {
    std::fs::symlink_metadata(path)
        .is_ok_and(|meta| meta.is_dir() && !meta.file_type().is_symlink())
}

fn safe_regular_file(path: &std::path::Path) -> bool {
    std::fs::symlink_metadata(path)
        .is_ok_and(|meta| meta.is_file() && !meta.file_type().is_symlink())
}

/// Remove a tree without following links and add only successfully unlinked
/// file bytes to `removed`.
fn remove_tree_counted(
    path: &std::path::Path,
    profile: &std::path::Path,
    removed: &mut u64,
) -> std::io::Result<()> {
    use std::fs;
    let root = profile.canonicalize()?;
    let meta = fs::symlink_metadata(path)?;
    if meta.file_type().is_symlink() || !path.canonicalize()?.starts_with(&root) {
        return Err(std::io::Error::other("unsafe generation removal path"));
    }
    if meta.is_dir() {
        for entry in fs::read_dir(path)? {
            let entry = entry?;
            remove_tree_counted(&entry.path(), profile, removed)?;
        }
        fs::remove_dir(path)?;
    } else {
        fs::remove_file(path)?;
        *removed = (*removed).saturating_add(meta.len());
    }
    Ok(())
}

/// `ui_state` scope holding the last-reclaim timestamp per worktree. A
/// `ui_state` row rather than a schema column: it is pure hysteresis bookkeeping
/// that may be lost without consequence, so it does not earn a migration.
const RECLAIM_SCOPE: &str = "disk_reclaim_at";

/// Seconds since thegn last reclaimed this worktree, or `None` if it never has
/// (or the stamp is unreadable/corrupt — in which case the worktree is simply
/// treated as never reclaimed, the pre-hysteresis behaviour).
fn last_reclaim_secs_ago(db: &Db, path: &str, now: u64) -> Option<u64> {
    let raw = db.get_ui_state(RECLAIM_SCOPE, path).ok()??;
    let then: u64 = raw.trim().parse().ok()?;
    Some(now.saturating_sub(then))
}

/// Branch label for the reclaim notification; empty when the path is not a
/// registered worktree (a workspace main checkout, say).
fn branch_of(db: &Db, path: &str) -> String {
    db.worktrees()
        .unwrap_or_default()
        .into_iter()
        .find(|w| w.worktree == path)
        .map(|w| w.branch)
        .unwrap_or_default()
}

#[cfg(test)]
mod generation_tests {
    use super::*;
    use std::fs;

    #[test]
    fn lazy_skip_uses_the_last_real_walk_after_cache_restamps() {
        let now = 200_000;
        assert!(!lazy_skip(
            now - 60,
            now - FORCE_WALK_AFTER_SECS - 1,
            now,
            true,
        ));
        assert!(lazy_skip(now - 60, now - 60, now, true));
    }

    #[test]
    fn artifact_names_match_cargo_library_dep_info_and_executable_outputs() {
        assert!(artifact_matches_generation("libpkg-a", "pkg", "-a"));
        assert!(artifact_matches_generation("pkg-a", "pkg", "-a"));
        assert!(artifact_matches_generation("pkg_cli-a", "pkg", "-a"));
        assert!(artifact_matches_generation("libmy_pkg-a", "my-pkg", "-a"));
        assert!(!artifact_matches_generation("libpkgx-a", "pkg", "-a"));
        assert!(!artifact_matches_generation("libpkg-b", "pkg", "-a"));
    }

    fn profile_tree() -> (std::path::PathBuf, std::path::PathBuf) {
        let root = std::env::temp_dir().join(format!(
            "thegn-generation-reclaim-{}-{}-{}",
            std::process::id(),
            thegn_core::util::now(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let profile = root.join("target/debug");
        fs::create_dir_all(profile.join(".fingerprint/pkg-a")).unwrap();
        fs::create_dir_all(profile.join(".fingerprint/pkg-m")).unwrap();
        fs::create_dir_all(profile.join(".fingerprint/pkg-z")).unwrap();
        fs::create_dir_all(profile.join("deps")).unwrap();
        fs::create_dir_all(profile.join("incremental/pkg-a")).unwrap();
        fs::write(profile.join(".cargo-lock"), []).unwrap();
        fs::write(profile.join(".fingerprint/pkg-a/lib-pkg"), b"fingerprint-a").unwrap();
        fs::write(profile.join(".fingerprint/pkg-m/lib-pkg"), b"fingerprint-m").unwrap();
        fs::write(profile.join(".fingerprint/pkg-z/lib-pkg"), b"fingerprint-z").unwrap();
        fs::write(profile.join("deps/libpkg-a.rlib"), b"artifact-a").unwrap();
        fs::write(profile.join("deps/pkg-a.d"), b"dep-info-a").unwrap();
        fs::write(profile.join("deps/pkg-a"), b"executable-a").unwrap();
        fs::write(profile.join("deps/libpkg-m.rlib"), b"artifact-m").unwrap();
        fs::write(profile.join("deps/pkg-m.d"), b"dep-info-m").unwrap();
        fs::write(profile.join("deps/pkg-m"), b"executable-m").unwrap();
        fs::write(profile.join("deps/libpkg-z.rlib"), b"artifact-z").unwrap();
        fs::write(profile.join("deps/pkg-z.d"), b"dep-info-z").unwrap();
        fs::write(profile.join("deps/pkg-z"), b"executable-z").unwrap();
        // Prefix-only matching would incorrectly associate this artifact with
        // package `pkg` when pruning the generation whose ID is `a`.
        fs::write(profile.join("deps/libpkgx-a.rlib"), b"other-package").unwrap();
        fs::write(profile.join("incremental/pkg-a/cache"), b"incremental-a").unwrap();
        #[expect(
            clippy::disallowed_methods,
            reason = "test fixture: a blocking wait on `git init` in a temp dir, off the event loop"
        )]
        // `git_cmd` rather than a bare `Command`: it scrubs GIT_ENV_VARS, and this
        // test can run inside a live thegn worktree whose inherited GIT_DIR would
        // otherwise point `git init` somewhere other than this temp dir.
        let status = thegn_core::util::git_cmd(&root)
            .args(["init", "-q"])
            .status()
            .unwrap();
        assert!(status.success(), "git init failed: {status}");
        fs::write(root.join(".git/info/exclude"), "target/\n").unwrap();
        (root, profile)
    }

    #[test]
    fn unchanged_noop_profiles_skip_inventory_until_fingerprint_mtime_changes() {
        let (root, debug_profile) = profile_tree();
        let release = root.join("target/release");
        fs::create_dir_all(release.join(".fingerprint/only-one")).unwrap();
        fs::write(release.join(".cargo-lock"), []).unwrap();
        fs::write(release.join(".fingerprint/only-one/marker"), b"one").unwrap();
        GENERATION_INVENTORY_CALLS.with(|calls| calls.set(0));
        let fresh = [(
            root.to_string_lossy().into_owned(),
            thegn_core::disk::DiskUsage::default(),
        )];
        assert_eq!(
            reap_generation_footprints(&fresh, 14, thegn_core::util::now().max(0) as u64,),
            0
        );
        GENERATION_INVENTORY_CALLS.with(|calls| assert_eq!(calls.get(), 2));
        assert_eq!(
            reap_generation_footprints(&fresh, 14, thegn_core::util::now().max(0) as u64,),
            0
        );
        GENERATION_INVENTORY_CALLS.with(|calls| assert_eq!(calls.get(), 2));

        fs::write(debug_profile.join(".fingerprint/touched"), b"mtime change").unwrap();
        assert_eq!(
            reap_generation_footprints(&fresh, 14, thegn_core::util::now().max(0) as u64,),
            0
        );
        GENERATION_INVENTORY_CALLS.with(|calls| assert_eq!(calls.get(), 3));
        assert!(debug_profile.join(".fingerprint/pkg-a").exists());
        assert!(debug_profile.join(".fingerprint/pkg-m").exists());
        assert!(debug_profile.join(".fingerprint/pkg-z").exists());
        assert!(debug_profile.join("deps/libpkg-a.rlib").exists());
        assert!(debug_profile.join("deps/libpkg-z.rlib").exists());
        assert!(debug_profile.join("incremental/pkg-a/cache").exists());
        assert!(release.join(".fingerprint/only-one/marker").exists());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn memoized_debug_only_profile_skips_inventory() {
        let (root, _) = profile_tree();
        GENERATION_INVENTORY_CALLS.with(|calls| calls.set(0));
        let fresh = [(
            root.to_string_lossy().into_owned(),
            thegn_core::disk::DiskUsage::default(),
        )];
        let now = thegn_core::util::now().max(0) as u64;
        assert_eq!(reap_generation_footprints(&fresh, 14, now), 0);
        GENERATION_INVENTORY_CALLS.with(|calls| assert_eq!(calls.get(), 1));

        assert_eq!(reap_generation_footprints(&fresh, 14, now), 0);
        GENERATION_INVENTORY_CALLS.with(|calls| assert_eq!(calls.get(), 1));

        assert_eq!(reap_generation_footprints(&fresh, 13, now), 0);
        GENERATION_INVENTORY_CALLS.with(|calls| assert_eq!(calls.get(), 2));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn unlocked_prune_removes_only_selected_footprint_and_records_real_bytes() {
        let (root, profile) = profile_tree();
        let before = fs::read_dir(profile.join(".fingerprint")).unwrap().count();
        let usage = thegn_core::disk::DiskUsage::default();
        let removed = reap_generation_footprints(
            &[(root.to_string_lossy().into_owned(), usage)],
            0,
            u64::MAX,
        );
        assert!(removed > 0);
        assert!(!profile.join(".fingerprint/pkg-a").exists());
        assert!(!profile.join("deps/libpkg-a.rlib").exists());
        assert!(!profile.join("deps/pkg-a.d").exists());
        assert!(!profile.join("deps/pkg-a").exists());
        assert!(!profile.join("incremental/pkg-a").exists());
        assert!(profile.join(".fingerprint/pkg-z").exists());
        assert!(profile.join(".fingerprint/pkg-m").exists());
        assert!(profile.join("deps/libpkg-z.rlib").exists());
        assert!(profile.join("deps/pkg-z.d").exists());
        assert!(profile.join("deps/pkg-z").exists());
        assert!(profile.join("deps/libpkgx-a.rlib").exists());
        assert_eq!(before, 3);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn held_profile_lock_skips_inventory_and_deletion() {
        let (root, profile) = profile_tree();
        let lock = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(profile.join(".cargo-lock"))
            .unwrap();
        lock.try_lock().unwrap();
        let removed = reap_generation_footprints(
            &[(
                root.to_string_lossy().into_owned(),
                thegn_core::disk::DiskUsage::default(),
            )],
            0,
            u64::MAX,
        );
        assert_eq!(removed, 0);
        assert!(profile.join(".fingerprint/pkg-a").exists());
        drop(lock);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn dirty_worktree_is_pruned_for_generation_pruning() {
        let (root, profile) = profile_tree();
        fs::write(root.join("uncommitted.txt"), b"work in progress").unwrap();
        let removed = reap_generation_footprints(
            &[(
                root.to_string_lossy().into_owned(),
                thegn_core::disk::DiskUsage::default(),
            )],
            0,
            u64::MAX,
        );
        assert!(removed > 0);
        assert!(!profile.join(".fingerprint/pkg-a").exists());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn active_worktree_is_pruned_for_generation_pruning() {
        let (root, profile) = profile_tree();
        let worktree = root.to_string_lossy().into_owned();
        // `slot_active` consults only the in-process job registry; it does not
        // open the real thegn database or depend on XDG state.
        assert!(!crate::task::slot_active(&root));
        let removed = reap_generation_footprints(
            &[(worktree.clone(), thegn_core::disk::DiskUsage::default())],
            0,
            u64::MAX,
        );
        assert!(removed > 0);
        assert!(!profile.join(".fingerprint/pkg-a").exists());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn build_script_units_are_not_evicted_by_lib_generations() {
        let root = tempfile::tempdir().unwrap();
        let profile = root.path().join("target/debug");
        fs::create_dir_all(profile.join(".fingerprint")).unwrap();
        fs::write(profile.join(".cargo-lock"), []).unwrap();
        for (fingerprint, marker) in [
            ("pkg-b1", "build-script-build-script-build"),
            ("pkg-r1", "run-build-script-build-script-build"),
            ("pkg-l1", "lib-pkg"),
            ("pkg-l2", "lib-pkg"),
        ] {
            let dir = profile.join(".fingerprint").join(fingerprint);
            fs::create_dir_all(&dir).unwrap();
            fs::write(dir.join(marker), b"fingerprint").unwrap();
        }
        let worktree = root.path().to_string_lossy().into_owned();
        assert_eq!(
            reap_generation_footprints(
                &[(worktree, thegn_core::disk::DiskUsage::default())],
                0,
                u64::MAX,
            ),
            0
        );
        for fingerprint in ["pkg-b1", "pkg-r1", "pkg-l1", "pkg-l2"] {
            assert!(profile.join(".fingerprint").join(fingerprint).exists());
        }
    }

    #[test]
    fn accessed_time_keeps_a_generation_footprint_recent() {
        let root = tempfile::tempdir().unwrap();
        let file = root.path().join("fingerprint");
        fs::write(&file, b"fingerprint").unwrap();
        let accessed = std::time::SystemTime::now();
        std::fs::File::open(&file)
            .unwrap()
            .set_times(
                std::fs::FileTimes::new()
                    .set_modified(std::time::UNIX_EPOCH + std::time::Duration::from_secs(1))
                    .set_accessed(accessed),
            )
            .unwrap();
        let (_, newest) = measure_safe_tree(&file, root.path()).unwrap();
        let accessed_secs = accessed
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        assert!(newest >= accessed_secs);
    }

    #[test]
    fn the_active_walk_floor_holds_for_two_minutes() {
        let now = 1_000_000;
        assert!(!active_walk_due(now - 10, now));
        assert!(!active_walk_due(now - 119, now));
        assert!(active_walk_due(now - 120, now));
        assert!(active_walk_due(0, now), "never walked this process");
        assert!(!active_walk_due(now + 50, now), "a future stamp holds");
    }

    #[test]
    fn once_per_period_slots_are_claimed_once() {
        let memo = Mutex::new(None);
        assert!(take_slot(&memo, 1_000, 600));
        assert!(!take_slot(&memo, 1_599, 600));
        assert!(take_slot(&memo, 1_600, 600));
        assert!(elapsed_at_least(None, 0, 600));

        let path = "/test/active-generation-slot";
        assert!(take_active_generation_slot(path, 10_000));
        assert!(!take_active_generation_slot(path, 10_299));
        assert!(take_active_generation_slot(path, 10_300));
    }

    #[test]
    fn disk_fingerprint_tracks_config_and_policy() {
        let cfg = thegn_core::config::DiskConfig::default();
        let policy = thegn_core::disk_reclaim::Policy {
            idle_days: 0,
            on_low_disk: false,
            free_warn_pct: 10,
            free_critical_pct: 5,
        };
        let mut other = cfg.clone();
        other.max_scan_per_round += 1;
        assert_ne!(
            disk_fingerprint(&cfg, &policy),
            disk_fingerprint(&other, &policy)
        );
        let busier = thegn_core::disk_reclaim::Policy {
            idle_days: 3,
            ..policy
        };
        assert_ne!(
            disk_fingerprint(&cfg, &policy),
            disk_fingerprint(&cfg, &busier)
        );
    }

    #[test]
    fn active_generation_fallback_does_not_enter_reclaim_rows() {
        let fresh = Vec::new();
        let rows = generation_prune_rows(&fresh, Some("/active"));
        assert!(fresh.is_empty());
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].0, "/active");
        assert_eq!(rows[0].1, thegn_core::disk::DiskUsage::default());
    }

    #[test]
    fn failed_profile_inventory_does_not_suppress_a_different_profile() {
        let (root, debug_profile) = profile_tree();
        fs::remove_dir_all(debug_profile.join(".fingerprint")).unwrap();
        fs::write(debug_profile.join(".fingerprint"), b"not a directory").unwrap();

        let release = root.join("target/release");
        fs::create_dir_all(release.join(".fingerprint/pkg-a")).unwrap();
        fs::create_dir_all(release.join(".fingerprint/pkg-m")).unwrap();
        fs::create_dir_all(release.join(".fingerprint/pkg-z")).unwrap();
        fs::create_dir_all(release.join("deps")).unwrap();
        fs::write(release.join(".cargo-lock"), []).unwrap();
        fs::write(release.join(".fingerprint/pkg-a/marker"), b"a").unwrap();
        fs::write(release.join(".fingerprint/pkg-m/marker"), b"m").unwrap();
        fs::write(release.join(".fingerprint/pkg-z/marker"), b"z").unwrap();
        fs::write(release.join("deps/libpkg-a.rlib"), b"artifact-a").unwrap();
        fs::write(release.join("deps/libpkg-m.rlib"), b"artifact-m").unwrap();
        fs::write(release.join("deps/libpkg-z.rlib"), b"artifact-z").unwrap();

        let removed = reap_generation_footprints(
            &[(
                root.to_string_lossy().into_owned(),
                thegn_core::disk::DiskUsage::default(),
            )],
            0,
            u64::MAX,
        );
        assert!(removed > 0);
        assert!(debug_profile.join(".fingerprint").is_file());
        assert!(!release.join(".fingerprint/pkg-a").exists());
        assert!(release.join(".fingerprint/pkg-m").exists());
        assert!(release.join(".fingerprint/pkg-z").exists());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn worktree_without_target_skips_generation_pruning() {
        let root = tempfile::tempdir().unwrap();
        let fresh = [(
            root.path().to_string_lossy().into_owned(),
            thegn_core::disk::DiskUsage::default(),
        )];

        assert_eq!(reap_generation_footprints(&fresh, 14, 0), 0);
    }

    #[test]
    #[expect(
        clippy::disallowed_methods,
        reason = "the fixture initializes an isolated temporary git repository"
    )]
    fn core_git_spawn_counter_counts_git_out_calls() {
        let root = tempfile::tempdir().unwrap();
        let init = thegn_core::util::git_cmd(root.path())
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .args(["init"])
            .output()
            .unwrap();
        assert!(init.status.success());

        let before = thegn_core::util::core_git_spawn_count();
        assert_eq!(
            thegn_core::util::git_out(root.path(), &["rev-parse", "--is-inside-work-tree"]),
            Some("true".to_string())
        );
        let after = thegn_core::util::core_git_spawn_count();
        assert_eq!(after - before, 1);
    }
    const LIVE_HASH: &str = "00112233445566aa";
    const OLD_HASH: &str = "0123456789abcdef";
    const YOUNG_HASH: &str = "fedcba9876543210";

    /// Pin both times so `measure_safe_tree`'s `max(mtime, atime)` sees `at`.
    fn set_file_time(path: &std::path::Path, at: std::time::SystemTime) {
        fs::File::options()
            .write(true)
            .open(path)
            .unwrap()
            .set_times(fs::FileTimes::new().set_modified(at).set_accessed(at))
            .unwrap();
    }

    fn write_file(path: &std::path::Path, bytes: &[u8]) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, bytes).unwrap();
    }

    /// A profile with one live fingerprint, its artifacts, and orphans. The
    /// caller injects `now` thirty days ahead, so everything written here reads
    /// a month old except the files pinned to `young_at`.
    fn orphan_profile(
        root: &std::path::Path,
        young_at: std::time::SystemTime,
    ) -> std::path::PathBuf {
        let profile = root.join("target/debug");
        write_file(&profile.join(".cargo-lock"), b"");
        write_file(
            &profile.join(format!(".fingerprint/pkg-{LIVE_HASH}/lib-pkg")),
            b"fp",
        );
        write_file(
            &profile.join(format!("deps/libpkg-{LIVE_HASH}.rlib")),
            b"live-rlib",
        );
        write_file(&profile.join(format!("deps/pkg-{LIVE_HASH}.d")), b"live-d");
        write_file(
            &profile.join(format!("deps/libpkg-{OLD_HASH}.rlib")),
            b"old-rlib",
        );
        write_file(&profile.join(format!("deps/pkg-{OLD_HASH}.d")), b"old-d");
        write_file(
            &profile.join(format!("deps/libpkg-{YOUNG_HASH}.rlib")),
            b"young",
        );
        set_file_time(
            &profile.join(format!("deps/libpkg-{YOUNG_HASH}.rlib")),
            young_at,
        );
        // Uppercase hex is not Cargo's suffix: ignored, never removed. Its
        // digits differ from every lowercase hash above: on a case-insensitive
        // filesystem (macOS APFS by default) a same-letters name would
        // overwrite that file instead of sitting beside it.
        write_file(
            &profile.join("deps/libpkg-0123456789ABCDEE.rlib"),
            b"not-hex",
        );
        write_file(
            &profile.join(format!("incremental/pkg-{OLD_HASH}/s-x/work.bin")),
            b"old-incr",
        );
        write_file(
            &profile.join(format!("incremental/pkg-{LIVE_HASH}/s-y/work.bin")),
            b"live-incr",
        );
        write_file(
            &profile.join(format!("build/pkg-{OLD_HASH}/out/gen.rs")),
            b"old-build",
        );
        write_file(
            &profile.join(format!("build/pkg-{LIVE_HASH}/output")),
            b"live-build",
        );
        profile
    }

    fn month_ahead() -> (u64, std::time::SystemTime) {
        let now = thegn_core::util::now().max(0) as u64 + 30 * 86_400;
        let young_at = std::time::UNIX_EPOCH + std::time::Duration::from_secs(now - 3_600);
        (now, young_at)
    }

    #[test]
    fn orphan_artifacts_are_pruned_by_age_and_live_ones_kept() {
        let root = tempfile::tempdir().unwrap();
        let (now, young_at) = month_ahead();
        let profile = orphan_profile(root.path(), young_at);
        let removed = reap_generation_footprints(
            &[(
                root.path().to_string_lossy().into_owned(),
                thegn_core::disk::DiskUsage::default(),
            )],
            14,
            now,
        );
        let old_bytes = ["old-rlib", "old-d", "old-incr", "old-build"]
            .iter()
            .map(|s| s.len() as u64)
            .sum::<u64>();
        assert_eq!(removed, old_bytes);
        assert!(
            !profile
                .join(format!("deps/libpkg-{OLD_HASH}.rlib"))
                .exists()
        );
        assert!(!profile.join(format!("deps/pkg-{OLD_HASH}.d")).exists());
        assert!(!profile.join(format!("incremental/pkg-{OLD_HASH}")).exists());
        assert!(!profile.join(format!("build/pkg-{OLD_HASH}")).exists());
        for kept in [
            format!(".fingerprint/pkg-{LIVE_HASH}/lib-pkg"),
            format!("deps/libpkg-{LIVE_HASH}.rlib"),
            format!("deps/pkg-{LIVE_HASH}.d"),
            format!("deps/libpkg-{YOUNG_HASH}.rlib"),
            "deps/libpkg-0123456789ABCDEE.rlib".to_string(),
            format!("incremental/pkg-{LIVE_HASH}/s-y/work.bin"),
            format!("build/pkg-{LIVE_HASH}/output"),
        ] {
            assert!(profile.join(&kept).exists(), "{kept} must survive");
        }
    }

    #[test]
    fn symlink_in_deps_aborts_the_orphan_pass() {
        if !crate::platform::test_symlink_supported() {
            return;
        }
        let root = tempfile::tempdir().unwrap();
        let (now, young_at) = month_ahead();
        let profile = orphan_profile(root.path(), young_at);
        let outside = root.path().join("outside.rlib");
        fs::write(&outside, b"outside").unwrap();
        let link = profile.join("deps/libother-0000000000000001.rlib");
        crate::platform::test_symlink(&outside, &link).unwrap();
        let removed = reap_generation_footprints(
            &[(
                root.path().to_string_lossy().into_owned(),
                thegn_core::disk::DiskUsage::default(),
            )],
            14,
            now,
        );
        assert_eq!(removed, 0);
        assert!(
            profile
                .join(format!("deps/libpkg-{OLD_HASH}.rlib"))
                .exists()
        );
        assert!(
            profile
                .join(format!("build/pkg-{OLD_HASH}/out/gen.rs"))
                .exists()
        );
        assert!(
            fs::symlink_metadata(&link)
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert!(outside.exists());
    }

    #[test]
    fn held_profile_lock_keeps_orphans() {
        let root = tempfile::tempdir().unwrap();
        let (now, young_at) = month_ahead();
        let profile = orphan_profile(root.path(), young_at);
        let lock = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(profile.join(".cargo-lock"))
            .unwrap();
        lock.try_lock().unwrap();
        let removed = reap_generation_footprints(
            &[(
                root.path().to_string_lossy().into_owned(),
                thegn_core::disk::DiskUsage::default(),
            )],
            14,
            now,
        );
        assert_eq!(removed, 0);
        assert!(
            profile
                .join(format!("deps/libpkg-{OLD_HASH}.rlib"))
                .exists()
        );
        drop(lock);
    }

    #[test]
    fn before_bytes_covers_orphans_and_every_output_root() {
        let root = tempfile::tempdir().unwrap();
        let (_, young_at) = month_ahead();
        let profile = orphan_profile(root.path(), young_at);
        let expected = [
            "fp",
            "live-rlib",
            "live-d",
            "old-rlib",
            "old-d",
            "young",
            "not-hex",
            "old-incr",
            "live-incr",
            "old-build",
            "live-build",
        ]
        .iter()
        .map(|s| s.len() as u64)
        .sum::<u64>();
        assert_eq!(profile_output_bytes(&profile).unwrap(), expected);
        // The fingerprint-only inventory sum would miss every orphan.
        let inventory = generation_inventory(&profile)
            .unwrap()
            .iter()
            .map(|footprint| footprint.bytes)
            .sum::<u64>();
        assert!(inventory < expected);
    }

    #[test]
    fn stale_live_builds_are_reaped_only_when_old_unlocked_and_real_dirs() {
        let root = tempfile::tempdir().unwrap();
        let target = root.path().join("target");
        let (now, young_at) = month_ahead();
        write_file(
            &target.join(".thegn-live-build-old1/output/release/thegn"),
            b"old-bin",
        );
        write_file(&target.join(".thegn-live-build-young/build.json"), b"{}");
        set_file_time(&target.join(".thegn-live-build-young/build.json"), young_at);
        let held_lock = target.join(".thegn-live-build-held/output/release/.cargo-lock");
        write_file(&held_lock, b"");
        write_file(
            &target.join(".thegn-live-build-held/output/release/thegn"),
            b"held",
        );
        write_file(&target.join("unrelated/keep"), b"keep");
        let rows = [(
            root.path().to_string_lossy().into_owned(),
            thegn_core::disk::DiskUsage::default(),
        )];

        // `idle_clean_days = 0` turns the rule off.
        assert_eq!(reap_stale_live_builds(&rows, 0, now), 0);
        assert!(target.join(".thegn-live-build-old1").exists());

        let lock = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&held_lock)
            .unwrap();
        lock.try_lock().unwrap();
        let mut outside = None;
        if crate::platform::test_symlink_supported() {
            let elsewhere = root.path().join("elsewhere");
            write_file(&elsewhere.join("precious"), b"precious");
            crate::platform::test_symlink(&elsewhere, &target.join(".thegn-live-build-link"))
                .unwrap();
            write_file(&root.path().join("victim"), b"victim");
            fs::create_dir_all(target.join(".thegn-live-build-inner")).unwrap();
            crate::platform::test_symlink(
                &root.path().join("victim"),
                &target.join(".thegn-live-build-inner/x"),
            )
            .unwrap();
            outside = Some(elsewhere);
        }

        let removed = reap_stale_live_builds(&rows, 14, now);
        assert_eq!(removed, "old-bin".len() as u64);
        assert!(!target.join(".thegn-live-build-old1").exists());
        assert!(target.join(".thegn-live-build-young/build.json").exists());
        assert!(
            target
                .join(".thegn-live-build-held/output/release/thegn")
                .exists()
        );
        assert!(target.join("unrelated/keep").exists());
        if let Some(elsewhere) = outside {
            assert!(elsewhere.join("precious").exists());
            assert!(fs::symlink_metadata(target.join(".thegn-live-build-link")).is_ok());
            assert!(target.join(".thegn-live-build-inner").exists());
            assert!(root.path().join("victim").exists());
        }
        drop(lock);
    }
}
