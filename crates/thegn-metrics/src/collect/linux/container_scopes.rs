//! Bounded, engine-free discovery of container cgroups from the cgroup v2 tree.

use std::ffi::OsString;
use std::io;
use std::path::{Path, PathBuf};

use super::containers_stats::ResolvedContainerScope;
use crate::collect::policy::cgroup_classify::{OwnerKind, classify};

pub const MAX_CONTAINER_SCOPES: usize = 256;
const MAX_DIRECTORY_LISTINGS: usize = 1024;
const MAX_DIRECTORY_ENTRIES: usize = 512;

pub trait DirectoryFs {
    /// Return direct child directory names. Implementations must not follow symlinks.
    fn directories(&self, path: &Path) -> io::Result<Vec<OsString>>;
}

#[derive(Debug, Default)]
pub struct RealDirectoryFs;

impl DirectoryFs for RealDirectoryFs {
    fn directories(&self, path: &Path) -> io::Result<Vec<OsString>> {
        if !std::fs::symlink_metadata(path)?.file_type().is_dir() {
            return Ok(Vec::new());
        }
        let mut names = Vec::new();
        for entry in std::fs::read_dir(path)?.take(MAX_DIRECTORY_ENTRIES + 1) {
            let entry = entry?;
            if entry.file_type()?.is_dir() {
                names.push(entry.file_name());
            }
        }
        Ok(names)
    }
}

#[derive(Debug, Default)]
pub struct Discovery {
    pub scopes: Vec<ResolvedContainerScope>,
    pub truncated: bool,
}

pub fn discover(fs: &dyn DirectoryFs, root: &Path) -> Discovery {
    let mut discovery = Discovery {
        scopes: Vec::with_capacity(MAX_CONTAINER_SCOPES),
        truncated: false,
    };
    let mut listings = 0;
    scan_scope_parent(
        fs,
        root,
        &root.join("system.slice"),
        &mut discovery,
        &mut listings,
    );
    scan_scope_parent(
        fs,
        root,
        &root.join("machine.slice"),
        &mut discovery,
        &mut listings,
    );

    let user_slice = root.join("user.slice");
    for user in list(fs, &user_slice, &mut listings, &mut discovery) {
        if !starts_with(&user, "user-") || !ends_with(&user, ".slice") {
            continue;
        }
        let user_slice_path = user_slice.join(user);
        for service in list(fs, &user_slice_path, &mut listings, &mut discovery) {
            if !starts_with(&service, "user@") || !ends_with(&service, ".service") {
                continue;
            }
            let service_path = user_slice_path.join(service);
            for parent in [
                service_path.join("user.slice"),
                service_path.join("app.slice"),
            ] {
                scan_scope_parent(fs, root, &parent, &mut discovery, &mut listings);
            }
        }
    }
    discovery
}

fn list(
    fs: &dyn DirectoryFs,
    path: &Path,
    listings: &mut usize,
    discovery: &mut Discovery,
) -> Vec<OsString> {
    if *listings >= MAX_DIRECTORY_LISTINGS {
        discovery.truncated = true;
        return Vec::new();
    }
    *listings += 1;
    match fs.directories(path) {
        Ok(mut directories) => {
            directories.sort_unstable();
            if directories.len() > MAX_DIRECTORY_ENTRIES {
                discovery.truncated = true;
                directories.truncate(MAX_DIRECTORY_ENTRIES);
            }
            directories
        }
        Err(_) => Vec::new(),
    }
}

fn scan_scope_parent(
    fs: &dyn DirectoryFs,
    root: &Path,
    parent: &Path,
    discovery: &mut Discovery,
    listings: &mut usize,
) {
    for name in list(fs, parent, listings, discovery) {
        let Some(name_str) = name.to_str() else {
            continue;
        };
        if !(name_str.starts_with("docker-") || name_str.starts_with("libpod-"))
            || !name_str.ends_with(".scope")
        {
            continue;
        }
        let relative = parent.strip_prefix(root).unwrap_or(parent).join(name_str);
        let classification = classify(name_str.as_bytes());
        if !matches!(classification.kind, OwnerKind::Docker | OwnerKind::Podman) {
            continue;
        }
        let id = String::from_utf8_lossy(classification.name).into_owned();
        if id.is_empty() {
            continue;
        }
        if discovery.scopes.len() == MAX_CONTAINER_SCOPES {
            discovery.truncated = true;
            return;
        }
        if let Some(scope) = ResolvedContainerScope::new(id, relative) {
            discovery.scopes.push(scope);
        }
    }
}

fn starts_with(name: &OsString, prefix: &str) -> bool {
    name.to_str().is_some_and(|name| name.starts_with(prefix))
}

fn ends_with(name: &OsString, suffix: &str) -> bool {
    name.to_str().is_some_and(|name| name.ends_with(suffix))
}

#[cfg(test)]
#[path = "container_scopes_tests.rs"]
mod tests;
