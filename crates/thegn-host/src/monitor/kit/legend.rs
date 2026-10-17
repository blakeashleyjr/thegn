use super::grid::{CellGrid, CellStyle, GridView};
use crate::{
    caps::{self, Glyph},
    compositor::Rect,
    seg::{StyleTok, Tok},
};
use unicode_width::UnicodeWidthStr;

#[derive(Debug, Clone, PartialEq)]
pub struct LegendSwatch {
    pub label: String,
    pub color: Tok,
}
impl LegendSwatch {
    pub fn series(label: impl Into<String>, role: thegn_core::theme::SeriesRole) -> Self {
        Self {
            label: label.into(),
            color: Tok::Series(role),
        }
    }
}

pub fn draw(grid: &mut CellGrid, rect: Rect, entries: &[LegendSwatch]) {
    let mut view = GridView::new(grid, rect);
    if view.rect().rows == 0 || view.rect().cols == 0 {
        return;
    }
    let row = 0;
    let mut x = 0;
    let width = view.rect().cols;
    let swatch = caps::glyph(Glyph::LegendSwatch);
    let mark = caps::glyph(Glyph::Ellipsis);
    let swatch_width = UnicodeWidthStr::width(swatch);
    let mark_width = UnicodeWidthStr::width(mark);
    let all_width = entries
        .iter()
        .map(|entry| swatch_width + 1 + UnicodeWidthStr::width(entry.label.as_str()) + 2)
        .sum::<usize>();
    let truncated = all_width > width;
    let limit = if truncated {
        width.saturating_sub(mark_width)
    } else {
        width
    };
    for entry in entries {
        let need = swatch_width + 1 + UnicodeWidthStr::width(entry.label.as_str()) + 2;
        if x + need > limit {
            break;
        }
        let style = CellStyle {
            fg: StyleTok::from_tok(entry.color).unwrap_or(StyleTok::INHERIT),
            bg: StyleTok::INHERIT,
            ..CellStyle::default()
        };
        x += view.put_str(x, row, swatch, style);
        x += view.put_str(x, row, " ", style);
        x += view.put_str(x, row, &entry.label, style);
        x += 2;
    }
    if truncated && mark_width <= width {
        view.put_str(width - mark_width, row, mark, CellStyle::default());
    }
}
