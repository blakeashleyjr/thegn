//! Session cache for backend candidates whose ensure/exec probe failed.
//!
//! A candidate is remembered only after it passed the current admission and
//! isolation checks. The memo therefore saves the expensive runtime probe while
//! leaving every policy decision in `prepare_sandbox_env` live.

use std::collections::{HashMap, VecDeque};
use std::sync::{Mutex, OnceLock};

const CAPACITY: usize = 256;

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub(crate) struct Key {
    env_name: String,
    placement: String,
    worktree: String,
}

impl Key {
    pub(crate) fn new(env_name: &str, placement: &str, worktree: &str) -> Self {
        Self {
            env_name: env_name.to_string(),
            placement: placement.to_string(),
            worktree: worktree.to_string(),
        }
    }
}

#[derive(Default)]
struct Memo {
    generation: Option<u64>,
    failures: HashMap<Key, HashMap<String, String>>,
    lru: VecDeque<Key>,
}

impl Memo {
    fn observe_generation(&mut self, generation: u64) {
        if self.generation != Some(generation) {
            self.generation = Some(generation);
            self.failures.clear();
            self.lru.clear();
        }
    }

    fn failed(&mut self, generation: u64, key: &Key, backend: &str) -> Option<String> {
        if self.generation != Some(generation) {
            return None;
        }
        let failure = self.failures.get(key)?.get(backend).cloned();
        if failure.is_some() {
            self.touch(key);
        }
        failure
    }

    fn reusable_failure(
        &mut self,
        explicit_choice: bool,
        generation: u64,
        key: &Key,
        backend: &str,
    ) -> Option<String> {
        if explicit_choice {
            None
        } else {
            self.failed(generation, key, backend)
        }
    }

    fn remember_failure(&mut self, generation: u64, key: Key, backend: String, warning: String) {
        if self.generation != Some(generation) {
            return;
        }
        if !self.failures.contains_key(&key) && self.failures.len() == CAPACITY {
            if let Some(evicted) = self.lru.pop_front() {
                self.failures.remove(&evicted);
            }
        }
        self.failures
            .entry(key.clone())
            .or_default()
            .insert(backend, warning);
        self.touch(&key);
    }

    fn clear_worktree(&mut self, worktree: &str) {
        self.failures.retain(|key, _| key.worktree != worktree);
        self.lru.retain(|key| key.worktree != worktree);
    }

    fn touch(&mut self, key: &Key) {
        self.lru.retain(|seen| seen != key);
        self.lru.push_back(key.clone());
    }
}

fn memo() -> &'static Mutex<Memo> {
    static MEMO: OnceLock<Mutex<Memo>> = OnceLock::new();
    MEMO.get_or_init(|| Mutex::new(Memo::default()))
}

pub(crate) fn observe_generation(generation: u64) {
    memo()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .observe_generation(generation);
}

pub(crate) fn reusable_failure(
    explicit_choice: bool,
    generation: u64,
    key: &Key,
    backend: &str,
) -> Option<String> {
    memo()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .reusable_failure(explicit_choice, generation, key, backend)
}

pub(crate) fn remember_failure(generation: u64, key: Key, backend: &str, warning: &str) {
    memo()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .remember_failure(generation, key, backend.to_string(), warning.to_string());
}

pub(crate) fn clear_worktree(worktree: &str) {
    memo()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clear_worktree(worktree);
}

#[cfg(test)]
mod tests {
    use super::{CAPACITY, Key, Memo};

    #[test]
    fn cached_fallback_skips_failed_backend_for_same_session_key() {
        let key = Key::new("dev", "local", "/repo/wt-a");
        let mut memo = Memo::default();
        memo.observe_generation(1);
        let mut attempts = 0;

        for _ in 0..2 {
            if memo
                .reusable_failure(false, 1, &key, "podman-rootless")
                .is_none()
            {
                attempts += 1;
                memo.remember_failure(
                    1,
                    key.clone(),
                    "podman-rootless".into(),
                    "sandbox podman-rootless failed: probe failed".into(),
                );
            }
            attempts += 1; // bwrap is reached and succeeds.
        }

        assert_eq!(attempts, 3, "the second resolution must not probe podman");
        assert!(
            memo.reusable_failure(false, 1, &key, "podman-rootless")
                .is_some()
        );
    }

    #[test]
    fn memo_is_scoped_invalidated_and_bounded() {
        let mut memo = Memo::default();
        memo.observe_generation(1);
        let original = Key::new("dev", "local", "/repo/wt-a");
        memo.remember_failure(1, original.clone(), "podman".into(), "failed".into());
        assert!(
            memo.failed(1, &Key::new("dev", "ssh host", "/repo/wt-a"), "podman")
                .is_none()
        );
        assert!(
            memo.failed(1, &Key::new("dev", "local", "/repo/wt-b"), "podman")
                .is_none()
        );

        memo.observe_generation(2);
        assert!(memo.failed(1, &original, "podman").is_none());
        assert!(memo.failed(2, &original, "podman").is_none());
        memo.remember_failure(2, original.clone(), "podman".into(), "failed".into());
        memo.clear_worktree("/repo/wt-a");
        assert!(memo.failed(2, &original, "podman").is_none());

        for index in 0..=CAPACITY {
            let key = Key::new("dev", "local", &format!("/repo/wt-{index}"));
            memo.remember_failure(2, key, "podman".into(), "failed".into());
        }
        assert_eq!(memo.failures.len(), CAPACITY);
        assert!(
            memo.failed(2, &Key::new("dev", "local", "/repo/wt-0"), "podman")
                .is_none()
        );
    }

    #[test]
    fn explicit_backend_does_not_reuse_a_memoized_failure() {
        let key = Key::new("dev", "local", "/repo/wt-a");
        let mut memo = Memo::default();
        memo.observe_generation(1);
        memo.remember_failure(1, key.clone(), "podman".into(), "failed".into());

        assert!(memo.reusable_failure(false, 1, &key, "podman").is_some());
        // An explicit backend still reaches its normal probe/error path.
        assert!(memo.reusable_failure(true, 1, &key, "podman").is_none());
    }

    #[test]
    fn shared_memo_is_safe_for_parallel_launch_workers() {
        use std::sync::{Arc, Barrier, Mutex};

        let key = Key::new("dev", "local", "/repo/wt-a");
        let mut initial = Memo::default();
        initial.observe_generation(1);
        initial.remember_failure(1, key.clone(), "podman".into(), "failed".into());
        let memo = Arc::new(Mutex::new(initial));
        let start = Arc::new(Barrier::new(5));
        let workers = (0..4)
            .map(|_| {
                let memo = Arc::clone(&memo);
                let start = Arc::clone(&start);
                let key = key.clone();
                std::thread::spawn(move || {
                    start.wait();
                    memo.lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .reusable_failure(false, 1, &key, "podman")
                        .is_some()
                })
            })
            .collect::<Vec<_>>();
        start.wait();
        assert!(workers.into_iter().all(|worker| worker.join().unwrap()));
    }
}
