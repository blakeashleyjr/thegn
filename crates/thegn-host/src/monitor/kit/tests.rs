use super::grid::{CellGrid, CellStyle};
use super::meter::{Meter, MeterSpec};
use super::*;
use crate::{
    caps::{self, Glyph},
    compositor::Rect,
    seg::{StyleTok, Tok},
};
use termwiz::surface::Surface;
use thegn_core::{
    numfmt::{NumFmt, Reading, Unit},
    termcaps::{ColorDepth, UnicodeLevel},
    theme::{RampId, SeriesRole, StateRole},
};

fn fixture(level: UnicodeLevel, depth: ColorDepth) -> (CellGrid, Surface) {
    let mut grid = CellGrid::new(48, 10);
    caps::test_override::with_unicode(level, || {
        caps::test_override::with_color(depth, || {
            {
                let mut interior = panel::panel(
                    &mut grid,
                    Rect::full(48, 10),
                    "Monitor fixture title",
                    Some("12s"),
                );
                interior.put_str(0, 0, "body", CellStyle::default());
            }
            legend::draw(
                &mut grid,
                Rect {
                    x: 1,
                    y: 1,
                    cols: 40,
                    rows: 1,
                },
                &[
                    legend::LegendSwatch::series("cpu", SeriesRole::CpuUser),
                    legend::LegendSwatch {
                        label: "memory".into(),
                        color: Tok::Ramp(RampId::Mem, 6),
                    },
                ],
            );
            axis::y_gutter(
                &mut grid,
                Rect {
                    x: 1,
                    y: 3,
                    cols: 7,
                    rows: 3,
                },
                100.0,
                Unit::Percent,
                NumFmt::default(),
            );
            axis::x_ticks(
                &mut grid,
                Rect {
                    x: 8,
                    y: 5,
                    cols: 25,
                    rows: 1,
                },
                &["1m", "30s", "now"],
            );
            chip::draw(
                &mut grid,
                Rect {
                    x: 1,
                    y: 7,
                    cols: 14,
                    rows: 1,
                },
                "temp",
                Reading::Value(42.0),
                Unit::Temp,
                NumFmt::default(),
                StateRole::Warn,
            );
            let mut meter = Meter::default();
            meter.paint(
                &mut grid,
                Rect {
                    x: 16,
                    y: 7,
                    cols: 20,
                    rows: 1,
                },
                MeterSpec {
                    value: 0.72,
                    warn: 0.6,
                    crit: 0.9,
                    ramp: RampId::Util,
                    label: true,
                    fmt: NumFmt::default(),
                },
            );
            let cols = [table::Col {
                title: "rate",
                width: 8,
                align: table::Align::Right,
                unit: Some(Unit::BitsPerSec),
            }];
            let values = [
                [table::TableCell::Number(Reading::Value(0.0))],
                [table::TableCell::Number(Reading::Value(12.0))],
            ];
            let rows: [&[table::TableCell<'_>]; 2] = [&values[0], &values[1]];
            table::draw(
                &mut grid,
                Rect {
                    x: 1,
                    y: 8,
                    cols: 20,
                    rows: 2,
                },
                &table::TableSpec {
                    cols: &cols,
                    rows: &rows,
                    offset: 0,
                    visible: 1,
                    sort: Some((0, table::SortDirection::Desc)),
                    selected: Some(0),
                    fmt: NumFmt {
                        zero: thegn_core::numfmt::ZeroStyle::Dot,
                        ..NumFmt::default()
                    },
                },
            );
        })
    });
    let mut surface = Surface::new(48, 10);
    let mut emitter = emit::Emitter::default();
    emitter.emit(&grid, Rect::full(48, 10), &mut surface, 0, 0);
    (grid, surface)
}

fn rows(surface: &mut Surface) -> Vec<String> {
    surface
        .screen_cells()
        .iter()
        .map(|row| row.iter().map(|cell| cell.str()).collect())
        .collect()
}

#[test]
fn widget_snapshot_matrix_degrades_for_every_unicode_and_color_level() {
    let levels = [UnicodeLevel::Full, UnicodeLevel::Basic, UnicodeLevel::Ascii];
    let depths = [
        ColorDepth::Truecolor,
        ColorDepth::Ansi256,
        ColorDepth::Ansi16,
        ColorDepth::None,
    ];
    for level in levels {
        for depth in depths {
            let (grid, mut surface) = fixture(level, depth);
            let rendered = rows(&mut surface);
            assert_eq!(rendered.len(), 10);
            assert!(rendered.iter().all(|row| row.chars().count() == 48));
            assert!(rendered.iter().any(|row| row.contains("Monitor")));
            assert!(rendered.iter().any(|row| row.contains("cpu")));
            caps::test_override::with_unicode(level, || {
                let (top_left, top_right) = if level == UnicodeLevel::Ascii {
                    (Glyph::BoxTl, Glyph::BoxTr)
                } else {
                    (Glyph::BoxRoundTl, Glyph::BoxRoundTr)
                };
                assert_eq!(
                    grid.get(0, 0).unwrap().ch,
                    top_left
                        .resolve(caps::active_glyphs())
                        .chars()
                        .next()
                        .unwrap()
                );
                assert_eq!(
                    grid.get(47, 0).unwrap().ch,
                    top_right
                        .resolve(caps::active_glyphs())
                        .chars()
                        .next()
                        .unwrap()
                );

                let full = caps::raster_table().hbar8[7];
                for col in 16..26 {
                    assert_eq!(grid.get(col, 7).unwrap().ch, full);
                }
            });
            if level == UnicodeLevel::Ascii {
                assert!(
                    rendered
                        .iter()
                        .flat_map(|row| row.chars())
                        .all(|ch| ch.is_ascii())
                );
            }
            if depth == ColorDepth::None {
                let (_, mut truecolor_surface) = fixture(level, ColorDepth::Truecolor);
                assert_eq!(rendered, rows(&mut truecolor_surface));
            }
            let (grid, mut surface) = fixture(level, depth);
            let cells = surface.screen_cells();
            for row in 0..grid.rows() {
                for col in 0..grid.cols() {
                    let cell = grid.get(col, row).unwrap();
                    let expected_fg = cell
                        .fg
                        .tok()
                        .map(|tok| {
                            caps::test_override::with_color(depth, || {
                                crate::chrome::with_palette(|p| tok.resolve(p))
                            })
                        })
                        .unwrap_or_default();
                    let expected_bg = if cell.bg == StyleTok::INHERIT {
                        caps::test_override::with_color(depth, || {
                            crate::chrome::with_palette(|p| {
                                crate::chrome::theme_color(crate::chrome::slot_rgb(
                                    p,
                                    crate::chrome::S::Panel,
                                ))
                            })
                        })
                    } else {
                        cell.bg
                            .tok()
                            .map(|tok| {
                                caps::test_override::with_color(depth, || {
                                    crate::chrome::with_palette(|p| tok.resolve(p))
                                })
                            })
                            .unwrap_or_default()
                    };
                    assert_eq!(
                        crate::wire::color_spec(cells[row][col].attrs().foreground(), depth),
                        crate::wire::color_spec(expected_fg, depth)
                    );
                    assert_eq!(
                        crate::wire::color_spec(cells[row][col].attrs().background(), depth),
                        crate::wire::color_spec(expected_bg, depth)
                    );
                }
            }
        }
    }
}

#[test]
fn emitter_coalesces_style_runs_and_rebuilds_cache_only_for_a_new_key() {
    let mut grid = CellGrid::new(6, 1);
    let red = StyleTok::from_tok(Tok::State(StateRole::Crit)).unwrap();
    let blue = StyleTok::from_tok(Tok::State(StateRole::Ok)).unwrap();
    {
        let mut v = grid.view(Rect::full(6, 1));
        v.put_str(
            0,
            0,
            "aa",
            CellStyle {
                fg: red,
                ..CellStyle::default()
            },
        );
        v.put_str(
            2,
            0,
            "bb",
            CellStyle {
                fg: blue,
                ..CellStyle::default()
            },
        );
        v.put_str(
            4,
            0,
            "cc",
            CellStyle {
                fg: red,
                ..CellStyle::default()
            },
        );
    }
    let mut emitter = emit::Emitter::default();
    let mut surface = Surface::new(6, 1);
    emitter.emit(&grid, Rect::full(6, 1), &mut surface, 0, 0);
    assert_eq!(emitter.text_runs(), 3);
    assert_eq!(emitter.rebuilds(), 1);
    emitter.emit(&grid, Rect::full(6, 1), &mut surface, 0, 0);
    assert_eq!(emitter.rebuilds(), 1);
    emitter.bump_test_generation();
    emitter.emit(&grid, Rect::full(6, 1), &mut surface, 0, 0);
    assert_eq!(emitter.rebuilds(), 2);
}

#[test]
fn wide_char_continuation_is_not_emitted_as_text() {
    let mut grid = CellGrid::new(8, 1);
    grid.view(Rect::full(8, 1))
        .put_str(0, 0, "a界b", CellStyle::default());
    let mut surface = Surface::new(8, 1);
    emit::Emitter::default().emit(&grid, Rect::full(8, 1), &mut surface, 0, 0);
    assert_eq!(rows(&mut surface)[0], format!("a界b{}", " ".repeat(4)));
    assert_eq!(grid.get(3, 0).unwrap().ch, 'b');

    let mut overwritten = CellGrid::new(4, 1);
    overwritten
        .view(Rect::full(4, 1))
        .put_str(0, 0, "界xy", CellStyle::default());
    overwritten.view(Rect::full(4, 1)).put(
        1,
        0,
        CellStyle {
            ch: 'q',
            ..CellStyle::default()
        },
    );
    assert_eq!(overwritten.get(0, 0).unwrap().ch, ' ');
    assert_eq!(overwritten.get(1, 0).unwrap().ch, 'q');
}

#[test]
fn ascii_panel_long_title_preserves_top_right_corner() {
    caps::test_override::with_unicode(UnicodeLevel::Ascii, || {
        let mut grid = CellGrid::new(10, 3);
        let _body = panel::panel(
            &mut grid,
            Rect::full(10, 3),
            "a very long panel title",
            None,
        );
        assert_eq!(
            grid.get(9, 0).unwrap().ch,
            Glyph::BoxTr
                .resolve(&caps::active_glyphs())
                .chars()
                .next()
                .unwrap()
        );
    });
}

#[test]
fn small_y_axis_keeps_max_and_zero_labels_distinct() {
    for rows in [1, 2] {
        let mut grid = CellGrid::new(8, rows);
        axis::y_gutter(
            &mut grid,
            Rect::full(8, rows),
            100.0,
            Unit::Percent,
            NumFmt::default(),
        );
        let max = (0..8)
            .map(|x| grid.get(x, 0).unwrap().ch)
            .collect::<String>();
        assert!(
            max.contains("100"),
            "max label missing at {rows} rows: {max:?}"
        );
        if rows == 2 {
            let zero = (0..8)
                .map(|x| grid.get(x, 1).unwrap().ch)
                .collect::<String>();
            assert!(zero.contains('0'), "zero label missing: {zero:?}");
        }
    }
}

#[test]
fn legend_exact_fit_does_not_drop_last_entry() {
    let entries = [legend::LegendSwatch::series("cpu", SeriesRole::CpuUser)];
    let width =
        unicode_width::UnicodeWidthStr::width(caps::glyph(Glyph::LegendSwatch)) + 1 + "cpu".len();
    let mut grid = CellGrid::new(width, 1);
    legend::draw(&mut grid, Rect::full(width, 1), &entries);
    let row = (0..width)
        .map(|x| grid.get(x, 0).unwrap().ch)
        .collect::<String>();
    assert!(row.contains("cpu"));
    assert!(!row.contains(caps::glyph(Glyph::Ellipsis)));
}

#[test]
fn util_meter_emits_three_ansi16_or_eight_truecolor_levels() {
    for (depth, expected) in [(ColorDepth::Ansi16, 3), (ColorDepth::Truecolor, 8)] {
        caps::test_override::with_color(depth, || {
            let mut grid = CellGrid::new(8, 1);
            let mut meter = Meter::default();
            meter.paint(
                &mut grid,
                Rect::full(8, 1),
                MeterSpec {
                    value: 1.0,
                    warn: 0.7,
                    crit: 0.9,
                    ramp: RampId::Util,
                    label: false,
                    fmt: NumFmt::default(),
                },
            );
            assert!((0..8).all(|x| grid.get(x, 0).unwrap().ch == caps::raster_table().hbar8[7]));
            let mut surface = Surface::new(8, 1);
            emit::Emitter::default().emit(&grid, Rect::full(8, 1), &mut surface, 0, 0);
            let mut specs = Vec::new();
            for cell in surface.screen_cells()[0].iter() {
                let spec = crate::wire::color_spec(cell.attrs().foreground(), depth);
                if !specs.contains(&spec) {
                    specs.push(spec);
                }
            }
            assert_eq!(specs.len(), expected);
        });
    }
}

#[test]
fn warm_chrome_repaint_allocates_nothing() {
    let mut grid = CellGrid::new(64, 8);
    let entries = [legend::LegendSwatch::series("cpu", SeriesRole::CpuUser)];
    let cols = [table::Col {
        title: "process",
        width: 16,
        align: table::Align::Left,
        unit: None,
    }];
    let row = [table::TableCell::Text("worker")];
    let table_rows: [&[table::TableCell<'_>]; 1] = [&row];
    let repaint = |grid: &mut CellGrid| {
        {
            let _body = panel::panel(grid, Rect::full(64, 8), "monitor panel title", None);
        }
        {
            let _clipped = panel::panel(
                grid,
                Rect {
                    x: 50,
                    y: 0,
                    cols: 12,
                    rows: 8,
                },
                "title that must be clipped",
                None,
            );
        }
        legend::draw(
            grid,
            Rect {
                x: 1,
                y: 1,
                cols: 40,
                rows: 1,
            },
            &entries,
        );
        axis::x_ticks(
            grid,
            Rect {
                x: 1,
                y: 2,
                cols: 40,
                rows: 1,
            },
            &["1m", "now"],
        );
        table::draw(
            grid,
            Rect {
                x: 1,
                y: 3,
                cols: 40,
                rows: 3,
            },
            &table::TableSpec {
                cols: &cols,
                rows: &table_rows,
                offset: 0,
                visible: 1,
                sort: None,
                selected: None,
                fmt: NumFmt::default(),
            },
        );
    };
    repaint(&mut grid);
    let ((), counts) = crate::proc_workload_alloc::measure(|| repaint(&mut grid));
    assert_eq!(
        counts.allocation_calls, 0,
        "warm chrome repaint allocated: {counts:?}"
    );
}

#[test]
fn ramps_clamp_and_ansi16_bands_have_three_roles() {
    let colors = crate::chrome::with_palette(|p| {
        (0..=7)
            .map(|level| Tok::Ramp(RampId::Util, level).resolve(p))
            .collect::<Vec<_>>()
    });
    assert_eq!(colors.len(), 8);
    assert!(
        colors
            .iter()
            .enumerate()
            .all(|(i, color)| !colors[..i].contains(color))
    );
    assert_ne!(
        Tok::Ramp(RampId::Util, 255).resolve(&crate::chrome::palette_snapshot()),
        termwiz::color::ColorAttribute::Default
    );
    let indices = [0_u8, 4, 5, 6, 7].map(|level| {
        caps::test_override::with_color(ColorDepth::Ansi16, || {
            Tok::Ramp(RampId::Util, level).resolve(&crate::chrome::palette_snapshot())
        })
    });
    assert_eq!(indices[0], indices[1]);
    assert_eq!(indices[2], indices[3]);
    assert_ne!(indices[1], indices[2]);
    assert_ne!(indices[3], indices[4]);
}

#[test]
fn basic_unicode_changes_only_the_four_frame_corners() {
    let (_full_grid, mut full) = fixture(UnicodeLevel::Full, ColorDepth::Truecolor);
    let (_basic_grid, mut basic) = fixture(UnicodeLevel::Basic, ColorDepth::Truecolor);
    let full_rows = rows(&mut full);
    let basic_rows = rows(&mut basic);
    let differences = full_rows
        .iter()
        .zip(&basic_rows)
        .enumerate()
        .flat_map(|(y, (a, b))| {
            a.chars()
                .zip(b.chars())
                .enumerate()
                .filter_map(move |(x, (left, right))| (left != right).then_some((x, y)))
        })
        .collect::<Vec<_>>();
    assert_eq!(differences, [(0, 0), (47, 0), (0, 9), (47, 9)]);
}

#[test]
fn table_window_sort_selection_and_dot_zero_are_visible() {
    let cols = [
        table::Col {
            title: "name",
            width: 6,
            align: table::Align::Left,
            unit: None,
        },
        table::Col {
            title: "rate",
            width: 5,
            align: table::Align::Right,
            unit: Some(Unit::BitsPerSec),
        },
    ];
    let values = [
        [
            table::TableCell::Text("zero"),
            table::TableCell::Number(Reading::Value(0.0)),
        ],
        [
            table::TableCell::Text("one"),
            table::TableCell::Number(Reading::Value(1.0)),
        ],
    ];
    let rows: [&[table::TableCell<'_>]; 2] = [&values[0], &values[1]];
    let mut grid = CellGrid::new(16, 3);
    let fmt = NumFmt {
        zero: thegn_core::numfmt::ZeroStyle::Dot,
        ..NumFmt::default()
    };
    table::draw(
        &mut grid,
        Rect::full(16, 3),
        &table::TableSpec {
            cols: &cols,
            rows: &rows,
            offset: 1,
            visible: 1,
            sort: Some((0, table::SortDirection::Asc)),
            selected: Some(1),
            fmt,
        },
    );
    let row = (0..16)
        .filter_map(|x| grid.get(x, 1))
        .map(|cell| cell.ch)
        .collect::<String>();
    assert!(row.contains("one"));
    assert!(!row.contains("zero"));
    assert!(row.contains(caps::glyph(Glyph::SortAsc)));
    assert_eq!(
        grid.get(0, 1).map(|cell| cell.bg),
        StyleTok::from_tok(Tok::Series(SeriesRole::Selected))
    );

    table::draw(
        &mut grid,
        Rect::full(16, 3),
        &table::TableSpec {
            cols: &cols[1..],
            rows: &rows[..1],
            offset: 0,
            visible: 1,
            sort: None,
            selected: None,
            fmt,
        },
    );
    let zero_row = (0..8)
        .filter_map(|x| grid.get(x, 1))
        .map(|cell| cell.ch)
        .collect::<String>();
    assert!(zero_row.contains(caps::num_marks().zero));
}

#[test]
fn warm_emit_allocations_scale_with_style_runs() {
    let mut grid = CellGrid::new(12, 1);
    {
        let mut view = grid.view(Rect::full(12, 1));
        view.put_str(0, 0, "same style", CellStyle::default());
    }
    let mut emitter = emit::Emitter::default();
    let mut surface = Surface::new(12, 1);
    emitter.emit(&grid, Rect::full(12, 1), &mut surface, 0, 0);
    let ((), counts) = crate::proc_workload_alloc::measure(|| {
        emitter.emit(&grid, Rect::full(12, 1), &mut surface, 0, 0)
    });
    assert!(
        counts.allocation_calls <= 4,
        "one run allows 2 * runs + 2 allocations: {counts:?}"
    );
}

#[test]
fn clipping_and_degenerate_widget_sizes_stay_inside_grid() {
    for (cols, rows) in [(0, 0), (1, 1), (2, 2), (7, 4)] {
        let mut grid = CellGrid::new(3, 3);
        let sentinel = CellStyle {
            ch: '!',
            ..CellStyle::default()
        };
        grid.view(Rect::full(3, 3)).fill(sentinel);
        let rect = Rect {
            x: 1,
            y: 1,
            cols,
            rows,
        };
        {
            let mut view = grid.view(rect);
            view.put_str(0, 0, "界wide", CellStyle::default());
            view.hline(0, 0, usize::MAX, CellStyle::default());
        }
        {
            let _interior = panel::panel(&mut grid, rect, "a long title", Some("badge"));
        }
        legend::draw(
            &mut grid,
            rect,
            &[legend::LegendSwatch::series(
                "overflowing",
                SeriesRole::CpuUser,
            )],
        );
        axis::y_gutter(&mut grid, rect, 100.0, Unit::Percent, NumFmt::default());
        axis::x_ticks(&mut grid, rect, &["old", "now"]);
        chip::draw(
            &mut grid,
            rect,
            "x",
            Reading::Denied,
            Unit::Count,
            NumFmt::default(),
            StateRole::Denied,
        );
        let mut meter = Meter::default();
        meter.paint(
            &mut grid,
            rect,
            MeterSpec {
                value: 0.5,
                warn: 0.7,
                crit: 0.9,
                ramp: RampId::Util,
                label: true,
                fmt: NumFmt::default(),
            },
        );
        table::draw(
            &mut grid,
            rect,
            &table::TableSpec {
                cols: &[],
                rows: &[],
                offset: 0,
                visible: usize::MAX,
                sort: None,
                selected: None,
                fmt: NumFmt::default(),
            },
        );
        assert!(grid.cols() == 3 && grid.rows() == 3);
        for y in 0..3 {
            for x in 0..3 {
                if x < rect.x
                    || x >= rect.x.saturating_add(rect.cols)
                    || y < rect.y
                    || y >= rect.y.saturating_add(rect.rows)
                {
                    assert_eq!(grid.get(x, y).unwrap().ch, sentinel.ch);
                }
            }
        }
    }
}
