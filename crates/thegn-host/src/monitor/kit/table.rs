use super::grid::{CellGrid, CellStyle, GridView};
use crate::{
    caps::{self, Glyph},
    compositor::Rect,
    seg::{StyleTok, Tok},
};
use thegn_core::numfmt::{NumFmt, Reading, Unit};
use unicode_width::UnicodeWidthStr;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Align {
    Left,
    Right,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Col {
    pub title: &'static str,
    pub width: usize,
    pub align: Align,
    pub unit: Option<Unit>,
}
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum TableCell<'a> {
    Text(&'a str),
    Number(Reading),
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SortDirection {
    Asc,
    Desc,
}
pub struct TableSpec<'a> {
    pub cols: &'a [Col],
    pub rows: &'a [&'a [TableCell<'a>]],
    pub offset: usize,
    pub visible: usize,
    pub sort: Option<(usize, SortDirection)>,
    pub selected: Option<usize>,
    pub fmt: NumFmt,
}

fn clipped(s: &str, width: usize) -> String {
    if UnicodeWidthStr::width(s) <= width {
        return s.to_owned();
    }
    if width == 0 {
        return String::new();
    }
    let mark = caps::glyph(Glyph::Ellipsis);
    let mark_w = UnicodeWidthStr::width(mark);
    let mut out = String::new();
    let limit = width.saturating_sub(mark_w);
    let mut used = 0;
    for ch in s.chars() {
        let cw = unicode_width::UnicodeWidthChar::width(ch).unwrap_or(0);
        if used + cw > limit {
            break;
        }
        out.push(ch);
        used += cw;
    }
    if mark_w <= width {
        out.push_str(mark);
    }
    out
}

pub fn draw(grid: &mut CellGrid, rect: Rect, spec: &TableSpec<'_>) {
    let mut v = GridView::new(grid, rect);
    let area = v.rect();
    if area.rows == 0 || area.cols == 0 {
        return;
    }
    let mut x = 0usize;
    for (i, col) in spec.cols.iter().enumerate() {
        if x >= area.cols {
            break;
        }
        let width = col.width.min(area.cols - x);
        let mut title = col.title.to_owned();
        if let Some((_, direction)) = spec.sort.filter(|(n, _)| *n == i) {
            title.push(' ');
            title.push_str(caps::glyph(match direction {
                SortDirection::Asc => Glyph::SortAsc,
                SortDirection::Desc => Glyph::SortDesc,
            }));
        }
        let text = clipped(&title, width);
        v.put_str(
            x,
            0,
            &text,
            CellStyle {
                attr: super::grid::BOLD,
                ..CellStyle::default()
            },
        );
        x += col.width.saturating_add(1);
    }
    let start = spec.offset.min(spec.rows.len());
    let end = start
        .saturating_add(spec.visible)
        .min(spec.rows.len())
        .min(start.saturating_add(area.rows - 1));
    for (visible_row, row_idx) in (start..end).enumerate() {
        let y = visible_row + 1;
        let selected = spec.selected == Some(row_idx);
        let mut x = 0usize;
        for (col_idx, col) in spec.cols.iter().enumerate() {
            if x >= area.cols {
                break;
            }
            let width = col.width.min(area.cols - x);
            let cell = spec.rows[row_idx]
                .get(col_idx)
                .copied()
                .unwrap_or(TableCell::Text(""));
            let mut text = match cell {
                TableCell::Text(value) => clipped(value, width),
                TableCell::Number(reading) => {
                    spec.fmt
                        .reading(reading, col.unit.unwrap_or(Unit::Count), width)
                }
            };
            let text_width = UnicodeWidthStr::width(text.as_str());
            if col.align == Align::Right && text_width < width {
                text.insert_str(0, &" ".repeat(width - text_width));
            }
            let style = CellStyle {
                fg: StyleTok::from_tok(Tok::Slot(crate::chrome::S::Text))
                    .unwrap_or(StyleTok::INHERIT),
                bg: if selected {
                    StyleTok::from_tok(Tok::Series(thegn_core::theme::SeriesRole::Selected))
                        .unwrap_or(StyleTok::INHERIT)
                } else {
                    StyleTok::INHERIT
                },
                ..CellStyle::default()
            };
            v.put_str(x, y, &text, style);
            x += col.width.saturating_add(1);
        }
    }
}
