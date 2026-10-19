use super::{
    DirectoryFs, DirectoryListing, MAX_CONTAINER_SCOPES, MAX_DIRECTORY_LISTINGS, discover,
    discover_for_uid,
};
use std::cell::Cell;
use std::collections::HashMap;
use std::ffi::OsString;
use std::io;
use std::path::{Path, PathBuf};

#[derive(Default)]
struct FakeDirs {
    listings: Cell<usize>,
    directories: HashMap<PathBuf, Vec<OsString>>,
}

impl DirectoryFs for FakeDirs {
    fn directories(&self, path: &Path) -> io::Result<DirectoryListing> {
        self.listings.set(self.listings.get() + 1);
        self.directories
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
    fs.directories.insert(
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

    let found = discover_for_uid(&fs, Path::new("/cg"), 65_534);
    let rows: Vec<_> = found.scopes.iter().map(|scope| scope.id.as_ref()).collect();
    assert_eq!(rows, vec!["111111", "222222", "333333", "444444"]);
    assert!(!found.truncated);
    assert_eq!(fs.listings.get(), 7);
}

#[test]
fn scope_cap_marks_truncation() {
    let names: Vec<String> = (0..MAX_CONTAINER_SCOPES + 1)
        .map(|index| format!("docker-{index}.scope"))
        .collect();
    let names_ref: Vec<&str> = names.iter().map(String::as_str).collect();
    let mut fs = FakeDirs::default();
    put(&mut fs, "/cg/system.slice", &names_ref);

    let found = discover_for_uid(&fs, Path::new("/cg"), 65_534);
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

    let found = discover_for_uid(&fs, Path::new("/cg"), 65_534);
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
    let found = discover_for_uid(&DeniedDirs, Path::new("/cg"), 65_534);
    assert_eq!(found.list_errors, 3);
}

#[test]
fn user_scope_walk_is_bounded_and_keeps_the_first_sorted_users() {
    assert_eq!(MAX_DIRECTORY_LISTINGS, 323);

    let mut fs = FakeDirs::default();
    let users: Vec<_> = (0..100)
        .map(|index| format!("user-{index:04}.slice"))
        .collect();
    let user_refs: Vec<_> = users.iter().map(String::as_str).collect();
    put(&mut fs, "/cg/user.slice", &user_refs);

    for index in 0..100 {
        let user = format!("user-{index:04}.slice");
        let services: Vec<_> = (0..3)
            .map(|service| format!("user@{index:04}-{service}.service"))
            .collect();
        let service_refs: Vec<_> = services.iter().map(String::as_str).collect();
        let user_path = format!("/cg/user.slice/{user}");
        put(&mut fs, &user_path, &service_refs);
        for (service_index, service) in services.iter().enumerate() {
            let service_path = format!("{user_path}/{service}");
            let app_path = format!("{service_path}/app.slice");
            if service_index == 0 {
                let scope = format!("docker-{index:04}-0.scope");
                put(&mut fs, &app_path, &[&scope]);
            } else {
                put(&mut fs, &app_path, &[]);
            }
        }
    }

    let found = discover_for_uid(&fs, Path::new("/cg"), 9999);
    assert_eq!(
        fs.listings.get(),
        MAX_DIRECTORY_LISTINGS,
        "3 + 64 x (1 + 2 x 2) = 323"
    );
    assert!(found.truncated);
    let expected: Vec<_> = (0..64).map(|index| format!("{index:04}-0")).collect();
    let actual: Vec<_> = found
        .scopes
        .iter()
        .map(|scope| scope.id.to_string())
        .collect();
    assert_eq!(actual, expected);
}

#[test]
fn too_many_user_services_reports_truncation() {
    let mut fs = FakeDirs::default();
    put(&mut fs, "/cg/user.slice", &["user-1000.slice"]);
    put(
        &mut fs,
        "/cg/user.slice/user-1000.slice",
        &[
            "user@1000-a.service",
            "user@1000-b.service",
            "user@1000-c.service",
        ],
    );
    for service in ["user@1000-a.service", "user@1000-b.service"] {
        let path = format!("/cg/user.slice/user-1000.slice/{service}");
        let app_path = format!("{path}/app.slice");
        put(&mut fs, &app_path, &["docker-1000.scope"]);
    }

    let found = discover_for_uid(&fs, Path::new("/cg"), 9999);
    assert!(found.truncated);
    assert_eq!(found.scopes.len(), 2);
}

#[test]
fn current_users_slice_is_walked_before_the_sorted_user_cap() {
    let mut fs = FakeDirs::default();
    let mut users: Vec<_> = (0..64).map(|uid| format!("user-{uid}.slice")).collect();
    users.push("user-999.slice".to_owned());
    let user_refs: Vec<_> = users.iter().map(String::as_str).collect();
    put(&mut fs, "/cg/user.slice", &user_refs);

    for uid in (0..64).chain(std::iter::once(999)) {
        let user = format!("user-{uid}.slice");
        let user_path = format!("/cg/user.slice/{user}");
        put(&mut fs, &user_path, &["user@session.service"]);
        let service_path = format!("{user_path}/user@session.service");
        put(&mut fs, &service_path, &["user.slice", "app.slice"]);
        let app_path = format!("{service_path}/app.slice");
        let scope = format!("docker-{uid}.scope");
        put(&mut fs, &app_path, &[&scope]);
    }

    let found = discover_for_uid(&fs, Path::new("/cg"), 999);
    assert!(found.truncated);
    assert_eq!(
        found.scopes.first().map(|scope| scope.id.as_ref()),
        Some("999")
    );
    let ids: Vec<_> = found.scopes.iter().map(|scope| scope.id.as_ref()).collect();
    assert!(ids.contains(&"0"));
    assert!(!ids.contains(&"9"));
    assert!(ids[1..].windows(2).all(|pair| pair[0] < pair[1]));
}

#[test]
fn discover_uses_the_current_users_slice() {
    let uid = nix::unistd::geteuid().as_raw();
    let user = format!("user-{uid}.slice");
    let user_path = format!("/cg/user.slice/{user}");
    let service_path = format!("{user_path}/user@session.service");
    let app_path = format!("{service_path}/app.slice");
    let mut fs = FakeDirs::default();
    put(&mut fs, "/cg/user.slice", &[&user]);
    put(&mut fs, &user_path, &["user@session.service"]);
    put(&mut fs, &app_path, &["docker-current.scope"]);

    let found = discover(&fs, Path::new("/cg"));
    assert_eq!(found.scopes.len(), 1);
    assert_eq!(found.scopes[0].id.as_ref(), "current");
}
