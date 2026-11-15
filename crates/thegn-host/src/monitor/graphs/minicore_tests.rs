use super::minicore::paint_minicore;
use super::{GraphOut, default_palette, reset_resolution_count, resolution_count, table};
use crate::caps::test_override;
use crate::seg::{Line, Tok};
use termwiz::color::ColorAttribute;
use thegn_core::termcaps::{ColorDepth, UnicodeLevel};
use thegn_core::theme::RampId;

fn row_text(out: &GraphOut) -> String {
    match out.lines.first() {
        Some(Line::Segs(segs)) => segs.iter().map(|seg| seg.text.as_str()).collect::<String>(),
        _ => String::new(),
    }
}

fn paint(out: &mut GraphOut, label: &str, vals: &[f32], pct: f64, w: usize) {
    let palette = default_palette();
    paint_minicore(out, label, vals, pct, w, RampId::Util, &palette);
}

#[test]
fn minicore_never_exceeds_width_for_any_width() {
    let vals = [0.0, 0.25, 0.5, 0.75, 1.0, 0.5, 0.1, 0.9, 0.3, 0.6];
    for level in [UnicodeLevel::Full, UnicodeLevel::Basic, UnicodeLevel::Ascii] {
        test_override::with_unicode(level, || {
            for label in ["", "c0", "a very long label indeed"] {
                for w in 0..=40 {
                    let mut out = GraphOut::new();
                    paint(&mut out, label, &vals, 42.0, w);
                    let text = row_text(&out);
                    assert!(
                        crate::seg::cells(&text) <= w,
                        "w={w} label={label:?} row={text:?}"
                    );
                    if w == 0 {
                        assert!(text.is_empty());
                    }
                }
            }
        });
    }
}

#[test]
fn minicore_ascii_golden_row() {
    test_override::with_unicode(UnicodeLevel::Ascii, || {
        test_override::with_color(ColorDepth::Truecolor, || {
            let mut out = GraphOut::new();
            paint(&mut out, "c0", &[0.0, 1.0], 42.0, 12);
            assert_eq!(row_text(&out), "c0   _#  42%");
        });
    });
}

#[test]
fn minicore_goldens_follow_unicode_tables() {
    for level in [UnicodeLevel::Full, UnicodeLevel::Basic, UnicodeLevel::Ascii] {
        test_override::with_unicode(level, || {
            test_override::with_color(ColorDepth::Truecolor, || {
                let spark = table().spark;
                let low = spark[0];
                let high = spark[spark.len() - 1];
                let mut out = GraphOut::new();
                paint(&mut out, "c0", &[0.0, 1.0], 42.0, 12);
                assert_eq!(row_text(&out), format!("c0   {low}{high}  42%"));
            });
        });
    }
}

#[test]
fn minicore_unicode_full_glyphs_are_escaped_blocks() {
    test_override::with_unicode(UnicodeLevel::Full, || {
        test_override::with_color(ColorDepth::Truecolor, || {
            let mut out = GraphOut::new();
            paint(&mut out, "", &[1.0], 100.0, 8);
            let text = row_text(&out);
            assert!(text.ends_with("100%"), "{text:?}");
            assert!(text.contains('\u{2588}'), "{text:?}");
        });
    });
}

#[test]
fn minicore_no_color_depth_keeps_text_with_default_attrs() {
    test_override::with_unicode(UnicodeLevel::Ascii, || {
        test_override::with_color(ColorDepth::None, || {
            let mut out = GraphOut::new();
            paint(&mut out, "c0", &[0.0, 1.0], 42.0, 12);
            assert_eq!(row_text(&out), "c0   _#  42%");
            let Line::Segs(segs) = &out.lines[0] else {
                panic!("expected segs")
            };
            for run in segs.iter().filter(|run| run.text.contains(['c', '4'])) {
                assert_eq!(run.fg, Tok::Attr(ColorAttribute::Default), "{run:?}");
            }
        });
    });
}

#[test]
fn minicore_resolves_palette_once_per_paint() {
    test_override::with_color(ColorDepth::Truecolor, || {
        let mut out = GraphOut::new();
        reset_resolution_count();
        paint(&mut out, "c0", &[0.2, 0.8, 0.5], 50.0, 20);
        assert_eq!(resolution_count(), 1);
        paint(&mut out, "c0", &[0.2, 0.8, 0.5], 50.0, 20);
        assert_eq!(resolution_count(), 2);
    });
}

#[test]
fn minicore_percent_is_right_aligned_and_clamped() {
    test_override::with_unicode(UnicodeLevel::Ascii, || {
        for (pct, tail) in [
            (250.0, "100%"),
            (-5.0, "  0%"),
            (f64::NAN, "  0%"),
            (7.0, "  7%"),
            (100.0, "100%"),
        ] {
            let mut out = GraphOut::new();
            paint(&mut out, "c", &[0.5; 6], pct, 16);
            let text = row_text(&out);
            assert_eq!(crate::seg::cells(&text), 16, "{text:?}");
            assert!(text.ends_with(tail), "pct={pct} row={text:?}");
        }
    });
}

#[test]
fn minicore_degrades_sparkline_then_label() {
    test_override::with_unicode(UnicodeLevel::Ascii, || {
        let vals = [0.5; 8];
        let mut out = GraphOut::new();
        // Room for label + percent but not a sparkline.
        paint(&mut out, "cpu", &vals, 42.0, 9);
        assert_eq!(row_text(&out), "cpu   42%");
        // Label (w/3 = 1 cell) plus percent no longer fits: percent only.
        paint(&mut out, "cpu", &vals, 42.0, 5);
        assert_eq!(row_text(&out), "  42%");
        paint(&mut out, "cpu", &vals, 42.0, 4);
        assert_eq!(row_text(&out), " 42%");
        paint(&mut out, "cpu", &vals, 42.0, 0);
        assert_eq!(row_text(&out), "");
    });
}

#[test]
fn minicore_label_is_clipped_to_a_third_of_the_width() {
    test_override::with_unicode(UnicodeLevel::Ascii, || {
        let mut out = GraphOut::new();
        paint(&mut out, "abcdefghij", &[0.5; 30], 42.0, 30);
        let text = row_text(&out);
        assert!(text.starts_with("abcdefghij "), "{text:?}");
        paint(&mut out, "abcdefghij", &[0.5; 30], 42.0, 15);
        let text = row_text(&out);
        assert!(text.starts_with("abcde "), "{text:?}");
        assert!(!text.starts_with("abcdef"), "{text:?}");
    });
}

#[test]
fn minicore_repeat_paint_reuses_storage() {
    test_override::with_unicode(UnicodeLevel::Ascii, || {
        let vals = [0.2, 0.9];
        let mut out = GraphOut::new();
        paint(&mut out, "c0", &vals, 42.0, 14);
        paint(&mut out, "c0", &vals, 42.0, 14);
        let first = row_text(&out);
        let seg_capacity = match &out.lines[0] {
            Line::Segs(segs) => segs.capacity(),
            _ => panic!("expected segs"),
        };
        let line_capacity = out.lines.capacity();
        let spare_capacity = out.spare_strings.capacity();
        for _ in 0..50 {
            paint(&mut out, "c0", &vals, 42.0, 14);
        }
        assert_eq!(row_text(&out), first);
        let Line::Segs(segs) = &out.lines[0] else {
            panic!("expected segs")
        };
        assert_eq!(segs.capacity(), seg_capacity);
        assert_eq!(out.lines.capacity(), line_capacity);
        assert_eq!(out.spare_strings.capacity(), spare_capacity);
    });
}
