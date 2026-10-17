use super::{GraphOut, Paint, Resolved, default_palette, table};
use thegn_core::theme::RampId;
use thegn_core::viz_raster;

pub(crate) fn paint_heatmap(out: &mut GraphOut, rows: &[&[f32]], ramp: RampId) {
    viz_raster::heatmap(rows, table(), &mut out.cells);
    let palette = default_palette();
    let paint = Paint::Ramp {
        id: ramp,
        warn: 0.7,
        crit: 0.9,
    };
    let resolved = Resolved::new(&palette, paint, crate::caps::color_depth());
    out.emit(&resolved);
}
