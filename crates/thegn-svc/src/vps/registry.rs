//! File-based VPS instance registry under `$XDG_STATE/thegn/vps/` — one JSON
//! file per instance name. This is the **leak-safety ledger**: a record is
//! written (state `creating`) *before* the create POST and finalized (state
//! `ready`, instance id + IP) after, so a crash between the two leaves an
//! intent record the reaper can reconcile against the provider's live list. It
//! doubles as the IP cache the `thegn vps-ssh` attach bridge reads (no API
//! call per pane/git-read).
//!
//! Files, not the DB, because both svc (the provider owns create/destroy) and
//! the host (attach bridge, reaper) read it — and every create/destroy flows
//! through the provider, so no call site can forget the bookkeeping.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

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
    thegn_core::util::thegn_dir().join("vps")
}

/// The per-instance known_hosts file for the ssh transport: fresh VPSes mean
/// fresh host keys, so each sandbox gets its own file (`accept-new` pins the
/// first key) instead of polluting the global known_hosts. Removed on destroy.
pub fn known_hosts_path(name: &str) -> PathBuf {
    let safe = checked_name(name).unwrap_or("invalid..");
    dir().join("known_hosts.d").join(safe)
}

/// Accept only a single provider name component using the RFC 1123 hostname
/// subset used by VPS admission.
pub fn checked_name(name: &str) -> Result<&str> {
    if name.is_empty() || name.len() > 63 || !name.split('.').all(valid_dns_label) {
        anyhow::bail!("vps registry: invalid provider name")
    }
    Ok(name)
}

fn valid_dns_label(label: &str) -> bool {
    let bytes = label.as_bytes();
    !bytes.is_empty()
        && bytes.len() <= 63
        && bytes[0].is_ascii_alphanumeric()
        && bytes[bytes.len() - 1].is_ascii_alphanumeric()
        && bytes
            .iter()
            .all(|byte| byte.is_ascii_alphanumeric() || *byte == b'-')
}

fn record_path(d: &Path, name: &str) -> PathBuf {
    d.join(format!("{name}.json"))
}

/// Write (atomically: tmp + rename) a record into `d`.
pub fn write_at(d: &Path, rec: &VpsRecord) -> Result<()> {
    checked_name(&rec.name)?;
    std::fs::create_dir_all(d)?;
    let path = record_path(d, &rec.name);
    let counter = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
    let tmp = d.join(format!(
        "{}.json.{}.{}.tmp",
        rec.name,
        std::process::id(),
        counter
    ));
    let file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&tmp)
        .with_context(|| format!("write {}", tmp.display()))?;
    serde_json::to_writer_pretty(file, rec)?;
    std::fs::rename(&tmp, &path).with_context(|| format!("rename {}", path.display()))?;
    Ok(())
}

pub fn write(rec: &VpsRecord) -> Result<()> {
    write_at(&dir(), rec)
}

pub fn read_at(d: &Path, name: &str) -> Option<VpsRecord> {
    checked_name(name).ok()?;
    let bytes = std::fs::read(record_path(d, name)).ok()?;
    serde_json::from_slice(&bytes).ok()
}

pub fn read(name: &str) -> Option<VpsRecord> {
    read_at(&dir(), name)
}

/// Remove a record (and the instance's known_hosts pin). Idempotent.
pub fn remove_at(d: &Path, name: &str) {
    if checked_name(name).is_err() {
        return;
    }
    let _ = std::fs::remove_file(record_path(d, name)); // best-effort: idempotent removal; absence is fine
    let _ = std::fs::remove_file(d.join("known_hosts.d").join(name)); // best-effort: idempotent removal; absence is fine
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
        write_at(d, &rec("tg-dev-x1", "creating")).unwrap();
        let r = read_at(d, "tg-dev-x1").unwrap();
        assert_eq!(r.state, "creating");
        assert!(r.ip.is_empty());

        // Finalize after the poll: id + ip land, state flips.
        let mut done = r.clone();
        done.state = "ready".into();
        done.instance_id = "42".into();
        done.ip = "203.0.113.7".into();
        write_at(d, &done).unwrap();
        assert_eq!(read_at(d, "tg-dev-x1").unwrap(), done);

        assert_eq!(list_at(d).len(), 1);
        remove_at(d, "tg-dev-x1");
        assert!(read_at(d, "tg-dev-x1").is_none());
        assert!(list_at(d).is_empty());
        // Idempotent remove.
        remove_at(d, "tg-dev-x1");
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

    #[test]
    fn unsafe_names_cannot_write_outside_registry() {
        let tmp = tempfile::tempdir().unwrap();
        let d = tmp.path().join("registry");
        std::fs::create_dir_all(d.join("a")).unwrap();
        let absolute = tmp.path().join("absolute");
        let parent = tmp.path().join("x.json");
        std::fs::write(&absolute.with_extension("json"), b"untouched").unwrap();
        std::fs::write(&parent, b"untouched").unwrap();
        for name in [absolute.to_str().unwrap(), "../x", "a/b", "", "."] {
            assert!(write_at(&d, &rec(name, "creating")).is_err(), "{name:?}");
        }
        assert_eq!(
            std::fs::read(absolute.with_extension("json")).unwrap(),
            b"untouched"
        );
        assert_eq!(std::fs::read(parent).unwrap(), b"untouched");
        assert!(std::fs::read_dir(d).unwrap().all(|entry| {
            let entry = entry.unwrap();
            entry.path().is_dir()
        }));
    }

    #[test]
    fn invalid_remove_and_known_hosts_names_stay_within_owned_roots() {
        let tmp = tempfile::tempdir().unwrap();
        let d = tmp.path().join("registry");
        std::fs::create_dir_all(&d).unwrap();
        let victim = tmp.path().join("victim.json");
        std::fs::write(&victim, b"untouched").unwrap();
        remove_at(&d, "../victim");
        assert_eq!(std::fs::read(&victim).unwrap(), b"untouched");

        let root = dir().join("known_hosts.d");
        let path = known_hosts_path("../../x");
        assert!(path.starts_with(root));
        assert!(path.ends_with("invalid.."));
    }
}
