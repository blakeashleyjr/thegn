//! The background `du` size scan behind the sidebar size badges, the bottom-bar
//! `disk` chip, and the statusbar's disk-warning rollup. Was
//! `hydrate::spawn_disk_scan`.

use std::sync::atomic::AtomicBool;

use termwiz::terminal::TerminalWaker;
use thegn_core::db::Db;
use thegn_core::scan_sched;
use thegn_core::store::WorktreeAuxStore;

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

        if !cfg.show_sizes {
            if reaped > 0
                && let Some(w) = &waker
            {
                let _ = w.wake();
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
        for path_s in &due {
            let path = std::path::Path::new(path_s);
            if !path.is_dir() {
                // Vanished since the registry row was written — drop any stale
                // size so the badge clears instead of freezing at its last value.
                let _ = db.delete_worktree_disk(path_s);
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
            let _ = db.put_worktree_disk(path_s, total as i64, usage.target_bytes as i64);
            measured += 1;
        }

        if (measured > 0 || reaped > 0)
            && let Some(w) = &waker
        {
            let _ = w.wake();
        }
    });
}

/// Every cached total, as owned paths for [`thegn_core::disk::net_root_bytes`].
fn cached_sizes(db: &Db) -> Vec<(std::path::PathBuf, u64)> {
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
        let _ = db.delete_worktree_disk(path);
    }
    gone.len()
}
