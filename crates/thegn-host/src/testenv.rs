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
        let _ambient_roots = thegn_core::testenv::ambient_state_roots();
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

/// Panic before a test writes into the live or caller-provided state root.
#[cfg(test)]
pub(crate) fn refuse_real_state_write(path: &std::path::Path, what: &str) {
    assert!(
        !thegn_core::testenv::is_real_state_path(
            path,
            &thegn_core::util::home(),
            thegn_core::testenv::ambient_state_roots(),
        ),
        "THE-744: refusing {what} write to real state path {}",
        path.display()
    );
}

/// A private state root with the host crate's own environment guard. Keep this
/// guard separate from core's EnvGuard to preserve the existing lock boundary.
#[cfg(test)]
pub(crate) struct IsolatedState {
    _env: EnvVarGuard,
    temp: tempfile::TempDir,
}

#[cfg(test)]
impl IsolatedState {
    pub(crate) fn new() -> Self {
        Self::with_runtime_dir_inner(false)
    }

    pub(crate) fn with_runtime_dir() -> Self {
        Self::with_runtime_dir_inner(true)
    }

    fn with_runtime_dir_inner(runtime_dir: bool) -> Self {
        let temp = tempfile::tempdir().expect("private isolated state dir");
        thegn_core::fsperm::restrict_dir_to_owner(temp.path())
            .expect("restrict isolated state dir");
        let mut vars = thegn_core::testenv::state_env_all(temp.path());
        vars.push(("THEGN_DIR", temp.path().join(".thegn")));
        if runtime_dir {
            vars.push(("XDG_RUNTIME_DIR", temp.path().to_path_buf()));
        }
        let strings: Vec<_> = vars
            .iter()
            .map(|(key, value)| (*key, value.to_str().expect("state path is UTF-8")))
            .collect();
        let env = EnvVarGuard::set(&strings);
        Self { _env: env, temp }
    }

    pub(crate) fn root(&self) -> &std::path::Path {
        self.temp.path()
    }

    pub(crate) fn state_home(&self) -> std::path::PathBuf {
        thegn_core::util::xdg_state_home()
    }

    pub(crate) fn thegn_dir(&self) -> std::path::PathBuf {
        self.root().join(".thegn")
    }
}

#[cfg(test)]
mod isolated_state_tests {
    use super::IsolatedState;

    #[test]
    fn isolated_state_accessors_resolve_inside_root() {
        {
            let state = IsolatedState::new();
            assert!(state.root().is_dir());
            assert!(state.state_home().starts_with(state.root()));
            assert!(state.thegn_dir().starts_with(state.root()));
            assert_eq!(state.state_home(), thegn_core::util::xdg_state_home());
            assert_eq!(state.thegn_dir(), thegn_core::util::thegn_dir());
        }

        let runtime = IsolatedState::with_runtime_dir();
        assert_eq!(
            std::env::var_os("XDG_RUNTIME_DIR").as_deref(),
            Some(runtime.root().as_os_str())
        );
    }
}
