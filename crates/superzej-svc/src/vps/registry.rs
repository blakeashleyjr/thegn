//! File-based VPS instance registry under `$XDG_STATE/superzej/vps/` — one JSON
//! file per instance name. This is the **leak-safety ledger**: a record is
//! written (state `creating`) *before* the create POST and finalized (state
//! `ready`, instance id + IP) after, so a crash between the two leaves an
//! intent record the reaper can reconcile against the provider's live list. It
//! doubles as the IP cache the `szhost vps-ssh` attach bridge reads (no API
//! call per pane/git-read).
//!
//! Files, not the DB, because both svc (the provider owns create/destroy) and
//! the host (attach bridge, reaper) read it — and every create/destroy flows
//! through the provider, so no call site can forget the bookkeeping.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VpsRecord {
    pub name: String,
    /// Provider kind, e.g. `"hetzner"`.
    pub provider: String,
    /// `"creating"` (intent written, instance may not exist yet) | `"ready"`.
    pub state: String,
    /// The provider's instance id (empty while `creating`).
    pub instance_id: String,
    /// Public IPv4 (empty while `creating`).
    pub ip: String,
    /// Unix seconds at intent time (the reaper's stale-`creating` clock).
    pub created_at: i64,
}

/// The registry directory (created on demand).
pub fn dir() -> PathBuf {
    superzej_core::util::superzej_dir().join("vps")
}

/// The per-instance known_hosts file for the ssh transport: fresh VPSes mean
/// fresh host keys, so each sandbox gets its own file (`accept-new` pins the
/// first key) instead of polluting the global known_hosts. Removed on destroy.
pub fn known_hosts_path(name: &str) -> PathBuf {
    dir().join("known_hosts.d").join(name)
}

fn record_path(d: &Path, name: &str) -> PathBuf {
    d.join(format!("{name}.json"))
}

/// Write (atomically: tmp + rename) a record into `d`.
pub fn write_at(d: &Path, rec: &VpsRecord) -> Result<()> {
    std::fs::create_dir_all(d)?;
    let path = record_path(d, &rec.name);
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, serde_json::to_vec_pretty(rec)?)
        .with_context(|| format!("write {}", tmp.display()))?;
    std::fs::rename(&tmp, &path).with_context(|| format!("rename {}", path.display()))?;
    Ok(())
}

pub fn write(rec: &VpsRecord) -> Result<()> {
    write_at(&dir(), rec)
}

pub fn read_at(d: &Path, name: &str) -> Option<VpsRecord> {
    let bytes = std::fs::read(record_path(d, name)).ok()?;
    serde_json::from_slice(&bytes).ok()
}

pub fn read(name: &str) -> Option<VpsRecord> {
    read_at(&dir(), name)
}

/// Remove a record (and the instance's known_hosts pin). Idempotent.
pub fn remove_at(d: &Path, name: &str) {
    let _ = std::fs::remove_file(record_path(d, name));
    let _ = std::fs::remove_file(d.join("known_hosts.d").join(name));
}

pub fn remove(name: &str) {
    remove_at(&dir(), name)
}

/// All records in `d` (unreadable/foreign files skipped).
pub fn list_at(d: &Path) -> Vec<VpsRecord> {
    let Ok(entries) = std::fs::read_dir(d) else {
        return Vec::new();
    };
    entries
        .flatten()
        .filter(|e| e.path().extension().is_some_and(|x| x == "json"))
        .filter_map(|e| serde_json::from_slice(&std::fs::read(e.path()).ok()?).ok())
        .collect()
}

pub fn list() -> Vec<VpsRecord> {
    list_at(&dir())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rec(name: &str, state: &str) -> VpsRecord {
        VpsRecord {
            name: name.into(),
            provider: "hetzner".into(),
            state: state.into(),
            instance_id: String::new(),
            ip: String::new(),
            created_at: 1_700_000_000,
        }
    }

    #[test]
    fn intent_then_finalize_round_trips() {
        let tmp = tempfile::tempdir().unwrap();
        let d = tmp.path();
        // Intent BEFORE create: the leak-safety window closes here.
        write_at(d, &rec("sz-dev-x1", "creating")).unwrap();
        let r = read_at(d, "sz-dev-x1").unwrap();
        assert_eq!(r.state, "creating");
        assert!(r.ip.is_empty());

        // Finalize after the poll: id + ip land, state flips.
        let mut done = r.clone();
        done.state = "ready".into();
        done.instance_id = "42".into();
        done.ip = "203.0.113.7".into();
        write_at(d, &done).unwrap();
        assert_eq!(read_at(d, "sz-dev-x1").unwrap(), done);

        assert_eq!(list_at(d).len(), 1);
        remove_at(d, "sz-dev-x1");
        assert!(read_at(d, "sz-dev-x1").is_none());
        assert!(list_at(d).is_empty());
        // Idempotent remove.
        remove_at(d, "sz-dev-x1");
    }

    #[test]
    fn list_skips_foreign_and_corrupt_files() {
        let tmp = tempfile::tempdir().unwrap();
        let d = tmp.path();
        write_at(d, &rec("a", "ready")).unwrap();
        std::fs::write(d.join("junk.json"), b"not json").unwrap();
        std::fs::write(d.join("readme.txt"), b"x").unwrap();
        let names: Vec<String> = list_at(d).into_iter().map(|r| r.name).collect();
        assert_eq!(names, vec!["a"]);
        // A missing dir lists empty, not an error.
        assert!(list_at(&d.join("nope")).is_empty());
    }
}
