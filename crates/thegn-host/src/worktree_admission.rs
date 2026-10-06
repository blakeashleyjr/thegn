//! Cross-process admission between pane opens and worktree destroys (THE-728).
//!
//! One advisory lock file per worktree root under
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

pub(crate) const REMOVING_MESSAGE: &str = "worktree is being removed";

fn lock_dir() -> PathBuf {
    #[cfg(test)]
    {
        // Per-thread (hence per-test) directory; never the shared /tmp.
        thread_local! {
            static DIR: tempfile::TempDir = tempfile::tempdir().expect("test lock dir");
        }
        DIR.with(|d| d.path().join("locks"))
    }
    #[cfg(not(test))]
    {
        thegn_core::util::xdg_state_home()
            .join("thegn")
            .join("locks")
    }
}

/// Create the lock directory owner-only (idempotent, cheap when it exists).
fn ensure_dir(dir: &Path) -> std::io::Result<()> {
    if dir.is_dir() {
        return Ok(());
    }
    std::fs::create_dir_all(dir)?;
    crate::platform::restrict_dir_owner_only(dir);
    Ok(())
}

fn lock_path_in(dir: &Path, key: &Path) -> PathBuf {
    let digest = Sha256::digest(key.to_string_lossy().as_bytes());
    let hex: String = digest.iter().take(16).map(|b| format!("{b:02x}")).collect();
    dir.join(hex)
}

/// Exclusive hold on a worktree (its lexical and canonical spellings) for the
/// duration of its removal. Dropping it releases every lock; lock files are
/// deliberately never unlinked (that would race flock against a re-open).
pub(crate) struct DestroyLock {
    _files: Vec<File>,
}

impl DestroyLock {
    /// Try to take the destroy lock for each spelling in `keys`. `Err` means
    /// refuse the destroy: a pane open holds a lock, or a lock could not be
    /// established (fail closed). Nothing is deleted before this succeeds.
    pub(crate) fn try_acquire(keys: &[&Path]) -> Result<Self, String> {
        Self::try_acquire_in(&lock_dir(), keys)
    }

    fn try_acquire_in(dir: &Path, keys: &[&Path]) -> Result<Self, String> {
        let fail = |e: std::io::Error| format!("cannot establish cleanup admission lock: {e}");
        ensure_dir(dir).map_err(fail)?;
        let mut files = Vec::new();
        let mut seen: Vec<PathBuf> = Vec::new();
        for key in keys {
            let path = lock_path_in(dir, key);
            if seen.contains(&path) {
                continue;
            }
            let file = crate::platform::open_lock_file(&path).map_err(fail)?;
            match file.try_lock() {
                Ok(()) => {}
                Err(std::fs::TryLockError::WouldBlock) => {
                    return Err("a pane is opening in this worktree; cleanup deferred".into());
                }
                Err(std::fs::TryLockError::Error(e)) => return Err(fail(e)),
            }
            seen.push(path);
            files.push(file);
        }
        Ok(Self { _files: files })
    }
}

/// Shared hold taken while a pane is opened.
pub(crate) struct PaneAdmission {
    _file: Option<File>,
}

/// Admit a pane open in the worktree rooted at `root` (the caller's lexical
/// worktree path; never canonicalized here, so safe on the event loop). The
/// lock file is opened-or-created and locked shared, non-blocking, and held
/// by the returned guard for the duration of the open. A destroy holding it
/// exclusively refuses the open. `None` (no cwd) is exempt.
pub(crate) fn admit_pane_open(root: Option<&Path>) -> Result<PaneAdmission, String> {
    admit_pane_open_in(&lock_dir(), root)
}

fn admit_pane_open_in(dir: &Path, root: Option<&Path>) -> Result<PaneAdmission, String> {
    let Some(root) = root else {
        return Ok(PaneAdmission { _file: None });
    };
    // In-process claim (no filesystem work unless a destroy is running).
    if crate::worktree_lifecycle::destroy_in_progress_for(root) {
        return Err(REMOVING_MESSAGE.into());
    }
    // Lock infrastructure trouble never blocks a pane open: the destroy side
    // fails closed on the same trouble, so it cannot be racing us.
    if ensure_dir(dir).is_err() {
        return Ok(PaneAdmission { _file: None });
    }
    let Ok(file) = crate::platform::open_lock_file(&lock_path_in(dir, root)) else {
        return Ok(PaneAdmission { _file: None });
    };
    match file.try_lock_shared() {
        Ok(()) => Ok(PaneAdmission { _file: Some(file) }),
        Err(std::fs::TryLockError::WouldBlock) => Err(REMOVING_MESSAGE.into()),
        Err(std::fs::TryLockError::Error(_)) => Ok(PaneAdmission { _file: None }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn destroy_lock_refuses_pane_open_with_no_prior_lock_file() {
        let state = tempfile::tempdir().unwrap();
        let wt = tempfile::tempdir().unwrap();
        let lock = DestroyLock::try_acquire_in(state.path(), &[wt.path()]).unwrap();
        let err = admit_pane_open_in(state.path(), Some(wt.path()))
            .err()
            .unwrap();
        assert_eq!(err, REMOVING_MESSAGE);
        drop(lock);
        assert!(admit_pane_open_in(state.path(), Some(wt.path())).is_ok());
    }

    #[test]
    fn pane_open_creates_the_lock_so_a_concurrent_destroy_is_refused() {
        // No destroy has ever run: the pane side must still create the file.
        let state = tempfile::tempdir().unwrap();
        let wt = tempfile::tempdir().unwrap();
        let held = admit_pane_open_in(state.path(), Some(wt.path())).unwrap();
        assert!(DestroyLock::try_acquire_in(state.path(), &[wt.path()]).is_err());
        drop(held);
        assert!(DestroyLock::try_acquire_in(state.path(), &[wt.path()]).is_ok());
    }

    #[test]
    fn other_roots_and_no_root_are_exempt() {
        let state = tempfile::tempdir().unwrap();
        let wt = tempfile::tempdir().unwrap();
        let other = tempfile::tempdir().unwrap();
        let _lock = DestroyLock::try_acquire_in(state.path(), &[wt.path()]).unwrap();
        assert!(admit_pane_open_in(state.path(), Some(other.path())).is_ok());
        assert!(admit_pane_open_in(state.path(), None).is_ok());
    }

    #[test]
    fn second_destroyer_is_refused_and_lock_file_persists() {
        let state = tempfile::tempdir().unwrap();
        let key = PathBuf::from("/nonexistent/worktree");
        let first = DestroyLock::try_acquire_in(state.path(), &[&key]).unwrap();
        assert!(DestroyLock::try_acquire_in(state.path(), &[&key]).is_err());
        drop(first);
        assert!(lock_path_in(state.path(), &key).exists());
        assert!(DestroyLock::try_acquire_in(state.path(), &[&key]).is_ok());
    }
}
