//! Export a pane's time-travel replay ring as an asciicast `.cast` file — the
//! `export-cast` palette action and the replay overlay's `e` key. Both call
//! [`export_pane_cast`], which reuses the pure [`thegn_core::asciicast`] writer
//! (via [`crate::replay::Recording::export_cast`]) and writes owner-only under
//! the same per-profile recordings dir the daemon recorder uses. The export is
//! honestly a *tail* — only what the `[replay]` budget still holds — so the
//! returned message reports the covered span.

use std::io::BufWriter;
use std::time::{SystemTime, UNIX_EPOCH};

use thegn_core::config::Config;

use crate::panes::Panes;

/// Export the pane's retained recording, returning a user-facing status line
/// (path + covered span on success; a clear reason naming `[replay]` when there
/// is nothing to export).
pub(crate) fn export_pane_cast(panes: &Panes, pane_id: u32, cfg: &Config) -> String {
    let Some(rec) = panes.table.get(&pane_id).and_then(|p| p.recording()) else {
        return "Replay is disabled ([replay] enabled = false) — nothing to export".to_string();
    };
    if rec.is_empty() {
        return "Replay: nothing recorded for this pane yet".to_string();
    }
    let dir = cfg.recording.resolved_dir();
    if let Err(e) = std::fs::create_dir_all(&dir) {
        return format!("Export failed: could not create {}: {e}", dir.display());
    }
    crate::platform::restrict_dir_owner_only(&dir);
    let path = dir.join(format!("pane{pane_id}_{}.cast", now_ms()));
    let file = match crate::platform::create_private_file(&path) {
        Ok(f) => f,
        Err(e) => return format!("Export failed: {e}"),
    };
    match rec.export_cast(BufWriter::new(file)) {
        Ok(span_ms) => format!(
            "Exported {} of history → {}",
            fmt_span(span_ms),
            path.display()
        ),
        Err(e) => format!("Export failed: {e}"),
    }
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// Compact human duration for the export toast (e.g. `2m03s`, `12s`, `340ms`).
fn fmt_span(ms: u64) -> String {
    let s = ms / 1000;
    if s >= 60 {
        format!("{}m{:02}s", s / 60, s % 60)
    } else if s > 0 {
        format!("{s}s")
    } else {
        format!("{ms}ms")
    }
}

#[cfg(test)]
mod tests {
    use super::fmt_span;

    #[test]
    fn fmt_span_reads_naturally() {
        assert_eq!(fmt_span(340), "340ms");
        assert_eq!(fmt_span(12_000), "12s");
        assert_eq!(fmt_span(123_000), "2m03s");
    }
}
