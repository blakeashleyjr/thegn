//! Filesystem semantics that require Unix symlinks or permission bits.

#![cfg(unix)]

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;

use thegn_core::snapshot_meta::{ArtifactMeta, SnapshotKey, SnapshotManifest};

use super::FsSnapshotStore;
use crate::snapshot::SnapshotStore;

fn store(root: &Path) -> FsSnapshotStore {
    FsSnapshotStore {
        root: root.to_path_buf(),
    }
}

fn key() -> SnapshotKey {
    SnapshotKey {
        repo_slug: "repo".into(),
        worktree_slug: "wt".into(),
        env: "hetzner".into(),
    }
}

fn manifest() -> SnapshotManifest {
    SnapshotManifest::new(
        Some("abcd1234"),
        "main",
        10,
        vec![ArtifactMeta {
            name: "bundle".into(),
            bytes: 4,
            sha256: "e".repeat(64),
        }],
    )
}

#[test]
fn refuses_symlink_destinations_and_reads() {
    let tmp = tempfile::tempdir().unwrap();
    let s = store(tmp.path());
    let k = key();
    let id = "snapshot";
    let dir = s.snap_dir(&k, id).unwrap();
    s.prepare_dir(&dir).unwrap();
    let target = tmp.path().join("canary");
    fs::write(&target, b"safe").unwrap();
    std::os::unix::fs::symlink(&target, dir.join("bundle")).unwrap();
    assert!(s.put(&k, id, "bundle", b"clobber").is_err());
    assert!(s.get(&k, id, "bundle").is_err());
    assert_eq!(fs::read(target).unwrap(), b"safe");
}

#[test]
fn refuses_symlinked_staging_candidate() {
    let tmp = tempfile::tempdir().unwrap();
    let s = store(tmp.path());
    let k = key();
    let id = "snapshot";
    let dir = s.snap_dir(&k, id).unwrap();
    s.prepare_dir(&dir).unwrap();
    let target = tmp.path().join("canary");
    fs::write(&target, b"safe").unwrap();
    // This was the old predictable staging name. The new publication
    // chooses a random sibling and must never write through this link.
    std::os::unix::fs::symlink(&target, dir.join(".tmp-bundle")).unwrap();
    s.put(&k, id, "bundle", b"data").unwrap();
    assert_eq!(fs::read(target).unwrap(), b"safe");
}

#[test]
fn accepts_legacy_private_directories_and_rejects_writable_ones() {
    let tmp = tempfile::tempdir().unwrap();
    let s = store(tmp.path());
    let k = key();
    let m = manifest();
    s.put(&k, &m.id, "bundle", b"data").unwrap();
    s.put_manifest(&k, &m).unwrap();

    let mut layout = vec![tmp.path().to_path_buf()];
    let mut directory = tmp.path().to_path_buf();
    for component in Path::new(&k.prefix()).components() {
        directory.push(component.as_os_str());
        layout.push(directory.clone());
    }
    let prefix = directory;
    let snapshot = prefix.join(&m.id);
    layout.push(snapshot.clone());
    for directory in &layout {
        fs::set_permissions(directory, fs::Permissions::from_mode(0o755)).unwrap();
    }

    assert_eq!(s.get_manifest(&k, &m.id).unwrap(), m);
    assert_eq!(s.get(&k, &m.id, "bundle").unwrap(), b"data");
    assert_eq!(s.list(&k).unwrap(), vec![m]);

    fs::set_permissions(&prefix, fs::Permissions::from_mode(0o775)).unwrap();
    assert!(s.get_manifest(&k, &m.id).is_err());
    assert!(s.get(&k, &m.id, "bundle").is_err());
    assert!(s.list(&k).is_err());
}
