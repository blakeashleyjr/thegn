use super::{GraphOut, Paint, Resolved, table};
use thegn_core::theme::{Palette, RampId};
use thegn_core::viz_raster;

pub(crate) fn paint_spark(
    out: &mut GraphOut,
    vals: &[f32],
    w: usize,
    ramp: RampId,
    palette: &Palette,
) {
    let tail = &vals[vals.len().saturating_sub(w)..];
    viz_raster::sparkline(tail, table(), &mut out.cells);
    let paint = Paint::Ramp {
        id: ramp,
        warn: 0.7,
        crit: 0.9,
    };
    let resolved = Resolved::new(palette, paint, crate::caps::color_depth());
    out.emit(&resolved);
    let missing = w.saturating_sub(tail.len());
    if missing > 0 {
        let mut prefix_text = out.take_string(missing);
        prefix_text.clear();
        for _ in 0..missing {
            prefix_text.push(' ');
        }
        let prefix = crate::seg::seg(crate::seg::Tok::Slot(crate::chrome::S::Dim), prefix_text);
        if let Some(crate::seg::Line::Segs(segs)) = out.lines.first_mut() {
            segs.insert(0, prefix);
        }
    }
}
