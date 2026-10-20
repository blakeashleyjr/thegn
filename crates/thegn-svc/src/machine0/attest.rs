use std::io::Read;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Attestation {
    pub version: u32,
    pub vm_name: String,
    pub vm_id: String,
    pub flake_identity: String,
    pub attested_at: i64,
}

pub fn dir_in(root: &Path) -> PathBuf {
    root.join("machine0").join("attest")
}

pub fn load_in(root: &Path, vm_name: &str) -> Option<Attestation> {
    let checked = match crate::vps::registry::checked_name(vm_name) {
        Ok(name) => name,
        Err(error) => {
            tracing::warn!(%error, vm_name, "machine0: invalid provisioning attestation name");
            return None;
        }
    };
    let path = dir_in(root).join(format!("{checked}.json"));
    let mut file = match thegn_core::fsperm::open_regular_file_nofollow(&path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return None,
        Err(error) => {
            tracing::warn!(%error, path = %path.display(), "machine0: cannot open provisioning attestation");
            return None;
        }
    };
    let metadata = match file.metadata() {
        Ok(metadata) => metadata,
        Err(error) => {
            tracing::warn!(%error, path = %path.display(), "machine0: cannot inspect provisioning attestation");
            return None;
        }
    };
    if !thegn_core::fsperm::owned_by_current_user(&metadata) {
        tracing::warn!(path = %path.display(), "machine0: provisioning attestation has a foreign owner");
        return None;
    }
    if let Err(error) = thegn_core::fsperm::ensure_single_link_regular_file(&metadata) {
        tracing::warn!(%error, path = %path.display(), "machine0: provisioning attestation has multiple links");
        return None;
    }
    if metadata.len() > 4096 {
        tracing::warn!(path = %path.display(), "machine0: provisioning attestation exceeds 4 KiB");
        return None;
    }
    let mut bytes = Vec::new();
    if let Err(error) = file.by_ref().take(4097).read_to_end(&mut bytes) {
        tracing::warn!(%error, path = %path.display(), "machine0: cannot read provisioning attestation");
        return None;
    }
    if bytes.len() > 4096 {
        tracing::warn!(path = %path.display(), "machine0: provisioning attestation exceeds 4 KiB");
        return None;
    }
    let record = match serde_json::from_slice::<Attestation>(&bytes) {
        Ok(record) => record,
        Err(error) => {
            tracing::warn!(%error, path = %path.display(), "machine0: corrupt provisioning attestation");
            return None;
        }
    };
    if record.version != 1 || record.vm_name != checked {
        tracing::warn!(
            path = %path.display(),
            version = record.version,
            record_name = %record.vm_name,
            "machine0: provisioning attestation version or name does not match"
        );
        return None;
    }
    Some(record)
}

pub fn store_in(root: &Path, record: &Attestation) -> anyhow::Result<()> {
    let name = crate::vps::registry::checked_name(&record.vm_name)?;
    let dir = dir_in(root);
    thegn_core::fsperm::ensure_private_dir(&dir)?;
    let bytes = serde_json::to_vec(record)?;
    thegn_core::fsperm::write_owner_only_atomic(&dir.join(format!("{name}.json")), &bytes)?;
    Ok(())
}

pub fn remove_in(root: &Path, vm_name: &str) -> std::io::Result<()> {
    let name = crate::vps::registry::checked_name(vm_name).map_err(|error| {
        std::io::Error::new(std::io::ErrorKind::InvalidInput, error.to_string())
    })?;
    match std::fs::remove_file(dir_in(root).join(format!("{name}.json"))) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

pub fn attested(record: Option<&Attestation>, vm_id: &str, flake_identity: &str) -> bool {
    !vm_id.is_empty()
        && record
            .is_some_and(|record| record.vm_id == vm_id && record.flake_identity == flake_identity)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Attestation {
        Attestation {
            version: 1,
            vm_name: "dev".to_string(),
            vm_id: "vm-1".to_string(),
            flake_identity: "tree:abc".to_string(),
            attested_at: 123,
        }
    }

    #[test]
    fn attested_requires_same_vm_id_and_identity() {
        let record = sample();
        let cases = [
            (Some(&record), "vm-1", "tree:abc", true),
            (Some(&record), "vm-2", "tree:abc", false),
            (Some(&record), "vm-1", "tree:def", false),
            (Some(&record), "", "tree:abc", false),
            (None, "vm-1", "tree:abc", false),
        ];
        for (record, vm_id, identity, expected) in cases {
            assert_eq!(attested(record, vm_id, identity), expected);
        }
    }

    #[test]
    fn store_then_load_round_trips_owner_only() {
        let root = tempfile::tempdir().unwrap();
        let record = sample();
        store_in(root.path(), &record).unwrap();
        assert_eq!(load_in(root.path(), "dev"), Some(record));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(dir_in(root.path()).join("dev.json"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777;
            assert_eq!(mode, 0o600);
        }
    }

    #[test]
    fn load_refuses_a_symlinked_record() {
        let root = tempfile::tempdir().unwrap();
        let dir = dir_in(root.path());
        std::fs::create_dir_all(&dir).unwrap();
        let target = dir.join("target.json");
        std::fs::write(&target, serde_json::to_vec(&sample()).unwrap()).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::symlink;
            symlink(target, dir.join("dev.json")).unwrap();
            assert!(load_in(root.path(), "dev").is_none());
        }
        #[cfg(not(unix))]
        {
            assert!(load_in(root.path(), "dev").is_none());
        }
    }

    #[test]
    fn load_treats_corrupt_or_wrong_name_as_unattested() {
        let root = tempfile::tempdir().unwrap();
        let dir = dir_in(root.path());
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("dev.json");
        std::fs::write(&path, b"{").unwrap();
        assert!(load_in(root.path(), "dev").is_none());
        let mut record = sample();
        record.vm_name = "other".to_string();
        std::fs::write(&path, serde_json::to_vec(&record).unwrap()).unwrap();
        assert!(load_in(root.path(), "dev").is_none());
    }

    #[test]
    fn remove_is_idempotent() {
        let root = tempfile::tempdir().unwrap();
        store_in(root.path(), &sample()).unwrap();
        remove_in(root.path(), "dev").unwrap();
        remove_in(root.path(), "dev").unwrap();
        assert!(load_in(root.path(), "dev").is_none());
    }
}
