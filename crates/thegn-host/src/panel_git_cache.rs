//! Last-known `status` / `diff HEAD` / `stash list` reads for the watched
//! worktree, reused while its change print (see [`crate::diff_watch`]) has not
//! moved (THE-718).
//!
//! Before this, every 5 s model tick re-ran those three reads per hydration
//! (36 forks/min on an idle one-repo session) for an answer that only changes
//! when something under the worktree or its git dir changes. A snapshot is
//! stored only when a live watcher vouched for the path, only when every read
//! succeeded, and is returned only for the exact print it was taken at. A path
//! without a live watcher (background/warming worktrees, remote locs, failed or
//! incomplete registration) never gets a print, so it never touches the cache
//! and behaves exactly as before.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use thegn_core::semantic::EntitySummary;
use thegn_svc::git::{DiffEntry, FileStatus};

use crate::diff_watch::WatchPrint;

/// The three reads a snapshot stands in for.
#[derive(Clone, Debug, Default)]
pub(crate) struct GitReads {
    pub(crate) diff_entries: Vec<DiffEntry>,
    pub(crate) entities: Option<EntitySummary>,
    pub(crate) status: Vec<FileStatus>,
    pub(crate) stash_count: usize,
}

/// Snapshots are only ever stored for watched paths (one active worktree, a few
/// across rapid switching); the bound just keeps a pathological session honest.
const CAP: usize = 8;

fn cache() -> &'static Mutex<HashMap<PathBuf, (WatchPrint, GitReads)>> {
    static CACHE: std::sync::OnceLock<Mutex<HashMap<PathBuf, (WatchPrint, GitReads)>>> =
        std::sync::OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

/// The stored reads for `path` iff they were taken at exactly `print`.
pub(crate) fn get(path: &Path, print: WatchPrint) -> Option<GitReads> {
    cache()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(path)
        .filter(|(p, _)| *p == print)
        .map(|(_, r)| r.clone())
}

/// Store `reads`, taken at `print` (read BEFORE the reads ran).
pub(crate) fn put(path: &Path, print: WatchPrint, reads: GitReads) {
    let mut c = cache().lock().unwrap_or_else(|e| e.into_inner());
    if c.len() >= CAP && !c.contains_key(path) {
        c.clear();
    }
    c.insert(path.to_path_buf(), (print, reads));
}

#[cfg(test)]
mod tests {
    use super::*;

    fn reads(n: usize) -> GitReads {
        GitReads {
            stash_count: n,
            ..Default::default()
        }
    }

    #[test]
    fn a_snapshot_is_returned_only_for_the_exact_print() {
        let p = Path::new("/tmp/tg-panel-git-cache/a");
        let print = WatchPrint::for_test(1, 1);
        assert!(get(p, print).is_none());
        put(p, print, reads(3));
        assert_eq!(get(p, print).map(|r| r.stash_count), Some(3));
        assert!(
            get(p, WatchPrint::for_test(2, 1)).is_none(),
            "generation moved"
        );
        assert!(get(p, WatchPrint::for_test(1, 2)).is_none(), "in-app write");
        assert!(get(Path::new("/tmp/tg-panel-git-cache/b"), print).is_none());
        put(p, WatchPrint::for_test(2, 1), reads(4));
        assert!(get(p, print).is_none(), "overwritten by the newer print");
    }

    #[test]
    fn the_cache_is_bounded() {
        for i in 0..(CAP + 3) {
            put(
                Path::new(&format!("/tmp/tg-panel-git-cache/bound{i}")),
                WatchPrint::for_test(9, 9),
                reads(i),
            );
        }
        let n = cache().lock().unwrap().len();
        assert!(n <= CAP, "{n} entries");
    }
}
