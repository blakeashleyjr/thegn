use std::ffi::CString;
use std::fs::{self, File};
use std::io::Read;
use std::os::fd::FromRawFd;
use std::os::unix::ffi::OsStrExt;
use std::path::Path;

use nix::libc;

use crate::config_capture::ConfigFileReadError as Error;

// The guarantee is observed coherence in a stable, owner-controlled
// namespace. It is not a hostile same-UID or mount-administrator guarantee.
fn regular_path(path: &Path) -> Result<bool, Error> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() => Err(Error::FinalLinkUnsupported),
        Ok(metadata) if !metadata.is_file() => Err(Error::NonRegular),
        Ok(_) => Ok(true),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(_) => Err(Error::Unavailable),
    }
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

fn open(path: &Path) -> Result<Option<File>, Error> {
    // macOS has no O_PATH equivalent. Prove the pathname is an ordinary file
    // before the nonblocking data open, then repeat the proof on the handle;
    // final symlinks are intentionally refused (including Nix-managed ones).
    if !regular_path(path)? {
        return Ok(None);
    }
    let name = CString::new(path.as_os_str().as_bytes()).map_err(|_| Error::Unavailable)?;
    let fd = unsafe {
        libc::open(
            name.as_ptr(),
            libc::O_RDONLY | libc::O_CLOEXEC | libc::O_NOFOLLOW | libc::O_NONBLOCK,
        )
    };
    if fd < 0 {
        return match std::io::Error::last_os_error().raw_os_error() {
            Some(libc::ENOENT) => Ok(None),
            Some(libc::ELOOP) => Err(Error::FinalLinkUnsupported),
            _ => Err(Error::Unavailable),
        };
    }
    Ok(Some(unsafe { File::from_raw_fd(fd) }))
}

fn regular_identity(file: &File) -> Result<Identity, Error> {
    // `File::metadata` is an fstat on the held descriptor; the portable
    // MetadataExt accessors carry the same fields with fixed widths on every
    // Unix this module is compiled for (macOS, and Linux as a fallback).
    use std::os::unix::fs::MetadataExt;
    let metadata = file.metadata().map_err(|_| Error::Unavailable)?;
    if !metadata.file_type().is_file() {
        return Err(Error::NonRegular);
    }
    Ok(Identity {
        device: metadata.dev(),
        inode: metadata.ino(),
        length: metadata.size(),
        mtime_seconds: metadata.mtime(),
        mtime_nanoseconds: metadata.mtime_nsec(),
        ctime_seconds: metadata.ctime(),
        ctime_nanoseconds: metadata.ctime_nsec(),
    })
}

fn read_path(
    path: &Path,
    limit: usize,
    after_read: impl FnOnce() -> Result<(), Error>,
) -> Result<Option<Vec<u8>>, Error> {
    if limit > thegn_core::config_budget::MAX_SOURCE_BYTES {
        return Err(Error::TooLarge);
    }
    let Some(file) = open(path)? else {
        return Ok(None);
    };
    let expected = regular_identity(&file)?;
    let mut input = file;
    let mut bytes = Vec::new();
    let mut chunk = [0u8; 8192];
    loop {
        let count = input.read(&mut chunk).map_err(|_| Error::Unavailable)?;
        if count == 0 {
            break;
        }
        if bytes.len().checked_add(count).is_none_or(|n| n > limit) {
            return Err(Error::TooLarge);
        }
        bytes.extend_from_slice(&chunk[..count]);
    }
    if regular_identity(&input)? != expected {
        return Err(Error::Changed);
    }
    after_read()?;
    let Some(after) = open(path)? else {
        return Err(Error::Changed);
    };
    if regular_identity(&after)? != expected {
        return Err(Error::Changed);
    }
    Ok(Some(bytes))
}

pub(super) fn read(path: &Path, limit: usize) -> Result<Option<Vec<u8>>, Error> {
    read_path(path, limit, || Ok(()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn final_symlink_and_equal_length_replacement_are_refused() {
        let dir = tempfile::tempdir().unwrap();
        let original = dir.path().join("original");
        let replacement = dir.path().join("replacement");
        fs::write(&original, b"one").unwrap();
        fs::write(&replacement, b"two").unwrap();
        std::os::unix::fs::symlink(&original, dir.path().join("link")).unwrap();
        let error = read(&dir.path().join("link"), 64).unwrap_err();
        assert_eq!(error, Error::FinalLinkUnsupported);
        assert!(!error.to_string().contains("one"));
        assert_eq!(
            read_path(&original, 64, || {
                fs::rename(&original, dir.path().join("old")).unwrap();
                fs::rename(&replacement, &original).unwrap();
                Ok(())
            }),
            Err(Error::Changed)
        );
    }
}
