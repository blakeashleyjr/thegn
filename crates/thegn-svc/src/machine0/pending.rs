//! Durable intent records for machine0 creates.
//!
//! A record means a create may have reached the provider, including when its
//! response or readiness result was lost. It remains on every ambiguous outcome
//! and is retired only after readiness succeeds or destroy retires local state.

use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(super) struct PendingCreate {
    pub(super) name: String,
    pub(super) created_at: u64,
    pub(super) pid: u32,
}

/// Write `<registry>/machine0-pending.d/<name>.json` atomically, owner-only.
/// Refuses an invalid name.
pub(super) fn record_at(registry: &Path, name: &str) -> Result<()> {
    crate::vps::registry::checked_name(name)?;
    let pending_dir = crate::vps::registry::ensure_machine0_pending_dir_at(registry)?;
    let record = PendingCreate {
        name: name.to_string(),
        created_at: SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |duration| duration.as_secs()),
        pid: std::process::id(),
    };
    let bytes = serde_json::to_vec(&record).context("machine0: serialize pending create record")?;
    pending_dir
        .write_owner_only_atomic(&format!("{name}.json"), &bytes)
        .context("machine0: write pending create record")?;
    Ok(())
}

/// Remove the record; an absent record (or absent directory) is Ok.
pub(super) fn remove_at(registry: &Path, name: &str) -> Result<()> {
    crate::vps::registry::checked_name(name)?;
    let Some(pending_dir) = crate::vps::registry::open_machine0_pending_dir_at(registry)? else {
        return Ok(());
    };
    pending_dir
        .remove_file(&format!("{name}.json"))
        .context("machine0: remove pending create record")?;
    Ok(())
}

/// Every valid pending filename, even if its contents cannot be read or parsed.
/// Non-JSON entries and entries with invalid names are skipped.
pub(super) fn names_at(registry: &Path) -> Result<Vec<String>> {
    let Some(pending_dir) = crate::vps::registry::open_machine0_pending_dir_at(registry)? else {
        return Ok(Vec::new());
    };
    let mut names = pending_dir
        .file_names()
        .context("machine0: list pending create records")?
        .into_iter()
        .filter_map(|entry| {
            let path = Path::new(&entry);
            if path.extension().is_none_or(|extension| extension != "json") {
                return None;
            }
            let stem = path.file_stem()?.to_str()?;
            crate::vps::registry::checked_name(stem)
                .ok()
                .map(str::to_owned)
        })
        .collect::<Vec<_>>();
    names.sort_unstable();
    names.dedup();
    Ok(names)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Admission {
    Create,
    Adopt,
}

#[cfg(test)]
mod tests {
    use super::{names_at, record_at, remove_at};

    #[test]
    fn record_list_remove_roundtrip_is_owner_only() {
        let root = tempfile::tempdir().unwrap();
        let registry = root.path().join("vps");

        record_at(&registry, "dev").unwrap();
        record_at(&registry, "web").unwrap();

        assert_eq!(
            names_at(&registry).unwrap(),
            vec!["dev".to_string(), "web".to_string()]
        );
        let record_path = registry
            .join(crate::vps::registry::MACHINE0_PENDING_DIR)
            .join("dev.json");
        if let Some(mode) = thegn_core::fsperm::mode_bits(&record_path).unwrap() {
            assert_eq!(mode & 0o777, 0o600);
        }

        remove_at(&registry, "dev").unwrap();
        assert_eq!(names_at(&registry).unwrap(), vec!["web".to_string()]);
        remove_at(&registry, "absent").unwrap();
    }

    #[test]
    fn an_unparsable_pending_record_still_counts() {
        let root = tempfile::tempdir().unwrap();
        let registry = root.path().join("vps");
        let pending_dir = crate::vps::registry::ensure_machine0_pending_dir_at(&registry).unwrap();
        std::fs::write(pending_dir.path().join("broken.json"), b"{").unwrap();
        std::fs::write(pending_dir.path().join("tmp-record"), b"ignored").unwrap();

        assert_eq!(names_at(&registry).unwrap(), vec!["broken".to_string()]);
    }

    #[test]
    fn an_absent_pending_dir_lists_nothing() {
        let root = tempfile::tempdir().unwrap();
        assert!(names_at(root.path()).unwrap().is_empty());
    }

    #[test]
    fn record_at_refuses_an_invalid_name() {
        let root = tempfile::tempdir().unwrap();
        let registry = root.path().join("vps");

        assert!(record_at(&registry, "../x").is_err());
        assert!(!registry.exists());
    }
}
