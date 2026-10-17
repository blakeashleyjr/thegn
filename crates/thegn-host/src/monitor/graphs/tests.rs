use super::graph::{GraphKind, paint_graph};
use super::heatmap::paint_heatmap;
use super::mirrored::paint_mirrored;
use super::spark::paint_spark;
use super::stacked::paint_stacked;
use super::{
    GraphOut, Paint, Resolved, default_palette, reset_resolution_count, resolution_count, table,
};
use crate::caps::test_override;
use crate::chrome;
use crate::seg::{Line, Tok};
use termwiz::color::ColorAttribute;
use thegn_core::termcaps::{ColorDepth, UnicodeLevel};
use thegn_core::theme::{RampId, SeriesRole};
use thegn_core::viz_raster;

fn text(out: &GraphOut) -> Vec<String> {
    out.lines
        .iter()
        .map(|line| match line {
            Line::Segs(segs) => segs
                .iter()
                .map(|seg| seg.text.as_str())
                .collect::<Vec<_>>()
                .join(""),
            _ => String::new(),
        })
        .collect()
}

fn assert_cell_styles(out: &GraphOut, cells: &viz_raster::CellBuf, resolved: &Resolved) {
    for (row_index, line) in out.lines.iter().enumerate() {
        let Line::Segs(segs) = line else {
            panic!("expected emitted runs")
        };
        let row = cells.row(row_index);
        let mut offset = 0;
        for run in segs {
            let width = crate::seg::seg_width(std::slice::from_ref(run));
            let expected = resolved.cell(row[offset]);
            assert_eq!(run.fg, Tok::Attr(expected.fg));
            assert_eq!(run.bg, expected.bg.map(Tok::Attr));
            assert_eq!(run.bold, expected.bold);
            assert!(
                row[offset..offset + width]
                    .iter()
                    .all(|cell| resolved.cell(*cell) == expected)
            );
            offset += width;
        }
        assert_eq!(offset, cells.width());
    }
}

#[test]
fn graph_glyph_and_style_goldens_follow_each_unicode_table() {
    for level in [UnicodeLevel::Full, UnicodeLevel::Basic, UnicodeLevel::Ascii] {
        test_override::with_unicode(level, || {
            let mut out = GraphOut::new();
            paint_graph(
                &mut out,
                GraphKind::Area,
                &[0.15, 0.4, 0.8, 1.0],
                None,
                4,
                2,
                Paint::Ramp {
                    id: RampId::Util,
                    warn: 0.6,
                    crit: 0.9,
                },
            );
            let expected_table = viz_raster::RasterTable::for_level(level);
            let mut expected = viz_raster::CellBuf::new();
            viz_raster::area(&[0.15, 0.4, 0.8, 1.0], expected_table, &mut expected);
            let expected_rows: Vec<String> = (0..expected.height())
                .map(|row| expected.row(row).iter().map(|cell| cell.ch).collect())
                .collect();
            assert_eq!(text(&out), expected_rows);
            let palette = default_palette();
            let paint = Paint::Ramp {
                id: RampId::Util,
                warn: 0.6,
                crit: 0.9,
            };
            let resolved = Resolved::new(&palette, paint, ColorDepth::Truecolor);
            assert_cell_styles(&out, &expected, &resolved);
            for (kind, vals, lo) in [
                (GraphKind::Line, &[0.2, 0.5, 0.8][..], None),
                (GraphKind::Blocks, &[0.2, 0.5, 0.8][..], None),
                (
                    GraphKind::Band,
                    &[0.7, 0.8, 0.9][..],
                    Some(&[0.1, 0.3, 0.5][..]),
                ),
            ] {
                paint_graph(
                    &mut out,
                    kind,
                    vals,
                    lo,
                    3,
                    2,
                    Paint::Flat(SeriesRole::CpuUser),
                );
                assert_eq!(out.lines.len(), 2);
                assert_eq!(out.cells.width(), 3);
                assert!(out.lines.iter().all(|line| matches!(line, Line::Segs(_))));
            }

            paint_stacked(
                &mut out,
                &[&[0.4, 0.4], &[0.4, 0.4]],
                &[SeriesRole::CpuUser, SeriesRole::CpuSys],
                2,
                4,
            );
            assert_eq!(out.lines.len(), 4);
            paint_mirrored(
                &mut out,
                &[0.8, 0.8],
                &[0.2, 0.2],
                2,
                4,
                RampId::Rx,
                RampId::Tx,
            );
            assert_eq!(out.lines.len(), 4);
            paint_heatmap(&mut out, &[&[0.1, 0.8], &[0.6, 0.4]], RampId::Heat);
            assert_eq!(out.cells.width(), 2);
            paint_spark(&mut out, &[0.1, 0.5, 0.8], 2, RampId::Mem);
            assert_eq!(text(&out)[0].chars().count(), 2);
            paint_spark(&mut out, &[0.8], 3, RampId::Mem);
            assert_eq!(text(&out)[0].chars().count(), 3);
            assert!(text(&out)[0].starts_with(' '));
        });
    }
}

#[test]
fn color_depth_resolves_ramps_once_and_degrades_without_color() {
    let palette = default_palette();
    let vals = [0.0, 0.14, 0.28, 0.42, 0.56, 0.7, 0.84, 1.0];
    for depth in [
        ColorDepth::Truecolor,
        ColorDepth::Ansi256,
        ColorDepth::Ansi16,
        ColorDepth::None,
    ] {
        crate::caps::test_override::with_color(depth, || {
            reset_resolution_count();
            let mut out = GraphOut::new();
            paint_graph(
                &mut out,
                GraphKind::Area,
                &vals,
                None,
                vals.len(),
                4,
                Paint::Ramp {
                    id: RampId::Util,
                    warn: 0.4,
                    crit: 0.8,
                },
            );
            assert_eq!(resolution_count(), 1);
            assert!(!out.lines.is_empty());
            if depth == ColorDepth::None {
                assert!(
                    out.lines
                        .iter()
                        .filter_map(|line| match line {
                            Line::Segs(segs) => Some(segs),
                            _ => None,
                        })
                        .flatten()
                        .all(|seg| matches!(seg.fg, Tok::Attr(ColorAttribute::Default)))
                );
            }
        });
    }
    let truecolor = Resolved::new(
        &palette,
        Paint::Ramp {
            id: RampId::Util,
            warn: 0.4,
            crit: 0.8,
        },
        ColorDepth::Truecolor,
    );
    let ansi256 = Resolved::new(
        &palette,
        Paint::Ramp {
            id: RampId::Util,
            warn: 0.4,
            crit: 0.8,
        },
        ColorDepth::Ansi256,
    );
    let ansi16 = Resolved::new(
        &palette,
        Paint::Ramp {
            id: RampId::Util,
            warn: 0.4,
            crit: 0.8,
        },
        ColorDepth::Ansi16,
    );
    let mono = Resolved::new(
        &palette,
        Paint::Ramp {
            id: RampId::Util,
            warn: 0.4,
            crit: 0.8,
        },
        ColorDepth::None,
    );
    let unique_colors = |resolved: &Resolved| {
        resolved
            .levels
            .iter()
            .fold(Vec::new(), |mut colors, style| {
                if !colors.contains(&style.fg) {
                    colors.push(style.fg);
                }
                colors
            })
            .len()
    };
    assert!(unique_colors(&truecolor) > 1);
    assert!(unique_colors(&ansi256) > 1);
    assert!(unique_colors(&ansi16) <= 3);
    assert_eq!(ansi16.levels[0].fg, ansi16.levels[2].fg);
    assert_eq!(ansi16.levels[3].fg, ansi16.levels[5].fg);
    assert_eq!(ansi16.levels[6].fg, ansi16.levels[7].fg);
    for (level, style) in mono.levels.iter().enumerate() {
        assert_eq!(style.bold, level >= 6);
        assert_eq!(style.fg, ColorAttribute::Default);
    }
}

#[test]
fn stacked_boundary_uses_lower_foreground_and_upper_background_roles() {
    crate::caps::test_override::with_unicode(UnicodeLevel::Full, || {
        crate::caps::test_override::with_color(ColorDepth::Truecolor, || {
            let mut out = GraphOut::new();
            let roles = &[SeriesRole::CpuUser, SeriesRole::CpuSys];
            paint_stacked(&mut out, &[&[0.45], &[0.45]], roles, 1, 4);
            let boundary = out
                .cells
                .row(2)
                .iter()
                .find(|cell| cell.bg.is_some())
                .copied();
            assert!(boundary.is_some());
            let row = out.lines.get(2).and_then(|line| match line {
                Line::Segs(segs) => segs.first(),
                _ => None,
            });
            let Some(run) = row else {
                panic!("expected a stacked boundary run")
            };
            assert_eq!(
                run.fg,
                Tok::Attr(chrome::theme_color(default_palette().series(roles[0])))
            );
            assert_eq!(
                run.bg,
                Some(Tok::Attr(chrome::theme_color(
                    default_palette().series(roles[1])
                )))
            );
        });
    });
}

#[test]
fn mirrored_halves_resolve_through_their_own_ramps() {
    crate::caps::test_override::with_unicode(UnicodeLevel::Full, || {
        crate::caps::test_override::with_color(ColorDepth::Truecolor, || {
            let mut out = GraphOut::new();
            paint_mirrored(&mut out, &[1.0], &[1.0], 1, 4, RampId::Rx, RampId::Tx);
            let palette = default_palette();
            let up = Resolved::new(
                &palette,
                Paint::Ramp {
                    id: RampId::Rx,
                    warn: 0.7,
                    crit: 0.9,
                },
                ColorDepth::Truecolor,
            );
            let down = Resolved::new(
                &palette,
                Paint::Ramp {
                    id: RampId::Tx,
                    warn: 0.7,
                    crit: 0.9,
                },
                ColorDepth::Truecolor,
            );
            let upper = match &out.lines[0] {
                Line::Segs(segs) => segs[0].fg,
                _ => panic!("expected row"),
            };
            let lower = match &out.lines[3] {
                Line::Segs(segs) => segs[0].fg,
                _ => panic!("expected row"),
            };
            assert_eq!(upper, Tok::Attr(up.levels[7].fg));
            assert_eq!(lower, Tok::Attr(down.levels[7].fg));
        });
    });
}

#[test]
fn same_size_repaint_reuses_rows_and_emits_exact_width_runs() {
    let mut out = GraphOut::new();
    let paint = Paint::Flat(SeriesRole::CpuUser);
    let palette = default_palette();
    let resolved = Resolved::new(&palette, paint, ColorDepth::Truecolor);
    out.prepare(12, 3);
    viz_raster::area(&[0.2, 0.8], table(), &mut out.cells);
    out.emit(&resolved);
    let capacity = out.lines.capacity();
    out.prepare(12, 3);
    viz_raster::area(&[0.3, 0.7], table(), &mut out.cells);
    out.emit(&resolved);
    assert_eq!(out.cells.width(), 12);
    assert_eq!(out.cells.height(), 3);
    assert_eq!(out.lines.capacity(), capacity);
    for line in &out.lines {
        let Line::Segs(segs) = line else {
            panic!("expected emitted runs")
        };
        assert!(segs.len() <= 12);
        assert_eq!(crate::seg::seg_width(segs), 12);
    }
}
