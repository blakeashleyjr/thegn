//! Background per-worktree measurement: the `du` size scan and the tokei LOC
//! count.
//!
//! Both are the same shape — pick targets, ask the pure planner
//! ([`thegn_core::scan_sched`]) what to do this round, do it off the loop on the
//! background lane, pulse the waker so the next hydration paints from cache — so
//! the shared parts live here and the two runners stay small. Lifted out of
//! `hydrate.rs`, which owned the size scan and (worse) ran the LOC walk inline
//! on the interactive hydration lane.
//!
//! Three rules hold for everything in this module:
//!
//! 1. **Never on the loop.** A cold `du` is seconds; a tokei walk on a large
//!    tree is longer. Both run on `spawn_blocking` behind a background-lane
//!    permit ([`crate::sched::bg_permit`]).
//! 2. **Never wake a sleeping sandbox.** Remote/provider worktrees are skipped
//!    outright — measuring them on a timer would fight `[lifecycle]`
//!    hibernation, and their host path is a stub whose size would be a lie.
//! 3. **One round at a time.** A round outliving its pump used to mean
//!    overlapping full scans double-measuring everything and holding several of
//!    the eight background permits, starving the PR/issue/CI refreshes.

pub(crate) mod disk;
pub(crate) mod loc;

use std::sync::atomic::{AtomicBool, Ordering};

use thegn_core::db::Db;
use thegn_core::scan_sched::ScanTarget;
use thegn_core::store::WorkspaceStore;

/// Tracing target for both scanners — `THEGN_LOG=thegn::measure=debug` shows
/// each round's plan, and every reason a round declined to run.
pub(crate) const LOG: &str = "thegn::measure";

/// RAII round guard. Process-global rather than a loop-side `bool` (contrast
/// `run.rs`'s `prq_inflight`) because a scan is fire-and-forget: there is no
/// completion message back to the loop that could clear a loop-side flag, and a
/// Drop-released atomic stays correct even if the round panics.
pub(crate) struct Round(&'static AtomicBool);

impl Drop for Round {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Release);
    }
}

/// Claim the single round slot for a scanner. `None` ⇒ a round is already in
/// flight, so skip — the next pump retries, and everything this round would
/// have measured is still stale (and now older, so it sorts earlier).
pub(crate) fn begin(flag: &'static AtomicBool, what: &'static str) -> Option<Round> {
    match flag.compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire) {
        Ok(_) => Some(Round(flag)),
        Err(_) => {
            tracing::debug!(target: LOG, scan = what, "round already in flight — skipping");
            None
        }
    }
}

/// Reserve a background-lane permit, logging the refusal. The bare
/// [`crate::sched::spawn_bg`] drops its closure silently when the lane is full,
/// which made a starved scanner indistinguishable from a broken one.
pub(crate) fn permit(what: &'static str) -> Option<tokio::sync::OwnedSemaphorePermit> {
    let p = crate::sched::bg_permit();
    if p.is_none() {
        tracing::debug!(target: LOG, scan = what, "background lane full — deferring round");
    }
    p
}

/// Kick both scans now: startup, a worktree created, a workspace added, a
/// worktree switched to.
///
/// Cheap to over-call — the planner drops every target still inside its TTL, so
/// a redundant kick costs one DB read and plans nothing. The point is that a
/// *cold* path (the freshly created worktree, the workspace you just switched
/// to) is measured within a second or two instead of waiting out a full pump
/// interval.
pub(crate) fn kick(tx: &tokio::sync::mpsc::UnboundedSender<crate::hydrate::RefreshKind>) {
    let _ = tx.send(crate::hydrate::RefreshKind::Disk);
    let _ = tx.send(crate::hydrate::RefreshKind::Loc { watch: false });
}

/// Every path a background scan should cover: the worktree registry, plus each
/// workspace's MAIN checkout.
///
/// The workspace roots are the fix for a whole class of "it never shows a size":
/// a workspace's home group is session-only — `workspace_create` and
/// `Session::switch_to_workspace` write `workspaces`/`repos` rows but never a
/// `worktrees` row — so a scan that walked `db.worktrees()` alone left the
/// sidebar workspace row (which does carry `worktree_path = repo_path`) and the
/// bottom-bar `disk` chip on a home tab permanently blank.
///
/// Remote/provider worktrees are excluded: see the module docs, rule 2.
fn candidate_paths(db: &Db) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    if let Ok(rows) = db.worktrees() {
        for row in rows {
            // Resolve from the row's own `location` column rather than
            // `GitLoc::for_worktree`, which would re-open the DB per row.
            let loc = thegn_core::remote::GitLoc::from_db(&row.worktree, Some(&row.location));
            if loc.is_remote() {
                continue;
            }
            out.push(row.worktree);
        }
    }
    if let Ok(rows) = db.workspaces() {
        for w in rows {
            if !w.repo_path.is_empty() {
                out.push(w.repo_path);
            }
        }
    }
    out.sort();
    out.dedup();
    out
}

/// [`candidate_paths`] joined with a cache's fetch stamps and the on-screen
/// worktree, ready for [`thegn_core::scan_sched::plan`].
fn targets(
    db: &Db,
    stamps: &std::collections::HashMap<String, i64>,
    active: Option<&str>,
) -> Vec<ScanTarget> {
    candidate_paths(db)
        .into_iter()
        .map(|path| ScanTarget {
            measured_at: stamps.get(&path).copied(),
            active: active == Some(path.as_str()),
            path,
        })
        .collect()
}

/// The live set for the orphan sweeps. Deliberately the FULL registry, remote
/// worktrees included: a remote worktree isn't measured, but it is still live,
/// and reaping a size some earlier version cached for it is the orphan sweep's
/// job only once the worktree itself is gone.
fn live_paths(db: &Db) -> Vec<String> {
    let mut out: Vec<String> = db
        .worktrees()
        .map(|rows| rows.into_iter().map(|r| r.worktree).collect())
        .unwrap_or_default();
    if let Ok(rows) = db.workspaces() {
        out.extend(
            rows.into_iter()
                .map(|w| w.repo_path)
                .filter(|p| !p.is_empty()),
        );
    }
    out
}

/// The active worktree path as a plain `String`, for the `active` flag.
pub(crate) fn active_key(active: Option<&std::path::Path>) -> Option<String> {
    active.map(|p| p.to_string_lossy().into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    use thegn_core::store::WorktreeAuxStore;

    static TEST_FLAG: AtomicBool = AtomicBool::new(false);

    #[test]
    fn begin_admits_one_round_and_releases_on_drop() {
        let first = begin(&TEST_FLAG, "test").expect("first round is admitted");
        assert!(
            begin(&TEST_FLAG, "test").is_none(),
            "a second concurrent round must be refused"
        );
        drop(first);
        assert!(
            begin(&TEST_FLAG, "test").is_some(),
            "the slot frees when the round ends"
        );
    }

    /// The whole point of covering `workspaces.repo_path`: a workspace's home
    /// group never gets a `worktrees` row, so walking the registry alone left
    /// every workspace row and home tab permanently sizeless.
    #[test]
    fn candidates_cover_workspace_roots_as_well_as_worktrees() {
        let db = Db::open_memory().unwrap();
        db.put_workspace("/repo", "repo", "repo").unwrap();
        db.put_worktree("repo/feat", "/repo", "/wt/feat", "feat", None, None)
            .unwrap();

        let paths = candidate_paths(&db);
        assert!(paths.contains(&"/repo".to_string()), "{paths:?}");
        assert!(paths.contains(&"/wt/feat".to_string()), "{paths:?}");
    }

    /// A provider/remote worktree's host path is a stub — `du`ing it produces a
    /// confident wrong number, and scanning it on a timer would fight
    /// `[lifecycle]` hibernation. It must never become a target.
    #[test]
    fn candidates_exclude_remote_worktrees() {
        let db = Db::open_memory().unwrap();
        db.put_worktree("repo/local", "/repo", "/wt/local", "local", None, None)
            .unwrap();
        db.put_worktree(
            "repo/far",
            "/repo",
            "/wt/far",
            "far",
            Some(r#"{"host":"build-box","path":"/srv/wt","port":22,"forward_agent":false}"#),
            None,
        )
        .unwrap();

        let paths = candidate_paths(&db);
        assert!(paths.contains(&"/wt/local".to_string()), "{paths:?}");
        assert!(!paths.contains(&"/wt/far".to_string()), "{paths:?}");

        // …but it is still LIVE, so the orphan sweep must not reap whatever an
        // earlier version cached for it.
        assert!(live_paths(&db).contains(&"/wt/far".to_string()));
    }

    #[test]
    fn candidates_dedupe_a_root_that_is_also_a_registered_worktree() {
        let db = Db::open_memory().unwrap();
        db.put_workspace("/repo", "repo", "repo").unwrap();
        db.put_worktree("repo/home", "/repo", "/repo", "main", None, None)
            .unwrap();
        let paths = candidate_paths(&db);
        assert_eq!(
            paths.iter().filter(|p| *p == "/repo").count(),
            1,
            "{paths:?}"
        );
    }

    #[test]
    fn targets_carry_the_cache_stamp_and_flag_the_active_worktree() {
        let db = Db::open_memory().unwrap();
        db.put_worktree("repo/a", "/repo", "/wt/a", "a", None, None)
            .unwrap();
        db.put_worktree("repo/b", "/repo", "/wt/b", "b", None, None)
            .unwrap();
        db.put_worktree_disk("/wt/a", 10, 2).unwrap();
        let stamps = db.all_worktree_disk_stamps().unwrap();

        let targets = targets(&db, &stamps, Some("/wt/b"));
        let a = targets.iter().find(|t| t.path == "/wt/a").unwrap();
        let b = targets.iter().find(|t| t.path == "/wt/b").unwrap();
        assert!(a.measured_at.is_some() && !a.active);
        assert!(
            b.measured_at.is_none() && b.active,
            "the on-screen, never-measured worktree is the one that must go first"
        );
        // And the planner agrees.
        let due = thegn_core::scan_sched::plan(&targets, thegn_core::util::now(), 100, 0);
        assert_eq!(due.first().map(String::as_str), Some("/wt/b"));
    }

    #[test]
    fn active_key_stringifies_the_path_or_yields_none() {
        assert_eq!(
            active_key(Some(std::path::Path::new("/wt/a"))).as_deref(),
            Some("/wt/a")
        );
        assert!(active_key(None).is_none());
    }
}
