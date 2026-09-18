//! Linux opened-file config reads with bounded identity-checked I/O.
//!
//! O_PATH walks avoid opening FIFOs/devices and preserve physical symlink
//! traversal semantics.  The supported local-filesystem set is deliberate:
//! this adapter does not claim hostile same-UID or mount-admin protection.

use std::collections::VecDeque;
use std::ffi::CString;
use std::fs::File;
use std::io::{self, Read};
use std::os::fd::{AsRawFd, FromRawFd, RawFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::MetadataExt;
use std::path::Path;

use nix::libc;

use crate::config_capture::ConfigFileReadError as Error;

const MAX_PATH: usize = 4096;
const MAX_STEPS: usize = 256;
const MAX_LINKS: usize = 40;
const PROC_MAGIC: i64 = 0x9fa0;

fn fs_kind(file: &File) -> Result<i64, Error> {
    let mut stat = std::mem::MaybeUninit::<libc::statfs>::uninit();
    if unsafe { libc::fstatfs(file.as_raw_fd(), stat.as_mut_ptr()) } != 0 {
        return Err(Error::Unavailable);
    }
    Ok(i64::from(unsafe { stat.assume_init() }.f_type as u32))
}

fn local_fs(file: &File) -> Result<(), Error> {
    match fs_kind(file)? {
        0xef53 | 0x58465342 | 0x9123683e | 0x01021994 => Ok(()),
        _ => Err(Error::Unavailable),
    }
}

fn open_at(parent: RawFd, name: &[u8], flags: i32) -> io::Result<File> {
    let name = CString::new(name).map_err(|_| io::Error::from_raw_os_error(libc::EINVAL))?;
    let fd = unsafe { libc::openat(parent, name.as_ptr(), flags | libc::O_CLOEXEC) };
    if fd < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(unsafe { File::from_raw_fd(fd) })
    }
}

fn root() -> Result<File, Error> {
    let file = open_at(
        libc::AT_FDCWD,
        b"/",
        libc::O_PATH | libc::O_NOFOLLOW | libc::O_DIRECTORY,
    )
    .map_err(|_| Error::Unavailable)?;
    local_fs(&file)?;
    Ok(file)
}

#[derive(Debug)]
enum Step {
    Component(Vec<u8>),
    EndTarget,
}

struct Budget {
    bytes: usize,
    components: usize,
    links: usize,
}

impl Budget {
    fn components(&mut self, raw: &[u8]) -> Result<VecDeque<Step>, Error> {
        if raw.is_empty() || raw.contains(&0) || raw.iter().any(u8::is_ascii_control) {
            return Err(Error::Unavailable);
        }
        self.bytes = self
            .bytes
            .checked_add(raw.len())
            .filter(|value| *value <= MAX_PATH)
            .ok_or(Error::Unavailable)?;
        let count = raw.split(|byte| *byte == b'/').count();
        self.components = self
            .components
            .checked_add(count)
            .filter(|value| *value <= MAX_STEPS)
            .ok_or(Error::Unavailable)?;
        let mut steps: VecDeque<_> = raw
            .split(|byte| *byte == b'/')
            .filter(|part| !part.is_empty())
            .map(|part| Step::Component(part.to_vec()))
            .collect();
        if raw.ends_with(b"/") {
            steps.push_back(Step::Component(b".".to_vec()));
        }
        Ok(steps)
    }
}

fn link_target(file: &File, remaining: usize) -> Result<Vec<u8>, Error> {
    let mut data = [0u8; MAX_PATH + 1];
    let len = unsafe {
        libc::readlinkat(
            file.as_raw_fd(),
            c"".as_ptr(),
            data.as_mut_ptr().cast(),
            data.len(),
        )
    };
    if len <= 0 || len as usize > MAX_PATH || len as usize > remaining {
        return Err(Error::Unavailable);
    }
    Ok(data[..len as usize].to_vec())
}

fn metadata_target(path: &Path) -> Result<Option<File>, Error> {
    if !path.is_absolute() {
        return Err(Error::Unavailable);
    }
    let mut budget = Budget {
        bytes: 0,
        components: 0,
        links: 0,
    };
    let mut steps = budget.components(path.as_os_str().as_bytes())?;
    let mut current = root()?;
    let mut unresolved_targets = 0usize;
    while let Some(step) = steps.pop_front() {
        let Step::Component(name) = step else {
            unresolved_targets = unresolved_targets
                .checked_sub(1)
                .ok_or(Error::Unavailable)?;
            continue;
        };
        if !current.metadata().map_err(|_| Error::Unavailable)?.is_dir() {
            return Err(Error::NonRegular);
        }
        if name == b"." {
            continue;
        }
        let child = match open_at(current.as_raw_fd(), &name, libc::O_PATH | libc::O_NOFOLLOW) {
            Ok(file) => file,
            Err(error) if error.raw_os_error() == Some(libc::ENOENT) => {
                return if unresolved_targets == 0 {
                    Ok(None)
                } else {
                    Err(Error::Unavailable)
                };
            }
            Err(error) if error.raw_os_error() == Some(libc::ENOTDIR) => {
                return Err(Error::NonRegular);
            }
            Err(_) => return Err(Error::Unavailable),
        };
        local_fs(&child)?;
        let metadata = child.metadata().map_err(|_| Error::Unavailable)?;
        if metadata.file_type().is_symlink() {
            budget.links = budget.links.checked_add(1).ok_or(Error::Unavailable)?;
            if budget.links > MAX_LINKS {
                return Err(Error::Unavailable);
            }
            let target = link_target(&child, MAX_PATH.saturating_sub(budget.bytes))?;
            let mut target_steps = budget.components(&target)?;
            if target.starts_with(b"/") {
                current = root()?;
            }
            unresolved_targets = unresolved_targets
                .checked_add(1)
                .ok_or(Error::Unavailable)?;
            target_steps.push_back(Step::EndTarget);
            target_steps.append(&mut steps);
            steps = target_steps;
        } else {
            current = child;
        }
    }
    if !current
        .metadata()
        .map_err(|_| Error::Unavailable)?
        .is_file()
    {
        return Err(Error::NonRegular);
    }
    Ok(Some(current))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Identity {
    device: u64,
    inode: u64,
    length: u64,
    mtime_seconds: i64,
    mtime_nanoseconds: i64,
    ctime_seconds: i64,
    ctime_nanoseconds: i64,
}

fn identity(file: &File) -> Result<Identity, Error> {
    let metadata = file.metadata().map_err(|_| Error::Unavailable)?;
    if !metadata.is_file() {
        return Err(Error::NonRegular);
    }
    local_fs(file)?;
    Ok(Identity {
        device: metadata.dev(),
        inode: metadata.ino(),
        length: metadata.len(),
        mtime_seconds: metadata.mtime(),
        mtime_nanoseconds: metadata.mtime_nsec(),
        ctime_seconds: metadata.ctime(),
        ctime_nanoseconds: metadata.ctime_nsec(),
    })
}

fn readable(metadata: &File) -> Result<File, Error> {
    let proc = open_at(
        libc::AT_FDCWD,
        b"/proc/self/fd",
        libc::O_PATH | libc::O_DIRECTORY,
    )
    .map_err(|_| Error::Unavailable)?;
    if fs_kind(&proc)? != PROC_MAGIC {
        return Err(Error::Unavailable);
    }
    open_at(
        proc.as_raw_fd(),
        metadata.as_raw_fd().to_string().as_bytes(),
        libc::O_RDONLY | libc::O_NONBLOCK,
    )
    .map_err(|_| Error::Unavailable)
}

fn read_target(
    metadata: File,
    limit: usize,
    open: impl FnOnce(&File) -> Result<File, Error>,
) -> Result<Vec<u8>, Error> {
    if limit > thegn_core::config_budget::MAX_SOURCE_BYTES {
        return Err(Error::TooLarge);
    }
    let expected = identity(&metadata)?;
    let readable = open(&metadata)?;
    if identity(&readable)? != expected {
        return Err(Error::Changed);
    }
    let extra = limit.checked_add(1).ok_or(Error::TooLarge)?;
    let mut read = readable.take(extra as u64);
    let mut bytes = Vec::new();
    let mut chunk = [0u8; 8192];
    loop {
        let count = read.read(&mut chunk).map_err(|_| Error::Unavailable)?;
        if count == 0 {
            break;
        }
        if bytes
            .len()
            .checked_add(count)
            .is_none_or(|size| size > limit)
        {
            return Err(Error::TooLarge);
        }
        bytes.extend_from_slice(&chunk[..count]);
    }
    if identity(read.get_ref())? != expected {
        return Err(Error::Changed);
    }
    Ok(bytes)
}

fn read_path(
    path: &Path,
    limit: usize,
    after_read: impl FnOnce() -> Result<(), Error>,
) -> Result<Option<Vec<u8>>, Error> {
    if limit > thegn_core::config_budget::MAX_SOURCE_BYTES {
        return Err(Error::TooLarge);
    }
    let Some(metadata) = metadata_target(path)? else {
        return Ok(None);
    };
    let expected = identity(&metadata)?;
    let bytes = read_target(metadata, limit, readable)?;
    after_read()?;
    // Re-open only for an O_PATH observation: data is read through the
    // already-opened descriptor above. This detects equal-size replacement of
    // the pathname while preserving the descriptor identity claim.
    let Some(after) = metadata_target(path)? else {
        return Err(Error::Changed);
    };
    if identity(&after)? != expected {
        return Err(Error::Changed);
    }
    Ok(Some(bytes))
}

pub(super) fn read(path: &Path, limit: usize) -> Result<Option<Vec<u8>>, Error> {
    read_path(path, limit, || Ok(()))
}

#[cfg(test)]
#[path = "config_file_capture_tests.rs"]
mod tests;
