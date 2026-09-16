use super::*;

fn table(names: &[&str]) -> TableSection {
    TableSection {
        header: vec![
            "pid".into(),
            "name".into(),
            "owner".into(),
            "cpu".into(),
            "mem".into(),
        ],
        rows: names
            .iter()
            .enumerate()
            .map(|(index, name)| {
                vec![
                    Cell::Text((12000 + index).to_string(), Tok::Slot(S::Ghost)),
                    Cell::Text((*name).into(), Tok::Slot(S::Text)),
                    Cell::Text("pane 9".into(), Tok::Slot(S::Ghost)),
                    Cell::Text("99.9%".into(), Tok::Slot(S::Text)),
                    Cell::Text("123 MiB".into(), Tok::Slot(S::Text)),
                ]
            })
            .collect(),
        sel: Some(0),
    }
}

fn seed_row(surface: &mut Surface, x: usize, y: usize, text: String, attrs: &CellAttributes) {
    surface.add_change(Change::CursorPosition {
        x: Position::Absolute(x),
        y: Position::Absolute(y),
    });
    surface.add_change(Change::AllAttributes(attrs.clone()));
    surface.add_change(Change::Text(text));
}

fn text_at(surface: &Surface, x: usize, y: usize, width: usize) -> String {
    surface.screen_lines()[y]
        .visible_cells()
        .filter(|cell| cell.cell_index() >= x && cell.cell_index() < x + width)
        .map(|cell| cell.str().to_owned())
        .collect()
}

#[test]
fn fixed_columns_use_rendered_graphemes_and_preserve_header_body_positions() {
    let widths = [10, 12, 9, 7, 9];
    let starts = [1, 12, 25, 35, 43];
    for name in [
        "fixture",
        "世-e\u{301}-worker",
        "👩‍💻-worker",
        "\u{301}lead",
        "bad\r\n\t\x1bX",
        "bad\r\n\t\x1b[31m\u{009b}\u{202e}name",
    ] {
        let table = table(&[name, "second"]);
        let mut surface = Surface::new(60, 6);
        draw(&mut surface, Rect::full(60, 6), 0, 0, 52, &table, &widths);
        for (column, start) in starts.into_iter().enumerate() {
            assert_eq!(
                text_at(&surface, start, 0, widths[column]).trim(),
                table.header[column]
            );
        }
        assert_eq!(text_at(&surface, starts[0], 1, 10).trim(), "12000");
        assert_eq!(text_at(&surface, starts[2], 1, 9).trim(), "pane 9");
        assert_eq!(text_at(&surface, starts[3], 1, 7).trim(), "99.9%");
        assert_eq!(text_at(&surface, starts[4], 1, 9).trim(), "123 MiB");
        let name_text = text_at(&surface, starts[1], 1, 12);
        match name {
            "fixture" | "世-e\u{301}-worker" | "👩‍💻-worker" => {
                assert_eq!(name_text.trim_end(), name)
            }
            "\u{301}lead" => assert_eq!(name_text.trim_end(), " lead"),
            "bad\r\n\t\x1bX" => assert_eq!(name_text.trim_end(), "bad   X"),
            _ => {}
        }
        assert_eq!(text_at(&surface, starts[1], 2, 12).trim(), "second");
        assert!(!text_at(&surface, 0, 1, 60).chars().any(char::is_control));
        assert!(text_at(&surface, 52, 1, 8).trim().is_empty());
        let cells = surface.screen_cells();
        assert_ne!(
            cells[1][2].attrs().background(),
            cells[2][2].attrs().background()
        );
        assert_eq!(
            cells[1][2].attrs().foreground(),
            cells[2][2].attrs().foreground()
        );
    }
}

#[test]
fn clipping_does_not_split_graphemes_or_move_adjacent_columns() {
    for width in 0..=12 {
        for value in [
            "世世世",
            "e\u{301}e\u{301}e\u{301}",
            "👩‍💻👩‍💻👩‍💻",
            "123456789012345",
        ] {
            let mut surface = Surface::new(20, 2);
            let table = TableSection {
                header: vec![],
                rows: vec![vec![
                    Cell::Text(value.into(), Tok::Slot(S::Text)),
                    Cell::Text("END".into(), Tok::Slot(S::Text)),
                ]],
                sel: None,
            };
            draw(
                &mut surface,
                Rect::full(20, 2),
                0,
                0,
                20,
                &table,
                &[width, 3],
            );
            let marker_start = if width == 0 { 0 } else { width + 1 };
            assert_eq!(text_at(&surface, marker_start, 0, 3), "END");
            let emitted = text_at(&surface, 0, 0, width);
            if width > 0 {
                let original: Vec<_> = Graphemes::new(value).collect();
                for cluster in Graphemes::new(emitted.trim_end()) {
                    assert!(
                        original.contains(&cluster)
                            || crate::caps::active_glyphs().ellipsis.contains(cluster),
                        "clipped cluster must stay whole: {cluster:?}"
                    );
                }
            }
            assert!(text_at(&surface, 0, 1, 20).trim().is_empty());
        }
    }
}

#[test]
fn fixed_table_scrolling_keeps_height_and_exact_selected_row() {
    let mut table = table(&["first", "second", "third", "fourth"]);
    table.sel = Some(2);
    let section = super::super::Section::FixedTable {
        table,
        widths: vec![10, 12, 9, 7, 9],
    };
    assert_eq!(section.height(), 5);
    let mut surface = Surface::new(60, 6);
    let clip = Rect {
        x: 2,
        y: 2,
        cols: 52,
        rows: 2,
    };
    super::super::render_stack(&mut surface, clip, 2, &[section]);
    assert_eq!(text_at(&surface, 14, 2, 12).trim(), "second");
    assert_eq!(text_at(&surface, 14, 3, 12).trim(), "third");
    let cells = surface.screen_cells();
    assert_ne!(
        cells[2][4].attrs().background(),
        cells[3][4].attrs().background()
    );
    assert!(text_at(&surface, 0, 1, 60).trim().is_empty());
    assert!(text_at(&surface, 0, 4, 60).trim().is_empty());
}

#[test]
fn malformed_width_metadata_and_tiny_viewports_clip_without_panicking() {
    let table = table(&["fixture"]);
    for widths in [vec![], vec![usize::MAX], vec![0; 10], vec![0, 0, 2]] {
        for width in 0..=8 {
            let mut surface = Surface::new(12, 4);
            draw(
                &mut surface,
                Rect {
                    x: 2,
                    y: 1,
                    cols: width,
                    rows: 2,
                },
                2,
                1,
                usize::MAX,
                &table,
                &widths,
            );
            for row in [1, 2] {
                assert!(text_at(&surface, 0, row, 2).trim().is_empty());
                assert!(
                    text_at(&surface, 2 + width, row, 10 - width)
                        .trim()
                        .is_empty()
                );
            }
        }
    }
}

#[test]
fn single_pass_emitted_text_covers_each_visible_cell_once() {
    for widths in [
        vec![],
        vec![0; 8],
        vec![usize::MAX],
        vec![0, 5, 0, 3],
        vec![5, 9, 4],
        vec![31],
    ] {
        for selection in [None, Some(1)] {
            for header in [false, true] {
                let mut table = table(&["界e\u{301}", "👩‍💻"]);
                table.sel = selection;
                if !header {
                    table.header.clear();
                }
                let mut surface = Surface::new(64, 10);
                surface.add_change(Change::AllAttributes(CellAttributes::default()));
                let sequence = surface.current_seqno();
                let clip = Rect {
                    x: 4,
                    y: 3,
                    cols: 32,
                    rows: 3,
                };
                draw(&mut surface, clip, 4, 3, usize::MAX, &table, &widths);
                let (_, changes) = surface.get_changes(sequence);
                let std::borrow::Cow::Borrowed(changes) = changes else {
                    panic!("work-shape evidence must be the actual emission log, not a repaint");
                };
                let mut coverage = [[0u8; 64]; 10];
                let mut cursor = None;
                let mut row_positions = [0u8; 10];
                for change in changes {
                    match change {
                        Change::CursorPosition {
                            x: Position::Absolute(x),
                            y: Position::Absolute(y),
                        } => {
                            assert_eq!(*x, clip.x, "only the row origin needs positioning");
                            row_positions[*y] += 1;
                            cursor = Some((*x, *y));
                        }
                        Change::Text(text) => {
                            let (mut x, y) = cursor.expect("text has an explicit position");
                            for grapheme in Graphemes::new(text) {
                                let width = grapheme_column_width(grapheme, None).max(1);
                                assert!(y < 10 && x + width <= 64, "no implicit wrap");
                                for cell in &mut coverage[y][x..x + width] {
                                    *cell += 1;
                                }
                                x += width;
                            }
                            cursor = Some((x, y));
                        }
                        Change::AllAttributes(_) => {}
                        _ => panic!("unexpected operation in fixed-table paint"),
                    }
                }
                // This counts emitted Text coverage, not internal Surface
                // updates (overwriting a wide cell may clear its continuation).
                for (y, count) in row_positions.iter().enumerate() {
                    assert_eq!(
                        *count,
                        u8::from((3..3 + 2 + usize::from(header)).contains(&y)),
                        "one cursor position per visible row: {widths:?}",
                    );
                }
                for (y, row) in coverage.iter().enumerate() {
                    for (x, count) in row.iter().enumerate() {
                        let painted =
                            (4..36).contains(&x) && (3..3 + 2 + usize::from(header)).contains(&y);
                        assert_eq!(*count, u8::from(painted), "cell {x},{y}: {widths:?}");
                    }
                }
            }
        }
    }
}

#[test]
fn reused_surface_clears_wide_cells_and_moves_the_entire_selection_band() {
    let mut surface = Surface::new(40, 7);
    for y in 0..7 {
        seed_row(
            &mut surface,
            0,
            y,
            "X".repeat(40),
            &CellAttributes::default(),
        );
    }
    let clip = Rect {
        x: 3,
        y: 2,
        cols: 32,
        rows: 3,
    };
    // Wide cells cross the new column and separator boundaries. The new paint
    // must clear these on a reused Surface, without an enclosing layer clear.
    let old = TableSection {
        header: vec!["界".repeat(15)],
        rows: (0..2)
            .map(|_| vec![Cell::Text("界".repeat(15), Tok::Slot(S::Accent))])
            .collect(),
        sel: Some(0),
    };
    draw(&mut surface, clip, 3, 2, 32, &old, &[30]);
    let table = TableSection {
        header: vec!["id".into(), "name".into(), "own".into()],
        rows: vec![
            vec![
                Cell::Text("1".into(), Tok::Slot(S::Ghost)),
                Cell::Text("a".into(), Tok::Slot(S::Text)),
            ],
            vec![
                Cell::Text("2".into(), Tok::Slot(S::Ghost)),
                Cell::Text("b".into(), Tok::Slot(S::Dim)),
                Cell::Text("?".into(), Tok::Slot(S::Accent)),
            ],
        ],
        sel: Some(1),
    };
    draw(&mut surface, clip, 3, 2, 32, &table, &[5, 9, 4]);
    assert_eq!(text_at(&surface, 4, 3, 5), "1    ");
    assert_eq!(text_at(&surface, 10, 3, 9), "a        ");
    assert_eq!(text_at(&surface, 20, 3, 4), "    ");
    assert_eq!(text_at(&surface, 4, 4, 5), "2    ");
    assert_eq!(text_at(&surface, 10, 4, 9), "b        ");
    assert_eq!(text_at(&surface, 20, 4, 4), "?   ");
    let cells = surface.screen_cells();
    chrome::with_palette(|palette| {
        for (y, row) in cells.iter().enumerate() {
            for (x, cell) in row.iter().enumerate() {
                if !(2..5).contains(&y) || !(3..35).contains(&x) {
                    assert_eq!(cell.str(), "X", "outside clip {x},{y}");
                    assert_eq!(cell.attrs(), &CellAttributes::default());
                    continue;
                }
                let selected = y == 4;
                let background = Tok::Slot(if selected { S::Panel2 } else { S::Panel });
                assert_eq!(cell.attrs().background(), background.resolve(palette));
                let local = x - 3;
                let tone = match local {
                    0 if selected => S::Accent,
                    1..=5 => S::Ghost,
                    7..=15 => match y {
                        2 => S::Ghost,
                        3 => S::Text,
                        _ => S::Dim,
                    },
                    17..=20 if selected => S::Accent,
                    17..=20 => S::Ghost,
                    _ => S::Text,
                };
                assert_eq!(cell.attrs().foreground(), Tok::Slot(tone).resolve(palette));
                if matches!(local, 6 | 16 | 21..=31) || (local == 0 && !selected) {
                    assert_eq!(cell.str(), " ", "blank span {x},{y}");
                }
                assert!(!cell.str().contains('界'), "stale wide glyph at {x},{y}");
            }
        }
    });
}

#[test]
fn empty_and_zero_width_metadata_clear_reused_rows_without_a_cursor() {
    for widths in [vec![], vec![0; 12]] {
        let mut surface = Surface::new(20, 3);
        seed_row(
            &mut surface,
            0,
            1,
            "X".repeat(20),
            &CellAttributes::default(),
        );
        let table = TableSection {
            header: vec![],
            rows: vec![vec![]],
            sel: None,
        };
        let clip = Rect {
            x: 2,
            y: 1,
            cols: 16,
            rows: 1,
        };
        draw(&mut surface, clip, 2, 1, usize::MAX, &table, &widths);
        assert_eq!(text_at(&surface, 0, 1, 20), "XX                XX");
        assert_eq!(text_at(&surface, 0, 2, 20), "                    ");
        chrome::with_palette(|palette| {
            for cell in &surface.screen_cells()[1][2..18] {
                assert_eq!(
                    cell.attrs().background(),
                    Tok::Slot(S::Panel).resolve(palette)
                );
                assert_eq!(
                    cell.attrs().foreground(),
                    Tok::Slot(S::Text).resolve(palette)
                );
            }
        });
    }
}

#[test]
fn contiguous_spans_at_bottom_right_preserve_unicode_and_outside_sentinels() {
    for selected in [None, Some(0), Some(1)] {
        let mut surface = Surface::new(24, 4);
        for y in 0..4 {
            seed_row(
                &mut surface,
                0,
                y,
                "X".repeat(24),
                &CellAttributes::default(),
            );
        }
        let clip = Rect {
            x: 3,
            y: 2,
            cols: 21,
            rows: 2,
        };
        // Seed wide glyphs across the future separator/column boundaries. The
        // new final column also ends with a wide glyph at the physical edge.
        let old = TableSection {
            header: vec![],
            rows: (0..2)
                .map(|_| {
                    vec![Cell::Text(
                        format!("{}Q", "界".repeat(10)),
                        Tok::Slot(S::Accent),
                    )]
                })
                .collect(),
            sel: None,
        };
        draw(&mut surface, clip, 3, 2, 21, &old, &[21]);
        let table = TableSection {
            header: vec![],
            rows: ["abcdef界", "ghijkl界"]
                .into_iter()
                .map(|last| {
                    vec![
                        Cell::Text("界e\u{301}".into(), Tok::Slot(S::Ghost)),
                        Cell::Text("👩‍💻".into(), Tok::Slot(S::Dim)),
                        Cell::Text(last.into(), Tok::Slot(S::Accent)),
                    ]
                })
                .collect(),
            sel: selected,
        };
        let gutter = usize::from(selected.is_some());
        let first_width = 6 - gutter;
        draw(
            &mut surface,
            clip,
            3,
            2,
            usize::MAX,
            &table,
            &[first_width, 5, 8],
        );
        for (y, last) in [(2, "abcdef界"), (3, "ghijkl界")] {
            assert_eq!(text_at(&surface, 16, y, 8), last);
        }
        let cells = surface.screen_cells();
        chrome::with_palette(|palette| {
            for (y, row) in cells.iter().enumerate() {
                for (x, cell) in row.iter().enumerate() {
                    if y < 2 || x < 3 {
                        assert_eq!(cell.str(), "X", "no wrap/scroll outside {x},{y}");
                        assert_eq!(cell.attrs(), &CellAttributes::default());
                        continue;
                    }
                    let cur = selected == Some(y - 2);
                    let tone = match x {
                        3 if gutter > 0 && cur => S::Accent,
                        3 if gutter > 0 => S::Text,
                        3..=8 => S::Ghost,
                        9 | 15 => S::Text,
                        10..=14 => S::Dim,
                        _ => S::Accent,
                    };
                    let mut expected = CellAttributes::default();
                    expected.set_foreground(Tok::Slot(tone).resolve(palette));
                    expected.set_background(
                        Tok::Slot(if cur { S::Panel2 } else { S::Panel }).resolve(palette),
                    );
                    assert_eq!(cell.attrs(), &expected, "complete attributes {x},{y}");
                }
            }
        });
        for y in [2, 3] {
            assert_eq!(cells[y][3 + gutter].str(), "界");
            assert_eq!(cells[y][3 + gutter].width(), 2);
            assert_eq!(cells[y][4 + gutter].str(), " ");
            assert_eq!(cells[y][5 + gutter].str(), "e\u{301}");
            assert_eq!(cells[y][10].str(), "👩‍💻");
            assert_eq!(cells[y][10].width(), 2);
            assert_eq!(cells[y][11].str(), " ");
            assert_eq!(cells[y][22].str(), "界");
            assert_eq!(cells[y][22].width(), 2);
            assert_eq!(cells[y][23].str(), " ");
            for (x, cell) in cells[y].iter().enumerate().take(10).skip(6 + gutter) {
                assert_eq!(cell.str(), " ", "first padding/separator {x},{y}");
            }
            for (x, cell) in cells[y].iter().enumerate().take(16).skip(12) {
                assert_eq!(cell.str(), " ", "second padding/separator {x},{y}");
            }
        }
    }
}
