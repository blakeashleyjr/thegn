//! Add-only discovery of Git worktrees for an already-registered project.
//!
//! Hydration fingerprints the shared Git `worktrees/` administrative
//! directory with filesystem metadata before asking Git for its authoritative
//! NUL-framed listing. The exclusion list is deliberately conservative:
//! bare, prunable, locked, detached-without-branch, and thegn's internal gate
//! worktrees never become sidebar rows. Discovery never opens a pane, runs a
//! hook, provisions an environment, checks out a branch, or changes the
//! active view. Existing rows retain their tab identity, order, folder,
//! hidden/closed state, and other cached metadata.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::SystemTime;

use anyhow::{Context, Result};
use thegn_core::models::WorktreeRow;
use thegn_core::store::WorkspaceStore;
use thegn_core::util::WorktreeEntry;

const FINGERPRINT_ENTRY_CAP: usize = 4096;
const DISCOVERY_RETRY_CAP: u8 = 3;

#[derive(Clone, Debug, Eq, PartialEq)]
struct MetadataFingerprint {
    modified: SystemTime,
    len: u64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct EntryFingerprint {
    id: PathBuf,
    dir: Option<MetadataFingerprint>,
    gitdir: Option<MetadataFingerprint>,
    head: Option<MetadataFingerprint>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct Fingerprint {
    worktrees_dir: Option<MetadataFingerprint>,
    entries: Vec<EntryFingerprint>,
    truncated: bool,
}

fn fingerprint_cache() -> &'static Mutex<HashMap<PathBuf, Fingerprint>> {
    static CACHE: OnceLock<Mutex<HashMap<PathBuf, Fingerprint>>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

fn failure_cache() -> &'static Mutex<HashMap<PathBuf, (Fingerprint, u8)>> {
    static CACHE: OnceLock<Mutex<HashMap<PathBuf, (Fingerprint, u8)>>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

fn metadata(path: &Path) -> Result<Option<MetadataFingerprint>> {
    match std::fs::metadata(path) {
        Ok(metadata) => Ok(Some(MetadataFingerprint {
            modified: metadata
                .modified()
                .with_context(|| format!("read mtime for {}", path.display()))?,
            len: metadata.len(),
        })),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error).with_context(|| format!("stat {}", path.display())),
    }
}

fn fingerprint(common_dir: &Path) -> Result<Fingerprint> {
    let worktrees_dir = common_dir.join("worktrees");
    let directory = metadata(&worktrees_dir)?;
    let mut ids = Vec::new();
    let mut truncated = false;
    match std::fs::read_dir(&worktrees_dir) {
        Ok(entries) => {
            for entry in entries.take(FINGERPRINT_ENTRY_CAP + 1) {
                ids.push(
                    entry
                        .with_context(|| format!("read {}", worktrees_dir.display()))?
                        .path(),
                );
            }
            truncated = ids.len() > FINGERPRINT_ENTRY_CAP;
            ids.truncate(FINGERPRINT_ENTRY_CAP);
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => {
            return Err(error).with_context(|| format!("read_dir {}", worktrees_dir.display()));
        }
    }
    ids.sort();

    let entries = ids
        .into_iter()
        .map(|id| {
            Ok(EntryFingerprint {
                dir: metadata(&id)?,
                gitdir: metadata(&id.join("gitdir"))?,
                head: metadata(&id.join("HEAD"))?,
                id,
            })
        })
        .collect::<Result<Vec<_>>>()?;
    Ok(Fingerprint {
        worktrees_dir: directory,
        entries,
        truncated,
    })
}

fn registered_roots(db: &thegn_core::db::Db) -> Result<Vec<PathBuf>> {
    Ok(db
        .workspaces()
        .context("read registered projects")?
        .into_iter()
        .filter(|workspace| workspace.kind == "repo")
        .map(|workspace| PathBuf::from(workspace.repo_path))
        .filter(|root| root.is_dir())
        .collect())
}

fn is_internal_gate(path: &Path) -> bool {
    path.components().any(|component| {
        let value = component.as_os_str().to_string_lossy();
        value == "thegn-gate" || value.starts_with("thegn-gate-")
    })
}

fn excluded(root: &Path, entry: &WorktreeEntry) -> bool {
    let path = Path::new(&entry.path);
    entry.bare
        || entry.prunable
        || entry.locked
        || entry.detached
        || entry.branch.is_none()
        || entry.branch.as_deref().is_some_and(str::is_empty)
        || same_path(root, path)
        || is_internal_gate(path)
}

fn same_path(left: &Path, right: &Path) -> bool {
    left == right
        || std::fs::canonicalize(left)
            .ok()
            .zip(std::fs::canonicalize(right).ok())
            .is_some_and(|(left, right)| left == right)
}

fn fingerprint_changed(
    cache: &HashMap<PathBuf, Fingerprint>,
    root: &Path,
    current: &Fingerprint,
) -> bool {
    cache.get(root) != Some(current)
}

fn add_missing(
    db: &thegn_core::db::Db,
    root: &Path,
    known: &HashSet<String>,
    entries: Vec<WorktreeEntry>,
) -> Result<usize> {
    let root_string = root.to_string_lossy().into_owned();
    let known_canonical: HashSet<PathBuf> = known
        .iter()
        .filter_map(|path| std::fs::canonicalize(path).ok())
        .collect();
    db.transaction(|db| {
        let mut added = 0;
        for entry in entries {
            if excluded(root, &entry)
                || known.contains(&entry.path)
                || std::fs::canonicalize(&entry.path)
                    .is_ok_and(|path| known_canonical.contains(&path))
            {
                continue;
            }
            if db.worktree_record(&entry.path)?.is_some() {
                continue;
            }
            let path = Path::new(&entry.path);
            let keys = match thegn_core::worktree_suppress::keys_for(path) {
                Ok(keys) => keys,
                Err(error) => {
                    tracing::debug!(
                        target: "thegn::worktree",
                        path = %path.display(),
                        %error,
                        "skipping discovered worktree with an unusable identity"
                    );
                    continue;
                }
            };
            if thegn_core::worktree_suppress::is_suppressed(db, &keys)? {
                continue;
            }
            let branch = entry.branch.as_deref().unwrap_or_default();
            let slug = match thegn_core::repo::repo_slug_with_checked(db, root) {
                Ok(slug) => slug,
                Err(error) => {
                    tracing::debug!(
                        target: "thegn::worktree",
                        root = %root.display(),
                        %error,
                        "skipping discovered worktree with an unavailable sidebar slug"
                    );
                    continue;
                }
            };
            let tab = thegn_core::repo::branch_tab(&slug, branch);
            db.put_worktree(&tab, &root_string, &entry.path, branch, None, None)?;
            added += 1;
        }
        Ok(added)
    })
}

fn record_failure(
    cache: &mut HashMap<PathBuf, Fingerprint>,
    failures: &mut HashMap<PathBuf, (Fingerprint, u8)>,
    root: &Path,
    current: &Fingerprint,
) {
    let count = match failures.get_mut(root) {
        Some((fingerprint, count)) if fingerprint == current => {
            *count = count.saturating_add(1);
            *count
        }
        _ => {
            failures.insert(root.to_path_buf(), (current.clone(), 1));
            1
        }
    };
    if count == DISCOVERY_RETRY_CAP {
        cache.insert(root.to_path_buf(), current.clone());
        tracing::warn!(
            target: "thegn::worktree",
            root = %root.display(),
            "worktree discovery gave up for this metadata state; retries on the next change"
        );
    }
}

fn discover_root<F>(
    db: &thegn_core::db::Db,
    root: &Path,
    known: &HashSet<String>,
    cache: &mut HashMap<PathBuf, Fingerprint>,
    failures: &mut HashMap<PathBuf, (Fingerprint, u8)>,
    list: &F,
) where
    F: Fn(&Path) -> Option<Vec<WorktreeEntry>>,
{
    let common_dir = thegn_core::util::git_common_dir(root);
    let Ok(current) = fingerprint(&common_dir) else {
        tracing::debug!(target: "thegn::worktree", root = %root.display(), "worktree fingerprint unavailable; discovery will retry");
        return;
    };
    if !fingerprint_changed(cache, root, &current) {
        return;
    }
    if current.truncated {
        tracing::warn!(
            target: "thegn::worktree",
            root = %root.display(),
            cap = FINGERPRINT_ENTRY_CAP,
            "worktree fingerprint truncated after 4096 administrative entries"
        );
    }
    let Some(entries) = list(root) else {
        record_failure(cache, failures, root, &current);
        tracing::debug!(target: "thegn::worktree", root = %root.display(), "git worktree listing failed; discovery will retry");
        return;
    };
    let result = add_missing(db, root, known, entries);
    match result {
        Ok(added) => {
            cache.insert(root.to_path_buf(), current);
            failures.remove(root);
            if added > 0 {
                tracing::debug!(target: "thegn::worktree", root = %root.display(), added, "discovered external worktrees");
            }
        }
        Err(error) => {
            record_failure(cache, failures, root, &current);
            tracing::debug!(target: "thegn::worktree", root = %root.display(), %error, "worktree discovery transaction failed; discovery will retry");
        }
    }
}

fn discover_with<F>(
    db: &thegn_core::db::Db,
    cache: &mut HashMap<PathBuf, Fingerprint>,
    failures: &mut HashMap<PathBuf, (Fingerprint, u8)>,
    list: F,
) -> Result<()>
where
    F: Fn(&Path) -> Option<Vec<WorktreeEntry>>,
{
    let roots = registered_roots(db)?;
    let registered: HashSet<PathBuf> = roots.iter().cloned().collect();
    cache.retain(|root, _| registered.contains(root));
    failures.retain(|root, _| registered.contains(root));
    let known: HashSet<String> = db
        .worktrees()
        .context("read registered worktrees for discovery")?
        .into_iter()
        .map(|WorktreeRow { worktree, .. }| worktree)
        .collect();
    for root in roots {
        discover_root(db, &root, &known, cache, failures, &list);
    }
    Ok(())
}

/// Run add-only discovery for all registered local projects. Discovery is a
/// cache-side enhancement: read and transaction failures are retried for a
/// bounded number of hydrations, then cached for this metadata state.
pub(crate) fn discover_registered(db: &thegn_core::db::Db) {
    let cache = fingerprint_cache();
    let mut cache = cache
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let failures = failure_cache();
    let mut failures = failures
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if let Err(error) = discover_with(
        db,
        &mut cache,
        &mut failures,
        thegn_core::util::worktree_entries,
    ) {
        tracing::debug!(target: "thegn::worktree", %error, "registered worktree discovery unavailable; discovery will retry");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use thegn_core::store::WorkspaceStore;

    fn fixture() -> (tempfile::TempDir, PathBuf, PathBuf) {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("repo");
        let worktree = temp.path().join("feature");
        let admin = root.join(".git/worktrees/feature");
        std::fs::create_dir_all(&admin).unwrap();
        std::fs::create_dir_all(&worktree).unwrap();
        std::fs::write(
            admin.join("gitdir"),
            format!("{}\n", worktree.join(".git").display()),
        )
        .unwrap();
        std::fs::write(admin.join("HEAD"), "abc\n").unwrap();
        (temp, root, worktree)
    }

    fn db_for(state: &thegn_core::testenv::IsolatedState, root: &Path) -> thegn_core::db::Db {
        let db = thegn_core::db::Db::open_at(&state.root().join("state.db")).unwrap();
        db.put_workspace(&root.to_string_lossy(), "r", "repo")
            .unwrap();
        db
    }

    fn entry(path: &Path, branch: Option<&str>) -> WorktreeEntry {
        WorktreeEntry {
            path: path.to_string_lossy().into_owned(),
            branch: branch.map(str::to_owned),
            bare: false,
            locked: false,
            prunable: false,
            detached: false,
        }
    }

    #[test]
    fn unchanged_fingerprint_runs_lister_zero_times() {
        let _state = thegn_core::testenv::IsolatedState::new();
        let (_temp, root, worktree) = fixture();
        let db = db_for(&_state, &root);
        let mut cache = HashMap::new();
        let mut failures = HashMap::new();
        let calls = std::cell::Cell::new(0);
        let list = |_: &Path| {
            calls.set(calls.get() + 1);
            Some(vec![entry(&worktree, Some("feature"))])
        };
        discover_with(&db, &mut cache, &mut failures, list).unwrap();
        assert_eq!(calls.get(), 1);
        assert_eq!(db.worktrees().unwrap().len(), 1);

        discover_with(&db, &mut cache, &mut failures, |_| {
            calls.set(calls.get() + 1);
            Some(vec![entry(&worktree, Some("feature"))])
        })
        .unwrap();
        assert_eq!(calls.get(), 1);
        assert_eq!(db.worktrees().unwrap().len(), 1);
    }

    #[test]
    fn failed_listing_retries_then_is_capped_for_unchanged_metadata() {
        let _state = thegn_core::testenv::IsolatedState::new();
        let (_temp, root, _worktree) = fixture();
        let db = db_for(&_state, &root);
        let mut cache = HashMap::new();
        let mut failures = HashMap::new();
        let calls = std::cell::Cell::new(0);
        for _ in 0..4 {
            discover_with(&db, &mut cache, &mut failures, |_| {
                calls.set(calls.get() + 1);
                None
            })
            .unwrap();
        }
        assert_eq!(calls.get(), DISCOVERY_RETRY_CAP as u32);
        assert_eq!(cache.len(), 1);
    }

    #[test]
    fn suppressed_worktree_is_not_rediscovered() {
        let _state = thegn_core::testenv::IsolatedState::new();
        let (_temp, root, worktree) = fixture();
        let db = db_for(&_state, &root);
        let root_string = root.to_string_lossy().into_owned();
        let worktree_string = worktree.to_string_lossy().into_owned();
        db.put_worktree(
            "r/feature",
            &root_string,
            &worktree_string,
            "feature",
            None,
            None,
        )
        .unwrap();
        let keys = thegn_core::worktree_suppress::keys_for(&worktree).unwrap();
        thegn_core::worktree_suppress::close_with_suppression(
            &db,
            &worktree_string,
            &keys,
            &thegn_core::worktree_suppress::record_json(&worktree_string, "r/feature"),
        )
        .unwrap();

        let mut cache = HashMap::new();
        let mut failures = HashMap::new();
        discover_with(&db, &mut cache, &mut failures, |_| {
            Some(vec![entry(&worktree, Some("feature"))])
        })
        .unwrap();
        assert!(db.worktrees().unwrap().is_empty());
    }

    #[test]
    fn unusable_entry_identity_does_not_poison_the_fingerprint_cache() {
        let _state = thegn_core::testenv::IsolatedState::new();
        let (_temp, root, _worktree) = fixture();
        let db = db_for(&_state, &root);
        let unusable = std::env::temp_dir().join("x".repeat(17 * 1024));
        let mut cache = HashMap::new();
        let mut failures = HashMap::new();
        let calls = std::cell::Cell::new(0);
        let list = |_: &Path| {
            calls.set(calls.get() + 1);
            Some(vec![entry(&unusable, Some("unusable"))])
        };
        discover_with(&db, &mut cache, &mut failures, list).unwrap();
        discover_with(&db, &mut cache, &mut failures, |_| {
            calls.set(calls.get() + 1);
            Some(vec![entry(&unusable, Some("unusable"))])
        })
        .unwrap();
        assert_eq!(calls.get(), 1);
        assert!(db.worktrees().unwrap().is_empty());
    }

    #[test]
    fn excluded_listing_entries_are_not_added() {
        let _state = thegn_core::testenv::IsolatedState::new();
        let (_temp, root, worktree) = fixture();
        let db = db_for(&_state, &root);
        let paths = [
            root.join("bare"),
            root.join("locked"),
            root.join("prunable"),
            root.join("detached"),
            root.join("branchless"),
            root.join("thegn-gate-1"),
        ];
        for path in &paths {
            std::fs::create_dir_all(path).unwrap();
        }
        let mut root_entry = entry(&root, Some("main"));
        root_entry.bare = false;
        let mut bare = entry(&paths[0], Some("bare"));
        bare.bare = true;
        let mut locked = entry(&paths[1], Some("locked"));
        locked.locked = true;
        let mut prunable = entry(&paths[2], Some("prunable"));
        prunable.prunable = true;
        let mut detached = entry(&paths[3], None);
        detached.detached = true;
        let branchless = entry(&paths[4], None);
        let gate = entry(&paths[5], Some("gate"));
        let entries = vec![
            root_entry, bare, locked, prunable, detached, branchless, gate,
        ];
        let mut cache = HashMap::new();
        let mut failures = HashMap::new();
        discover_with(&db, &mut cache, &mut failures, |_| Some(entries.clone())).unwrap();
        assert!(db.worktrees().unwrap().is_empty());
        assert!(worktree.exists());
    }

    #[test]
    fn canonical_path_spelling_does_not_duplicate_a_row() {
        let _state = thegn_core::testenv::IsolatedState::new();
        let (_temp, root, worktree) = fixture();
        let db = db_for(&_state, &root);
        let root_string = root.to_string_lossy().into_owned();
        let worktree_string = worktree.to_string_lossy().into_owned();
        db.put_worktree(
            "r/feature",
            &root_string,
            &worktree_string,
            "feature",
            None,
            None,
        )
        .unwrap();
        let listed_path = worktree.join(".");
        let mut cache = HashMap::new();
        let mut failures = HashMap::new();
        discover_with(&db, &mut cache, &mut failures, |_| {
            Some(vec![entry(&listed_path, Some("feature"))])
        })
        .unwrap();
        assert_eq!(db.worktrees().unwrap().len(), 1);
        assert_eq!(db.worktrees().unwrap()[0].worktree, worktree_string);
    }

    #[test]
    fn unlocking_an_admin_entry_changes_the_discovery_fingerprint() {
        let _state = thegn_core::testenv::IsolatedState::new();
        let (_temp, root, worktree) = fixture();
        let locked = root.join(".git/worktrees/feature/locked");
        std::fs::write(&locked, "operator\n").unwrap();
        let db = db_for(&_state, &root);
        let mut cache = HashMap::new();
        let mut failures = HashMap::new();
        let calls = std::cell::Cell::new(0);
        let list = |_: &Path| {
            calls.set(calls.get() + 1);
            Some(vec![entry(&worktree, Some("feature"))])
        };
        discover_with(&db, &mut cache, &mut failures, list).unwrap();
        std::fs::remove_file(locked).unwrap();
        discover_with(&db, &mut cache, &mut failures, |_| {
            calls.set(calls.get() + 1);
            Some(vec![entry(&worktree, Some("feature"))])
        })
        .unwrap();
        assert_eq!(calls.get(), 2);
    }

    #[test]
    fn fingerprint_truncates_4097_entries_with_diagnostic() {
        let temp = tempfile::tempdir().unwrap();
        let common = temp.path().join(".git");
        let worktrees = common.join("worktrees");
        std::fs::create_dir_all(&worktrees).unwrap();
        for index in 0..=FINGERPRINT_ENTRY_CAP {
            std::fs::create_dir(worktrees.join(format!("{index:04}"))).unwrap();
        }
        let current = fingerprint(&common).unwrap();
        assert!(current.truncated);
        assert_eq!(current.entries.len(), FINGERPRINT_ENTRY_CAP);
    }
}
