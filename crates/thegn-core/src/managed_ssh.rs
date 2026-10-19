//! Secret-free custody ledger for SSH keys authorized on managed sandboxes.
//!
//! One JSON record per provider instance captures the exact private-key path
//! and public-key fingerprint authorized at provision time. The private key and
//! public-key body never enter the ledger. Provider destroy removes the record
//! only after remote deletion succeeds, so a failed teardown remains visible to
//! rotation/audit rather than silently losing revocation state.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AuthorizedKeyRecord {
    pub provider: String,
    pub account: String,
    pub instance: String,
    pub key_path: PathBuf,
    pub key_fingerprint: String,
    pub authorized_at: i64,
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
    Ok(format!(
        "SHA256:{}",
        base64_unpadded(&Sha256::digest(&blob))
    ))
}

pub fn fingerprint_is_verified(fingerprint: &str) -> bool {
    fingerprint.starts_with("SHA256:")
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

fn record_path(dir: &Path, provider: &str, account: &str, instance: &str) -> PathBuf {
    let mut tuple = Vec::new();
    for part in [provider.as_bytes(), account.as_bytes(), instance.as_bytes()] {
        tuple.extend_from_slice(&(part.len() as u64).to_be_bytes());
        tuple.extend_from_slice(part);
    }
    let digest = Sha256::digest(&tuple);
    dir.join(format!("{}.json", hex_prefix(&digest, 64)))
}

fn legacy_record_path(dir: &Path, provider: &str, account: &str, instance: &str) -> PathBuf {
    let id = crate::util::short_hash(&format!("{provider}\0{account}\0{instance}"), 24);
    dir.join(format!("{id}.json"))
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

fn read_record(path: &Path) -> Result<Option<AuthorizedKeyRecord>> {
    let body = match std::fs::read(path) {
        Ok(body) => body,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error).with_context(|| format!("read {}", path.display())),
    };
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
    record_authorized_with_proxy_worktree_at(
        &dir(),
        provider,
        account,
        instance,
        key_path,
        public_key,
        proxy_worktree,
    )?;
    let fingerprint = try_key_fingerprint(public_key)?;
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

fn record_authorized_with_proxy_worktree_at(
    dir: &Path,
    provider: &str,
    account: &str,
    instance: &str,
    key_path: &Path,
    public_key: &str,
    proxy_worktree: Option<&str>,
) -> Result<()> {
    let fingerprint = try_key_fingerprint(public_key)?;
    record_authorized_at(
        dir,
        &AuthorizedKeyRecord {
            provider: provider.to_string(),
            account: account.to_string(),
            instance: instance.to_string(),
            key_path: key_path.to_path_buf(),
            key_fingerprint: fingerprint.clone(),
            authorized_at: crate::util::now(),
            proxy_worktree: proxy_worktree.map(str::to_string),
            rotation_recovery: None,
        },
    )?;
    Ok(())
}

pub fn record_authorized_at(dir: &Path, record: &AuthorizedKeyRecord) -> Result<()> {
    std::fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
    crate::fsperm::restrict_dir_to_owner(dir)
        .with_context(|| format!("restrict {}", dir.display()))?;
    let path = record_path(dir, &record.provider, &record.account, &record.instance);
    let legacy = legacy_record_path(dir, &record.provider, &record.account, &record.instance);
    for candidate in [&path, &legacy] {
        if let Some(existing) = read_record(candidate).with_context(|| {
            format!("publish managed SSH custody record {}", candidate.display())
        })? {
            check_tuple(
                &existing,
                &record.provider,
                &record.account,
                &record.instance,
                candidate,
            )?;
        }
    }
    crate::fsperm::write_owner_only_atomic(&path, &serde_json::to_vec_pretty(record)?)
        .with_context(|| format!("publish managed SSH custody record {}", path.display()))?;
    if legacy != path {
        remove_if_exists(&legacy)?;
    }
    Ok(())
}

fn remove_if_exists(path: &Path) -> Result<()> {
    match std::fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error).with_context(|| format!("remove {}", path.display())),
    }
}

pub fn record_revoked(provider: &str, account: &str, instance: &str) -> Result<()> {
    let existing = read(provider, account, instance)?;
    remove_record_path(&dir(), provider, account, instance)?;
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
    // Parse before delete. An unreadable or corrupt row may be the only local
    // evidence of a remotely authorized key and must never be collapsed into
    // "missing" by a mutation path.
    read_at(dir, provider, account, instance)?;
    remove_record_path(dir, provider, account, instance)
}

fn remove_record_path(dir: &Path, provider: &str, account: &str, instance: &str) -> Result<()> {
    let canonical = record_path(dir, provider, account, instance);
    let legacy = legacy_record_path(dir, provider, account, instance);
    for path in [&canonical, &legacy] {
        if let Some(record) = read_record(path)
            .with_context(|| format!("remove managed SSH custody record {}", path.display()))?
        {
            check_tuple(&record, provider, account, instance, path)?;
        }
    }
    for path in [&canonical, &legacy] {
        remove_if_exists(path)?;
    }
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
    let canonical = record_path(dir, provider, account, instance);
    let legacy = legacy_record_path(dir, provider, account, instance);
    let (path, record) = if let Some(record) = read_record(&canonical)? {
        (&canonical, record)
    } else if let Some(record) = read_record(&legacy)? {
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
    record_authorized_at(&dir(), &updated)?;
    Ok(updated)
}

pub fn list() -> Result<Vec<AuthorizedKeyRecord>> {
    list_at(&dir())
}

pub fn list_at(dir: &Path) -> Result<Vec<AuthorizedKeyRecord>> {
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => {
            return Err(error).with_context(|| format!("read directory {}", dir.display()));
        }
    };
    let mut records = Vec::new();
    let mut tuples = std::collections::HashMap::new();
    for entry in entries {
        let entry = entry.with_context(|| format!("read entry in {}", dir.display()))?;
        let path = entry.path();
        if !path.extension().is_some_and(|ext| ext == "json") {
            continue;
        }
        let body = std::fs::read(&path).with_context(|| format!("read {}", path.display()))?;
        let record = serde_json::from_slice(&body)
            .with_context(|| format!("decode managed SSH custody record {}", path.display()))?;
        let canonical = record_path(dir, &record.provider, &record.account, &record.instance);
        let legacy = legacy_record_path(dir, &record.provider, &record.account, &record.instance);
        anyhow::ensure!(
            path == canonical || path == legacy,
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
        assert_eq!(std::fs::read_dir(temp.path()).unwrap().count(), 0);
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
        std::fs::write(&legacy, serde_json::to_vec(&existing).unwrap()).unwrap();

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
        record_authorized_at(temp.path(), &updated).unwrap();
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
        let record = record();
        record_authorized_at(temp.path(), &record).unwrap();
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
                .next()
                .unwrap()
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
        let mut original = record();
        record_authorized_at(temp.path(), &original).unwrap();

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
        record_authorized_at(temp.path(), &original).unwrap();

        assert_eq!(list_at(temp.path()).unwrap(), vec![original.clone()]);
        assert_eq!(
            read_at(temp.path(), "fly", "env:FLY_WORK", "tg-project-feature").unwrap(),
            Some(original)
        );
        assert_eq!(std::fs::read_dir(temp.path()).unwrap().count(), 1);
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
            "ssh-ed25519 AAAATEST do-not-persist",
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
            "ssh-ed25519 AAAAONE",
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
            "ssh-ed25519 AAAATWO",
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
        record_authorized_at(temp.path(), &first).unwrap();
        record_authorized_at(temp.path(), &second).unwrap();
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
        record_authorized_at(temp.path(), &z).unwrap();
        record_authorized_at(temp.path(), &a).unwrap();
        record_authorized_at(temp.path(), &first).unwrap();

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
        let path = record_path(&ledger, &record.provider, &record.account, &record.instance);
        std::fs::create_dir(&path).unwrap();
        let read_error =
            read_at(&ledger, &record.provider, &record.account, &record.instance).unwrap_err();
        assert!(format!("{read_error:#}").contains("read"));
        let publish_error = record_authorized_at(&ledger, &record).unwrap_err();
        assert!(format!("{publish_error:#}").contains("publish managed SSH custody record"));
        let remove_error =
            remove_record_path(&ledger, &record.provider, &record.account, &record.instance)
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
}
