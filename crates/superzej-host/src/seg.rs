//! The seg/line-spec layer: chrome rows as colored segment lists, rendered
//! into a `Surface` rect with left/right alignment, truncation, and padding —
//! the Rust translation of the design mockup's `S()/fit()/{l,r}` model.
//!
//! A [`Line`] fits to exactly `w` cells: the right side wins space, the left
//! truncates with a ghost `…`, and padding (plus any seg without an explicit
//! bg) is painted in the line's pad background — so a tinted row colors its
//! whole width with one call.
//!
//! Cell math is **display width** (`unicode-width`): a wide glyph (CJK, many
//! emoji) advances two cells, so a line that measures `w` paints exactly `w`
//! columns and truncation never splits a wide glyph across the right edge. The
//! viz primitives (blocks, braille, box drawing) are width-1.
#![allow(dead_code)] // the full mockup style vocabulary (italic/strike/Rgb/underline) — kept whole even where chrome hasn't adopted a token yet

use termwiz::cell::{CellAttributes, Intensity, Underline};
use termwiz::color::ColorAttribute;
use termwiz::surface::{Change, Position, Surface};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

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
    ///
    /// Note: `chip_fg` is near-`bg0`, so this only reads on a *bright* token
    /// (accent or a semantic hue). For a chip on a dark surface (a keycap, a
    /// badge) use [`Seg::key`] — `chip` there is dark-on-dark and unreadable.
    pub fn chip(bg: Tok, text: impl Into<String>) -> Seg {
        seg(Tok::Slot(S::ChipFg), text).bg(bg).bold()
    }

    /// A neutral "keycap" chip for dark surfaces: legible bright text on the
    /// raised surface. The counterpart to [`Seg::chip`] for key hints and
    /// badges, where the inverse chip's near-black `chip_fg` would vanish into
    /// the dark `raise` fill.
    pub fn key(text: impl Into<String>) -> Seg {
        seg(Tok::Slot(S::Text), text).bg(Tok::Slot(S::Raise)).bold()
    }

    fn width(&self) -> usize {
        self.text.width()
    }
}

/// Return the longest prefix of `s` whose display width is `<= max`, never
/// splitting a wide glyph across the boundary. The returned width is therefore
/// `<= max` and may be one cell short when a 2-wide glyph straddles the edge
/// (the caller pads the gap). Ambiguous-width glyphs count as 1, matching most
/// terminals.
pub(crate) fn take_cols(s: &str, max: usize) -> &str {
    let mut used = 0usize;
    for (i, ch) in s.char_indices() {
        let w = ch.width().unwrap_or(0);
        if used + w > max {
            return &s[..i];
        }
        used += w;
    }
    s
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

/// Truncate a seg run to at most `max` cells, ending with a ghost ellipsis
/// when anything was cut (the mockup's cutSegs). The ellipsis comes from the
/// capability glyph table (`…`, or `...` under ASCII caps), so a truncated
/// line degrades with the rest of the chrome; when the fallback is wider than
/// the available space, the run hard-clips instead.
pub(crate) fn cut(segs: &[Seg], max: usize) -> Vec<Seg> {
    if max == 0 {
        return Vec::new();
    }
    if seg_width(segs) <= max {
        return segs.to_vec();
    }
    let ell = crate::caps::active_glyphs().ellipsis;
    let ell_seg = seg(Tok::Slot(S::Ghost), ell);
    let ell_w = ell_seg.width();
    // No room for the marker itself (e.g. "..." into 2 cells): hard-clip.
    let keep = if ell_w >= max { max } else { max - ell_w };
    let mut out = Vec::new();
    let mut used = 0usize;
    for s in segs {
        let w = s.width();
        if used + w <= keep {
            out.push(s.clone());
            used += w;
            continue;
        }
        let room = keep - used;
        if room > 0 {
            let mut clipped = s.clone();
            clipped.text = take_cols(&s.text, room).to_string();
            out.push(clipped);
        }
        if ell_w < max {
            out.push(ell_seg);
        }
        return out;
    }
    out
}

/// Flow a seg run into physical lines no wider than `width` display cells,
/// wrapping (rather than truncating) long content. The first line starts at
/// column 0; every continuation line is prefixed with `sp(cont_indent)` spaces
/// (a hanging indent) so wrapped text aligns under the item's primary text
/// rather than the leading gutter. Breaks on ASCII spaces where a word fits; a
/// single token wider than the line body is hard-split on a display-width
/// boundary (never mid-wide-glyph) via [`take_cols`]. Each seg's fg/bg/style is
/// preserved across splits. Always returns at least one line.
pub(crate) fn wrap(segs: &[Seg], width: usize, cont_indent: usize) -> Vec<Vec<Seg>> {
    if width == 0 {
        return vec![Vec::new()];
    }
    let indent = cont_indent.min(width - 1);
    let mut lines: Vec<Vec<Seg>> = Vec::new();
    let mut cur: Vec<Seg> = Vec::new();
    let mut cur_w = 0usize; // display width of `cur`, including any indent
    let mut line_indent = 0usize; // the current line's hanging indent (0 on line 0)

    // Close the current line and seed a fresh continuation line.
    macro_rules! break_line {
        () => {{
            lines.push(std::mem::take(&mut cur));
            line_indent = indent;
            cur_w = indent;
            if indent > 0 {
                cur.push(sp(indent));
            }
        }};
    }

    for s in segs {
        if s.text.is_empty() {
            continue;
        }
        // Walk atoms: alternating runs of spaces and non-spaces, preserving the
        // item's own leading gutter (spaces are dropped only when they would
        // lead a wrapped continuation line).
        for (is_space, atom) in atoms(&s.text) {
            if is_space {
                // Drop spaces that would lead a continuation line (the hanging
                // indent stands in for them); keep the gutter on line 0.
                if !lines.is_empty() && cur_w == line_indent {
                    continue;
                }
                let room = width.saturating_sub(cur_w);
                let take = atom.len().min(room); // spaces are 1 byte / 1 cell
                if take > 0 {
                    cur.push(seg_like(s, &atom[..take]));
                    cur_w += take;
                }
                continue;
            }
            // A word (non-space run).
            let ww = atom.width();
            if ww <= width.saturating_sub(cur_w) {
                cur.push(seg_like(s, atom));
                cur_w += ww;
                continue;
            }
            // Doesn't fit here. If the whole word fits on a fresh continuation
            // line, move it down intact (word wrap).
            if ww <= width.saturating_sub(indent) && cur_w > line_indent {
                break_line!();
                cur.push(seg_like(s, atom));
                cur_w += ww;
                continue;
            }
            // A token wider than the line body: hard-split, filling the current
            // line's remaining room first, then across continuation lines.
            let mut rest = atom;
            loop {
                let room = width.saturating_sub(cur_w);
                let chunk = take_cols(rest, room);
                if chunk.is_empty() {
                    // No room for even one cell (a 2-wide glyph with 1 left);
                    // break to a fresh line and retry.
                    break_line!();
                    continue;
                }
                cur.push(seg_like(s, chunk));
                cur_w += chunk.width();
                rest = &rest[chunk.len()..];
                if rest.is_empty() {
                    break;
                }
                break_line!();
            }
        }
    }
    // Flush the final line. Suppress a trailing pure-indent line, but always
    // return at least one line.
    if cur_w > line_indent || lines.is_empty() {
        lines.push(cur);
    }
    lines
}

/// Clone `s` with replacement text, preserving all styling.
fn seg_like(s: &Seg, text: &str) -> Seg {
    let mut c = s.clone();
    c.text = text.to_string();
    c
}

/// Split `s` into maximal alternating runs of spaces / non-spaces, yielding
/// `(is_space, run)`. ASCII space is the only separator.
fn atoms(s: &str) -> Vec<(bool, &str)> {
    let mut out: Vec<(bool, &str)> = Vec::new();
    let mut start = 0usize;
    let mut prev: Option<bool> = None;
    for (i, ch) in s.char_indices() {
        let is_space = ch == ' ';
        if let Some(p) = prev
            && p != is_space
        {
            out.push((p, &s[start..i]));
            start = i;
        }
        prev = Some(is_space);
    }
    if let Some(p) = prev {
        out.push((p, &s[start..]));
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

/// Test-only legibility scanner: walk a rendered surface and return every
/// *text* cell whose foreground/background contrast (WCAG, via
/// [`superzej_core::theme::contrast_ratio`]) falls below `min`, as
/// `(x, y, glyph, ratio)`.
///
/// Decorative glyphs — spaces, box-drawing, block/shade elements, braille
/// (spinners), arrows, and a few geometric markers — are exempt: the contract
/// is "readable text is readable", not "every separator meets body-text
/// contrast". Cells whose colors don't resolve to truecolor (terminal
/// defaults) are skipped; all chrome tokens resolve to truecolor.
#[cfg(test)]
pub(crate) fn text_contrast_violations(
    surface: &mut Surface,
    min: f32,
) -> Vec<(usize, usize, String, f32)> {
    fn rgb(c: ColorAttribute) -> Option<String> {
        match c {
            ColorAttribute::TrueColorWithDefaultFallback(t)
            | ColorAttribute::TrueColorWithPaletteFallback(t, _) => Some(format!(
                "{};{};{}",
                (t.0 * 255.0).round() as u8,
                (t.1 * 255.0).round() as u8,
                (t.2 * 255.0).round() as u8,
            )),
            _ => None,
        }
    }
    fn decorative(s: &str) -> bool {
        !s.is_empty()
            && s.chars().all(|c| {
                c.is_whitespace()
                    || ('\u{2190}'..='\u{21FF}').contains(&c) // arrows
                    || ('\u{2500}'..='\u{259F}').contains(&c) // box drawing + blocks/shades
                    || ('\u{25A0}'..='\u{25FF}').contains(&c) // geometric shapes (● ○ ◆ ◈)
                    || ('\u{2800}'..='\u{28FF}').contains(&c) // braille (spinners)
                    || matches!(c, '·' | '•' | '…' | '±' | '↵')
            })
    }
    let mut out = Vec::new();
    for (y, row) in surface.screen_cells().iter().enumerate() {
        for (x, cell) in row.iter().enumerate() {
            let glyph = cell.str();
            if decorative(glyph) {
                continue;
            }
            let (Some(fg), Some(bg)) = (
                rgb(cell.attrs().foreground()),
                rgb(cell.attrs().background()),
            ) else {
                continue;
            };
            let r = theme::contrast_ratio(&fg, &bg);
            if r < min {
                out.push((x, y, glyph.to_string(), r));
            }
        }
    }
    out
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
    fn take_cols_counts_display_width() {
        // ASCII: one column per char.
        assert_eq!(take_cols("hello", 3), "hel");
        // Wide glyphs (CJK) advance two columns each.
        assert_eq!(take_cols("\u{4f60}\u{597d}", 4), "\u{4f60}\u{597d}"); // 你好 = 4 cols
        assert_eq!(take_cols("\u{4f60}\u{597d}", 3), "\u{4f60}"); // only 你 fits in 3
        // A wide glyph straddling the boundary is dropped, not split.
        assert_eq!(take_cols("\u{4f60}\u{597d}", 2), "\u{4f60}");
        assert_eq!(take_cols("\u{4f60}\u{597d}", 1), ""); // 你 needs 2, won't split
    }

    #[test]
    fn seg_width_is_display_width() {
        assert_eq!(seg(Tok::Slot(S::Text), "ab").width(), 2);
        assert_eq!(seg(Tok::Slot(S::Text), "\u{4f60}\u{597d}").width(), 4);
    }

    fn line_text(l: &[Seg]) -> String {
        l.iter().map(|s| s.text.clone()).collect()
    }

    #[test]
    fn wrap_flows_ascii_on_word_boundaries() {
        let t = Tok::Slot(S::Text);
        let lines = wrap(&[seg(t, "aaaa bbbb cccc")], 9, 2);
        // Each physical line fits the width.
        for l in &lines {
            assert!(seg_width(l) <= 9, "{:?}", line_text(l));
        }
        // First line has no indent, breaks on the space; continuation carries 2.
        assert_eq!(line_text(&lines[0]), "aaaa bbbb");
        assert!(lines.len() >= 2);
        assert!(
            line_text(&lines[1]).starts_with("  "),
            "continuation indent: {:?}",
            line_text(&lines[1])
        );
    }

    #[test]
    fn wrap_hard_splits_long_unbroken_token() {
        let t = Tok::Slot(S::Text);
        let path = "src/averylongpath/no/spaces/here.rs";
        let lines = wrap(&[seg(t, path)], 10, 4);
        assert!(lines.len() >= 2);
        for l in &lines {
            assert!(seg_width(l) <= 10, "{:?}", line_text(l));
        }
        // First line no indent; continuations start with 4 spaces.
        assert!(!line_text(&lines[0]).starts_with(' '));
        for l in &lines[1..] {
            assert!(line_text(l).starts_with("    "), "{:?}", line_text(l));
        }
        // Reassembling (stripping the hanging indent) recovers the path.
        let joined: String = lines
            .iter()
            .enumerate()
            .map(|(i, l)| {
                let s = line_text(l);
                if i == 0 { s } else { s[4..].to_string() }
            })
            .collect();
        assert_eq!(joined, path);
    }

    #[test]
    fn wrap_never_splits_a_wide_glyph() {
        let t = Tok::Slot(S::Text);
        // 你好世界 — each glyph is 2 cols; wrap at an odd width to force a straddle.
        let lines = wrap(&[seg(t, "\u{4f60}\u{597d}\u{4e16}\u{754c}")], 3, 0);
        for l in &lines {
            assert!(seg_width(l) <= 3, "{:?}", line_text(l));
            // No line ends mid-glyph: every glyph is whole (2 cols) so the text
            // reconstructs exactly.
        }
        let joined: String = lines.iter().map(|l| line_text(l)).collect();
        assert_eq!(joined, "\u{4f60}\u{597d}\u{4e16}\u{754c}");
    }

    #[test]
    fn wrap_handles_empty_and_degenerate_inputs() {
        let t = Tok::Slot(S::Text);
        assert_eq!(wrap(&[], 10, 4), vec![Vec::<Seg>::new()]);
        assert_eq!(wrap(&[seg(t, "x")], 0, 0), vec![Vec::<Seg>::new()]);
        // cont_indent >= width is clamped, no panic, body stays >= 1 cell.
        let lines = wrap(&[seg(t, "abcdef")], 3, 9);
        for l in &lines {
            assert!(seg_width(l) <= 3);
        }
    }

    #[test]
    fn wrap_preserves_style_across_splits() {
        let lines = wrap(&[seg(Tok::Slot(S::Accent), "alpha bravo").bold()], 6, 0);
        assert!(lines.len() >= 2);
        for l in &lines {
            for s in l {
                if !s.text.trim().is_empty() {
                    assert_eq!(s.fg, Tok::Slot(S::Accent));
                    assert!(s.bold);
                }
            }
        }
    }

    #[test]
    fn wide_glyph_line_paints_exact_width() {
        // A 6-cell line holding "你好" (each glyph is 2 cols) + pad must place the
        // two wide glyphs at cols 0 and 2 (col 1/3 are continuation cells) and
        // pad cols 4–5 — i.e. exactly 6 cells, no overflow.
        let mut s = Surface::new(6, 1);
        let line = Line::Segs(vec![seg(Tok::Slot(S::Text), "\u{4f60}\u{597d}")]);
        draw_line(&mut s, 0, 0, 6, &line, PAD);
        let cells = s.screen_cells();
        assert_eq!(cells[0][0].str(), "\u{4f60}", "first wide glyph at col 0");
        assert_eq!(cells[0][2].str(), "\u{597d}", "second wide glyph at col 2");
        assert_eq!(cells[0][4].str(), " ", "pad after glyphs");
        assert_eq!(cells[0][5].str(), " ", "pad fills to width");
    }

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
    fn contrast_scanner_flags_dark_on_dark_text_only() {
        // Readable text on a surface: clean.
        let mut ok = Surface::new(6, 1);
        draw_line(
            &mut ok,
            0,
            0,
            6,
            &Line::segs(vec![seg(Tok::Slot(S::Text), "hi")]),
            Tok::Slot(S::Panel),
        );
        assert!(text_contrast_violations(&mut ok, 3.0).is_empty());

        // Near-black chip text on the dark `raise` surface — the modal bug.
        let mut bad = Surface::new(6, 1);
        draw_line(
            &mut bad,
            0,
            0,
            6,
            &Line::segs(vec![Seg::chip(Tok::Slot(S::Raise), "no")]),
            Tok::Slot(S::Panel),
        );
        let v = text_contrast_violations(&mut bad, 3.0);
        assert_eq!(v.len(), 2, "both glyph cells should be flagged: {v:?}");

        // A box-drawing rule at the same low contrast is decorative — exempt.
        let mut rule = Surface::new(6, 1);
        draw_line(
            &mut rule,
            0,
            0,
            6,
            &Line::Fill {
                ch: '─',
                fg: Tok::Slot(S::Raise),
            },
            Tok::Slot(S::Panel),
        );
        assert!(text_contrast_violations(&mut rule, 3.0).is_empty());
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
