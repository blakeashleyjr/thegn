//! Secret-free custody ledger for SSH keys authorized on managed sandboxes.
//!
//! One JSON record per provider instance captures the exact private-key path
//! and public-key fingerprint authorized at provision time. The private key and
//! public-key body never enter the ledger. Provider destroy removes the record
//! only after remote deletion succeeds, so a failed teardown remains visible to
//! rotation/audit rather than silently losing revocation state.
//! Provisioning never replaces a row with pending rotation recovery.
//! Every mutation runs under one process-shared ledger lock; rotation writes
//! are compare-and-swap against the row they read, and every publish stamps a
//! ledger-wide monotonic generation.

use std::io::Read;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

#[derive(Debug)]
pub struct RotationRecoveryPending {
    pub provider: String,
    pub instance: String,
}

impl std::fmt::Display for RotationRecoveryPending {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "managed SSH custody for {}/{} has an unfinished key rotation; run `thegn secret ssh rotate` to finish it before provisioning again (nothing was changed)",
            self.provider, self.instance
        )
    }
}

impl std::error::Error for RotationRecoveryPending {}

#[derive(Debug)]
pub struct CustodyChanged {
    pub provider: String,
    pub instance: String,
}

impl std::fmt::Display for CustodyChanged {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "managed SSH custody row {}/{} changed since this rotation read it; no further remote change was made",
            self.provider, self.instance
        )
    }
}

impl std::error::Error for CustodyChanged {}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AuthorizedKeyRecord {
    pub provider: String,
    pub account: String,
    pub instance: String,
    pub key_path: PathBuf,
    pub key_fingerprint: String,
    pub authorized_at: i64,
    /// Ledger-wide monotonic generation stamped by every custody publish under
    /// the ledger lock; rows written before generations existed read as 0.
    /// Compare-and-swap equality includes it, so a row revoked and re-published
    /// with identical fields never matches an older snapshot.
    #[serde(default)]
    pub generation: u64,
    /// Host-side worktree identity needed to reconstruct transports whose
    /// proxy is worktree-bound (currently Sprites SSH-over-WSS). Optional so
    /// pre-field custody records continue to deserialize and fail closed when
    /// they cannot supply a connection proof.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub proxy_worktree: Option<String>,
    /// Recoverable state for a replacement key that may already be authorized
    /// remotely but has not become canonical. Keeping its value-free path and
    /// fingerprint in the ledger prevents a failed rotation from creating an
    /// invisible, indefinitely-live credential.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rotation_recovery: Option<RotationRecovery>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RotationRecovery {
    pub key_path: PathBuf,
    pub key_fingerprint: String,
    /// Planned retiring-key location. This is recorded before local promotion,
    /// so recovery can distinguish and undo an interruption on either side of
    /// the atomic rename sequence without storing key material.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub backup_key_path: Option<PathBuf>,
    pub authorized: bool,
    pub verified: bool,
    /// True before any old-key removal begins. Recovery then first re-adds the
    /// primary key everywhere using the auxiliary key, making cleanup safe even
    /// if the previous process stopped midway through remote retirement.
    #[serde(default)]
    pub retiring_started: bool,
    /// Set only after every remote accepted removal of the primary key. A
    /// partially-written final custody update can then be completed forward
    /// safely instead of rolling the filesystem back to a remotely retired key.
    #[serde(default)]
    pub retiring_completed: bool,
}

pub fn dir() -> PathBuf {
    crate::util::thegn_dir().join("ssh").join("authorized.d")
}

const GENERATION_FILE: &str = ".custody.generation";
const MAX_CUSTODY_ROW_BYTES: u64 = 64 * 1024;
const MAX_CUSTODY_ROWS: usize = 1024;

fn open_ledger_dir(dir: &Path) -> Result<Option<crate::fsperm::PrivateDir>> {
    match crate::fsperm::open_private_dir(dir) {
        Ok(private) => Ok(Some(private)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error)
            .with_context(|| format!("open managed SSH custody directory {}", dir.display())),
    }
}

fn read_generation_high_water(private: &crate::fsperm::PrivateDir) -> Result<u64> {
    let path = private.path().join(GENERATION_FILE);
    let mut file = match private.open_regular_nofollow(GENERATION_FILE) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(0),
        Err(error) => {
            return Err(error).with_context(|| {
                format!(
                    "read managed SSH custody generation counter {}",
                    path.display()
                )
            });
        }
    };
    let mut value = String::new();
    std::io::Read::take(&mut file, 64)
        .read_to_string(&mut value)
        .with_context(|| {
            format!(
                "managed SSH custody generation counter {} is corrupt",
                path.display()
            )
        })?;
    if value.len() > 32 {
        anyhow::bail!(
            "managed SSH custody generation counter {} is corrupt",
            path.display()
        );
    }
    value.trim().parse::<u64>().with_context(|| {
        format!(
            "managed SSH custody generation counter {} is corrupt",
            path.display()
        )
    })
}

fn next_generation(private: &crate::fsperm::PrivateDir, floor: u64) -> Result<u64> {
    let path = private.path().join(GENERATION_FILE);
    let next = read_generation_high_water(private)?
        .max(floor)
        .checked_add(1)
        .context("managed SSH custody generation counter is exhausted")?;
    private
        .write_owner_only_atomic(GENERATION_FILE, format!("{next}\n").as_bytes())
        .with_context(|| {
            format!(
                "publish managed SSH custody generation counter {}",
                path.display()
            )
        })?;
    Ok(next)
}

pub fn key_fingerprint(public_key: &str) -> String {
    try_key_fingerprint(public_key).unwrap_or_else(|_| {
        let material = public_key
            .split_whitespace()
            .take(2)
            .collect::<Vec<_>>()
            .join(" ");
        let digest = Sha256::digest(material.as_bytes());
        format!("invalid:{}", hex_prefix(&digest, 16))
    })
}

/// OpenSSH SHA-256 fingerprint of the decoded public-key wire blob.
pub fn try_key_fingerprint(public_key: &str) -> Result<String> {
    Ok(format!(
        "SHA256:{}",
        base64_unpadded(&Sha256::digest(decoded_key_blob(public_key)?))
    ))
}

fn decoded_key_blob(public_key: &str) -> Result<Vec<u8>> {
    let mut fields = public_key.split_whitespace();
    let key_type = fields.next().context("missing SSH public key type")?;
    let encoded = fields.next().context("missing SSH public key body")?;
    let blob = crate::provider_admission::decode_base64(encoded)
        .context("invalid SSH public key encoding")?;
    let length = blob
        .get(..4)
        .context("SSH public key blob is missing its type string")?;
    let type_len = u32::from_be_bytes(length.try_into()?) as usize;
    let type_end = 4usize
        .checked_add(type_len)
        .context("SSH public key type length overflow")?;
    let blob_type = blob
        .get(4..type_end)
        .context("SSH public key blob has a truncated type string")?;
    anyhow::ensure!(
        blob_type == key_type.as_bytes(),
        "SSH public key type does not match its blob"
    );
    Ok(blob)
}

pub fn fingerprint_is_verified(fingerprint: &str) -> bool {
    fingerprint.starts_with("SHA256:")
}

/// Provider-side name for thegn's managed public key: SHA-256 of the decoded
/// key blob, first 32 hex characters (128 bits). Comments and whitespace never
/// change it; an unparsable key is refused.
pub fn vendor_key_name(public_key: &str) -> Result<String> {
    let blob = decoded_key_blob(public_key)?;
    Ok(format!(
        "thegn-managed-{}",
        hex_prefix(&Sha256::digest(&blob), 32)
    ))
}

/// Fingerprint text for custody audit output, identifying pre-SHA256 rows.
pub fn fingerprint_audit_label(fingerprint: &str) -> String {
    if fingerprint_is_verified(fingerprint) {
        fingerprint.to_string()
    } else {
        format!("{fingerprint} (legacy, unverified)")
    }
}

fn base64_unpadded(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut encoded = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let first = chunk[0];
        let second = chunk.get(1).copied().unwrap_or(0);
        let third = chunk.get(2).copied().unwrap_or(0);
        encoded.push(char::from(ALPHABET[usize::from(first >> 2)]));
        encoded.push(char::from(
            ALPHABET[usize::from(((first & 0x03) << 4) | (second >> 4))],
        ));
        if chunk.len() > 1 {
            encoded.push(char::from(
                ALPHABET[usize::from(((second & 0x0f) << 2) | (third >> 6))],
            ));
        }
        if chunk.len() > 2 {
            encoded.push(char::from(ALPHABET[usize::from(third & 0x3f)]));
        }
    }
    encoded
}

fn hex_prefix(bytes: &[u8], chars: usize) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(chars);
    for byte in bytes.iter().take(chars.div_ceil(2)) {
        out.push(char::from(HEX[usize::from(byte >> 4)]));
        if out.len() < chars {
            out.push(char::from(HEX[usize::from(byte & 0x0f)]));
        }
    }
    out
}

/// Stable value-free account label recoverable by svc providers from the key
/// they were handed. The filename is already scoped from the configured
/// credential ref by the host factory; shared custody is named explicitly.
pub fn account_label(key_path: &Path) -> String {
    let name = key_path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("managed");
    if name == "sprite_ed25519" {
        "shared".to_string()
    } else {
        name.strip_suffix("_ed25519").unwrap_or(name).to_string()
    }
}

fn record_file_name(provider: &str, account: &str, instance: &str) -> String {
    let mut tuple = Vec::new();
    for part in [provider.as_bytes(), account.as_bytes(), instance.as_bytes()] {
        tuple.extend_from_slice(&(part.len() as u64).to_be_bytes());
        tuple.extend_from_slice(part);
    }
    let digest = Sha256::digest(&tuple);
    format!("{}.json", hex_prefix(&digest, 64))
}

fn legacy_file_name(provider: &str, account: &str, instance: &str) -> String {
    let id = crate::util::short_hash(&format!("{provider}\0{account}\0{instance}"), 24);
    format!("{id}.json")
}

#[cfg(test)]
fn record_path(dir: &Path, provider: &str, account: &str, instance: &str) -> PathBuf {
    dir.join(record_file_name(provider, account, instance))
}

#[cfg(test)]
fn legacy_record_path(dir: &Path, provider: &str, account: &str, instance: &str) -> PathBuf {
    dir.join(legacy_file_name(provider, account, instance))
}

fn check_tuple(
    record: &AuthorizedKeyRecord,
    provider: &str,
    account: &str,
    instance: &str,
    path: &Path,
) -> Result<()> {
    anyhow::ensure!(
        record.provider == provider && record.account == account && record.instance == instance,
        "custody collision or corruption at {}",
        path.display()
    );
    Ok(())
}

fn read_row(
    private: &crate::fsperm::PrivateDir,
    name: &str,
) -> Result<Option<AuthorizedKeyRecord>> {
    let path = private.path().join(name);
    let file = match private.open_regular_nofollow(name) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error).with_context(|| format!("read {}", path.display())),
    };
    let metadata = file
        .metadata()
        .with_context(|| format!("read {}", path.display()))?;
    anyhow::ensure!(
        crate::fsperm::owned_by_current_user(&metadata),
        "managed SSH custody record {} is owned by another user",
        path.display()
    );
    crate::fsperm::ensure_single_link_regular_file(&metadata).with_context(|| {
        format!(
            "managed SSH custody record {} has extra hard links",
            path.display()
        )
    })?;
    let mut body = Vec::new();
    std::io::Read::take(file, MAX_CUSTODY_ROW_BYTES + 1)
        .read_to_end(&mut body)
        .with_context(|| format!("read {}", path.display()))?;
    anyhow::ensure!(
        body.len() as u64 <= MAX_CUSTODY_ROW_BYTES,
        "managed SSH custody record {} exceeds {MAX_CUSTODY_ROW_BYTES} bytes",
        path.display()
    );
    serde_json::from_slice(&body)
        .with_context(|| format!("decode managed SSH custody record {}", path.display()))
        .map(Some)
}

pub fn record_authorized(
    provider: &str,
    account: &str,
    instance: &str,
    key_path: &Path,
    public_key: &str,
) -> Result<()> {
    record_authorized_with_proxy_worktree(provider, account, instance, key_path, public_key, None)
}

/// Record authorization plus optional value-free context for a worktree-bound
/// connection proxy. Secret refs, token values, and public-key bodies are never
/// stored here.
pub fn record_authorized_with_proxy_worktree(
    provider: &str,
    account: &str,
    instance: &str,
    key_path: &Path,
    public_key: &str,
    proxy_worktree: Option<&str>,
) -> Result<()> {
    let fingerprint = record_authorized_with_proxy_worktree_at(
        &dir(),
        provider,
        account,
        instance,
        key_path,
        public_key,
        proxy_worktree,
    )?;
    tracing::info!(
        target: "thegn::secret::audit",
        consumer = "managed-ssh:provision",
        provider,
        account,
        instance,
        key_fingerprint = %fingerprint,
        outcome = "authorized",
        "managed SSH key authorization recorded",
    );
    Ok(())
}

pub fn record_rotation_completed(
    expected: &AuthorizedKeyRecord,
    key_path: &Path,
    public_key: &str,
) -> Result<()> {
    let fingerprint = record_rotation_completed_at(&dir(), expected, key_path, public_key)?;
    tracing::info!(
        target: "thegn::secret::audit",
        consumer = "managed-ssh:rotate",
        provider = %expected.provider,
        account = %expected.account,
        instance = %expected.instance,
        key_fingerprint = %fingerprint,
        outcome = "rotated",
        "managed SSH key rotation recorded",
    );
    Ok(())
}

fn record_authorized_with_proxy_worktree_at(
    dir: &Path,
    provider: &str,
    account: &str,
    instance: &str,
    key_path: &Path,
    public_key: &str,
    proxy_worktree: Option<&str>,
) -> Result<String> {
    let record = authorized_record(
        provider,
        account,
        instance,
        key_path,
        public_key,
        proxy_worktree,
    )?;
    let ledger = lock_ledger(dir)?;
    if let Some(existing) = read_at_in(&ledger.dir, provider, account, instance)?
        && existing.rotation_recovery.is_some()
    {
        return Err(RotationRecoveryPending {
            provider: provider.to_string(),
            instance: instance.to_string(),
        }
        .into());
    }
    let published = publish_locked(&ledger.dir, &record)?;
    Ok(published.key_fingerprint)
}

pub fn record_rotation_completed_at(
    dir: &Path,
    expected: &AuthorizedKeyRecord,
    key_path: &Path,
    public_key: &str,
) -> Result<String> {
    let record = authorized_record(
        &expected.provider,
        &expected.account,
        &expected.instance,
        key_path,
        public_key,
        expected.proxy_worktree.as_deref(),
    )?;
    let published = publish_if_unchanged_at(dir, expected, &record)?;
    Ok(published.key_fingerprint)
}

fn authorized_record(
    provider: &str,
    account: &str,
    instance: &str,
    key_path: &Path,
    public_key: &str,
    proxy_worktree: Option<&str>,
) -> Result<AuthorizedKeyRecord> {
    Ok(AuthorizedKeyRecord {
        provider: provider.to_string(),
        account: account.to_string(),
        instance: instance.to_string(),
        key_path: key_path.to_path_buf(),
        key_fingerprint: try_key_fingerprint(public_key)?,
        authorized_at: crate::util::now(),
        generation: 0,
        proxy_worktree: proxy_worktree.map(str::to_string),
        rotation_recovery: None,
    })
}

pub fn record_authorized_at(
    dir: &Path,
    record: &AuthorizedKeyRecord,
) -> Result<AuthorizedKeyRecord> {
    let ledger = lock_ledger(dir)?;
    publish_locked(&ledger.dir, record)
}

struct LockedLedger {
    dir: crate::fsperm::PrivateDir,
    _lock: std::fs::File,
}

/// Acquire the process-shared custody ledger lock. Hold it only around local
/// file operations, never across remote calls; never acquire it twice in one
/// call chain because a second flock descriptor deadlocks.
fn lock_ledger(dir: &Path) -> Result<LockedLedger> {
    crate::fsperm::ensure_private_dir(dir).with_context(|| format!("create {}", dir.display()))?;
    let held = crate::fsperm::open_private_dir(dir)
        .with_context(|| format!("open managed SSH custody directory {}", dir.display()))?;
    let path = held.path().join(".custody.lock");
    let file = held
        .open_or_create_regular_nofollow(".custody.lock")
        .with_context(|| format!("open managed SSH custody ledger lock {}", path.display()))?;
    file.lock()
        .with_context(|| format!("lock managed SSH custody ledger {}", path.display()))?;
    Ok(LockedLedger {
        dir: held,
        _lock: file,
    })
}

fn publish_locked(
    private: &crate::fsperm::PrivateDir,
    record: &AuthorizedKeyRecord,
) -> Result<AuthorizedKeyRecord> {
    let name = record_file_name(&record.provider, &record.account, &record.instance);
    let path = private.path().join(&name);
    let legacy_name = legacy_file_name(&record.provider, &record.account, &record.instance);
    let legacy = private.path().join(&legacy_name);
    let mut floor = 0;
    for (candidate_name, candidate_path) in [(&name, &path), (&legacy_name, &legacy)] {
        if let Some(existing) = read_row(private, candidate_name).with_context(|| {
            format!(
                "publish managed SSH custody record {}",
                candidate_path.display()
            )
        })? {
            floor = floor.max(existing.generation);
            check_tuple(
                &existing,
                &record.provider,
                &record.account,
                &record.instance,
                candidate_path,
            )?;
        }
    }
    let mut stamped = record.clone();
    stamped.generation = next_generation(private, floor)?;
    private
        .write_owner_only_atomic(&name, &serde_json::to_vec_pretty(&stamped)?)
        .with_context(|| format!("publish managed SSH custody record {}", path.display()))?;
    if legacy_name != name {
        private
            .remove_file(&legacy_name)
            .with_context(|| format!("remove {}", legacy.display()))?;
        private
            .sync()
            .context("sync managed SSH custody directory")?;
    }
    Ok(stamped)
}

pub fn publish_if_unchanged_at(
    dir: &Path,
    expected: &AuthorizedKeyRecord,
    next: &AuthorizedKeyRecord,
) -> Result<AuthorizedKeyRecord> {
    anyhow::ensure!(
        next.provider == expected.provider
            && next.account == expected.account
            && next.instance == expected.instance,
        "custody compare-and-swap cannot change the row identity"
    );
    let ledger = lock_ledger(dir)?;
    let actual = read_at_in(
        &ledger.dir,
        &expected.provider,
        &expected.account,
        &expected.instance,
    )?;
    if actual.as_ref() != Some(expected) {
        return Err(CustodyChanged {
            provider: expected.provider.clone(),
            instance: expected.instance.clone(),
        }
        .into());
    }
    publish_locked(&ledger.dir, next)
}

pub fn record_revoked(provider: &str, account: &str, instance: &str) -> Result<()> {
    let existing = revoke_at(&dir(), provider, account, instance)?;
    tracing::info!(
        target: "thegn::secret::audit",
        consumer = "managed-ssh:destroy",
        provider,
        instance,
        account = existing.as_ref().map(|r| r.account.as_str()).unwrap_or("unknown"),
        key_fingerprint = existing
            .as_ref()
            .map(|r| r.key_fingerprint.as_str())
            .unwrap_or("unknown"),
        outcome = "revoked",
        "managed SSH key authorization retired with instance",
    );
    Ok(())
}

pub fn record_revoked_at(dir: &Path, provider: &str, account: &str, instance: &str) -> Result<()> {
    revoke_at(dir, provider, account, instance).map(|_| ())
}

fn revoke_at(
    dir: &Path,
    provider: &str,
    account: &str,
    instance: &str,
) -> Result<Option<AuthorizedKeyRecord>> {
    match std::fs::symlink_metadata(dir) {
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error).with_context(|| format!("inspect {}", dir.display())),
    }
    let ledger = lock_ledger(dir)?;
    // Parse before delete. An unreadable or corrupt row may be the only local
    // evidence of a remotely authorized key and must never be collapsed into
    // "missing" by a mutation path.
    let existing = read_at_in(&ledger.dir, provider, account, instance)?;
    remove_record_path(&ledger.dir, provider, account, instance)?;
    Ok(existing)
}

fn remove_record_path(
    private: &crate::fsperm::PrivateDir,
    provider: &str,
    account: &str,
    instance: &str,
) -> Result<()> {
    let canonical_name = record_file_name(provider, account, instance);
    let canonical = private.path().join(&canonical_name);
    let legacy_name = legacy_file_name(provider, account, instance);
    let legacy = private.path().join(&legacy_name);
    for (name, path) in [(&canonical_name, &canonical), (&legacy_name, &legacy)] {
        if let Some(record) = read_row(private, name)
            .with_context(|| format!("remove managed SSH custody record {}", path.display()))?
        {
            check_tuple(&record, provider, account, instance, path)?;
        }
    }
    for (name, path) in [(&canonical_name, &canonical), (&legacy_name, &legacy)] {
        private
            .remove_file(name)
            .with_context(|| format!("remove {}", path.display()))?;
    }
    private
        .sync()
        .context("sync managed SSH custody directory")?;
    Ok(())
}

pub fn read(provider: &str, account: &str, instance: &str) -> Result<Option<AuthorizedKeyRecord>> {
    read_at(&dir(), provider, account, instance)
}

pub fn read_at(
    dir: &Path,
    provider: &str,
    account: &str,
    instance: &str,
) -> Result<Option<AuthorizedKeyRecord>> {
    let Some(private) = open_ledger_dir(dir)? else {
        return Ok(None);
    };
    read_at_in(&private, provider, account, instance)
}

fn read_at_in(
    private: &crate::fsperm::PrivateDir,
    provider: &str,
    account: &str,
    instance: &str,
) -> Result<Option<AuthorizedKeyRecord>> {
    let canonical_name = record_file_name(provider, account, instance);
    let canonical = private.path().join(&canonical_name);
    let legacy_name = legacy_file_name(provider, account, instance);
    let legacy = private.path().join(&legacy_name);
    let (path, record) = if let Some(record) = read_row(private, &canonical_name)? {
        (&canonical, record)
    } else if let Some(record) = read_row(private, &legacy_name)? {
        (&legacy, record)
    } else {
        return Ok(None);
    };
    check_tuple(&record, provider, account, instance, path)?;
    Ok(Some(record))
}

/// Resolve a provider+instance only when it is unambiguous across account
/// namespaces. Callers that cannot identify the provider account must fail
/// closed instead of selecting or deleting another account's custody row.
pub fn read_unique_instance(provider: &str, instance: &str) -> Result<Option<AuthorizedKeyRecord>> {
    let mut matches = list()?
        .into_iter()
        .filter(|record| record.provider == provider && record.instance == instance);
    let first = matches.next();
    if matches.next().is_some() {
        anyhow::bail!(
            "managed SSH instance {provider}/{instance} is ambiguous across provider accounts"
        );
    }
    Ok(first)
}

/// Persist value-free recovery state before and after each remote rotation
/// mutation. `None` clears a fully recovered/completed attempt.
pub fn record_rotation_recovery(
    record: &AuthorizedKeyRecord,
    recovery: Option<RotationRecovery>,
) -> Result<AuthorizedKeyRecord> {
    let mut updated = record.clone();
    updated.rotation_recovery = recovery;
    publish_if_unchanged_at(&dir(), record, &updated)
}

pub fn list() -> Result<Vec<AuthorizedKeyRecord>> {
    list_at(&dir())
}

pub fn list_at(dir: &Path) -> Result<Vec<AuthorizedKeyRecord>> {
    let Some(private) =
        open_ledger_dir(dir).with_context(|| format!("read directory {}", dir.display()))?
    else {
        return Ok(Vec::new());
    };
    let names = private
        .file_names()
        .with_context(|| format!("read directory {}", dir.display()))?;
    let mut row_names = Vec::new();
    for name in names {
        if let Some(text) = name.to_str()
            && Path::new(text)
                .extension()
                .is_some_and(|extension| extension == "json")
        {
            row_names.push(text.to_string());
        }
    }
    // Enforce the cap using the complete set of JSON names before opening any row.
    anyhow::ensure!(
        row_names.len() <= MAX_CUSTODY_ROWS,
        "managed SSH custody inventory has more than {MAX_CUSTODY_ROWS} rows"
    );
    let mut records = Vec::new();
    let mut tuples = std::collections::HashMap::new();
    for name in row_names {
        let path = private.path().join(&name);
        if let Some(record) = read_row(&private, &name)? {
            let canonical = record_file_name(&record.provider, &record.account, &record.instance);
            let legacy = legacy_file_name(&record.provider, &record.account, &record.instance);
            anyhow::ensure!(
                name == canonical || name == legacy,
                "custody filename does not match embedded tuple at {}",
                path.display()
            );
            let tuple = (
                record.provider.clone(),
                record.account.clone(),
                record.instance.clone(),
            );
            if let Some(previous) = tuples.insert(tuple, path.clone()) {
                anyhow::bail!(
                    "duplicate managed SSH custody tuple at {} and {}",
                    previous.display(),
                    path.display()
                );
            }
            records.push(record);
        }
    }
    records.sort_by(|a: &AuthorizedKeyRecord, b| {
        (&a.provider, &a.account, &a.instance).cmp(&(&b.provider, &b.account, &b.instance))
    });
    Ok(records)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record() -> AuthorizedKeyRecord {
        AuthorizedKeyRecord {
            provider: "fly".into(),
            account: "env:FLY_WORK".into(),
            instance: "tg-project-feature".into(),
            key_path: PathBuf::from("/state/ssh/fly-env-FLY-WORK_ed25519"),
            key_fingerprint: key_fingerprint("ssh-ed25519 AAAATEST comment"),
            authorized_at: 1,
            generation: 0,
            proxy_worktree: None,
            rotation_recovery: None,
        }
    }

    #[test]
    fn fingerprint_canonicalizes_key_comments_and_whitespace() {
        let plain = key_fingerprint("ssh-ed25519 AAAATEST first-comment");
        let different_comment = key_fingerprint("ssh-ed25519 AAAATEST another comment entirely");
        let irregular_whitespace = key_fingerprint("  ssh-ed25519\n\tAAAATEST   comment");

        assert_eq!(plain, different_comment);
        assert_eq!(plain, irregular_whitespace);
        assert!(plain.starts_with("invalid:"));
        assert_ne!(plain, key_fingerprint("ssh-ed25519 AAAAOTHER comment"));
        assert!(!plain.contains("AAAATEST"));
    }

    #[test]
    fn fingerprint_matches_openssh_sha256_vectors() {
        let key = "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIIgVgF3FLyN2aHUalBpkk3cMVfTgD+7TrbdfTAcSvLvB thegn";
        let expected = "SHA256:7wNHZPUVQSnbsSe5yU7EEx0GGDS9qo9UK41TH7Cam/g";
        assert_eq!(try_key_fingerprint(key).unwrap(), expected);
        assert_eq!(key_fingerprint(key), expected);
        assert_eq!(
            try_key_fingerprint("  ssh-ed25519\n\tAAAAC3NzaC1lZDI1NTE5AAAAIIgVgF3FLyN2aHUalBpkk3cMVfTgD+7TrbdfTAcSvLvB   other comment ").unwrap(),
            expected
        );
        assert_eq!(
            try_key_fingerprint("ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAICXLY4ay5H8w/Ud7QvnU1EtrjkvENKLoy7B0YU17bVzq vec2").unwrap(),
            "SHA256:pGlFHtaffuJRePHewrYb6Y+zhMDhFIBNiF5SbSOmpug"
        );
    }

    #[test]
    fn legacy_fingerprints_are_labeled_unverified_for_audit() {
        assert_eq!(
            fingerprint_audit_label("0123456789abcdef"),
            "0123456789abcdef (legacy, unverified)"
        );
        assert_eq!(fingerprint_audit_label("SHA256:abc"), "SHA256:abc");
    }

    #[test]
    fn unparsable_keys_never_get_a_sha256_fingerprint() {
        let temp = tempfile::tempdir().unwrap();
        for key in [
            "",
            "ssh-ed25519 AAAATEST x",
            "ssh-ed25519 AAAA",
            "ssh-ed25519 AAAABXNz",
            "ssh-rsa AAAAC3NzaC1lZDI1NTE5AAAAIIgVgF3FLyN2aHUalBpkk3cMVfTgD+7TrbdfTAcSvLvB",
            "ssh-ed25519",
        ] {
            assert!(try_key_fingerprint(key).is_err());
            let fingerprint = key_fingerprint(key);
            assert!(fingerprint.starts_with("invalid:"));
            assert!(!fingerprint_is_verified(&fingerprint));
        }
        assert!(
            record_authorized_with_proxy_worktree_at(
                temp.path(),
                "fly",
                "account",
                "instance",
                Path::new("/state/key"),
                "ssh-ed25519 AAAATEST x",
                None,
            )
            .is_err()
        );
        assert_eq!(
            std::fs::read_dir(temp.path())
                .unwrap()
                .map(|entry| entry.unwrap())
                .filter(|entry| entry
                    .path()
                    .extension()
                    .is_some_and(|extension| extension == "json"))
                .count(),
            0
        );
    }

    #[test]
    fn authorization_helper_returns_the_published_fingerprint() {
        let temp = tempfile::tempdir().unwrap();
        let key = "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIIgVgF3FLyN2aHUalBpkk3cMVfTgD+7TrbdfTAcSvLvB thegn";
        let expected = "SHA256:7wNHZPUVQSnbsSe5yU7EEx0GGDS9qo9UK41TH7Cam/g";

        let fingerprint = record_authorized_with_proxy_worktree_at(
            temp.path(),
            "fly",
            "account",
            "instance",
            Path::new("/state/key"),
            key,
            None,
        )
        .unwrap();
        let published = read_at(temp.path(), "fly", "account", "instance")
            .unwrap()
            .unwrap();

        assert_eq!(fingerprint, expected);
        assert_eq!(published.key_fingerprint, fingerprint);
    }

    #[test]
    fn every_publish_stamps_a_strictly_increasing_generation() {
        let temp = tempfile::tempdir().unwrap();
        let key = "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIIgVgF3FLyN2aHUalBpkk3cMVfTgD+7TrbdfTAcSvLvB thegn";
        let fingerprint = record_authorized_with_proxy_worktree_at(
            temp.path(),
            "fly",
            "account",
            "instance",
            Path::new("/k"),
            key,
            None,
        )
        .unwrap();
        let first = read_at(temp.path(), "fly", "account", "instance")
            .unwrap()
            .unwrap();
        assert_eq!(first.key_fingerprint, fingerprint);
        assert!(first.generation >= 1);

        std::fs::remove_file(temp.path().join(GENERATION_FILE)).unwrap();
        let second = publish_if_unchanged_at(temp.path(), &first, &first).unwrap();
        assert!(second.generation > first.generation);
        assert_eq!(
            read_at(temp.path(), "fly", "account", "instance").unwrap(),
            Some(second.clone())
        );

        record_revoked_at(temp.path(), "fly", "account", "instance").unwrap();
        let third = record_authorized_at(temp.path(), &second).unwrap();
        assert!(third.generation > second.generation);
    }

    #[test]
    fn compare_and_publish_refuses_a_row_revoked_and_republished_with_identical_fields() {
        let temp = tempfile::tempdir().unwrap();
        let input = record();
        let snapshot = record_authorized_at(temp.path(), &input).unwrap();
        record_revoked_at(
            temp.path(),
            &input.provider,
            &input.account,
            &input.instance,
        )
        .unwrap();
        let republished = record_authorized_at(temp.path(), &input).unwrap();
        let stale = AuthorizedKeyRecord {
            key_fingerprint: "SHA256:stale".into(),
            ..snapshot.clone()
        };

        let result = publish_if_unchanged_at(temp.path(), &snapshot, &stale);

        assert!(matches!(result, Err(error) if error.downcast_ref::<CustodyChanged>().is_some()));
        assert_eq!(
            read_at(
                temp.path(),
                &input.provider,
                &input.account,
                &input.instance
            )
            .unwrap(),
            Some(republished.clone())
        );
        assert!(republished.generation > snapshot.generation);
    }

    #[test]
    fn pre_generation_rows_read_as_generation_zero_and_can_be_compared_and_published() {
        let temp = tempfile::tempdir().unwrap();
        crate::fsperm::restrict_dir_to_owner(temp.path()).unwrap();
        let input = record();
        let mut value = serde_json::to_value(&input).unwrap();
        value.as_object_mut().unwrap().remove("generation");
        let path = record_path(
            temp.path(),
            &input.provider,
            &input.account,
            &input.instance,
        );
        crate::fsperm::write_owner_only_atomic(&path, &serde_json::to_vec(&value).unwrap())
            .unwrap();
        let snapshot = read_at(
            temp.path(),
            &input.provider,
            &input.account,
            &input.instance,
        )
        .unwrap()
        .unwrap();
        assert_eq!(snapshot.generation, 0);

        let next = AuthorizedKeyRecord {
            key_fingerprint: "SHA256:updated-legacy".into(),
            ..snapshot.clone()
        };
        let published = publish_if_unchanged_at(temp.path(), &snapshot, &next).unwrap();
        assert!(published.generation >= 1);
        assert_eq!(
            read_at(
                temp.path(),
                &input.provider,
                &input.account,
                &input.instance
            )
            .unwrap(),
            Some(published)
        );
    }

    #[test]
    fn corrupt_generation_counter_fails_closed_before_any_row_is_written() {
        let temp = tempfile::tempdir().unwrap();
        let counter = temp.path().join(GENERATION_FILE);
        std::fs::write(&counter, b"not-a-number").unwrap();
        let error = record_authorized_at(temp.path(), &record()).unwrap_err();
        assert!(format!("{error:#}").contains("generation counter"));
        assert!(list_at(temp.path()).unwrap().is_empty());

        std::fs::remove_file(&counter).unwrap();
        std::fs::create_dir(&counter).unwrap();
        let error = record_authorized_at(temp.path(), &record()).unwrap_err();
        assert!(format!("{error:#}").contains("generation counter"));
        assert!(list_at(temp.path()).unwrap().is_empty());

        std::fs::remove_dir(&counter).unwrap();
        std::fs::write(&counter, "18446744073709551615").unwrap();
        let error = record_authorized_at(temp.path(), &record()).unwrap_err();
        assert!(format!("{error:#}").contains("exhausted"));
        assert!(list_at(temp.path()).unwrap().is_empty());

        std::fs::write(&counter, "123456789012345678901234567890123").unwrap();
        assert!(
            format!(
                "{:#}",
                record_authorized_at(temp.path(), &record()).unwrap_err()
            )
            .contains("generation counter")
        );
        std::fs::write(&counter, [0xff]).unwrap();
        assert!(
            format!(
                "{:#}",
                record_authorized_at(temp.path(), &record()).unwrap_err()
            )
            .contains("generation counter")
        );
        assert!(list_at(temp.path()).unwrap().is_empty());
    }

    #[test]
    fn generation_counter_is_not_listed_as_custody() {
        let temp = tempfile::tempdir().unwrap();
        let published = record_authorized_at(temp.path(), &record()).unwrap();

        assert_eq!(list_at(temp.path()).unwrap(), vec![published]);
        assert!(temp.path().join(GENERATION_FILE).exists());
    }

    fn recovering_record(key_fingerprint: &str) -> AuthorizedKeyRecord {
        AuthorizedKeyRecord {
            provider: "fly".into(),
            account: "account".into(),
            instance: "instance".into(),
            key_path: PathBuf::from("/state/old-key"),
            key_fingerprint: key_fingerprint.into(),
            authorized_at: 1,
            generation: 0,
            proxy_worktree: None,
            rotation_recovery: Some(RotationRecovery {
                key_path: PathBuf::from("/state/next-key"),
                key_fingerprint: "SHA256:7wNHZPUVQSnbsSe5yU7EEx0GGDS9qo9UK41TH7Cam/g".into(),
                backup_key_path: Some(PathBuf::from("/state/backup-key")),
                authorized: true,
                verified: false,
                retiring_started: false,
                retiring_completed: false,
            }),
        }
    }

    #[test]
    fn provision_publish_refuses_a_row_with_pending_rotation_recovery() {
        let temp = tempfile::tempdir().unwrap();
        let key = "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIIgVgF3FLyN2aHUalBpkk3cMVfTgD+7TrbdfTAcSvLvB thegn";
        let old_key =
            "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAICXLY4ay5H8w/Ud7QvnU1EtrjkvENKLoy7B0YU17bVzq vec2";
        let old_fingerprint = try_key_fingerprint(old_key).unwrap();
        let original =
            record_authorized_at(temp.path(), &recovering_record(&old_fingerprint)).unwrap();
        let record_file = record_path(
            temp.path(),
            &original.provider,
            &original.account,
            &original.instance,
        );
        let original_bytes = std::fs::read(&record_file).unwrap();

        let result = record_authorized_with_proxy_worktree_at(
            temp.path(),
            &original.provider,
            &original.account,
            &original.instance,
            Path::new("/state/new-key"),
            key,
            None,
        );

        let error = result.unwrap_err();
        assert!(error.downcast_ref::<RotationRecoveryPending>().is_some());
        assert!(format!("{error:#}").contains("fly/instance"));
        assert!(format!("{error:#}").contains("thegn secret ssh rotate"));
        assert_eq!(std::fs::read(&record_file).unwrap(), original_bytes);
        assert_eq!(
            read_at(
                temp.path(),
                &original.provider,
                &original.account,
                &original.instance,
            )
            .unwrap(),
            Some(original)
        );
    }

    #[test]
    fn rotation_completion_publish_replaces_a_recovering_row() {
        let temp = tempfile::tempdir().unwrap();
        let old_key =
            "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAICXLY4ay5H8w/Ud7QvnU1EtrjkvENKLoy7B0YU17bVzq vec2";
        let new_key = "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIIgVgF3FLyN2aHUalBpkk3cMVfTgD+7TrbdfTAcSvLvB thegn";
        let original = record_authorized_at(
            temp.path(),
            &recovering_record(try_key_fingerprint(old_key).unwrap().as_str()),
        )
        .unwrap();

        let fingerprint = record_rotation_completed_at(
            temp.path(),
            &original,
            Path::new("/state/new-key"),
            new_key,
        )
        .unwrap();
        let published = read_at(
            temp.path(),
            &original.provider,
            &original.account,
            &original.instance,
        )
        .unwrap()
        .unwrap();

        assert_eq!(
            fingerprint,
            "SHA256:7wNHZPUVQSnbsSe5yU7EEx0GGDS9qo9UK41TH7Cam/g"
        );
        assert_eq!(published.key_fingerprint, fingerprint);
        assert_eq!(published.key_path, PathBuf::from("/state/new-key"));
        assert_eq!(published.rotation_recovery, None);
    }

    #[test]
    fn global_rotation_completion_retires_only_the_matching_recovery_record() {
        let _state = crate::testenv::IsolatedState::new();
        let old_key =
            "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAICXLY4ay5H8w/Ud7QvnU1EtrjkvENKLoy7B0YU17bVzq vec2";
        let new_key = "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIIgVgF3FLyN2aHUalBpkk3cMVfTgD+7TrbdfTAcSvLvB thegn";
        let old_fingerprint = try_key_fingerprint(old_key).unwrap();
        let original = record_authorized_at(&dir(), &recovering_record(&old_fingerprint)).unwrap();

        let unrelated = AuthorizedKeyRecord {
            instance: "another-instance".into(),
            ..original.clone()
        };
        let unrelated = record_authorized_at(&dir(), &unrelated).unwrap();

        record_rotation_completed(&original, Path::new("/state/completed-key"), new_key).unwrap();

        let completed = read(&original.provider, &original.account, &original.instance)
            .unwrap()
            .unwrap();
        assert_eq!(
            completed.key_fingerprint,
            try_key_fingerprint(new_key).unwrap()
        );
        assert_eq!(completed.key_path, PathBuf::from("/state/completed-key"));
        assert_eq!(completed.rotation_recovery, None);
        assert_eq!(
            read(&unrelated.provider, &unrelated.account, &unrelated.instance).unwrap(),
            Some(unrelated),
            "rotation completion must preserve recovery for a different instance tuple"
        );
    }

    #[test]
    fn provision_publish_replaces_a_row_without_recovery() {
        let temp = tempfile::tempdir().unwrap();
        let old_key =
            "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAICXLY4ay5H8w/Ud7QvnU1EtrjkvENKLoy7B0YU17bVzq vec2";
        let new_key = "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIIgVgF3FLyN2aHUalBpkk3cMVfTgD+7TrbdfTAcSvLvB thegn";
        let original = AuthorizedKeyRecord {
            provider: "fly".into(),
            account: "account".into(),
            instance: "instance".into(),
            key_path: PathBuf::from("/state/old-key"),
            key_fingerprint: try_key_fingerprint(old_key).unwrap(),
            authorized_at: 1,
            generation: 0,
            proxy_worktree: None,
            rotation_recovery: None,
        };
        record_authorized_at(temp.path(), &original).unwrap();

        let fingerprint = record_authorized_with_proxy_worktree_at(
            temp.path(),
            &original.provider,
            &original.account,
            &original.instance,
            Path::new("/state/new-key"),
            new_key,
            None,
        )
        .unwrap();
        let published = read_at(
            temp.path(),
            &original.provider,
            &original.account,
            &original.instance,
        )
        .unwrap()
        .unwrap();

        assert_ne!(published.key_fingerprint, original.key_fingerprint);
        assert_eq!(published.key_fingerprint, fingerprint);
        assert_eq!(published.key_path, PathBuf::from("/state/new-key"));
        assert_eq!(published.rotation_recovery, None);
    }

    #[test]
    fn account_labels_are_value_free_and_stable() {
        assert_eq!(
            account_label(Path::new("/state/ssh/sprite_ed25519")),
            "shared"
        );
        assert_eq!(
            account_label(Path::new("/state/ssh/fly-env-FLY-WORK_ed25519")),
            "fly-env-FLY-WORK"
        );
        assert_eq!(account_label(Path::new("custom-key")), "custom-key");
        assert_eq!(account_label(Path::new("/")), "managed");
    }

    #[test]
    fn record_paths_are_stable_opaque_and_account_scoped() {
        let root = Path::new("/custody");
        let first = record_path(root, "fly", "account-a", "instance");
        let repeat = record_path(root, "fly", "account-a", "instance");
        let other_account = record_path(root, "fly", "account-b", "instance");

        assert_eq!(first, repeat);
        assert_ne!(first, other_account);
        assert_eq!(first.parent(), Some(root));
        let name = first.file_name().unwrap().to_string_lossy();
        assert!(name.ends_with(".json"));
        assert!(!name.contains("fly"));
        assert!(!name.contains("account"));
        assert!(!name.contains("instance"));
        assert_eq!(name.len(), 69);
    }

    #[test]
    fn a_planted_row_with_another_tuple_is_a_collision() {
        let temp = tempfile::tempdir().unwrap();
        let requested = record();
        let mut planted = requested.clone();
        planted.provider = "other-provider".into();
        planted.instance = "other-instance".into();
        let path = record_path(
            temp.path(),
            &requested.provider,
            &requested.account,
            &requested.instance,
        );
        let planted_bytes = serde_json::to_vec(&planted).unwrap();
        std::fs::write(&path, &planted_bytes).unwrap();

        assert!(
            read_at(
                temp.path(),
                &requested.provider,
                &requested.account,
                &requested.instance
            )
            .is_err()
        );
        assert!(record_authorized_at(temp.path(), &requested).is_err());
        assert_eq!(std::fs::read(&path).unwrap(), planted_bytes);
        assert!(
            record_revoked_at(
                temp.path(),
                &requested.provider,
                &requested.account,
                &requested.instance
            )
            .is_err()
        );
        assert!(path.exists());
        assert!(list_at(temp.path()).is_err());
    }

    #[test]
    fn legacy_fnv_rows_are_read_and_migrated_on_write() {
        let temp = tempfile::tempdir().unwrap();
        crate::fsperm::restrict_dir_to_owner(temp.path()).unwrap();
        let existing = record();
        let legacy = legacy_record_path(
            temp.path(),
            &existing.provider,
            &existing.account,
            &existing.instance,
        );
        let canonical = record_path(
            temp.path(),
            &existing.provider,
            &existing.account,
            &existing.instance,
        );
        crate::fsperm::write_owner_only_atomic(&legacy, &serde_json::to_vec(&existing).unwrap())
            .unwrap();

        assert_eq!(
            read_at(
                temp.path(),
                &existing.provider,
                &existing.account,
                &existing.instance
            )
            .unwrap(),
            Some(existing.clone())
        );
        assert_eq!(list_at(temp.path()).unwrap(), vec![existing.clone()]);
        let mut updated = existing.clone();
        updated.authorized_at += 1;
        let updated = record_authorized_at(temp.path(), &updated).unwrap();
        assert!(canonical.exists());
        assert!(!legacy.exists());
        assert_eq!(list_at(temp.path()).unwrap(), vec![updated]);
    }

    #[test]
    fn inventory_rejects_duplicate_legacy_and_canonical_tuple_rows() {
        let temp = tempfile::tempdir().unwrap();
        let existing = record();
        let legacy = legacy_record_path(
            temp.path(),
            &existing.provider,
            &existing.account,
            &existing.instance,
        );
        let canonical = record_path(
            temp.path(),
            &existing.provider,
            &existing.account,
            &existing.instance,
        );
        let bytes = serde_json::to_vec(&existing).unwrap();
        std::fs::write(&legacy, &bytes).unwrap();
        std::fs::write(&canonical, bytes).unwrap();

        assert!(list_at(temp.path()).is_err());
    }

    #[test]
    fn custody_round_trip_records_identity_without_public_key_material() {
        let temp = tempfile::tempdir().unwrap();
        let record = record_authorized_at(temp.path(), &record()).unwrap();
        assert_eq!(
            read_at(
                temp.path(),
                &record.provider,
                &record.account,
                &record.instance
            )
            .unwrap(),
            Some(record.clone())
        );
        let json = std::fs::read_to_string(
            std::fs::read_dir(temp.path())
                .unwrap()
                .map(|entry| entry.unwrap())
                .find(|entry| {
                    entry
                        .path()
                        .extension()
                        .is_some_and(|extension| extension == "json")
                })
                .unwrap()
                .path(),
        )
        .unwrap();
        assert!(!json.contains("AAAATEST"));
        record_revoked_at(
            temp.path(),
            &record.provider,
            &record.account,
            &record.instance,
        )
        .unwrap();
        assert!(list_at(temp.path()).unwrap().is_empty());
    }

    #[test]
    fn custody_publish_is_owner_only_and_atomically_replaces_one_identity() {
        let temp = tempfile::tempdir().unwrap();
        let mut original = record_authorized_at(temp.path(), &record()).unwrap();

        if let Some(mode) = crate::fsperm::mode_bits(temp.path()).unwrap() {
            assert_eq!(mode, 0o700);
        }
        let path = record_path(
            temp.path(),
            &original.provider,
            &original.account,
            &original.instance,
        );
        if let Some(mode) = crate::fsperm::mode_bits(&path).unwrap() {
            assert_eq!(mode, 0o600);
        }

        original.key_path = PathBuf::from("/state/ssh/replacement_ed25519");
        original.key_fingerprint = "replacement-hash".into();
        original.proxy_worktree = Some("project/feature".into());
        original.rotation_recovery = Some(RotationRecovery {
            key_path: PathBuf::from("/state/ssh/recover_ed25519"),
            key_fingerprint: "recover-hash".into(),
            backup_key_path: Some(PathBuf::from("/state/ssh/retired_ed25519")),
            authorized: true,
            verified: true,
            retiring_started: true,
            retiring_completed: false,
        });
        original = record_authorized_at(temp.path(), &original).unwrap();

        assert_eq!(list_at(temp.path()).unwrap(), vec![original.clone()]);
        assert_eq!(
            read_at(temp.path(), "fly", "env:FLY_WORK", "tg-project-feature").unwrap(),
            Some(original)
        );
        assert_eq!(
            std::fs::read_dir(temp.path())
                .unwrap()
                .map(|entry| entry.unwrap())
                .filter(|entry| entry
                    .path()
                    .extension()
                    .is_some_and(|extension| extension == "json"))
                .count(),
            1
        );
    }

    #[test]
    fn global_operations_honor_thegn_dir_and_persist_rotation_recovery() {
        let state = crate::testenv::IsolatedState::new();
        let key_path = Path::new("/state/ssh/fly-env-FLY-WORK_ed25519");

        assert_eq!(dir(), state.thegn_dir().join("ssh/authorized.d"));
        record_authorized(
            "fly",
            "env:FLY_WORK",
            "instance-1",
            key_path,
            "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIIgVgF3FLyN2aHUalBpkk3cMVfTgD+7TrbdfTAcSvLvB do-not-persist",
        )
        .unwrap();
        let original = read("fly", "env:FLY_WORK", "instance-1").unwrap().unwrap();
        assert_eq!(original.proxy_worktree, None);
        assert_eq!(original.key_path, key_path);
        assert_eq!(
            read_unique_instance("fly", "instance-1").unwrap(),
            Some(original.clone())
        );
        assert_eq!(read_unique_instance("fly", "missing").unwrap(), None);

        let recovery = RotationRecovery {
            key_path: PathBuf::from("/state/ssh/next_ed25519"),
            key_fingerprint: "next-hash".into(),
            backup_key_path: Some(PathBuf::from("/state/ssh/old_ed25519")),
            authorized: true,
            verified: false,
            retiring_started: false,
            retiring_completed: false,
        };
        let recovering = record_rotation_recovery(&original, Some(recovery.clone())).unwrap();
        assert_eq!(recovering.rotation_recovery, Some(recovery));
        assert_eq!(
            read("fly", "env:FLY_WORK", "instance-1")
                .unwrap()
                .unwrap()
                .rotation_recovery,
            recovering.rotation_recovery
        );
        let recovered = record_rotation_recovery(&recovering, None).unwrap();
        assert_eq!(recovered.rotation_recovery, None);

        record_revoked("fly", "env:FLY_WORK", "instance-1").unwrap();
        assert_eq!(read("fly", "env:FLY_WORK", "instance-1").unwrap(), None);
        record_revoked("fly", "env:FLY_WORK", "instance-1").unwrap();
        assert!(list().unwrap().is_empty());
    }

    #[test]
    fn proxy_context_round_trips_and_unique_lookup_rejects_account_ambiguity() {
        let _state = crate::testenv::IsolatedState::new();
        let key = Path::new("/state/ssh/sprite_ed25519");

        record_authorized_with_proxy_worktree(
            "sprites",
            "account-a",
            "sprite-1",
            key,
            "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIIgVgF3FLyN2aHUalBpkk3cMVfTgD+7TrbdfTAcSvLvB",
            Some("project/feature"),
        )
        .unwrap();
        let record = read_unique_instance("sprites", "sprite-1")
            .unwrap()
            .unwrap();
        assert_eq!(record.proxy_worktree.as_deref(), Some("project/feature"));

        record_authorized_with_proxy_worktree(
            "sprites",
            "account-b",
            "sprite-1",
            key,
            "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAICXLY4ay5H8w/Ud7QvnU1EtrjkvENKLoy7B0YU17bVzq",
            Some("project/other"),
        )
        .unwrap();
        let error = read_unique_instance("sprites", "sprite-1").unwrap_err();
        assert!(
            error
                .to_string()
                .contains("ambiguous across provider accounts")
        );
        assert_eq!(list().unwrap().len(), 2);
    }

    #[test]
    fn custody_reads_pre_proxy_context_records() {
        let legacy = br#"{
          "provider":"sprites","account":"shared","instance":"sprite-1",
          "key_path":"/state/ssh/sprite_ed25519",
          "key_fingerprint":"0123456789abcdef","authorized_at":1
        }"#;
        let record: AuthorizedKeyRecord = serde_json::from_slice(legacy).unwrap();
        assert_eq!(record.proxy_worktree, None);
        assert_eq!(record.rotation_recovery, None);
    }

    #[test]
    fn custody_reads_pre_rotation_phase_records_with_safe_defaults() {
        let legacy = br#"{
          "key_path":"/state/ssh/next_ed25519",
          "key_fingerprint":"0123456789abcdef",
          "authorized":true,"verified":false
        }"#;
        let recovery: RotationRecovery = serde_json::from_slice(legacy).unwrap();
        assert_eq!(recovery.backup_key_path, None);
        assert!(!recovery.retiring_started);
        assert!(!recovery.retiring_completed);
    }

    #[test]
    fn custody_identity_includes_account_namespace() {
        let temp = tempfile::tempdir().unwrap();
        let mut first = record();
        first.account = "account-a".into();
        let mut second = first.clone();
        second.account = "account-b".into();
        second.key_path = PathBuf::from("/state/ssh/account-b_ed25519");
        let first = record_authorized_at(temp.path(), &first).unwrap();
        let second = record_authorized_at(temp.path(), &second).unwrap();
        assert_eq!(list_at(temp.path()).unwrap().len(), 2);
        assert_eq!(
            read_at(
                temp.path(),
                &first.provider,
                &first.account,
                &first.instance
            )
            .unwrap(),
            Some(first)
        );
        assert_eq!(
            read_at(
                temp.path(),
                &second.provider,
                &second.account,
                &second.instance
            )
            .unwrap(),
            Some(second)
        );
    }

    #[test]
    fn inventory_is_sorted_and_ignores_non_json_files() {
        let temp = tempfile::tempdir().unwrap();
        std::fs::write(temp.path().join("README"), "not a custody record").unwrap();
        std::fs::create_dir(temp.path().join("nested.json.bak")).unwrap();

        let mut z = record();
        z.provider = "sprites".into();
        z.account = "shared".into();
        z.instance = "z".into();
        let mut a = record();
        a.provider = "fly".into();
        a.account = "account-b".into();
        a.instance = "b".into();
        let mut first = a.clone();
        first.account = "account-a".into();
        first.instance = "a".into();
        let z = record_authorized_at(temp.path(), &z).unwrap();
        let a = record_authorized_at(temp.path(), &a).unwrap();
        let first = record_authorized_at(temp.path(), &first).unwrap();

        assert_eq!(list_at(temp.path()).unwrap(), vec![first, a, z]);
    }

    #[test]
    fn missing_ledger_and_missing_records_are_idempotent() {
        let temp = tempfile::tempdir().unwrap();
        let missing_dir = temp.path().join("absent");

        assert!(list_at(&missing_dir).unwrap().is_empty());
        assert_eq!(
            read_at(&missing_dir, "fly", "account", "instance").unwrap(),
            None
        );
        record_revoked_at(&missing_dir, "fly", "account", "instance").unwrap();
        assert!(!missing_dir.exists());
    }

    #[test]
    fn filesystem_failures_retain_context_and_custody_evidence() {
        let temp = tempfile::tempdir().unwrap();
        let record = record();

        let file_instead_of_ledger = temp.path().join("ledger-file");
        std::fs::write(&file_instead_of_ledger, "occupied").unwrap();
        let create_error = record_authorized_at(&file_instead_of_ledger, &record).unwrap_err();
        assert!(format!("{create_error:#}").contains("create"));
        let list_error = list_at(&file_instead_of_ledger).unwrap_err();
        assert!(format!("{list_error:#}").contains("read directory"));

        let ledger = temp.path().join("ledger");
        std::fs::create_dir(&ledger).unwrap();
        crate::fsperm::restrict_dir_to_owner(&ledger).unwrap();
        let path = record_path(&ledger, &record.provider, &record.account, &record.instance);
        std::fs::create_dir(&path).unwrap();
        let read_error =
            read_at(&ledger, &record.provider, &record.account, &record.instance).unwrap_err();
        assert!(format!("{read_error:#}").contains("read"));
        let publish_error = record_authorized_at(&ledger, &record).unwrap_err();
        assert!(format!("{publish_error:#}").contains("publish managed SSH custody record"));
        let remove_error = remove_record_path(
            &crate::fsperm::open_private_dir(&ledger).unwrap(),
            &record.provider,
            &record.account,
            &record.instance,
        )
        .unwrap_err();
        assert!(format!("{remove_error:#}").contains("remove"));
        assert!(
            path.is_dir(),
            "failed mutations must retain custody evidence"
        );
    }

    #[test]
    fn malformed_record_fails_list_and_cannot_be_revoked_as_missing() {
        let temp = tempfile::tempdir().unwrap();
        let record = record();
        let path = record_path(
            temp.path(),
            &record.provider,
            &record.account,
            &record.instance,
        );
        std::fs::write(&path, b"{not valid json").unwrap();

        assert!(list_at(temp.path()).is_err());
        assert!(
            read_at(
                temp.path(),
                &record.provider,
                &record.account,
                &record.instance
            )
            .is_err()
        );
        assert!(
            record_revoked_at(
                temp.path(),
                &record.provider,
                &record.account,
                &record.instance
            )
            .is_err()
        );
        assert!(path.exists(), "corrupt custody evidence must be preserved");
    }

    #[test]
    fn unreadable_json_entry_fails_the_complete_inventory() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("unreadable.json");
        std::fs::create_dir(&path).unwrap();
        let error = list_at(temp.path()).unwrap_err();
        assert!(error.to_string().contains("read"));
        assert!(path.exists());
    }

    #[test]
    fn vendor_key_name_is_the_sha256_of_the_decoded_key() {
        let key = "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIIgVgF3FLyN2aHUalBpkk3cMVfTgD+7TrbdfTAcSvLvB thegn";
        let name = vendor_key_name(key).unwrap();

        assert_eq!(name, "thegn-managed-ef034764f5154129dbb127b9c94ec413");
        assert_eq!(name.len(), 14 + 32);
    }

    #[test]
    fn vendor_key_name_ignores_comment_and_whitespace() {
        let key = "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIIgVgF3FLyN2aHUalBpkk3cMVfTgD+7TrbdfTAcSvLvB thegn";
        let same_key = "  ssh-ed25519\n\tAAAAC3NzaC1lZDI1NTE5AAAAIIgVgF3FLyN2aHUalBpkk3cMVfTgD+7TrbdfTAcSvLvB   other comment ";
        let different_key =
            "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAICXLY4ay5H8w/Ud7QvnU1EtrjkvENKLoy7B0YU17bVzq vec2";

        assert_eq!(
            vendor_key_name(key).unwrap(),
            vendor_key_name(same_key).unwrap()
        );
        assert_ne!(
            vendor_key_name(key).unwrap(),
            vendor_key_name(different_key).unwrap()
        );
    }

    #[test]
    fn vendor_key_name_refuses_unparsable_keys() {
        for key in [
            "",
            "ssh-ed25519 AAAATEST x",
            "ssh-ed25519 AAAA",
            "ssh-ed25519 AAAABXNz",
            "ssh-rsa AAAAC3NzaC1lZDI1NTE5AAAAIIgVgF3FLyN2aHUalBpkk3cMVfTgD+7TrbdfTAcSvLvB",
            "ssh-ed25519",
        ] {
            assert!(vendor_key_name(key).is_err(), "accepted key: {key:?}");
        }
    }

    #[test]
    fn publish_waits_for_a_held_ledger_lock() {
        let temp = tempfile::tempdir().unwrap();
        let dir = temp.path().to_path_buf();
        let row = record();
        let held = lock_ledger(&dir).unwrap();
        let (sender, receiver) = std::sync::mpsc::channel();
        let worker_dir = dir.clone();
        let worker = std::thread::spawn(move || {
            sender
                .send(record_authorized_at(&worker_dir, &row))
                .unwrap();
        });

        assert!(matches!(
            receiver.recv_timeout(std::time::Duration::from_millis(150)),
            Err(std::sync::mpsc::RecvTimeoutError::Timeout)
        ));
        assert_eq!(
            read_at(&dir, "fly", "env:FLY_WORK", "tg-project-feature").unwrap(),
            None
        );
        drop(held);

        receiver.recv().unwrap().unwrap();
        worker.join().unwrap();
        assert!(
            read_at(&dir, "fly", "env:FLY_WORK", "tg-project-feature")
                .unwrap()
                .is_some()
        );
    }

    #[test]
    fn revoke_waits_for_a_held_ledger_lock() {
        let temp = tempfile::tempdir().unwrap();
        let dir = temp.path().to_path_buf();
        let row = record_authorized_at(&dir, &record()).unwrap();
        let held = lock_ledger(&dir).unwrap();
        let (sender, receiver) = std::sync::mpsc::channel();
        let worker_dir = dir.clone();
        let worker = std::thread::spawn(move || {
            sender
                .send(record_revoked_at(
                    &worker_dir,
                    "fly",
                    "env:FLY_WORK",
                    "tg-project-feature",
                ))
                .unwrap();
        });

        assert!(matches!(
            receiver.recv_timeout(std::time::Duration::from_millis(150)),
            Err(std::sync::mpsc::RecvTimeoutError::Timeout)
        ));
        assert_eq!(
            read_at(&dir, "fly", "env:FLY_WORK", "tg-project-feature").unwrap(),
            Some(row)
        );
        drop(held);

        receiver.recv().unwrap().unwrap();
        worker.join().unwrap();
        assert_eq!(
            read_at(&dir, "fly", "env:FLY_WORK", "tg-project-feature").unwrap(),
            None
        );
    }

    #[test]
    fn compare_and_publish_refuses_a_changed_row_and_keeps_it() {
        let temp = tempfile::tempdir().unwrap();
        let input = record();
        let changed = AuthorizedKeyRecord {
            key_fingerprint: "SHA256:changed".into(),
            ..input.clone()
        };
        let next = AuthorizedKeyRecord {
            key_fingerprint: "SHA256:next".into(),
            ..input.clone()
        };
        let original = record_authorized_at(temp.path(), &input).unwrap();
        let changed = record_authorized_at(temp.path(), &changed).unwrap();

        let result = publish_if_unchanged_at(temp.path(), &original, &next);

        assert!(
            result
                .unwrap_err()
                .downcast_ref::<CustodyChanged>()
                .is_some()
        );
        assert_eq!(
            read_at(
                temp.path(),
                &original.provider,
                &original.account,
                &original.instance
            )
            .unwrap(),
            Some(changed)
        );
    }

    #[test]
    fn compare_and_publish_refuses_a_changed_identity_before_writing() {
        let temp = tempfile::tempdir().unwrap();
        let input = record();
        let changed_identity = AuthorizedKeyRecord {
            instance: "another-instance".into(),
            ..input.clone()
        };
        let original = record_authorized_at(temp.path(), &input).unwrap();

        let result = publish_if_unchanged_at(temp.path(), &original, &changed_identity);

        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("cannot change the row identity")
        );
        assert_eq!(
            read_at(
                temp.path(),
                &original.provider,
                &original.account,
                &original.instance
            )
            .unwrap(),
            Some(original)
        );
    }

    #[test]
    fn concurrent_compare_and_publish_from_one_snapshot_has_exactly_one_winner() {
        let temp = tempfile::tempdir().unwrap();
        let dir = temp.path();
        let original = record_authorized_at(dir, &record()).unwrap();
        // Every scoped writer starts from the same stored snapshot. The shared
        // barrier releases all eight before any writer can publish; every
        // handle is collected before joining so the race is concurrent.
        let barrier = std::sync::Barrier::new(8);
        let outcomes = std::thread::scope(|scope| {
            let handles = (0..8)
                .map(|writer| {
                    let barrier = &barrier;
                    let original = &original;
                    scope.spawn(move || {
                        let next = AuthorizedKeyRecord {
                            key_fingerprint: format!("SHA256:writer-{writer}"),
                            ..original.clone()
                        };
                        barrier.wait();
                        let result = publish_if_unchanged_at(dir, original, &next);
                        (next, result)
                    })
                })
                .collect::<Vec<_>>();
            handles
                .into_iter()
                .map(|handle| handle.join().unwrap())
                .collect::<Vec<_>>()
        });

        let mut winners = Vec::new();
        let mut changed = 0;
        for (_next, result) in outcomes {
            match result {
                Ok(published) => winners.push(published),
                Err(error) => {
                    assert!(error.downcast_ref::<CustodyChanged>().is_some());
                    changed += 1;
                }
            }
        }
        assert_eq!(winners.len(), 1);
        assert_eq!(changed, 7);
        assert_eq!(
            read_at(
                dir,
                &original.provider,
                &original.account,
                &original.instance
            )
            .unwrap(),
            Some(winners[0].clone())
        );
    }

    #[test]
    fn provision_publish_cannot_replace_a_recovery_row_written_while_it_waited() {
        let temp = tempfile::tempdir().unwrap();
        let dir = temp.path().to_path_buf();
        let key = "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIIgVgF3FLyN2aHUalBpkk3cMVfTgD+7TrbdfTAcSvLvB thegn";
        let recovering = recovering_record("SHA256:old");
        let initial = AuthorizedKeyRecord {
            rotation_recovery: None,
            ..recovering.clone()
        };
        record_authorized_at(&dir, &initial).unwrap();
        let held = lock_ledger(&dir).unwrap();
        let (sender, receiver) = std::sync::mpsc::channel();
        let worker_dir = dir.clone();
        let worker = std::thread::spawn(move || {
            sender
                .send(record_authorized_with_proxy_worktree_at(
                    &worker_dir,
                    "fly",
                    "account",
                    "instance",
                    Path::new("/k"),
                    key,
                    None,
                ))
                .unwrap();
        });

        assert!(matches!(
            receiver.recv_timeout(std::time::Duration::from_millis(150)),
            Err(std::sync::mpsc::RecvTimeoutError::Timeout)
        ));
        publish_locked(&crate::fsperm::open_private_dir(&dir).unwrap(), &recovering).unwrap();
        drop(held);

        let result = receiver.recv().unwrap();
        worker.join().unwrap();
        assert!(
            result
                .unwrap_err()
                .downcast_ref::<RotationRecoveryPending>()
                .is_some()
        );
        assert!(
            read_at(&dir, "fly", "account", "instance")
                .unwrap()
                .unwrap()
                .rotation_recovery
                .is_some()
        );
    }

    #[test]
    fn rotation_completion_refuses_a_changed_row() {
        let temp = tempfile::tempdir().unwrap();
        let old_key =
            "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAICXLY4ay5H8w/Ud7QvnU1EtrjkvENKLoy7B0YU17bVzq vec2";
        let new_key = "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIIgVgF3FLyN2aHUalBpkk3cMVfTgD+7TrbdfTAcSvLvB thegn";
        let original_input = recovering_record(&try_key_fingerprint(old_key).unwrap());
        let changed = AuthorizedKeyRecord {
            key_fingerprint: "SHA256:changed".into(),
            ..original_input.clone()
        };
        let original = record_authorized_at(temp.path(), &original_input).unwrap();
        let changed = record_authorized_at(temp.path(), &changed).unwrap();

        let result = record_rotation_completed_at(
            temp.path(),
            &original,
            Path::new("/state/new-key"),
            new_key,
        );

        assert!(
            result
                .unwrap_err()
                .downcast_ref::<CustodyChanged>()
                .is_some()
        );
        assert_eq!(
            read_at(temp.path(), "fly", "account", "instance").unwrap(),
            Some(changed)
        );
    }

    #[test]
    fn ledger_lock_file_is_not_listed_as_custody() {
        let temp = tempfile::tempdir().unwrap();
        let row = record_authorized_at(temp.path(), &record()).unwrap();

        assert_eq!(list_at(temp.path()).unwrap(), vec![row]);
        // The base already ignores non-JSON entries; the lock's existence is
        // the new behavior this positive-control test checks.
        assert!(temp.path().join(".custody.lock").exists());
    }

    #[test]
    fn ledger_lock_refuses_a_non_file_lock_path() {
        let temp = tempfile::tempdir().unwrap();
        std::fs::create_dir(temp.path().join(".custody.lock")).unwrap();

        let result = record_authorized_at(temp.path(), &record());

        assert!(result.is_err());
        assert!(list_at(temp.path()).unwrap().is_empty());
    }

    #[test]
    fn ledger_lock_refuses_a_dangling_symlink() {
        let temp = tempfile::tempdir().unwrap();
        let lock = temp.path().join(".custody.lock");
        let target = temp.path().join("missing-lock-target");
        if let Err(error) = crate::fsperm::create_symlink(&target, &lock, false) {
            assert!(
                matches!(
                    error.kind(),
                    std::io::ErrorKind::PermissionDenied | std::io::ErrorKind::Unsupported
                ),
                "unexpected symlink fixture error: {error}"
            );
            return;
        }

        let result = record_authorized_at(temp.path(), &record());

        assert!(result.is_err());
        assert!(!target.exists());
        assert!(list_at(temp.path()).unwrap().is_empty());
    }

    #[test]
    fn custody_row_symlink_is_refused_not_followed() {
        let temp = tempfile::tempdir().unwrap();
        let ledger = temp.path().join("ledger");
        std::fs::create_dir(&ledger).unwrap();
        crate::fsperm::restrict_dir_to_owner(&ledger).unwrap();
        let row = record();
        let target = temp.path().join("canary.json.txt");
        let canary = serde_json::to_vec(&row).unwrap();
        std::fs::write(&target, &canary).unwrap();
        let link = record_path(&ledger, &row.provider, &row.account, &row.instance);
        if let Err(error) = crate::fsperm::create_symlink(&target, &link, false) {
            assert!(
                matches!(
                    error.kind(),
                    std::io::ErrorKind::PermissionDenied | std::io::ErrorKind::Unsupported
                ),
                "unexpected symlink fixture error: {error}"
            );
            return;
        }

        assert!(read_at(&ledger, &row.provider, &row.account, &row.instance).is_err());
        assert!(list_at(&ledger).is_err());
        assert!(record_revoked_at(&ledger, &row.provider, &row.account, &row.instance).is_err());
        assert_eq!(std::fs::read(&target).unwrap(), canary);
    }

    #[test]
    fn oversized_custody_row_is_refused() {
        let temp = tempfile::tempdir().unwrap();
        crate::fsperm::restrict_dir_to_owner(temp.path()).unwrap();
        let row = record();
        let mut body = serde_json::to_vec(&row).unwrap();
        body.extend(std::iter::repeat_n(b' ', MAX_CUSTODY_ROW_BYTES as usize));
        let path = record_path(temp.path(), &row.provider, &row.account, &row.instance);
        crate::fsperm::write_owner_only_atomic(&path, &body).unwrap();

        let error = read_at(temp.path(), &row.provider, &row.account, &row.instance).unwrap_err();
        assert!(format!("{error:#}").contains("exceeds"));
        assert!(list_at(temp.path()).is_err());
    }

    #[test]
    fn hard_linked_custody_row_is_refused() {
        let temp = tempfile::tempdir().unwrap();
        let row = record_authorized_at(temp.path(), &record()).unwrap();
        let path = record_path(temp.path(), &row.provider, &row.account, &row.instance);
        let alias = temp.path().join("alias.txt");
        std::fs::hard_link(&path, &alias).unwrap();
        let extra_links = crate::fsperm::extra_hard_links(&std::fs::metadata(&path).unwrap());
        if extra_links == 0 {
            return;
        }
        assert_eq!(extra_links, 1, "one planted hard link adds one link name");

        let error = read_at(temp.path(), &row.provider, &row.account, &row.instance).unwrap_err();
        assert!(format!("{error:#}").contains("hard links"));
    }

    #[test]
    fn custody_inventory_over_the_row_cap_is_refused() {
        let temp = tempfile::tempdir().unwrap();
        crate::fsperm::restrict_dir_to_owner(temp.path()).unwrap();
        for row in 0..=MAX_CUSTODY_ROWS {
            let path = temp.path().join(format!("row-{row}.json"));
            std::fs::write(&path, []).unwrap();
            crate::fsperm::set_mode_for_test(&path, 0o600).unwrap();
        }

        let error = list_at(temp.path()).unwrap_err();
        assert!(format!("{error:#}").contains("more than"));
    }

    #[test]
    fn custody_writes_stay_in_the_held_directory_when_the_path_is_swapped() {
        let temp = tempfile::tempdir().unwrap();
        let dir = temp.path().join("ledger");
        let moved = temp.path().join("ledger-moved");
        let ledger = lock_ledger(&dir).unwrap();
        std::fs::rename(&dir, &moved).unwrap();
        std::fs::create_dir(&dir).unwrap();
        crate::fsperm::restrict_dir_to_owner(&dir).unwrap();

        publish_locked(&ledger.dir, &record()).unwrap();

        let row_path = record_path(&moved, "fly", "env:FLY_WORK", "tg-project-feature");
        assert!(row_path.exists());
        assert!(std::fs::read_dir(&dir).unwrap().next().is_none());
    }

    #[test]
    fn custody_directory_that_is_not_owner_only_is_refused() {
        let temp = tempfile::tempdir().unwrap();
        let row = record();
        record_authorized_at(temp.path(), &row).unwrap();
        if let Err(error) = crate::fsperm::set_mode_for_test(temp.path(), 0o755) {
            assert!(
                matches!(
                    error.kind(),
                    std::io::ErrorKind::PermissionDenied | std::io::ErrorKind::Unsupported
                ),
                "unexpected mode fixture error: {error}"
            );
            return;
        }

        let read_error =
            read_at(temp.path(), &row.provider, &row.account, &row.instance).unwrap_err();
        assert!(format!("{read_error:#}").contains("custody directory"));
        let list_error = list_at(temp.path()).unwrap_err();
        assert!(format!("{list_error:#}").contains("custody directory"));
        crate::fsperm::set_mode_for_test(temp.path(), 0o700).unwrap();
    }
}
