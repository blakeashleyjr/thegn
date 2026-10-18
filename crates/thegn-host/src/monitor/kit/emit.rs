use super::grid::CellGrid;
use crate::{
    caps, chrome,
    compositor::Rect,
    seg::{StyleTok, Tok},
};
use std::collections::HashMap;
use termwiz::cell::{Cell, CellAttributes, Intensity, Underline};
use termwiz::color::ColorAttribute;
use termwiz::surface::Surface;
use thegn_core::termcaps::ColorDepth;
use unicode_width::UnicodeWidthChar;

#[derive(Clone, Copy, PartialEq, Eq)]
struct CacheKey {
    generation: u64,
    depth: ColorDepth,
}
#[derive(Default)]
struct StyleCache {
    key: Option<CacheKey>,
    colors: HashMap<StyleTok, ColorAttribute>,
    #[cfg(test)]
    rebuilds: usize,
}
#[derive(Default)]
pub struct Emitter {
    cache: StyleCache,
    attrs: CellAttributes,
    #[cfg(test)]
    text_runs: usize,
}
impl Emitter {
    fn rebuild(&mut self) {
        let key = CacheKey {
            generation: chrome::palette_generation(),
            depth: caps::color_depth(),
        };
        if self.cache.key == Some(key) {
            return;
        }
        let mut tokens = Vec::new();
        tokens.extend(chrome::S::ALL.into_iter().map(Tok::Slot));
        tokens.extend(thegn_core::theme::Hue::ALL.into_iter().map(Tok::Hue));
        tokens.extend((0..=4).map(Tok::Heat));
        tokens.push(Tok::SelAccent);
        tokens.extend(
            thegn_core::theme::SeriesRole::ALL
                .into_iter()
                .map(Tok::Series),
        );
        tokens.extend(
            thegn_core::theme::StateRole::ALL
                .into_iter()
                .map(Tok::State),
        );
        tokens.extend(
            thegn_core::theme::OwnerRole::ALL
                .into_iter()
                .map(Tok::Owner),
        );
        let colors = chrome::with_palette(|palette| {
            let mut colors: HashMap<StyleTok, ColorAttribute> = tokens
                .into_iter()
                .filter_map(|tok| {
                    let packed = StyleTok::from_tok(tok)?;
                    Some((packed, tok.resolve(palette)))
                })
                .collect();
            for id in thegn_core::theme::RampId::ALL {
                if key.depth == ColorDepth::Ansi16 {
                    for level in 0..=7 {
                        let band = match level {
                            0..=4 => thegn_core::theme::Band::Normal,
                            5..=6 => thegn_core::theme::Band::Warn,
                            _ => thegn_core::theme::Band::Crit,
                        };
                        if let Some(packed) = StyleTok::from_tok(Tok::Ramp(id, level)) {
                            colors.insert(packed, chrome::theme_color(palette.band(id, band)));
                        }
                    }
                } else {
                    for (level, color) in palette.ramp(id).iter().enumerate() {
                        if let Some(packed) = StyleTok::from_tok(Tok::Ramp(id, level as u8)) {
                            colors.insert(packed, chrome::theme_color(color));
                        }
                    }
                }
            }
            colors
        });
        self.cache.colors = colors;
        self.cache.key = Some(key);
        #[cfg(test)]
        {
            self.cache.rebuilds += 1;
        }
    }
    fn color(&self, token: StyleTok) -> ColorAttribute {
        self.cache
            .colors
            .get(&token)
            .copied()
            .unwrap_or(ColorAttribute::Default)
    }
    /// Writes cells directly, bypassing the Surface change log and line seqno.
    /// Host frame paths diff cells (`diff_region`/`diff_screens` and
    /// `draw_from_screen`), and image placements in overwritten cells are
    /// dropped because the host draws images out of band. For 1–4 rows,
    /// `screen_cells()` needs one row-table allocation and no growth
    /// reallocations; beyond 4 rows, Vec doubling adds
    /// `ceil(log2(rows / 4))` reallocations.
    pub fn emit(&mut self, grid: &CellGrid, src: Rect, surface: &mut Surface, x: usize, y: usize) {
        self.rebuild();
        let (surface_cols, surface_rows) = surface.dimensions();
        let cols = src
            .cols
            .min(grid.cols().saturating_sub(src.x))
            .min(surface_cols.saturating_sub(x));
        let rows = src
            .rows
            .min(grid.rows().saturating_sub(src.y))
            .min(surface_rows.saturating_sub(y));
        let mut lines = surface.screen_cells();
        for row in 0..rows {
            let mut previous_style = None;
            let Some(dest_row) = lines.get_mut(y + row) else {
                break;
            };
            if cols > 0 && x > 0 && dest_row[x - 1].width() > 1 {
                let old_attrs = dest_row[x - 1].attrs().clone();
                dest_row[x - 1] = Cell::blank_with_attrs(old_attrs.clone());
                dest_row[x] = Cell::blank_with_attrs(old_attrs);
            }
            for col in 0..cols {
                let Some(cell) = grid.get(src.x + col, src.y + row) else {
                    break;
                };
                let style = (cell.fg, cell.bg, cell.attr);
                if previous_style != Some(style) {
                    self.attrs.set_foreground(self.color(cell.fg));
                    self.attrs.set_background(if cell.bg == StyleTok::INHERIT {
                        self.color(
                            StyleTok::from_tok(Tok::Slot(chrome::S::Panel))
                                .unwrap_or(StyleTok::INHERIT),
                        )
                    } else {
                        self.color(cell.bg)
                    });
                    self.attrs
                        .set_intensity(if cell.attr & super::grid::BOLD != 0 {
                            Intensity::Bold
                        } else if cell.attr & super::grid::DIM != 0 {
                            Intensity::Half
                        } else {
                            Intensity::Normal
                        });
                    self.attrs.set_italic(cell.attr & super::grid::ITALIC != 0);
                    self.attrs
                        .set_underline(if cell.attr & super::grid::UNDER != 0 {
                            Underline::Single
                        } else {
                            Underline::None
                        });
                    previous_style = Some(style);
                    #[cfg(test)]
                    {
                        self.text_runs += 1;
                    }
                }
                let ch = if cell.ch == '\0'
                    || (UnicodeWidthChar::width(cell.ch) == Some(2) && col + 1 == cols)
                {
                    ' '
                } else {
                    cell.ch
                };
                let mut encoded = [0; 4];
                let text = ch.encode_utf8(&mut encoded);
                let dest = &mut dest_row[x + col];
                if dest.attrs() != &self.attrs || dest.str() != text {
                    *dest = Cell::new(ch, self.attrs.clone());
                }
            }
        }
    }
    #[cfg(test)]
    pub(crate) fn rebuilds(&self) -> usize {
        self.cache.rebuilds
    }
    #[cfg(test)]
    pub(super) fn text_runs(&self) -> usize {
        self.text_runs
    }
    #[cfg(test)]
    pub(super) fn bump_test_generation(&mut self) {
        if let Some(key) = &mut self.cache.key {
            key.generation = key.generation.wrapping_add(1);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn repeated_emits_reuse_the_resolved_style_cache() {
        let grid = CellGrid::new(4, 1);
        let mut surface = Surface::new(4, 1);
        let mut emitter = Emitter::default();
        for _ in 0..2 {
            emitter.emit(&grid, Rect::full(4, 1), &mut surface, 0, 0);
        }
        assert_eq!(emitter.rebuilds(), 1);
    }
}
