//! The panel's context-keys block: the keys for whatever has keyboard focus,
//! painted in the panel's blank tail rows (above the Layout keys block) so a
//! user who doesn't know the binds can read them where the space is free.
//!
//! The pairs are `FrameModel::keyhints` — the focus-aware, config-resolved
//! list the statusbar already shows (`keyhint::context_hints`) — so the block,
//! the statusbar strip and the Layout keys block all read the live keymap and
//! never drift from it. Pure — `chrome::draw_panel` paints the rows and decides
//! the space; nothing here is clickable, so it needs no hit-table entry.
//!
//! The sidebar has its own NAVIGATE footer and the corner overlay is
//! transient, so neither gets a panel block. See the OpenSpec change
//! `dynamic-key-discovery` for the later slices (per-section hints, fading
//! learned chords, the which-key overlay).

use crate::chrome::FrameModel;
use crate::panel::layout_hints::clip;

/// Most tips the block shows while `[panel] context_hints` is on. The pairs are
/// priority-sorted, so the cap trims the least important first.
pub(crate) const MAX_TIPS: usize = 12;
/// The cap with `[panel] context_hints = false` (the panel-focused NAVIGATE
/// footer only — its size before the block learned to follow focus).
pub(crate) const MAX_TIPS_OFF: usize = 6;

/// The block's title for the focused zone, or `None` when no block is drawn:
/// the panel's own NAVIGATE footer always shows while it has focus; the other
/// zones need `[panel] context_hints` on.
pub(crate) fn title(model: &FrameModel) -> Option<&'static str> {
    if model.panel_focused {
        return Some(" NAVIGATE ");
    }
    if model.context_hints_off {
        return None;
    }
    if model.center_focused {
        Some(" PANE KEYS ")
    } else if model.drawer_focused {
        Some(" DRAWER KEYS ")
    } else if model.masthead_focused || model.statusbar_focused {
        Some(" BAR KEYS ")
    } else {
        None
    }
}

/// Chords compare loosely: the statusbar list renders `Ctrl-x`, a user
/// override may read `ctrl+x`.
fn chord_key(chord: &str) -> String {
    chord.to_lowercase().replace('+', "-")
}

/// The `(chord, label)` rows that fit `cols` columns and `max_rows` rows
/// (title row included), taken from `model.keyhints` in order. Rows whose
/// chord the Layout keys block already shows (`shown`) are skipped. Empty when
/// no zone qualifies (see [`title`]), the panel is narrower than 16 columns,
/// or fewer than two rows are free. Overlong text is cut with `…`.
pub(crate) fn rows(
    model: &FrameModel,
    cols: usize,
    max_rows: usize,
    shown: &[(String, String)],
) -> Vec<(String, String)> {
    if title(model).is_none() || cols < 16 || max_rows < 2 {
        return Vec::new();
    }
    let cap = if model.context_hints_off {
        MAX_TIPS_OFF
    } else {
        MAX_TIPS
    };
    let skip: std::collections::HashSet<String> = shown.iter().map(|(c, _)| chord_key(c)).collect();
    let pairs: Vec<&(String, String)> = model
        .keyhints
        .iter()
        .filter(|(c, l)| !c.is_empty() && !l.is_empty() && !skip.contains(&chord_key(c)))
        .take(cap.min(max_rows - 1))
        .collect();
    // 1 col left margin, chord, 1 col gap, label.
    let inner = cols.saturating_sub(1);
    let chord_w = pairs
        .iter()
        .map(|(c, _)| c.chars().count())
        .max()
        .unwrap_or(0)
        .min(inner / 2);
    let label_w = inner.saturating_sub(chord_w + 1);
    pairs
        .into_iter()
        .map(|(c, l)| (clip(c, chord_w), clip(l, label_w)))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pane_model(n: usize) -> FrameModel {
        FrameModel {
            center_focused: true,
            keyhints: (0..n)
                .map(|i| (format!("Alt-{i}"), format!("action {i}")))
                .collect(),
            ..Default::default()
        }
    }

    #[test]
    fn title_follows_focus() {
        let mut m = FrameModel::default();
        assert_eq!(title(&m), None, "nothing focused: no block");
        m.center_focused = true;
        assert_eq!(title(&m), Some(" PANE KEYS "));
        m.center_focused = false;
        m.drawer_focused = true;
        assert_eq!(title(&m), Some(" DRAWER KEYS "));
        m.drawer_focused = false;
        m.statusbar_focused = true;
        assert_eq!(title(&m), Some(" BAR KEYS "));
        m.statusbar_focused = false;
        m.sidebar_focused = true;
        assert_eq!(title(&m), None, "the sidebar has its own footer");
        m.sidebar_focused = false;
        m.panel_focused = true;
        assert_eq!(title(&m), Some(" NAVIGATE "));
    }

    #[test]
    fn off_keeps_only_the_panel_footer() {
        let mut m = pane_model(3);
        m.context_hints_off = true;
        assert_eq!(title(&m), None);
        assert!(rows(&m, 44, 20, &[]).is_empty());
        m.center_focused = false;
        m.panel_focused = true;
        m.keyhints = (0..10).map(|i| (format!("{i}"), "x".to_string())).collect();
        assert_eq!(rows(&m, 44, 20, &[]).len(), MAX_TIPS_OFF);
    }

    #[test]
    fn expands_into_free_rows_up_to_the_cap() {
        let m = pane_model(20);
        assert_eq!(rows(&m, 44, 4, &[]).len(), 3, "title takes one row");
        assert_eq!(rows(&m, 44, 40, &[]).len(), MAX_TIPS);
        assert!(rows(&m, 44, 1, &[]).is_empty());
        let r = rows(&m, 44, 3, &[]);
        assert_eq!(r[0].1, "action 0", "priority order kept");
    }

    #[test]
    fn skips_chords_the_layout_block_shows() {
        let m = pane_model(3);
        let shown = vec![("alt+1".to_string(), "zoom pane".to_string())];
        let r = rows(&m, 44, 20, &shown);
        assert_eq!(r.len(), 2);
        assert!(!r.iter().any(|(c, _)| c == "Alt-1"));
    }

    #[test]
    fn fits_narrow_panels_and_hides_when_tiny() {
        let mut m = pane_model(4);
        m.keyhints[0].1 = "a very long label that cannot fit in the panel".into();
        for (c, l) in rows(&m, 20, 20, &[]) {
            assert!(
                1 + c.chars().count() + 1 + l.chars().count() <= 20,
                "{c} {l}"
            );
        }
        assert!(rows(&m, 10, 20, &[]).is_empty());
    }
}
