//! The panel's "Layout keys" hint block: one dim `chord  label` row per
//! rearranging action (split, swap, resize, zoom, lock, tab and pane moves),
//! read from the live keymap through [`FrameModel::chord_hints`] so a user
//! rebind shows correctly. Pure — `chrome::draw_panel` paints the rows in the
//! blank tail of the panel body, above the pinned Lead footer, and never over
//! panel content, so it needs no hit-table entry.

use crate::chrome::FrameModel;

/// `(action id, short label)` in priority order: when the panel is short the
/// tail rows are dropped first. Every id here must be in
/// `chrome::INLINE_CHORD_IDS` (a test pins that).
pub(crate) const ROWS: &[(&str, &str)] = &[
    ("split-right", "split"),
    ("swap-pane-left", "swap pane (+h/j/k/l)"),
    ("resize-left", "resize pane (+arrows)"),
    ("zoom", "zoom pane"),
    ("toggle-key-lock", "lock keys"),
    ("move-tab-left", "move tab left"),
    ("move-tab-right", "move tab right"),
    ("break-pane-to-tab", "pane to own tab"),
    ("move-pane-to-prev-tab", "pane to prev tab"),
    ("move-pane-to-next-tab", "pane to next tab"),
];

/// The block's title row.
pub(crate) const TITLE: &str = " LAYOUT KEYS ";

pub(crate) fn clip(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let keep = max.saturating_sub(1);
    let mut out: String = s.chars().take(keep).collect();
    if max > 0 {
        out.push('…');
    }
    out
}

/// The `(chord, label)` rows that fit `cols` columns and `max_rows` rows
/// (title row included). Empty when the block is disabled, the panel is too
/// narrow, or fewer than two rows are available. Actions with no live chord
/// are skipped; overlong chord/label text is truncated with `…`.
pub(crate) fn rows(model: &FrameModel, cols: usize, max_rows: usize) -> Vec<(String, String)> {
    if model.layout_hints_off || cols < 16 || max_rows < 2 {
        return Vec::new();
    }
    let chords: Vec<(String, &str)> = ROWS
        .iter()
        .filter_map(|(id, label)| model.chord_hints.get(*id).map(|c| (c.clone(), *label)))
        .take(max_rows - 1)
        .collect();
    // 1 col left margin, chord, 1 col gap, label.
    let inner = cols.saturating_sub(1);
    let chord_w = chords
        .iter()
        .map(|(c, _)| c.chars().count())
        .max()
        .unwrap_or(0)
        .min(inner / 2);
    chords
        .into_iter()
        .map(|(c, l)| {
            let label_w = inner.saturating_sub(chord_w + 1);
            (clip(&c, chord_w), clip(l, label_w))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn model_with_chords() -> FrameModel {
        let mut m = FrameModel::default();
        for (id, _) in ROWS {
            m.chord_hints.insert((*id).to_string(), format!("K-{id}"));
        }
        m
    }

    #[test]
    fn every_row_id_is_resolved_into_chord_hints() {
        for (id, _) in ROWS {
            assert!(
                crate::chrome::INLINE_CHORD_IDS.contains(id),
                "`{id}` missing from INLINE_CHORD_IDS"
            );
        }
    }

    #[test]
    fn fits_and_drops_lowest_priority_rows_first() {
        let m = model_with_chords();
        let all = rows(&m, 60, 20);
        assert_eq!(all.len(), ROWS.len());
        let few = rows(&m, 60, 4);
        assert_eq!(few.len(), 3);
        assert_eq!(few[0].1, "split");
        assert!(rows(&m, 60, 1).is_empty());
    }

    #[test]
    fn reflects_a_rebind_and_skips_unbound_actions() {
        let mut m = model_with_chords();
        m.chord_hints.insert("zoom".into(), "Ctrl-z".into());
        m.chord_hints.remove("split-right");
        let r = rows(&m, 60, 20);
        assert!(r.iter().any(|(c, l)| c == "Ctrl-z" && l == "zoom pane"));
        assert!(!r.iter().any(|(_, l)| l == "split"));
    }

    #[test]
    fn truncates_on_narrow_panels_and_hides_when_off_or_tiny() {
        let m = model_with_chords();
        for (c, l) in rows(&m, 24, 20) {
            assert!(
                1 + c.chars().count() + 1 + l.chars().count() <= 24,
                "{c} {l}"
            );
        }
        assert!(rows(&m, 10, 20).is_empty());
        let mut off = model_with_chords();
        off.layout_hints_off = true;
        assert!(rows(&off, 60, 20).is_empty());
    }
}
