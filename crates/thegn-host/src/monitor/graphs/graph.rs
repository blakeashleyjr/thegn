use super::{GraphOut, Paint, Resolved, table};
use thegn_core::theme::Palette;
use thegn_core::viz_raster;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum GraphKind {
    Area,
    Line,
    Band,
    Blocks,
}

pub(crate) struct GraphPaint<'a> {
    pub(crate) kind: GraphKind,
    pub(crate) vals: &'a [f32],
    pub(crate) lo: Option<&'a [f32]>,
    pub(crate) w: usize,
    pub(crate) h: usize,
    pub(crate) paint: Paint,
}

pub(crate) fn paint_graph(out: &mut GraphOut, graph: GraphPaint<'_>, palette: &Palette) {
    out.prepare(graph.w, graph.h);
    let raster_table = table();
    match graph.kind {
        GraphKind::Area => viz_raster::area(graph.vals, raster_table, &mut out.cells),
        GraphKind::Line => viz_raster::line(graph.vals, raster_table, &mut out.cells),
        GraphKind::Band => match graph.lo {
            Some(low) => viz_raster::band(low, graph.vals, raster_table, &mut out.cells),
            None => viz_raster::area(graph.vals, raster_table, &mut out.cells),
        },
        GraphKind::Blocks => viz_raster::columns(
            graph.vals,
            &raster_table.ramp8,
            raster_table,
            &mut out.cells,
        ),
    }
    let resolved = Resolved::new(palette, graph.paint, crate::caps::color_depth());
    out.emit(&resolved);
}
