//! Private custody of the merge queue's temp bundle: a per-run directory and
//! its exclusively-created leaf, owned until `git fetch` has consumed them.
//!
//! The custody guarantee is a syscall property, so it is per-OS and lives here
//! rather than at the call site (`merge_remote.rs`, which stays platform-free).
//! Unix goes through the descriptor-relative gate seam ([`super::gate_path`]);
//! Windows keeps the fetch path openable with `CREATE_NEW`, delete-sharing and
//! a final reparse-point refusal, and compares handle identities instead of
//! inode identities. Every other platform refuses rather than degrading to an
//! unprivate temp file.
//!
//! Removal is never recursive and never adopts a replacement: cleanup only
//! unlinks identities that still match the retained handles, so an observed
//! swap is preserved for inspection instead of being deleted.

use std::io;
use std::path::{Path, PathBuf};

/// Own a private directory and its exclusively-created bundle leaf until the
/// fetch has consumed it. Drop only removes identities that still match the
/// retained handles; a replacement is preserved for inspection.
pub(crate) struct BundleTemp {
    parent_path: PathBuf,
    path: PathBuf,
    #[cfg(test)]
    fail_write: bool,
    #[cfg(test)]
    fail_flush: bool,
    #[cfg(unix)]
    parent: Option<super::gate_path::Directory>,
    #[cfg(unix)]
    file: Option<super::gate_path::Regular>,
    #[cfg(windows)]
    parent: Option<std::fs::File>,
    #[cfg(windows)]
    file: Option<std::fs::File>,
}

impl BundleTemp {
    /// Take custody under the process temp directory.
    pub(crate) fn new() -> io::Result<Self> {
        Self::new_in(&std::env::temp_dir())
    }

    /// Take custody under `root` (the caller supplies the root only in tests
    /// that need to assert on what the run left behind).
    pub(crate) fn new_in(root: &Path) -> io::Result<Self> {
        Self::create(root, false)
    }

    #[cfg(all(test, unix))]
    fn new_for_test(root: &Path, precreate_leaf: bool) -> io::Result<Self> {
        Self::create(root, precreate_leaf)
    }

    fn create(root: &Path, precreate_leaf: bool) -> io::Result<Self> {
        #[cfg(not(test))]
        let _ = precreate_leaf;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut builder = tempfile::Builder::new();
            builder
                .prefix("thegn-mq-")
                .permissions(std::fs::Permissions::from_mode(0o700))
                .disable_cleanup(true);
            let temp = builder.tempdir_in(std::fs::canonicalize(root)?)?;
            let temp_path = temp.path().to_owned();
            #[cfg(test)]
            if precreate_leaf
                && let Err(error) = std::fs::write(temp_path.join("bundle"), b"foreign fixture")
            {
                return Err(match std::fs::remove_dir(&temp_path) {
                    Ok(()) => error,
                    Err(cleanup) => io::Error::other(format!(
                        "seed hostile bundle leaf failed: {error}; nonrecursive cleanup failed: {cleanup}"
                    )),
                });
            }
            let parent = match super::gate_path::Directory::open(&temp_path, Some(&temp_path)) {
                Ok(parent) => parent,
                Err(error) => {
                    return Err(match std::fs::remove_dir(&temp_path) {
                        Ok(()) => error,
                        Err(cleanup) => io::Error::other(format!(
                            "open private bundle parent failed: {error}; nonrecursive cleanup failed: {cleanup}"
                        )),
                    });
                }
            };
            let parent_path = temp.keep();
            let path = parent_path.join("bundle");
            let mut custody = Self {
                parent_path,
                path,
                parent: Some(parent),
                file: None,
                #[cfg(test)]
                fail_write: false,
                #[cfg(test)]
                fail_flush: false,
            };
            custody.file = match super::gate_path::Regular::create_exclusive_at(
                custody.parent.as_ref().expect("bundle parent retained"),
                &custody.path,
            ) {
                Ok(file) => Some(file),
                Err(error) => {
                    return Err(match custody.cleanup() {
                        Ok(()) => error,
                        Err(cleanup) => io::Error::other(format!(
                            "create private bundle leaf failed: {error}; nonrecursive cleanup failed: {cleanup}"
                        )),
                    });
                }
            };
            Ok(custody)
        }
        #[cfg(windows)]
        {
            use std::os::windows::fs::OpenOptionsExt;
            use windows_sys::Win32::Storage::FileSystem::{
                FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE,
            };
            // CreateDirectoryW receives a SECURITY_ATTRIBUTES containing the
            // current process's user SID, so the private parent has its
            // owner-only DACL at creation. The follow-up ACL readback and
            // retained no-follow handle still guard the seam before fetch.
            let temp_path = super::create_private_directory(root)?;
            #[cfg(test)]
            if precreate_leaf
                && let Err(error) = std::fs::write(temp_path.join("bundle"), b"foreign fixture")
            {
                return Err(match std::fs::remove_dir(&temp_path) {
                    Ok(()) => error,
                    Err(cleanup) => io::Error::other(format!(
                        "seed hostile bundle leaf failed: {error}; nonrecursive cleanup failed: {cleanup}"
                    )),
                });
            }
            if let Err(error) = super::secure_private_directory(&temp_path) {
                return Err(match std::fs::remove_dir(&temp_path) {
                    Ok(()) => error,
                    Err(cleanup) => io::Error::other(format!(
                        "secure private bundle parent failed: {error}; nonrecursive cleanup failed: {cleanup}"
                    )),
                });
            }
            let parent = match super::open_directory_nofollow(&temp_path) {
                Ok(parent) => parent,
                Err(error) => {
                    return Err(match std::fs::remove_dir(&temp_path) {
                        Ok(()) => error,
                        Err(cleanup) => io::Error::other(format!(
                            "open private bundle parent failed: {error}; nonrecursive cleanup failed: {cleanup}"
                        )),
                    });
                }
            };
            let parent_path = temp_path;
            let path = parent_path.join("bundle");
            let mut custody = Self {
                parent_path,
                path,
                parent: Some(parent),
                file: None,
                #[cfg(test)]
                fail_write: false,
                #[cfg(test)]
                fail_flush: false,
            };
            custody.file = match std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .create_new(true)
                .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)
                .open(&custody.path)
            {
                Ok(file) => Some(file),
                Err(error) => {
                    return Err(match custody.cleanup() {
                        Ok(()) => error,
                        Err(cleanup) => io::Error::other(format!(
                            "create private bundle leaf failed: {error}; nonrecursive cleanup failed: {cleanup}"
                        )),
                    });
                }
            };
            Ok(custody)
        }
        #[cfg(not(any(unix, windows)))]
        {
            let _ = root;
            Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "private bundle custody is unsupported on this platform",
            ))
        }
    }

    /// The retained leaf's path — what `git fetch` is pointed at.
    pub(crate) fn path(&self) -> &Path {
        &self.path
    }

    /// Recheck that the retained parent and leaf are still the identities this
    /// custody created, immediately before handing the path to git.
    pub(crate) fn verify(&self) -> io::Result<()> {
        #[cfg(unix)]
        {
            self.parent
                .as_ref()
                .expect("bundle parent retained")
                .verify()?;
            self.file.as_ref().expect("bundle file retained").verify()
        }
        #[cfg(windows)]
        {
            self.verify_windows_parent()?;
            let current = super::open_read_nofollow(&self.path)?;
            if !windows_same_identity(self.file.as_ref().expect("bundle file retained"), &current) {
                return Err(io::Error::other("bundle file identity changed"));
            }
            Ok(())
        }
        #[cfg(not(any(unix, windows)))]
        {
            Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "private bundle custody is unsupported on this platform",
            ))
        }
    }

    /// Release custody, reporting whatever cleanup could not be completed.
    pub(crate) fn finish(mut self) -> io::Result<()> {
        self.cleanup()
    }

    /// Publish `bytes` into the retained leaf, revalidating the identity after
    /// the flush so a swapped-in target is never the thing git reads.
    pub(crate) fn write(&mut self, bytes: &[u8]) -> io::Result<()> {
        #[cfg(unix)]
        {
            #[cfg(test)]
            if self.fail_write {
                return Err(io::Error::other("injected bundle write failure"));
            }
            let file = self.file.as_mut().expect("bundle file retained");
            file.write_all(bytes)?;
            #[cfg(test)]
            if self.fail_flush {
                return Err(io::Error::other("injected bundle flush failure"));
            }
            file.flush()?;
            file.verify()
        }
        #[cfg(windows)]
        {
            use std::io::Write;
            #[cfg(test)]
            if self.fail_write {
                return Err(io::Error::other("injected bundle write failure"));
            }
            self.verify_windows_parent()?;
            let file = self.file.as_mut().expect("bundle file retained");
            file.write_all(bytes)?;
            #[cfg(test)]
            if self.fail_flush {
                return Err(io::Error::other("injected bundle flush failure"));
            }
            file.flush()?;
            self.verify()
        }
        #[cfg(not(any(unix, windows)))]
        {
            let _ = bytes;
            Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "private bundle custody is unsupported on this platform",
            ))
        }
    }

    #[cfg(unix)]
    fn cleanup(&mut self) -> io::Result<()> {
        let mut failure = None;
        if let Some(file) = self.file.take()
            && let Err(error) = file.remove_verified()
        {
            failure = Some(error);
        }
        if let Some(parent) = self.parent.take() {
            if parent.verify().is_ok() {
                if let Err(error) = std::fs::remove_dir(&self.parent_path) {
                    failure.get_or_insert(error);
                }
            } else if failure.is_none() {
                failure = Some(io::Error::other("bundle parent identity changed"));
            }
        }
        failure.map_or(Ok(()), Err)
    }

    #[cfg(windows)]
    fn cleanup(&mut self) -> io::Result<()> {
        let had_file = self.file.is_some();
        let same_identity = self.file.as_ref().and_then(|owned| {
            let current = super::open_read_nofollow(&self.path).ok()?;
            Some(windows_same_identity(owned, &current))
        }) == Some(true);
        let parent_identity = self.parent.as_ref().and_then(|owned| {
            let current = super::open_directory_nofollow(&self.parent_path).ok()?;
            Some(windows_same_identity(owned, &current))
        }) == Some(true);
        let path_missing = matches!(
            std::fs::symlink_metadata(&self.path),
            Err(error) if error.kind() == io::ErrorKind::NotFound
        );
        let mut failure = None;
        if same_identity && parent_identity {
            if let Err(error) = std::fs::remove_file(&self.path) {
                failure = Some(error);
            }
        } else if had_file {
            failure = Some(io::Error::other("bundle identity changed before cleanup"));
        }
        drop(self.file.take());
        drop(self.parent.take());
        if same_identity && parent_identity {
            if let Err(error) = std::fs::remove_dir(&self.parent_path) {
                failure.get_or_insert(error);
            }
        } else if !had_file && parent_identity && path_missing {
            if let Err(error) = std::fs::remove_dir(&self.parent_path) {
                failure.get_or_insert(error);
            }
        }
        failure.map_or(Ok(()), Err)
    }

    #[cfg(not(any(unix, windows)))]
    fn cleanup(&mut self) -> io::Result<()> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "private bundle custody is unsupported on this platform",
        ))
    }

    #[cfg(windows)]
    fn verify_windows_parent(&self) -> io::Result<()> {
        let current = super::open_directory_nofollow(&self.parent_path)?;
        if !windows_same_identity(
            self.parent.as_ref().expect("bundle parent retained"),
            &current,
        ) {
            return Err(io::Error::other("bundle parent identity changed"));
        }
        Ok(())
    }
}

#[cfg(windows)]
fn windows_same_identity(a: &std::fs::File, b: &std::fs::File) -> bool {
    let (Ok(a), Ok(b)) = (super::handle_identity(a), super::handle_identity(b)) else {
        return false;
    };
    a == b
}

impl Drop for BundleTemp {
    fn drop(&mut self) {
        // best-effort: a failed unwind-time cleanup must not panic the drain;
        // an observed replacement is deliberately left in place, and `finish`
        // is the path that reports cleanup failure to the caller.
        #[cfg(unix)]
        let _ = self.cleanup();
        #[cfg(windows)]
        let _ = self.cleanup();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    #[test]
    fn bundle_cleanup_preserves_an_observed_replacement() {
        let temp = BundleTemp::new().unwrap();
        let replacement = temp.path().with_file_name("replacement");
        std::fs::rename(temp.path(), &replacement).unwrap();
        std::fs::write(temp.path(), b"foreign fixture").unwrap();
        let parent = temp.parent_path.clone();
        drop(temp);
        assert_eq!(
            std::fs::read(parent.join("bundle")).unwrap(),
            b"foreign fixture"
        );
        std::fs::remove_file(parent.join("bundle")).unwrap();
        std::fs::remove_file(replacement).unwrap();
        std::fs::remove_dir(parent).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn bundle_custody_rejects_special_replacements_and_keeps_modes() {
        use std::os::unix::ffi::OsStrExt;
        use std::os::unix::fs::{FileTypeExt, MetadataExt, PermissionsExt};
        let initial = BundleTemp::new().unwrap();
        assert_eq!(
            std::fs::metadata(initial.path())
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
        assert_eq!(
            std::fs::metadata(&initial.parent_path)
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o700
        );
        drop(initial);

        for kind in ["symlink", "fifo", "hardlink", "directory"] {
            let temp = BundleTemp::new().unwrap();
            let parent = temp.parent_path.clone();
            let path = temp.path().to_path_buf();
            let retained = parent.join("retained");
            std::fs::rename(&path, &retained).unwrap();
            match kind {
                "symlink" => {
                    std::fs::write(parent.join("target"), b"foreign").unwrap();
                    std::os::unix::fs::symlink(parent.join("target"), &path).unwrap();
                }
                "fifo" => {
                    let name = std::ffi::CString::new(path.as_os_str().as_bytes()).unwrap();
                    assert_eq!(unsafe { libc::mkfifo(name.as_ptr(), 0o600) }, 0);
                }
                "hardlink" => {
                    std::fs::write(&retained, b"foreign").unwrap();
                    std::fs::hard_link(&retained, &path).unwrap();
                }
                "directory" => std::fs::create_dir(&path).unwrap(),
                _ => unreachable!(),
            }
            drop(temp);
            let metadata = std::fs::symlink_metadata(&path).unwrap();
            match kind {
                "symlink" => assert!(metadata.file_type().is_symlink()),
                "fifo" => assert!(metadata.file_type().is_fifo()),
                "hardlink" => assert_eq!(metadata.nlink(), 2),
                "directory" => assert!(metadata.is_dir()),
                _ => unreachable!(),
            }
            if kind == "directory" {
                std::fs::remove_dir(&path).unwrap();
            } else {
                std::fs::remove_file(&path).unwrap();
            }
            std::fs::remove_file(&retained).unwrap();
            if kind == "symlink" {
                std::fs::remove_file(parent.join("target")).unwrap();
            }
            std::fs::remove_dir(parent).unwrap();
        }
    }

    #[cfg(unix)]
    #[test]
    fn bundle_write_revalidation_failure_preserves_replacement() {
        let mut temp = BundleTemp::new().unwrap();
        let parent = temp.parent_path.clone();
        let path = temp.path().to_path_buf();
        std::fs::rename(&path, parent.join("retained")).unwrap();
        std::fs::create_dir(&path).unwrap();
        assert!(temp.write(b"must not publish").is_err());
        drop(temp);
        assert!(path.is_dir());
        std::fs::remove_dir(path).unwrap();
        std::fs::remove_file(parent.join("retained")).unwrap();
        std::fs::remove_dir(parent).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn bundle_creation_collision_preserves_preexisting_leaf() {
        let root = tempfile::tempdir().unwrap();
        assert!(BundleTemp::new_for_test(root.path(), true).is_err());
        let children: Vec<_> = std::fs::read_dir(root.path()).unwrap().collect();
        assert_eq!(children.len(), 1);
        let parent = children[0].as_ref().unwrap().path();
        assert_eq!(
            std::fs::read(parent.join("bundle")).unwrap(),
            b"foreign fixture"
        );
        std::fs::remove_file(parent.join("bundle")).unwrap();
        std::fs::remove_dir(parent).unwrap();
    }

    #[cfg(any(unix, windows))]
    #[test]
    fn bundle_write_and_flush_failures_finish_without_recursive_cleanup() {
        for (fail_write, fail_flush) in [(true, false), (false, true)] {
            let mut temp = BundleTemp::new().unwrap();
            temp.fail_write = fail_write;
            temp.fail_flush = fail_flush;
            let parent = temp.parent_path.clone();
            assert!(temp.write(b"injected failure").is_err());
            temp.finish().unwrap();
            assert!(!parent.exists());
        }
    }

    #[cfg(any(unix, windows))]
    #[test]
    fn bundle_unwind_after_injected_write_failure_cleans_owned_fixture() {
        use std::sync::{Arc, Mutex};
        let parent_slot = Arc::new(Mutex::new(None));
        let slot = Arc::clone(&parent_slot);
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let mut temp = BundleTemp::new().unwrap();
            *slot.lock().unwrap() = Some(temp.parent_path.clone());
            temp.fail_write = true;
            let _ = temp.write(b"injected failure");
            panic!("injected bundle unwind");
        }));
        assert!(result.is_err());
        let parent = parent_slot.lock().unwrap().take().unwrap();
        assert!(!parent.exists());
    }
}
