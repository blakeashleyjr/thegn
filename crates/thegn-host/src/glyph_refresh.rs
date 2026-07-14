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

/// Map a cached `GlyphRow` `(dirty, ahead, behind, branch, repo_root)` to the
/// renderable `GitGlyphs`.
pub(crate) fn glyphs_from_row(row: &GlyphRow) -> GitGlyphs {
    GitGlyphs {
        dirty: row.0,
        ahead: row.1,
        behind: row.2,
    }
}

/// Overlay last-known-good glyphs onto `git` for every path in `paths` that has
/// a cached row and is not already present. Never overwrites a row that a fresh
/// scan already populated (path already in `git`); a path with no cache entry is
/// left absent (renders blank, same as a never-scanned worktree).
pub(crate) fn seed_glyphs_from_cache(
    git: &mut BTreeMap<String, GitGlyphs>,
    paths: impl IntoIterator<Item = String>,
    cache: &HashMap<String, (GlyphRow, Instant)>,
) {
    for p in paths {
        if git.contains_key(&p) {
            continue;
        }
        if let Some((row, _)) = cache.get(&p) {
            git.insert(p, glyphs_from_row(row));
        }
    }
}

/// Overlay last-known-good glyphs for `paths` from the process-global glyph
/// cache, without scanning. In-memory only (a mutex lock, no git/DB/subprocess),
/// so it's safe to call on the event loop.
pub(crate) fn seed_from_global_cache(
    git: &mut BTreeMap<String, GitGlyphs>,
    paths: impl IntoIterator<Item = String>,
) {
    let cache = crate::hydrate::glyph_cache().lock().unwrap();
    seed_glyphs_from_cache(git, paths, &cache);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(dirty: bool, ahead: usize, behind: usize) -> (GlyphRow, Instant) {
        ((dirty, ahead, behind, None, String::new()), Instant::now())
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
            }
        );
    }

    #[test]
    fn seeds_missing_paths_from_cache() {
        let mut cache = HashMap::new();
        cache.insert("/a".to_string(), row(true, 1, 0));
        cache.insert("/b".to_string(), row(false, 0, 4));
        let mut git = BTreeMap::new();
        seed_glyphs_from_cache(&mut git, ["/a".to_string(), "/b".to_string()], &cache);
        assert_eq!(
            git.get("/a"),
            Some(&GitGlyphs {
                dirty: true,
                ahead: 1,
                behind: 0
            })
        );
        assert_eq!(
            git.get("/b"),
            Some(&GitGlyphs {
                dirty: false,
                ahead: 0,
                behind: 4
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
        };
        git.insert("/a".to_string(), fresh); // fresh scan already present
        seed_glyphs_from_cache(&mut git, ["/a".to_string()], &cache);
        assert_eq!(git.get("/a"), Some(&fresh), "must not clobber a fresh scan");
    }

    #[test]
    fn leaves_uncached_paths_absent() {
        let cache = HashMap::new();
        let mut git = BTreeMap::new();
        seed_glyphs_from_cache(&mut git, ["/nope".to_string()], &cache);
        assert!(git.is_empty(), "no cache entry -> no glyph inserted");
    }
}
