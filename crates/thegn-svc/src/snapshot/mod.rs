//! The durable snapshot store behind worktree hibernation: artifacts captured
//! from a sandbox just before its compute is destroyed, addressable by
//! [`SnapshotKey`] and described by a [`SnapshotManifest`]
//! (`thegn_core::snapshot_meta`).
//!
//! The trait is synchronous on purpose — every caller (the hibernator reaper,
//! the restore plan step) already runs on a background thread, never the
//! compositor loop. Two backends: the host-local filesystem (default, zero
//! config) and any S3-compatible object store (opt-in via
//! `[lifecycle.snapshot]`).
//!
//! Write protocol shared by all backends: artifacts first, manifest LAST. A
//! snapshot without a manifest is invisible to `list`/restore — a torn write
//! is garbage to collect, never a truncated restore source.

pub mod fs;
pub mod s3;

use anyhow::Result;
use thegn_core::config_env_tables::{SnapshotBackend, SnapshotStoreConfig};
use thegn_core::secretref::SecretRef;
use thegn_core::snapshot_meta::{SnapshotKey, SnapshotManifest};

pub trait SnapshotStore: Send + Sync {
    /// Store one artifact blob under `key/id/name`.
    fn put(&self, key: &SnapshotKey, id: &str, name: &str, data: &[u8]) -> Result<()>;
    /// Fetch one artifact blob.
    fn get(&self, key: &SnapshotKey, id: &str, name: &str) -> Result<Vec<u8>>;
    /// Publish the manifest — the commit point that makes the snapshot real.
    fn put_manifest(&self, key: &SnapshotKey, manifest: &SnapshotManifest) -> Result<()>;
    /// Fetch one snapshot's manifest.
    fn get_manifest(&self, key: &SnapshotKey, id: &str) -> Result<SnapshotManifest>;
    /// Every published (manifest-bearing) snapshot under `key`, oldest first.
    fn list(&self, key: &SnapshotKey) -> Result<Vec<SnapshotManifest>>;
    /// Remove one snapshot (manifest + artifacts). Missing is not an error —
    /// retention pruning races harmlessly with itself.
    fn delete(&self, key: &SnapshotKey, id: &str) -> Result<()>;
}

/// Open the configured snapshot store. `resolve_secret` receives an already
/// parsed ref and a stable consumer tag — injected by the host so the typed
/// keyring/file/env/literal broker and its audit trail stay out of svc.
pub fn open_store(
    cfg: &SnapshotStoreConfig,
    resolve_secret: &dyn Fn(&SecretRef, &str) -> Option<String>,
) -> Result<Box<dyn SnapshotStore>> {
    match cfg.backend {
        SnapshotBackend::Local => Ok(Box::new(fs::FsSnapshotStore::new(cfg))),
        SnapshotBackend::S3 => Ok(Box::new(s3::S3SnapshotStore::new(cfg, resolve_secret)?)),
    }
}
