use std::{
    ffi::{CString, OsStr, OsString},
    fs::{File, Metadata},
    mem::MaybeUninit,
    os::{
        fd::{AsRawFd, FromRawFd},
        unix::{ffi::OsStrExt, fs::MetadataExt},
    },
    path::Path,
};

use crate::state_host_capture::{StateHostCapture as Capture, StateHostReadError as Error};

const MAX_COMPONENTS: usize = 256;
const SIDECARS: [&str; 3] = ["-wal", "-shm", "-journal"];

fn io_error(error: std::io::Error) -> Error {
    // Missing is classified only by the specific component lookup caller.
    match error.raw_os_error() {
        Some(libc::ELOOP) => Error::Unsupported,
        Some(libc::ENOTDIR) => Error::NonRegular,
        _ => Error::Unavailable,
    }
}

fn open_at(parent: i32, name: &OsStr) -> std::io::Result<File> {
    let name = CString::new(name.as_bytes())
        .map_err(|_| std::io::Error::from(std::io::ErrorKind::InvalidInput))?;
    // O_PATH inspects the object without opening a FIFO or device for I/O.
    let fd = unsafe {
        libc::openat(
            parent,
            name.as_ptr(),
            libc::O_PATH | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        )
    };
    if fd < 0 {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: openat returned a new owned descriptor.
    Ok(unsafe { File::from_raw_fd(fd) })
}

fn supported_filesystem(kind: i64) -> bool {
    // Standard local Linux POSIX-permission models only. In particular overlay,
    // NFS, CIFS, FUSE, 9p and unknown filesystems are not inferred safe.
    matches!(kind, 0xef53 | 0x5846_5342 | 0x9123_683e | 0x0102_1994)
}

fn filesystem(file: &File) -> Result<i64, Error> {
    let mut value = MaybeUninit::<libc::statfs>::uninit();
    if unsafe { libc::fstatfs(file.as_raw_fd(), value.as_mut_ptr()) } != 0 {
        return Err(Error::Unavailable);
    }
    // Linux filesystem magic values are 32-bit even when __fsword_t is wider;
    // normalize sign extension on a 32-bit build before classification.
    Ok(i64::from(unsafe { value.assume_init() }.f_type as u32))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Identity {
    device: u64,
    inode: u64,
    uid: u32,
    mode: u32,
}

fn private_owner_mode(owner: u32, mode: u32, uid: u32) -> bool {
    (owner == uid || owner == 0) && mode & 0o022 == 0
}

fn inspect(file: &File, directory: bool, uid: u32) -> Result<Identity, Error> {
    let metadata: Metadata = file.metadata().map_err(io_error)?;
    if metadata.file_type().is_symlink() {
        return Err(Error::Unsupported);
    }
    if if directory {
        !metadata.is_dir()
    } else {
        !metadata.is_file()
    } {
        return Err(Error::NonRegular);
    }
    if !private_owner_mode(metadata.uid(), metadata.mode(), uid) {
        return Err(Error::Unsupported);
    }
    if !supported_filesystem(filesystem(file)?) {
        return Err(Error::Unsupported);
    }
    // On admitted Linux POSIX ACL filesystems, named-entry effective rights
    // are masked by the group mode bits. No group write means no named ACL
    // write grant. Same-UID/root and mount-admin attacks remain out of scope.
    Ok(Identity {
        device: metadata.dev(),
        inode: metadata.ino(),
        uid: metadata.uid(),
        mode: metadata.mode(),
    })
}

struct Entry {
    file: File,
    name: OsString,
    identity: Identity,
}
struct Sidecar {
    name: OsString,
    observed: Option<(File, Identity)>,
}
struct PathChain {
    entries: Vec<Entry>,
    uid: u32,
}

struct PresentObservation {
    chain: PathChain,
    sidecars: Vec<Sidecar>,
}

struct MissingObservation {
    chain: PathChain,
    name: OsString,
    final_component: bool,
}

enum Observation {
    Present(PresentObservation),
    Missing(MissingObservation),
}

impl PathChain {
    fn verify(&self, final_directory: bool) -> Result<(), Error> {
        if inspect(&self.entries[0].file, true, self.uid)? != self.entries[0].identity {
            return Err(Error::Changed);
        }
        for index in 1..self.entries.len() {
            let entry = &self.entries[index];
            let current = open_at(self.entries[index - 1].file.as_raw_fd(), &entry.name)
                .map_err(|_| Error::Changed)?;
            let directory = final_directory || index + 1 < self.entries.len();
            if inspect(&current, directory, self.uid)? != entry.identity {
                return Err(Error::Changed);
            }
        }
        Ok(())
    }
}

impl MissingObservation {
    fn verify(&self) -> Result<(), Error> {
        self.chain.verify(true)?;
        let parent = &self.chain.entries.last().unwrap().file;
        if self.final_component {
            for suffix in SIDECARS {
                let mut name = self.name.clone();
                name.push(suffix);
                match open_at(parent.as_raw_fd(), &name) {
                    // Any adjacent object is orphaned state, even a symlink or
                    // special file. O_PATH never opens it for data/device I/O.
                    Ok(_) => return Err(Error::OrphanedSidecar),
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                    Err(error) => return Err(io_error(error)),
                }
            }
        }
        // Last observation before returning Absent. A new intermediate parent
        // also holds, even if the deeper selected leaf may still be missing.
        // This remains a detector, not an atomic absence/freshness lease.
        match open_at(parent.as_raw_fd(), &self.name) {
            Ok(_) => Err(Error::Changed),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(io_error(error)),
        }
    }
}

fn relative_components<'a>(path: &'a Path, root: &Path) -> Result<Vec<&'a OsStr>, Error> {
    let bytes = path.as_os_str().as_bytes();
    if !path.is_absolute()
        || bytes.len() > 4096
        || bytes
            .iter()
            .any(|b| b.is_ascii_control() || matches!(*b, 0 | b'?' | b'#'))
    {
        return Err(Error::InvalidPath);
    }
    // components() normalizes internal '.', so inspect raw spellings first.
    if bytes[1..]
        .split(|b| *b == b'/')
        .any(|part| part.is_empty() || part == b"." || part == b"..")
    {
        return Err(Error::InvalidPath);
    }
    let relative = path.strip_prefix(root).map_err(|_| Error::InvalidPath)?;
    let parts = relative.iter().collect::<Vec<_>>();
    if parts.is_empty() || parts.len() > MAX_COMPONENTS {
        return Err(Error::InvalidPath);
    }
    Ok(parts)
}

impl Observation {
    fn at(path: &Path, root: &Path) -> Result<Self, Error> {
        let parts = relative_components(path, root)?;
        let uid = unsafe { libc::geteuid() };
        let root_file = open_at(libc::AT_FDCWD, root.as_os_str()).map_err(io_error)?;
        let root_identity = inspect(&root_file, true, uid)?;
        let mut chain = vec![Entry {
            file: root_file,
            name: OsString::new(),
            identity: root_identity,
        }];
        for (index, name) in parts.iter().enumerate() {
            let file = match open_at(chain.last().unwrap().file.as_raw_fd(), name) {
                Ok(file) => file,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                    return Ok(Self::Missing(MissingObservation {
                        chain: PathChain {
                            entries: chain,
                            uid,
                        },
                        name: name.to_os_string(),
                        final_component: index + 1 == parts.len(),
                    }));
                }
                Err(error) => return Err(io_error(error)),
            };
            let identity = inspect(&file, index + 1 < parts.len(), uid)?;
            chain.push(Entry {
                file,
                name: name.to_os_string(),
                identity,
            });
        }
        let parent = &chain[chain.len() - 2].file;
        let leaf = parts.last().unwrap();
        let mut sidecars = Vec::with_capacity(SIDECARS.len());
        for suffix in SIDECARS {
            let mut name = leaf.to_os_string();
            name.push(suffix);
            let observed = match open_at(parent.as_raw_fd(), &name) {
                Ok(file) => {
                    let id = inspect(&file, false, uid)?;
                    Some((file, id))
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
                Err(error) => return Err(io_error(error)),
            };
            sidecars.push(Sidecar { name, observed });
        }
        Ok(Self::Present(PresentObservation {
            chain: PathChain {
                entries: chain,
                uid,
            },
            sidecars,
        }))
    }

    fn verify(&self) -> Result<(), Error> {
        match self {
            Self::Present(observed) => observed.verify(),
            Self::Missing(observed) => observed.verify(),
        }
    }
}

impl PresentObservation {
    fn verify(&self) -> Result<(), Error> {
        self.chain.verify(false)?;
        let parent = &self.chain.entries[self.chain.entries.len() - 2].file;
        for sidecar in &self.sidecars {
            match open_at(parent.as_raw_fd(), &sidecar.name) {
                Ok(file) => {
                    let current = inspect(&file, false, self.chain.uid)?;
                    if let Some((pinned, expected)) = &sidecar.observed
                        && (current != *expected
                            || inspect(pinned, false, self.chain.uid)? != *expected)
                    {
                        return Err(Error::Changed);
                    }
                    // Missing sidecars may legitimately be created by SQLite.
                    // They still must pass the same type/owner/FS/mode checks.
                }
                Err(error)
                    if error.kind() == std::io::ErrorKind::NotFound
                        && sidecar.observed.is_none() => {}
                Err(_) => return Err(Error::Changed),
            }
        }
        Ok(())
    }
}

pub(crate) fn capture(path: &Path) -> Result<Capture, Error> {
    finish_capture(
        path,
        Observation::at(path, Path::new("/"))?,
        || Ok(()),
        thegn_core::host_db_capture::capture_host_definitions_wal_at,
    )
}

// This trusted-root entrypoint is absent from release builds. Production always
// walks from literal '/' and cannot opt out of ancestor checks.
#[cfg(test)]
fn capture_at(
    path: &Path,
    root: &Path,
    after_inspection: impl FnOnce() -> Result<(), Error>,
) -> Result<Capture, Error> {
    finish_capture(
        path,
        Observation::at(path, root)?,
        after_inspection,
        thegn_core::host_db_capture::capture_host_definitions_wal_at,
    )
}

fn finish_capture(
    path: &Path,
    observed: Observation,
    after_inspection: impl FnOnce() -> Result<(), Error>,
    read: impl FnOnce(
        &Path,
    ) -> Result<
        thegn_core::host_definition_snapshot::HostDefinitionsSnapshot,
        thegn_core::host_db_capture::HostCaptureReadError,
    >,
) -> Result<Capture, Error> {
    after_inspection()?;
    observed.verify()?;
    if matches!(observed, Observation::Missing(_)) {
        return Ok(Capture::Absent);
    }
    let result = read(path).map_err(Error::Database);
    // This detects observed replacement, not ABA or the actual SQLite fd.
    observed.verify()?;
    result.map(Capture::Present)
}

#[cfg(test)]
#[path = "state_db_capture_tests.rs"]
mod tests;
