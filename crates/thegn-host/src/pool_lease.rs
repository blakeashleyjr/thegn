//! Process-shared advisory leases for warm-pool reconciliation and provisioning.

use sha2::{Digest, Sha256};
use std::fmt;
use std::fs::File;
use std::path::{Path, PathBuf};

#[derive(Debug)]
pub(crate) enum LeaseError {
    Busy,
    Io(std::io::Error),
}

impl fmt::Display for LeaseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Busy => f.write_str("pool lease is busy"),
            Self::Io(error) => write!(f, "cannot establish pool lease: {error}"),
        }
    }
}

impl std::error::Error for LeaseError {}

/// Holds an advisory file lock until dropped. Pool lock files are never removed:
/// unlinking one could let another process lock a different inode at the same path.
pub(crate) struct Lease {
    _file: File,
}

fn lock_dir() -> PathBuf {
    #[cfg(test)]
    {
        thread_local! {
            static DIR: tempfile::TempDir = tempfile::tempdir().expect("test pool lock dir");
        }
        DIR.with(|dir| dir.path().join("locks").join("pool"))
    }
    #[cfg(not(test))]
    {
        thegn_core::util::xdg_state_home()
            .join("thegn")
            .join("locks")
            .join("pool")
    }
}

fn ensure_dir(dir: &Path) -> std::io::Result<()> {
    if dir.is_dir() {
        return Ok(());
    }
    std::fs::create_dir_all(dir)?;
    crate::platform::restrict_dir_owner_only(dir);
    Ok(())
}

fn lock_path_in(dir: &Path, key: &str) -> PathBuf {
    let digest = Sha256::digest(key.as_bytes());
    let hex: String = digest
        .iter()
        .take(16)
        .map(|byte| format!("{byte:02x}"))
        .collect();
    dir.join(hex)
}

pub(crate) fn pool_lease(repo: &str, env: &str) -> Result<Lease, LeaseError> {
    pool_lease_in(&lock_dir(), repo, env)
}

fn pool_lease_in(dir: &Path, repo: &str, env: &str) -> Result<Lease, LeaseError> {
    lease_in(dir, &format!("pool\0{repo}\0{env}"))
}

pub(crate) fn spare_lease(name: &str) -> Result<Lease, LeaseError> {
    spare_lease_in(&lock_dir(), name)
}

fn spare_lease_in(dir: &Path, name: &str) -> Result<Lease, LeaseError> {
    lease_in(dir, &format!("spare\0{name}"))
}

fn lease_in(dir: &Path, key: &str) -> Result<Lease, LeaseError> {
    ensure_dir(dir).map_err(LeaseError::Io)?;
    let file = crate::platform::open_lock_file(&lock_path_in(dir, key)).map_err(LeaseError::Io)?;
    match file.try_lock() {
        Ok(()) => Ok(Lease { _file: file }),
        Err(std::fs::TryLockError::WouldBlock) => Err(LeaseError::Busy),
        Err(std::fs::TryLockError::Error(error)) => Err(LeaseError::Io(error)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pool_lease_second_holder_is_busy() {
        let dir = tempfile::tempdir().unwrap();
        let first = pool_lease_in(dir.path(), "r", "e").unwrap();
        assert!(matches!(
            pool_lease_in(dir.path(), "r", "e"),
            Err(LeaseError::Busy)
        ));
        drop(first);
        assert!(pool_lease_in(dir.path(), "r", "e").is_ok());
    }

    #[test]
    fn pool_lease_keys_are_distinct() {
        let dir = tempfile::tempdir().unwrap();
        let _pool = pool_lease_in(dir.path(), "r", "e").unwrap();
        assert!(pool_lease_in(dir.path(), "r", "other").is_ok());
        assert!(pool_lease_in(dir.path(), "other", "e").is_ok());
        assert!(spare_lease_in(dir.path(), "r").is_ok());
    }

    #[test]
    fn pool_lease_lock_dir_error_is_typed() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("file");
        std::fs::write(&path, b"not a directory").unwrap();
        assert!(matches!(
            pool_lease_in(&path, "r", "e"),
            Err(LeaseError::Io(_))
        ));
    }
}
