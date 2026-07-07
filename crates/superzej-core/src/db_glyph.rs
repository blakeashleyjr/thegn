//! Sidebar-glyph warm-start cache.
//!
//! Persists each worktree's last-known git glyph row (dirty / ahead / behind /
//! branch / repo-root, serialized) so a fresh launch paints last session's
//! sidebar instantly instead of blank-then-scan — stale-while-revalidate: the
//! next hydration refreshes it. Best-effort (git is the source of truth); a miss
//! just re-scans. Kept out of the pinned `db.rs` schema via a lazy
//! `CREATE TABLE IF NOT EXISTS` so the cache can be added without touching the
//! central DDL.

use anyhow::Result;
use rusqlite::params;

use crate::db::Db;
use crate::util;

impl Db {
    fn ensure_glyph_cache_table(&self) -> Result<()> {
        self.conn().execute_batch(
            "CREATE TABLE IF NOT EXISTS glyph_cache (
                 worktree   TEXT PRIMARY KEY,
                 row_json   TEXT NOT NULL,
                 fetched_at INTEGER NOT NULL
             )",
        )?;
        Ok(())
    }

    /// Upsert a worktree's serialized glyph row.
    pub fn put_glyph_cache(&self, worktree: &str, row_json: &str) -> Result<()> {
        self.ensure_glyph_cache_table()?;
        self.conn().execute(
            r#"INSERT INTO glyph_cache(worktree,row_json,fetched_at)
               VALUES(?1,?2,?3)
               ON CONFLICT(worktree) DO UPDATE SET row_json=?2, fetched_at=?3"#,
            params![worktree, row_json, util::now()],
        )?;
        Ok(())
    }

    /// Every cached `(worktree, row_json)` — the launch warm-start seed. Empty on
    /// a first-ever run (no table yet) or any read error.
    pub fn all_glyph_cache(&self) -> Vec<(String, String)> {
        if self.ensure_glyph_cache_table().is_err() {
            return Vec::new();
        }
        let Ok(mut stmt) = self
            .conn()
            .prepare("SELECT worktree, row_json FROM glyph_cache")
        else {
            return Vec::new();
        };
        let Ok(rows) = stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))
        else {
            return Vec::new();
        };
        rows.filter_map(std::result::Result::ok).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn put_then_load_roundtrips_and_upserts() {
        let db = Db::open_memory().unwrap();
        assert!(db.all_glyph_cache().is_empty());
        db.put_glyph_cache("/wt/a", "[true,1,0,\"main\",\"/repo\"]")
            .unwrap();
        db.put_glyph_cache("/wt/b", "[false,0,0,null,\"/repo\"]")
            .unwrap();
        let all: std::collections::HashMap<_, _> = db.all_glyph_cache().into_iter().collect();
        assert_eq!(all.len(), 2);
        assert_eq!(all["/wt/a"], "[true,1,0,\"main\",\"/repo\"]");
        // Upsert replaces the row, not appends.
        db.put_glyph_cache("/wt/a", "[false,0,0,null,\"/repo\"]")
            .unwrap();
        assert_eq!(db.all_glyph_cache().len(), 2);
    }
}
