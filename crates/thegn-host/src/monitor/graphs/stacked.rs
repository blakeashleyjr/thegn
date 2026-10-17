use super::{GraphOut, Paint, Resolved, table};
use thegn_core::theme::{Palette, SeriesRole};
use thegn_core::viz_raster;

pub(crate) fn paint_stacked(
    out: &mut GraphOut,
    bands: &[&[f32]],
    roles: &'static [SeriesRole],
    w: usize,
    h: usize,
    palette: &Palette,
) {
    out.prepare(w, h);
    viz_raster::stacked(bands, table(), &mut out.cells);
    let paint = Paint::Bands(roles);
    let resolved = Resolved::new(palette, paint, crate::caps::color_depth());
    out.emit(&resolved);
}
