use super::{
    GraphOut, Paint, Resolved, palette_resolution_count, reset_palette_cache, table, with_palette,
};
use crate::seg::{Line, Tok};
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
fn palette_generation_invalidation_updates_painted_ramp_color() {
    let original = crate::chrome::palette_snapshot();
    reset_palette_cache();
    with_palette(|_| {});
    let mut changed = original.clone();
    changed.hues.red = "1;2;3".to_owned();
    crate::chrome::set_palette(changed);

    let mut out = GraphOut::new();
    let expected = with_palette(|palette| {
        let resolved = Resolved::new(
            palette,
            Paint::Ramp {
                id: RampId::Util,
                warn: 0.7,
                crit: 0.9,
            },
            ColorDepth::Truecolor,
        );
        out.prepare(1, 1);
        viz_raster::line(&[1.0], table(), &mut out.cells);
        out.emit(&resolved);
        resolved.cell(out.cells.row(0)[0]).fg
    });
    assert_eq!(palette_resolution_count(), 2);
    let Line::Segs(runs) = &out.lines[0] else {
        panic!("expected painted graph run")
    };
    assert_eq!(runs[0].fg, Tok::Attr(expected));
    crate::chrome::set_palette(original);
}

#[test]
fn cached_palette_resolves_current_color_depth_on_each_paint() {
    with_palette(|palette| {
        for depth in [ColorDepth::Ansi16, ColorDepth::Truecolor] {
            crate::caps::test_override::with_color(depth, || {
                let paint = Paint::Ramp {
                    id: RampId::Util,
                    warn: 0.7,
                    crit: 0.9,
                };
                let resolved = Resolved::new(palette, paint, crate::caps::color_depth());
                let expected = Resolved::new(
                    &crate::chrome::palette_snapshot(),
                    paint,
                    crate::caps::color_depth(),
                );
                let mut out = GraphOut::new();
                out.prepare(1, 1);
                viz_raster::line(&[1.0], table(), &mut out.cells);
                out.emit(&resolved);
                let Line::Segs(runs) = &out.lines[0] else {
                    panic!("expected painted graph run")
                };
                assert_eq!(runs[0].fg, Tok::Attr(expected.cell(out.cells.row(0)[0]).fg));
            });
        }
    });
}

fn emit_shape(out: &mut GraphOut, values: &[f32], resolved: &Resolved) {
    out.prepare(4, 1);
    viz_raster::area(values, &viz_raster::RasterTable::FULL, &mut out.cells);
    out.emit(resolved);
}

fn text(out: &GraphOut) -> Vec<String> {
    out.lines
        .iter()
        .map(|line| match line {
            Line::Segs(segs) => segs.iter().map(|seg| seg.text.as_str()).collect::<String>(),
            _ => String::new(),
        })
        .collect()
}

#[test]
fn graph_run_strings_reuse_storage_after_run_count_falls_and_recovers() {
    let resolved = with_palette(|palette| {
        Resolved::new(
            palette,
            Paint::Ramp {
                id: RampId::Util,
                warn: 0.7,
                crit: 0.9,
            },
            ColorDepth::None,
        )
    });
    let many = [0.0, 0.0, 1.0, 1.0, 0.0, 0.0, 1.0, 1.0];
    let few = [0.5; 8];
    let mut out = GraphOut::new();
    emit_shape(&mut out, &many, &resolved);
    let Line::Segs(many_runs) = &out.lines[0] else {
        panic!("expected shape A graph runs")
    };
    let many_count = many_runs.len();
    emit_shape(&mut out, &few, &resolved);
    let Line::Segs(few_runs) = &out.lines[0] else {
        panic!("expected shape B graph runs")
    };
    assert!(
        few_runs.len() < many_count,
        "shape B must have fewer live runs than shape A"
    );
    let (_, counts) = crate::proc_workload_alloc::measure(|| {
        emit_shape(&mut out, &many, &resolved);
    });
    let mut fresh = GraphOut::new();
    emit_shape(&mut fresh, &many, &resolved);
    assert_eq!(
        text(&out),
        text(&fresh),
        "reused runs contain no stale content"
    );
    let (Line::Segs(reused_runs), Line::Segs(fresh_runs)) = (&out.lines[0], &fresh.lines[0]) else {
        panic!("expected emitted graph runs")
    };
    assert_eq!(
        reused_runs.len(),
        fresh_runs.len(),
        "emitted Line keeps only live runs"
    );
    assert_eq!(
        counts.allocation_calls, 0,
        "shape A after A → B allocated: {counts:?}"
    );
    assert_eq!(
        counts.realloc_calls, 0,
        "shape A after A → B reallocated: {counts:?}"
    );
}

#[test]
fn short_sparkline_paints_keep_exact_runs_and_reuse_graph_storage() {
    let values = [0.25, 0.75];
    let mut surface = termwiz::surface::Surface::new(12, 1);
    for _ in 0..3 {
        super::paint_sparkline(
            &values,
            8,
            RampId::Util,
            crate::compositor::Rect::full(12, 1),
            &mut surface,
        );
    }
    let run_count = super::GRAPH_OUT.with(|scratch| {
        let out = scratch.borrow();
        let Line::Segs(segs) = &out.lines[0] else {
            panic!("expected sparkline row")
        };
        segs.len()
    });
    for _ in 0..1_000 {
        super::paint_sparkline(
            &values,
            8,
            RampId::Util,
            crate::compositor::Rect::full(12, 1),
            &mut surface,
        );
    }
    let final_count = super::GRAPH_OUT.with(|scratch| {
        let out = scratch.borrow();
        let Line::Segs(segs) = &out.lines[0] else {
            panic!("expected sparkline row")
        };
        segs.len()
    });
    assert_eq!(
        final_count, run_count,
        "sparkline run count grew across repeated paints"
    );
}
