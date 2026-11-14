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

#[derive(Clone, Debug, Eq, PartialEq)]
struct MetadataFingerprint {
    modified: SystemTime,
    len: u64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct EntryFingerprint {
    id: PathBuf,
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
    db.transaction(|db| {
        let mut added = 0;
        for entry in entries {
            if excluded(root, &entry) || known.contains(&entry.path) {
                continue;
            }
            if db.worktree_record(&entry.path)?.is_some() {
                continue;
            }
            let path = Path::new(&entry.path);
            let keys = thegn_core::worktree_suppress::keys_for(path)
                .with_context(|| format!("identify discovered worktree {}", path.display()))?;
            if thegn_core::worktree_suppress::is_suppressed(db, &keys)? {
                continue;
            }
            let branch = entry.branch.as_deref().unwrap_or_default();
            let slug = thegn_core::repo::repo_slug_with_checked(db, root)
                .with_context(|| format!("resolve sidebar slug for {root_string}"))?;
            let tab = thegn_core::repo::branch_tab(&slug, branch);
            db.put_worktree(&tab, &root_string, &entry.path, branch, None, None)?;
            added += 1;
        }
        Ok(added)
    })
}

fn discover_root<F>(
    db: &thegn_core::db::Db,
    root: &Path,
    known: &HashSet<String>,
    cache: &mut HashMap<PathBuf, Fingerprint>,
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
        tracing::debug!(target: "thegn::worktree", root = %root.display(), "git worktree listing failed; discovery will retry");
        return;
    };
    let result = add_missing(db, root, known, entries);
    match result {
        Ok(added) => {
            cache.insert(root.to_path_buf(), current);
            if added > 0 {
                tracing::debug!(target: "thegn::worktree", root = %root.display(), added, "discovered external worktrees");
            }
        }
        Err(error) => {
            tracing::debug!(target: "thegn::worktree", root = %root.display(), %error, "worktree discovery transaction failed; discovery will retry");
        }
    }
}

fn discover_with<F>(
    db: &thegn_core::db::Db,
    cache: &mut HashMap<PathBuf, Fingerprint>,
    list: F,
) -> Result<()>
where
    F: Fn(&Path) -> Option<Vec<WorktreeEntry>>,
{
    let roots = registered_roots(db)?;
    let registered: HashSet<PathBuf> = roots.iter().cloned().collect();
    cache.retain(|root, _| registered.contains(root));
    let known: HashSet<String> = db
        .worktrees()
        .context("read registered worktrees for discovery")?
        .into_iter()
        .map(|WorktreeRow { worktree, .. }| worktree)
        .collect();
    for root in roots {
        discover_root(db, &root, &known, cache, &list);
    }
    Ok(())
}

/// Run add-only discovery for all registered local projects. Discovery is a
/// cache-side enhancement: any read or transaction failure is logged and left
/// uncached so the next hydration retries it.
pub(crate) fn discover_registered(db: &thegn_core::db::Db) {
    let cache = fingerprint_cache();
    let mut cache = cache
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if let Err(error) = discover_with(db, &mut cache, thegn_core::util::worktree_entries) {
        tracing::debug!(target: "thegn::worktree", %error, "registered worktree discovery unavailable; discovery will retry");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unchanged_fingerprint_runs_lister_zero_times() {
        let temp = tempfile::tempdir().unwrap();
        let common = temp.path().join(".git");
        std::fs::create_dir_all(common.join("worktrees/one")).unwrap();
        std::fs::write(common.join("worktrees/one/gitdir"), "/tmp/one/.git\n").unwrap();
        std::fs::write(common.join("worktrees/one/HEAD"), "abc\n").unwrap();
        let current = fingerprint(&common).unwrap();
        let cache = HashMap::from([(temp.path().to_path_buf(), current.clone())]);
        let calls = std::cell::Cell::new(0);
        let lister = || calls.set(calls.get() + 1);
        if fingerprint_changed(&cache, temp.path(), &current) {
            lister();
        }
        assert_eq!(calls.get(), 0);
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
