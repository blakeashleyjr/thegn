use super::{GraphOut, Paint, Resolved, default_palette, table};
use thegn_core::theme::RampId;
use thegn_core::viz_raster;

pub(crate) fn paint_mirrored(
    out: &mut GraphOut,
    up: &[f32],
    down: &[f32],
    w: usize,
    h: usize,
    up_ramp: RampId,
    down_ramp: RampId,
) {
    out.prepare(w, h);
    viz_raster::mirrored(up, down, table(), &mut out.cells);
    let palette = default_palette();
    let top_h = h.div_ceil(2);
    let up_paint = Paint::Ramp {
        id: up_ramp,
        warn: 0.7,
        crit: 0.9,
    };
    let down_paint = Paint::Ramp {
        id: down_ramp,
        warn: 0.7,
        crit: 0.9,
    };
    let upper = Resolved::new(&palette, up_paint, crate::caps::color_depth());
    let lower = Resolved::new(&palette, down_paint, crate::caps::color_depth());
    out.emit_rows(top_h, &upper, &lower);
}
