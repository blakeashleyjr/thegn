//! Process-local memo for successful current-schema verification of a shared DB.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(super) struct VerificationKey {
    path: PathBuf,
    identity: String,
    version: i64,
}

impl VerificationKey {
    pub(super) fn for_path(path: &Path, version: i64) -> Option<Self> {
        let path = std::fs::canonicalize(path).ok()?;
        let identity = crate::util::file_identity(&path)?;
        Some(Self {
            path,
            identity,
            version,
        })
    }
}

fn verified() -> &'static Mutex<HashSet<VerificationKey>> {
    static VERIFIED: OnceLock<Mutex<HashSet<VerificationKey>>> = OnceLock::new();
    VERIFIED.get_or_init(|| Mutex::new(HashSet::new()))
}

pub(super) fn is_verified(key: &VerificationKey) -> bool {
    verified()
        .lock()
        .unwrap_or_else(|poison| poison.into_inner())
        .contains(key)
}

pub(super) fn verify_once(
    key: VerificationKey,
    verify: impl FnOnce() -> anyhow::Result<()>,
) -> anyhow::Result<()> {
    // An in-place malformation by another process on this inode is not
    // re-detected until restart; replacement files get a different identity.
    let mut verified = verified()
        .lock()
        .unwrap_or_else(|poison| poison.into_inner());
    if verified.contains(&key) {
        return Ok(());
    }
    verify()?;
    verified.insert(key);
    Ok(())
}

#[cfg(test)]
thread_local! {
    static VERIFICATION_COUNT: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

#[cfg(test)]
pub(super) fn count_verification() {
    VERIFICATION_COUNT.with(|count| count.set(count.get() + 1));
}

#[cfg(test)]
fn verification_count() -> usize {
    VERIFICATION_COUNT.with(std::cell::Cell::get)
}

#[cfg(test)]
mod tests {
    use super::{VerificationKey, verification_count};
    use crate::db::Db;

    fn ready_db(path: &Path) {
        // This is the same canonical shared-path branch used by `open_at` for
        // the state DB; ordinary unrelated `open_at` paths intentionally do
        // not memoize verification.
        let conn = Db::open_connection(path).unwrap();
        let db = Db::init_with(conn, true, Some(path)).unwrap();
        drop(db);
    }

    use std::path::Path;

    #[test]
    fn fast_open_memo_is_path_and_inode_specific() {
        let dir = tempfile::tempdir().unwrap();
        let first = dir.path().join("first.db");
        let second = dir.path().join("second.db");
        ready_db(&first);
        ready_db(&first); // first fast open performs and memoizes verification
        let before = verification_count();
        ready_db(&first);
        assert_eq!(verification_count(), before, "same file verifies once");
        ready_db(&second);
        ready_db(&second);
        assert_eq!(
            verification_count(),
            before + 1,
            "different path verifies again"
        );

        let replacement = dir.path().join("replacement.db");
        ready_db(&replacement);
        std::fs::rename(&replacement, &first).unwrap();
        ready_db(&first);
        assert_eq!(
            verification_count(),
            before + 2,
            "replacement inode verifies again"
        );
    }

    #[test]
    fn verification_key_includes_schema_version() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("schema.db");
        ready_db(&path);
        let v1 = VerificationKey::for_path(&path, 1).unwrap();
        let v2 = VerificationKey::for_path(&path, 2).unwrap();
        assert_ne!(v1, v2);
    }
}
