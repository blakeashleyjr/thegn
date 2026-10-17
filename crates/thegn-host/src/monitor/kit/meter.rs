use super::grid::{CellGrid, CellStyle, GridView};
use crate::{
    caps,
    compositor::Rect,
    seg::{StyleTok, Tok},
};
use thegn_core::{
    numfmt::{NumFmt, Unit},
    theme::{Band, RampId, band_of},
    viz_raster::{self, CellBuf},
};

#[derive(Default)]
pub struct Meter {
    raster: CellBuf,
}
pub struct MeterSpec {
    pub value: f32,
    pub warn: f32,
    pub crit: f32,
    pub ramp: RampId,
    pub label: bool,
    pub fmt: NumFmt,
}
impl Meter {
    pub fn paint(&mut self, grid: &mut CellGrid, rect: Rect, spec: MeterSpec) {
        let mut v = GridView::new(grid, rect);
        if v.rect().rows == 0 || v.rect().cols == 0 {
            return;
        }
        let label_width = if spec.label { 5.min(v.rect().cols) } else { 0 };
        let bar_width = v.rect().cols.saturating_sub(label_width);
        viz_raster::meter(
            spec.value,
            bar_width,
            caps::raster_table(),
            &mut self.raster,
        );
        let status = band_of(spec.value, spec.warn, spec.crit);
        let status_level = match status {
            Band::Normal => 3,
            Band::Warn => 5,
            Band::Crit => 7,
        };
        for x in 0..bar_width {
            if let Some(cell) = self.raster.row(0).get(x) {
                let style = CellStyle {
                    ch: cell.ch,
                    fg: StyleTok::from_tok(if cell.ch == caps::raster_table().track {
                        Tok::Slot(crate::chrome::S::Dim)
                    } else {
                        Tok::Ramp(spec.ramp, cell.fg)
                    })
                    .unwrap_or(StyleTok::INHERIT),
                    bg: StyleTok::INHERIT,
                    ..CellStyle::default()
                };
                v.put(x, 0, style);
            }
        }
        if spec.label && label_width > 0 {
            let text = spec
                .fmt
                .fmt(f64::from(spec.value) * 100.0, Unit::Percent, label_width);
            let style = CellStyle {
                fg: StyleTok::from_tok(Tok::Ramp(spec.ramp, status_level))
                    .unwrap_or(StyleTok::INHERIT),
                ..CellStyle::default()
            };
            v.put_str(bar_width, 0, &text, style);
        }
    }
}
