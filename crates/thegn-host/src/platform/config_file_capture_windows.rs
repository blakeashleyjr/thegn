use std::fs::{self, File, OpenOptions};
use std::io::Read;
use std::os::windows::fs::OpenOptionsExt;
use std::path::Path;

use crate::config_capture::ConfigFileReadError as Error;

// Win32 FILE_FLAG_OPEN_REPARSE_POINT: do not traverse a final reparse point
// while opening the selected source.  Ancestor custody remains the documented
// stable-namespace assumption, as on the Unix adapters.
const FILE_FLAG_OPEN_REPARSE_POINT: u32 = 0x0020_0000;

fn identity(metadata: &fs::Metadata) -> Result<(u64, Option<std::time::SystemTime>), Error> {
    if metadata.file_type().is_symlink() {
        return Err(Error::Unsupported);
    }
    if !metadata.is_file() {
        return Err(Error::NonRegular);
    }
    Ok((metadata.len(), metadata.modified().ok()))
}

fn open(path: &Path) -> Result<Option<File>, Error> {
    let before = match fs::symlink_metadata(path) {
        Ok(metadata) => Some(identity(&metadata)?),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(_) => return Err(Error::Unavailable),
    };
    let Some(before) = before else {
        return Ok(None);
    };
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT)
        .open(path)
        .map_err(|_| Error::Unavailable)?;
    if identity(&file.metadata().map_err(|_| Error::Unavailable)?)? != before {
        return Err(Error::Changed);
    }
    Ok(Some(file))
}

pub(super) fn read(path: &Path, limit: usize) -> Result<Option<Vec<u8>>, Error> {
    if limit > thegn_core::config_budget::MAX_SOURCE_BYTES {
        return Err(Error::TooLarge);
    }
    let Some(mut file) = open(path)? else {
        return Ok(None);
    };
    let expected = identity(&file.metadata().map_err(|_| Error::Unavailable)?)?;
    let mut bytes = Vec::new();
    let mut chunk = [0u8; 8192];
    loop {
        let count = file.read(&mut chunk).map_err(|_| Error::Unavailable)?;
        if count == 0 {
            break;
        }
        if bytes.len().checked_add(count).is_none_or(|n| n > limit) {
            return Err(Error::TooLarge);
        }
        bytes.extend_from_slice(&chunk[..count]);
    }
    if identity(&file.metadata().map_err(|_| Error::Unavailable)?)? != expected {
        return Err(Error::Changed);
    }
    if let Ok(after) = fs::symlink_metadata(path) {
        if identity(&after)? != expected {
            return Err(Error::Changed);
        }
    } else {
        return Err(Error::Changed);
    }
    Ok(Some(bytes))
}
