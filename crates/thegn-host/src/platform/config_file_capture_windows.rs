use std::fs::{self, File};
use std::io::Read;
use std::path::Path;

use crate::config_capture::ConfigFileReadError as Error;

// Win32 FILE_FLAG_OPEN_REPARSE_POINT: do not traverse a final reparse point
// while opening the selected source.  Ancestor custody remains the documented
// stable-namespace assumption, as on the Unix adapters.
const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x0400;

// Handle identity and the final path recheck assume a stable, owner-controlled
// namespace; they do not claim protection from hostile same-UID replacement.
fn identity(file: &File) -> Result<(u32, u32, u32, u64), Error> {
    let metadata = file.metadata().map_err(|_| Error::Unavailable)?;
    if !metadata.is_file() {
        return Err(Error::NonRegular);
    }
    if crate::platform::handle_file_attributes(file).map_err(|_| Error::Unavailable)?
        & FILE_ATTRIBUTE_REPARSE_POINT
        != 0
    {
        // Reject every final reparse point. No unreviewed tag is treated as a
        // regular config-file symlink policy.
        return Err(Error::Unsupported);
    }
    let (volume, high, low) =
        crate::platform::handle_identity(file).map_err(|_| Error::Unavailable)?;
    Ok((volume, high, low, metadata.len()))
}

fn open(path: &Path) -> Result<Option<File>, Error> {
    let present = match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() => return Err(Error::Unsupported),
        Ok(metadata) if !metadata.is_file() => return Err(Error::NonRegular),
        Ok(_) => true,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => false,
        Err(_) => return Err(Error::Unavailable),
    };
    if !present {
        return Ok(None);
    }
    crate::platform::open_nofollow(path)
        .map(Some)
        .map_err(|_| Error::Unavailable)
}

fn read_path(
    path: &Path,
    limit: usize,
    after_read: impl FnOnce() -> Result<(), Error>,
) -> Result<Option<Vec<u8>>, Error> {
    if limit > thegn_core::config_budget::MAX_SOURCE_BYTES {
        return Err(Error::TooLarge);
    }
    let Some(mut file) = open(path)? else {
        return Ok(None);
    };
    let expected = identity(&file)?;
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
    if identity(&file)? != expected {
        return Err(Error::Changed);
    }
    after_read()?;
    let Some(after) = open(path)? else {
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
mod tests {
    use super::*;

    #[test]
    fn owned_handle_identity_rejects_equal_size_replacement() {
        let dir = tempfile::tempdir().unwrap();
        let original = dir.path().join("original");
        let replacement = dir.path().join("replacement");
        fs::write(&original, b"one").unwrap();
        fs::write(&replacement, b"two").unwrap();
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
