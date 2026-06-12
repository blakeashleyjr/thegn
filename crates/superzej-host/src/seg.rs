//! The seg/line-spec layer: chrome rows as colored segment lists, rendered
//! into a `Surface` rect with left/right alignment, truncation, and padding —
//! the Rust translation of the design mockup's `S()/fit()/{l,r}` model.
//!
//! A [`Line`] fits to exactly `w` cells: the right side wins space, the left
//! truncates with a ghost `…`, and padding (plus any seg without an explicit
//! bg) is painted in the line's pad background — so a tinted row colors its
//! whole width with one call.
//!
//! Cell math is `chars().count()`, matching the rest of chrome; the viz
//! primitives (blocks, braille, box drawing) are all width-1. CJK/emoji in
//! segs is out of contract.
#![allow(dead_code)] // the full mockup style vocabulary (italic/strike/Rgb/underline) — kept whole even where chrome hasn't adopted a token yet

use termwiz::cell::{CellAttributes, Intensity, Underline};
use termwiz::color::ColorAttribute;
use termwiz::surface::{Change, Position, Surface};

use crate::chrome::{self, S};
use crate::compositor::Rect;
use superzej_core::theme::{self, Palette};

/// Whether the outer terminal renders curly underlines; when false, [`Under::Curly`]
/// degrades to a single underline at draw time so the surface never holds an
/// attribute the wire can't express. Installed at startup by the renderer.
static UNDERCURL: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(true);

pub fn set_undercurl_supported(on: bool) {
    UNDERCURL.store(on, std::sync::atomic::Ordering::Relaxed);
}

fn undercurl_supported() -> bool {
    UNDERCURL.load(std::sync::atomic::Ordering::Relaxed)
}

/// A resolvable color token. Resolution reads the live palette once per line.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Tok {
    /// A palette slot (the chrome vocabulary).
    Slot(S),
    /// One of the eight semantic hues.
    Hue(theme::Hue),
    /// Heat-ramp level 0..=4.
    Heat(u8),
    /// The accent selection-row tint (mockup x-sel).
    SelAccent,
    /// A hue-tinted selection row at `percent` alpha (mockup x-selm/x-selr).
    Sel(theme::Hue, u8),
    /// An explicit color — escape hatch for precomputed blends.
    Rgb(u8, u8, u8),
    /// An already-resolved termwiz color (bridges legacy color-returning code).
    Attr(ColorAttribute),
}

impl Tok {
    /// Resolve against a borrowed palette (callers hold the lock once).
    pub fn resolve(&self, p: &Palette) -> ColorAttribute {
        match self {
            Tok::Slot(s) => chrome::theme_color(chrome::slot_rgb(p, *s)),
            Tok::Hue(h) => chrome::theme_color(p.hue(*h)),
            Tok::Heat(l) => chrome::theme_color(p.heat(*l as usize)),
            Tok::SelAccent => chrome::theme_color(&p.sel_accent()),
            Tok::Sel(h, pct) => chrome::theme_color(&p.sel(*h, *pct as f32 / 100.0)),
            Tok::Rgb(r, g, b) => chrome::theme_color(&format!("{r};{g};{b}")),
            Tok::Attr(c) => *c,
        }
    }
}

/// Underline styling for a seg.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Under {
    #[default]
    None,
    Single,
    /// Curly underline in the seg's own fg color.
    Curly,
    /// Curly underline in a hue (the mockup's red/blue squiggles under
    /// normal-colored text).
    CurlyHue(theme::Hue),
}

/// One run of styled text within a line.
#[derive(Debug, Clone, PartialEq)]
pub struct Seg {
    pub text: String,
    pub fg: Tok,
    /// `None` inherits the line's pad background (the mockup's bgify
    /// semantics) — explicit bgs (chips, selection tints) opt out.
    pub bg: Option<Tok>,
    pub bold: bool,
    pub italic: bool,
    pub strike: bool,
    pub under: Under,
}

/// The standard constructor: `seg(Tok::Slot(S::Dim), "text")`.
pub fn seg(fg: Tok, text: impl Into<String>) -> Seg {
    Seg {
        text: text.into(),
        fg,
        bg: None,
        bold: false,
        italic: false,
        strike: false,
        under: Under::None,
    }
}

/// `n` spaces with inherited background.
pub fn sp(n: usize) -> Seg {
    seg(Tok::Slot(S::Text), " ".repeat(n))
}

impl Seg {
    pub fn bold(mut self) -> Seg {
        self.bold = true;
        self
    }
    pub fn italic(mut self) -> Seg {
        self.italic = true;
        self
    }
    pub fn strike(mut self) -> Seg {
        self.strike = true;
        self
    }
    pub fn under(mut self, u: Under) -> Seg {
        self.under = u;
        self
    }
    pub fn bg(mut self, bg: Tok) -> Seg {
        self.bg = Some(bg);
        self
    }

    /// An inverse chip: bold chip-fg text on a filled token background —
    /// `Seg::chip(Tok::Slot(S::Accent), " N ")` (mockup x-inv*).
    pub fn chip(bg: Tok, text: impl Into<String>) -> Seg {
        seg(Tok::Slot(S::ChipFg), text).bg(bg).bold()
    }

    fn width(&self) -> usize {
        self.text.chars().count()
    }
}

/// A row spec, fitted to exactly `w` cells by [`draw_line`].
#[derive(Debug, Clone, PartialEq, Default)]
pub enum Line {
    /// An empty row (pure pad background).
    #[default]
    Blank,
    /// Left-aligned segments.
    Segs(Vec<Seg>),
    /// Left and right clusters; right wins space, left truncates.
    Split { l: Vec<Seg>, r: Vec<Seg> },
    /// A horizontal rule: `ch` repeated across the width.
    Fill { ch: char, fg: Tok },
}

impl Line {
    pub fn segs(segs: impl Into<Vec<Seg>>) -> Line {
        Line::Segs(segs.into())
    }
    pub fn split(l: impl Into<Vec<Seg>>, r: impl Into<Vec<Seg>>) -> Line {
        Line::Split {
            l: l.into(),
            r: r.into(),
        }
    }
}

/// Total cell width of a seg run.
pub fn seg_width(segs: &[Seg]) -> usize {
    segs.iter().map(Seg::width).sum()
}

/// Truncate a seg run to at most `max` cells, ending with a ghost `…` when
/// anything was cut (the mockup's cutSegs).
pub(crate) fn cut(segs: &[Seg], max: usize) -> Vec<Seg> {
    if max == 0 {
        return Vec::new();
    }
    if seg_width(segs) <= max {
        return segs.to_vec();
    }
    let mut out = Vec::new();
    let mut used = 0usize;
    for s in segs {
        let w = s.width();
        if used + w <= max.saturating_sub(1) {
            out.push(s.clone());
            used += w;
            continue;
        }
        let room = max - 1 - used;
        if room > 0 {
            let mut clipped = s.clone();
            clipped.text = s.text.chars().take(room).collect();
            out.push(clipped);
        }
        out.push(seg(Tok::Slot(S::Ghost), "…"));
        return out;
    }
    out
}

fn attrs_for(s: &Seg, p: &Palette, pad_bg: Tok) -> CellAttributes {
    let mut a = CellAttributes::default();
    a.set_foreground(s.fg.resolve(p));
    a.set_background(s.bg.unwrap_or(pad_bg).resolve(p));
    if s.bold {
        a.set_intensity(Intensity::Bold);
    }
    if s.italic {
        a.set_italic(true);
    }
    if s.strike {
        a.set_strikethrough(true);
    }
    match s.under {
        Under::None => {}
        Under::Single => {
            a.set_underline(Underline::Single);
        }
        Under::Curly => {
            a.set_underline(if undercurl_supported() {
                Underline::Curly
            } else {
                Underline::Single
            });
        }
        Under::CurlyHue(h) => {
            a.set_underline(if undercurl_supported() {
                Underline::Curly
            } else {
                Underline::Single
            });
            a.set_underline_color(chrome::theme_color(p.hue(h)));
        }
    }
    a
}

fn emit(surface: &mut Surface, x: usize, y: usize, segs: &[Seg], p: &Palette, pad_bg: Tok) {
    surface.add_change(Change::CursorPosition {
        x: Position::Absolute(x),
        y: Position::Absolute(y),
    });
    for s in segs {
        if s.text.is_empty() {
            continue;
        }
        surface.add_change(Change::AllAttributes(attrs_for(s, p, pad_bg)));
        surface.add_change(Change::Text(s.text.clone()));
    }
    surface.add_change(Change::AllAttributes(CellAttributes::default()));
}

/// Render one line spec into exactly `w` cells at `(x, y)`. Padding and
/// bg-less segs are painted in `pad_bg`.
pub fn draw_line(surface: &mut Surface, x: usize, y: usize, w: usize, line: &Line, pad_bg: Tok) {
    if w == 0 {
        return;
    }
    chrome::with_palette(|p| {
        let pad = |n: usize| sp(n);
        let fitted: Vec<Seg> = match line {
            Line::Blank => vec![pad(w)],
            Line::Fill { ch, fg } => {
                vec![seg(*fg, ch.to_string().repeat(w))]
            }
            Line::Segs(l) => {
                let l = cut(l, w);
                let used = seg_width(&l);
                let mut v = l;
                v.push(pad(w - used));
                v
            }
            Line::Split { l, r } => {
                let r = cut(r, w);
                let rl = seg_width(&r);
                let avail = w.saturating_sub(rl + if rl > 0 { 1 } else { 0 });
                let l = cut(l, avail);
                let ll = seg_width(&l);
                let mut v = l;
                v.push(pad(w - ll - rl));
                v.extend(r);
                v
            }
        };
        emit(surface, x, y, &fitted, p, pad_bg);
    });
}

/// Render a stack of lines into `rect`, one per row; missing rows are blank.
/// Lines beyond the rect's height are dropped.
pub fn draw_lines(surface: &mut Surface, rect: Rect, lines: &[Line], pad_bg: Tok) {
    for row in 0..rect.rows {
        let line = lines.get(row).unwrap_or(&Line::Blank);
        draw_line(surface, rect.x, rect.y + row, rect.cols, line, pad_bg);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row_text(s: &mut Surface, y: usize) -> String {
        s.screen_cells()[y].iter().map(|c| c.str()).collect()
    }

    fn cell_attrs(s: &mut Surface, x: usize, y: usize) -> CellAttributes {
        s.screen_cells()[y][x].attrs().clone()
    }

    const PAD: Tok = Tok::Slot(S::Bg1);

    #[test]
    fn blank_line_fills_width() {
        let mut s = Surface::new(8, 1);
        draw_line(&mut s, 0, 0, 8, &Line::Blank, PAD);
        assert_eq!(row_text(&mut s, 0), "        ");
    }

    #[test]
    fn fill_line_repeats_rule_char() {
        let mut s = Surface::new(6, 1);
        let line = Line::Fill {
            ch: '─',
            fg: Tok::Slot(S::Ghost3),
        };
        draw_line(&mut s, 0, 0, 6, &line, PAD);
        assert_eq!(row_text(&mut s, 0), "──────");
    }

    #[test]
    fn segs_left_aligned_and_padded() {
        let mut s = Surface::new(10, 1);
        let line = Line::segs(vec![
            seg(Tok::Slot(S::Text), "ab"),
            seg(Tok::Slot(S::Dim), "cd"),
        ]);
        draw_line(&mut s, 0, 0, 10, &line, PAD);
        assert_eq!(row_text(&mut s, 0), "abcd      ");
    }

    #[test]
    fn split_right_aligns_and_left_truncates_with_ellipsis() {
        let mut s = Surface::new(12, 1);
        let line = Line::split(
            vec![seg(Tok::Slot(S::Text), "left-side-too-long")],
            vec![seg(Tok::Slot(S::Dim), "RR")],
        );
        draw_line(&mut s, 0, 0, 12, &line, PAD);
        // 12 cells: right "RR" + 1-space gap → 9 for left → 8 chars + "…".
        assert_eq!(row_text(&mut s, 0), "left-sid… RR");
    }

    #[test]
    fn split_right_wins_space() {
        let mut s = Surface::new(6, 1);
        let line = Line::split(
            vec![seg(Tok::Slot(S::Text), "abcdef")],
            vec![seg(Tok::Slot(S::Dim), "wide-right")],
        );
        draw_line(&mut s, 0, 0, 6, &line, PAD);
        // Right truncates to 6 ("wide-…"), left gets nothing.
        assert_eq!(row_text(&mut s, 0), "wide-…");
    }

    #[test]
    fn exact_fit_keeps_all_text() {
        let mut s = Surface::new(7, 1);
        let line = Line::split(
            vec![seg(Tok::Slot(S::Text), "ab")],
            vec![seg(Tok::Slot(S::Dim), "cdef")],
        );
        draw_line(&mut s, 0, 0, 7, &line, PAD);
        assert_eq!(row_text(&mut s, 0), "ab cdef");
    }

    #[test]
    fn pad_bg_paints_padding_and_bgless_segs() {
        let mut s = Surface::new(6, 1);
        let line = Line::segs(vec![
            seg(Tok::Slot(S::Text), "a"),
            Seg::chip(Tok::Slot(S::Accent), "C"),
        ]);
        draw_line(&mut s, 0, 0, 6, &line, Tok::Rgb(1, 2, 3));
        let pad_attr = cell_attrs(&mut s, 5, 0);
        let a_attr = cell_attrs(&mut s, 0, 0);
        let chip_attr = cell_attrs(&mut s, 1, 0);
        // The bg-less seg and the padding share the pad bg.
        assert_eq!(a_attr.background(), pad_attr.background());
        // The chip carries its own bg.
        assert_ne!(chip_attr.background(), pad_attr.background());
        assert_eq!(chip_attr.intensity(), Intensity::Bold);
    }

    #[test]
    fn attribute_plumb_bold_italic_strike_under() {
        let mut s = Surface::new(8, 1);
        let line = Line::segs(vec![
            seg(Tok::Slot(S::Text), "b").bold(),
            seg(Tok::Slot(S::Text), "i").italic(),
            seg(Tok::Slot(S::Text), "s").strike(),
            seg(Tok::Slot(S::Text), "u").under(Under::Single),
            seg(Tok::Slot(S::Text), "c").under(Under::Curly),
            seg(Tok::Slot(S::Text), "h").under(Under::CurlyHue(theme::Hue::Red)),
        ]);
        draw_line(&mut s, 0, 0, 8, &line, PAD);
        assert_eq!(cell_attrs(&mut s, 0, 0).intensity(), Intensity::Bold);
        assert!(cell_attrs(&mut s, 1, 0).italic());
        assert!(cell_attrs(&mut s, 2, 0).strikethrough());
        assert_eq!(cell_attrs(&mut s, 3, 0).underline(), Underline::Single);
        assert_eq!(cell_attrs(&mut s, 4, 0).underline(), Underline::Curly);
        let h = cell_attrs(&mut s, 5, 0);
        assert_eq!(h.underline(), Underline::Curly);
        assert_ne!(h.underline_color(), ColorAttribute::Default);
        // Past the styled run, padding is back to plain.
        assert_eq!(cell_attrs(&mut s, 7, 0).intensity(), Intensity::Normal);
    }

    #[test]
    fn undercurl_degrades_when_unsupported() {
        set_undercurl_supported(false);
        let mut s = Surface::new(4, 1);
        let line = Line::segs(vec![
            seg(Tok::Slot(S::Text), "c").under(Under::Curly),
            seg(Tok::Slot(S::Text), "h").under(Under::CurlyHue(theme::Hue::Blue)),
        ]);
        draw_line(&mut s, 0, 0, 4, &line, PAD);
        assert_eq!(cell_attrs(&mut s, 0, 0).underline(), Underline::Single);
        assert_eq!(cell_attrs(&mut s, 1, 0).underline(), Underline::Single);
        set_undercurl_supported(true);
    }

    #[test]
    fn token_resolution_covers_every_variant() {
        chrome::with_palette(|p| {
            for tok in [
                Tok::Slot(S::Accent),
                Tok::Hue(theme::Hue::Magenta),
                Tok::Heat(3),
                Tok::SelAccent,
                Tok::Sel(theme::Hue::Red, 14),
                Tok::Rgb(9, 8, 7),
            ] {
                assert_ne!(tok.resolve(p), ColorAttribute::Default, "{tok:?}");
            }
        });
    }

    #[test]
    fn draw_lines_fills_rect_and_drops_overflow() {
        let mut s = Surface::new(5, 3);
        let rect = Rect {
            x: 0,
            y: 0,
            cols: 5,
            rows: 2,
        };
        let lines = vec![
            Line::segs(vec![seg(Tok::Slot(S::Text), "one")]),
            Line::segs(vec![seg(Tok::Slot(S::Text), "two")]),
            Line::segs(vec![seg(Tok::Slot(S::Text), "DROPPED")]),
        ];
        draw_lines(&mut s, rect, &lines, PAD);
        assert_eq!(row_text(&mut s, 0), "one  ");
        assert_eq!(row_text(&mut s, 1), "two  ");
        assert_eq!(row_text(&mut s, 2), "     "); // untouched row
    }

    #[test]
    fn zero_width_and_empty_inputs_never_panic() {
        let mut s = Surface::new(4, 1);
        draw_line(&mut s, 0, 0, 0, &Line::Blank, PAD);
        draw_line(&mut s, 0, 0, 4, &Line::segs(Vec::<Seg>::new()), PAD);
        draw_line(
            &mut s,
            0,
            0,
            1,
            &Line::split(
                vec![seg(Tok::Slot(S::Text), "abc")],
                vec![seg(Tok::Slot(S::Text), "def")],
            ),
            PAD,
        );
        assert_eq!(seg_width(&[]), 0);
        assert_eq!(cut(&[seg(Tok::Slot(S::Text), "abc")], 0), Vec::<Seg>::new());
    }
}
