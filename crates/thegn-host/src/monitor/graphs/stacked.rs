use super::{GraphOut, Paint, Resolved, default_palette, table};
use thegn_core::theme::SeriesRole;
use thegn_core::viz_raster;

pub(crate) fn paint_stacked(
    out: &mut GraphOut,
    bands: &[&[f32]],
    roles: &'static [SeriesRole],
    w: usize,
    h: usize,
) {
    out.prepare(w, h);
    viz_raster::stacked(bands, table(), &mut out.cells);
    let palette = default_palette();
    let paint = Paint::Bands(roles);
    let resolved = Resolved::new(&palette, paint, crate::caps::color_depth());
    out.emit(&resolved);
}
