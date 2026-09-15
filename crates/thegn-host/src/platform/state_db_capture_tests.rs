//! Root-injected namespace/SQL fixtures, not proof of a production /tmp route.
//! Every fixture verifies its actual supported local filesystem, never an injected class.
use super::*;
use rusqlite::Connection;
use std::os::unix::{
    fs::{PermissionsExt, symlink},
    net::UnixListener,
};
use thegn_core::{
    host_db_capture::HostCaptureReadError, host_definition_snapshot::HostDefinitionReadError,
};

fn directory() -> tempfile::TempDir {
    let dir = tempfile::Builder::new()
        .prefix("thegn-603-")
        .tempdir()
        .unwrap();
    std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    let file = open_at(libc::AT_FDCWD, dir.path().as_os_str()).unwrap();
    assert!(
        matches!(
            filesystem(&file).unwrap(),
            0xef53 | 0x5846_5342 | 0x9123_683e | 0x0102_1994
        ),
        "native fixture needs an actual supported local filesystem"
    );
    dir
}

fn file(path: &Path) {
    std::fs::write(path, []).unwrap();
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).unwrap();
}

fn database(path: &Path) -> Connection {
    file(path);
    let conn = Connection::open(path).unwrap();
    conn.execute_batch(&format!("PRAGMA user_version={}; CREATE TABLE hosts(host_id TEXT PRIMARY KEY,name TEXT,config_json TEXT);", thegn_core::db::SCHEMA_VERSION)).unwrap();
    conn
}

#[test]
fn actual_filesystem_classification_is_not_a_test_injected_label() {
    let dir = directory();
    let accepted = open_at(libc::AT_FDCWD, dir.path().as_os_str()).unwrap();
    assert!(inspect(&accepted, true, unsafe { libc::geteuid() }).is_ok());
    let rejected = open_at(libc::AT_FDCWD, OsStr::new("/proc")).unwrap();
    assert!(!supported_filesystem(filesystem(&rejected).unwrap()));
    assert_eq!(
        inspect(&rejected, true, unsafe { libc::geteuid() }).unwrap_err(),
        Error::Unsupported
    );
    for kind in [
        0x6969,
        0xff53_4d42,
        0x6573_5546,
        0x0102_1997,
        0x794c_7630,
        0,
    ] {
        assert!(!supported_filesystem(kind));
    }
    dir.close().expect("owned filesystem fixture cleanup");
}

#[test]
fn ownership_and_io_classification_do_not_launder_other_users_or_errors() {
    assert!(private_owner_mode(1000, 0o700, 1000));
    assert!(private_owner_mode(0, 0o755, 1000));
    assert!(!private_owner_mode(2000, 0o700, 1000));
    for mode in [0o770, 0o702, 0o1777] {
        assert!(!private_owner_mode(1000, mode, 1000));
    }
    for code in [libc::EACCES, libc::EIO, libc::ENOENT] {
        assert_eq!(
            io_error(std::io::Error::from_raw_os_error(code)),
            Error::Unavailable
        );
    }
}

#[test]
fn missing_only_is_absent_and_does_not_create_parents() {
    let dir = directory();
    for path in [
        dir.path().join("missing.db"),
        dir.path().join("missing/child.db"),
    ] {
        assert!(matches!(
            capture_at(&path, dir.path(), || Ok(())),
            Ok(Capture::Absent)
        ));
        assert!(!path.exists());
    }
    assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0);
    dir.close().expect("owned absence fixture cleanup");
}

#[test]
fn raw_traversal_relative_uri_and_component_budgets_refuse() {
    let dir = directory();
    for path in [
        "relative.db".into(),
        "file:/db?immutable=1".into(),
        dir.path().join("a/../db"),
        dir.path().join("a/./db"),
        dir.path().join("a//db"),
        dir.path().join("line\nbreak"),
        dir.path().join("nul\0byte"),
        dir.path().join("db?immutable=1"),
        dir.path().join("db#fragment"),
        dir.path().join("x".repeat(4097)),
    ] {
        assert_eq!(
            relative_components(&path, dir.path()).unwrap_err(),
            Error::InvalidPath
        );
    }
    let too_deep = dir.path().join(vec!["a"; MAX_COMPONENTS + 1].join("/"));
    assert_eq!(
        relative_components(&too_deep, dir.path()).unwrap_err(),
        Error::InvalidPath
    );
    assert_eq!(
        relative_components(Path::new("/"), Path::new("/")).unwrap_err(),
        Error::InvalidPath
    );
    dir.close().expect("owned path-policy fixture cleanup");
}

#[test]
fn symlinks_and_static_special_files_never_reach_sqlite() {
    let dir = directory();
    let dangling = dir.path().join("dangling");
    symlink("missing", &dangling).unwrap();
    assert!(matches!(
        Observation::at(&dangling, dir.path()),
        Err(Error::Unsupported)
    ));
    let fifo = dir.path().join("fifo");
    let name = CString::new(fifo.as_os_str().as_bytes()).unwrap();
    assert_eq!(unsafe { libc::mkfifo(name.as_ptr(), 0o600) }, 0);
    assert!(matches!(
        Observation::at(&fifo, dir.path()),
        Err(Error::NonRegular)
    ));
    let socket = dir.path().join("socket");
    let _listener = UnixListener::bind(&socket).unwrap();
    assert!(matches!(
        Observation::at(&socket, dir.path()),
        Err(Error::NonRegular)
    ));
    assert!(matches!(
        Observation::at(&dir.path().join("fifo/db"), dir.path()),
        Err(Error::NonRegular)
    ));
    let subdir = dir.path().join("folder");
    std::fs::create_dir(&subdir).unwrap();
    assert!(matches!(
        Observation::at(&subdir, dir.path()),
        Err(Error::NonRegular)
    ));
    symlink(&subdir, dir.path().join("alias")).unwrap();
    assert!(matches!(
        Observation::at(&dir.path().join("alias/db"), dir.path()),
        Err(Error::Unsupported)
    ));
    // O_PATH only: inspect an actual device without opening it for device I/O.
    let device = open_at(libc::AT_FDCWD, OsStr::new("/dev/null")).unwrap();
    assert_eq!(
        inspect(&device, false, unsafe { libc::geteuid() }).unwrap_err(),
        Error::NonRegular
    );
    drop(_listener);
    dir.close().expect("owned special-file fixture cleanup");
}

#[test]
fn shared_modes_and_sidecar_aliases_refuse_without_contents_in_errors() {
    let dir = directory();
    let path = dir.path().join("private-secret.db");
    file(&path);
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o620)).unwrap();
    let error = capture_at(&path, dir.path(), || Ok(())).unwrap_err();
    assert_eq!(error, Error::Unsupported);
    assert!(!format!("{error:?}: {error}").contains("private-secret"));
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
    symlink("missing", dir.path().join("private-secret.db-shm")).unwrap();
    assert!(matches!(
        Observation::at(&path, dir.path()),
        Err(Error::Unsupported)
    ));
    std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o770)).unwrap();
    assert!(matches!(
        Observation::at(&path, dir.path()),
        Err(Error::Unsupported)
    ));
    std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    dir.close().expect("owned mode/alias fixture cleanup");
}

#[test]
fn rooted_real_wal_helper_captures_latest_definitions_and_typed_errors() {
    let dir = directory();
    let path = dir.path().join("state.db");
    let writer = database(&path);
    writer.execute_batch("PRAGMA journal_mode=WAL; PRAGMA wal_autocheckpoint=0; INSERT INTO hosts VALUES ('id','host','{}');").unwrap();
    let Capture::Present(snapshot) = capture_at(&path, dir.path(), || Ok(())).unwrap() else {
        panic!("existing database cannot be absent");
    };
    assert_eq!(snapshot.definitions()[0].0, "host");
    assert!(dir.path().join("state.db-wal").is_file());
    writer
        .execute_batch("UPDATE hosts SET config_json='{'")
        .unwrap();
    assert!(matches!(
        capture_at(&path, dir.path(), || Ok(())),
        Err(Error::Database(HostCaptureReadError::Definitions(
            HostDefinitionReadError::InvalidJson
        )))
    ));
    writer.execute_batch("PRAGMA user_version=999").unwrap();
    assert!(matches!(
        capture_at(&path, dir.path(), || Ok(())),
        Err(Error::Database(HostCaptureReadError::Definitions(
            HostDefinitionReadError::IncompatibleSchema { observed: 999, .. }
        )))
    ));
    drop(writer);
    dir.close().expect("owned WAL fixture cleanup");
}

#[test]
fn replacements_are_detectors_not_a_consumed_sqlite_inode_proof() {
    let dir = directory();
    let path = dir.path().join("state.db");
    file(&path);
    let error = capture_at(&path, dir.path(), || {
        std::fs::rename(&path, dir.path().join("old.db")).unwrap();
        file(&path);
        Ok(())
    })
    .unwrap_err();
    assert_eq!(error, Error::Changed);
    let parent = dir.path().join("parent");
    std::fs::create_dir(&parent).unwrap();
    let nested = parent.join("state.db");
    file(&nested);
    let observed = Observation::at(&nested, dir.path()).unwrap();
    std::fs::rename(&parent, dir.path().join("moved")).unwrap();
    std::fs::create_dir(&parent).unwrap();
    file(&nested);
    assert_eq!(observed.verify(), Err(Error::Changed));
    drop(observed);
    dir.close().expect("owned replacement fixture cleanup");
}

#[test]
fn existing_sidecar_disappearance_holds_but_new_regular_sidecar_is_checked() {
    let dir = directory();
    let path = dir.path().join("state.db");
    file(&path);
    let wal = dir.path().join("state.db-wal");
    file(&wal);
    let observed = Observation::at(&path, dir.path()).unwrap();
    std::fs::remove_file(&wal).unwrap();
    assert_eq!(observed.verify(), Err(Error::Changed));
    let observed = Observation::at(&path, dir.path()).unwrap();
    file(&wal);
    assert_eq!(observed.verify(), Ok(()));
    std::fs::set_permissions(&wal, std::fs::Permissions::from_mode(0o622)).unwrap();
    assert_eq!(observed.verify(), Err(Error::Unsupported));
    drop(observed);
    dir.close().expect("owned sidecar fixture cleanup");
}

#[test]
fn actual_post_read_guard_refuses_replacement_without_returning_copied_data() {
    let dir = directory();
    let path = dir.path().join("state.db");
    let writer = database(&path);
    drop(writer);
    let observed = Observation::at(&path, dir.path()).unwrap();
    let result = finish_capture(
        &path,
        observed,
        || Ok(()),
        |path| {
            let data = thegn_core::host_db_capture::capture_host_definitions_wal_at(path)?;
            std::fs::rename(path, dir.path().join("original.db")).unwrap();
            file(path);
            Ok(data)
        },
    );
    assert!(matches!(result, Err(Error::Changed)));
    dir.close().expect("owned post-read fixture cleanup");
}

#[test]
fn production_route_does_not_ignore_globally_writable_ancestors() {
    let dir = directory();
    let path = dir.path().join("missing.db");
    // The runner's private temp root is beneath /tmp. Production still walks
    // from literal '/' and cannot opt out of the globally writable ancestor.
    assert!(
        path.starts_with("/tmp"),
        "native fixture requires private /tmp custody"
    );
    let shared = open_at(libc::AT_FDCWD, OsStr::new("/tmp")).unwrap();
    assert!(shared.metadata().unwrap().mode() & 0o022 != 0);
    assert!(matches!(
        crate::state_host_capture::capture(&path),
        Err(Error::Unsupported)
    ));
    dir.close()
        .expect("owned production-refusal fixture cleanup");
}

// Permission restoration uses an ordinary descriptor acquired while the owned
// path is readable, not O_PATH (which cannot be used with fchmod).
struct RestorePermissions {
    file: File,
    permissions: std::fs::Permissions,
    restored: bool,
}
impl RestorePermissions {
    fn new(path: &Path) -> Self {
        let file = File::open(path).unwrap();
        let permissions = file.metadata().unwrap().permissions();
        Self {
            file,
            permissions,
            restored: false,
        }
    }
    fn set(&self, mode: u32) {
        self.file
            .set_permissions(std::fs::Permissions::from_mode(mode))
            .unwrap();
    }
    fn restore(&mut self) {
        self.file.set_permissions(self.permissions.clone()).unwrap();
        self.restored = true;
    }
}
impl Drop for RestorePermissions {
    fn drop(&mut self) {
        if !self.restored
            && let Err(error) = self.file.set_permissions(self.permissions.clone())
        {
            if !std::thread::panicking() {
                panic!("restore owned capture fixture permissions: {error}");
            }
            std::io::Write::write_fmt(
                &mut std::io::stderr(),
                format_args!(
                    "owned capture fixture permission restoration failed during unwind: {error}\n"
                ),
            )
            .unwrap_or(());
        }
    }
}

fn without_sql(
    path: &Path,
    root: &Path,
    after_inspection: impl FnOnce() -> Result<(), Error>,
) -> (Result<Capture, Error>, usize) {
    let reads = std::cell::Cell::new(0);
    let result = Observation::at(path, root).and_then(|observed| {
        finish_capture(path, observed, after_inspection, |_| {
            reads.set(reads.get() + 1);
            Err(HostCaptureReadError::Open)
        })
    });
    (result, reads.get())
}

#[test]
fn missing_base_and_missing_parent_creation_refuse_before_sql() {
    for create_parent in [false, true] {
        let dir = directory();
        let path = if create_parent {
            dir.path().join("new/state.db")
        } else {
            dir.path().join("state.db")
        };
        let reached = std::cell::Cell::new(false);
        let (result, reads) = without_sql(&path, dir.path(), || {
            reached.set(true);
            if create_parent {
                std::fs::create_dir(path.parent().unwrap()).unwrap();
            } else {
                drop(database(&path));
            }
            Ok(())
        });
        assert!(
            reached.get(),
            "missing-path publication must pass the rendezvous"
        );
        assert_eq!(reads, 0);
        assert!(matches!(result, Err(Error::Changed)));
        let next = capture_at(&path, dir.path(), || Ok(())).unwrap();
        if create_parent {
            assert!(matches!(next, Capture::Absent));
        } else {
            let Capture::Present(snapshot) = next else {
                panic!("new database must be present")
            };
            assert!(snapshot.definitions().is_empty());
        }
        dir.close().expect("owned creation-race fixture cleanup");
    }
}

#[test]
fn missing_ancestor_replacement_and_permission_change_hold() {
    for change_mode in [false, true] {
        let dir = directory();
        let parent = dir.path().join("parent");
        std::fs::create_dir(&parent).unwrap();
        std::fs::set_permissions(&parent, std::fs::Permissions::from_mode(0o700)).unwrap();
        let mut restore = RestorePermissions::new(&parent);
        let path = parent.join("missing.db");
        let reached = std::cell::Cell::new(false);
        let (result, reads) = without_sql(&path, dir.path(), || {
            reached.set(true);
            if change_mode {
                restore.set(0o500);
            } else {
                std::fs::rename(&parent, dir.path().join("original-parent")).unwrap();
                std::fs::create_dir(&parent).unwrap();
            }
            Ok(())
        });
        restore.restore();
        assert!(reached.get());
        assert_eq!(reads, 0);
        assert!(matches!(result, Err(Error::Changed)));
        assert!(matches!(
            capture_at(&path, dir.path(), || Ok(())),
            Ok(Capture::Absent)
        ));
        drop(restore);
        dir.close().expect("owned ancestor-race fixture cleanup");
    }
}

#[test]
fn orphan_sidecars_preclude_absence_before_and_after_observation() {
    for suffix in SIDECARS {
        for late in [false, true] {
            let dir = directory();
            let path = dir.path().join("private-state.db");
            let orphan = dir.path().join(format!("private-state.db{suffix}"));
            if !late {
                std::fs::write(&orphan, b"owned orphan bytes").unwrap();
            }
            let reached = std::cell::Cell::new(false);
            let (result, reads) = without_sql(&path, dir.path(), || {
                reached.set(true);
                if late {
                    std::fs::write(&orphan, b"owned orphan bytes").unwrap();
                }
                Ok(())
            });
            assert!(reached.get());
            assert_eq!(reads, 0);
            let error = result.unwrap_err();
            assert_eq!(error, Error::OrphanedSidecar);
            assert!(!format!("{error:?}: {error}").contains("private-state"));
            assert!(!path.exists());
            assert_eq!(std::fs::read(&orphan).unwrap(), b"owned orphan bytes");
            assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1);
            dir.close().expect("owned orphan fixture cleanup");
        }
    }
    for special in [false, true] {
        let dir = directory();
        let path = dir.path().join("state.db");
        let orphan = dir.path().join("state.db-shm");
        if special {
            let name = CString::new(orphan.as_os_str().as_bytes()).unwrap();
            assert_eq!(unsafe { libc::mkfifo(name.as_ptr(), 0o600) }, 0);
        } else {
            symlink("absent-target", &orphan).unwrap();
        }
        let (result, reads) = without_sql(&path, dir.path(), || Ok(()));
        assert_eq!(reads, 0);
        assert!(matches!(result, Err(Error::OrphanedSidecar)));
        assert!(!path.exists());
        dir.close().expect("owned special-orphan cleanup");
    }
}

#[test]
fn unreadable_parent_and_database_hold_then_recover_after_permission_restore() {
    assert_ne!(
        unsafe { libc::geteuid() },
        0,
        "native permission coverage requires an unprivileged runner"
    );
    for deny_parent in [false, true] {
        let dir = directory();
        let parent = dir.path().join("parent");
        std::fs::create_dir(&parent).unwrap();
        let path = parent.join("state.db");
        drop(database(&path));
        let before = std::fs::read(&path).unwrap();
        let target = if deny_parent { &parent } else { &path };
        let mut restore = RestorePermissions::new(target);
        restore.set(0);
        let result = capture_at(&path, dir.path(), || Ok(()));
        restore.restore();
        let error = result.unwrap_err();
        if deny_parent {
            assert_eq!(error, Error::Unavailable);
        } else {
            assert_eq!(error, Error::Database(HostCaptureReadError::Open));
        }
        let Capture::Present(snapshot) = capture_at(&path, dir.path(), || Ok(())).unwrap() else {
            panic!("restored supported empty database must be present");
        };
        assert!(snapshot.definitions().is_empty());
        assert_eq!(std::fs::read(&path).unwrap(), before);
        assert_eq!(std::fs::read_dir(&parent).unwrap().count(), 1);
        drop(restore);
        dir.close().expect("owned permission fixture cleanup");
    }
}

#[test]
fn permission_restore_guard_survives_assertion_unwind() {
    let dir = directory();
    let path = dir.path().join("owned.txt");
    file(&path);
    let mode = std::fs::metadata(&path).unwrap().mode();
    let unwind = std::panic::catch_unwind(|| {
        let restore = RestorePermissions::new(&path);
        restore.set(0);
        panic!("injected assertion after owned chmod");
    });
    assert!(unwind.is_err());
    assert_eq!(std::fs::metadata(&path).unwrap().mode(), mode);
    assert_eq!(std::fs::read(&path).unwrap(), b"");
    dir.close().expect("owned unwind fixture cleanup");
}
