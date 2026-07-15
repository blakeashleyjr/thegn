//! Process-global, repo-keyed cache of the local branch list.
//!
//! Branches are a **repo-level** resource: every worktree of a repo shares the
//! same `.git` object/ref store, so `git for-each-ref refs/heads` returns an
//! identical list from any worktree. The only per-worktree difference is which
//! branch is `HEAD` (`is_head`), and that is recomputed locally from the cheap
//! per-worktree `current_branch` at the [`crate::hydrate::build_panel`] join.
//!
//! So the (comparatively heavy) `branches_full` subprocess only needs to run
//! **once per repo**, not once per tab per hydration. This cache holds the last
//! fetched list keyed by repo root and shares it across every worktree tab.
//! Mirrors the global-state shape of [`crate::hydrate::glyph_cache`] /
//! [`crate::panel_header_cache`], so it needs no threading through
//! `build_panel`'s call sites. In-memory only (session-scoped); the
//! `refs/heads/*` fs-watcher ([`crate::hydrate::RefreshKind::MainRefMoved`])
//! invalidates it on branch create/delete/commit/fetch, with the TTL as a
//! backstop.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use thegn_core::gitrefs::BranchInfo;

/// Backstop staleness bound. The ref-watcher does the prompt invalidation, so
/// this only bounds how long a change that somehow slipped the watcher can
/// linger; a few seconds is plenty for a repo-global list.
pub(crate) const BRANCH_CACHE_TTL: Duration = Duration::from_secs(5);

#[allow(clippy::type_complexity)]
fn cache() -> &'static Mutex<HashMap<PathBuf, (Vec<BranchInfo>, Instant)>> {
    static CACHE: OnceLock<Mutex<HashMap<PathBuf, (Vec<BranchInfo>, Instant)>>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

/// The cached branch list for `repo_root` plus its age, if present.
pub(crate) fn get(repo_root: &Path) -> Option<(Vec<BranchInfo>, Duration)> {
    let map = cache().lock().unwrap();
    map.get(repo_root)
        .map(|(branches, at)| (branches.clone(), at.elapsed()))
}

/// Store a freshly-fetched branch list for `repo_root`.
pub(crate) fn put(repo_root: &Path, branches: Vec<BranchInfo>) {
    cache()
        .lock()
        .unwrap()
        .insert(repo_root.to_path_buf(), (branches, Instant::now()));
}

/// Drop every cached list — the next hydration re-fetches for whatever repo is
/// active. Called from the event loop on `RefreshKind::MainRefMoved` (a ref
/// under `refs/heads/*` moved): resolving the affected repo root would need a
/// `git` subprocess, which must never run on the loop, so we clear the whole
/// (tiny, in-memory) map instead. Cheap — no I/O.
pub(crate) fn invalidate_all() {
    cache().lock().unwrap().clear();
}

/// Whether the branch list must be re-fetched now, or can be served from cache.
/// Pure, so it is unit-tested. A missing entry always fetches; a present entry
/// fetches only once it is at least `ttl` old. There is deliberately no
/// `is_active` case (cf. [`crate::hydrate::should_rescan_glyphs`]) — the list is
/// repo-global, so sharing it across the active tab is the whole point;
/// `is_head` is recomputed per-worktree regardless.
pub(crate) fn should_refetch(cached_age: Option<Duration>, ttl: Duration) -> bool {
    match cached_age {
        None => true,
        Some(age) => age >= ttl,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_entry_refetches() {
        assert!(should_refetch(None, BRANCH_CACHE_TTL));
    }

    #[test]
    fn fresh_entry_serves_from_cache() {
        assert!(!should_refetch(
            Some(Duration::from_millis(0)),
            BRANCH_CACHE_TTL
        ));
        assert!(!should_refetch(
            Some(BRANCH_CACHE_TTL - Duration::from_millis(1)),
            BRANCH_CACHE_TTL
        ));
    }

    #[test]
    fn stale_entry_refetches_at_ttl() {
        assert!(should_refetch(Some(BRANCH_CACHE_TTL), BRANCH_CACHE_TTL));
        assert!(should_refetch(
            Some(BRANCH_CACHE_TTL + Duration::from_secs(1)),
            BRANCH_CACHE_TTL
        ));
    }

    #[test]
    fn put_get_invalidate_round_trip() {
        let root = PathBuf::from("/tmp/thegn-branch-cache-test-repo");
        let branches = vec![BranchInfo {
            name: "main".into(),
            is_head: true,
            upstream: Some("origin/main".into()),
            ahead: 0,
            behind: 0,
            upstream_gone: false,
            sha: "abc123".into(),
            date: 0,
            subject: "init".into(),
        }];
        put(&root, branches.clone());
        let (got, _age) = get(&root).expect("entry present after put");
        assert_eq!(got, branches);

        invalidate_all();
        assert!(get(&root).is_none());
    }
}
