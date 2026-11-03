//! Pure helpers for serving last-known-good sidebar git glyphs from the
//! process-global glyph cache.
//!
//! The sidebar's dirty-dot + ahead/behind arrows are only *scanned* fresh for
//! the active worktree (see `hydrate::should_rescan_glyphs`); every other row is
//! served from the persistent, path-keyed `hydrate::glyph_cache`. These helpers
//! overlay those cached rows onto a `SidebarStatus` so glyphs persist instantly
//! across a workspace switch (before the async hydration lands) and so
//! non-session worktrees still render their last-known state. Kept pure and
//! unit-tested; the cache lock is taken by the thin `hydrate` wrapper.

use std::collections::{BTreeMap, HashMap};
use std::sync::{Mutex, OnceLock};
use std::time::Instant;

use crate::glyph_types::GlyphRow;
use crate::sidebar::GitGlyphs;

fn jj_memo() -> &'static Mutex<HashMap<String, bool>> {
    static MEMO: OnceLock<Mutex<HashMap<String, bool>>> = OnceLock::new();
    MEMO.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Record the jj state observed during the off-loop glyph scan. The following
/// cache refresh re-confirms roots present in the glyph cache.
pub(crate) fn record_jj(repo_root: &str, jj: bool) {
    jj_memo()
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .insert(repo_root.to_string(), jj);
}

/// Stat each distinct, non-empty repo root. Callers keep this off the event
/// loop.
fn jj_for_roots(roots: impl IntoIterator<Item = String>) -> HashMap<String, bool> {
    roots
        .into_iter()
        .filter(|root| !root.is_empty())
        .collect::<std::collections::BTreeSet<_>>()
        .into_iter()
        .map(|root| {
            let jj = thegn_core::jj::is_colocated(std::path::Path::new(&root));
            (root, jj)
        })
        .collect()
}

/// Refresh memoized jj state for all roots currently in the glyph cache.
///
/// This stats the filesystem; call from the hydration thread, never the event
/// loop.
pub(crate) fn refresh_cached_jj() {
    let roots = {
        let cache = crate::hydrate::glyph_cache()
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        cache
            .values()
            .map(|(row, _)| row.repo_root.clone())
            .collect::<Vec<_>>()
    };
    let fresh = jj_for_roots(roots);
    let mut memo = jj_memo().lock().unwrap_or_else(|error| error.into_inner());
    memo.retain(|root, _| fresh.contains_key(root));
    memo.extend(fresh);
}

/// Map a cached named glyph row to the renderable `GitGlyphs`.
pub(crate) fn glyphs_from_row(row: &GlyphRow, jj: bool) -> GitGlyphs {
    GitGlyphs {
        dirty: row.dirty,
        ahead: row.ahead,
        behind: row.behind,
        add: row.add,
        del: row.del,
        branch_diff: row.branch_diff,
        submodule_dirty: row.submodule_dirty,
        // Read from the off-loop memo without widening the persisted GlyphRow.
        jj,
    }
}

/// Overlay last-known-good glyphs onto `git` (and the cached HEAD branch onto
/// `branches`) for every path in `paths` that has a cached row and is not
/// already present. Never overwrites a row that a fresh scan already populated
/// (path already in `git`); a path with no cache entry is left absent (renders
/// blank, same as a never-scanned worktree). On a cold start, a cache-restored
/// row with unknown jj state renders no marker until hydration refreshes it.
pub(crate) fn seed_glyphs_from_cache(
    git: &mut BTreeMap<String, GitGlyphs>,
    branches: &mut BTreeMap<String, String>,
    paths: impl IntoIterator<Item = String>,
    cache: &HashMap<String, (GlyphRow, Instant)>,
    jj: &HashMap<String, bool>,
) {
    for p in paths {
        if git.contains_key(&p) {
            continue;
        }
        if let Some((row, _)) = cache.get(&p) {
            // The cached row carries the branch HEAD pointed at when it was
            // scanned — the right fallback for a row whose live scan is gated
            // (suspended sandbox, other workspace), and still fresher than the
            // creation-time tab name.
            if let Some(branch) = &row.branch {
                branches.entry(p.clone()).or_insert_with(|| branch.clone());
            }
            git.insert(
                p,
                glyphs_from_row(row, jj.get(&row.repo_root).copied().unwrap_or(false)),
            );
        }
    }
}

/// Overlay last-known-good glyphs for `paths` from the process-global glyph
/// cache, without scanning. In-memory only (a mutex lock, no git/DB/subprocess),
/// so it's safe to call on the event loop.
pub(crate) fn seed_from_global_cache(
    git: &mut BTreeMap<String, GitGlyphs>,
    branches: &mut BTreeMap<String, String>,
    paths: impl IntoIterator<Item = String>,
) {
    // The only dual-lock site: cache before memo. Neither lock is held during a
    // stat.
    let cache = crate::hydrate::glyph_cache()
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    let memo = jj_memo().lock().unwrap_or_else(|error| error.into_inner());
    seed_glyphs_from_cache(git, branches, paths, &cache, &memo);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(dirty: bool, ahead: usize, behind: usize) -> (GlyphRow, Instant) {
        (
            GlyphRow {
                dirty,
                ahead,
                behind,
                branch: None,
                repo_root: String::new(),
                add: thegn_svc::git::DiffCount::Exact(0),
                del: thegn_svc::git::DiffCount::Exact(0),
                branch_diff: None,
                submodule_dirty: false,
            },
            Instant::now(),
        )
    }

    #[test]
    fn glyphs_from_row_maps_fields() {
        let (r, _) = row(true, 3, 2);
        assert_eq!(
            glyphs_from_row(&r, false),
            GitGlyphs {
                dirty: true,
                ahead: 3,
                behind: 2,
                ..Default::default()
            }
        );
    }

    #[test]
    fn glyphs_from_row_maps_diff_stats() {
        let r = GlyphRow {
            dirty: true,
            ahead: 0,
            behind: 0,
            branch: None,
            repo_root: String::new(),
            add: thegn_svc::git::DiffCount::Exact(42),
            del: thegn_svc::git::DiffCount::Exact(7),
            branch_diff: Some((
                thegn_svc::git::DiffCount::Exact(310),
                thegn_svc::git::DiffCount::Exact(84),
            )),
            submodule_dirty: false,
        };
        assert_eq!(
            glyphs_from_row(&r, false),
            GitGlyphs {
                dirty: true,
                add: thegn_svc::git::DiffCount::Exact(42),
                del: thegn_svc::git::DiffCount::Exact(7),
                branch_diff: Some((
                    thegn_svc::git::DiffCount::Exact(310),
                    thegn_svc::git::DiffCount::Exact(84)
                )),
                ..Default::default()
            }
        );
    }

    #[test]
    fn seeds_missing_paths_from_cache() {
        let mut cache = HashMap::new();
        cache.insert("/a".to_string(), row(true, 1, 0));
        cache.insert("/b".to_string(), row(false, 0, 4));
        let mut git = BTreeMap::new();
        let mut branches = BTreeMap::new();
        seed_glyphs_from_cache(
            &mut git,
            &mut branches,
            ["/a".to_string(), "/b".to_string()],
            &cache,
            &HashMap::new(),
        );
        assert_eq!(
            git.get("/a"),
            Some(&GitGlyphs {
                dirty: true,
                ahead: 1,
                behind: 0,
                ..Default::default()
            })
        );
        assert_eq!(
            git.get("/b"),
            Some(&GitGlyphs {
                dirty: false,
                ahead: 0,
                behind: 4,
                ..Default::default()
            })
        );
    }

    #[test]
    fn does_not_overwrite_existing_scanned_rows() {
        let mut cache = HashMap::new();
        cache.insert("/a".to_string(), row(false, 0, 0)); // stale cache
        let mut git = BTreeMap::new();
        let fresh = GitGlyphs {
            dirty: true,
            ahead: 9,
            behind: 0,
            ..Default::default()
        };
        git.insert("/a".to_string(), fresh); // fresh scan already present
        let mut branches = BTreeMap::new();
        seed_glyphs_from_cache(
            &mut git,
            &mut branches,
            ["/a".to_string()],
            &cache,
            &HashMap::new(),
        );
        assert_eq!(git.get("/a"), Some(&fresh), "must not clobber a fresh scan");
    }

    #[test]
    fn seeds_the_cached_head_branch_alongside_the_glyphs() {
        // A row served from cache (other workspace / gated remote scan) still
        // needs a branch, or its sidebar label falls back to the creation-time
        // tab name and reads stale.
        let mut cache = HashMap::new();
        cache.insert(
            "/a".to_string(),
            (
                GlyphRow {
                    dirty: false,
                    ahead: 0,
                    behind: 0,
                    branch: Some("tg/live".to_string()),
                    repo_root: String::new(),
                    add: thegn_svc::git::DiffCount::Exact(0),
                    del: thegn_svc::git::DiffCount::Exact(0),
                    branch_diff: None,
                    submodule_dirty: false,
                },
                Instant::now(),
            ),
        );
        let mut git = BTreeMap::new();
        let mut branches = BTreeMap::new();
        seed_glyphs_from_cache(
            &mut git,
            &mut branches,
            ["/a".to_string()],
            &cache,
            &HashMap::new(),
        );
        assert_eq!(branches.get("/a").map(String::as_str), Some("tg/live"));

        // A fresher branch already in the map (from this pass's live scan) wins.
        let mut branches = BTreeMap::new();
        branches.insert("/a".to_string(), "tg/fresh".to_string());
        let mut git = BTreeMap::new();
        seed_glyphs_from_cache(
            &mut git,
            &mut branches,
            ["/a".to_string()],
            &cache,
            &HashMap::new(),
        );
        assert_eq!(branches.get("/a").map(String::as_str), Some("tg/fresh"));
    }

    #[test]
    fn leaves_uncached_paths_absent() {
        let cache = HashMap::new();
        let mut git = BTreeMap::new();
        let mut branches = BTreeMap::new();
        seed_glyphs_from_cache(
            &mut git,
            &mut branches,
            ["/nope".to_string()],
            &cache,
            &HashMap::new(),
        );
        assert!(git.is_empty(), "no cache entry -> no glyph inserted");
    }

    #[test]
    fn seed_reads_jj_from_the_memo_without_touching_the_filesystem() {
        let root = tempfile::tempdir_in("/tmp").unwrap();
        std::fs::create_dir(root.path().join(".jj")).unwrap();
        let root_path = root.path().display().to_string();
        let mut cached_row = row(false, 0, 0).0;
        cached_row.repo_root.clone_from(&root_path);
        let mut cache = HashMap::new();
        cache.insert("/worktree".to_string(), (cached_row, Instant::now()));

        let mut git = BTreeMap::new();
        let mut branches = BTreeMap::new();
        seed_glyphs_from_cache(
            &mut git,
            &mut branches,
            ["/worktree".to_string()],
            &cache,
            &HashMap::new(),
        );
        assert!(!git["/worktree"].jj, "unknown memo state must not stat .jj");

        let jj_memo = HashMap::from([(root_path, true)]);
        let mut git = BTreeMap::new();
        seed_glyphs_from_cache(
            &mut git,
            &mut branches,
            ["/worktree".to_string()],
            &cache,
            &jj_memo,
        );
        assert!(git["/worktree"].jj, "a recorded memo bit reaches the glyph");
    }

    #[test]
    fn jj_for_roots_detects_a_jj_directory_and_ignores_empty_roots() {
        let colocated = tempfile::tempdir_in("/tmp").unwrap();
        std::fs::create_dir(colocated.path().join(".jj")).unwrap();
        let plain = tempfile::tempdir_in("/tmp").unwrap();
        let file_jj = tempfile::tempdir_in("/tmp").unwrap();
        std::fs::write(file_jj.path().join(".jj"), "").unwrap();
        let colocated_root = colocated.path().display().to_string();
        let plain_root = plain.path().display().to_string();
        let file_root = file_jj.path().display().to_string();

        let found = jj_for_roots([
            colocated_root.clone(),
            plain_root.clone(),
            file_root.clone(),
            String::new(),
        ]);

        assert_eq!(found.get(&colocated_root), Some(&true));
        assert_eq!(found.get(&plain_root), Some(&false));
        assert_eq!(found.get(&file_root), Some(&false));
        assert!(!found.contains_key(""), "empty roots are omitted");
    }
}
