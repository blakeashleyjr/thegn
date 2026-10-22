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
//! Count-and-reserve is serialised by a lock file, and unreadable records count as occupied.
//! Reads, listing and the lock are resolved through the held registry root.

use std::io::Read;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

pub const RESERVE_LOCK: &str = ".reserve.lock";
pub const BAKE_NAME_PREFIX: &str = "tg-bake-";

#[derive(Debug, Default)]
pub struct LedgerView {
    pub records: Vec<VpsRecord>,
    pub unreadable: Vec<String>,
}

#[derive(Debug, Clone, Copy)]
pub enum ReserveScope {
    All,
    Provider(&'static str),
}

#[derive(Debug)]
pub enum ReserveError {
    AtCap { occupied: usize, cap: usize },
    OwnedByOther { name: String },
    OtherAccount { name: String },
    Store(anyhow::Error),
}

/// Removes a newly reserved intent if creation exits before a provider-side
/// resource-creating call can have taken effect.
pub struct IntentGuard {
    directory: PathBuf,
    name: String,
    armed: bool,
}

impl IntentGuard {
    pub fn new(name: &str) -> Self {
        Self::at(&dir(), name)
    }

    fn at(directory: &Path, name: &str) -> Self {
        Self {
            directory: directory.to_path_buf(),
            name: name.to_owned(),
            armed: true,
        }
    }

    /// Keep the intent for reconciliation once a remote create may have begun.
    pub fn preserve(&mut self) {
        self.armed = false;
    }
}

impl Drop for IntentGuard {
    fn drop(&mut self) {
        if self.armed {
            remove_at(&self.directory, &self.name);
        }
    }
}

impl std::fmt::Display for ReserveError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::AtCap { occupied, cap } => {
                write!(
                    formatter,
                    "registry capacity reached ({occupied} occupied; cap {cap})"
                )
            }
            Self::OwnedByOther { name } => write!(
                formatter,
                "vps registry: sandbox name {name} is bound to a different worktree; refusing to reuse it"
            ),
            Self::OtherAccount { name } => write!(
                formatter,
                "vps registry: sandbox name {name} is recorded under a different provider account; refusing to reuse it"
            ),
            Self::Store(error) => write!(formatter, "registry reservation failed: {error}"),
        }
    }
}

impl std::error::Error for ReserveError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::AtCap { .. } => None,
            Self::OwnedByOther { .. } => None,
            Self::OtherAccount { .. } => None,
            Self::Store(error) => Some(error.as_ref()),
        }
    }
}

impl From<anyhow::Error> for ReserveError {
    fn from(error: anyhow::Error) -> Self {
        Self::Store(error)
    }
}

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
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub owner: String,
    /// Credential-free provider-account fingerprint; empty means legacy/unknown.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub account: String,
}

/// Whether `name` is a bake source name: the fixed prefix and exactly sixteen
/// lowercase hexadecimal digits.
pub fn is_bake_name(name: &str) -> bool {
    name.strip_prefix(BAKE_NAME_PREFIX).is_some_and(|suffix| {
        suffix.len() == 16
            && suffix
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    })
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

fn open_root(d: &Path, create: bool) -> Result<Option<thegn_core::fsperm::PrivateDir>> {
    if create {
        thegn_core::fsperm::ensure_private_dir(d)
            .context("vps registry: registry root is not an owned private directory")?;
        return thegn_core::fsperm::open_private_dir(d)
            .map(Some)
            .context("vps registry: registry root is not an owned private directory");
    }

    let metadata = match std::fs::symlink_metadata(d) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    anyhow::ensure!(
        !metadata.file_type().is_symlink()
            && metadata.is_dir()
            && thegn_core::fsperm::owned_by_current_user(&metadata),
        "vps registry: registry root is not an owned real directory"
    );
    // A concurrent root swap could make this path-based chmod follow a new
    // symlink before open_private_dir's O_NOFOLLOW check; mutation requires
    // write access to the owner-only registry directory.
    thegn_core::fsperm::restrict_dir_to_owner(d)
        .context("vps registry: registry root is not an owned private directory")?;
    thegn_core::fsperm::open_private_dir(d)
        .map(Some)
        .context("vps registry: registry root is not an owned private directory")
}

pub fn ensure_known_hosts_dir_at(d: &Path) -> Result<PathBuf> {
    let _root = open_root(d, true)?;
    thegn_core::fsperm::ensure_private_dir_beneath(d, Path::new("known_hosts.d"))
        .map(|directory| directory.path().to_path_buf())
        .context("vps registry: known_hosts.d is not an owned private directory")
}

pub fn ensure_known_hosts_dir() -> Result<PathBuf> {
    ensure_known_hosts_dir_at(&dir())
}

/// Write (atomically: tmp + rename) a record into `d`.
pub fn write_at(d: &Path, rec: &VpsRecord) -> Result<()> {
    checked_name(&rec.name)?;
    let root = open_root(d, true)?.ok_or_else(|| {
        anyhow::anyhow!("vps registry: registry root was not opened after creation")
    })?;
    let mut record = rec.clone();
    if record.owner.is_empty() {
        match read_in(&root, &record.name) {
            Some(existing) if !existing.owner.is_empty() => record.owner = existing.owner,
            Some(_) => {}
            None => {
                if record_exists_in(&root, &record.name)? {
                    anyhow::bail!(
                        "vps registry: cannot preserve owner for unreadable existing record {}",
                        record.name
                    );
                }
            }
        }
    }
    if record.account.is_empty() {
        match read_in(&root, &record.name) {
            Some(existing) if !existing.account.is_empty() => {
                record.account = existing.account;
            }
            Some(_) => {}
            None => {
                if record_exists_in(&root, &record.name)? {
                    anyhow::bail!(
                        "vps registry: cannot preserve account for unreadable existing record {}",
                        record.name
                    );
                }
            }
        }
    }
    let bytes = serde_json::to_vec_pretty(&record)?;
    root.write_owner_only_atomic(&format!("{}.json", rec.name), &bytes)
        .with_context(|| format!("publish VPS registry record {}", rec.name))?;
    Ok(())
}

pub fn write(rec: &VpsRecord) -> Result<()> {
    write_at(&dir(), rec)
}

fn record_exists_in(root: &thegn_core::fsperm::PrivateDir, name: &str) -> Result<bool> {
    root.entry_exists_nofollow(&format!("{name}.json"))
        .map_err(Into::into)
}

/// Capacity is per provider account. Unknown-account records (legacy or unreadable) count against every account,
/// and an unstamped reservation counts everything.
fn occupied_at(
    root: &thegn_core::fsperm::PrivateDir,
    scope: ReserveScope,
    account: &str,
) -> Result<usize> {
    let entries = root.file_names()?;
    let mut occupied = 0;
    for file_name in entries {
        if Path::new(&file_name)
            .extension()
            .is_none_or(|extension| extension != "json")
        {
            continue;
        }
        let stem = Path::new(&file_name)
            .file_stem()
            .and_then(|stem| stem.to_str());
        let other_record = if matches!(scope, ReserveScope::Provider(_)) || !account.is_empty() {
            stem.and_then(|name| read_in(root, name))
                .is_some_and(|record| {
                    matches!(scope, ReserveScope::Provider(provider) if record.provider != provider)
                        || (!account.is_empty()
                            && !record.account.is_empty()
                            && record.account != account)
                })
        } else {
            false
        };
        if !other_record {
            occupied += 1;
        }
    }
    Ok(occupied)
}

pub fn reserve_at(
    d: &Path,
    rec: &VpsRecord,
    cap: usize,
    scope: ReserveScope,
) -> std::result::Result<(), ReserveError> {
    checked_name(&rec.name).map_err(ReserveError::Store)?;
    let root = open_root(d, true)
        .map_err(ReserveError::Store)?
        .ok_or_else(|| {
            ReserveError::Store(anyhow::anyhow!(
                "vps registry: registry root was not opened after creation"
            ))
        })?;
    let lock = root
        .open_or_create_regular_nofollow(RESERVE_LOCK)
        .map_err(anyhow::Error::from)
        .map_err(ReserveError::Store)?;
    lock.lock()
        .map_err(anyhow::Error::from)
        .map_err(ReserveError::Store)?;
    if !rec.owner.is_empty() {
        match read_in(&root, &rec.name) {
            Some(existing) if !existing.owner.is_empty() && existing.owner != rec.owner => {
                return Err(ReserveError::OwnedByOther {
                    name: rec.name.clone(),
                });
            }
            Some(_) => {}
            None => match record_exists_in(&root, &rec.name) {
                Ok(true) => {
                    return Err(ReserveError::Store(anyhow::anyhow!(
                        "vps registry: cannot verify owner for existing record {}",
                        rec.name
                    )));
                }
                Ok(false) => {}
                Err(error) => return Err(ReserveError::Store(error)),
            },
        }
    }
    if !rec.account.is_empty() {
        match read_in(&root, &rec.name) {
            Some(existing) if !existing.account.is_empty() && existing.account != rec.account => {
                return Err(ReserveError::OtherAccount {
                    name: rec.name.clone(),
                });
            }
            Some(_) => {}
            None => match record_exists_in(&root, &rec.name) {
                Ok(true) => {
                    return Err(ReserveError::Store(anyhow::anyhow!(
                        "vps registry: cannot verify account for existing record {}",
                        rec.name
                    )));
                }
                Ok(false) => {}
                Err(error) => return Err(ReserveError::Store(error)),
            },
        }
    }
    let occupied = occupied_at(&root, scope, &rec.account).map_err(ReserveError::Store)?;
    if occupied >= cap {
        return Err(ReserveError::AtCap { occupied, cap });
    }
    write_at(d, rec).map_err(ReserveError::Store)
}

/// A stable, versioned digest of the exact worktree path bytes.
pub fn worktree_owner(worktree: &[u8]) -> String {
    let mut digest = Sha256::new();
    digest.update(b"thegn.provider-owner.v1\0");
    digest.update(worktree);
    let bytes = digest.finalize();
    let digest_hex = bytes[..16]
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    format!("wt1-{digest_hex}")
}

/// A stable, credential-free fingerprint for one provider account authority.
pub fn account_fingerprint(kind: &str, authority: &str, credential_ref: &str) -> String {
    let mut digest = Sha256::new();
    digest.update(b"thegn.provider-account.v1\0");
    digest.update(kind.as_bytes());
    digest.update([0]);
    digest.update(authority.as_bytes());
    digest.update([0]);
    digest.update(credential_ref.as_bytes());
    let bytes = digest.finalize();
    let digest_hex = bytes[..16]
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    format!("acct1-{digest_hex}")
}

pub fn check_owner_at(d: &Path, name: &str, owner: &str) -> Result<()> {
    checked_name(name)?;
    let Some(root) = open_root(d, false)? else {
        return Ok(());
    };
    let record = read_in(&root, name);
    if let Some(record) = record.as_ref()
        && !record.owner.is_empty()
        && record.owner != owner
    {
        anyhow::bail!(ReserveError::OwnedByOther {
            name: name.to_owned()
        });
    }
    if record.is_some() {
        return Ok(());
    }
    if record_exists_in(&root, name)? {
        anyhow::bail!("vps registry: cannot verify owner for existing record {name}");
    }
    Ok(())
}

pub fn check_owner(name: &str, owner: &str) -> Result<()> {
    check_owner_at(&dir(), name, owner)
}

pub fn reserve(
    rec: &VpsRecord,
    cap: usize,
    scope: ReserveScope,
) -> std::result::Result<(), ReserveError> {
    reserve_at(&dir(), rec, cap, scope)
}

pub fn read_at(d: &Path, name: &str) -> Option<VpsRecord> {
    checked_name(name).ok()?;
    let root = open_root(d, false).ok()??;
    read_in(&root, name)
}

fn read_in(root: &thegn_core::fsperm::PrivateDir, name: &str) -> Option<VpsRecord> {
    checked_name(name).ok()?;
    let mut file = root.open_regular_nofollow(&format!("{name}.json")).ok()?;
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
    let root = match open_root(d, false) {
        Ok(Some(root)) => root,
        Ok(None) => return,
        Err(error) => {
            tracing::debug!(target: "thegn::vps", %error, path = %d.display(), "unable to open VPS registry for removal");
            return;
        }
    };
    if let Err(error) = root.remove_file(&format!("{name}.json")) {
        tracing::debug!(target: "thegn::vps", %error, name, "unable to remove VPS registry record");
    }
    if let Err(error) = remove_pin_in(&root, d, name) {
        tracing::debug!(target: "thegn::vps", %error, name, "unable to remove VPS known_hosts pin");
    }
}

pub fn remove(name: &str) {
    remove_at(&dir(), name)
}

fn remove_pin_from_held_dir(
    known_hosts: &thegn_core::fsperm::PrivateDir,
    name: &str,
) -> Result<()> {
    known_hosts
        .remove_file(name)
        .context("vps registry: cannot remove the known_hosts pin")?;
    Ok(())
}

fn remove_pin_in(root: &thegn_core::fsperm::PrivateDir, d: &Path, name: &str) -> Result<()> {
    if root.entry_exists_nofollow("known_hosts.d")? {
        let known_hosts = thegn_core::fsperm::open_private_dir(&d.join("known_hosts.d"))
            .context("vps registry: known_hosts.d is not an owned private directory")?;
        remove_pin_from_held_dir(&known_hosts, name)?;
    }
    Ok(())
}

/// Remove one known_hosts pin without touching the same-name VPS record.
pub fn remove_known_hosts_pin_at(d: &Path, name: &str) -> Result<()> {
    checked_name(name)?;
    let Some(root) = open_root(d, false)? else {
        return Ok(());
    };
    remove_pin_in(&root, d, name)
}

/// Remove one known_hosts pin from the active VPS registry without touching its record.
pub fn remove_known_hosts_pin(name: &str) -> Result<()> {
    remove_known_hosts_pin_at(&dir(), name)
}

/// Lenient view for display; reapers use `list_checked`.
pub fn list_at(d: &Path) -> Vec<VpsRecord> {
    let Ok(Some(root)) = open_root(d, false) else {
        return Vec::new();
    };
    records_in(&root).map_or_else(|_| Vec::new(), |(records, _)| records)
}

fn records_in(root: &thegn_core::fsperm::PrivateDir) -> Result<(Vec<VpsRecord>, Vec<String>)> {
    let mut records = Vec::new();
    let mut unreadable = Vec::new();
    for file_name in root.file_names()? {
        if Path::new(&file_name)
            .extension()
            .is_none_or(|extension| extension != "json")
        {
            continue;
        }
        let Some(stem) = Path::new(&file_name)
            .file_stem()
            .and_then(|stem| stem.to_str())
        else {
            continue;
        };
        if checked_name(stem).is_err() {
            continue;
        }
        match read_in(root, stem) {
            Some(record) => records.push(record),
            None => unreadable.push(stem.to_owned()),
        }
    }
    Ok((records, unreadable))
}

/// Strict ledger view for lifecycle decisions. A root or directory traversal
/// error invalidates the complete view; individual malformed records are named.
pub fn list_checked_at(d: &Path) -> Result<LedgerView> {
    let Some(root) = open_root(d, false).context(
        "vps registry: registry root is not an owned real directory; refusing to list the ledger",
    )?
    else {
        return Ok(LedgerView::default());
    };
    let (records, unreadable) =
        records_in(&root).context("vps registry: could not read registry directory")?;
    Ok(LedgerView {
        records,
        unreadable,
    })
}

pub fn list_checked() -> Result<LedgerView> {
    list_checked_at(&dir())
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
            owner: String::new(),
            account: String::new(),
        }
    }

    #[test]
    fn checked_ledger_refuses_symlinked_root() {
        let tmp = tempfile::tempdir().unwrap();
        let real = tmp.path().join("real");
        let link = tmp.path().join("link");
        std::fs::create_dir(&real).unwrap();
        write_at(&real, &rec("a", "ready")).unwrap();
        if let Err(error) = thegn_core::fsperm::create_symlink(&real, &link, true) {
            if matches!(
                error.kind(),
                std::io::ErrorKind::Unsupported | std::io::ErrorKind::PermissionDenied
            ) {
                return;
            }
            panic!("could not create test symlink: {error}");
        }
        assert!(list_checked_at(&link).is_err());
        assert!(read_at(&link, "a").is_none());
        let listed = list_checked_at(&real).unwrap();
        assert_eq!(listed.records, vec![rec("a", "ready")]);
        assert!(listed.unreadable.is_empty());
    }

    #[test]
    fn checked_ledger_reports_corrupt_records() {
        let tmp = tempfile::tempdir().unwrap();
        let d = tmp.path();
        write_at(d, &rec("a", "ready")).unwrap();
        std::fs::write(d.join("junk.json"), b"not json").unwrap();
        std::fs::write(
            d.join("b.json"),
            serde_json::to_vec(&rec("c", "ready")).unwrap(),
        )
        .unwrap();
        std::fs::write(
            d.join("d.json"),
            serde_json::to_vec(&rec("../x", "ready")).unwrap(),
        )
        .unwrap();
        std::fs::write(d.join("readme.txt"), b"x").unwrap();
        std::fs::write(d.join(RESERVE_LOCK), b"").unwrap();

        let mut view = list_checked_at(d).unwrap();
        assert_eq!(view.records, vec![rec("a", "ready")]);
        view.unreadable.sort();
        assert_eq!(
            view.unreadable,
            vec!["b".to_owned(), "d".to_owned(), "junk".to_owned()]
        );
    }

    #[test]
    fn checked_ledger_missing_root_is_empty() {
        let tmp = tempfile::tempdir().unwrap();
        let view = list_checked_at(&tmp.path().join("missing")).unwrap();
        assert!(view.records.is_empty());
        assert!(view.unreadable.is_empty());
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
            let path = entry.unwrap().path();
            let name = path
                .file_name()
                .and_then(|name| name.to_str())
                .unwrap_or("");
            !name.starts_with(".secret.tmp-") && !name.ends_with(".tmp")
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
            let path = entry.unwrap().path();
            let name = path
                .file_name()
                .and_then(|name| name.to_str())
                .unwrap_or("");
            !name.starts_with(".secret.tmp-") && !name.ends_with(".tmp")
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

    #[test]
    fn concurrent_reserves_never_exceed_the_cap() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().to_path_buf();
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(12));
        let workers: Vec<_> = (0..12)
            .map(|index| {
                let dir = dir.clone();
                let barrier = std::sync::Arc::clone(&barrier);
                std::thread::spawn(move || {
                    let record = rec(&format!("n{index}"), "creating");
                    barrier.wait();
                    reserve_at(&dir, &record, 3, ReserveScope::All)
                })
            })
            .collect();
        let mut reserved = 0;
        let mut refused = 0;
        for worker in workers {
            match worker.join().unwrap() {
                Ok(()) => reserved += 1,
                Err(ReserveError::AtCap { .. }) => refused += 1,
                Err(ReserveError::OwnedByOther { .. }) => {
                    panic!("owner refusal is not expected for distinct names")
                }
                Err(ReserveError::OtherAccount { .. }) => {
                    panic!("account refusal is not expected for unstamped records")
                }
                Err(ReserveError::Store(error)) => panic!("reservation store error: {error}"),
            }
        }
        assert_eq!(reserved, 3);
        assert_eq!(refused, 9);
        assert_eq!(list_at(&dir).len(), 3);
    }

    #[test]
    fn corrupt_and_unreadable_records_count_as_occupied() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        std::fs::write(dir.join("bad.json"), b"{not json").unwrap();
        write_at(dir, &rec("valid", "creating")).unwrap();

        assert!(matches!(
            reserve_at(dir, &rec("blocked", "creating"), 2, ReserveScope::All),
            Err(ReserveError::AtCap {
                occupied: 2,
                cap: 2
            })
        ));
        reserve_at(dir, &rec("allowed", "creating"), 3, ReserveScope::All).unwrap();
        std::fs::create_dir(dir.join("unreadable.json")).unwrap();
        let result = reserve_at(
            dir,
            &rec("blocked-by-unreadable", "creating"),
            4,
            ReserveScope::All,
        );
        assert!(
            matches!(
                &result,
                Err(ReserveError::AtCap {
                    occupied: 4,
                    cap: 4
                })
            ),
            "unexpected reservation result: {result:?}"
        );
    }

    #[test]
    fn provider_scope_counts_own_and_unknown_records_only() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        write_at(dir, &rec("hetzner", "creating")).unwrap();
        std::fs::write(dir.join("unknown.json"), b"{not json").unwrap();

        let blocked = reserve_at(
            dir,
            &rec("fly-blocked", "creating"),
            1,
            ReserveScope::Provider("fly"),
        );
        assert!(
            matches!(
                &blocked,
                Err(ReserveError::AtCap {
                    occupied: 1,
                    cap: 1
                })
            ),
            "unexpected reservation result: {blocked:?}"
        );
        assert_eq!(list_at(dir).len(), 1, "failed reservation wrote a record");
        reserve_at(
            dir,
            &rec("fly-allowed", "creating"),
            2,
            ReserveScope::Provider("fly"),
        )
        .unwrap();
        assert_eq!(list_at(dir).len(), 2);

        let only_hetzner = tempfile::tempdir().unwrap();
        write_at(only_hetzner.path(), &rec("hetzner", "creating")).unwrap();
        reserve_at(
            only_hetzner.path(),
            &rec("fly", "creating"),
            1,
            ReserveScope::Provider("fly"),
        )
        .unwrap();
    }

    #[test]
    fn reserve_refuses_symlinked_root_and_lock() {
        let tmp = tempfile::tempdir().unwrap();
        let real = tmp.path().join("real");
        let root_link = tmp.path().join("root-link");
        std::fs::create_dir(&real).unwrap();
        if let Err(error) = thegn_core::fsperm::create_symlink(&real, &root_link, true) {
            if matches!(
                error.kind(),
                std::io::ErrorKind::Unsupported | std::io::ErrorKind::PermissionDenied
            ) {
                return;
            }
            panic!("could not create test symlink: {error}");
        }
        assert!(matches!(
            reserve_at(&root_link, &rec("root", "creating"), 1, ReserveScope::All),
            Err(ReserveError::Store(_))
        ));
        assert!(!real.join(RESERVE_LOCK).exists());
        assert!(!real.join("root.json").exists());

        let dir = tmp.path().join("registry");
        std::fs::create_dir(&dir).unwrap();
        let outside = tmp.path().join("outside-lock");
        std::fs::write(&outside, b"leave unchanged").unwrap();
        if let Err(error) =
            thegn_core::fsperm::create_symlink(&outside, &dir.join(RESERVE_LOCK), false)
        {
            if matches!(
                error.kind(),
                std::io::ErrorKind::Unsupported | std::io::ErrorKind::PermissionDenied
            ) {
                return;
            }
            panic!("could not create test symlink: {error}");
        }
        assert!(matches!(
            reserve_at(&dir, &rec("lock", "creating"), 1, ReserveScope::All),
            Err(ReserveError::Store(_))
        ));
        assert_eq!(std::fs::read(&outside).unwrap(), b"leave unchanged");
        assert!(!dir.join("lock.json").exists());
    }

    #[test]
    fn list_ignores_the_lock_file() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        let record = rec("reserved", "creating");
        reserve_at(dir, &record, 1, ReserveScope::All).unwrap();
        assert_eq!(list_at(dir), vec![record]);
    }

    #[test]
    fn intent_guard_clears_pre_create_failure_and_preserves_ambiguous_intent() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        reserve_at(dir, &rec("cleanup", "creating"), 1, ReserveScope::All).unwrap();
        drop(IntentGuard::at(dir, "cleanup"));
        assert!(read_at(dir, "cleanup").is_none());

        reserve_at(dir, &rec("preserve", "creating"), 1, ReserveScope::All).unwrap();
        let mut guard = IntentGuard::at(dir, "preserve");
        guard.preserve();
        drop(guard);
        assert_eq!(read_at(dir, "preserve").unwrap().state, "creating");
    }

    #[test]
    fn bake_names_are_recognised_exactly() {
        assert!(is_bake_name("tg-bake-00010203fafbfeff"));
        assert!(!is_bake_name("tg-bake-00010203fafbfee"));
        assert!(!is_bake_name("tg-bake-00010203fafbfeff0"));
        assert!(!is_bake_name("tg-bake-00010203FAFBFEFF"));
        assert!(!is_bake_name("tg-bake-0001020304050607.x"));
        assert!(!is_bake_name("tg-auto-cx32-ab"));
        assert!(!is_bake_name(""));
    }

    #[test]
    fn owner_mismatch_refuses_reservation_before_counting() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        let mut existing = rec("tg-a-x1", "ready");
        existing.owner = worktree_owner(b"/worktree/owner-a");
        write_at(dir, &existing).unwrap();
        let candidate = VpsRecord {
            owner: worktree_owner(b"/worktree/owner-b"),
            ..rec("tg-a-x1", "creating")
        };

        assert!(matches!(
            reserve_at(dir, &candidate, 0, ReserveScope::All),
            Err(ReserveError::OwnedByOther { name }) if name == "tg-a-x1"
        ));
        assert_eq!(read_at(dir, "tg-a-x1").unwrap().owner, existing.owner);
    }

    #[test]
    fn legacy_record_is_claimed_on_first_owned_reservation() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        write_at(dir, &rec("tg-a-x1", "ready")).unwrap();
        let candidate = VpsRecord {
            owner: worktree_owner(b"/worktree/owner-b"),
            ..rec("tg-a-x1", "creating")
        };

        reserve_at(dir, &candidate, 5, ReserveScope::All).unwrap();

        assert_eq!(read_at(dir, "tg-a-x1").unwrap().owner, candidate.owner);
    }

    #[test]
    fn owner_less_rewrite_keeps_the_binding() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        let mut existing = rec("tg-a-x1", "creating");
        existing.owner = worktree_owner(b"/worktree/owner-a");
        write_at(dir, &existing).unwrap();
        write_at(dir, &rec("tg-a-x1", "ready")).unwrap();

        assert_eq!(read_at(dir, "tg-a-x1").unwrap().owner, existing.owner);
    }

    #[test]
    fn ownerless_rewrite_refuses_unreadable_existing_record() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        let path = dir.join("tg-a-x1.json");
        let malformed = b"not json";
        std::fs::write(&path, malformed).unwrap();

        assert!(write_at(dir, &rec("tg-a-x1", "ready")).is_err());
        assert_eq!(std::fs::read(path).unwrap(), malformed);
    }

    #[test]
    fn worktree_owner_is_versioned_and_path_exact() {
        let owner = worktree_owner(b"/w/a");
        assert_eq!(owner, worktree_owner(b"/w/a"));
        assert_ne!(owner, worktree_owner(b"/w/a/"));
        assert!(owner.starts_with("wt1-"));
        assert_eq!(owner.len(), 36);
    }

    #[test]
    fn old_json_without_owner_parses() {
        let json = r#"{"name":"tg-a-x1","provider":"hetzner","state":"ready","instance_id":"i-1","ip":"192.0.2.1","created_at":7}"#;
        let record: VpsRecord = serde_json::from_str(json).unwrap();
        assert!(record.owner.is_empty());
        let encoded = serde_json::to_value(record).unwrap();
        assert!(encoded.get("owner").is_none());
    }

    #[test]
    fn account_fingerprint_is_deterministic_and_credential_free() {
        let account = account_fingerprint("hetzner", "api.example", "HCLOUD_TOKEN");
        assert_eq!(
            account,
            account_fingerprint("hetzner", "api.example", "HCLOUD_TOKEN")
        );
        assert!(account.starts_with("acct1-"));
        assert_eq!(account.len(), 38);
        assert!(account[6..].bytes().all(|byte| byte.is_ascii_hexdigit()));
        assert!(!account.contains("HCLOUD_TOKEN"));
        assert_ne!(
            account,
            account_fingerprint("fly", "api.example", "HCLOUD_TOKEN")
        );
        assert_ne!(
            account,
            account_fingerprint("hetzner", "other.example", "HCLOUD_TOKEN")
        );
        assert_ne!(
            account,
            account_fingerprint("hetzner", "api.example", "HCLOUD_TOKEN_WORK")
        );
    }

    #[test]
    fn account_reserve_refuses_a_record_of_another_account() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        let mut first = rec("tg-a-x1", "creating");
        first.account = account_fingerprint("hetzner", "api.example", "TOKEN_A");
        reserve_at(dir, &first, 5, ReserveScope::All).unwrap();

        let mut second = rec("tg-a-x1", "creating");
        second.account = account_fingerprint("hetzner", "api.example", "TOKEN_B");
        assert!(matches!(
            reserve_at(dir, &second, 5, ReserveScope::All),
            Err(ReserveError::OtherAccount { name }) if name == "tg-a-x1"
        ));
        assert_eq!(read_at(dir, "tg-a-x1").unwrap().account, first.account);
    }

    #[test]
    fn account_legacy_json_without_account_still_parses() {
        let json = r#"{"name":"tg-a-x1","provider":"hetzner","state":"ready","instance_id":"i-1","ip":"192.0.2.1","created_at":7}"#;
        let record: VpsRecord = serde_json::from_str(json).unwrap();
        assert!(record.account.is_empty());
        let encoded = serde_json::to_value(rec("tg-a-x1", "ready")).unwrap();
        assert!(encoded.get("account").is_none());
    }

    #[test]
    fn account_empty_rewrite_preserves_existing_stamp() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        let mut stamped = rec("tg-a-x1", "ready");
        stamped.account = account_fingerprint("hetzner", "api.example", "TOKEN_A");
        write_at(dir, &stamped).unwrap();

        let mut rewrite = rec("tg-a-x1", "ready");
        rewrite.ip = "192.0.2.9".into();
        write_at(dir, &rewrite).unwrap();

        assert_eq!(read_at(dir, "tg-a-x1").unwrap().account, stamped.account);
    }

    #[test]
    fn known_hosts_dir_is_created_private_beneath_the_registry_root() {
        let tmp = tempfile::tempdir().unwrap();
        let d = tmp.path().join("vps");

        let path = ensure_known_hosts_dir_at(&d).unwrap();

        assert_eq!(path, d.join("known_hosts.d"));
        assert!(path.is_dir());
        for directory in [&d, &path] {
            assert!(
                thegn_core::fsperm::mode_bits(directory)
                    .unwrap()
                    .is_none_or(|mode| mode & 0o777 == 0o700)
            );
        }
    }

    #[test]
    fn known_hosts_dir_refuses_a_symlinked_leaf() {
        let tmp = tempfile::tempdir().unwrap();
        let d = tmp.path().join("vps");
        write_at(&d, &rec("a", "ready")).unwrap();
        let victim = tmp.path().join("victim");
        std::fs::create_dir(&victim).unwrap();
        std::fs::write(victim.join("keep"), b"unchanged").unwrap();
        let mode_before = thegn_core::fsperm::mode_bits(&victim).unwrap();
        let link = d.join("known_hosts.d");
        if let Err(error) = thegn_core::fsperm::create_symlink(&victim, &link, true) {
            if matches!(
                error.kind(),
                std::io::ErrorKind::Unsupported | std::io::ErrorKind::PermissionDenied
            ) {
                return;
            }
            panic!("could not create test symlink: {error}");
        }

        assert!(ensure_known_hosts_dir_at(&d).is_err());
        assert_eq!(std::fs::read(victim.join("keep")).unwrap(), b"unchanged");
        assert_eq!(thegn_core::fsperm::mode_bits(&victim).unwrap(), mode_before);
    }

    #[test]
    fn write_tightens_an_existing_registry_root_to_owner_only() {
        let tmp = tempfile::tempdir().unwrap();
        let d = tmp.path().join("vps");
        std::fs::create_dir(&d).unwrap();

        write_at(&d, &rec("a", "ready")).unwrap();

        assert!(
            thegn_core::fsperm::mode_bits(&d)
                .unwrap()
                .is_none_or(|mode| mode & 0o777 == 0o700)
        );
        assert!(
            thegn_core::fsperm::mode_bits(&d.join("a.json"))
                .unwrap()
                .is_none_or(|mode| mode & 0o777 == 0o600)
        );
    }

    #[test]
    fn remove_removes_the_known_hosts_pin_through_the_private_directory() {
        let tmp = tempfile::tempdir().unwrap();
        let d = tmp.path().join("vps");
        write_at(&d, &rec("a", "ready")).unwrap();
        let known_hosts = ensure_known_hosts_dir_at(&d).unwrap();
        std::fs::write(known_hosts.join("a"), b"pinned key").unwrap();

        remove_at(&d, "a");

        assert!(!d.join("a.json").exists());
        assert!(!known_hosts.join("a").exists());
    }

    #[test]
    fn remove_known_hosts_pin_removes_only_the_named_pin() {
        let tmp = tempfile::tempdir().unwrap();
        let d = tmp.path().join("vps");
        write_at(&d, &rec("a", "ready")).unwrap();
        let known_hosts = ensure_known_hosts_dir_at(&d).unwrap();
        std::fs::write(known_hosts.join("a"), b"key a").unwrap();
        std::fs::write(known_hosts.join("b"), b"key b").unwrap();

        remove_known_hosts_pin_at(&d, "a").unwrap();

        assert!(!known_hosts.join("a").exists());
        assert!(known_hosts.join("b").exists());
        assert!(read_at(&d, "a").is_some());
    }

    #[test]
    fn remove_known_hosts_pin_is_ok_when_absent() {
        let tmp = tempfile::tempdir().unwrap();
        let d = tmp.path().join("vps");
        assert!(remove_known_hosts_pin_at(&d, "a").is_ok());

        write_at(&d, &rec("a", "ready")).unwrap();
        assert!(remove_known_hosts_pin_at(&d, "a").is_ok());

        let known_hosts = ensure_known_hosts_dir_at(&d).unwrap();
        assert!(remove_known_hosts_pin_at(&d, "a").is_ok());
        assert!(remove_known_hosts_pin_at(&d, "../x").is_err());
        assert!(known_hosts.exists());
    }

    #[test]
    fn remove_known_hosts_pin_refuses_a_symlinked_known_hosts_dir() {
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

        assert!(remove_known_hosts_pin_at(&d, "a").is_err());
        assert!(victim.join("a").exists());
    }

    #[test]
    fn reads_and_listing_stay_on_the_held_root_after_a_swap() {
        let tmp = tempfile::tempdir().unwrap();
        let d = tmp.path().join("vps");
        write_at(&d, &rec("a", "ready")).unwrap();
        let root = open_root(&d, false).unwrap().unwrap();
        let moved = tmp.path().join("moved");
        std::fs::rename(&d, &moved).unwrap();
        write_at(&d, &rec("b", "ready")).unwrap();

        assert_eq!(read_in(&root, "a"), Some(rec("a", "ready")));
        assert!(read_in(&root, "b").is_none());
        let (records, unreadable) = records_in(&root).unwrap();
        assert_eq!(records, vec![rec("a", "ready")]);
        assert!(unreadable.is_empty());
    }

    #[test]
    fn reserve_lock_is_created_owner_only_beneath_the_registry_root() {
        let tmp = tempfile::tempdir().unwrap();
        let d = tmp.path().join("vps");

        reserve_at(&d, &rec("a", "creating"), 1, ReserveScope::All).unwrap();

        assert!(
            thegn_core::fsperm::mode_bits(&d.join(RESERVE_LOCK))
                .unwrap()
                .is_none_or(|mode| mode & 0o777 == 0o600)
        );
    }

    #[test]
    fn occupied_count_skips_the_lock_and_counts_unreadable_records() {
        let tmp = tempfile::tempdir().unwrap();
        let d = tmp.path().join("vps");
        write_at(&d, &rec("ready", "ready")).unwrap();
        std::fs::write(d.join("bad.json"), b"not json").unwrap();

        assert!(matches!(
            reserve_at(&d, &rec("new", "creating"), 2, ReserveScope::All),
            Err(ReserveError::AtCap {
                occupied: 2,
                cap: 2
            })
        ));
    }

    #[test]
    fn account_capacity_ignores_records_of_another_account() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        let account_a = account_fingerprint("hetzner", "api.example", "TOKEN_A");
        let account_b = account_fingerprint("hetzner", "api.example", "TOKEN_B");
        let mut first = rec("a-one", "creating");
        first.account = account_a.clone();
        write_at(dir, &first).unwrap();
        let mut second = rec("a-two", "creating");
        second.account = account_a;
        write_at(dir, &second).unwrap();

        let mut candidate = rec("b-one", "creating");
        candidate.account = account_b;
        let result = reserve_at(dir, &candidate, 1, ReserveScope::All);
        assert!(
            matches!(&result, Ok(())),
            "unexpected reservation result: {result:?}"
        );
        assert_eq!(read_at(dir, "b-one"), Some(candidate));
    }

    #[test]
    fn account_capacity_provider_scope_combines_with_account() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        let account_a = account_fingerprint("hetzner", "api.example", "TOKEN_A");
        let account_b = account_fingerprint("hetzner", "api.example", "TOKEN_B");
        let mut existing = rec("a-fly", "creating");
        existing.provider = "fly".into();
        existing.account = account_a;
        write_at(dir, &existing).unwrap();

        let mut candidate = rec("b-fly", "creating");
        candidate.provider = "fly".into();
        candidate.account = account_b;
        let result = reserve_at(dir, &candidate, 1, ReserveScope::Provider("fly"));
        assert!(
            matches!(&result, Ok(())),
            "unexpected reservation result: {result:?}"
        );
        assert_eq!(read_at(dir, "b-fly"), Some(candidate));
    }

    #[test]
    fn account_capacity_counts_same_account_and_legacy_records() {
        // positive control
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        let mut stamped = rec("same-account", "creating");
        stamped.account = account_fingerprint("hetzner", "api.example", "TOKEN_A");
        write_at(dir, &stamped).unwrap();
        write_at(dir, &rec("legacy", "creating")).unwrap();

        let mut candidate = rec("blocked", "creating");
        candidate.account = stamped.account;
        let result = reserve_at(dir, &candidate, 2, ReserveScope::All);
        assert!(
            matches!(
                &result,
                Err(ReserveError::AtCap {
                    occupied: 2,
                    cap: 2
                })
            ),
            "unexpected reservation result: {result:?}"
        );
    }

    #[test]
    fn account_capacity_counts_unreadable_records_for_every_account() {
        // positive control
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        std::fs::write(dir.join("bad.json"), b"{not json").unwrap();

        let mut candidate = rec("blocked", "creating");
        candidate.account = account_fingerprint("hetzner", "api.example", "TOKEN_B");
        let result = reserve_at(dir, &candidate, 1, ReserveScope::All);
        assert!(
            matches!(
                &result,
                Err(ReserveError::AtCap {
                    occupied: 1,
                    cap: 1
                })
            ),
            "unexpected reservation result: {result:?}"
        );
    }

    #[test]
    fn account_capacity_unstamped_candidate_counts_every_account() {
        // positive control
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        let mut account_a = rec("account-a", "creating");
        account_a.account = account_fingerprint("hetzner", "api.example", "TOKEN_A");
        write_at(dir, &account_a).unwrap();
        let mut account_b = rec("account-b", "creating");
        account_b.account = account_fingerprint("hetzner", "api.example", "TOKEN_B");
        write_at(dir, &account_b).unwrap();

        let result = reserve_at(dir, &rec("unstamped", "creating"), 2, ReserveScope::All);
        assert!(
            matches!(
                &result,
                Err(ReserveError::AtCap {
                    occupied: 2,
                    cap: 2
                })
            ),
            "unexpected reservation result: {result:?}"
        );
    }
}
