//! Fixed columns use the same grapheme boundaries and widths as Surface.
//! Text is made single-line before measurement; no shared dynamic table changes.
use super::{Cell, TableSection};
use crate::chrome::{self, S};
use crate::compositor::Rect;
use crate::seg::Tok;
use finl_unicode::grapheme_clusters::Graphemes;
use termwiz::cell::{CellAttributes, grapheme_column_width};
use termwiz::surface::{Change, Position, Surface};

/// Fit whole rendered graphemes, then pad exactly to the column boundary.
/// Controls become spaces, including line movement and bidi formatting. A
/// standalone zero-width grapheme becomes a space, matching Surface's one-cell
/// advancement without letting a combining mark attach to a preceding column.
fn prefix(text: &str, width: usize) -> (String, usize, bool) {
    let mut result = String::new();
    let mut used = 0;
    for grapheme in Graphemes::new(text) {
        if used == width {
            return (result, used, true);
        }
        let unsafe_text = grapheme.chars().any(|c| {
            c.is_control() || matches!(c, '\u{2028}'..='\u{202e}' | '\u{2066}'..='\u{2069}')
        });
        let cells = grapheme_column_width(grapheme, None);
        let (text, cells) = if unsafe_text || cells == 0 {
            (" ", 1)
        } else {
            (grapheme, cells)
        };
        if cells > width - used {
            return (result, used, true);
        }
        result.push_str(text);
        used += cells;
    }
    (result, used, false)
}

fn fitted(text: &str, width: usize) -> String {
    let (mut result, mut used, truncated) = prefix(text, width);
    if truncated {
        let marker = crate::caps::active_glyphs().ellipsis;
        let marker_width = termwiz::cell::unicode_column_width(marker, None);
        if marker_width <= width {
            (result, used, _) = prefix(&result, width - marker_width);
            result.push_str(marker);
            used += marker_width;
        } else {
            // A narrow ASCII viewport cannot fit its three-cell marker. Blank
            // the cell rather than presenting a truncated numeric identity.
            result.clear();
            used = 0;
        }
    }
    result.extend(std::iter::repeat_n(' ', width - used));
    result
}

fn emit(surface: &mut Surface, text: String, attrs: &CellAttributes) {
    surface.add_change(Change::AllAttributes(attrs.clone()));
    surface.add_change(Change::Text(text));
}

#[allow(clippy::too_many_arguments)] // same stack drawing boundary as dynamic tables
pub(super) fn draw(
    surface: &mut Surface,
    clip: Rect,
    x: usize,
    y0: i64,
    width: usize,
    table: &TableSection,
    widths: &[usize],
) {
    let (surface_cols, surface_rows) = surface.dimensions();
    let width = width
        .min(clip.x.saturating_add(clip.cols).saturating_sub(x))
        .min(surface_cols.saturating_sub(x));
    if width == 0 || x < clip.x {
        return;
    }
    let top = clip.y.min(surface_rows) as i64;
    let bottom = clip.y.saturating_add(clip.rows).min(surface_rows) as i64;
    let gutter = usize::from(table.sel.is_some());
    chrome::with_palette(|palette| {
        let paint = |surface: &mut Surface, y: i64, row: Option<usize>| {
            if y < top || y >= bottom {
                return;
            }
            let selected = row.is_some() && row == table.sel;
            let background = Tok::Slot(if selected { S::Panel2 } else { S::Panel });
            let attrs = |tone: Tok| {
                let mut attrs = CellAttributes::default();
                attrs.set_foreground(tone.resolve(palette));
                attrs.set_background(background.resolve(palette));
                attrs
            };
            // Paint disjoint spans, including blanks: a full-row preclear
            // would make Surface construct every padded column cell twice.
            // Covering the gutter, separators and tail still clears a reused
            // surface and preserves the full selected background band.
            // Every fitted span advances by its exact display width. Position
            // once per row; the next row resets before any deferred right-edge
            // wrap can occur, including on the bottom row of the Surface.
            surface.add_change(Change::CursorPosition {
                x: Position::Absolute(x),
                y: Position::Absolute(y as usize),
            });
            if gutter > 0 {
                let (text, tone) = if selected {
                    (
                        fitted(crate::caps::active_glyphs().half_block_r, 1),
                        Tok::Slot(S::Accent),
                    )
                } else {
                    (" ".into(), Tok::Slot(S::Text))
                };
                emit(surface, text, &attrs(tone));
            }
            let mut offset = gutter;
            // Missing widths/extra cells are clipped, never indexed unchecked.
            // Zero-width columns consume no separator. The viewport bounds all
            // work even if a caller supplies malformed or oversized metadata.
            for (column, requested) in widths.iter().take(width).enumerate() {
                if offset >= width {
                    break;
                }
                let count = (*requested).min(width - offset);
                if count == 0 {
                    continue;
                }
                let (text, tone) = match row {
                    None => (
                        fitted(
                            table.header.get(column).map(String::as_str).unwrap_or(""),
                            count,
                        ),
                        Tok::Slot(S::Ghost),
                    ),
                    Some(row) => match table.rows.get(row).and_then(|row| row.get(column)) {
                        Some(Cell::Text(text, tone)) => (fitted(text, count), *tone),
                        Some(Cell::Bar(frac, cells, tone)) => {
                            let (bar, track) = crate::caps::bar_track(*frac, (*cells).min(count));
                            (fitted(&format!("{bar}{track}"), count), *tone)
                        }
                        None => (" ".repeat(count), Tok::Slot(S::Ghost)),
                    },
                };
                emit(surface, text, &attrs(tone));
                offset += count;
                if offset < width {
                    emit(surface, " ".into(), &attrs(Tok::Slot(S::Text)));
                    offset += 1;
                }
            }
            if offset < width {
                emit(
                    surface,
                    " ".repeat(width - offset),
                    &attrs(Tok::Slot(S::Text)),
                );
            }
        };
        let header = i64::from(!table.header.is_empty());
        if header > 0 {
            paint(surface, y0, None);
        }
        let body_y = y0.saturating_add(header);
        let first = top.saturating_sub(body_y).max(0) as usize;
        let end = (bottom.saturating_sub(body_y).max(0) as usize).min(table.rows.len());
        for row in first..end {
            paint(surface, body_y + row as i64, Some(row));
        }
    });
    surface.add_change(Change::AllAttributes(CellAttributes::default()));
}

#[cfg(test)]
#[path = "sections_fixed_table_tests.rs"]
mod tests;
