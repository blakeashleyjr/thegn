use super::{
    GraphOut, Paint, Resolved, palette_resolution_count, reset_palette_cache, table, with_palette,
};
use crate::seg::Line;
use thegn_core::termcaps::ColorDepth;
use thegn_core::theme::RampId;
use thegn_core::viz_raster;

#[test]
fn unchanged_palette_generation_is_resolved_once() {
    reset_palette_cache();
    with_palette(|_| {});
    with_palette(|_| {});
    assert_eq!(palette_resolution_count(), 1);
}

#[test]
fn graph_run_strings_keep_capacity_when_run_count_falls() {
    let mut out = GraphOut::new();
    let resolved = with_palette(|palette| {
        Resolved::new(
            palette,
            Paint::Ramp {
                id: RampId::Util,
                warn: 0.7,
                crit: 0.9,
            },
            ColorDepth::Truecolor,
        )
    });
    out.prepare(16, 1);
    viz_raster::line(&[0.0, 1.0, 0.0, 1.0, 0.0, 1.0], table(), &mut out.cells);
    out.emit(&resolved);
    let Line::Segs(runs) = &out.lines[0] else {
        panic!("expected emitted graph runs")
    };
    let capacities: Vec<_> = runs.iter().map(|run| run.text.capacity()).collect();
    let run_count = runs.len();

    out.prepare(16, 1);
    viz_raster::line(&[0.5, 0.5, 0.5, 0.5, 0.5, 0.5], table(), &mut out.cells);
    out.emit(&resolved);

    let Line::Segs(runs) = &out.lines[0] else {
        panic!("expected emitted graph runs")
    };
    assert_eq!(
        runs.len(),
        run_count,
        "run storage remains at its high-water mark"
    );
    assert_eq!(
        runs.iter()
            .map(|run| run.text.capacity())
            .collect::<Vec<_>>(),
        capacities,
        "existing run String allocations should be reused"
    );
}
