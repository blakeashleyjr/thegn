//! Launch warm-start for the sidebar-glyph cache.
//!
//! Seeds the in-memory `crate::hydrate::glyph_cache` from the DB (persisted by
//! the previous session) so the sidebar paints last-known git glyphs instantly
//! on launch instead of blank-then-scan. Invoked lazily from that cache's
//! `OnceLock` init — off the event loop, on the first hydration — so it costs
//! nothing until the first sidebar build and needs no startup wiring.

use std::collections::HashMap;
use std::time::Instant;

use crate::hydrate::GlyphRow;

/// Load every persisted glyph row into a fresh cache map. Best-effort: a DB miss
/// or an unparseable row is skipped. Rows are stamped `now`, so they serve
/// immediately and revalidate at the normal TTL (stale-while-revalidate); the
/// active worktree always live-scans regardless.
pub(crate) fn load_glyphs() -> HashMap<String, (GlyphRow, Instant)> {
    let mut m = HashMap::new();
    let Ok(db) = superzej_core::db::Db::open() else {
        return m;
    };
    let now = Instant::now();
    for (path, json) in db.all_glyph_cache() {
        if let Ok(row) = serde_json::from_str::<GlyphRow>(&json) {
            m.insert(path, (row, now));
        }
    }
    m
}
