use super::{DirectoryFs, DirectoryListing, MAX_CONTAINER_SCOPES, discover};
use std::collections::HashMap;
use std::ffi::OsString;
use std::io;
use std::path::{Path, PathBuf};

#[derive(Default)]
struct FakeDirs(HashMap<PathBuf, Vec<OsString>>);

impl DirectoryFs for FakeDirs {
    fn directories(&self, path: &Path) -> io::Result<DirectoryListing> {
        self.0
            .get(path)
            .cloned()
            .map(|directories| DirectoryListing {
                directories,
                truncated: false,
            })
            .ok_or_else(|| io::Error::from(io::ErrorKind::NotFound))
    }
}

fn put(fs: &mut FakeDirs, path: &str, names: &[&str]) {
    fs.0.insert(
        PathBuf::from(path),
        names.iter().map(OsString::from).collect(),
    );
}

#[test]
fn discovers_rootful_docker_podman_and_rootless_scopes_only() {
    let id_docker = "docker-111111.scope";
    let id_machine = "libpod-222222.scope";
    let id_rootless = "libpod-333333.scope";
    let mut fs = FakeDirs::default();
    put(
        &mut fs,
        "/cg/system.slice",
        &[id_docker, "init.scope", "session-1.scope"],
    );
    put(
        &mut fs,
        "/cg/machine.slice",
        &[id_machine, "libpod-conmon-1.scope"],
    );
    put(
        &mut fs,
        "/cg/user.slice",
        &["user-1000.slice", "user-1001.slice"],
    );
    put(
        &mut fs,
        "/cg/user.slice/user-1000.slice",
        &["user@1000.service"],
    );
    put(
        &mut fs,
        "/cg/user.slice/user-1000.slice/user@1000.service",
        &["user.slice", "app.slice"],
    );
    put(
        &mut fs,
        "/cg/user.slice/user-1000.slice/user@1000.service/user.slice",
        &[id_rootless],
    );
    put(
        &mut fs,
        "/cg/user.slice/user-1000.slice/user@1000.service/app.slice",
        &["docker-444444.scope", "app-555555.scope"],
    );
    put(
        &mut fs,
        "/cg/user.slice/user-1001.slice",
        &["unrelated.service"],
    );

    let found = discover(&fs, Path::new("/cg"));
    let rows: Vec<_> = found.scopes.iter().map(|scope| scope.id.as_ref()).collect();
    assert_eq!(rows, vec!["111111", "222222", "333333", "444444"]);
    assert!(!found.truncated);
}

#[test]
fn scope_cap_marks_truncation() {
    let names: Vec<String> = (0..MAX_CONTAINER_SCOPES + 1)
        .map(|index| format!("docker-{index}.scope"))
        .collect();
    let names_ref: Vec<&str> = names.iter().map(String::as_str).collect();
    let mut fs = FakeDirs::default();
    put(&mut fs, "/cg/system.slice", &names_ref);

    let found = discover(&fs, Path::new("/cg"));
    assert_eq!(found.scopes.len(), MAX_CONTAINER_SCOPES);
    assert!(found.truncated);
}

#[test]
fn more_than_the_directory_cap_reports_truncation() {
    let names: Vec<String> = (0..MAX_CONTAINER_SCOPES * 2 + 1)
        .map(|index| format!("unit-{index}.service"))
        .collect();
    let names_ref: Vec<&str> = names.iter().map(String::as_str).collect();
    let mut fs = FakeDirs::default();
    put(&mut fs, "/cg/system.slice", &names_ref);

    let found = discover(&fs, Path::new("/cg"));
    assert!(found.scopes.is_empty());
    assert!(found.truncated);
}

struct DeniedDirs;

impl DirectoryFs for DeniedDirs {
    fn directories(&self, _path: &Path) -> io::Result<DirectoryListing> {
        Err(io::Error::from(io::ErrorKind::PermissionDenied))
    }
}

#[test]
fn listing_errors_are_counted() {
    let found = discover(&DeniedDirs, Path::new("/cg"));
    assert_eq!(found.list_errors, 3);
}
