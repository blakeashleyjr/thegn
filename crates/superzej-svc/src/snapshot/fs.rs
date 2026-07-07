//! Host-local filesystem snapshot store — the zero-config default. Layout:
//! `<root>/<repo>/<worktree>/<env>/<snapshot-id>/{manifest.json,bundle,patch,tar}`
//! with `<root>` = `[lifecycle.snapshot] dir` or
//! `$XDG_STATE_HOME/superzej/snapshots`. Writes are atomic (`.tmp` + rename)
//! and the manifest lands last, so a torn snapshot is invisible to `list`.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use superzej_core::config_env_tables::SnapshotStoreConfig;
use superzej_core::snapshot_meta::{SnapshotKey, SnapshotManifest};

use super::SnapshotStore;

pub struct FsSnapshotStore {
    root: PathBuf,
}

impl FsSnapshotStore {
    pub fn new(cfg: &SnapshotStoreConfig) -> Self {
        let dir = cfg.dir.trim();
        let root = if dir.is_empty() {
            superzej_core::util::xdg_state_home()
                .join("superzej")
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

    fn write_atomic(dir: &Path, name: &str, data: &[u8]) -> Result<()> {
        fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
        let tmp = dir.join(format!(".tmp-{name}"));
        fs::write(&tmp, data).with_context(|| format!("write {}", tmp.display()))?;
        let dst = dir.join(name);
        fs::rename(&tmp, &dst).with_context(|| format!("publish {}", dst.display()))?;
        Ok(())
    }
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
        Self::write_atomic(&self.snap_dir(key, id)?, name, data)
    }

    fn get(&self, key: &SnapshotKey, id: &str, name: &str) -> Result<Vec<u8>> {
        check_component(name)?;
        let p = self.snap_dir(key, id)?.join(name);
        fs::read(&p).with_context(|| format!("read {}", p.display()))
    }

    fn put_manifest(&self, key: &SnapshotKey, manifest: &SnapshotManifest) -> Result<()> {
        let data = serde_json::to_vec_pretty(manifest)?;
        Self::write_atomic(&self.snap_dir(key, &manifest.id)?, "manifest.json", &data)
    }

    fn get_manifest(&self, key: &SnapshotKey, id: &str) -> Result<SnapshotManifest> {
        let p = self.snap_dir(key, id)?.join("manifest.json");
        let data = fs::read(&p).with_context(|| format!("read {}", p.display()))?;
        Ok(serde_json::from_slice(&data)?)
    }

    fn list(&self, key: &SnapshotKey) -> Result<Vec<SnapshotManifest>> {
        let dir = self.root.join(key.prefix());
        let mut out = Vec::new();
        let entries = match fs::read_dir(&dir) {
            Ok(e) => e,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(out),
            Err(e) => return Err(e).with_context(|| format!("list {}", dir.display())),
        };
        for entry in entries.flatten() {
            // Only manifest-bearing dirs are real snapshots; a torn capture
            // (artifacts without a manifest) is invisible garbage here.
            let Ok(data) = fs::read(entry.path().join("manifest.json")) else {
                continue;
            };
            if let Ok(m) = serde_json::from_slice::<SnapshotManifest>(&data) {
                out.push(m);
            }
        }
        out.sort_by(|a, b| a.id.cmp(&b.id));
        Ok(out)
    }

    fn delete(&self, key: &SnapshotKey, id: &str) -> Result<()> {
        let dir = self.snap_dir(key, id)?;
        match fs::remove_dir_all(&dir) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e).with_context(|| format!("delete {}", dir.display())),
        }
    }
}

#[cfg(test)]
mod tests {
    use superzej_core::snapshot_meta::{ArtifactMeta, SnapshotManifest};

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
        // No tmp droppings either — writes publish or vanish.
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
        s.delete(&k, &b.id).unwrap(); // second delete: no error
        assert_eq!(s.list(&k).unwrap().len(), 1);
        // Missing keys list as empty, not as an error.
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
}
