//! Open a regular file beneath a pinned root without following path components.

use std::fs::File;
use std::io;
use std::path::{Component, Path};

/// Open a regular file beneath `root`, refusing absolute paths and traversal.
///
/// Unix walks with `openat` and `O_NOFOLLOW` for every component. Other
/// platforms retain the root-relative path check and the platform helper's
/// final-component protection, which is weaker than the Unix walk.
pub(crate) fn open_regular_beneath(root: &Path, rel: &Path) -> io::Result<File> {
    validate_relative(rel)?;
    #[cfg(unix)]
    {
        open_unix(root, rel)
    }
    #[cfg(not(unix))]
    {
        let path = root.join(rel);
        let file = crate::platform::open_read_nonblocking_nofollow(&path)?;
        if !file.metadata()?.is_file() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "not a regular file",
            ));
        }
        Ok(file)
    }
}

fn validate_relative(rel: &Path) -> io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        if rel
            .as_os_str()
            .as_bytes()
            .split(|byte| *byte == b'/')
            .any(|part| part.is_empty() || part == b"." || part == b"..")
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "path must contain only normal relative components",
            ));
        }
    }
    #[cfg(not(unix))]
    {
        if rel
            .to_string_lossy()
            .split(|character| character == '/' || character == '\\')
            .any(|part| part == "." || part == "..")
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "path must contain only normal relative components",
            ));
        }
    }
    if rel.as_os_str().is_empty()
        || rel.is_absolute()
        || rel
            .components()
            .any(|component| !matches!(component, Component::Normal(_)))
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "path must contain only normal relative components",
        ));
    }
    Ok(())
}

#[cfg(unix)]
fn open_unix(root: &Path, rel: &Path) -> io::Result<File> {
    use std::ffi::CString;
    use std::os::fd::{AsRawFd, FromRawFd};
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::fs::OpenOptionsExt;

    let mut parent = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_CLOEXEC | libc::O_DIRECTORY | libc::O_NOFOLLOW)
        .open(root)?;
    let parts = rel.components().collect::<Vec<_>>();
    for component in &parts[..parts.len() - 1] {
        let Component::Normal(name) = component else {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "path must contain only normal relative components",
            ));
        };
        let name = CString::new(name.as_bytes()).map_err(io::Error::other)?;
        let flags = libc::O_RDONLY
            | libc::O_DIRECTORY
            | libc::O_NOFOLLOW
            | libc::O_CLOEXEC
            | libc::O_NONBLOCK;
        // SAFETY: parent remains alive, and name is one NUL-terminated component.
        let fd = unsafe { libc::openat(parent.as_raw_fd(), name.as_ptr(), flags) };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: successful openat returned a new, exclusively owned descriptor.
        parent = unsafe { File::from_raw_fd(fd) };
    }

    let Component::Normal(name) = parts[parts.len() - 1] else {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "path must end in a normal component",
        ));
    };
    let name = CString::new(name.as_bytes()).map_err(io::Error::other)?;
    let flags =
        libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_NOCTTY | libc::O_CLOEXEC;
    // SAFETY: parent remains alive, and name is one NUL-terminated component.
    let fd = unsafe { libc::openat(parent.as_raw_fd(), name.as_ptr(), flags) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: successful openat returned a new, exclusively owned descriptor.
    let file = unsafe { File::from_raw_fd(fd) };
    if !file.metadata()?.is_file() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "not a regular file",
        ));
    }
    Ok(file)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_non_relative_and_empty_paths() {
        let root = tempfile::tempdir().unwrap();
        for rel in [Path::new(""), Path::new("../secret"), Path::new("/secret")] {
            assert_eq!(
                open_regular_beneath(root.path(), rel).unwrap_err().kind(),
                io::ErrorKind::InvalidInput
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn rejects_dot_components() {
        let root = tempfile::tempdir().unwrap();
        assert_eq!(
            open_regular_beneath(root.path(), Path::new("sub/./secret"))
                .unwrap_err()
                .kind(),
            io::ErrorKind::InvalidInput
        );
    }
}
