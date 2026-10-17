use super::grid::CellGrid;
use crate::{
    caps, chrome,
    compositor::Rect,
    seg::{StyleTok, Tok},
};
use std::collections::HashMap;
use termwiz::cell::{CellAttributes, Intensity, Underline};
use termwiz::color::ColorAttribute;
use termwiz::surface::{Change, Position, Surface};
use thegn_core::termcaps::ColorDepth;

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
    scratch: String,
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
    pub fn emit(&mut self, grid: &CellGrid, src: Rect, surface: &mut Surface, x: usize, y: usize) {
        self.rebuild();
        let cols = src.cols.min(grid.cols().saturating_sub(src.x));
        let rows = src.rows.min(grid.rows().saturating_sub(src.y));
        for row in 0..rows {
            let mut col = 0;
            while col < cols {
                let Some(cell) = grid.get(src.x + col, src.y + row) else {
                    break;
                };
                if cell.ch == '\0' {
                    col += 1;
                    continue;
                }
                let style = (cell.fg, cell.bg, cell.attr);
                self.scratch.clear();
                let mut end = col;
                while end < cols {
                    let Some(next) = grid.get(src.x + end, src.y + row) else {
                        break;
                    };
                    if (next.fg, next.bg, next.attr) != style {
                        break;
                    }
                    if next.ch != '\0' {
                        self.scratch.push(next.ch);
                    }
                    end += 1;
                }
                surface.add_change(Change::CursorPosition {
                    x: Position::Absolute(x + col),
                    y: Position::Absolute(y + row),
                });
                let mut attrs = CellAttributes::default();
                attrs.set_foreground(self.color(cell.fg));
                attrs.set_background(if cell.bg == StyleTok::INHERIT {
                    self.color(
                        StyleTok::from_tok(Tok::Slot(chrome::S::Panel))
                            .unwrap_or(StyleTok::INHERIT),
                    )
                } else {
                    self.color(cell.bg)
                });
                attrs.set_intensity(if cell.attr & super::grid::BOLD != 0 {
                    Intensity::Bold
                } else if cell.attr & super::grid::DIM != 0 {
                    Intensity::Half
                } else {
                    Intensity::Normal
                });
                attrs.set_italic(cell.attr & super::grid::ITALIC != 0);
                attrs.set_underline(if cell.attr & super::grid::UNDER != 0 {
                    Underline::Single
                } else {
                    Underline::None
                });
                surface.add_change(Change::AllAttributes(attrs));
                surface.add_change(Change::Text(self.scratch.clone()));
                #[cfg(test)]
                {
                    self.text_runs += 1;
                }
                col = end;
            }
        }
    }
    #[cfg(test)]
    pub(super) fn rebuilds(&self) -> usize {
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
