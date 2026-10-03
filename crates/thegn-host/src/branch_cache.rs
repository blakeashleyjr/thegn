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

/// `(branch, worktree path)` for every worktree of a repo holding a branch
/// (detached / unborn omitted). Unfiltered by cwd: the per-worktree exclusion
/// of "this" checkout is applied by the reader, so one entry serves every tab.
pub(crate) type Holders = Vec<(String, String)>;

#[allow(clippy::type_complexity)]
fn holders_cache() -> &'static Mutex<HashMap<PathBuf, (Holders, Instant)>> {
    static CACHE: OnceLock<Mutex<HashMap<PathBuf, (Holders, Instant)>>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

/// The cached branch holders for `repo_root` plus their age, if present. Same
/// TTL and invalidation as the branch list, so the `git worktree list` behind
/// them is not re-run on every hydration while the Branches section is open.
pub(crate) fn get_holders(repo_root: &Path) -> Option<(Holders, Duration)> {
    let map = holders_cache().lock().unwrap();
    map.get(repo_root)
        .map(|(holders, at)| (holders.clone(), at.elapsed()))
}

pub(crate) fn put_holders(repo_root: &Path, holders: Holders) {
    holders_cache()
        .lock()
        .unwrap()
        .insert(repo_root.to_path_buf(), (holders, Instant::now()));
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
    holders_cache().lock().unwrap().clear();
    // The memoised in-process HEAD reads share the same ref-move signal.
    crate::worktree_snapshot::invalidate_head_reads();
}

/// A branch ref under `refs/heads/*` moved (create/delete/commit/fetch, or the
/// merge-queue advancing the target): request an off-loop heal of the canonical
/// checkout and drop the shared branch cache so the list re-fetches. Kept here
/// (not inline in the loop's `RefreshKind::MainRefMoved` arm) so the god-file
/// `run.rs` stays flat.
pub(crate) fn ref_moved(want_main_sync: &mut bool) {
    *want_main_sync = true;
    invalidate_all();
}

/// Decides when a `MainRefMoved` request may be dropped, and remembers what a
/// heal that ACTUALLY RAN saw. Two senders feed the request: the fs-watcher (a
/// ref moved) and the coarse 20 s backstop ticker (a missed event). While a live
/// watcher vouches for the active worktree, a request at a print the last
/// SPAWNED heal already saw is the backstop firing with nothing to catch.
///
/// The record is made when the heal is spawned ([`HealGate::spawned`]), NOT when
/// the request is handled: the loop's 2 s heal throttle can drop a request after
/// it was accepted, and marking at acceptance would then suppress the backstop
/// forever while the main checkout stays stale.
pub(crate) struct HealGate {
    seen: crate::diff_watch::Seen,
    pending: std::sync::Mutex<Option<PendingHeal>>,
    ref_seen: std::sync::Mutex<Option<std::collections::HashMap<std::path::PathBuf, u64>>>,
}

struct PendingHeal {
    path: std::path::PathBuf,
    print: Option<crate::diff_watch::WatchPrint>,
    ref_gen: Option<u64>,
}

impl HealGate {
    pub(crate) const fn new() -> Self {
        Self {
            seen: crate::diff_watch::Seen::new(),
            pending: std::sync::Mutex::new(None),
            ref_seen: std::sync::Mutex::new(None),
        }
    }

    /// Whether a request for `path` at `print` / `ref_gen` should be acted on.
    /// Acting on it parks what was seen until [`spawned`](Self::spawned).
    pub(crate) fn request(
        &self,
        path: &std::path::Path,
        print: Option<crate::diff_watch::WatchPrint>,
        ref_gen: Option<u64>,
    ) -> bool {
        if self.seen.matches(path, print) {
            return false;
        }
        *self.pending.lock().unwrap_or_else(|e| e.into_inner()) = Some(PendingHeal {
            path: path.to_path_buf(),
            print,
            ref_gen,
        });
        true
    }

    /// The heal for the last accepted request was actually spawned: record it.
    pub(crate) fn spawned(&self) {
        let Some(p) = self
            .pending
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take()
        else {
            return;
        };
        self.seen.mark(&p.path, p.print);
        if let Some(g) = p.ref_gen {
            let mut m = self.ref_seen.lock().unwrap_or_else(|e| e.into_inner());
            let m = m.get_or_insert_with(Default::default);
            if m.len() >= 16 && !m.contains_key(&p.path) {
                m.clear(); // bounded like `Seen`: a cache, not a ledger
            }
            m.insert(p.path, g);
        }
    }

    /// A ref moved since the last SPAWNED heal for `path` (O(1)): the trailing
    /// edge for events the 500 ms refresh throttle swallowed.
    pub(crate) fn ref_moved_since(&self, path: &std::path::Path, ref_gen: Option<u64>) -> bool {
        ref_gen.is_some_and(|g| {
            self.ref_seen
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .as_ref()
                .and_then(|m| m.get(path))
                != Some(&g)
        })
    }
}

static HEAL_GATE: HealGate = HealGate::new();

/// [`ref_moved`] for the loop's `MainRefMoved` arm; see [`HealGate`].
pub(crate) fn ref_moved_unless_unchanged(want_main_sync: &mut bool, active: &std::path::Path) {
    let print = crate::diff_watch::current_print(active);
    let ref_gen = crate::diff_watch::current_ref_generation(active);
    if HEAL_GATE.request(active, print, ref_gen) {
        ref_moved(want_main_sync);
    }
}

/// Called on the loop's model tick: if a ref moved since the last heal that
/// actually ran, request one now. O(1); no timer. Covers events whose own
/// `MainRefMoved` the refresh throttle dropped (trailing edge <= one tick).
pub(crate) fn ref_gen_tick(want_main_sync: &mut bool, active: &std::path::Path) {
    let ref_gen = crate::diff_watch::current_ref_generation(active);
    if HEAL_GATE.ref_moved_since(active, ref_gen) {
        let print = crate::diff_watch::current_print(active);
        *HEAL_GATE.pending.lock().unwrap_or_else(|e| e.into_inner()) = Some(PendingHeal {
            path: active.to_path_buf(),
            print,
            ref_gen,
        });
        ref_moved(want_main_sync);
    }
}

/// The loop's heal decision, extracted so the wiring is testable: when a heal is
/// wanted AND the 2 s throttle allows, run `spawn` and ONLY THEN record on `gate`
/// what it saw. A throttled request records nothing, so it stays due.
pub(crate) fn maybe_spawn_heal_with(
    gate: &HealGate,
    want: bool,
    last: &mut Option<std::time::Instant>,
    spawn: impl FnOnce(),
) -> bool {
    if want && last.is_none_or(|t| t.elapsed() >= std::time::Duration::from_secs(2)) {
        *last = Some(std::time::Instant::now());
        spawn();
        gate.spawned();
        true
    } else {
        false
    }
}

/// [`maybe_spawn_heal_with`] on the process-wide gate (the loop's call).
pub(crate) fn maybe_spawn_heal(
    want: bool,
    last: &mut Option<std::time::Instant>,
    spawn: impl FnOnce(),
) -> bool {
    maybe_spawn_heal_with(&HEAL_GATE, want, last, spawn)
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

    fn wp(g: u64) -> Option<crate::diff_watch::WatchPrint> {
        Some(crate::diff_watch::WatchPrint::for_test(g, 0))
    }

    #[test]
    fn a_throttled_heal_never_suppresses_the_backstop() {
        let gate = HealGate::new();
        let p = std::path::Path::new("/tmp/tg-heal-gate/a");
        // First request at print 1 is accepted...
        assert!(gate.request(p, wp(1), Some(1)));
        // ...but the loop's 2 s throttle dropped it: `spawned` never ran. The
        // backstop firing at the SAME print must still be acted on.
        assert!(gate.request(p, wp(1), Some(1)), "unmarked => still due");
        // Once a heal really spawns, the same print is a no-op backstop...
        gate.spawned();
        assert!(!gate.request(p, wp(1), Some(1)));
        // ...a moved print is acted on, and no live print never skips.
        assert!(gate.request(p, wp(2), Some(2)));
        gate.spawned();
        assert!(gate.request(p, None, None));
    }

    #[test]
    fn the_loop_records_a_heal_only_when_it_really_spawns() {
        let gate = HealGate::new();
        let p = std::path::Path::new("/tmp/tg-heal-gate/c");
        let mut last = None;
        let mut spawns = 0;
        assert!(gate.request(p, wp(1), Some(1)));
        assert!(maybe_spawn_heal_with(&gate, true, &mut last, || spawns += 1));
        assert!(!gate.request(p, wp(1), Some(1)), "spawned => recorded");
        // A second accepted request inside the 2 s throttle is DROPPED...
        assert!(gate.request(p, wp(2), Some(2)));
        assert!(!maybe_spawn_heal_with(&gate, true, &mut last, || spawns += 1));
        assert_eq!(spawns, 1);
        // ...and must stay due (not recorded) for the next backstop.
        assert!(gate.request(p, wp(2), Some(2)));
        // No heal wanted: nothing spawned, nothing recorded.
        assert!(!maybe_spawn_heal_with(&gate, false, &mut None, || {
            spawns += 1
        }));
        assert_eq!(spawns, 1);
    }

    #[test]
    fn the_ref_seen_ledger_is_bounded() {
        let gate = HealGate::new();
        for i in 0..40 {
            let p = std::path::PathBuf::from(format!("/tmp/tg-heal-gate/n{i}"));
            assert!(gate.request(&p, wp(1), Some(1)));
            gate.spawned();
        }
        let n = gate
            .ref_seen
            .lock()
            .unwrap()
            .as_ref()
            .map_or(0, |m| m.len());
        assert!(n <= 16, "{n} entries");
    }

    #[test]
    fn a_ref_move_since_the_last_spawned_heal_is_noticed_in_o1() {
        let gate = HealGate::new();
        let p = std::path::Path::new("/tmp/tg-heal-gate/b");
        assert!(!gate.ref_moved_since(p, None), "no watcher: nothing to say");
        assert!(
            gate.ref_moved_since(p, Some(5)),
            "never healed under a claim"
        );
        assert!(gate.request(p, wp(1), Some(5)));
        assert!(gate.ref_moved_since(p, Some(5)), "accepted but not spawned");
        gate.spawned();
        assert!(!gate.ref_moved_since(p, Some(5)));
        assert!(gate.ref_moved_since(p, Some(6)), "a later ref move");
    }

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
