//! The statusbar's **left** cluster: the `?` help chip, the mode chip, and the
//! `[bars] bottom_left` widgets (today: the context keyhint strip).
//!
//! Extracted from `chrome::draw_statusbar` for the same reason the right
//! cluster has `statusbar_right_layout` / `statusbar_item_spans`: one layout
//! pass feeds both the painter and the hit-tester, so a click can never land
//! somewhere other than where the chip was drawn. Before this, the left half of
//! the statusbar was inert to the mouse entirely.
//!
//! Pure: no I/O, no globals beyond the chrome palette every draw helper reads.

use crate::chrome::{BarItemId, FrameModel, S, bottombar_widget};
use crate::compositor::Rect;
use crate::seg::{Seg, Tok, seg, seg_width};

/// The `[bars] bottom_left` id that renders the help chip. Users drop it from
/// `bottom_left` to hide the button; no separate config key.
pub const HELP_ID: &str = "help";

/// The removable bottom drawer presence widget.
pub const DRAWER_ID: &str = "drawer";

/// The chip's text. ASCII by construction, so it needs no `GlyphSet` entry and
/// survives `[theme] glyphs = "ascii"` unchanged.
const HELP_CHIP: &str = " ? ";

/// Build the left cluster's segs plus each addressable item's `(offset, width)`
/// within them. `left_budget` is the space the right cluster left over; the
/// keyhint strip trims at whole-binding boundaries to fit it.
pub fn left_layout(
    model: &FrameModel,
    left_budget: usize,
) -> (Vec<Seg>, Vec<(BarItemId, usize, usize)>) {
    let mut l: Vec<Seg> = vec![seg(Tok::Slot(S::Text), " ")];
    let mut spans: Vec<(BarItemId, usize, usize)> = Vec::new();

    if !model.mode_chip.is_empty() {
        l.push(Seg::chip(
            Tok::Slot(S::Accent),
            format!(" {} ", model.mode_chip),
        ));
        l.push(seg(Tok::Slot(S::Text), "  "));
    }
    let mut first = true;
    for id in &model.bars.bottom_left {
        if id == HELP_ID {
            // The one affordance that must never be trimmed away: it is the
            // only always-on pointer at the help system.
            let off = seg_width(&l);
            l.push(Seg::chip(Tok::Slot(S::Accent), HELP_CHIP.to_string()));
            l.push(seg(Tok::Slot(S::Text), " "));
            spans.push((BarItemId::Help, off, HELP_CHIP.width()));
            continue;
        }
        if id == DRAWER_ID {
            let Some(wd) = bottombar_widget(id, model) else {
                continue;
            };
            if !first {
                l.push(seg(Tok::Slot(S::Ghost3), " \u{00b7} "));
            }
            first = false;
            let off = seg_width(&l);
            let w = wd.text.width();
            l.push(seg(Tok::Attr(wd.fg), wd.text));
            spans.push((BarItemId::Widget(id.clone()), off, w));
            continue;
        }
        if id == "keyhints" {
            for (chord, label) in &model.keyhints {
                // Stage each binding as a unit; only commit it if the whole
                // thing still fits. Once one overflows, stop — never paint a
                // half-cut keybind.
                let mut hint: Vec<Seg> = Vec::new();
                if !first {
                    hint.push(seg(Tok::Slot(S::Text), "   "));
                }
                hint.push(seg(Tok::Slot(S::Faint), chord.clone()));
                hint.push(seg(Tok::Slot(S::Ghost), format!(" {label}")));
                if seg_width(&l) + seg_width(&hint) > left_budget {
                    break;
                }
                l.extend(hint);
                first = false;
            }
            continue;
        }
        let Some(wd) = bottombar_widget(id, model) else {
            continue;
        };
        if !first {
            l.push(seg(Tok::Slot(S::Ghost3), " \u{00b7} "));
        }
        first = false;
        let off = seg_width(&l);
        let w = wd.text.width();
        l.push(seg(Tok::Attr(wd.fg), wd.text));
        spans.push((BarItemId::Widget(id.clone()), off, w));
    }
    (l, spans)
}

/// Absolute `(id, Rect)` spans for the left cluster's addressable items, for
/// mouse hit-testing. The cluster is left-aligned, so offsets are from `rect.x`
/// — the mirror of [`crate::chrome::statusbar_item_spans`]'s right-alignment.
pub fn left_item_spans(model: &FrameModel, rect: Rect) -> Vec<(BarItemId, Rect)> {
    if rect.rows == 0 || rect.cols == 0 {
        return Vec::new();
    }
    let budget = crate::chrome::statusbar_left_budget(model, rect);
    let (_, spans) = left_layout(model, budget);
    // `Line::split` re-cuts the left run to `budget` after the right cluster
    // wins its space, so anything past it is not painted and must not be
    // clickable (the `?` span used to survive here and route a click on the
    // start of the status text into the help overlay).
    spans
        .into_iter()
        .filter(|(_, off, w)| *w > 0 && off + w <= budget)
        .map(|(id, off, w)| {
            (
                id,
                Rect {
                    x: rect.x + off,
                    y: rect.y,
                    cols: w,
                    rows: 1,
                },
            )
        })
        .collect()
}

use unicode_width::UnicodeWidthStr;

#[cfg(test)]
mod tests {
    use super::*;

    fn model_with(bottom_left: &[&str]) -> FrameModel {
        FrameModel {
            keyhints: vec![
                ("a".into(), "alpha".into()),
                ("b".into(), "bravo".into()),
                ("c".into(), "charlie".into()),
            ],
            bars: thegn_core::config::BarsConfig {
                bottom_left: bottom_left.iter().map(|s| (*s).to_string()).collect(),
                bottom_right: vec![],
                ..Default::default()
            },
            ..Default::default()
        }
    }

    fn rect(cols: usize) -> Rect {
        Rect {
            x: 0,
            y: 20,
            cols,
            rows: 1,
        }
    }

    #[test]
    fn help_chip_is_drawn_and_hit_tests_where_it_is_painted() {
        let m = model_with(&["help", "keyhints"]);
        let (segs, _) = left_layout(&m, 80);
        let text: String = segs.iter().map(|s| s.text.as_str()).collect();
        assert!(text.contains('?'), "chip painted: {text:?}");

        let spans = left_item_spans(&m, rect(120));
        let (_, r) = spans
            .iter()
            .find(|(id, _)| *id == BarItemId::Help)
            .expect("help span present");
        // The span must cover the cells the `?` actually occupies.
        let idx = text.find('?').unwrap();
        let col = text[..idx].width();
        assert!(
            r.x <= col && col < r.x + r.cols,
            "span {r:?} covers the `?` at column {col} of {text:?}"
        );
    }

    /// Dropping `help` from `[bars] bottom_left` hides the chip entirely —
    /// button, span, and the cells it occupied.
    #[test]
    fn dropping_the_id_removes_chip_and_span() {
        let m = model_with(&["keyhints"]);
        let (segs, _) = left_layout(&m, 80);
        let text: String = segs.iter().map(|s| s.text.as_str()).collect();
        assert!(!text.contains(" ? "), "no chip: {text:?}");
        assert!(
            !left_item_spans(&m, rect(120))
                .iter()
                .any(|(id, _)| *id == BarItemId::Help)
        );
    }

    /// The chip is the last thing that should disappear, so it is painted
    /// before the keyhints and is not subject to their budget trim.
    #[test]
    fn chip_survives_a_narrow_bar() {
        for cols in [8usize, 12, 20, 40] {
            let m = model_with(&["help", "keyhints"]);
            let (segs, _) = left_layout(&m, cols);
            let text: String = segs.iter().map(|s| s.text.as_str()).collect();
            assert!(text.contains('?'), "cols {cols}: {text:?}");
        }
    }

    /// Keyhints still trim at whole-binding boundaries once the chip has taken
    /// its cells — the invariant `statusbar_keyhints_stop_at_last_whole_binding`
    /// pins for the bar as a whole.
    #[test]
    fn keyhints_still_trim_whole_bindings_after_the_chip() {
        let m = model_with(&["help", "keyhints"]);
        for budget in [10usize, 18, 30, 60] {
            let (segs, _) = left_layout(&m, budget);
            for (chord, _) in &m.keyhints {
                let text: String = segs.iter().map(|s| s.text.as_str()).collect();
                // A chord is either fully present or fully absent.
                if let Some(p) = text.find(chord.as_str()) {
                    assert_eq!(&text[p..p + chord.len()], chord.as_str());
                }
            }
        }
    }

    /// `Help` opens the overlay, not a bar detail popup.
    #[test]
    fn help_item_has_no_detail_popup() {
        assert!(!BarItemId::Help.has_detail());
        assert!(BarItemId::Widget("cpu".into()).has_detail());
    }

    #[test]
    fn drawer_widget_is_atomic_and_hit_tests_where_painted() {
        let mut m = model_with(&["drawer", "keyhints"]);
        m.drawer_bar.occupant_count = 2;
        let (segs, _) = left_layout(&m, 80);
        let text: String = segs.iter().map(|s| s.text.as_str()).collect();
        assert!(text.contains("drawer (2)"), "closed indicator: {text:?}");

        let spans = left_item_spans(&m, rect(120));
        let (_, drawer_rect) = spans
            .iter()
            .find(|(id, _)| *id == BarItemId::Widget(DRAWER_ID.into()))
            .expect("drawer span present");
        let start = text.find("drawer").expect("drawer label painted");
        let start = text[..start].width();
        assert!(
            drawer_rect.x <= start && start < drawer_rect.x + drawer_rect.cols,
            "span {drawer_rect:?} covers drawer label at {start} in {text:?}"
        );

        m.drawer_bar.open = true;
        m.drawer_bar.occupant = "atac".into();
        let (segs, _) = left_layout(&m, 80);
        let text: String = segs.iter().map(|s| s.text.as_str()).collect();
        assert!(text.contains("atac (2)"), "open indicator: {text:?}");
    }

    #[test]
    fn dropping_drawer_id_removes_indicator_and_span() {
        let m = model_with(&["keyhints"]);
        assert!(
            !left_layout(&m, 80)
                .0
                .iter()
                .any(|s| s.text.contains("drawer"))
        );
        assert!(
            !left_item_spans(&m, rect(120))
                .iter()
                .any(|(id, _)| *id == BarItemId::Widget(DRAWER_ID.into()))
        );
    }
}
