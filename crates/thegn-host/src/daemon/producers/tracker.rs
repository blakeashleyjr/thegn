//! The daemon's tracker producer (THE-2208, THE-2209): polls the issue
//! tracker for `issue_matched` rules and runs autopilot pickup while the
//! daemon holds the tracker lease. Blocking; the owner runs it in
//! `spawn_blocking`.
//!
//! Each tick refreshes the main checkout of up to [`MAX_REPOS_PER_TICK`]
//! registered local repos through the same `hydrate_tracker` refresh the
//! UI uses (every enabled account of each repo's tracker config, the same
//! cache keys, so the first daemon pass of a repo diffs against the cache the
//! UI wrote). Repos rotate round-robin by key, and the cursor advances only
//! past repos actually handled, so within the time budget none waits more than
//! `ceil(repos / MAX_REPOS_PER_TICK)` ticks. Autopilot pickup inside the
//! refresh goes through `producers::queues::daemon_pickup`, and only for a
//! repo whose `[autopilot]` is enabled. One `ListMemo` per tick means an
//! account shared by several repos is listed once per tick, not once per repo.
#![allow(dead_code, reason = "the tick is driven by the b161 lane 5 owner")]

use std::collections::{BTreeSet, HashMap};
use std::path::Path;
use std::sync::Mutex;

use thegn_core::store::WorkspaceStore;

use super::{TickCtx, TickOutcome};
use crate::hydrate_tracker::{ListMemo, RefreshSite, refresh_repo_memo};

/// At most this many repos are refreshed per tick.
pub(crate) const MAX_REPOS_PER_TICK: usize = 32;

/// A tick stops starting new repos after this long (one cadence), so a slow
/// tracker cannot stretch a tick past the tracker lease TTL.
const TICK_BUDGET: std::time::Duration = std::time::Duration::from_secs(60);

/// The last repo key refreshed. The next tick starts strictly after it in key
/// order (wrapping), so a repo added or removed between ticks cannot shift the
/// rotation past a repo that has not had its turn.
static CURSOR: Mutex<Option<String>> = Mutex::new(None);

/// One tracker tick. Blocking.
pub(crate) fn tick(ctx: &TickCtx) -> TickOutcome {
    if crate::connectivity_gate::should_skip_refresh(
        &crate::hydrate::RefreshKind::Issues,
        thegn_core::connectivity::current(),
    ) {
        return TickOutcome::Skipped("offline");
    }
    let roots = {
        let Ok(db) = thegn_core::db::Db::open() else {
            return TickOutcome::Skipped("database unavailable");
        };
        let Ok(rows) = db.worktrees() else {
            return TickOutcome::Skipped("worktree registry unreadable");
        };
        let mut ambient = HashMap::new();
        local_repo_roots(rows.iter().map(|row| {
            let remote = crate::hydrate::row_is_remote_effective(
                &db,
                &ctx.cfg,
                &row.location,
                row.env_name.as_deref(),
                &row.repo_root,
                &mut ambient,
            );
            (row.repo_root.as_str(), remote)
        }))
    };
    let batch = {
        let cursor = CURSOR
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        next_batch(&roots, cursor.as_deref(), MAX_REPOS_PER_TICK)
    };
    let started = std::time::Instant::now();
    let mut refreshed = 0;
    // One list call per (account, filter) per tick: repos sharing a tracker
    // account reuse its answer (errors included) for the rest of this tick.
    let memo = ListMemo::default();
    for root in &batch {
        if started.elapsed() >= TICK_BUDGET {
            break;
        }
        let path = Path::new(root);
        if !path.is_dir() {
            *CURSOR
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(root.clone());
            continue;
        }
        let summary = refresh_repo_memo(path, &ctx.cfg, RefreshSite::Daemon, &memo);
        tracing::debug!(
            target: "thegn::producers",
            repo = %root,
            accounts_ok = summary.accounts_ok,
            accounts_failed = summary.accounts_failed,
            events = summary.events,
            "daemon tracker refresh"
        );
        *CURSOR
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(root.clone());
        refreshed += 1;
    }
    TickOutcome::Done { work: refreshed }
}

/// The distinct repo roots that have at least one LOCAL registered worktree,
/// sorted by key. Remote rows (provider, ssh, k8s) are skipped: their trees
/// live off the host, and the UI that owns them refreshes them itself.
fn local_repo_roots<'a>(rows: impl IntoIterator<Item = (&'a str, bool)>) -> Vec<String> {
    rows.into_iter()
        .filter(|(root, remote)| !remote && !root.is_empty())
        .map(|(root, _)| root.to_owned())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect()
}

/// Pure round-robin: up to `max` repos from `sorted`, starting strictly after
/// `after` in key order and wrapping, never repeating one within a batch.
fn next_batch(sorted: &[String], after: Option<&str>, max: usize) -> Vec<String> {
    if sorted.is_empty() || max == 0 {
        return Vec::new();
    }
    let start = after.map_or(0, |after| {
        sorted
            .iter()
            .position(|key| key.as_str() > after)
            .unwrap_or(0)
    });
    sorted
        .iter()
        .cycle()
        .skip(start)
        .take(max.min(sorted.len()))
        .cloned()
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn keys(n: usize) -> Vec<String> {
        (0..n).map(|i| format!("/repo/{i:03}")).collect()
    }

    #[test]
    fn remote_rows_are_skipped() {
        let roots = local_repo_roots([
            ("/repo/a", false),
            ("/repo/a", true),
            ("/repo/remote-only", true),
            ("/repo/b", false),
            ("/repo/a", false),
            ("", false),
        ]);
        assert_eq!(roots, vec!["/repo/a".to_owned(), "/repo/b".to_owned()]);
    }

    #[test]
    fn rotation_is_bounded_and_wraps() {
        let repos = keys(70);
        let first = next_batch(&repos, None, MAX_REPOS_PER_TICK);
        assert_eq!(first.len(), MAX_REPOS_PER_TICK);
        assert_eq!(first[0], repos[0]);
        let second = next_batch(&repos, first.last().map(String::as_str), MAX_REPOS_PER_TICK);
        assert_eq!(second[0], repos[32]);
        let third = next_batch(
            &repos,
            second.last().map(String::as_str),
            MAX_REPOS_PER_TICK,
        );
        // 6 left before the wrap, then back to the start.
        assert_eq!(third[0], repos[64]);
        assert_eq!(third[6], repos[0]);
        assert_eq!(third.len(), MAX_REPOS_PER_TICK);
    }

    #[test]
    fn small_sets_never_repeat_within_a_tick() {
        let repos = keys(3);
        let batch = next_batch(&repos, Some("/repo/001"), MAX_REPOS_PER_TICK);
        assert_eq!(
            batch,
            vec![repos[2].clone(), repos[0].clone(), repos[1].clone()]
        );
        assert!(next_batch(&[], None, MAX_REPOS_PER_TICK).is_empty());
        assert!(next_batch(&repos, None, 0).is_empty());
    }

    #[test]
    fn rotation_is_fair_every_repo_within_ceil_ticks() {
        let repos = keys(100);
        let ticks = repos.len().div_ceil(MAX_REPOS_PER_TICK);
        let mut cursor: Option<String> = None;
        let mut seen = BTreeSet::new();
        for _ in 0..ticks {
            let batch = next_batch(&repos, cursor.as_deref(), MAX_REPOS_PER_TICK);
            cursor = batch.last().cloned();
            seen.extend(batch);
        }
        assert_eq!(seen.len(), repos.len());
    }

    #[test]
    fn a_removed_cursor_repo_resumes_at_its_successor() {
        // The last-refreshed repo was unregistered between ticks: the next
        // tick starts at the first key after it, skipping nobody.
        let repos = vec!["/a".to_owned(), "/c".to_owned(), "/d".to_owned()];
        assert_eq!(next_batch(&repos, Some("/b"), 1), vec!["/c".to_owned()]);
        // A cursor past every key wraps to the start.
        assert_eq!(next_batch(&repos, Some("/z"), 1), vec!["/a".to_owned()]);
    }
}
