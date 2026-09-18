use std::ffi::CString;
use std::fs::File;
use std::io::Read;
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::ffi::OsStrExt;
use std::path::Path;

use nix::libc;

use crate::config_capture::ConfigFileReadError as Error;

fn open(path: &Path) -> Result<Option<File>, Error> {
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
            Some(libc::ELOOP) => Err(Error::Unsupported),
            _ => Err(Error::Unavailable),
        };
    }
    Ok(Some(unsafe { File::from_raw_fd(fd) }))
}

fn regular_identity(file: &File) -> Result<(u64, u64, u64), Error> {
    let mut stat = std::mem::MaybeUninit::<libc::stat>::uninit();
    if unsafe { libc::fstat(file.as_raw_fd(), stat.as_mut_ptr()) } != 0 {
        return Err(Error::Unavailable);
    }
    let stat = unsafe { stat.assume_init() };
    if (stat.st_mode & libc::S_IFMT) != libc::S_IFREG {
        return Err(Error::NonRegular);
    }
    Ok((stat.st_dev as u64, stat.st_ino as u64, stat.st_size as u64))
}

pub(super) fn read(path: &Path, limit: usize) -> Result<Option<Vec<u8>>, Error> {
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
    Ok(Some(bytes))
}
