//! Provision arbitration: the process-global coordination between the flows
//! that can provision a sandbox concurrently (the eager provisioner, the
//! focused materialize, the warm-pool spare builder).
//!
//! Two primitives, both RAII guards over a name registry:
//! - [`sandbox_lock`] — EXCLUSIVE, keyed by the resolved sandbox name.
//!   `provision_provider_env_named` takes it around the whole pipeline so two
//!   concurrent provisions of the SAME sandbox serialize; the loser wakes into
//!   the provision-marker short-circuit and no-ops. The marker alone only
//!   guards *sequential* re-runs (it is written at the END of the pipeline) —
//!   dropping this lock (a7211cc) let an eager and a materialize provision
//!   interleave on one sprite, so the materialize leg raced through the
//!   eager leg's completed steps, returned early, and attached a pane to a
//!   half-provisioned sprite (the premature raw `sh` at `~`, with the splash
//!   returning at the eager leg's next stage).
//! - [`worktree_live_guard`] / [`worktree_live`] — a SHARED "a provision for
//!   this worktree is in flight" flag, held by `provision_worktree`. The
//!   materialize warm-claim fast path consults it: claiming a spare (and
//!   clearing the splash) while the eager provisioner is mid-provision of the
//!   worktree's derived sprite would flip the binding under a live stream and
//!   orphan the derived sandbox.
//!
//! Blocking by design: everything here runs inside `spawn_blocking` tasks
//! (provisioning is minutes of subprocess/network work) — never on the loop.
//!
//! Also home to the warm-pool naming helpers ([`mint_spare_name`],
//! [`flake_lock_hash`]) and the materialize warm-claim step
//! ([`try_claim_spare`]), extracted from the pinned `agent.rs`/`run.rs`.

use std::collections::HashMap;
use std::sync::{Condvar, LazyLock, Mutex, MutexGuard, PoisonError};

/// A name registry with a hold-count per name. Exclusive acquires wait for the
/// count to reach zero; shared acquires just increment. One registry per use
/// (locks vs live-flags) so an exclusive sandbox lock never contends with the
/// shared worktree flag.
struct Registry {
    held: Mutex<HashMap<String, usize>>,
    freed: Condvar,
}

impl Registry {
    fn new() -> Self {
        Self {
            held: Mutex::new(HashMap::new()),
            freed: Condvar::new(),
        }
    }

    /// Lock the map, shrugging off poisoning: a panicked provision thread must
    /// not wedge every future provision in the process.
    fn map(&self) -> MutexGuard<'_, HashMap<String, usize>> {
        self.held.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn acquire(&'static self, name: &str, exclusive: bool) -> Guard {
        let mut held = self.map();
        if exclusive {
            while held.contains_key(name) {
                held = self
                    .freed
                    .wait(held)
                    .unwrap_or_else(PoisonError::into_inner);
            }
        }
        *held.entry(name.to_string()).or_insert(0) += 1;
        Guard {
            reg: self,
            name: name.to_string(),
        }
    }

    fn count(&self, name: &str) -> usize {
        self.map().get(name).copied().unwrap_or(0)
    }
}

/// RAII hold on a registry name; releasing wakes exclusive waiters.
pub(crate) struct Guard {
    reg: &'static Registry,
    name: String,
}

impl Drop for Guard {
    fn drop(&mut self) {
        let mut held = self.reg.map();
        if let Some(n) = held.get_mut(&self.name) {
            *n -= 1;
            if *n == 0 {
                held.remove(&self.name);
            }
        }
        drop(held);
        self.reg.freed.notify_all();
    }
}

/// Exclusive per-sandbox-name provision locks.
static LOCKS: LazyLock<Registry> = LazyLock::new(Registry::new);
/// Shared per-worktree "provision in flight" flags.
static LIVE: LazyLock<Registry> = LazyLock::new(Registry::new);

/// Serialize provisioning of `name` (the RESOLVED sandbox name): blocks while
/// another thread holds the lock for the same name. Take it around the whole
/// provision pipeline; the marker short-circuit makes the woken loser a no-op.
pub(crate) fn sandbox_lock(name: &str) -> Guard {
    LOCKS.acquire(name, true)
}

/// Mark a provision as in flight for `worktree` (shared — the eager and the
/// materialize legs may both hold it; they serialize on [`sandbox_lock`]).
pub(crate) fn worktree_live_guard(worktree: &str) -> Guard {
    LIVE.acquire(worktree, false)
}

/// Whether any provision for `worktree` is currently in flight.
pub(crate) fn worktree_live(worktree: &str) -> bool {
    LIVE.count(worktree) > 0
}

/// A stable-ish generic name for a new warm-pool spare: `<repo>-pool-<hash>`. The
/// hash varies per process+counter so concurrent mints never collide.
pub(crate) fn mint_spare_name(repo: &str) -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static N: AtomicU64 = AtomicU64::new(0);
    let n = N.fetch_add(1, Ordering::Relaxed);
    let slug = std::path::Path::new(repo)
        .file_name()
        .and_then(|s| s.to_str())
        .map(superzej_core::util::slugify)
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "sz".to_string());
    let h = superzej_core::util::short_hash(&format!("{repo}-{}-{n}", std::process::id()), 6);
    format!("{slug}-pool-{h}")
}

/// Hash of the repo's `flake.lock` (staleness key for a spare's seeded devShell);
/// empty when the repo has no lockfile.
pub(crate) fn flake_lock_hash(repo_root: &std::path::Path) -> String {
    std::fs::read(repo_root.join("flake.lock"))
        .ok()
        .map(|b| superzej_core::util::short_hash(&String::from_utf8_lossy(&b), 16))
        .unwrap_or_default()
}

/// The materialize warm-claim fast path: bind a ready pool spare to `worktree`
/// (repo/env resolved from the worktree, branch settled in the spare's clone).
/// `false` when the worktree isn't remote, no spare is ready, or — the
/// arbitration — a provision for this worktree is already IN FLIGHT
/// ([`worktree_live`]): claiming then would clear a live splash, flip the
/// sandbox binding under the running provision, and orphan the derived sprite;
/// the caller falls through to `provision_worktree`, which serializes on the
/// sandbox lock and short-circuits once the live run finishes. Off-loop only
/// (DB + git subprocess + provider network).
pub(crate) fn try_claim_spare(cfg: &superzej_core::config::Config, worktree: &str) -> bool {
    let loc = superzej_core::remote::GitLoc::for_worktree(std::path::Path::new(worktree));
    if !loc.is_remote() || worktree_live(worktree) {
        return false;
    }
    let repo_root = superzej_core::db::Db::open()
        .ok()
        .and_then(|db| db.repo_root_for(worktree).ok().flatten())
        .filter(|s| !s.is_empty())
        .map(std::path::PathBuf::from)
        .or_else(|| superzej_core::repo::main_worktree(std::path::Path::new(worktree)))
        .unwrap_or_else(|| std::path::PathBuf::from(worktree));
    let env_name = cfg
        .resolve_env(&repo_root, &loc, std::path::Path::new(worktree), None)
        .name;
    // off-loop: inside spawn_blocking
    #[expect(clippy::disallowed_methods)]
    let branch = superzej_core::util::git_cmd(std::path::Path::new(worktree))
        .args(["rev-parse", "--abbrev-ref", "HEAD"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .filter(|b| !b.is_empty() && b != "HEAD");
    crate::agent::claim_spare(cfg, worktree, &repo_root, &env_name, branch.as_deref()).is_some()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;
    use std::time::Duration;

    #[test]
    fn sandbox_lock_serializes_same_name() {
        // Thread B must not acquire "same" until A drops it — the eager-vs-
        // materialize double-provision race, distilled.
        let a = sandbox_lock("same");
        let (tx, rx) = mpsc::channel();
        let t = std::thread::spawn(move || {
            let _b = sandbox_lock("same");
            tx.send(()).unwrap();
        });
        assert!(
            rx.recv_timeout(Duration::from_millis(100)).is_err(),
            "second lock on the same sandbox must block while the first is held"
        );
        drop(a);
        assert!(
            rx.recv_timeout(Duration::from_secs(5)).is_ok(),
            "second lock must acquire once the first is released"
        );
        t.join().unwrap();
    }

    #[test]
    fn sandbox_lock_distinct_names_dont_contend() {
        let _a = sandbox_lock("name-a");
        let (tx, rx) = mpsc::channel();
        let t = std::thread::spawn(move || {
            let _b = sandbox_lock("name-b");
            tx.send(()).unwrap();
        });
        assert!(
            rx.recv_timeout(Duration::from_secs(5)).is_ok(),
            "a lock on a different sandbox must not block"
        );
        t.join().unwrap();
    }

    #[test]
    fn worktree_live_only_while_a_guard_is_held() {
        assert!(!worktree_live("/wt/x"));
        let g1 = worktree_live_guard("/wt/x");
        assert!(worktree_live("/wt/x"));
        // Shared: a second holder (eager + materialize overlap) stacks.
        let g2 = worktree_live_guard("/wt/x");
        drop(g1);
        assert!(
            worktree_live("/wt/x"),
            "still live while the second holder remains"
        );
        drop(g2);
        assert!(!worktree_live("/wt/x"));
        // Independent per worktree.
        let _g = worktree_live_guard("/wt/x");
        assert!(!worktree_live("/wt/y"));
    }

    #[test]
    fn mint_spare_name_is_repo_slugged_and_unique() {
        let a = mint_spare_name("/home/u/code/superzej");
        let b = mint_spare_name("/home/u/code/superzej");
        assert!(a.starts_with("superzej-pool-"), "{a}");
        assert_ne!(a, b, "concurrent mints must never collide");
    }

    #[test]
    fn flake_lock_hash_empty_without_lockfile() {
        let dir = std::env::temp_dir().join("sz-gate-no-flake");
        let _ = std::fs::create_dir_all(&dir);
        assert_eq!(flake_lock_hash(&dir), "");
    }
}
