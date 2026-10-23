//! No-replace directory promotion where the host platform provides it.

/// Rename a directory without replacing an existing destination when supported.
///
/// Linux glibc uses `renameat2(RENAME_NOREPLACE)` atomically. The fallback is
/// check-then-rename and is not atomic.
pub(crate) fn rename_dir_noreplace(
    from: &std::path::Path,
    to: &std::path::Path,
) -> std::io::Result<()> {
    #[cfg(all(target_os = "linux", target_env = "gnu"))]
    {
        match nix::fcntl::renameat2(
            nix::fcntl::AT_FDCWD,
            from,
            nix::fcntl::AT_FDCWD,
            to,
            nix::fcntl::RenameFlags::RENAME_NOREPLACE,
        ) {
            Ok(()) => return Ok(()),
            Err(nix::errno::Errno::EEXIST) => return Err(destination_exists()),
            Err(nix::errno::Errno::EINVAL | nix::errno::Errno::ENOSYS) => {
                tracing::debug!(
                    source = %from.display(),
                    destination = %to.display(),
                    "atomic no-replace directory rename unavailable; using fallback"
                );
            }
            Err(error) => return Err(std::io::Error::from(error)),
        }
    }

    match std::fs::symlink_metadata(to) {
        Ok(_) => Err(destination_exists()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => std::fs::rename(from, to),
        Err(error) => Err(error),
    }
}

fn destination_exists() -> std::io::Error {
    std::io::Error::new(std::io::ErrorKind::AlreadyExists, "destination exists")
}

#[cfg(test)]
mod tests {
    use super::rename_dir_noreplace;

    #[test]
    fn rename_dir_noreplace_refuses_an_existing_empty_directory() {
        let temp = tempfile::tempdir().unwrap();
        let from = temp.path().join("src");
        let to = temp.path().join("dst");
        std::fs::create_dir(&from).unwrap();
        std::fs::write(from.join("file"), b"source").unwrap();
        std::fs::create_dir(&to).unwrap();

        let result = rename_dir_noreplace(&from, &to);

        assert!(matches!(
            result,
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists
        ));
        assert_eq!(std::fs::read(from.join("file")).unwrap(), b"source");
        assert!(to.is_dir());
        assert_eq!(std::fs::read_dir(&to).unwrap().count(), 0);
    }

    #[test]
    fn rename_dir_noreplace_moves_into_an_absent_destination() {
        let temp = tempfile::tempdir().unwrap();
        let from = temp.path().join("src");
        let to = temp.path().join("dst");
        std::fs::create_dir(&from).unwrap();
        std::fs::write(from.join("file"), b"source").unwrap();

        rename_dir_noreplace(&from, &to).unwrap();

        assert!(!from.exists());
        assert_eq!(std::fs::read(to.join("file")).unwrap(), b"source");
    }
}
