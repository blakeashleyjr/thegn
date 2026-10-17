use super::{GraphOut, Paint, Resolved, default_palette, table};
use thegn_core::viz_raster;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum GraphKind {
    Area,
    Line,
    Band,
    Blocks,
}

pub(crate) fn paint_graph(
    out: &mut GraphOut,
    kind: GraphKind,
    vals: &[f32],
    lo: Option<&[f32]>,
    w: usize,
    h: usize,
    paint: Paint,
) {
    out.prepare(w, h);
    let raster_table = table();
    match kind {
        GraphKind::Area => viz_raster::area(vals, raster_table, &mut out.cells),
        GraphKind::Line => viz_raster::line(vals, raster_table, &mut out.cells),
        GraphKind::Band => match lo {
            Some(low) => viz_raster::band(low, vals, raster_table, &mut out.cells),
            None => viz_raster::area(vals, raster_table, &mut out.cells),
        },
        GraphKind::Blocks => {
            viz_raster::columns(vals, &raster_table.ramp8, raster_table, &mut out.cells)
        }
    }
    let palette = default_palette();
    let resolved = Resolved::new(&palette, paint, crate::caps::color_depth());
    out.emit(&resolved);
}
