use super::grid::{CellGrid, CellStyle, GridView};
use crate::{
    caps::{self, Glyph},
    chrome::S,
    compositor::Rect,
    seg::StyleTok,
};
use thegn_core::termcaps::UnicodeLevel;
use unicode_width::UnicodeWidthStr;
fn tok(t: crate::seg::Tok) -> StyleTok {
    StyleTok::from_tok(t).unwrap_or(StyleTok::INHERIT)
}
fn edge(ch: char) -> CellStyle {
    CellStyle {
        ch,
        fg: tok(crate::seg::Tok::Slot(S::Border)),
        bg: StyleTok::INHERIT,
        attr: 0,
    }
}

pub fn panel<'a>(
    grid: &'a mut CellGrid,
    rect: Rect,
    title: &str,
    badge: Option<&str>,
) -> GridView<'a> {
    let r = super::grid::GridView::new(grid, rect).rect();
    if r.cols < 2 || r.rows < 2 {
        return super::grid::GridView::new(
            grid,
            Rect {
                x: r.x,
                y: r.y,
                cols: 0,
                rows: 0,
            },
        );
    }
    let g = caps::active_glyphs();
    let badge_width = badge
        .map_or(0, UnicodeWidthStr::width)
        .min(r.cols.saturating_sub(4));
    let badge_start = r.cols.saturating_sub(badge_width + 2);
    let title_width = badge.map_or(r.cols.saturating_sub(4), |_| badge_start.saturating_sub(3));
    {
        let mut v = super::grid::GridView::new(grid, r);
        let corners = if caps::unicode_level() == UnicodeLevel::Ascii {
            [Glyph::BoxTl, Glyph::BoxTr, Glyph::BoxBl, Glyph::BoxBr]
        } else {
            [
                Glyph::BoxRoundTl,
                Glyph::BoxRoundTr,
                Glyph::BoxRoundBl,
                Glyph::BoxRoundBr,
            ]
        };
        let round = |glyph: Glyph| CellStyle {
            ch: caps::glyph(glyph).chars().next().unwrap_or('+'),
            ..edge('+')
        };
        v.put(0, 0, round(corners[0]));
        v.put(r.cols - 1, 0, round(corners[1]));
        v.put(0, r.rows - 1, round(corners[2]));
        v.put(r.cols - 1, r.rows - 1, round(corners[3]));
        v.hline(
            1,
            0,
            r.cols - 2,
            edge(g.box_h.chars().next().unwrap_or('-')),
        );
        v.hline(
            1,
            r.rows - 1,
            r.cols - 2,
            edge(g.box_h.chars().next().unwrap_or('-')),
        );
        v.vline(
            0,
            1,
            r.rows - 2,
            edge(g.box_v.chars().next().unwrap_or('|')),
        );
        v.vline(
            r.cols - 1,
            1,
            r.rows - 2,
            edge(g.box_v.chars().next().unwrap_or('|')),
        );
        let textstyle = CellStyle {
            ch: ' ',
            fg: tok(crate::seg::Tok::Slot(S::Text)),
            bg: StyleTok::INHERIT,
            attr: 0,
        };
        super::grid::put_clipped(&mut v, 2, 0, title, title_width, textstyle);
        if let Some(b) = badge {
            super::grid::put_clipped(
                &mut v,
                badge_start,
                0,
                b,
                badge_width,
                CellStyle {
                    fg: tok(crate::seg::Tok::State(thegn_core::theme::StateRole::Stale)),
                    ..textstyle
                },
            );
        }
    }
    super::grid::GridView::new(
        grid,
        Rect {
            x: r.x + 1,
            y: r.y + 1,
            cols: r.cols - 2,
            rows: r.rows - 2,
        },
    )
}
