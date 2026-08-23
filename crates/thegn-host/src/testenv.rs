//! Shared test-only synchronization for tests that mutate process-global state.
//!
//! **Isolating the DB: use [`STATE_HOME_VAR`], not `"XDG_STATE_HOME"`.**
//! `thegn_core::util::xdg_state_home()` reads `%LOCALAPPDATA%` on Windows, so a
//! test that sets only the unix name isolates nothing there — it silently opens
//! the developer's REAL database, and every such test then shares one DB. That
//! is not hypothetical: it is how rows from unrelated tests
//! (`tg-halt-ghost-…`) turned up inside the sidebar-reorder assertions.
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

/// The environment variable `thegn_core::util::xdg_state_home()` actually reads
/// on this platform — the one a test must set to isolate the DB.
///
/// `XDG_STATE_HOME` on unix; `%LOCALAPPDATA%` on Windows, which has no XDG
/// convention. Always redirect through this rather than hardcoding the unix
/// name, or the isolation is a silent no-op off unix.
pub(crate) const STATE_HOME_VAR: &str = if cfg!(windows) {
    "LOCALAPPDATA"
} else {
    "XDG_STATE_HOME"
};

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

/// A trivially-spawnable interactive shell for tests that need a pane to
/// actually come up.
///
/// `/bin/sh` on unix; `cmd.exe` on Windows, where `/bin/sh` is not a path at
/// all (no drive letter) so the spawn simply fails and the pane never lands in
/// the table. Deliberately NOT the MSYS `sh` that ships with Git for Windows:
/// its emulated `fork()` under ConPTY never signals EOF on master close, which
/// turns a fast pane-test failure into a hang.
#[cfg(test)]
pub(crate) const SHELL_PROGRAM: &str = if cfg!(windows) { "cmd.exe" } else { "/bin/sh" };

/// A trivially-spawnable command that starts and exits immediately, for tests
/// that only care that a pane *came up*. `/bin/sh -c true` on unix,
/// `cmd.exe /c exit` on Windows.
#[cfg(test)]
pub(crate) fn noop_argv() -> Vec<String> {
    if cfg!(windows) {
        vec!["cmd.exe".into(), "/c".into(), "exit".into()]
    } else {
        vec!["/bin/sh".into(), "-c".into(), "true".into()]
    }
}

/// Kill whatever children a test's panes still have running.
///
/// Nothing in the product leaves a pane dangling — the close paths reap — but
/// a test that spawns one and never closes it does, and on Windows a live
/// ConPTY child holds the test process's inherited handles open, so the
/// harness waits out its timeout on a process that finished its work in
/// milliseconds. Call this once the assertions are done.
#[cfg(test)]
pub(crate) fn reap_panes(panes: &crate::panes::Panes) {
    for pane in panes.table.values() {
        if let Some(pid) = pane.live_pid() {
            crate::platform::terminate_pid(pid);
        }
    }
}
