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

    #[cfg(unix)]
    fn try_clone(&self) -> io::Result<Self> {
        Ok(Self {
            path: self.path.clone(),
            pins: self
                .pins
                .iter()
                .map(|(path, file, private)| Ok((path.clone(), file.try_clone()?, *private)))
                .collect::<io::Result<_>>()?,
        })
    }
}

impl Regular {
    /// Create a new owner-only regular leaf without ever opening an existing
    /// path. The caller retains this descriptor for the whole operation and
    /// may later remove it through [`remove_verified`].
    #[cfg(all(test, unix))]
    pub(crate) fn create_exclusive(path: &Path) -> io::Result<Self> {
        Self::open(path, true, true)
    }

    /// Create a leaf beneath an already-retained directory descriptor.  The
    /// caller uses this when the parent was admitted as a private fixture;
    /// reopening the absolute path would reintroduce a pathname race between
    /// parent admission and leaf creation.
    #[cfg(unix)]
    pub(crate) fn create_exclusive_at(parent: &Directory, path: &Path) -> io::Result<Self> {
        use std::ffi::CString;
        use std::os::fd::FromRawFd;
        use std::os::unix::ffi::OsStrExt;
        if path.parent() != Some(parent.path()) {
            return Err(refused(
                "gate file parent does not match retained directory",
            ));
        }
        // Admit and clone the parent before creation. No post-create failure
        // can then leave a leaf without an owned parent descriptor.
        parent.verify()?;
        let retained_parent = parent.try_clone()?;
        let name = CString::new(
            path.file_name()
                .ok_or_else(|| refused("gate file has no name"))?
                .as_bytes(),
        )
        .map_err(io::Error::other)?;
        // SAFETY: parent is retained and name is one terminated component.
        let fd = unsafe {
            libc::openat(
                parent.fd(),
                name.as_ptr(),
                libc::O_RDWR
                    | libc::O_CREAT
                    | libc::O_EXCL
                    | libc::O_NOFOLLOW
                    | libc::O_NONBLOCK
                    | libc::O_CLOEXEC,
                0o600,
            )
        };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        // Adopt the descriptor immediately so every post-create error closes
        // it. The retained parent was admitted before openat above.
        let file = unsafe { File::from_raw_fd(fd) };
        let this = Self {
            parent: retained_parent,
            path: path.into(),
            file,
        };
        if let Err(error) = this.verify() {
            return match this.remove_if_same_identity() {
                Ok(()) => Err(error),
                Err(cleanup) => Err(io::Error::other(format!(
                    "created gate leaf verification failed; leaf preserved: {error}; cleanup refused or failed: {cleanup}"
                ))),
            };
        }
        Ok(this)
    }

    pub(crate) fn open_existing(path: &Path) -> io::Result<Self> {
        Self::open(path, false, false)
    }

    /// Remove only this verified regular leaf via its pinned parent. There is
    /// still no atomic lease against an arbitrary same-UID leaf replacement.
    pub(crate) fn remove_verified(self) -> io::Result<()> {
        self.verify()?;
        self.remove_leaf()
    }

    fn remove_leaf(self) -> io::Result<()> {
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

    /// Settle a construction error only when both retained identities still
    /// name the objects opened by this instance. A replacement or changed
    /// parent is preserved; this is not a pathname-only cleanup fallback.
    #[cfg(unix)]
    fn remove_if_same_identity(self) -> io::Result<()> {
        let current_parent = std::fs::symlink_metadata(self.parent.path())?;
        if !same_file(
            &self.parent.pins.last().unwrap().1.metadata()?,
            &current_parent,
        ) {
            return Err(refused(
                "gate parent identity changed; preserving created leaf",
            ));
        }
        let current_leaf = std::fs::symlink_metadata(&self.path)?;
        if !same_file(&self.file.metadata()?, &current_leaf)
            || !safe_regular(
                current_leaf.is_file(),
                current_leaf.uid(),
                unsafe { libc::geteuid() },
                current_leaf.nlink(),
                current_leaf.mode(),
            )
        {
            return Err(refused(
                "gate leaf identity changed; preserving replacement",
            ));
        }
        self.remove_leaf()
    }

    #[cfg_attr(not(unix), allow(unused_variables))]
    fn open(path: &Path, create_lock: bool, create_exclusive: bool) -> io::Result<Self> {
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
            let create = if create_exclusive {
                libc::O_CREAT | libc::O_EXCL
            } else {
                0
            };
            // SAFETY: parent is retained and name is a single terminated component.
            let fd = unsafe {
                libc::openat(
                    parent.fd(),
                    name.as_ptr(),
                    access | create | libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC,
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

    pub(crate) fn write_all(&mut self, bytes: &[u8]) -> io::Result<()> {
        use std::io::Write;
        self.file.write_all(bytes)
    }

    pub(crate) fn flush(&mut self) -> io::Result<()> {
        use std::io::Write;
        self.file.flush()
    }
}

pub(crate) fn read_regular(path: &Path) -> io::Result<String> {
    use std::io::Read;
    const LIMIT: u64 = 64 * 1024;
    let regular = Regular::open(path, false, false)?;
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
        let regular = Regular::open(path, true, false)?;
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
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    use super::super::owned_test_child::OwnedChild;

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

    #[test]
    fn exclusive_regular_creation_never_adopts_an_existing_leaf() {
        let root = private_root();
        let path = root.path().join("bundle");
        let mut regular = Regular::create_exclusive(&path).unwrap();
        regular.write_all(b"bundle").unwrap();
        regular.flush().unwrap();
        regular.verify().unwrap();
        assert!(Regular::create_exclusive(&path).is_err());
        assert_eq!(std::fs::read(&path).unwrap(), b"bundle");
    }

    #[test]
    fn failed_identity_cleanup_preserves_a_replacement() {
        let root = private_root();
        let path = root.path().join("bundle");
        let regular = Regular::create_exclusive(&path).unwrap();
        let retained = root.path().join("retained");
        std::fs::rename(&path, &retained).unwrap();
        std::fs::write(&path, b"foreign replacement").unwrap();
        assert!(regular.remove_if_same_identity().is_err());
        assert_eq!(std::fs::read(&path).unwrap(), b"foreign replacement");
        std::fs::remove_file(path).unwrap();
        std::fs::remove_file(retained).unwrap();
    }

    #[test]
    #[ignore = "root-owned exact child custody gate"]
    fn exclusive_regular_creation_child() {
        let root = PathBuf::from(std::env::var_os("THEGN_GATE_CHILD_ROOT").unwrap());
        let ready = PathBuf::from(std::env::var_os("THEGN_GATE_CHILD_READY").unwrap());
        let result = PathBuf::from(std::env::var_os("THEGN_GATE_CHILD_RESULT").unwrap());
        let release = root.join("release");
        let target = root.join("shared.bundle");
        File::create_new(&ready).unwrap();
        let deadline = Instant::now() + Duration::from_secs(2);
        while !release.exists() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(release.exists(), "child barrier release timed out");
        let outcome = match Regular::create_exclusive(&target) {
            Ok(mut file) => {
                file.write_all(b"winner").unwrap();
                file.flush().unwrap();
                "winner"
            }
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => "loser",
            Err(error) => panic!("unexpected exclusive-create error: {error}"),
        };
        std::fs::write(result, outcome).unwrap();
    }

    #[test]
    fn exclusive_regular_creation_has_one_winner_across_two_owned_children() {
        use std::process::{Command, Stdio};
        let root = Arc::new(private_root());
        let root_path = root.path().to_path_buf();
        let release = root_path.join("release");
        let target = root_path.join("shared.bundle");
        let mut children = Vec::new();
        for name in ["a", "b"] {
            let child_dir = root_path.join(format!("child-{name}"));
            std::fs::create_dir(&child_dir).unwrap();
            let mut command = Command::new(std::env::current_exe().unwrap());
            command
                .args([
                    "--ignored",
                    "--exact",
                    "platform::gate_path::tests::exclusive_regular_creation_child",
                    "--nocapture",
                ])
                .env_clear()
                .env("THEGN_GATE_CHILD_ROOT", &root_path)
                .env("THEGN_GATE_CHILD_READY", child_dir.join("ready"))
                .env("THEGN_GATE_CHILD_RESULT", child_dir.join("result"))
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null());
            children.push(OwnedChild::spawn(&mut command, Arc::clone(&root)));
        }
        let mut settled = [false, false];
        let deadline = Instant::now() + Duration::from_secs(2);
        while !(root_path.join("child-a/ready").exists()
            && root_path.join("child-b/ready").exists())
            && Instant::now() < deadline
        {
            for (child, done) in children.iter_mut().zip(&mut settled) {
                if !*done {
                    *done = child.poll().is_some();
                }
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(
            root_path.join("child-a/ready").exists() && root_path.join("child-b/ready").exists()
        );
        File::create_new(&release).unwrap();
        let deadline = Instant::now() + Duration::from_secs(2);
        while !(root_path.join("child-a/result").exists()
            && root_path.join("child-b/result").exists())
            && Instant::now() < deadline
        {
            for (child, done) in children.iter_mut().zip(&mut settled) {
                if !*done {
                    *done = child.poll().is_some();
                }
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(
            root_path.join("child-a/result").exists() && root_path.join("child-b/result").exists()
        );
        for (index, child) in children.iter_mut().enumerate() {
            let deadline = Instant::now() + Duration::from_secs(2);
            while !settled[index] && Instant::now() < deadline {
                settled[index] = child.poll().is_some();
                std::thread::sleep(Duration::from_millis(5));
            }
            assert!(settled[index], "child did not settle");
        }
        let outcomes = [
            std::fs::read_to_string(root_path.join("child-a/result")).unwrap(),
            std::fs::read_to_string(root_path.join("child-b/result")).unwrap(),
        ];
        assert_eq!(
            outcomes.iter().filter(|v| v.as_str() == "winner").count(),
            1
        );
        assert_eq!(outcomes.iter().filter(|v| v.as_str() == "loser").count(), 1);
        assert_eq!(std::fs::read(&target).unwrap(), b"winner");
        for name in ["a", "b"] {
            let child_dir = root_path.join(format!("child-{name}"));
            std::fs::remove_file(child_dir.join("ready")).unwrap();
            std::fs::remove_file(child_dir.join("result")).unwrap();
            std::fs::remove_dir(child_dir).unwrap();
        }
        std::fs::remove_file(target).unwrap();
        std::fs::remove_file(release).unwrap();
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
