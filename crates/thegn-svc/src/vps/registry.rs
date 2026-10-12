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

use std::io::Read;
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
    let metadata = std::fs::symlink_metadata(d)?;
    anyhow::ensure!(
        !metadata.file_type().is_symlink()
            && metadata.is_dir()
            && thegn_core::fsperm::owned_by_current_user(&metadata),
        "vps registry: registry root is not an owned real directory"
    );
    let path = record_path(d, &rec.name);
    let counter = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
    let tmp = d.join(format!(
        "{}.json.{}.{}.tmp",
        rec.name,
        std::process::id(),
        counter
    ));
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&tmp)
        .with_context(|| format!("write {}", tmp.display()))?;
    let publish = (|| -> Result<()> {
        serde_json::to_writer_pretty(&mut file, rec)?;
        file.sync_all()
            .with_context(|| format!("sync {}", tmp.display()))?;
        std::fs::rename(&tmp, &path).with_context(|| format!("rename {}", path.display()))?;
        if let Ok(dir) = std::fs::File::open(d)
            && let Err(error) = dir.sync_all()
        {
            tracing::warn!(
                %error,
                path = %d.display(),
                "VPS registry record was published but its directory could not be synced"
            );
        }
        Ok(())
    })();
    if let Err(error) = publish {
        match std::fs::remove_file(&tmp) {
            Ok(()) => return Err(error),
            Err(cleanup) if cleanup.kind() == std::io::ErrorKind::NotFound => return Err(error),
            Err(cleanup) => {
                return Err(error.context(format!("also failed to remove staging file: {cleanup}")));
            }
        }
    }
    Ok(())
}

pub fn write(rec: &VpsRecord) -> Result<()> {
    write_at(&dir(), rec)
}

pub fn read_at(d: &Path, name: &str) -> Option<VpsRecord> {
    checked_name(name).ok()?;
    let mut file = thegn_core::fsperm::open_regular_file_nofollow(&record_path(d, name)).ok()?;
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes).ok()?;
    let rec: VpsRecord = serde_json::from_slice(&bytes).ok()?;
    (rec.name == name).then_some(rec)
}

pub fn read(name: &str) -> Option<VpsRecord> {
    read_at(&dir(), name)
}

/// Remove a record (and the instance's known_hosts pin). Idempotent.
pub fn remove_at(d: &Path, name: &str) {
    if checked_name(name).is_err() {
        return;
    }
    let Ok(root) = std::fs::symlink_metadata(d) else {
        return;
    };
    if root.file_type().is_symlink() || !root.is_dir() {
        return;
    }
    let known_hosts = d.join("known_hosts.d");
    if let Ok(metadata) = std::fs::symlink_metadata(&known_hosts)
        && (metadata.file_type().is_symlink() || !metadata.is_dir())
    {
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
        .filter_map(|e| {
            let path = e.path();
            let stem = path.file_stem()?.to_str()?;
            checked_name(stem).ok()?;
            let mut file = thegn_core::fsperm::open_regular_file_nofollow(&path).ok()?;
            let mut bytes = Vec::new();
            file.read_to_end(&mut bytes).ok()?;
            let rec: VpsRecord = serde_json::from_slice(&bytes).ok()?;
            (rec.name == stem && checked_name(&rec.name).is_ok()).then_some(rec)
        })
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
    fn list_skips_records_with_mismatched_or_unsafe_identity() {
        let tmp = tempfile::tempdir().unwrap();
        let d = tmp.path();
        write_at(d, &rec("valid", "ready")).unwrap();
        std::fs::write(
            d.join("a.json"),
            serde_json::to_vec(&rec("b", "ready")).unwrap(),
        )
        .unwrap();
        std::fs::write(
            d.join("c.json"),
            serde_json::to_vec(&rec("../x", "ready")).unwrap(),
        )
        .unwrap();
        let names: Vec<String> = list_at(d).into_iter().map(|r| r.name).collect();
        assert_eq!(names, vec!["valid"]);
    }

    #[test]
    fn failed_rename_removes_staging_file() {
        let tmp = tempfile::tempdir().unwrap();
        let d = tmp.path();
        std::fs::create_dir(d.join("blocked.json")).unwrap();
        std::fs::write(d.join("blocked.json/child"), b"keep directory nonempty").unwrap();
        assert!(write_at(d, &rec("blocked", "ready")).is_err());
        assert!(std::fs::read_dir(d).unwrap().all(|entry| {
            !entry
                .unwrap()
                .path()
                .extension()
                .is_some_and(|x| x == "tmp")
        }));
    }

    #[test]
    fn concurrent_writes_publish_complete_record_without_staging_debris() {
        let tmp = tempfile::tempdir().unwrap();
        let d = tmp.path().to_path_buf();
        let workers: Vec<_> = (0..8)
            .map(|index| {
                let d = d.clone();
                std::thread::spawn(move || {
                    let mut record = rec("shared", "ready");
                    record.instance_id = index.to_string();
                    write_at(&d, &record).unwrap();
                })
            })
            .collect();
        for worker in workers {
            worker.join().unwrap();
        }
        let record = read_at(&d, "shared").unwrap();
        assert_eq!(record.name, "shared");
        assert_eq!(record.state, "ready");
        assert!(std::fs::read_dir(d).unwrap().all(|entry| {
            !entry
                .unwrap()
                .path()
                .extension()
                .is_some_and(|x| x == "tmp")
        }));
    }

    #[test]
    fn unsafe_names_cannot_write_outside_registry() {
        let tmp = tempfile::tempdir().unwrap();
        let d = tmp.path().join("registry");
        std::fs::create_dir_all(d.join("a")).unwrap();
        let absolute = tmp.path().join("absolute");
        let absolute_json = absolute.with_extension("json");
        let parent = tmp.path().join("x.json");
        std::fs::write(absolute_json.as_path(), b"untouched").unwrap();
        std::fs::write(&parent, b"untouched").unwrap();
        for name in [absolute.to_str().unwrap(), "../x", "a/b", "", "."] {
            assert!(write_at(&d, &rec(name, "creating")).is_err(), "{name:?}");
        }
        assert_eq!(std::fs::read(absolute_json).unwrap(), b"untouched");
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

    #[test]
    fn write_rejects_symlinked_registry_root() {
        let tmp = tempfile::tempdir().unwrap();
        let real = tmp.path().join("real");
        let link = tmp.path().join("link");
        std::fs::create_dir(&real).unwrap();
        if let Err(error) = thegn_core::fsperm::create_symlink(&real, &link, true) {
            if matches!(
                error.kind(),
                std::io::ErrorKind::Unsupported | std::io::ErrorKind::PermissionDenied
            ) {
                return;
            }
            panic!("could not create test symlink: {error}");
        }
        assert!(write_at(&link, &rec("a", "ready")).is_err());
        assert!(!real.join("a.json").exists());
    }

    #[test]
    fn read_rejects_record_with_mismatched_body_identity() {
        let tmp = tempfile::tempdir().unwrap();
        let d = tmp.path();
        std::fs::write(
            d.join("a.json"),
            serde_json::to_vec(&rec("b", "ready")).unwrap(),
        )
        .unwrap();
        assert!(read_at(d, "a").is_none());
    }

    #[test]
    fn read_and_list_reject_symlinked_records() {
        let tmp = tempfile::tempdir().unwrap();
        let d = tmp.path().join("registry");
        let outside = tmp.path().join("outside");
        std::fs::create_dir(&d).unwrap();
        std::fs::create_dir(&outside).unwrap();
        std::fs::write(
            outside.join("x.json"),
            serde_json::to_vec(&rec("x", "ready")).unwrap(),
        )
        .unwrap();
        if let Err(error) =
            thegn_core::fsperm::create_symlink(&outside.join("x.json"), &d.join("x.json"), false)
        {
            if matches!(
                error.kind(),
                std::io::ErrorKind::Unsupported | std::io::ErrorKind::PermissionDenied
            ) {
                return;
            }
            panic!("could not create test symlink: {error}");
        }
        assert!(read_at(&d, "x").is_none());
        assert!(list_at(&d).is_empty());
    }

    #[test]
    fn remove_does_not_follow_symlinked_known_hosts_directory() {
        let tmp = tempfile::tempdir().unwrap();
        let d = tmp.path().join("registry");
        let victim = tmp.path().join("victim");
        std::fs::create_dir(&d).unwrap();
        std::fs::create_dir(&victim).unwrap();
        std::fs::write(victim.join("a"), b"keep").unwrap();
        if let Err(error) =
            thegn_core::fsperm::create_symlink(&victim, &d.join("known_hosts.d"), true)
        {
            if matches!(
                error.kind(),
                std::io::ErrorKind::Unsupported | std::io::ErrorKind::PermissionDenied
            ) {
                return;
            }
            panic!("could not create test symlink: {error}");
        }
        remove_at(&d, "a");
        assert!(victim.join("a").exists());
    }

    #[test]
    fn remove_does_not_follow_symlinked_registry_root() {
        let tmp = tempfile::tempdir().unwrap();
        let victim = tmp.path().join("victim");
        let link = tmp.path().join("registry");
        std::fs::create_dir(&victim).unwrap();
        std::fs::write(victim.join("a.json"), b"keep").unwrap();
        if let Err(error) = thegn_core::fsperm::create_symlink(&victim, &link, true) {
            if matches!(
                error.kind(),
                std::io::ErrorKind::Unsupported | std::io::ErrorKind::PermissionDenied
            ) {
                return;
            }
            panic!("could not create test symlink: {error}");
        }
        remove_at(&link, "a");
        assert!(victim.join("a.json").exists());
    }
}
