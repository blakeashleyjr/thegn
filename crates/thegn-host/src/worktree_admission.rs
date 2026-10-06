//! Cross-process admission between pane opens and worktree destroys (THE-728).
//!
//! One advisory lock file per canonical worktree path under
//! `$XDG_STATE_HOME/thegn/locks/`. A destroy takes it exclusive (non-blocking)
//! for the whole physical removal; a pane open takes it shared (non-blocking)
//! for the open itself. Neither side ever blocks: a destroy that cannot get the
//! lock is refused (fail closed, nothing is deleted), and a pane open that
//! finds a destroy holding it is refused with "worktree is being removed".
//!
//! Scope: this closes the window only while the destroy holds the lock. It does
//! not make a removal durable for stale layout snapshots written afterwards by
//! another process (that needs a schema change; tracked separately).

use sha2::{Digest, Sha256};
use std::fs::File;
use std::path::{Path, PathBuf};

/// How many ancestors of a pane cwd are probed for a destroy lock. A pane cwd
/// is the worktree root or a shallow subdirectory of it.
const MAX_ANCESTORS: usize = 8;

pub(crate) const REMOVING_MESSAGE: &str = "worktree is being removed";

fn lock_dir() -> PathBuf {
    #[cfg(test)]
    let base = std::env::temp_dir();
    #[cfg(not(test))]
    let base = thegn_core::util::xdg_state_home();
    base.join("thegn").join("locks")
}

fn lock_path_in(dir: &Path, key: &Path) -> PathBuf {
    let digest = Sha256::digest(key.to_string_lossy().as_bytes());
    let hex: String = digest.iter().take(16).map(|b| format!("{b:02x}")).collect();
    dir.join(hex)
}

/// Exclusive hold on one worktree path for the duration of its removal.
pub(crate) struct DestroyLock {
    file: Option<File>,
    path: PathBuf,
}

impl DestroyLock {
    /// Try to take the destroy lock for `key` (a canonical worktree key).
    /// `Err` means refuse the destroy: either a pane open holds the lock or
    /// the lock could not be established (fail closed).
    pub(crate) fn try_acquire(key: &Path) -> Result<Self, String> {
        Self::try_acquire_in(&lock_dir(), key)
    }

    fn try_acquire_in(dir: &Path, key: &Path) -> Result<Self, String> {
        std::fs::create_dir_all(dir)
            .map_err(|e| format!("cannot establish cleanup admission lock: {e}"))?;
        let path = lock_path_in(dir, key);
        let file = crate::platform::open_lock_file(&path)
            .map_err(|e| format!("cannot establish cleanup admission lock: {e}"))?;
        match file.try_lock() {
            Ok(()) => Ok(Self {
                file: Some(file),
                path,
            }),
            Err(std::fs::TryLockError::WouldBlock) => {
                Err("a pane is opening in this worktree; cleanup deferred".into())
            }
            Err(std::fs::TryLockError::Error(e)) => {
                Err(format!("cannot establish cleanup admission lock: {e}"))
            }
        }
    }

    /// The worktree is gone: drop the (now meaningless) lock file too.
    pub(crate) fn forget_file(&mut self) {
        // best-effort: a leftover empty lock file is harmless.
        let _ = std::fs::remove_file(&self.path);
    }
}

impl Drop for DestroyLock {
    fn drop(&mut self) {
        // Closing the fd releases the flock; dropping explicitly keeps intent clear.
        self.file.take();
    }
}

/// Shared hold taken while a pane is opened.
pub(crate) struct PaneAdmission {
    _file: Option<File>,
}

/// Admit a pane open in `cwd`. Only a cwd that is, or sits under, a worktree
/// with a destroy lock file is affected; everything else is admitted with no
/// lock taken. Non-blocking: safe on the event loop (a handful of stats).
pub(crate) fn admit_pane_open(cwd: Option<&Path>) -> Result<PaneAdmission, String> {
    admit_pane_open_in(&lock_dir(), cwd)
}

fn admit_pane_open_in(dir: &Path, cwd: Option<&Path>) -> Result<PaneAdmission, String> {
    let free = Ok(PaneAdmission { _file: None });
    let Some(cwd) = cwd else {
        return free;
    };
    // In-process claim (no lock file needed).
    if crate::worktree_lifecycle::destroy_in_progress_for(cwd) {
        return Err(REMOVING_MESSAGE.into());
    }
    if !dir.is_dir() {
        return free;
    }
    let canonical = crate::worktree_lifecycle::destroy_key_pub(cwd);
    for ancestor in canonical.ancestors().take(MAX_ANCESTORS) {
        let path = lock_path_in(dir, ancestor);
        if !path.exists() {
            continue;
        }
        let Ok(file) = crate::platform::open_lock_file(&path) else {
            // A lock file that exists but cannot be opened: never block the open.
            continue;
        };
        match file.try_lock_shared() {
            Ok(()) => return Ok(PaneAdmission { _file: Some(file) }),
            Err(std::fs::TryLockError::WouldBlock) => return Err(REMOVING_MESSAGE.into()),
            Err(std::fs::TryLockError::Error(_)) => continue,
        }
    }
    free
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn destroy_lock_refuses_pane_open_at_and_under_the_worktree() {
        let state = tempfile::tempdir().unwrap();
        let wt = tempfile::tempdir().unwrap();
        let sub = wt.path().join("src");
        std::fs::create_dir(&sub).unwrap();
        let key = crate::worktree_lifecycle::destroy_key_pub(wt.path());
        let lock = DestroyLock::try_acquire_in(state.path(), &key).unwrap();
        for cwd in [wt.path(), sub.as_path()] {
            let err = admit_pane_open_in(state.path(), Some(cwd)).err().unwrap();
            assert_eq!(err, REMOVING_MESSAGE);
        }
        drop(lock);
        assert!(admit_pane_open_in(state.path(), Some(wt.path())).is_ok());
    }

    #[test]
    fn cwd_outside_a_locked_worktree_and_no_cwd_are_exempt() {
        let state = tempfile::tempdir().unwrap();
        let wt = tempfile::tempdir().unwrap();
        let other = tempfile::tempdir().unwrap();
        let key = crate::worktree_lifecycle::destroy_key_pub(wt.path());
        let _lock = DestroyLock::try_acquire_in(state.path(), &key).unwrap();
        assert!(admit_pane_open_in(state.path(), Some(other.path())).is_ok());
        assert!(admit_pane_open_in(state.path(), None).is_ok());
    }

    #[test]
    fn destroy_is_refused_while_a_pane_open_holds_the_shared_lock() {
        let state = tempfile::tempdir().unwrap();
        let wt = tempfile::tempdir().unwrap();
        let key = crate::worktree_lifecycle::destroy_key_pub(wt.path());
        // A destroy that has come and gone leaves/creates the lock file.
        drop(DestroyLock::try_acquire_in(state.path(), &key).unwrap());
        let held = admit_pane_open_in(state.path(), Some(wt.path())).unwrap();
        assert!(DestroyLock::try_acquire_in(state.path(), &key).is_err());
        drop(held);
        assert!(DestroyLock::try_acquire_in(state.path(), &key).is_ok());
    }

    #[test]
    fn second_destroyer_is_refused_and_forget_removes_the_file() {
        let state = tempfile::tempdir().unwrap();
        let key = PathBuf::from("/nonexistent/worktree");
        let mut first = DestroyLock::try_acquire_in(state.path(), &key).unwrap();
        assert!(DestroyLock::try_acquire_in(state.path(), &key).is_err());
        let path = lock_path_in(state.path(), &key);
        assert!(path.exists());
        first.forget_file();
        assert!(!path.exists());
    }
}
