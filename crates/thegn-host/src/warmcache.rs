//! Launch warm-start for the sidebar-glyph cache.
//!
//! Seeds the in-memory `crate::hydrate::glyph_cache` from the DB (persisted by
//! the previous session) so the sidebar paints last-known git glyphs instantly
//! on launch instead of blank-then-scan. Invoked lazily from that cache's
//! `OnceLock` init — off the event loop, on the first hydration — so it costs
//! nothing until the first sidebar build and needs no startup wiring.

use std::collections::HashMap;
use std::time::Instant;

use crate::glyph_types::GlyphRow;
use crate::hydrate_tuning::{glyph_ttls, warm_stamp};

/// Load every persisted glyph row into a fresh cache map. Best-effort: a DB miss
/// or an unparseable row is skipped. Rows receive deterministic staggered
/// stamps, so warm-cache validation does not create a lockstep scan burst.
pub(crate) fn load_glyphs() -> HashMap<String, (GlyphRow, Instant)> {
    let mut m = HashMap::new();
    let Ok(db) = thegn_core::db::Db::open() else {
        return m;
    };
    let now = Instant::now();
    let ttls = glyph_ttls();
    for (path, json) in db.all_glyph_cache() {
        if let Ok(row) = serde_json::from_str::<GlyphRow>(&json) {
            let stamp = warm_stamp(now, std::path::Path::new(&path), &ttls);
            m.insert(path, (row, stamp));
        }
    }
    m
}
