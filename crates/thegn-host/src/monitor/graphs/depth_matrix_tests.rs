//! Unicode-level by color-depth matrix over every graph painter (THE-842).
//!
//! Glyph choice must follow only the unicode level; color depth may change
//! styles but never text. Goldens use `\u{..}` escapes (glyph-literal ratchet).
use super::graph::{GraphKind, GraphPaint, paint_graph};
use super::heatmap::paint_heatmap;
use super::minicore::paint_minicore;
use super::mirrored::paint_mirrored;
use super::stacked::paint_stacked;
use super::{GraphOut, Paint, default_palette};
use crate::caps::test_override::{with_color, with_unicode};
use crate::compositor::Rect;
use crate::seg::{Line, Tok};
use termwiz::color::ColorAttribute;
use thegn_core::termcaps::{ColorDepth, UnicodeLevel};
use thegn_core::theme::{RampId, SeriesRole};

const LEVELS: [UnicodeLevel; 3] = [UnicodeLevel::Ascii, UnicodeLevel::Basic, UnicodeLevel::Full];
const DEPTHS: [ColorDepth; 4] = [
    ColorDepth::Truecolor,
    ColorDepth::Ansi256,
    ColorDepth::Ansi16,
    ColorDepth::None,
];
const ROLES: &[SeriesRole] = &[SeriesRole::CpuUser, SeriesRole::CpuSys];

/// One painter's output: text rows plus whether every run is color-neutral.
struct Case {
    name: &'static str,
    rows: Vec<String>,
    neutral: bool,
}

fn is_default(tok: &Tok) -> bool {
    matches!(tok, Tok::Attr(ColorAttribute::Default))
}

fn case(name: &'static str, out: &GraphOut) -> Case {
    let mut rows = Vec::new();
    let mut neutral = true;
    for line in &out.lines {
        let mut row = String::new();
        if let Line::Segs(segs) = line {
            for seg in segs {
                row.push_str(&seg.text);
                neutral &= is_default(&seg.fg) && seg.bg.as_ref().is_none_or(is_default);
            }
        }
        rows.push(row);
    }
    Case {
        name,
        rows,
        neutral,
    }
}

fn surface_rows(surface: &termwiz::surface::Surface) -> Vec<String> {
    surface
        .screen_lines()
        .iter()
        .map(|line| {
            line.visible_cells()
                .map(|cell| cell.str().to_owned())
                .collect()
        })
        .collect()
}

fn graph(kind: GraphKind, vals: &[f32], w: usize, h: usize) -> GraphOut {
    let mut out = GraphOut::new();
    paint_graph(
        &mut out,
        GraphPaint {
            kind,
            vals,
            lo: None,
            w,
            h,
            paint: Paint::Ramp {
                id: RampId::Util,
                warn: 0.7,
                crit: 0.9,
            },
        },
        &default_palette(),
    );
    out
}

/// Runs every painter at the currently overridden level and depth.
fn paint_all() -> Vec<Case> {
    let palette = default_palette();
    let mut cases = Vec::new();

    cases.push(case("area", &graph(GraphKind::Area, &[1.0], 1, 1)));
    cases.push(case("blocks", &graph(GraphKind::Blocks, &[1.0, 0.0], 2, 1)));

    let mut out = GraphOut::new();
    let bands = [&[0.5][..], &[0.5][..]];
    paint_stacked(&mut out, &bands, ROLES, 1, 2, &palette);
    cases.push(case("stacked", &out));

    let mut out = GraphOut::new();
    paint_mirrored(
        &mut out,
        &[1.0],
        &[1.0],
        1,
        2,
        (RampId::Rx, RampId::Tx),
        &palette,
    );
    cases.push(case("mirrored", &out));

    let mut out = GraphOut::new();
    let heat_rows = [&[0.0, 1.0][..], &[1.0, 0.0][..]];
    paint_heatmap(&mut out, &heat_rows, RampId::Heat, &palette);
    cases.push(case("heatmap", &out));

    let mut out = GraphOut::new();
    let vals = [0.0, 1.0, 0.0, 1.0];
    super::spark::paint_spark(&mut out, &vals, 4, RampId::Mem, &palette);
    cases.push(case("spark", &out));

    let mut out = GraphOut::new();
    paint_minicore(&mut out, "c0", &vals, 42.0, 12, RampId::Util, &palette);
    cases.push(case("minicore", &out));

    let rect = |cols, rows| Rect {
        x: 0,
        y: 0,
        cols,
        rows,
    };
    let mut surface = termwiz::surface::Surface::new(4, 1);
    super::paint_sparkline(&vals, 4, RampId::Mem, rect(4, 1), &mut surface);
    cases.push(Case {
        name: "sparkline",
        rows: surface_rows(&surface),
        neutral: true,
    });
    let mut surface = termwiz::surface::Surface::new(1, 1);
    super::paint_history_graph(&[1.0], RampId::Util, rect(1, 1), &mut surface);
    cases.push(Case {
        name: "history",
        rows: surface_rows(&surface),
        neutral: true,
    });
    cases
}

fn at(level: UnicodeLevel, depth: ColorDepth) -> Vec<Case> {
    with_unicode(level, || with_color(depth, paint_all))
}

fn golden(level: UnicodeLevel) -> Vec<(&'static str, Vec<&'static str>)> {
    match level {
        UnicodeLevel::Ascii => vec![
            ("area", vec!["#"]),
            ("blocks", vec!["# "]),
            ("stacked", vec!["#", "#"]),
            ("mirrored", vec!["#", "#"]),
            ("heatmap", vec![" @", "@ "]),
            ("spark", vec!["_#_#"]),
            ("minicore", vec!["c0 _#_#  42%"]),
            ("sparkline", vec!["_#_#"]),
            ("history", vec!["#"]),
        ],
        UnicodeLevel::Basic | UnicodeLevel::Full => vec![
            ("area", vec!["\u{28f8}"]),
            ("blocks", vec!["\u{2588} "]),
            ("stacked", vec!["\u{2588}", "\u{2588}"]),
            ("mirrored", vec!["\u{28f8}", "\u{28f8}"]),
            ("heatmap", vec!["\u{2580}\u{2580}", "\u{2580}\u{2580}"]),
            ("spark", vec!["\u{2581}\u{2588}\u{2581}\u{2588}"]),
            ("minicore", vec!["c0 \u{2581}\u{2588}\u{2581}\u{2588}  42%"]),
            ("sparkline", vec!["\u{2581}\u{2588}\u{2581}\u{2588}"]),
            ("history", vec!["\u{28f8}"]),
        ],
    }
}

#[test]
fn text_rows_do_not_depend_on_color_depth() {
    for level in LEVELS {
        let base = at(level, ColorDepth::Truecolor);
        for depth in DEPTHS {
            let got = at(level, depth);
            assert_eq!(got.len(), base.len());
            for (a, b) in base.iter().zip(&got) {
                assert_eq!(a.name, b.name);
                assert_eq!(a.rows, b.rows, "{} {level:?} {depth:?}", a.name);
            }
        }
    }
}

#[test]
fn ascii_level_emits_only_ascii_at_every_depth() {
    for depth in DEPTHS {
        for c in at(UnicodeLevel::Ascii, depth) {
            for row in &c.rows {
                assert!(row.is_ascii(), "{} {depth:?}: {row:?}", c.name);
            }
        }
    }
}

#[test]
fn no_color_depth_paints_only_default_attributes() {
    for level in LEVELS {
        for c in at(level, ColorDepth::None) {
            assert!(c.neutral, "{} {level:?} has non-default color", c.name);
        }
    }
}

#[test]
fn rows_never_exceed_requested_width() {
    let widths: [(&str, usize); 9] = [
        ("area", 1),
        ("blocks", 2),
        ("stacked", 1),
        ("mirrored", 1),
        ("heatmap", 2),
        ("spark", 4),
        ("minicore", 12),
        ("sparkline", 4),
        ("history", 1),
    ];
    for level in LEVELS {
        for depth in DEPTHS {
            for c in at(level, depth) {
                let max = widths
                    .iter()
                    .find(|(name, _)| *name == c.name)
                    .map(|(_, w)| *w)
                    .expect("width listed for every painter");
                for row in &c.rows {
                    assert!(
                        crate::seg::cells(row) <= max,
                        "{} {level:?} {depth:?}: {row:?}",
                        c.name
                    );
                }
            }
        }
    }
}

#[test]
fn every_painter_matches_its_golden_at_every_level() {
    for level in LEVELS {
        let want = golden(level);
        let got = at(level, ColorDepth::Truecolor);
        assert_eq!(got.len(), want.len());
        for (c, (name, rows)) in got.iter().zip(&want) {
            assert_eq!(c.name, *name);
            assert_eq!(c.rows, *rows, "{name} {level:?}");
        }
    }
}
