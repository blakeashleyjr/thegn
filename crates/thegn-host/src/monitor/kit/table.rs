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
        let style = CellStyle {
            attr: super::grid::BOLD,
            ..CellStyle::default()
        };
        let sort_mark = spec.sort.filter(|(n, _)| *n == i).map(|(_, direction)| {
            caps::glyph(match direction {
                SortDirection::Asc => Glyph::SortAsc,
                SortDirection::Desc => Glyph::SortDesc,
            })
        });
        if let Some(mark) = sort_mark {
            let mark_width = UnicodeWidthStr::width(mark);
            let title_width = width.saturating_sub(mark_width + 1);
            let title_used = super::grid::put_clipped(&mut v, x, 0, col.title, title_width, style);
            if title_used + 1 + mark_width <= width {
                v.put_str(x + title_used, 0, " ", style);
                v.put_str(x + title_used + 1, 0, mark, style);
            }
        } else {
            super::grid::put_clipped(&mut v, x, 0, col.title, width, style);
        }
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
            let text = match cell {
                TableCell::Text(_) => None,
                TableCell::Number(reading) => Some(spec.fmt.reading(
                    reading,
                    col.unit.unwrap_or(Unit::Count),
                    width,
                )),
            };
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
            if let Some(text) = text {
                v.put_str(x, y, &text, style);
            } else if let TableCell::Text(value) = cell {
                let text_width = UnicodeWidthStr::width(value);
                let pad = if col.align == Align::Right {
                    width.saturating_sub(text_width.min(width))
                } else {
                    0
                };
                for dx in 0..pad {
                    v.put(x + dx, y, CellStyle { ch: ' ', ..style });
                }
                super::grid::put_clipped(
                    &mut v,
                    x + pad,
                    y,
                    value,
                    width.saturating_sub(pad),
                    style,
                );
            }
            x += col.width.saturating_add(1);
        }
    }
}
