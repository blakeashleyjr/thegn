//! Bounded, engine-free discovery of container cgroups from the cgroup v2 tree.

use std::collections::BTreeSet;
use std::ffi::{OsStr, OsString};
use std::io;
use std::path::Path;

use super::containers_stats::ResolvedContainerScope;
use crate::collect::policy::cgroup_classify::{OwnerKind, classify};

pub const MAX_CONTAINER_SCOPES: usize = 256;
// Two child directories per accepted scope plus one sentinel directory to detect overflow.
const MAX_DIRECTORY_DIRECTORIES: usize = MAX_CONTAINER_SCOPES * 2;
// The entry budget adds one sentinel so listing truncation can be reported without an unbounded read.
const MAX_DIRECTORY_ENTRIES: usize = MAX_DIRECTORY_DIRECTORIES + 1;
// Three fixed listings plus every possible user slice, its services, and each service's two slices.
const MAX_DIRECTORY_LISTINGS: usize = 3 + MAX_DIRECTORY_ENTRIES * (1 + MAX_DIRECTORY_ENTRIES * 3);

#[derive(Debug, Default)]
pub struct DirectoryListing {
    pub directories: Vec<OsString>,
    pub truncated: bool,
}

pub trait DirectoryFs {
    /// Return direct child directory names. Implementations must not follow symlinks.
    fn directories(&self, path: &Path) -> io::Result<DirectoryListing>;
}

#[derive(Debug, Default)]
pub struct RealDirectoryFs;

impl DirectoryFs for RealDirectoryFs {
    fn directories(&self, path: &Path) -> io::Result<DirectoryListing> {
        if !std::fs::symlink_metadata(path)?.file_type().is_dir() {
            return Ok(DirectoryListing::default());
        }
        let mut listing = DirectoryListing::default();
        let mut directories = BTreeSet::new();
        let mut entries_read = 0;
        for entry in std::fs::read_dir(path)?.take(MAX_DIRECTORY_ENTRIES) {
            entries_read += 1;
            let Ok(entry) = entry else { continue };
            let Ok(file_type) = entry.file_type() else {
                continue;
            };
            if file_type.is_dir() {
                directories.insert(entry.file_name());
                if directories.len() > MAX_DIRECTORY_DIRECTORIES {
                    listing.truncated = true;
                    directories.pop_last();
                }
            }
        }
        if entries_read == MAX_DIRECTORY_ENTRIES {
            listing.truncated = true;
        }
        listing.directories = directories.into_iter().collect();
        Ok(listing)
    }
}

#[derive(Debug, Default)]
pub struct Discovery {
    pub scopes: Vec<ResolvedContainerScope>,
    pub truncated: bool,
    pub list_errors: u32,
}

pub fn discover(fs: &dyn DirectoryFs, root: &Path) -> Discovery {
    let mut discovery = Discovery {
        scopes: Vec::with_capacity(MAX_CONTAINER_SCOPES),
        truncated: false,
        list_errors: 0,
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
        Ok(mut listing) => {
            discovery.truncated |= listing.truncated;
            listing.directories.sort_unstable();
            if listing.directories.len() > MAX_DIRECTORY_DIRECTORIES {
                discovery.truncated = true;
                listing.directories.truncate(MAX_DIRECTORY_DIRECTORIES);
            }
            listing.directories
        }
        Err(error) => {
            if error.kind() != io::ErrorKind::NotFound {
                discovery.list_errors = discovery.list_errors.saturating_add(1);
            }
            Vec::new()
        }
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

fn starts_with(name: &OsStr, prefix: &str) -> bool {
    name.to_str().is_some_and(|name| name.starts_with(prefix))
}

fn ends_with(name: &OsStr, suffix: &str) -> bool {
    name.to_str().is_some_and(|name| name.ends_with(suffix))
}

#[cfg(test)]
#[path = "container_scopes_tests.rs"]
mod tests;
