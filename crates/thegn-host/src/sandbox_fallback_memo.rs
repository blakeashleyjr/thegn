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
    failures: HashMap<Key, FailureEntry>,
    lru: VecDeque<Key>,
}

#[derive(Default)]
struct FailureEntry {
    generation: u64,
    failures: HashMap<String, String>,
}

impl Memo {
    fn observe_generation(&mut self, generation: u64, key: &Key) {
        if self
            .failures
            .get(key)
            .is_some_and(|entry| entry.generation < generation)
        {
            self.failures.remove(key);
            self.lru.retain(|seen| seen != key);
        }
    }

    fn failed(&mut self, generation: u64, key: &Key, backend: &str) -> Option<String> {
        let found_generation = self.failures.get(key)?.generation;
        if found_generation < generation {
            self.failures.remove(key);
            self.lru.retain(|seen| seen != key);
            return None;
        }
        if found_generation != generation {
            return None;
        }
        let failure = self.failures.get(key)?.failures.get(backend).cloned();
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
        if self
            .failures
            .get(&key)
            .is_some_and(|entry| entry.generation > generation)
        {
            return;
        }
        if !self.failures.contains_key(&key)
            && self.failures.len() == CAPACITY
            && let Some(evicted) = self.lru.pop_front()
        {
            self.failures.remove(&evicted);
        }
        let entry = self
            .failures
            .entry(key.clone())
            .or_insert_with(|| FailureEntry {
                generation,
                failures: HashMap::new(),
            });
        if entry.generation < generation {
            entry.generation = generation;
            entry.failures.clear();
        }
        entry.failures.insert(backend, warning);
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

pub(crate) fn observe_generation(generation: u64, key: &Key) {
    memo()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .observe_generation(generation, key);
}

pub(crate) fn reusable_failure(
    explicit_choice: bool,
    generation: u64,
    key: &Key,
    backend: &str,
) -> Option<String> {
    let result = memo()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .reusable_failure(explicit_choice, generation, key, backend);
    #[cfg(test)]
    if result.is_some() {
        REUSED.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    }
    result
}

pub(crate) fn remember_failure(generation: u64, key: Key, backend: &str, warning: &str) {
    #[cfg(test)]
    ATTEMPTS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    memo()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .remember_failure(generation, key, backend.to_string(), warning.to_string());
}

#[cfg(test)]
static ATTEMPTS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

#[cfg(test)]
pub(crate) fn reset_for_test() {
    clear_all();
    ATTEMPTS.store(0, std::sync::atomic::Ordering::Relaxed);
    REUSED.store(0, std::sync::atomic::Ordering::Relaxed);
}

#[cfg(test)]
pub(crate) fn attempts_for_test() -> usize {
    ATTEMPTS.load(std::sync::atomic::Ordering::Relaxed)
}

#[cfg(test)]
pub(crate) fn reused_for_test() -> usize {
    REUSED.load(std::sync::atomic::Ordering::Relaxed)
}

#[cfg(test)]
pub(crate) fn is_empty_for_test() -> bool {
    memo()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .failures
        .is_empty()
}

#[cfg(test)]
pub(crate) static TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[cfg(test)]
static REUSED: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

pub(crate) fn clear_worktree(worktree: &str) {
    memo()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clear_worktree(worktree);
}

#[cfg(test)]
pub(crate) fn clear_all() {
    let mut memo = memo()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    memo.failures.clear();
    memo.lru.clear();
}

#[cfg(test)]
mod tests {
    use super::{CAPACITY, Key, Memo};

    #[test]
    fn memo_entry_is_scoped_and_generation_is_per_key() {
        let key = Key::new("dev", "local", "/repo/wt-a");
        let mut memo = Memo::default();
        memo.remember_failure(1, key.clone(), "podman-rootless".into(), "failed".into());
        assert!(
            memo.reusable_failure(false, 1, &key, "podman-rootless")
                .is_some()
        );
        assert!(
            memo.failed(
                1,
                &Key::new("dev", "ssh host", "/repo/wt-a"),
                "podman-rootless"
            )
            .is_none()
        );
        assert!(
            memo.failed(
                1,
                &Key::new("dev", "local", "/repo/wt-b"),
                "podman-rootless"
            )
            .is_none()
        );

        memo.observe_generation(2, &key);
        assert!(memo.failed(2, &key, "podman-rootless").is_none());
        memo.remember_failure(
            2,
            key.clone(),
            "podman-rootless".into(),
            "new failure".into(),
        );
        memo.observe_generation(1, &key);
        assert_eq!(
            memo.failed(2, &key, "podman-rootless"),
            Some("new failure".into())
        );
        memo.clear_worktree("/repo/wt-a");
        assert!(memo.failed(2, &key, "podman-rootless").is_none());

        let mut memo = Memo::default();
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
        memo.remember_failure(1, key.clone(), "podman".into(), "failed".into());

        assert!(memo.reusable_failure(false, 1, &key, "podman").is_some());
        // An explicit backend still reaches its normal probe/error path.
        assert!(memo.reusable_failure(true, 1, &key, "podman").is_none());
    }

    #[test]
    fn admitted_generation_bump_is_observed_through_shared_memo() {
        use super::{TEST_LOCK, clear_all, observe_generation, remember_failure, reusable_failure};

        let _guard = TEST_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        clear_all();
        let key = Key::new("dev", "local", "/repo/generation-wt");
        remember_failure(1, key.clone(), "podman", "old failure");
        observe_generation(2, &key);
        assert!(reusable_failure(false, 2, &key, "podman").is_none());

        remember_failure(2, key.clone(), "podman", "new failure");
        observe_generation(1, &key);
        assert_eq!(
            reusable_failure(false, 2, &key, "podman").as_deref(),
            Some("new failure")
        );
        clear_all();
    }

    #[test]
    fn shared_memo_is_safe_for_parallel_launch_workers() {
        use super::{clear_all, observe_generation, remember_failure, reusable_failure};
        use std::sync::{Arc, Barrier};

        let _guard = super::TEST_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        clear_all();
        let key = Key::new("dev", "local", "/repo/wt-a");
        remember_failure(1, key.clone(), "podman", "failed");
        observe_generation(1, &key);
        let start = Arc::new(Barrier::new(5));
        let workers = (0..4)
            .map(|_| {
                let start = Arc::clone(&start);
                let key = key.clone();
                std::thread::spawn(move || {
                    start.wait();
                    reusable_failure(false, 1, &key, "podman").is_some()
                })
            })
            .collect::<Vec<_>>();
        start.wait();
        assert!(workers.into_iter().all(|worker| worker.join().unwrap()));
    }
}
