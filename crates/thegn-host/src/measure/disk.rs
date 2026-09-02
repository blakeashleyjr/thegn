//! The background `du` size scan behind the sidebar size badges, the bottom-bar
//! `disk` chip, and the statusbar's disk-warning rollup. Was
//! `hydrate::spawn_disk_scan`.

use std::sync::atomic::AtomicBool;

use termwiz::terminal::TerminalWaker;
use thegn_core::db::Db;
use thegn_core::scan_sched;
use thegn_core::store::{WorkspaceStore, WorktreeAuxStore};

use super::LOG;

static INFLIGHT: AtomicBool = AtomicBool::new(false);

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
    tokio::task::spawn_blocking(move || {
        let Some(_round) = super::begin(&INFLIGHT, "disk") else {
            return;
        };
        let Some(_permit) = super::permit("disk") else {
            return;
        };
        let Ok(db) = Db::open() else {
            return;
        };

        // The orphan sweep runs BEFORE the `show_sizes` gate. A row for a
        // removed worktree is never re-measured by the loop below and would
        // otherwise inflate the statusbar total forever — and turning badges off
        // is not a reason to stop reclaiming them.
        let reaped = sweep_orphans(&db);

        // `show_sizes` hides the badges; it is not a reason to stop reclaiming
        // disk. The round still runs when a reclaim rule is on, because the
        // reclaim decision is made from the very measurements this round takes
        // (see `reclaim`). With badges off AND both rules off there is nothing
        // to do beyond the orphan sweep above.
        let reclaim_on = policy.idle_days > 0 || policy.on_low_disk;
        if !cfg.show_sizes && !reclaim_on {
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
        let due = scan_sched::plan(
            &targets,
            thegn_core::util::now(),
            cfg.scan_interval_secs,
            cfg.max_scan_per_round as usize,
        );
        tracing::debug!(
            target: LOG,
            scan = "disk",
            known = targets.len(),
            due = due.len(),
            reaped,
            "planned round"
        );

        let mut measured = 0u32;
        // Freshly measured this round, for the reclaim pass below.
        let mut fresh: Vec<(String, thegn_core::disk::DiskUsage)> = Vec::new();
        for path_s in &due {
            let path = std::path::Path::new(path_s);
            if !path.is_dir() {
                // Vanished since the registry row was written — drop any stale
                // size so the badge clears instead of freezing at its last value.
                let _ = db.delete_worktree_disk(path_s); // best-effort: cache write: the DB is a cache; git/forge stays the source of truth
                measured += 1;
                continue;
            }
            let usage = thegn_core::disk::measure_worktree(path);
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
        }

        let reclaimed = reclaim(&db, &policy, &fresh, active.as_deref());

        if (measured > 0 || reaped > 0 || reclaimed > 0)
            && let Some(w) = &waker
        {
            let _ = w.wake(); // best-effort: waker pulse: an input nudge must never fail the calling path
        }
    });
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
