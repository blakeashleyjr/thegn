//! Shared test-only synchronization for tests that read or mutate process-global
//! environment state.
//!
//! Some tests read the process environment (`std::env::var`) inside the code
//! under test — e.g. [`crate::sandbox::enter_argv`] omits an env pair from the
//! world-readable `--setenv` argv when its value matches the launcher's own env.
//! Such a test is fragile if it assumes a *clean* ambient environment: running
//! `cargo test` inside a live superzej bwrap sandbox leaks `SUPERZEJ_SANDBOX=1`
//! into the runner and flips the outcome. The fix is to control the ambient var
//! explicitly. The process environment is global, so tests that mutate it must
//! serialize on a single crate-wide lock — a per-module `static` would be two
//! mutexes over one resource and would not serialize across modules.
//!
//! [`EnvGuard`] takes the lock, sets or unsets one or more env vars, and
//! **restores their prior values on drop** — even on an early return or panic.
//! A test that mutates an env var but forgets to restore it leaks process-global
//! state into every test that runs afterward; the guard makes that impossible.

#[cfg(test)]
pub(crate) static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// RAII guard that serializes on [`ENV_LOCK`], mutates one or more process env
/// vars, and restores their previous values (or unsets them) when dropped.
///
/// ```ignore
/// let _env = crate::testenv::EnvGuard::unset(&["SUPERZEJ_SANDBOX"]);
/// // ... `SUPERZEJ_SANDBOX` is guaranteed absent and exclusive until `_env` drops ...
/// ```
#[cfg(test)]
pub(crate) struct EnvGuard {
    _lock: std::sync::MutexGuard<'static, ()>,
    restore: Vec<(String, Option<std::ffi::OsString>)>,
}

#[cfg(test)]
impl EnvGuard {
    /// Set each `(key, value)`, snapshotting prior values for restore on drop.
    pub(crate) fn set(vars: &[(&str, &str)]) -> Self {
        Self::mutate(vars.iter().map(|(k, v)| ((*k).to_string(), Some(*v))))
    }

    /// Remove each `key` from the environment, snapshotting prior values for
    /// restore on drop.
    pub(crate) fn unset(keys: &[&str]) -> Self {
        Self::mutate(keys.iter().map(|k| ((*k).to_string(), None)))
    }

    fn mutate<'a>(ops: impl Iterator<Item = (String, Option<&'a str>)>) -> Self {
        let lock = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let mut restore = Vec::new();
        for (k, new) in ops {
            restore.push((k.clone(), std::env::var_os(&k)));
            // SAFETY: the guard holds ENV_LOCK for its whole lifetime, so no
            // other ENV_LOCK-respecting test reads/writes the env concurrently.
            unsafe {
                match new {
                    Some(v) => std::env::set_var(&k, v),
                    None => std::env::remove_var(&k),
                }
            }
        }
        Self {
            _lock: lock,
            restore,
        }
    }
}

#[cfg(test)]
impl Drop for EnvGuard {
    fn drop(&mut self) {
        for (k, prev) in self.restore.drain(..) {
            // SAFETY: ENV_LOCK is still held until this guard finishes dropping.
            unsafe {
                match prev {
                    Some(v) => std::env::set_var(&k, v),
                    None => std::env::remove_var(&k),
                }
            }
        }
    }
}
