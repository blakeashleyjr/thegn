use super::{GraphOut, Paint, Resolved, table};
use thegn_core::theme::{Palette, RampId};
use thegn_core::viz_raster;

pub(crate) fn paint_heatmap(out: &mut GraphOut, rows: &[&[f32]], ramp: RampId, palette: &Palette) {
    viz_raster::heatmap(rows, table(), &mut out.cells);
    let paint = Paint::Ramp {
        id: ramp,
        warn: 0.7,
        crit: 0.9,
    };
    let resolved = Resolved::new(palette, paint, crate::caps::color_depth());
    out.emit(&resolved);
}
