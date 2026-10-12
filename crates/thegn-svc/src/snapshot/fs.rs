//! Host-local filesystem snapshot store — the zero-config default. Layout:
//! `<root>/<repo>/<worktree>/<env>/<snapshot-id>/{manifest.json,bundle,patch,tar}`
//! with `<root>` = `[lifecycle.snapshot] dir` or
//! `$XDG_STATE_HOME/thegn/snapshots`.

use std::fs::{self, File};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use thegn_core::config_env_tables::SnapshotStoreConfig;
use thegn_core::snapshot_meta::{SnapshotKey, SnapshotManifest};

use super::SnapshotStore;

const MAX_SNAPSHOT_FILE_BYTES: u64 = 1024 * 1024 * 1024;

pub struct FsSnapshotStore {
    root: PathBuf,
}

impl FsSnapshotStore {
    pub fn new(cfg: &SnapshotStoreConfig) -> Self {
        let dir = cfg.dir.trim();
        let root = if dir.is_empty() {
            thegn_core::util::xdg_state_home()
                .join("thegn")
                .join("snapshots")
        } else {
            PathBuf::from(dir)
        };
        FsSnapshotStore { root }
    }

    /// Root for one snapshot id. `id` and artifact names come from manifests
    /// we wrote, but stay defensive: reject anything path-shaped.
    fn snap_dir(&self, key: &SnapshotKey, id: &str) -> Result<PathBuf> {
        check_component(id)?;
        Ok(self.root.join(key.prefix()).join(id))
    }

    fn prepare_dir(&self, dir: &Path) -> Result<()> {
        // Do not chmod ancestors such as /tmp or /home. Validate the configured
        // root, then each store-owned component below it independently.
        thegn_core::fsperm::ensure_private_dir(&self.root)
            .with_context(|| format!("prepare private snapshot root {}", self.root.display()))?;
        let relative = dir
            .strip_prefix(&self.root)
            .context("snapshot path escaped store root")?;
        let mut current = self.root.clone();
        for component in relative.components() {
            current.push(component.as_os_str());
            match fs::symlink_metadata(&current) {
                Ok(meta) if meta.file_type().is_symlink() || !meta.is_dir() => {
                    anyhow::bail!(
                        "refusing non-directory snapshot component {}",
                        current.display()
                    );
                }
                Ok(_) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                    match fs::create_dir(&current) {
                        Ok(()) => sync_directory(current.parent().unwrap_or(&self.root))?,
                        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
                        Err(error) => {
                            return Err(error)
                                .with_context(|| format!("create {}", current.display()));
                        }
                    }
                }
                Err(error) => {
                    return Err(error).with_context(|| format!("stat {}", current.display()));
                }
            }
            thegn_core::fsperm::ensure_private_dir(&current).with_context(|| {
                format!("prepare private snapshot directory {}", current.display())
            })?;
        }
        Ok(())
    }

    fn check_existing_dir(&self, dir: &Path) -> Result<()> {
        let relative = dir
            .strip_prefix(&self.root)
            .context("snapshot path escaped store root")?;
        let mut current = self.root.clone();
        let mut components = relative.components();
        let meta = fs::symlink_metadata(&current)
            .with_context(|| format!("stat snapshot directory {}", current.display()))?;
        check_private_directory(&current, &meta)?;
        for component in &mut components {
            current.push(component.as_os_str());
            let meta = fs::symlink_metadata(&current)
                .with_context(|| format!("stat snapshot directory {}", current.display()))?;
            check_private_directory(&current, &meta)?;
        }
        Ok(())
    }

    fn write_atomic(&self, dir: &Path, name: &str, data: &[u8]) -> Result<()> {
        check_component(name)?;
        check_size(data.len() as u64)?;
        self.prepare_dir(dir)?;
        let dst = dir.join(name);
        match fs::symlink_metadata(&dst) {
            Ok(meta) if meta.file_type().is_symlink() || !meta.file_type().is_file() => {
                anyhow::bail!(
                    "refusing non-regular snapshot destination {}",
                    dst.display()
                );
            }
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error).with_context(|| format!("stat {}", dst.display())),
        }

        let (tmp, mut file) = create_staging_file(dir, name)?;
        let result = file
            .write_all(data)
            .and_then(|()| file.sync_all())
            .and_then(|()| fs::rename(&tmp, &dst))
            .and_then(|()| sync_directory(dir));
        if let Err(error) = result {
            drop(file);
            let publish_error = error.context(format!("publish {}", dst.display()));
            return match fs::remove_file(&tmp) {
                Ok(()) => Err(publish_error),
                Err(cleanup_error) if cleanup_error.kind() == std::io::ErrorKind::NotFound => {
                    Err(publish_error)
                }
                Err(cleanup_error) => Err(publish_error.context(format!(
                    "also failed to remove staging file {}: {cleanup_error}",
                    tmp.display()
                ))),
            };
        }
        Ok(())
    }

    fn read_file(path: &Path) -> Result<Vec<u8>> {
        let mut file = thegn_core::fsperm::open_regular_file_nofollow(path)
            .with_context(|| format!("open snapshot file {}", path.display()))?;
        let mut bytes = Vec::new();
        Read::by_ref(&mut file)
            .take(MAX_SNAPSHOT_FILE_BYTES + 1)
            .read_to_end(&mut bytes)
            .with_context(|| format!("read snapshot file {}", path.display()))?;
        if bytes.len() as u64 > MAX_SNAPSHOT_FILE_BYTES {
            anyhow::bail!(
                "snapshot file {} exceeds the {} byte limit",
                path.display(),
                MAX_SNAPSHOT_FILE_BYTES
            );
        }
        Ok(bytes)
    }
}

fn create_staging_file(dir: &Path, name: &str) -> Result<(PathBuf, File)> {
    for _ in 0..16 {
        let mut random = [0_u8; 16];
        getrandom::fill(&mut random).context("generate snapshot staging name")?;
        let suffix = random
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>();
        let path = dir.join(format!(".tmp-{name}-{suffix}"));
        match thegn_core::fsperm::create_owner_only_exclusive(&path) {
            Ok(file) => {
                let meta = file.metadata().context("stat open snapshot staging file")?;
                if !meta.file_type().is_file() {
                    anyhow::bail!("snapshot staging file is not a private regular file");
                }
                if !thegn_core::fsperm::owned_by_current_user(&meta) {
                    anyhow::bail!("snapshot staging file is owned by another user");
                }
                return Ok((path, file));
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("create staging file {}", path.display()));
            }
        }
    }
    anyhow::bail!("could not allocate a unique snapshot staging file for {name:?}")
}

fn sync_directory(path: &Path) -> std::io::Result<()> {
    File::open(path)?.sync_all()
}

fn check_private_directory(path: &Path, meta: &fs::Metadata) -> Result<()> {
    if meta.file_type().is_symlink() || !meta.is_dir() {
        anyhow::bail!(
            "refusing non-directory snapshot component {}",
            path.display()
        );
    }
    if !thegn_core::fsperm::owned_by_current_user(meta) {
        anyhow::bail!(
            "snapshot directory {} is owned by another user",
            path.display()
        );
    }
    if thegn_core::fsperm::mode_bits(path)?.is_some_and(|mode| mode & 0o022 != 0) {
        anyhow::bail!(
            "snapshot directory {} is group- or world-writable",
            path.display()
        );
    }
    Ok(())
}

fn check_size(len: u64) -> Result<()> {
    if len > MAX_SNAPSHOT_FILE_BYTES {
        anyhow::bail!(
            "snapshot file exceeds the {} byte limit",
            MAX_SNAPSHOT_FILE_BYTES
        );
    }
    Ok(())
}

/// Reject ids/names that could escape the snapshot directory.
fn check_component(s: &str) -> Result<()> {
    if s.is_empty() || s.contains(['/', '\\']) || s == "." || s == ".." {
        anyhow::bail!("invalid snapshot path component {s:?}");
    }
    Ok(())
}

impl SnapshotStore for FsSnapshotStore {
    fn put(&self, key: &SnapshotKey, id: &str, name: &str, data: &[u8]) -> Result<()> {
        check_component(name)?;
        self.write_atomic(&self.snap_dir(key, id)?, name, data)
    }

    fn get(&self, key: &SnapshotKey, id: &str, name: &str) -> Result<Vec<u8>> {
        check_component(name)?;
        let dir = self.snap_dir(key, id)?;
        self.check_existing_dir(&dir)?;
        let p = dir.join(name);
        Self::read_file(&p)
    }

    fn put_manifest(&self, key: &SnapshotKey, manifest: &SnapshotManifest) -> Result<()> {
        let data = serde_json::to_vec_pretty(manifest)?;
        self.write_atomic(&self.snap_dir(key, &manifest.id)?, "manifest.json", &data)
    }

    fn get_manifest(&self, key: &SnapshotKey, id: &str) -> Result<SnapshotManifest> {
        let dir = self.snap_dir(key, id)?;
        self.check_existing_dir(&dir)?;
        let p = dir.join("manifest.json");
        let data = Self::read_file(&p)?;
        let manifest: SnapshotManifest = serde_json::from_slice(&data)?;
        if manifest.id != id {
            anyhow::bail!(
                "snapshot manifest id {:?} does not match directory id {id:?}",
                manifest.id
            );
        }
        Ok(manifest)
    }

    fn list(&self, key: &SnapshotKey) -> Result<Vec<SnapshotManifest>> {
        let dir = self.root.join(key.prefix());
        let mut out = Vec::new();
        match self.check_existing_dir(&dir) {
            Ok(()) => {}
            Err(error)
                if error
                    .root_cause()
                    .downcast_ref::<std::io::Error>()
                    .is_some_and(|io_error| io_error.kind() == std::io::ErrorKind::NotFound) =>
            {
                return Ok(out);
            }
            Err(error) => return Err(error),
        }
        let entries = match fs::read_dir(&dir) {
            Ok(e) => e,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(out),
            Err(e) => return Err(e).with_context(|| format!("list {}", dir.display())),
        };
        for entry in entries {
            let entry = entry.with_context(|| format!("read entry in {}", dir.display()))?;
            let id = entry
                .file_name()
                .into_string()
                .map_err(|_| anyhow::anyhow!("non-UTF-8 snapshot directory name"))?;
            check_component(&id)?;
            let meta = fs::symlink_metadata(entry.path())?;
            if meta.file_type().is_symlink() || !meta.is_dir() {
                anyhow::bail!(
                    "refusing non-directory snapshot entry {}",
                    entry.path().display()
                );
            }
            match self.get_manifest(key, &id) {
                Ok(manifest) => out.push(manifest),
                Err(error)
                    if error
                        .root_cause()
                        .downcast_ref::<std::io::Error>()
                        .is_some_and(|io_error| {
                            io_error.kind() == std::io::ErrorKind::NotFound
                        }) => {}
                Err(error) => return Err(error),
            }
        }
        out.sort_by(|a, b| a.id.cmp(&b.id));
        Ok(out)
    }

    fn delete(&self, key: &SnapshotKey, id: &str) -> Result<()> {
        let dir = self.snap_dir(key, id)?;
        if let Err(error) = self.check_existing_dir(dir.parent().unwrap_or(&self.root)) {
            if error
                .root_cause()
                .downcast_ref::<std::io::Error>()
                .is_some_and(|io_error| io_error.kind() == std::io::ErrorKind::NotFound)
            {
                return Ok(());
            }
            return Err(error);
        }
        match fs::symlink_metadata(&dir) {
            Ok(meta) if meta.file_type().is_symlink() || !meta.is_dir() => {
                anyhow::bail!("refusing non-directory snapshot path {}", dir.display());
            }
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(e) => return Err(e).with_context(|| format!("stat {}", dir.display())),
        }
        fs::remove_dir_all(&dir).with_context(|| format!("delete {}", dir.display()))?;
        sync_directory(dir.parent().unwrap_or(&self.root))
            .context("sync snapshot directory after delete")
    }
}

#[cfg(test)]
mod tests {
    use thegn_core::snapshot_meta::{ArtifactMeta, SnapshotManifest};

    use super::*;

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

    fn manifest(t: i64) -> SnapshotManifest {
        SnapshotManifest::new(
            Some("abcd1234"),
            "main",
            t,
            vec![ArtifactMeta {
                name: "bundle".into(),
                bytes: 4,
                sha256: "e".repeat(64),
            }],
        )
    }

    #[test]
    fn roundtrip_artifacts_and_manifest() {
        let tmp = tempfile::tempdir().unwrap();
        let s = store(tmp.path());
        let k = key();
        let m = manifest(10);
        s.put(&k, &m.id, "bundle", b"data").unwrap();
        s.put_manifest(&k, &m).unwrap();
        assert_eq!(s.get(&k, &m.id, "bundle").unwrap(), b"data");
        assert_eq!(s.get_manifest(&k, &m.id).unwrap(), m);
        let all = s.list(&k).unwrap();
        assert_eq!(all, vec![m]);
    }

    #[test]
    fn torn_write_without_manifest_is_invisible() {
        let tmp = tempfile::tempdir().unwrap();
        let s = store(tmp.path());
        let k = key();
        s.put(&k, "00000000000000000005-dead", "bundle", b"x")
            .unwrap();
        assert!(s.list(&k).unwrap().is_empty());
        let m = manifest(6);
        s.put_manifest(&k, &m).unwrap();
        let dir = tmp.path().join(k.prefix()).join(&m.id);
        let names: Vec<String> = std::fs::read_dir(dir)
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(names, vec!["manifest.json"]);
    }

    #[test]
    fn list_sorts_oldest_first_and_delete_is_idempotent() {
        let tmp = tempfile::tempdir().unwrap();
        let s = store(tmp.path());
        let k = key();
        let (a, b) = (manifest(2), manifest(1));
        s.put_manifest(&k, &a).unwrap();
        s.put_manifest(&k, &b).unwrap();
        let ids: Vec<String> = s.list(&k).unwrap().into_iter().map(|m| m.id).collect();
        assert_eq!(ids, vec![b.id.clone(), a.id.clone()]);
        s.delete(&k, &b.id).unwrap();
        s.delete(&k, &b.id).unwrap();
        assert_eq!(s.list(&k).unwrap().len(), 1);
        let other = SnapshotKey {
            repo_slug: "nope".into(),
            worktree_slug: "x".into(),
            env: "y".into(),
        };
        assert!(s.list(&other).unwrap().is_empty());
    }

    #[test]
    fn path_shaped_ids_and_names_are_rejected() {
        let tmp = tempfile::tempdir().unwrap();
        let s = store(tmp.path());
        let k = key();
        assert!(s.put(&k, "../escape", "bundle", b"x").is_err());
        assert!(s.put(&k, "ok-id", "a/b", b"x").is_err());
        assert!(s.get(&k, "..", "bundle").is_err());
    }

    #[test]
    fn rejects_manifest_whose_id_does_not_match_its_directory() {
        let tmp = tempfile::tempdir().unwrap();
        let s = store(tmp.path());
        let k = key();
        let dir = s.snap_dir(&k, "directory-id").unwrap();
        s.prepare_dir(&dir).unwrap();
        fs::write(
            dir.join("manifest.json"),
            serde_json::to_vec(&manifest(10)).unwrap(),
        )
        .unwrap();
        assert!(s.get_manifest(&k, "directory-id").is_err());
        assert!(s.list(&k).is_err());
    }

    #[test]
    fn rejects_oversized_snapshot_data_before_writing() {
        assert!(check_size(MAX_SNAPSHOT_FILE_BYTES + 1).is_err());
        assert!(check_size(MAX_SNAPSHOT_FILE_BYTES).is_ok());
    }
}

#[cfg(test)]
#[path = "fs_tests.rs"]
mod fs_tests;
