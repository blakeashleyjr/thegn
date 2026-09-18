//! Owned opened-file fixtures.  Unsupported filesystem namespaces exercise the
//! typed refusal path instead of being relabeled as positive coverage.

use super::*;
use std::ffi::CString;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::symlink;

fn fixture() -> Option<tempfile::TempDir> {
    let dir = tempfile::Builder::new()
        .prefix("thegn-config-capture-")
        .tempdir_in("/dev/shm")
        .unwrap_or_else(|error| panic!("Linux config capture fixture setup failed: {error}"));
    let canary = dir.path().join("canary");
    std::fs::write(&canary, b"canary").unwrap();
    match read(&canary, 64) {
        Ok(Some(bytes)) => {
            assert_eq!(bytes, b"canary");
            Some(dir)
        }
        Err(Error::Unavailable) => {
            eprintln!(
                "SKIP: Linux config capture fixture cannot exercise the supported /dev/shm tmpfs"
            );
            None
        }
        other => panic!("unexpected config fixture result: {other:?}"),
    }
}

#[test]
fn regular_files_and_symlink_targets_are_read_once_and_bounded() {
    let Some(dir) = fixture() else { return };
    let file = dir.path().join("config.toml");
    std::fs::write(&file, b"branch_prefix='captured/'\n").unwrap();
    symlink("config.toml", dir.path().join("relative")).unwrap();
    symlink(&file, dir.path().join("absolute")).unwrap();
    for path in [
        file,
        dir.path().join("relative"),
        dir.path().join("absolute"),
    ] {
        assert_eq!(
            read(&path, thegn_core::config_budget::MAX_SOURCE_BYTES),
            Ok(Some(b"branch_prefix='captured/'\n".to_vec()))
        );
    }
    assert_eq!(read(&dir.path().join("relative"), 4), Err(Error::TooLarge));
}

#[test]
fn missing_is_absent_but_fifo_and_directory_never_enter_data_read() {
    let Some(dir) = fixture() else { return };
    assert_eq!(read(&dir.path().join("missing"), 64), Ok(None));
    let fifo = dir.path().join("fifo");
    let name = CString::new(fifo.as_os_str().as_bytes()).unwrap();
    assert_eq!(unsafe { nix::libc::mkfifo(name.as_ptr(), 0o600) }, 0);
    assert_eq!(read(&fifo, 64), Err(Error::NonRegular));
    assert_eq!(read(dir.path(), 64), Err(Error::NonRegular));
}

#[test]
fn opened_identity_change_is_rejected_without_path_reread() {
    let Some(dir) = fixture() else { return };
    let original = dir.path().join("original");
    let replacement = dir.path().join("replacement");
    std::fs::write(&original, b"one").unwrap();
    std::fs::write(&replacement, b"two").unwrap();
    let metadata = metadata_target(&original).unwrap().unwrap();
    assert_eq!(
        read_target(metadata, 64, |_file| {
            std::fs::File::open(replacement).map_err(|_| Error::Unavailable)
        }),
        Err(Error::Changed)
    );
}

#[test]
fn equal_length_mutation_changes_observed_descriptor_time_identity() {
    let Some(dir) = fixture() else { return };
    let original = dir.path().join("same-length");
    std::fs::write(&original, b"one").unwrap();
    let metadata = metadata_target(&original).unwrap().unwrap();
    assert_eq!(
        read_target(metadata, 64, |file| {
            let readable = readable(file)?;
            // Keep the same inode and length. Linux mtime/ctime nanoseconds
            // are the observable mutation evidence; the old identity tuple
            // (dev, inode, length) incorrectly accepted this.
            std::fs::write(&original, b"two").unwrap();
            Ok(readable)
        }),
        Err(Error::Changed)
    );
}

#[test]
fn equal_size_path_replacement_is_rejected_after_descriptor_read() {
    let Some(dir) = fixture() else { return };
    let original = dir.path().join("original");
    let replacement = dir.path().join("replacement");
    std::fs::write(&original, b"one").unwrap();
    std::fs::write(&replacement, b"two").unwrap();
    let result = read_path(&original, 64, || {
        std::fs::rename(&original, dir.path().join("old")).unwrap();
        std::fs::rename(&replacement, &original).unwrap();
        Ok(())
    });
    assert_eq!(result, Err(Error::Changed));
}

#[test]
fn path_and_link_budgets_refuse_before_unbounded_allocation() {
    assert!(metadata_target(Path::new("relative")).is_err());
    let mut budget = Budget {
        bytes: 0,
        components: 0,
        links: 0,
    };
    assert!(budget.components(&vec![b'x'; MAX_PATH + 1]).is_err());
    let Some(dir) = fixture() else { return };
    symlink("cycle", dir.path().join("cycle")).unwrap();
    assert!(metadata_target(&dir.path().join("cycle")).is_err());
}
