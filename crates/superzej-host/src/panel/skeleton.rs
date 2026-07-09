//! The panel's cold-switch skeleton: dim placeholder bars shaped like the
//! panel's usual content (header line, status row, a list), drawn while a
//! cache-miss switch waits for its hydration (`FrameModel::panel_pending`).
//! Deliberately **static** — an animated shimmer would need a timer wake,
//! violating the idle-loop invariant for pure decoration.

use crate::chrome::S;
use crate::compositor::Rect;
use crate::seg::{Line, Seg, Tok, seg, sp};
use termwiz::surface::Surface;

/// Bar widths per row, as a fraction of the panel width — a header, a short
/// status pair, and a list of entries. Tuned to read as "content loading",
/// not as real (mistakable) data.
const ROWS: &[&[(usize, usize)]] = &[
    &[(2, 5)],         // section header
    &[(1, 4), (1, 6)], // branch + status chips
    &[],               // gap
    &[(3, 5)],
    &[(2, 5)],
    &[(3, 7)],
    &[(1, 2)],
    &[(2, 5)],
];

/// Draw the skeleton into the panel rect (already filled with the panel bg).
pub(crate) fn draw(surface: &mut Surface, rect: Rect) {
    let h = crate::caps::active_glyphs().box_h;
    for (i, bars) in ROWS.iter().enumerate() {
        let y = rect.y + 1 + i;
        if y >= rect.y + rect.rows {
            break;
        }
        if bars.is_empty() {
            continue;
        }
        let mut segs: Vec<Seg> = vec![sp(2)];
        for (num, den) in bars.iter() {
            let w = (rect.cols.saturating_sub(4) * num / den).max(3);
            segs.push(seg(Tok::Slot(S::Faint), h.repeat(w)));
            segs.push(sp(2));
        }
        crate::seg::draw_line(
            surface,
            rect.x,
            y,
            rect.cols,
            &Line::Segs(segs),
            Tok::Slot(S::Panel),
        );
    }
}
