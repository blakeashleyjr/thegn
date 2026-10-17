use super::{GraphOut, Paint, Resolved, default_palette, table};
use thegn_core::theme::RampId;
use thegn_core::viz_raster;

pub(crate) fn paint_spark(out: &mut GraphOut, vals: &[f32], w: usize, ramp: RampId) {
    let tail = &vals[vals.len().saturating_sub(w)..];
    viz_raster::sparkline(tail, table(), &mut out.cells);
    let palette = default_palette();
    let paint = Paint::Ramp {
        id: ramp,
        warn: 0.7,
        crit: 0.9,
    };
    let resolved = Resolved::new(&palette, paint, crate::caps::color_depth());
    out.emit(&resolved);
    let missing = w.saturating_sub(tail.len());
    if missing > 0 {
        let prefix = crate::seg::seg(
            crate::seg::Tok::Slot(crate::chrome::S::Dim),
            " ".repeat(missing),
        );
        if let Some(crate::seg::Line::Segs(segs)) = out.lines.first_mut() {
            segs.insert(0, prefix);
        }
    }
}
