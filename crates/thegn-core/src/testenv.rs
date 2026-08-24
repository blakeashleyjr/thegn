//! Shared test-only synchronization for tests that read or mutate process-global
//! environment state.
//!
//! Some tests read the process environment (`std::env::var`) inside the code
//! under test — e.g. [`crate::sandbox::enter_argv`] omits an env pair from the
//! world-readable `--setenv` argv when its value matches the launcher's own env.
//! Such a test is fragile if it assumes a *clean* ambient environment: running
//! `cargo test` inside a live thegn bwrap sandbox leaks `THEGN_SANDBOX=1`
//! into the runner and flips the outcome. The fix is to control the ambient var
//! explicitly. The process environment is global, so tests that mutate it must
//! serialize on a single crate-wide lock — a per-module `static` would be two
//! mutexes over one resource and would not serialize across modules.
//!
//! [`EnvGuard`] takes the lock, sets or unsets one or more env vars, and
//! **restores their prior values on drop** — even on an early return or panic.
//! A test that mutates an env var but forgets to restore it leaks process-global
//! state into every test that runs afterward; the guard makes that impossible.

#[cfg(any(test, feature = "test-utils"))]
pub(crate) static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// RAII guard that serializes on [`ENV_LOCK`], mutates one or more process env
/// vars, and restores their previous values (or unsets them) when dropped.
///
/// ```ignore
/// let _env = crate::testenv::EnvGuard::unset(&["THEGN_SANDBOX"]);
/// // ... `THEGN_SANDBOX` is guaranteed absent and exclusive until `_env` drops ...
/// ```
#[cfg(any(test, feature = "test-utils"))]
pub struct EnvGuard {
    _lock: std::sync::MutexGuard<'static, ()>,
    restore: Vec<(String, Option<std::ffi::OsString>)>,
}

#[cfg(any(test, feature = "test-utils"))]
impl EnvGuard {
    /// Set each `(key, value)`, snapshotting prior values for restore on drop.
    pub fn set(vars: &[(&str, &str)]) -> Self {
        Self::mutate(vars.iter().map(|(k, v)| ((*k).to_string(), Some(*v))))
    }

    /// Remove each `key` from the environment, snapshotting prior values for
    /// restore on drop.
    pub fn unset(keys: &[&str]) -> Self {
        Self::mutate(keys.iter().map(|k| ((*k).to_string(), None)))
    }

    /// Mixed set/unset in one guard (`Some` sets, `None` unsets). Use this
    /// instead of stacking two guards — [`ENV_LOCK`] is not reentrant.
    pub fn mutate_pairs(vars: &[(&str, Option<&str>)]) -> Self {
        Self::mutate(vars.iter().map(|(k, v)| ((*k).to_string(), *v)))
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

#[cfg(any(test, feature = "test-utils"))]
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

/// Rewrite a `/`-separated path fragment into the platform's spelling.
///
/// Assertions like `assert!(p.ends_with("/.gnupg"))` encode *structure*, but
/// they spell it the unix way. `Path::join` yields `\` on Windows, so those
/// assertions fail there for a reason that has nothing to do with what the test
/// is checking. Wrapping the expected fragment keeps the assertion honest on
/// both platforms without weakening it.
///
/// Use this only for paths the code under test built with `Path`/`PathBuf`.
/// A path that is deliberately unix-shaped on every platform — a *container*
/// mount target, a remote Linux path, a git-relative path — must keep its
/// literal `/` and must NOT be wrapped.
pub fn native_sep(rel: &str) -> String {
    if cfg!(windows) {
        rel.replace('/', "\\")
    } else {
        rel.to_string()
    }
}

/// Normalize a produced path to `/` separators, for comparing against a
/// unix-spelled literal.
///
/// The counterpart to [`native_sep`], for the case it cannot handle: a path
/// built by joining a *literal that already contains `/`* onto a `PathBuf`
/// comes out mixed on Windows (`...\tools\pi\node_modules/.bin\pi`), so
/// neither an all-`/` nor an all-`\` expectation matches. Windows accepts both
/// separators, so the mixing is harmless — normalize the actual value and
/// assert on structure.
pub fn norm_sep(path: &str) -> String {
    path.replace('\\', "/")
}

/// The environment variable each XDG-ish root actually reads on this platform.
///
/// `util::{home, xdg_state_home, xdg_config_home}` consult different variables
/// per OS, so a test that drives them with the unix names on Windows leaves the
/// developer's REAL roots in play — the fixture matches nothing and the
/// assertion fails for a reason that has nothing to do with the code.
///
/// They live here, in the test-support module, rather than as `#[cfg(windows)]`
/// constants inside each test: `thegn-core` is substrate-agnostic and its
/// platform ratchet says so. One table beats a per-file pair of cfgs.
pub const HOME_VAR: &str = if cfg!(windows) { "USERPROFILE" } else { "HOME" };

/// See [`HOME_VAR`].
pub const STATE_VAR: &str = if cfg!(windows) {
    "LOCALAPPDATA"
} else {
    "XDG_STATE_HOME"
};

/// See [`HOME_VAR`].
pub const CONFIG_VAR: &str = if cfg!(windows) {
    "APPDATA"
} else {
    "XDG_CONFIG_HOME"
};
/// How long a test may wait for a subprocess it expects to finish immediately.
///
/// Headroom, never the thing under test — a test that asserts a deadline *fires*
/// must keep its own short timeout. This is for the opposite case: a fixture
/// that runs `printf` and should obviously succeed.
///
/// "Immediately" is relative. On Windows these spawn MSYS binaries through fork
/// emulation with a security agent inspecting every process creation, and the
/// scripted probes spawn a dozen of them in a row. Measured here: the same
/// tests pass in 2–4s when run alone and blow a fixed 5s budget under the full
/// suite, failing on the fixture rather than on anything they assert. Growing
/// the budget cannot mask a real regression — the assertions still have to hold
/// — it only stops a saturated machine from reading as a broken one.
pub const SPAWN_BUDGET: std::time::Duration = if cfg!(windows) {
    std::time::Duration::from_secs(120)
} else {
    std::time::Duration::from_secs(10)
};
#[cfg(test)]
mod native_sep_tests {
    use super::native_sep;

    #[test]
    fn rewrites_only_on_windows() {
        let got = native_sep("/.gnupg");
        if cfg!(windows) {
            assert_eq!(got, r"\.gnupg");
        } else {
            assert_eq!(got, "/.gnupg");
        }
    }
}
