//! Descriptor-relative gate-state admission. No operation recursively removes.
//!
//! Pins and rechecks detect observed replacement; advisory locking is not a
//! filesystem lease against an arbitrary hostile process with the same UID.

use std::fs::File;
use std::io;
use std::path::{Path, PathBuf};

#[cfg(unix)]
use std::os::unix::fs::MetadataExt;

pub(crate) struct Directory {
    path: PathBuf,
    pins: Vec<(PathBuf, File, bool)>,
}

pub(crate) struct Regular {
    parent: Directory,
    path: PathBuf,
    file: File,
}

pub(crate) struct Lock(Regular);

#[cfg(test)]
pub(crate) fn history_test_symlink(_original: &Path, _link: &Path) -> io::Result<()> {
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(_original, _link)
    }
    #[cfg(not(unix))]
    {
        Err(io::Error::from(io::ErrorKind::Unsupported))
    }
}

#[cfg(test)]
pub(crate) fn history_test_nonunicode() -> Option<std::ffi::OsString> {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStringExt;
        Some(std::ffi::OsString::from_vec(vec![255]))
    }
    #[cfg(not(unix))]
    {
        None
    }
}

fn refused(message: &'static str) -> io::Error {
    io::Error::other(message)
}

#[cfg(unix)]
fn directory_metadata(meta: &std::fs::Metadata, private: bool) -> io::Result<()> {
    // SAFETY: geteuid has no memory arguments.
    let uid = unsafe { libc::geteuid() };
    let owner = meta.uid();
    let writable = meta.mode() & 0o022 != 0;
    // MetadataExt::mode is u32; libc::mode_t is narrower on Apple/BSD.
    let root_sticky = owner == 0 && meta.mode() & 0o1000_u32 != 0;
    if !meta.is_dir()
        || (owner != uid && owner != 0)
        || (private && owner != uid)
        || (writable && (private || !root_sticky))
    {
        return Err(refused(
            "gate directory ownership or permissions are unsafe",
        ));
    }
    Ok(())
}

#[cfg(unix)]
fn same_file(a: &std::fs::Metadata, b: &std::fs::Metadata) -> bool {
    a.dev() == b.dev() && a.ino() == b.ino()
}

#[cfg(unix)]
fn safe_regular(is_file: bool, owner: u32, expected_owner: u32, links: u64, mode: u32) -> bool {
    is_file && owner == expected_owner && links == 1 && mode & 0o022 == 0
}

impl Directory {
    /// Only missing components at/below `create_from` may be created. Existing
    /// ancestor symlinks are refused, not silently canonicalized and adopted.
    #[cfg_attr(not(unix), allow(unused_variables))]
    pub(crate) fn open(path: &Path, create_from: Option<&Path>) -> io::Result<Self> {
        #[cfg(unix)]
        {
            use std::ffi::CString;
            use std::os::fd::{AsRawFd, FromRawFd};
            use std::os::unix::ffi::OsStrExt;
            use std::path::Component;
            if !path.is_absolute() {
                return Err(refused("gate path must be absolute"));
            }
            let mut at = PathBuf::from("/");
            let mut pins = vec![(at.clone(), File::open("/")?, false)];
            for part in path.components().skip(1) {
                let Component::Normal(part) = part else {
                    return Err(refused("gate path is not canonical"));
                };
                at.push(part);
                let name = CString::new(part.as_bytes()).map_err(io::Error::other)?;
                let parent = pins.last().unwrap().1.as_raw_fd();
                let flags = libc::O_RDONLY
                    | libc::O_CLOEXEC
                    | libc::O_NOFOLLOW
                    | libc::O_DIRECTORY
                    | libc::O_NONBLOCK;
                // SAFETY: parent is retained, name is one terminated component.
                let mut fd = unsafe { libc::openat(parent, name.as_ptr(), flags) };
                if fd < 0
                    && io::Error::last_os_error().kind() == io::ErrorKind::NotFound
                    && create_from.is_some_and(|root| at.starts_with(root))
                {
                    // SAFETY: same pinned parent and single component as above.
                    let created = unsafe { libc::mkdirat(parent, name.as_ptr(), 0o700) };
                    if created != 0
                        && io::Error::last_os_error().kind() != io::ErrorKind::AlreadyExists
                    {
                        return Err(io::Error::last_os_error());
                    }
                    // SAFETY: reopen with NOFOLLOW even if a concurrent creator won.
                    fd = unsafe { libc::openat(parent, name.as_ptr(), flags) };
                }
                if fd < 0 {
                    return Err(io::Error::last_os_error());
                }
                // SAFETY: openat returned an exclusively owned descriptor.
                let file = unsafe { File::from_raw_fd(fd) };
                directory_metadata(
                    &file.metadata()?,
                    create_from.is_some_and(|root| at.starts_with(root)),
                )?;
                pins.push((
                    at.clone(),
                    file,
                    create_from.is_some_and(|root| at.starts_with(root)),
                ));
            }
            if at.as_os_str() != path.as_os_str() {
                return Err(refused("gate path is not canonical"));
            }
            let this = Self { path: at, pins };
            this.verify()?;
            Ok(this)
        }
        #[cfg(not(unix))]
        {
            Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "verified local gate state is unsupported on this platform",
            ))
        }
    }

    pub(crate) fn path(&self) -> &Path {
        &self.path
    }

    pub(crate) fn verify(&self) -> io::Result<()> {
        #[cfg(unix)]
        {
            for (path, file, private) in &self.pins {
                let current = std::fs::symlink_metadata(path)?;
                directory_metadata(&current, *private)?;
                if !same_file(&file.metadata()?, &current) {
                    return Err(refused("gate directory identity changed"));
                }
            }
            Ok(())
        }
        #[cfg(not(unix))]
        {
            Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "verified local gate state is unsupported on this platform",
            ))
        }
    }

    #[cfg(unix)]
    fn fd(&self) -> std::os::fd::RawFd {
        use std::os::fd::AsRawFd;
        self.pins.last().unwrap().1.as_raw_fd()
    }
}

impl Regular {
    pub(crate) fn open_existing(path: &Path) -> io::Result<Self> {
        Self::open(path, false)
    }

    /// Remove only this verified regular leaf via its pinned parent. There is
    /// still no atomic lease against an arbitrary same-UID leaf replacement.
    pub(crate) fn remove_verified(self) -> io::Result<()> {
        self.verify()?;
        #[cfg(unix)]
        {
            use std::ffi::CString;
            use std::os::unix::ffi::OsStrExt;
            let name = CString::new(self.path.file_name().unwrap().as_bytes())
                .map_err(io::Error::other)?;
            // SAFETY: retained parent and one terminated leaf name; no recursion.
            if unsafe { libc::unlinkat(self.parent.fd(), name.as_ptr(), 0) } != 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        }
        #[cfg(not(unix))]
        {
            Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "verified gate removal is unsupported on this platform",
            ))
        }
    }

    #[cfg_attr(not(unix), allow(unused_variables))]
    fn open(path: &Path, create_lock: bool) -> io::Result<Self> {
        #[cfg(unix)]
        {
            use std::ffi::CString;
            use std::os::fd::FromRawFd;
            use std::os::unix::ffi::OsStrExt;
            let parent = Directory::open(
                path.parent()
                    .ok_or_else(|| refused("gate file has no parent"))?,
                None,
            )?;
            let name = CString::new(
                path.file_name()
                    .ok_or_else(|| refused("gate file has no name"))?
                    .as_bytes(),
            )
            .map_err(io::Error::other)?;
            let access = if create_lock {
                libc::O_RDWR | libc::O_CREAT
            } else {
                libc::O_RDONLY
            };
            // SAFETY: parent is retained and name is a single terminated component.
            let fd = unsafe {
                libc::openat(
                    parent.fd(),
                    name.as_ptr(),
                    access | libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC,
                    0o600,
                )
            };
            if fd < 0 {
                return Err(io::Error::last_os_error());
            }
            // SAFETY: descriptor is newly owned here.
            let file = unsafe { File::from_raw_fd(fd) };
            let this = Self {
                parent,
                path: path.into(),
                file,
            };
            this.verify()?;
            Ok(this)
        }
        #[cfg(not(unix))]
        {
            Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "verified gate files are unsupported on this platform",
            ))
        }
    }

    pub(crate) fn verify(&self) -> io::Result<()> {
        #[cfg(unix)]
        {
            self.parent.verify()?;
            let meta = self.file.metadata()?;
            // SAFETY: geteuid has no memory arguments.
            if !safe_regular(
                meta.is_file(),
                meta.uid(),
                unsafe { libc::geteuid() },
                meta.nlink(),
                meta.mode(),
            ) || !same_file(&meta, &std::fs::symlink_metadata(&self.path)?)
            {
                return Err(refused("gate file ownership, type or identity changed"));
            }
            Ok(())
        }
        #[cfg(not(unix))]
        {
            Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "verified gate files are unsupported on this platform",
            ))
        }
    }
}

pub(crate) fn read_regular(path: &Path) -> io::Result<String> {
    use std::io::Read;
    const LIMIT: u64 = 64 * 1024;
    let regular = Regular::open(path, false)?;
    if regular.file.metadata()?.len() > LIMIT {
        return Err(refused("gate identity file is oversized"));
    }
    let mut data = Vec::new();
    (&regular.file).take(LIMIT + 1).read_to_end(&mut data)?;
    if data.len() as u64 > LIMIT {
        return Err(refused("gate identity file grew beyond limit"));
    }
    regular.verify()?;
    String::from_utf8(data).map_err(|_| refused("gate identity is not UTF-8"))
}

impl Lock {
    pub(crate) fn acquire(path: &Path) -> io::Result<Self> {
        let regular = Regular::open(path, true)?;
        regular.file.try_lock().map_err(|error| match error {
            std::fs::TryLockError::WouldBlock => io::Error::new(
                io::ErrorKind::WouldBlock,
                "gate is already running; retry after it finishes",
            ),
            std::fs::TryLockError::Error(error) => error,
        })?;
        regular.verify()?;
        Ok(Self(regular))
    }
    pub(crate) fn verify(&self) -> io::Result<()> {
        self.0.verify()
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::os::unix::fs::{PermissionsExt, symlink};

    fn private_root() -> tempfile::TempDir {
        tempfile::Builder::new()
            .prefix("thegn-gate-path-")
            .tempdir_in(std::fs::canonicalize(std::env::temp_dir()).unwrap())
            .unwrap()
    }

    #[test]
    fn foreign_owner_and_privilege_sharing_never_admit_regular_files() {
        assert!(safe_regular(true, 1000, 1000, 1, 0o600));
        assert!(!safe_regular(true, 1001, 1000, 1, 0o600));
        assert!(!safe_regular(true, 1000, 1000, 2, 0o600));
        assert!(!safe_regular(true, 1000, 1000, 1, 0o666));
        assert!(!safe_regular(false, 1000, 1000, 1, 0o600));
    }

    #[test]
    fn lock_is_nonblocking_and_releases_with_its_owner() {
        let root = private_root();
        let path = root.path().join("gate.lock");
        let lock = Lock::acquire(&path).unwrap();
        let start = std::time::Instant::now();
        assert!(
            matches!(Lock::acquire(&path), Err(error) if error.kind() == io::ErrorKind::WouldBlock)
        );
        assert!(start.elapsed() < std::time::Duration::from_secs(1));
        drop(lock);
        assert!(Lock::acquire(&path).is_ok());
    }

    #[test]
    fn lock_rejects_symlinks_hardlinks_and_replaced_identity() {
        let root = private_root();
        let path = root.path().join("lock");
        let lock = Lock::acquire(&path).unwrap();
        symlink(&path, root.path().join("alias")).unwrap();
        assert!(Lock::acquire(&root.path().join("alias")).is_err());
        std::fs::hard_link(&path, root.path().join("hardlink")).unwrap();
        assert!(lock.verify().is_err());
        assert!(Lock::acquire(&root.path().join("hardlink")).is_err());
        std::fs::rename(&path, root.path().join("previous")).unwrap();
        std::fs::write(&path, b"replacement").unwrap();
        assert!(lock.verify().is_err());
        assert_eq!(std::fs::read(&path).unwrap(), b"replacement");
    }

    #[test]
    fn special_files_and_oversized_identity_fail_without_waiting() {
        use std::ffi::CString;
        use std::os::unix::ffi::OsStrExt;
        let root = private_root();
        let fifo = root.path().join("fifo");
        let cpath = CString::new(fifo.as_os_str().as_bytes()).unwrap();
        // SAFETY: owned private fixture path, valid terminated name.
        assert_eq!(unsafe { libc::mkfifo(cpath.as_ptr(), 0o600) }, 0);
        let (sender, receiver) = std::sync::mpsc::channel();
        let worker_path = fifo.clone();
        let worker = std::thread::spawn(move || {
            sender
                .send((
                    Lock::acquire(&worker_path).is_err(),
                    read_regular(&worker_path).is_err(),
                ))
                .unwrap()
        });
        let result = receiver.recv_timeout(std::time::Duration::from_secs(1));
        // A regressed blocking open is released before joining, so the negative
        // test reports failure rather than leaving an indefinitely stuck worker.
        let unblock = if result.is_err() {
            use std::os::unix::fs::OpenOptionsExt;
            Some(
                std::fs::OpenOptions::new()
                    .read(true)
                    .write(true)
                    .custom_flags(libc::O_NONBLOCK)
                    .open(&fifo)
                    .unwrap(),
            )
        } else {
            None
        };
        worker.join().unwrap();
        drop(unblock);
        assert_eq!(result.unwrap(), (true, true));
        let socket = root.path().join("socket");
        let _listener = std::os::unix::net::UnixListener::bind(&socket).unwrap();
        assert!(Lock::acquire(&socket).is_err());
        let large = root.path().join("large");
        std::fs::write(&large, vec![b'x'; 64 * 1024 + 1]).unwrap();
        assert!(read_regular(&large).is_err());
    }

    #[test]
    fn directory_rejects_alias_permissions_and_physical_replacement() {
        let root = private_root();
        let path = root.path().join("state");
        let directory = Directory::open(&path, Some(&path)).unwrap();
        symlink(&path, root.path().join("alias")).unwrap();
        assert!(Directory::open(&root.path().join("alias"), None).is_err());
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o777)).unwrap();
        assert!(directory.verify().is_err());
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700)).unwrap();
        std::fs::rename(&path, root.path().join("old")).unwrap();
        std::fs::create_dir(&path).unwrap();
        assert!(directory.verify().is_err());
        assert!(root.path().join("old").is_dir());
        assert!(path.is_dir());
    }

    #[test]
    fn lock_open_failure_is_not_unlocked_success() {
        let root = private_root();
        assert!(Lock::acquire(&root.path().join("missing/lock")).is_err());
        let path = root.path().join("directory");
        std::fs::create_dir(&path).unwrap();
        assert!(Lock::acquire(&path).is_err());
        let file = root.path().join("writable");
        std::fs::write(&file, b"sentinel").unwrap();
        std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o666)).unwrap();
        assert!(Lock::acquire(&file).is_err());
        assert_eq!(std::fs::read(&file).unwrap(), b"sentinel");
    }
}

#[cfg(all(test, not(unix)))]
mod unsupported_tests {
    use super::*;
    #[test]
    fn unsupported_gate_paths_refuse_without_creation() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("state");
        assert!(
            matches!(Directory::open(&path, Some(&path)), Err(error) if error.kind() == io::ErrorKind::Unsupported)
        );
        assert!(
            matches!(Lock::acquire(&path), Err(error) if error.kind() == io::ErrorKind::Unsupported)
        );
        assert!(!path.exists());
    }
}
