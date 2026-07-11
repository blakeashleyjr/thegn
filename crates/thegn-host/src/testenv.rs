//! Shared test-only synchronization for tests that mutate process-global state.
//!
//! Several modules' tests redirect `XDG_STATE_HOME` (via `set_var`) so the DB
//! opens against a throwaway dir. The process environment is global, so two such
//! tests in different modules — e.g. `run`'s sidebar-persistence tests and
//! `agent`'s sandbox tests — will clobber each other's `XDG_STATE_HOME` when the
//! test runner schedules them in parallel, unless they serialize on the *same*
//! lock. A per-module `static ENV_LOCK` does NOT do that (two mutexes, one
//! resource). This single crate-wide lock does.
//!
//! Hold it for the entire span between setting and restoring the env var:
//! `let _env = crate::testenv::ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());`
//!
//! Better: use [`EnvVarGuard`], which takes the lock, sets the vars, and
//! **restores their prior values on drop** — even on an early return or panic.
//! A test that sets an env var but forgets to restore it leaks process-global
//! state into every test that runs afterward (this is exactly how a stray
//! `set_var("PATH", "/usr/bin:/bin")` once dropped git out of PATH and broke
//! every later test that shelled out). The guard makes that impossible.

#[cfg(test)]
pub(crate) static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// RAII guard that serializes on [`ENV_LOCK`], sets one or more process env
/// vars, and restores their previous values (or unsets them) when dropped.
///
/// ```ignore
/// let _env = crate::testenv::EnvVarGuard::set(&[("SHELL", "/bin/sh")]);
/// // ... env mutation is live and exclusive until `_env` drops ...
/// ```
#[cfg(test)]
pub(crate) struct EnvVarGuard {
    _lock: std::sync::MutexGuard<'static, ()>,
    restore: Vec<(String, Option<std::ffi::OsString>)>,
}

#[cfg(test)]
impl EnvVarGuard {
    pub(crate) fn set(vars: &[(&str, &str)]) -> Self {
        let lock = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let mut restore = Vec::with_capacity(vars.len());
        for (k, v) in vars {
            restore.push(((*k).to_string(), std::env::var_os(k)));
            // SAFETY: the guard holds ENV_LOCK for its whole lifetime, so no
            // other ENV_LOCK-respecting test reads/writes the env concurrently.
            unsafe { std::env::set_var(k, v) };
        }
        Self {
            _lock: lock,
            restore,
        }
    }
}

#[cfg(test)]
impl Drop for EnvVarGuard {
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
