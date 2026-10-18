//! F5b painters live here while F5a owns `monitor/kit/mod.rs`; F6b may re-home them under `kit/`.
#![allow(dead_code)] // F6b is the first caller of the monitor graph kit.

use crate::chrome;
use crate::seg::{self, Line, Tok};
use termwiz::color::ColorAttribute;
use thegn_core::termcaps::ColorDepth;
use thegn_core::theme::{Band, Palette, RampId, SeriesRole, band_of};
use thegn_core::viz_raster::{CellBuf, RCell, RasterTable};

mod graph;
mod heatmap;
mod mirrored;
mod spark;
mod stacked;

#[cfg(test)]
mod tests;

/// How raster levels map to the monitor palette.
#[derive(Debug, Clone, Copy)]
pub(crate) enum Paint {
    Ramp { id: RampId, warn: f32, crit: f32 },
    Flat(SeriesRole),
    Bands(&'static [SeriesRole]),
}

#[derive(Debug, Clone, Copy, PartialEq)]
struct Style {
    fg: ColorAttribute,
    bg: Option<ColorAttribute>,
    bold: bool,
}

/// Pre-resolved colors for a single paint operation.
struct Resolved {
    levels: [Style; 8],
    dim: Style,
}

impl Resolved {
    fn new(palette: &Palette, paint: Paint, depth: ColorDepth) -> Self {
        #[cfg(test)]
        RESOLUTIONS.with(|count| count.set(count.get() + 1));

        let dim = Style {
            fg: if depth == ColorDepth::None {
                ColorAttribute::Default
            } else {
                chrome::theme_color(&palette.dim)
            },
            bg: None,
            bold: false,
        };
        let levels = match paint {
            Paint::Ramp { id, warn, crit } => {
                let ramp = palette.ramp(id);
                std::array::from_fn(|level| {
                    let threshold = band_of(level as f32 / 7.0, warn, crit);
                    let fg = match depth {
                        ColorDepth::None => ColorAttribute::Default,
                        ColorDepth::Ansi16 => chrome::theme_color(palette.band(id, threshold)),
                        ColorDepth::Truecolor | ColorDepth::Ansi256 => {
                            chrome::theme_color(&ramp[level])
                        }
                    };
                    Style {
                        fg,
                        bg: None,
                        bold: depth == ColorDepth::None && threshold == Band::Crit,
                    }
                })
            }
            Paint::Flat(role) => {
                let fg = if depth == ColorDepth::None {
                    ColorAttribute::Default
                } else {
                    chrome::theme_color(palette.series(role))
                };
                [Style {
                    fg,
                    bg: None,
                    bold: false,
                }; 8]
            }
            Paint::Bands(roles) => std::array::from_fn(|level| {
                let fg = match roles.get(level) {
                    Some(role) if depth != ColorDepth::None => {
                        chrome::theme_color(palette.series(*role))
                    }
                    Some(_) => ColorAttribute::Default,
                    None => dim.fg,
                };
                Style {
                    fg,
                    bg: None,
                    bold: false,
                }
            }),
        };
        Self { levels, dim }
    }

    fn cell(&self, cell: RCell) -> Style {
        let mut style = self
            .levels
            .get(usize::from(cell.fg))
            .copied()
            .unwrap_or(self.dim);
        style.bg = cell.bg.map(|index| {
            self.levels
                .get(usize::from(index))
                .map_or(self.dim.fg, |style| style.fg)
        });
        style
    }
}

/// Reusable raster and emitted rows.
#[derive(Default)]
pub(crate) struct GraphOut {
    pub(crate) cells: CellBuf,
    pub(crate) lines: Vec<Line>,
}

pub(crate) fn paint_sparkline(
    values: &[f32],
    width: usize,
    ramp: RampId,
    rect: crate::compositor::Rect,
    surface: &mut termwiz::surface::Surface,
) {
    if values.is_empty() || rect.rows == 0 || rect.cols == 0 {
        return;
    }
    let mut out = GraphOut::new();
    spark::paint_spark(
        &mut out,
        values,
        width,
        ramp,
        &crate::chrome::palette_snapshot(),
    );
    if let Some(line) = out.lines.first() {
        crate::seg::draw_line(
            surface,
            rect.x,
            rect.y,
            rect.cols,
            line,
            crate::seg::Tok::Slot(crate::chrome::S::Dim),
        );
    }
}

impl GraphOut {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    fn prepare(&mut self, w: usize, h: usize) {
        self.cells.reset(w, h);
        self.lines.resize_with(h, Line::default);
        for line in &mut self.lines {
            if let Line::Segs(segs) = line {
                segs.clear();
            } else {
                *line = Line::Segs(Vec::new());
            }
        }
    }

    fn emit(&mut self, resolved: &Resolved) {
        self.lines.resize_with(self.cells.height(), Line::default);
        for row_index in 0..self.cells.height() {
            self.emit_row(row_index, resolved);
        }
    }

    fn emit_rows(&mut self, upper_h: usize, upper: &Resolved, lower: &Resolved) {
        self.lines.resize_with(self.cells.height(), Line::default);
        for row_index in 0..self.cells.height() {
            let resolved = if row_index < upper_h { upper } else { lower };
            self.emit_row(row_index, resolved);
        }
    }

    fn emit_row(&mut self, row_index: usize, resolved: &Resolved) {
        let line = &mut self.lines[row_index];
        if !matches!(line, Line::Segs(_)) {
            *line = Line::Segs(Vec::new());
        }
        if let Line::Segs(segs) = line {
            segs.clear();
            let row = self.cells.row(row_index);
            let mut start = 0;
            while start < row.len() {
                let style = resolved.cell(row[start]);
                let mut end = start + 1;
                while end < row.len() && resolved.cell(row[end]) == style {
                    end += 1;
                }
                let text: String = row[start..end].iter().map(|cell| cell.ch).collect();
                let mut run = seg::seg(Tok::Attr(style.fg), text);
                run.bg = style.bg.map(Tok::Attr);
                run.bold = style.bold;
                segs.push(run);
                start = end;
            }
        }
    }
}

pub(crate) fn table() -> &'static RasterTable {
    RasterTable::for_level(crate::caps::unicode_level())
}

#[cfg(test)]
pub(crate) fn default_palette() -> Palette {
    crate::chrome::palette_snapshot()
}

#[cfg(test)]
thread_local! {
    static RESOLUTIONS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

#[cfg(test)]
fn resolution_count() -> usize {
    RESOLUTIONS.with(std::cell::Cell::get)
}

#[cfg(test)]
fn reset_resolution_count() {
    RESOLUTIONS.with(|count| count.set(0));
}
