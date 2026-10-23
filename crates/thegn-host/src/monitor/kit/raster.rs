use super::grid::{CellGrid, CellStyle};
use crate::{
    compositor::Rect,
    seg::{StyleTok, Tok},
};
use thegn_core::{
    theme::{RampId, SeriesRole},
    viz_raster::{CellBuf, RasterTable},
};

/// How raster levels map to the monitor palette.
#[derive(Debug, Clone, Copy)]
pub(crate) enum RasterStyle<'a> {
    Series(&'a [SeriesRole]),
    Ramp(RampId),
}

/// Paint raster cells into a grid, preserving optional stacked backgrounds.
pub(crate) fn draw_raster(
    grid: &mut CellGrid,
    rect: Rect,
    raster: &CellBuf,
    style: RasterStyle<'_>,
    table: &RasterTable,
    skip_blank: bool,
) {
    for y in 0..rect.rows.min(raster.height()) {
        for x in 0..rect.cols.min(raster.width()) {
            let cell = raster.row(y)[x];
            if skip_blank && (cell.ch == table.blank || cell.ch == table.cell_blank) {
                continue;
            }
            let (fg, bg) = match style {
                RasterStyle::Series(roles) => (
                    roles
                        .get(usize::from(cell.fg))
                        .copied()
                        .map(Tok::Series)
                        .and_then(StyleTok::from_tok)
                        .unwrap_or(StyleTok::INHERIT),
                    cell.bg
                        .and_then(|index| roles.get(usize::from(index)).copied())
                        .map(Tok::Series)
                        .and_then(StyleTok::from_tok)
                        .unwrap_or(StyleTok::INHERIT),
                ),
                RasterStyle::Ramp(ramp) => (
                    StyleTok::from_tok(Tok::Ramp(ramp, cell.fg)).unwrap_or(StyleTok::INHERIT),
                    cell.bg
                        .and_then(|level| StyleTok::from_tok(Tok::Ramp(ramp, level)))
                        .unwrap_or(StyleTok::INHERIT),
                ),
            };
            grid.view(Rect {
                x: rect.x + x,
                y: rect.y + y,
                cols: 1,
                rows: 1,
            })
            .put(
                0,
                0,
                CellStyle {
                    ch: cell.ch,
                    fg,
                    bg,
                    ..Default::default()
                },
            );
        }
    }
}
