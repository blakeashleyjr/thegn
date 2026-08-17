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
use std::time::Instant;

use crate::hydrate::GlyphRow;
use crate::sidebar::GitGlyphs;

/// Map a cached `GlyphRow` `(dirty, ahead, behind, branch, repo_root, add, del,
/// branch_diff)` to the renderable `GitGlyphs`.
pub(crate) fn glyphs_from_row(row: &GlyphRow) -> GitGlyphs {
    GitGlyphs {
        dirty: row.0,
        ahead: row.1,
        behind: row.2,
        add: row.5,
        del: row.6,
        branch_diff: row.7,
    }
}

/// Overlay last-known-good glyphs onto `git` (and the cached HEAD branch onto
/// `branches`) for every path in `paths` that has a cached row and is not
/// already present. Never overwrites a row that a fresh scan already populated
/// (path already in `git`); a path with no cache entry is left absent (renders
/// blank, same as a never-scanned worktree).
pub(crate) fn seed_glyphs_from_cache(
    git: &mut BTreeMap<String, GitGlyphs>,
    branches: &mut BTreeMap<String, String>,
    paths: impl IntoIterator<Item = String>,
    cache: &HashMap<String, (GlyphRow, Instant)>,
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
            if let Some(branch) = &row.3 {
                branches.entry(p.clone()).or_insert_with(|| branch.clone());
            }
            git.insert(p, glyphs_from_row(row));
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
    let cache = crate::hydrate::glyph_cache().lock().unwrap();
    seed_glyphs_from_cache(git, branches, paths, &cache);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(dirty: bool, ahead: usize, behind: usize) -> (GlyphRow, Instant) {
        (
            (dirty, ahead, behind, None, String::new(), 0, 0, None),
            Instant::now(),
        )
    }

    #[test]
    fn glyphs_from_row_maps_fields() {
        let (r, _) = row(true, 3, 2);
        assert_eq!(
            glyphs_from_row(&r),
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
        let r: GlyphRow = (true, 0, 0, None, String::new(), 42, 7, Some((310, 84)));
        assert_eq!(
            glyphs_from_row(&r),
            GitGlyphs {
                dirty: true,
                add: 42,
                del: 7,
                branch_diff: Some((310, 84)),
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
        seed_glyphs_from_cache(&mut git, &mut branches, ["/a".to_string()], &cache);
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
                (
                    false,
                    0,
                    0,
                    Some("tg/live".to_string()),
                    String::new(),
                    0,
                    0,
                    None,
                ),
                Instant::now(),
            ),
        );
        let mut git = BTreeMap::new();
        let mut branches = BTreeMap::new();
        seed_glyphs_from_cache(&mut git, &mut branches, ["/a".to_string()], &cache);
        assert_eq!(branches.get("/a").map(String::as_str), Some("tg/live"));

        // A fresher branch already in the map (from this pass's live scan) wins.
        let mut branches = BTreeMap::new();
        branches.insert("/a".to_string(), "tg/fresh".to_string());
        let mut git = BTreeMap::new();
        seed_glyphs_from_cache(&mut git, &mut branches, ["/a".to_string()], &cache);
        assert_eq!(branches.get("/a").map(String::as_str), Some("tg/fresh"));
    }

    #[test]
    fn leaves_uncached_paths_absent() {
        let cache = HashMap::new();
        let mut git = BTreeMap::new();
        let mut branches = BTreeMap::new();
        seed_glyphs_from_cache(&mut git, &mut branches, ["/nope".to_string()], &cache);
        assert!(git.is_empty(), "no cache entry -> no glyph inserted");
    }
}
