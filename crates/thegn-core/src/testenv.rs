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

use crate::sandbox_backend::{HostOs, host_os};
use std::path::{Path, PathBuf};

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

/// A temporary directory reached through a symlink, for path tests that need
/// the spelling supplied by a caller to differ from the kernel's resolved path.
#[cfg(any(test, feature = "test-utils"))]
pub struct SymlinkedTempDir {
    _temp: tempfile::TempDir,
    link: PathBuf,
    real: PathBuf,
}

#[cfg(any(test, feature = "test-utils"))]
impl SymlinkedTempDir {
    pub fn path(&self) -> &Path {
        &self.link
    }

    pub fn real_path(&self) -> &Path {
        &self.real
    }
}

#[cfg(any(test, feature = "test-utils"))]
pub fn symlinked_tempdir() -> SymlinkedTempDir {
    let temp = tempfile::tempdir().expect("temporary symlink fixture");
    let real = temp.path().join("real");
    std::fs::create_dir(&real).expect("create real fixture directory");
    let link = if cfg!(unix) {
        let link = temp.path().join("link");
        symlink_directory(&real, &link).expect("create symlink fixture");
        Some(link)
    } else {
        None
    };
    let real = std::fs::canonicalize(real).expect("canonicalize fixture directory");
    let link = link.unwrap_or_else(|| real.clone());
    SymlinkedTempDir {
        _temp: temp,
        link,
        real,
    }
}

#[cfg(all(unix, any(test, feature = "test-utils")))]
fn symlink_directory(real: &Path, link: &Path) -> std::io::Result<()> {
    std::os::unix::fs::symlink(real, link)
}

#[cfg(all(not(unix), any(test, feature = "test-utils")))]
fn symlink_directory(_real: &Path, _link: &Path) -> std::io::Result<()> {
    // std has no portable unprivileged directory-symlink API.
    Ok(())
}

/// Environment pairs that the given OS's home/config/state resolvers read.
#[cfg(any(test, feature = "test-utils"))]
pub fn state_env_for(os: HostOs, root: &Path) -> Vec<(&'static str, PathBuf)> {
    match os {
        HostOs::Windows => vec![
            ("USERPROFILE", root.to_path_buf()),
            ("APPDATA", root.join("AppData").join("Roaming")),
            ("LOCALAPPDATA", root.join("AppData").join("Local")),
        ],
        // XDG_STATE_HOME deliberately lives OUTSIDE `$HOME/.local/state`: the THE-744
        // guard treats `$HOME/.local/state` as the real state root, so an isolated
        // root there would be refused.
        _ => vec![
            ("HOME", root.to_path_buf()),
            ("XDG_CONFIG_HOME", root.join(".config")),
            ("XDG_STATE_HOME", root.join("state")),
        ],
    }
}

/// Union of environment pairs needed to isolate either supported resolver arm.
#[cfg(any(test, feature = "test-utils"))]
pub fn state_env_all(root: &Path) -> Vec<(&'static str, PathBuf)> {
    vec![
        ("HOME", root.to_path_buf()),
        ("XDG_CONFIG_HOME", root.join(".config")),
        ("XDG_STATE_HOME", root.join("state")),
        ("USERPROFILE", root.to_path_buf()),
        ("APPDATA", root.join("AppData").join("Roaming")),
        ("LOCALAPPDATA", root.join("AppData").join("Local")),
    ]
}

/// Private filesystem roots and environment for a test that writes state.
#[cfg(any(test, feature = "test-utils"))]
pub struct IsolatedState {
    _env: EnvGuard,
    temp: tempfile::TempDir,
}

#[cfg(any(test, feature = "test-utils"))]
impl IsolatedState {
    pub fn new() -> Self {
        let temp = tempfile::tempdir().expect("private isolated state dir");
        crate::fsperm::restrict_dir_to_owner(temp.path()).expect("restrict isolated state dir");
        let mut vars = state_env_all(temp.path());
        vars.push(("THEGN_DIR", temp.path().join(".thegn")));
        let strings: Vec<_> = vars
            .iter()
            .map(|(key, value)| (*key, value.to_str().expect("state path is UTF-8")))
            .collect();
        let env = EnvGuard::set(&strings);
        Self { _env: env, temp }
    }

    pub fn root(&self) -> &Path {
        self.temp.path()
    }

    pub fn state_home(&self) -> PathBuf {
        let state = crate::util::xdg_state_home();
        let expected = match host_os() {
            HostOs::Windows => self.root().join("AppData").join("Local"),
            _ => self.root().join("state"),
        };
        debug_assert_eq!(state, expected);
        state
    }

    pub fn config_home(&self) -> PathBuf {
        crate::util::xdg_config_home()
    }

    pub fn thegn_dir(&self) -> PathBuf {
        self.root().join(".thegn")
    }
}

#[cfg(any(test, feature = "test-utils"))]
impl Default for IsolatedState {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::{
        IsolatedState, ambient_roots_from, is_real_state_path, state_env_for, symlinked_tempdir,
    };
    use crate::sandbox_backend::HostOs;
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

    #[test]
    fn symlinked_tempdir_exposes_link_and_kernel_paths() {
        let temp = symlinked_tempdir();
        let file = temp.path().join("written-through-link");
        std::fs::write(&file, "fixture").unwrap();
        assert_eq!(
            std::fs::read(temp.real_path().join("written-through-link")).unwrap(),
            b"fixture"
        );
        if cfg!(unix) {
            assert_ne!(temp.path(), temp.real_path());
            assert_eq!(
                std::fs::canonicalize(temp.path()).unwrap(),
                temp.real_path()
            );
            let built = temp.path().join("written-through-link");
            let canonicalized = std::fs::canonicalize(&file).unwrap();
            assert!(!canonicalized.starts_with(temp.path()));
            assert!(!built.starts_with(temp.real_path()));
        } else {
            assert_eq!(temp.path(), temp.real_path());
            assert_eq!(
                std::fs::canonicalize(temp.path()).unwrap(),
                temp.real_path()
            );
        }
    }

    #[test]
    fn state_environment_isolates_every_os_resolver() {
        let root = Path::new("/isolated/state-root");
        for os in [HostOs::Linux, HostOs::MacOs, HostOs::Windows, HostOs::Other] {
            let values: Vec<_> = state_env_for(os, root)
                .into_iter()
                .map(|(key, value)| (key, value.into_os_string()))
                .collect();
            let lookup = |key: &str| {
                values
                    .iter()
                    .find(|(name, _)| *name == key)
                    .map(|(_, value)| value.clone())
            };
            assert!(crate::util::home_for(os, &lookup).starts_with(root));
            assert!(crate::util::xdg_config_home_for(os, &lookup).starts_with(root));
            assert!(crate::util::xdg_state_home_for(os, &lookup).starts_with(root));
        }
    }

    #[test]
    fn isolated_state_sets_all_state_roots() {
        let state = IsolatedState::new();
        assert!(state.state_home().starts_with(state.root()));
        assert!(state.config_home().starts_with(state.root()));
        assert!(state.thegn_dir().starts_with(state.root()));
    }
}
