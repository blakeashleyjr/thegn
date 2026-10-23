use super::graph::{GraphKind, GraphPaint, paint_graph};
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
    assert_cell_styles_rows(out, cells, resolved, 0, out.lines.len());
}

fn assert_cell_styles_rows(
    out: &GraphOut,
    cells: &viz_raster::CellBuf,
    resolved: &Resolved,
    start_row: usize,
    end_row: usize,
) {
    for row_index in start_row..end_row {
        let line = &out.lines[row_index];
        let Line::Segs(segs) = line else {
            panic!("expected emitted runs")
        };
        let row = cells.row(row_index);
        let mut offset = 0;
        let mut previous_style = None;
        for run in segs {
            let width = crate::seg::seg_width(std::slice::from_ref(run));
            let expected = resolved.cell(row[offset]);
            assert_ne!(previous_style, Some(expected), "adjacent equal-style runs");
            assert_eq!(run.fg, Tok::Attr(expected.fg));
            assert_eq!(run.bg, expected.bg.map(Tok::Attr));
            assert_eq!(run.bold, expected.bold);
            assert!(
                row[offset..offset + width]
                    .iter()
                    .all(|cell| resolved.cell(*cell) == expected)
            );
            offset += width;
            previous_style = Some(expected);
        }
        assert_eq!(offset, cells.width());
    }
}

#[test]
fn graph_glyph_and_style_goldens_follow_each_unicode_table() {
    for level in [UnicodeLevel::Full, UnicodeLevel::Basic, UnicodeLevel::Ascii] {
        test_override::with_unicode(level, || {
            test_override::with_color(ColorDepth::Truecolor, || {
                let palette = default_palette();
                let raster_table = viz_raster::RasterTable::for_level(level);
                let mut out = GraphOut::new();
                paint_graph(
                    &mut out,
                    GraphPaint {
                        kind: GraphKind::Area,
                        vals: &[0.15, 0.4, 0.8, 1.0],
                        lo: None,
                        w: 4,
                        h: 2,
                        paint: Paint::Ramp {
                            id: RampId::Util,
                            warn: 0.6,
                            crit: 0.9,
                        },
                    },
                    &palette,
                );
                let mut expected = viz_raster::CellBuf::new();
                expected.reset(4, 2);
                viz_raster::area(&[0.15, 0.4, 0.8, 1.0], raster_table, &mut expected);
                assert_eq!(text(&out), raster_text(&expected));
                let area_resolved = Resolved::new(
                    &palette,
                    Paint::Ramp {
                        id: RampId::Util,
                        warn: 0.6,
                        crit: 0.9,
                    },
                    ColorDepth::Truecolor,
                );
                assert_cell_styles(&out, &expected, &area_resolved);

                let graph_cases = [
                    (GraphKind::Line, &[0.2, 0.5, 0.8][..], None),
                    (GraphKind::Blocks, &[0.2, 0.5, 0.8][..], None),
                    (
                        GraphKind::Band,
                        &[0.7, 0.8, 0.9][..],
                        Some(&[0.1, 0.3, 0.5][..]),
                    ),
                ];
                for (kind, vals, lo) in graph_cases {
                    let paint = Paint::Flat(SeriesRole::CpuUser);
                    paint_graph(
                        &mut out,
                        GraphPaint {
                            kind,
                            vals,
                            lo,
                            w: 3,
                            h: 2,
                            paint,
                        },
                        &palette,
                    );
                    let mut expected = viz_raster::CellBuf::new();
                    expected.reset(3, 2);
                    match kind {
                        GraphKind::Line => viz_raster::line(vals, raster_table, &mut expected),
                        GraphKind::Blocks => viz_raster::columns(
                            vals,
                            &raster_table.ramp8,
                            raster_table,
                            &mut expected,
                        ),
                        GraphKind::Band => {
                            if let Some(low) = lo {
                                viz_raster::band(low, vals, raster_table, &mut expected);
                            }
                        }
                        GraphKind::Area => unreachable!(),
                    }
                    let resolved = Resolved::new(&palette, paint, ColorDepth::Truecolor);
                    assert_eq!(text(&out), raster_text(&expected));
                    assert_cell_styles(&out, &expected, &resolved);
                }

                let stacked_bands = [&[0.4, 0.4][..], &[0.4, 0.4][..]];
                let stacked_roles = &[SeriesRole::CpuUser, SeriesRole::CpuSys];
                paint_stacked(&mut out, &stacked_bands, stacked_roles, 2, 4, &palette);
                let mut expected = viz_raster::CellBuf::new();
                expected.reset(2, 4);
                viz_raster::stacked(&stacked_bands, raster_table, &mut expected);
                let stacked_resolved =
                    Resolved::new(&palette, Paint::Bands(stacked_roles), ColorDepth::Truecolor);
                assert_eq!(text(&out), raster_text(&expected));
                assert_cell_styles(&out, &expected, &stacked_resolved);

                let up = [0.8, 0.8];
                let down = [0.2, 0.2];
                let ramps = (RampId::Rx, RampId::Tx);
                paint_mirrored(&mut out, &up, &down, 2, 4, ramps, &palette);
                let mut expected = viz_raster::CellBuf::new();
                expected.reset(2, 4);
                viz_raster::mirrored(&up, &down, raster_table, &mut expected);
                let up_resolved = Resolved::new(
                    &palette,
                    Paint::Ramp {
                        id: ramps.0,
                        warn: 0.7,
                        crit: 0.9,
                    },
                    ColorDepth::Truecolor,
                );
                let down_resolved = Resolved::new(
                    &palette,
                    Paint::Ramp {
                        id: ramps.1,
                        warn: 0.7,
                        crit: 0.9,
                    },
                    ColorDepth::Truecolor,
                );
                assert_eq!(text(&out), raster_text(&expected));
                assert_cell_styles_rows(&out, &expected, &up_resolved, 0, 2);
                assert_cell_styles_rows(&out, &expected, &down_resolved, 2, 4);

                let heat_rows = [&[0.1, 0.8][..], &[0.6, 0.4][..]];
                paint_heatmap(&mut out, &heat_rows, RampId::Heat, &palette);
                let mut expected = viz_raster::CellBuf::new();
                viz_raster::heatmap(&heat_rows, raster_table, &mut expected);
                let heat_resolved = Resolved::new(
                    &palette,
                    Paint::Ramp {
                        id: RampId::Heat,
                        warn: 0.7,
                        crit: 0.9,
                    },
                    ColorDepth::Truecolor,
                );
                assert_eq!(text(&out), raster_text(&expected));
                assert_cell_styles(&out, &expected, &heat_resolved);

                paint_spark(&mut out, &[0.1, 0.5, 0.8], 2, RampId::Mem, &palette);
                let mut expected = viz_raster::CellBuf::new();
                viz_raster::sparkline(&[0.5, 0.8], raster_table, &mut expected);
                let spark_resolved = Resolved::new(
                    &palette,
                    Paint::Ramp {
                        id: RampId::Mem,
                        warn: 0.7,
                        crit: 0.9,
                    },
                    ColorDepth::Truecolor,
                );
                assert_eq!(text(&out), raster_text(&expected));
                assert_cell_styles(&out, &expected, &spark_resolved);

                paint_spark(&mut out, &[0.8], 3, RampId::Mem, &palette);
                assert_eq!(text(&out)[0].chars().count(), 3);
                assert!(text(&out)[0].starts_with(' '));
            });
        });
    }
}

#[test]
fn host_graph_entry_points_match_hand_written_ascii_rows() {
    let palette = default_palette();
    crate::caps::test_override::with_unicode(UnicodeLevel::Ascii, || {
        crate::caps::test_override::with_color(ColorDepth::None, || {
            let mut history = termwiz::surface::Surface::new(4, 2);
            super::paint_history_graph(
                &[0.5, 1.0],
                RampId::Util,
                crate::compositor::Rect {
                    x: 0,
                    y: 0,
                    cols: 4,
                    rows: 2,
                },
                &mut history,
            );
            let history_rows: Vec<String> = history
                .screen_lines()
                .iter()
                .map(|line| {
                    line.visible_cells()
                        .map(|cell| cell.str().to_owned())
                        .collect()
                })
                .collect();
            assert_eq!(history_rows, ["   #", "  ##"]);

            let mut spark = termwiz::surface::Surface::new(4, 1);
            super::paint_sparkline(
                &[0.0, 1.0],
                4,
                RampId::Mem,
                crate::compositor::Rect {
                    x: 0,
                    y: 0,
                    cols: 4,
                    rows: 1,
                },
                &mut spark,
            );
            let spark_rows: Vec<String> = spark
                .screen_lines()
                .iter()
                .map(|line| {
                    line.visible_cells()
                        .map(|cell| cell.str().to_owned())
                        .collect()
                })
                .collect();
            assert_eq!(spark_rows, ["  _#"]);

            let mut heat = GraphOut::new();
            let heat_rows = [&[0.0, 0.5, 1.0][..], &[0.5][..]];
            paint_heatmap(&mut heat, &heat_rows, RampId::Heat, &palette);
            assert_eq!(text(&heat), [" +@", "  +"]);
        });
    });
}

#[test]
fn host_history_graph_paints_braille_golden() {
    crate::caps::test_override::with_unicode(UnicodeLevel::Full, || {
        crate::caps::test_override::with_color(ColorDepth::None, || {
            let mut history = termwiz::surface::Surface::new(1, 1);
            super::paint_history_graph(
                &[1.0],
                RampId::Util,
                crate::compositor::Rect {
                    x: 0,
                    y: 0,
                    cols: 1,
                    rows: 1,
                },
                &mut history,
            );
            let history_rows: Vec<String> = history
                .screen_lines()
                .iter()
                .map(|line| {
                    line.visible_cells()
                        .map(|cell| cell.str().to_owned())
                        .collect()
                })
                .collect();
            assert_eq!(history_rows, ["⢸"]);
        });
    });
}

fn raster_text(cells: &viz_raster::CellBuf) -> Vec<String> {
    (0..cells.height())
        .map(|row| cells.row(row).iter().map(|cell| cell.ch).collect())
        .collect()
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
                GraphPaint {
                    kind: GraphKind::Area,
                    vals: &vals,
                    lo: None,
                    w: vals.len(),
                    h: 4,
                    paint: Paint::Ramp {
                        id: RampId::Util,
                        warn: 0.4,
                        crit: 0.8,
                    },
                },
                &palette,
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
            let palette = default_palette();
            paint_stacked(&mut out, &[&[0.45], &[0.45]], roles, 1, 4, &palette);
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
                Tok::Attr(chrome::theme_color(palette.series(roles[0])))
            );
            assert_eq!(
                run.bg,
                Some(Tok::Attr(chrome::theme_color(palette.series(roles[1]))))
            );
        });
    });
}

#[test]
fn mirrored_halves_resolve_through_their_own_ramps() {
    crate::caps::test_override::with_unicode(UnicodeLevel::Full, || {
        crate::caps::test_override::with_color(ColorDepth::Truecolor, || {
            let mut out = GraphOut::new();
            let palette = default_palette();
            paint_mirrored(
                &mut out,
                &[1.0],
                &[1.0],
                1,
                4,
                (RampId::Rx, RampId::Tx),
                &palette,
            );
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
fn heatmap_half_block_uses_upper_foreground_and_lower_background() {
    let palette = default_palette();
    let rows = [&[0.2][..], &[0.8][..]];
    crate::caps::test_override::with_unicode(UnicodeLevel::Full, || {
        crate::caps::test_override::with_color(ColorDepth::Truecolor, || {
            let mut out = GraphOut::new();
            paint_heatmap(&mut out, &rows, RampId::Heat, &palette);
            let upper = (0.2_f32 * 7.0).round() as usize;
            let lower = (0.8_f32 * 7.0).round() as usize;
            let Line::Segs(runs) = &out.lines[0] else {
                panic!("expected heatmap row")
            };
            assert_eq!(runs.len(), 1);
            assert_eq!(
                runs[0].fg,
                Tok::Attr(chrome::theme_color(&palette.ramp(RampId::Heat)[upper]))
            );
            assert_eq!(
                runs[0].bg,
                Some(Tok::Attr(chrome::theme_color(
                    &palette.ramp(RampId::Heat)[lower]
                )))
            );
        });
    });
    crate::caps::test_override::with_unicode(UnicodeLevel::Ascii, || {
        let mut out = GraphOut::new();
        paint_heatmap(&mut out, &rows, RampId::Heat, &palette);
        assert_eq!(out.lines.len(), rows.len());
        assert!(out.lines.iter().all(|line| match line {
            Line::Segs(runs) => runs.len() == 1,
            _ => false,
        }));
    });
}

#[test]
fn stacked_out_of_range_band_index_falls_back_to_dim() {
    let palette = default_palette();
    static ROLES: &[SeriesRole] = &[
        SeriesRole::CpuUser,
        SeriesRole::CpuNice,
        SeriesRole::CpuSys,
        SeriesRole::CpuIrq,
        SeriesRole::CpuIowait,
        SeriesRole::CpuSteal,
        SeriesRole::CpuGuest,
        SeriesRole::Cat5,
    ];
    assert_ne!(
        chrome::theme_color(palette.series(ROLES[7])),
        chrome::theme_color(&palette.dim)
    );
    let resolved = Resolved::new(&palette, Paint::Bands(ROLES), ColorDepth::Truecolor);
    let style = resolved.cell(viz_raster::RCell {
        ch: 'x',
        fg: 8,
        bg: Some(9),
    });
    assert_eq!(style.fg, chrome::theme_color(&palette.dim));
    assert_eq!(style.bg, Some(chrome::theme_color(&palette.dim)));
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
    let Line::Segs(first_row) = &mut out.lines[0] else {
        panic!("expected emitted runs")
    };
    first_row[0].text.reserve(1_000);
    let text_capacity = first_row[0].text.capacity();
    out.prepare(12, 3);
    viz_raster::area(&[0.3, 0.7], table(), &mut out.cells);
    out.emit(&resolved);
    assert_eq!(out.cells.width(), 12);
    assert_eq!(out.cells.height(), 3);
    assert_eq!(out.lines.capacity(), capacity);
    let Line::Segs(first_row) = &out.lines[0] else {
        panic!("expected emitted runs")
    };
    assert_eq!(first_row[0].text.capacity(), text_capacity);
    for line in &out.lines {
        let Line::Segs(segs) = line else {
            panic!("expected emitted runs")
        };
        assert!(segs.len() <= 12);
        assert_eq!(crate::seg::seg_width(segs), 12);
    }
}

/// Graph cells with no explicit background sit on the panel surface. They used to be
/// padded with the dim *foreground* slot, which painted every empty graph area as a
/// light block on dark themes.
#[test]
fn graph_backgrounds_use_the_panel_slot_not_the_dim_text_slot() {
    use crate::compositor::Rect;
    for depth in [ColorDepth::Truecolor, ColorDepth::Ansi256] {
        test_override::with_color(depth, || {
            let (panel, dim) = chrome::with_palette(|palette| {
                (
                    crate::wire::color_spec(chrome::theme_color(&palette.panel), depth),
                    crate::wire::color_spec(chrome::theme_color(&palette.dim), depth),
                )
            });
            assert_ne!(panel, dim, "fixture palette must distinguish panel and dim");
            let values = [0.0, 0.1, f32::NAN, 0.5, 0.9, 0.2];
            let mut surface = termwiz::surface::Surface::new(20, 6);
            let spark = Rect {
                x: 0,
                y: 0,
                cols: 20,
                rows: 1,
            };
            super::paint_sparkline(&values, 20, RampId::Util, spark, &mut surface);
            let history = Rect {
                x: 0,
                y: 1,
                cols: 20,
                rows: 5,
            };
            super::paint_history_graph(&values, RampId::Util, history, &mut surface);
            let mut heatmap = super::GraphOut::new();
            super::paint_heatmap(
                &mut heatmap,
                &[&[0.4]],
                RampId::Heat,
                &chrome::palette_snapshot(),
            );
            if let Some(line) = heatmap.lines.first() {
                crate::seg::draw_line(&mut surface, 0, 5, 20, line, Tok::Slot(chrome::S::Panel));
            }
            for (y, line) in surface.screen_cells().iter().enumerate() {
                for (x, cell) in line.iter().enumerate() {
                    let actual = crate::wire::color_spec(cell.attrs().background(), depth);
                    assert_eq!(
                        actual,
                        panel,
                        "cell ({x}, {y}) {:?} is not on the panel",
                        cell.str()
                    );
                }
            }
        });
    }
}

#[test]
fn unresolved_stacked_background_inherits_instead_of_using_dim() {
    let palette = default_palette();
    let resolved = Resolved::new(
        &palette,
        Paint::Bands(&[SeriesRole::MemAnon]),
        ColorDepth::Truecolor,
    );
    let style = resolved.cell(viz_raster::RCell {
        ch: '█',
        fg: 0,
        bg: Some(7),
    });
    assert_eq!(
        style.bg, None,
        "a missing stacked role must inherit the panel surface"
    );
    assert_eq!(
        style.fg,
        chrome::theme_color(&palette.series(SeriesRole::MemAnon))
    );
}
