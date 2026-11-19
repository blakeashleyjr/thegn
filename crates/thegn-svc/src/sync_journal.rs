//! Durable upload state for provider-backed worktree synchronization.

use std::collections::BTreeMap;
use std::io::Read;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

pub const JOURNAL_VERSION: u32 = 1;

const MAX_ENTRY_BYTES: usize = 16 * 1024 * 1024;
const MAX_ENTRIES: usize = 4096;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum SyncState {
    Uploaded,
    Pulling,
    Failed { reason: String },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SyncJournalEntry {
    pub version: u32,
    pub worktree: String,
    pub env_name: String,
    pub provider: String,
    pub sandbox: String,
    pub workdir: String,
    /// Path -> lowercase hex SHA-256 (the upload baseline,
    /// [`crate::provider::SyncBaseline`]).
    pub baseline: BTreeMap<String, String>,
    pub state: SyncState,
    pub uploaded_at_ms: i64,
    pub updated_at_ms: i64,
}

/// The canonical journal location: `$XDG_STATE_HOME/thegn/provider-sync`.
pub fn dir() -> PathBuf {
    thegn_core::util::xdg_state_home().join("thegn/provider-sync")
}

/// The on-disk path for one worktree's journal entry.
pub fn entry_path(directory: &Path, worktree: &str) -> PathBuf {
    directory.join(file_name(worktree))
}

/// Publish one journal entry atomically beneath an owner-only journal root.
pub fn record_at(directory: &Path, entry: &SyncJournalEntry) -> Result<()> {
    let root = open_root(directory, true)?.ok_or_else(|| {
        anyhow::anyhow!("provider sync journal root was not opened after creation")
    })?;
    let bytes = serde_json::to_vec_pretty(entry).context("serialize provider sync journal")?;
    root.write_owner_only_atomic(&file_name(&entry.worktree), &bytes)
        .context("publish provider sync journal entry")?;
    Ok(())
}

/// Load one entry. Only an absent root or absent entry is reported as `None`;
/// malformed, unsupported, or mismatched entries are errors.
pub fn load_at(directory: &Path, worktree: &str) -> Result<Option<SyncJournalEntry>> {
    let Some(root) = open_root(directory, false)? else {
        return Ok(None);
    };
    let name = file_name(worktree);
    let mut file = match root.open_regular_nofollow(&name) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(error).with_context(|| format!("open provider sync journal entry {name}"));
        }
    };
    let entry = read_entry(&mut file, &name)?;
    Ok(Some(validate_entry(entry, worktree)?))
}

/// Update only the state and update timestamp of an existing entry.
pub fn set_state_at(directory: &Path, worktree: &str, state: SyncState, now_ms: i64) -> Result<()> {
    let mut entry = load_at(directory, worktree)?
        .ok_or_else(|| anyhow::anyhow!("provider sync journal entry is absent"))?;
    entry.state = state;
    entry.updated_at_ms = now_ms;
    record_at(directory, &entry)
}

/// Remove one entry. Removing an absent root or entry is successful and returns
/// `false`.
pub fn remove_at(directory: &Path, worktree: &str) -> Result<bool> {
    let Some(root) = open_root(directory, false)? else {
        return Ok(false);
    };
    root.remove_file(&file_name(worktree))
        .context("remove provider sync journal entry")
}

/// Load every journal entry, refusing an unexpectedly large store.
pub fn list_at(directory: &Path) -> Result<Vec<SyncJournalEntry>> {
    let Some(root) = open_root(directory, false)? else {
        return Ok(Vec::new());
    };
    let all_names = root.file_names().context("list provider sync journal")?;
    anyhow::ensure!(
        all_names.len() <= MAX_ENTRIES,
        "provider sync journal contains more than {MAX_ENTRIES} entries"
    );
    let mut names = all_names
        .into_iter()
        .filter_map(|name| {
            let name = name.to_str()?;
            Path::new(name)
                .extension()
                .is_some_and(|extension| extension == "json")
                .then_some(name.to_owned())
        })
        .collect::<Vec<_>>();
    names.sort_unstable();

    names
        .into_iter()
        .map(|name| {
            let mut file = root
                .open_regular_nofollow(&name)
                .with_context(|| format!("open provider sync journal entry {name}"))?;
            read_entry(&mut file, &name)
        })
        .collect()
}

pub fn baseline_to_hex(baseline: &crate::provider::SyncBaseline) -> BTreeMap<String, String> {
    baseline
        .iter()
        .map(|(path, digest)| (path.clone(), encode_hex(digest)))
        .collect()
}

pub fn baseline_from_hex(
    encoded: &BTreeMap<String, String>,
) -> Result<crate::provider::SyncBaseline> {
    encoded
        .iter()
        .map(|(path, value)| Ok((path.clone(), decode_digest(value)?)))
        .collect()
}

fn file_name(worktree: &str) -> String {
    let digest = Sha256::digest(worktree.as_bytes());
    format!("{}.json", encode_hex(&digest[..16]))
}

fn open_root(directory: &Path, create: bool) -> Result<Option<thegn_core::fsperm::PrivateDir>> {
    if create {
        thegn_core::fsperm::ensure_private_dir(directory)
            .context("provider sync journal root is not an owned private directory")?;
        return thegn_core::fsperm::open_private_dir(directory)
            .map(Some)
            .context("provider sync journal root is not an owned private directory");
    }

    let metadata = match std::fs::symlink_metadata(directory) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    anyhow::ensure!(
        !metadata.file_type().is_symlink()
            && metadata.is_dir()
            && thegn_core::fsperm::owned_by_current_user(&metadata),
        "provider sync journal root is not an owned real directory"
    );
    thegn_core::fsperm::restrict_dir_to_owner(directory)
        .context("provider sync journal root is not an owned private directory")?;
    thegn_core::fsperm::open_private_dir(directory)
        .map(Some)
        .context("provider sync journal root is not an owned private directory")
}

fn read_entry(file: &mut std::fs::File, name: &str) -> Result<SyncJournalEntry> {
    let mut bytes = Vec::new();
    file.take((MAX_ENTRY_BYTES + 1) as u64)
        .read_to_end(&mut bytes)
        .with_context(|| format!("read provider sync journal entry {name}"))?;
    anyhow::ensure!(
        bytes.len() <= MAX_ENTRY_BYTES,
        "provider sync journal entry {name} exceeds {MAX_ENTRY_BYTES} bytes"
    );
    let entry: SyncJournalEntry = serde_json::from_slice(&bytes)
        .with_context(|| format!("parse provider sync journal entry {name}"))?;
    anyhow::ensure!(
        entry.version == JOURNAL_VERSION,
        "provider sync journal entry {name} has unsupported version {}",
        entry.version
    );
    Ok(entry)
}

fn validate_entry(entry: SyncJournalEntry, worktree: &str) -> Result<SyncJournalEntry> {
    anyhow::ensure!(
        entry.worktree == worktree,
        "provider sync journal entry names another worktree"
    );
    Ok(entry)
}

fn encode_hex(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut encoded = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        encoded.push(HEX[(byte >> 4) as usize] as char);
        encoded.push(HEX[(byte & 0x0f) as usize] as char);
    }
    encoded
}

fn decode_digest(value: &str) -> Result<[u8; 32]> {
    anyhow::ensure!(
        value.len() == 64 && value.bytes().all(|byte| byte.is_ascii_hexdigit()),
        "sync baseline digest must be exactly 64 hexadecimal characters"
    );
    let mut digest = [0u8; 32];
    for (index, pair) in value.as_bytes().chunks_exact(2).enumerate() {
        digest[index] = (hex_nibble(pair[0]) << 4) | hex_nibble(pair[1]);
    }
    Ok(digest)
}

fn hex_nibble(byte: u8) -> u8 {
    match byte {
        b'0'..=b'9' => byte - b'0',
        b'a'..=b'f' => byte - b'a' + 10,
        b'A'..=b'F' => byte - b'A' + 10,
        _ => unreachable!("validated hexadecimal input"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(worktree: &str) -> SyncJournalEntry {
        SyncJournalEntry {
            version: JOURNAL_VERSION,
            worktree: worktree.to_owned(),
            env_name: "env-a".to_owned(),
            provider: "sprites".to_owned(),
            sandbox: "sandbox-a".to_owned(),
            workdir: "/workspace".to_owned(),
            baseline: BTreeMap::from([("src/lib.rs".to_owned(), "00".repeat(32))]),
            state: SyncState::Uploaded,
            uploaded_at_ms: 100,
            updated_at_ms: 100,
        }
    }

    fn write_raw(directory: &Path, worktree: &str, bytes: &[u8]) {
        let root = thegn_core::fsperm::open_private_dir(directory).unwrap();
        root.write_owner_only_atomic(&file_name(worktree), bytes)
            .unwrap();
    }

    #[test]
    fn sync_journal_round_trips_an_entry() {
        let _state = thegn_core::testenv::IsolatedState::new();
        let temp = tempfile::tempdir().unwrap();
        let original = entry("/worktrees/a");

        record_at(temp.path(), &original).unwrap();

        assert_eq!(
            load_at(temp.path(), &original.worktree).unwrap(),
            Some(original)
        );
    }

    #[test]
    fn sync_journal_files_are_owner_only() {
        let _state = thegn_core::testenv::IsolatedState::new();
        let temp = tempfile::tempdir().unwrap();
        let original = entry("/worktrees/a");

        record_at(temp.path(), &original).unwrap();

        assert!(
            thegn_core::fsperm::mode_bits(temp.path())
                .unwrap()
                .is_none_or(|mode| mode == 0o700)
        );
        assert!(
            thegn_core::fsperm::mode_bits(&temp.path().join(file_name(&original.worktree)))
                .unwrap()
                .is_none_or(|mode| mode == 0o600)
        );
    }

    #[test]
    fn sync_journal_refuses_a_symlinked_root() {
        let _state = thegn_core::testenv::IsolatedState::new();
        let temp = tempfile::tempdir().unwrap();
        let real = temp.path().join("real");
        let link = temp.path().join("link");
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

        assert!(record_at(&link, &entry("/worktrees/a")).is_err());
        assert!(real.read_dir().unwrap().next().is_none());
    }

    #[test]
    fn sync_journal_corrupt_entry_is_an_error_not_absent() {
        let _state = thegn_core::testenv::IsolatedState::new();
        let temp = tempfile::tempdir().unwrap();
        let original = entry("/worktrees/a");
        record_at(temp.path(), &original).unwrap();
        write_raw(temp.path(), &original.worktree, b"{");

        assert!(load_at(temp.path(), &original.worktree).is_err());
    }

    #[test]
    fn sync_journal_wrong_worktree_is_an_error() {
        let _state = thegn_core::testenv::IsolatedState::new();
        let temp = tempfile::tempdir().unwrap();
        let original = entry("/worktrees/a");
        let requested = "/worktrees/b";
        thegn_core::fsperm::ensure_private_dir(temp.path()).unwrap();
        write_raw(
            temp.path(),
            requested,
            &serde_json::to_vec(&original).unwrap(),
        );

        assert!(load_at(temp.path(), requested).is_err());
    }

    #[test]
    fn sync_journal_set_state_updates_only_state_and_time() {
        let _state = thegn_core::testenv::IsolatedState::new();
        let temp = tempfile::tempdir().unwrap();
        let original = entry("/worktrees/a");
        record_at(temp.path(), &original).unwrap();

        set_state_at(
            temp.path(),
            &original.worktree,
            SyncState::Failed {
                reason: "network".to_owned(),
            },
            200,
        )
        .unwrap();
        let updated = load_at(temp.path(), &original.worktree).unwrap().unwrap();

        assert_eq!(updated.version, original.version);
        assert_eq!(updated.worktree, original.worktree);
        assert_eq!(updated.env_name, original.env_name);
        assert_eq!(updated.provider, original.provider);
        assert_eq!(updated.sandbox, original.sandbox);
        assert_eq!(updated.workdir, original.workdir);
        assert_eq!(updated.baseline, original.baseline);
        assert_eq!(updated.uploaded_at_ms, original.uploaded_at_ms);
        assert_eq!(
            updated.state,
            SyncState::Failed {
                reason: "network".into()
            }
        );
        assert_eq!(updated.updated_at_ms, 200);
    }

    #[test]
    fn sync_journal_remove_is_idempotent() {
        let _state = thegn_core::testenv::IsolatedState::new();
        let temp = tempfile::tempdir().unwrap();
        let original = entry("/worktrees/a");
        record_at(temp.path(), &original).unwrap();

        assert!(remove_at(temp.path(), &original.worktree).unwrap());
        assert!(!remove_at(temp.path(), &original.worktree).unwrap());
        assert_eq!(load_at(temp.path(), &original.worktree).unwrap(), None);
    }

    #[test]
    fn sync_journal_list_is_bounded() {
        let _state = thegn_core::testenv::IsolatedState::new();
        let temp = tempfile::tempdir().unwrap();
        thegn_core::fsperm::ensure_private_dir(temp.path()).unwrap();
        let root = thegn_core::fsperm::open_private_dir(temp.path()).unwrap();
        for index in 0..=MAX_ENTRIES {
            let mut current = entry(&format!("/worktrees/{index}"));
            current.baseline.clear();
            root.write_owner_only_atomic(
                &file_name(&current.worktree),
                &serde_json::to_vec(&current).unwrap(),
            )
            .unwrap();
        }

        assert!(list_at(temp.path()).is_err());
    }

    #[test]
    fn sync_journal_baseline_hex_round_trip() {
        let _state = thegn_core::testenv::IsolatedState::new();
        let baseline = BTreeMap::from([
            ("a.txt".to_owned(), [0xab; 32]),
            ("b.txt".to_owned(), [0x01; 32]),
        ]);

        let encoded = baseline_to_hex(&baseline);
        assert_eq!(encoded["a.txt"], "ab".repeat(32));
        assert_eq!(baseline_from_hex(&encoded).unwrap(), baseline);
    }
}
