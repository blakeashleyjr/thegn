//! Deterministic text and style snapshots for monitor views.

use crate::compositor::Rect;
use crate::detail::StatusCtx;
use crate::monitor::{MonitorOverlay, MonitorTab};
use crate::telemetry::TelemetryHistory;
use std::fmt::Write as _;
use termwiz::cell::{CellAttributes, Intensity, Underline};
use termwiz::color::ColorSpec;
use termwiz::surface::Surface;

#[derive(Debug, Clone, PartialEq, Eq)]
struct TextGrid {
    cols: usize,
    rows: usize,
    lines: Vec<String>,
}
impl TextGrid {
    fn from_surface(surface: &Surface) -> Self {
        let (cols, rows) = surface.dimensions();
        Self {
            cols,
            rows,
            lines: surface
                .screen_chars_to_string()
                .split('\n')
                .map(str::to_owned)
                .collect(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct StyleRun {
    x: usize,
    y: usize,
    len: usize,
    fg: ColorSpec,
    bg: ColorSpec,
    attrs: u8,
}

fn style_bits(attrs: &CellAttributes) -> u8 {
    let intensity = match attrs.intensity() {
        Intensity::Normal => 0,
        Intensity::Bold => 1,
        Intensity::Half => 2,
    };
    let underline = match attrs.underline() {
        Underline::None => 0,
        Underline::Single => 1,
        Underline::Double => 2,
        Underline::Curly => 3,
        Underline::Dotted => 4,
        Underline::Dashed => 5,
    };
    intensity
        | (u8::from(attrs.italic()) << 2)
        | (underline << 3)
        | (u8::from(attrs.reverse()) << 6)
        | (u8::from(attrs.strikethrough()) << 7)
}

fn style_runs(surface: &Surface) -> Vec<StyleRun> {
    let depth = crate::caps::color_depth();
    let mut runs: Vec<StyleRun> = Vec::new();
    for (y, line) in surface.screen_lines().iter().enumerate() {
        for cell in line.visible_cells() {
            let x = cell.cell_index();
            let attr = cell.attrs();
            let fg = crate::wire::color_spec(attr.foreground(), depth);
            let bg = crate::wire::color_spec(attr.background(), depth);
            let attrs = style_bits(attr);
            let len = cell.width();
            if let Some(last) = runs.last_mut()
                && last.y == y
                && last.x + last.len == x
                && last.fg == fg
                && last.bg == bg
                && last.attrs == attrs
            {
                last.len += len;
            } else {
                runs.push(StyleRun {
                    x,
                    y,
                    len,
                    fg,
                    bg,
                    attrs,
                });
            }
        }
    }
    runs
}

fn serialize_runs(runs: &[StyleRun]) -> String {
    let mut out = String::new();
    for run in runs {
        writeln!(
            out,
            "{},{},{},{:?},{:?},{:02x}",
            run.x, run.y, run.len, run.fg, run.bg, run.attrs
        )
        .expect("writing to String cannot fail");
    }
    out
}

fn fill_ratio(surface: &Surface, area: Rect) -> f32 {
    let (cols, rows) = surface.dimensions();
    let x0 = area.x.min(cols);
    let y0 = area.y.min(rows);
    let x1 = area.x.saturating_add(area.cols).min(cols);
    let y1 = area.y.saturating_add(area.rows).min(rows);
    let area_cells = x1.saturating_sub(x0).saturating_mul(y1.saturating_sub(y0));
    if area_cells == 0 {
        return 0.0;
    }
    let occupied: usize = surface
        .screen_lines()
        .iter()
        .enumerate()
        .filter(|(y, _)| *y >= y0 && *y < y1)
        .flat_map(|(_, line)| line.visible_cells())
        .filter(|cell| {
            let x = cell.cell_index();
            x < x1
                && x.saturating_add(cell.width()) > x0
                && !cell.str().is_empty()
                && cell.str() != " "
                // A blank braille cell (U+2800) is visually empty.
                && cell.str() != "\u{2800}"
        })
        .map(|cell| {
            let x = cell.cell_index();
            cell.width()
                .min(x1.saturating_sub(x))
                .min(x.saturating_add(cell.width()).saturating_sub(x0))
        })
        .sum();
    occupied as f32 / area_cells as f32
}

fn frozen_overlay(tab: MonitorTab, screen: Rect) -> MonitorOverlay {
    const NOW_MS: u64 = 1_767_268_800_000;
    let mut hist = TelemetryHistory::default();
    let snap = crate::e2e_freeze::stats();
    for age in (0..120_u64).rev() {
        hist.push(&snap, NOW_MS - age * 1000);
    }
    let model = crate::chrome::FrameModel {
        stats: snap,
        procs: crate::e2e_freeze::procs(),
        process_state: crate::model_eq::ProcessViewState::Fresh,
        procs_disabled: false,
        ..Default::default()
    };
    let mut ctx = StatusCtx::new_for_test_on(&hist, screen);
    ctx.now_ms = NOW_MS as i64;
    MonitorOverlay::open(tab, Default::default(), &model, &ctx)
}

fn graph_overlay(tab: MonitorTab, screen: Rect) -> MonitorOverlay {
    const NOW_MS: u64 = 1_767_268_800_000;
    let stats = thegn_metrics::StatsSnapshot {
        cpu_temp_c: Some(82.0),
        temps: vec![("coretemp".into(), 82.0)],
        net_bps: Some((18_000_000, 7_000_000)),
        battery: Some((65, false)),
        battery_power_w: Some(24.0),
        power_present: true,
        ..crate::e2e_freeze::stats()
    };
    let mut history = TelemetryHistory::default();
    for age in (0..120_u64).rev() {
        history.push(&stats, NOW_MS - age * 1000);
    }
    let model = crate::chrome::FrameModel {
        stats,
        ..Default::default()
    };
    let mut ctx = StatusCtx::new_for_test_on(&history, screen);
    ctx.now_ms = NOW_MS as i64;
    let mut overlay = MonitorOverlay::open(tab, Default::default(), &model, &ctx);
    let mut frame = thegn_metrics::collect::frame::MonitorFrame::default();
    frame.parts.hwmon.layout = Some(std::sync::Arc::new(
        thegn_metrics::collect::model::hwmon::HwmonLayout {
            generation: 1,
            chips: vec![thegn_metrics::collect::model::hwmon::HwmonChip {
                name: "coretemp".into(),
                device: "cpu0".into(),
                group: thegn_metrics::collect::model::hwmon::ChipGroup::Cpu,
                ordinal: 0,
                gated: false,
            }],
            sensors: vec![thegn_metrics::collect::model::hwmon::HwmonSensor {
                key: "package".into(),
                chip: 0,
                kind: thegn_metrics::collect::model::hwmon::SensorKind::Temp,
                channel: 1,
                role: thegn_metrics::collect::model::hwmon::SensorRole::Input,
                label: Some("Package".into()),
                thresholds: thegn_metrics::collect::model::hwmon::Thresholds {
                    max: Some(85.0),
                    crit: Some(95.0),
                    ..Default::default()
                },
                cost: thegn_metrics::collect::model::hwmon::CostClass::Fast,
            }],
        },
    ));
    frame
        .parts
        .hwmon
        .values
        .push(thegn_metrics::collect::model::hwmon::HwmonValue {
            value: Some(82.0),
            ..Default::default()
        });
    for _ in 0..120 {
        overlay.ingest_frame(&frame);
    }
    overlay
}

fn snapshot(ov: &MonitorOverlay, w: usize, h: usize) -> (String, String) {
    let mut surface = Surface::new(w, h);
    ov.render(&mut surface, Rect::full(w, h));
    let text = surface.screen_chars_to_string();
    let runs = serialize_runs(&style_runs(&surface));
    (text, runs)
}

#[cfg(test)]
mod tests {
    use super::*;
    use termwiz::color::ColorAttribute;
    use termwiz::surface::{Change, Position};
    use thegn_core::termcaps::{ColorDepth, UnicodeLevel};

    fn paint(surface: &mut Surface, x: usize, y: usize, attrs: CellAttributes, text: &str) {
        surface.add_change(Change::CursorPosition {
            x: Position::Absolute(x),
            y: Position::Absolute(y),
        });
        surface.add_change(Change::AllAttributes(attrs));
        surface.add_change(Change::Text(text.to_owned()));
    }

    #[test]
    fn frozen_monitor_renders_byte_identically() {
        let tabs = MonitorTab::visible_for(&crate::e2e_freeze::stats(), true, false);
        for tab in tabs {
            let first = frozen_overlay(tab, Rect::full(100, 32));
            let second = frozen_overlay(tab, Rect::full(100, 32));
            assert_eq!(
                snapshot(&first, 100, 32),
                snapshot(&second, 100, 32),
                "{tab:?}"
            );
        }
    }

    #[test]
    fn procs_tab_shows_frozen_rows() {
        let ov = frozen_overlay(MonitorTab::Procs, Rect::full(100, 32));
        let (text, _) = snapshot(&ov, 100, 32);
        for name in [
            "thegn",
            "thegn-daemon",
            "pane-shell",
            "pane-child",
            "editor",
            "worker",
        ] {
            assert!(text.contains(name), "missing {name}: {text}");
        }
        assert!(!text.contains("init"));
    }

    #[test]
    fn text_grid_matches_screen_chars() {
        let mut surface = Surface::new(5, 2);
        surface.add_change(Change::Text("ab\ncd".to_owned()));
        let grid = TextGrid::from_surface(&surface);
        assert_eq!(grid.lines.join("\n"), surface.screen_chars_to_string());
        assert_eq!((grid.cols, grid.rows), (5, 2));
    }

    #[test]
    fn style_runs_merge_equal_neighbours_and_split_on_change() {
        let mut surface = Surface::new(8, 2);
        let mut red = CellAttributes::default();
        red.set_foreground(ColorAttribute::PaletteIndex(1));
        paint(&mut surface, 0, 0, red.clone(), "ab");
        let mut blue = red.clone();
        blue.set_foreground(ColorAttribute::PaletteIndex(2));
        paint(&mut surface, 2, 0, blue, "c");
        let runs = style_runs(&surface);
        assert_eq!(runs.len(), 4);
        assert_eq!((runs[0].x, runs[0].y, runs[0].len), (0, 0, 2));
        assert_eq!((runs[1].x, runs[1].y, runs[1].len), (2, 0, 1));
        assert_eq!((runs[2].x, runs[2].y, runs[2].len), (3, 0, 5));
        assert_eq!(runs[2].fg, ColorSpec::Default);
        assert_eq!((runs[3].x, runs[3].y, runs[3].len), (0, 1, 8));
        // Exact row starts also guard the x-adjacency check against crossing rows.
    }

    #[test]
    fn wide_glyph_run_counts_cells() {
        let mut surface = Surface::new(4, 1);
        let mut wide = CellAttributes::default();
        wide.set_foreground(ColorAttribute::PaletteIndex(1));
        paint(&mut surface, 0, 0, wide, "界");
        let runs = style_runs(&surface);
        assert_eq!((runs[0].x, runs[0].len), (0, 2));
        assert_eq!(runs[1].x, 2);
    }

    #[test]
    fn fill_ratio_known_grids() {
        let mut surface = Surface::new(4, 2);
        assert_eq!(fill_ratio(&surface, Rect::full(4, 2)), 0.0);
        paint(&mut surface, 0, 0, CellAttributes::default(), "abcd");
        paint(&mut surface, 0, 1, CellAttributes::default(), "efgh");
        assert_eq!(fill_ratio(&surface, Rect::full(4, 2)), 1.0);
        assert_eq!(
            fill_ratio(
                &surface,
                Rect {
                    x: 1,
                    y: 0,
                    cols: 8,
                    rows: 1
                }
            ),
            1.0
        );
        let mut half = Surface::new(4, 1);
        paint(&mut half, 0, 0, CellAttributes::default(), "ab");
        assert_eq!(fill_ratio(&half, Rect::full(4, 1)), 0.5);
        let mut wide = Surface::new(4, 1);
        paint(&mut wide, 0, 0, CellAttributes::default(), "界");
        assert_eq!(fill_ratio(&wide, Rect::full(4, 1)), 0.5);
        assert_eq!(
            fill_ratio(
                &surface,
                Rect {
                    x: 8,
                    y: 8,
                    cols: 2,
                    rows: 2
                }
            ),
            0.0
        );
        assert_eq!(
            fill_ratio(
                &surface,
                Rect {
                    x: 1,
                    y: 0,
                    cols: 0,
                    rows: 2
                }
            ),
            0.0
        );
    }

    #[test]
    fn ansi16_runs_carry_no_truecolor() {
        crate::caps::test_override::with_color(ColorDepth::Ansi16, || {
            let ov = frozen_overlay(MonitorTab::Cpu, Rect::full(100, 32));
            let mut surface = Surface::new(100, 32);
            ov.render(&mut surface, Rect::full(100, 32));
            for run in style_runs(&surface) {
                assert!(!matches!(run.fg, ColorSpec::TrueColor(_)));
                assert!(!matches!(run.bg, ColorSpec::TrueColor(_)));
                for color in [run.fg, run.bg] {
                    assert!(!matches!(color, ColorSpec::PaletteIndex(i) if i >= 16));
                }
            }
        });
    }

    #[test]
    fn graph_cells_sit_on_the_panel_surface_and_keep_their_color() {
        for depth in [ColorDepth::Truecolor, ColorDepth::Ansi256] {
            crate::caps::test_override::with_color(depth, || {
                let (panel, dim, text) = crate::chrome::with_palette(|palette| {
                    (
                        crate::wire::color_spec(crate::chrome::theme_color(&palette.panel), depth),
                        crate::wire::color_spec(crate::chrome::theme_color(&palette.dim), depth),
                        crate::wire::color_spec(crate::chrome::theme_color(&palette.text), depth),
                    )
                });
                assert!(
                    thegn_core::theme::relative_luminance(&crate::chrome::palette_snapshot().panel)
                        < 0.1,
                    "fixture palette must be the dark default"
                );
                assert_ne!(panel, dim);
                for tab in [
                    MonitorTab::Thermal,
                    MonitorTab::Network,
                    MonitorTab::Power,
                    MonitorTab::Overview,
                    MonitorTab::Cpu,
                ] {
                    let overlay = graph_overlay(tab, Rect::full(100, 32));
                    let mut surface = Surface::new(100, 32);
                    overlay.render(&mut surface, Rect::full(100, 32));
                    let mut tab_graph_cells = 0;
                    for (y, line) in surface.screen_lines().iter().enumerate() {
                        for cell in line.visible_cells() {
                            let ch = cell.str().chars().next().unwrap_or(' ');
                            let graph_glyph = ('\u{2580}'..='\u{259F}').contains(&ch)
                                || ('\u{2800}'..='\u{28FF}').contains(&ch);
                            if !graph_glyph {
                                continue;
                            }
                            tab_graph_cells += 1;
                            let x = cell.cell_index();
                            let attrs = cell.attrs();
                            let bg = crate::wire::color_spec(attrs.background(), depth);
                            let fg = crate::wire::color_spec(attrs.foreground(), depth);
                            assert!(
                                bg != dim && bg != text,
                                "{tab:?} ({x},{y}) {ch:?} background {bg:?}"
                            );
                            if ch == '\u{2800}' {
                                assert!(
                                    bg == panel || bg == ColorSpec::Default,
                                    "{tab:?} ({x},{y}) blank graph background {bg:?}"
                                );
                            }
                            if ch != '\u{2800}' && ch != '\u{2591}' {
                                assert!(
                                    fg != ColorSpec::Default && fg != text,
                                    "{tab:?} ({x},{y}) {ch:?} foreground {fg:?}"
                                );
                            }
                        }
                    }
                    assert!(
                        tab_graph_cells > 0,
                        "seeded {tab:?} frame must paint graph cells"
                    );
                }
            });
        }
    }

    #[test]
    fn no_colour_runs_are_default() {
        crate::caps::test_override::with_color(ColorDepth::None, || {
            let ov = frozen_overlay(MonitorTab::Cpu, Rect::full(100, 32));
            let mut surface = Surface::new(100, 32);
            ov.render(&mut surface, Rect::full(100, 32));
            assert!(
                style_runs(&surface)
                    .iter()
                    .all(|run| run.fg == ColorSpec::Default && run.bg == ColorSpec::Default)
            );
        });
    }

    #[test]
    fn truecolor_snapshot_has_truecolor() {
        crate::caps::test_override::with_color(ColorDepth::Truecolor, || {
            let mut surface = Surface::new(2, 1);
            let mut attr = CellAttributes::default();
            attr.set_foreground(ColorAttribute::TrueColorWithDefaultFallback(
                termwiz::color::SrgbaTuple(0.1, 0.2, 0.3, 1.0),
            ));
            paint(&mut surface, 0, 0, attr, "x");
            assert!(
                style_runs(&surface)
                    .iter()
                    .any(|run| matches!(run.fg, ColorSpec::TrueColor(_)))
            );
        });
    }

    #[test]
    fn ascii_level_changes_graph_glyphs() {
        let ascii = crate::caps::test_override::with_unicode(UnicodeLevel::Ascii, || {
            snapshot(
                &frozen_overlay(MonitorTab::Cpu, Rect::full(100, 32)),
                100,
                32,
            )
        });
        let full = crate::caps::test_override::with_unicode(UnicodeLevel::Full, || {
            snapshot(
                &frozen_overlay(MonitorTab::Cpu, Rect::full(100, 32)),
                100,
                32,
            )
        });
        assert_ne!(ascii.0, full.0);
    }
}
