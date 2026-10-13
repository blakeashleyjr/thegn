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

/// State roots inherited by this test process before any test mutates its
/// environment. These roots stay protected even when a test changes `HOME` or
/// `XDG_STATE_HOME` to point somewhere else.
#[cfg(any(test, feature = "test-utils"))]
pub fn ambient_state_roots() -> &'static [std::path::PathBuf] {
    static ROOTS: std::sync::OnceLock<Vec<std::path::PathBuf>> = std::sync::OnceLock::new();
    ROOTS.get_or_init(|| {
        ambient_roots_from(
            std::env::var_os("HOME").as_deref(),
            std::env::var_os("XDG_STATE_HOME").as_deref(),
            std::env::var_os("NEXTEST").is_some(),
            &std::env::temp_dir(),
        )
    })
}

#[cfg(any(test, feature = "test-utils"))]
fn ambient_roots_from(
    home: Option<&std::ffi::OsStr>,
    state_home: Option<&std::ffi::OsStr>,
    nextest: bool,
    temp: &std::path::Path,
) -> Vec<std::path::PathBuf> {
    let mut roots = Vec::new();
    if let Some(home) = home.filter(|value| !value.is_empty()) {
        roots.push(std::path::PathBuf::from(home).join(".local/state"));
    }

    if !nextest && let Some(state_home) = state_home.filter(|value| !value.is_empty()) {
        let state_home = std::path::PathBuf::from(state_home);
        if !state_home.starts_with(temp) {
            roots.push(state_home);
        }
    }
    roots
}

/// Whether a state write would target the current HOME-derived root or one of
/// the roots inherited before test environment mutation began.
#[cfg(any(test, feature = "test-utils"))]
pub fn is_real_state_path(
    path: &std::path::Path,
    dynamic_home: &std::path::Path,
    ambient: &[std::path::PathBuf],
) -> bool {
    path.starts_with(dynamic_home.join(".local/state"))
        || ambient.iter().any(|root| path.starts_with(root))
}

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
        let _ambient_roots = ambient_state_roots();
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

#[cfg(test)]
mod tests {
    use super::{ambient_roots_from, is_real_state_path};
    use std::ffi::OsStr;
    use std::path::{Path, PathBuf};

    #[test]
    fn ambient_roots_include_home_state_only() {
        assert_eq!(
            ambient_roots_from(
                Some(OsStr::new("/home/user")),
                None,
                false,
                Path::new("/tmp")
            ),
            [PathBuf::from("/home/user/.local/state")]
        );
    }

    #[test]
    fn ambient_roots_include_inherited_non_temp_xdg_state() {
        assert_eq!(
            ambient_roots_from(
                None,
                Some(OsStr::new("/var/state")),
                false,
                Path::new("/tmp")
            ),
            [PathBuf::from("/var/state")]
        );
    }

    #[test]
    fn ambient_roots_exclude_xdg_state_under_temp() {
        assert!(
            ambient_roots_from(
                None,
                Some(OsStr::new("/tmp/test-state")),
                false,
                Path::new("/tmp")
            )
            .is_empty()
        );
    }

    #[test]
    fn ambient_roots_exclude_xdg_state_under_nextest() {
        assert!(
            ambient_roots_from(
                None,
                Some(OsStr::new("/var/state")),
                true,
                Path::new("/tmp")
            )
            .is_empty()
        );
    }

    #[test]
    fn ambient_xdg_state_path_is_real_even_with_another_home() {
        let path = Path::new("/x/state/thegn/thegn.db");
        let ambient = vec![PathBuf::from("/x/state")];

        assert!(is_real_state_path(path, Path::new("/h"), &ambient));
    }

    #[test]
    fn inherited_state_path_stays_real_after_home_override() {
        let temp = tempfile::tempdir().unwrap();
        crate::fsperm::restrict_dir_to_owner(temp.path()).unwrap();
        let home = temp.path().join("test-home");
        std::fs::create_dir(&home).unwrap();
        let inherited_state = temp.path().join("inherited-state");
        let path = inherited_state.join("thegn/thegn.db");
        let ambient = vec![inherited_state];

        assert!(is_real_state_path(&path, &home, &ambient));
    }

    #[test]
    fn sibling_path_under_temp_root_is_not_real_state() {
        let temp = tempfile::tempdir().unwrap();
        crate::fsperm::restrict_dir_to_owner(temp.path()).unwrap();
        let home = temp.path().join("home");
        let sibling = temp.path().join("sibling/thegn.db");
        let ambient = vec![temp.path().join("state")];

        assert!(!is_real_state_path(&sibling, &home, &ambient));
    }

    #[test]
    fn empty_ambient_roots_allow_paths_outside_dynamic_home_state() {
        let temp = tempfile::tempdir().unwrap();
        crate::fsperm::restrict_dir_to_owner(temp.path()).unwrap();
        let path = temp.path().join("other/thegn.db");

        assert!(!is_real_state_path(&path, Path::new("/h"), &[]));
    }
}
