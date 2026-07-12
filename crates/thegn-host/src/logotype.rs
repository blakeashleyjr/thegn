//! The thegn logotype: a hand-rolled micro pixel font rendered with
//! half-block cells (`▀` `▄` `█`), plus the empty-center splash that uses it.
//!
//! Two faces, both with even pixel heights so two pixel rows always map onto
//! one terminal cell with no ragged half-row:
//!   - [`Face::Small`] — 3×4 px glyphs → 2 terminal rows (the masthead brand).
//!   - [`Face::Large`] — 5×6 px glyphs → 3 terminal rows (the splash).
//!
//! Cells are written with explicit fg AND bg (termwiz has no transparency: a
//! `▀` shows the cell's bg in its lower half), so callers pass the surface
//! color they have already filled. Everything draws in the normal dirty-frame
//! scratch-surface pass — no timers, no animation.

use termwiz::color::ColorAttribute;
use termwiz::surface::Surface;
use unicode_width::UnicodeWidthStr;

use crate::chrome::{self, S, col};
use crate::compositor::Rect;

/// One letterform: `width` pixel columns; one bitmask per pixel row,
/// MSB-first (bit `width-1` is the leftmost pixel column).
struct Glyph {
    width: u8,
    rows: &'static [u8],
}

/// Pixel-font size. Pixel heights are even by construction (see module doc).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Face {
    Small,
    Large,
}

impl Face {
    fn px_height(self) -> usize {
        match self {
            Face::Small => 4,
            Face::Large => 6,
        }
    }

    /// Terminal rows the face occupies (two pixel rows per cell).
    pub fn cell_rows(self) -> usize {
        self.px_height() / 2
    }
}

// ---- Small face: 3×4 px. Only the letters the wordmark needs (YAGNI). ----
const SMALL_S: Glyph = Glyph {
    width: 3,
    rows: &[0b111, 0b100, 0b001, 0b111],
};
const SMALL_U: Glyph = Glyph {
    width: 3,
    rows: &[0b101, 0b101, 0b101, 0b111],
};
const SMALL_P: Glyph = Glyph {
    width: 3,
    rows: &[0b111, 0b101, 0b111, 0b100],
};
const SMALL_E: Glyph = Glyph {
    width: 3,
    rows: &[0b111, 0b110, 0b100, 0b111],
};
const SMALL_R: Glyph = Glyph {
    width: 3,
    rows: &[0b111, 0b101, 0b110, 0b101],
};
const SMALL_Z: Glyph = Glyph {
    width: 3,
    rows: &[0b111, 0b001, 0b100, 0b111],
};
const SMALL_J: Glyph = Glyph {
    width: 3,
    rows: &[0b111, 0b010, 0b010, 0b110],
};
const SMALL_T: Glyph = Glyph {
    width: 3,
    rows: &[0b111, 0b010, 0b010, 0b010],
};
const SMALL_H: Glyph = Glyph {
    width: 3,
    rows: &[0b101, 0b111, 0b101, 0b101],
};
const SMALL_G: Glyph = Glyph {
    width: 3,
    rows: &[0b111, 0b100, 0b101, 0b111],
};
const SMALL_N: Glyph = Glyph {
    width: 3,
    rows: &[0b111, 0b101, 0b101, 0b101],
};

// ---- Large face: 5×6 px, 1-px corner cuts for a rounded-techy look. ----
const LARGE_S: Glyph = Glyph {
    width: 5,
    rows: &[0b01111, 0b10000, 0b01110, 0b00001, 0b00001, 0b11110],
};
const LARGE_U: Glyph = Glyph {
    width: 5,
    rows: &[0b10001, 0b10001, 0b10001, 0b10001, 0b10001, 0b01110],
};
const LARGE_P: Glyph = Glyph {
    width: 5,
    rows: &[0b11110, 0b10001, 0b10001, 0b11110, 0b10000, 0b10000],
};
const LARGE_E: Glyph = Glyph {
    width: 5,
    rows: &[0b11111, 0b10000, 0b11110, 0b10000, 0b10000, 0b11111],
};
const LARGE_R: Glyph = Glyph {
    width: 5,
    rows: &[0b11110, 0b10001, 0b10001, 0b11110, 0b10010, 0b10001],
};
const LARGE_Z: Glyph = Glyph {
    width: 5,
    rows: &[0b11111, 0b00010, 0b00100, 0b01000, 0b10000, 0b11111],
};
const LARGE_J: Glyph = Glyph {
    width: 5,
    rows: &[0b00111, 0b00010, 0b00010, 0b00010, 0b10010, 0b01100],
};
const LARGE_T: Glyph = Glyph {
    width: 5,
    rows: &[0b11111, 0b00100, 0b00100, 0b00100, 0b00100, 0b00100],
};
const LARGE_H: Glyph = Glyph {
    width: 5,
    rows: &[0b10001, 0b10001, 0b11111, 0b10001, 0b10001, 0b10001],
};
const LARGE_G: Glyph = Glyph {
    width: 5,
    rows: &[0b01111, 0b10000, 0b10000, 0b10011, 0b10001, 0b01110],
};
const LARGE_N: Glyph = Glyph {
    width: 5,
    rows: &[0b10001, 0b11001, 0b10101, 0b10011, 0b10001, 0b10001],
};

fn glyph(face: Face, c: char) -> Option<&'static Glyph> {
    let small = matches!(face, Face::Small);
    Some(match c.to_ascii_uppercase() {
        'S' => {
            if small {
                &SMALL_S
            } else {
                &LARGE_S
            }
        }
        'U' => {
            if small {
                &SMALL_U
            } else {
                &LARGE_U
            }
        }
        'P' => {
            if small {
                &SMALL_P
            } else {
                &LARGE_P
            }
        }
        'E' => {
            if small {
                &SMALL_E
            } else {
                &LARGE_E
            }
        }
        'R' => {
            if small {
                &SMALL_R
            } else {
                &LARGE_R
            }
        }
        'Z' => {
            if small {
                &SMALL_Z
            } else {
                &LARGE_Z
            }
        }
        'J' => {
            if small {
                &SMALL_J
            } else {
                &LARGE_J
            }
        }
        'T' => {
            if small {
                &SMALL_T
            } else {
                &LARGE_T
            }
        }
        'H' => {
            if small {
                &SMALL_H
            } else {
                &LARGE_H
            }
        }
        'G' => {
            if small {
                &SMALL_G
            } else {
                &LARGE_G
            }
        }
        'N' => {
            if small {
                &SMALL_N
            } else {
                &LARGE_N
            }
        }
        _ => return None,
    })
}

/// The wordmark text. One brand, one place.
pub const WORDMARK: &str = "THEGN";

/// Rows the idle (non-loading) splash body occupies below the wordmark+version
/// header: worktree line, gaps, and the ruled keybind-hint block. Constant so
/// the anchor math is state-free.
const IDLE_BODY_ROWS: usize = 6;

/// (cols, rows) `text` occupies in `face`: glyph widths + 1-px letter spacing.
/// Unknown characters are skipped and contribute nothing (no gap either).
pub fn measure(face: Face, text: &str) -> (usize, usize) {
    let mut cols = 0usize;
    for g in text.chars().filter_map(|c| glyph(face, c)) {
        if cols > 0 {
            cols += 1;
        }
        cols += g.width as usize;
    }
    (cols, if cols == 0 { 0 } else { face.cell_rows() })
}

/// Render `text` at `(x, y)`, clipped to `max_cols` columns and `max_rows`
/// terminal rows. Every cell is written with the same `fg`/`bg` pair; the
/// half-block chosen per cell is the (top, bottom) pixel pair:
/// (1,1)→`█`, (1,0)→`▀`, (0,1)→`▄`, (0,0)→space.
#[allow(clippy::too_many_arguments)]
pub fn draw(
    surface: &mut Surface,
    x: usize,
    y: usize,
    text: &str,
    face: Face,
    fg: ColorAttribute,
    bg: ColorAttribute,
    max_cols: usize,
    max_rows: usize,
) {
    let glyphs: Vec<&Glyph> = text.chars().filter_map(|c| glyph(face, c)).collect();
    if glyphs.is_empty() || max_cols == 0 {
        return;
    }
    for cy in 0..face.cell_rows().min(max_rows) {
        let mut line = String::new();
        for (i, g) in glyphs.iter().enumerate() {
            if i > 0 {
                line.push(' ');
            }
            let (top, bot) = (g.rows[2 * cy], g.rows[2 * cy + 1]);
            for px in (0..g.width as usize).rev() {
                let pair = ((top >> px) & 1, (bot >> px) & 1);
                line.push(match pair {
                    (1, 1) => '█',
                    (1, 0) => '▀',
                    (0, 1) => '▄',
                    _ => ' ',
                });
            }
        }
        chrome::draw_text(surface, x, y + cy, &line, fg, bg, max_cols);
    }
}

/// Which splash fits a center rect. Column thresholds derive from the
/// wordmark's measured width plus 2 cols of margin each side, so a wordmark
/// change re-tunes them automatically; row thresholds cover the text stack
/// beneath.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SplashVariant {
    Large,
    Small,
    Text,
    None,
}

/// Minimum center columns for a pixel-face splash: wordmark width + margin.
fn face_min_cols(face: Face) -> usize {
    measure(face, WORDMARK).0 + 4
}

pub fn splash_variant(cols: usize, rows: usize) -> SplashVariant {
    if cols >= face_min_cols(Face::Large) && rows >= 11 {
        SplashVariant::Large
    } else if cols >= face_min_cols(Face::Small) && rows >= 6 {
        SplashVariant::Small
    } else if cols >= 12 && rows >= 1 {
        SplashVariant::Text
    } else {
        SplashVariant::None
    }
}

/// The empty-center splash: wordmark in the accent color, version line, and —
/// loading — the live step timeline ([`crate::loading::screen`]) or — idle —
/// the worktree identity + ruled keybind hints. Pure function of `rect` + the
/// model + the live palette + the ambient clock (spinner frames); resize
/// re-centers it for free, and the splash-scoped ticker repaints it while
/// loading.
///
/// An ASCII-only terminal keeps the FULL layout — only the half-block pixel
/// wordmark swaps for a letter-spaced text masthead. (It used to collapse the
/// whole splash to the one-line Text variant, hiding all step progress.)
pub fn draw_splash(surface: &mut Surface, rect: Rect, model: &crate::chrome::FrameModel) {
    chrome::fill(surface, rect, col(S::Bg0));
    let accent = chrome::theme_color(model.accent_or_default());
    let bg = col(S::Bg0);
    let version = concat!("v", env!("CARGO_PKG_VERSION"));
    let tagline = " · git worktree IDE";
    let loading = !model.load_steps.is_empty();
    let pixel_ok = crate::caps::unicode_level() != thegn_core::termcaps::UnicodeLevel::Ascii;

    // Center one line made of (text, fg) parts.
    let centered_parts = |surface: &mut Surface, y: usize, parts: &[(&str, ColorAttribute)]| {
        let w: usize = parts.iter().map(|(t, _)| UnicodeWidthStr::width(*t)).sum();
        let mut x = rect.x + rect.cols.saturating_sub(w) / 2;
        for (t, fg) in parts {
            chrome::draw_text(surface, x, y, t, *fg, bg, rect.x + rect.cols - x);
            x += UnicodeWidthStr::width(*t);
        }
    };

    match splash_variant(rect.cols, rect.rows) {
        SplashVariant::Large => {
            // Header: wordmark(3) + gap(1) + version(1) + gap(1) = 6 rows,
            // then the body. Loading reserves `screen::reserved_rows` (a pure
            // function of the PLAN, not of tick-by-tick state, so the anchor
            // never bounces); idle reserves the constant hint block.
            let content_rows = if loading {
                crate::loading::screen::reserved_rows(&model.load_steps, &model.load_context)
            } else {
                IDLE_BODY_ROWS
            };
            let base_rows = 3 + 3 + content_rows;
            // Mascot above the wordmark when the center is tall enough for the
            // whole block plus a row of margin top and bottom; small centers
            // keep the compact splash unchanged. Uniform math — no per-state
            // special cases, so the anchor stays stable while steps tick.
            // `[theme] mascot` picks the sprite (owl / knight / off); its
            // cell footprint feeds the same centering math either way.
            let mascot_kind = crate::owl::active_kind();
            let (mcols, mrows) = match mascot_kind {
                thegn_core::config::MascotKind::Owl => (crate::owl::COLS, crate::owl::ROWS),
                thegn_core::config::MascotKind::Knight => {
                    (crate::mascot::COLS, crate::mascot::ROWS)
                }
                thegn_core::config::MascotKind::Off => (0, 0),
            };
            let mascot_block = mrows + 1; // sprite + 1-row gap
            let with_mascot =
                mrows > 0 && rect.cols >= mcols && rect.rows >= base_rows + mascot_block + 2;
            let total_rows = base_rows + if with_mascot { mascot_block } else { 0 };
            let top = rect.y + rect.rows.saturating_sub(total_rows) / 2;
            if with_mascot {
                let mx = rect.x + rect.cols.saturating_sub(mcols) / 2;
                let maxc = (rect.x + rect.cols).saturating_sub(mx);
                match mascot_kind {
                    thegn_core::config::MascotKind::Owl => {
                        crate::owl::draw(surface, mx, top, maxc, crate::owl::blink_now());
                    }
                    _ => crate::mascot::draw(surface, mx, top, maxc),
                }
            }
            let y0 = top + if with_mascot { mascot_block } else { 0 };
            if pixel_ok {
                let (w, _) = measure(Face::Large, WORDMARK);
                let x = rect.x + rect.cols.saturating_sub(w) / 2;
                draw(
                    surface,
                    x,
                    y0,
                    WORDMARK,
                    Face::Large,
                    accent,
                    bg,
                    rect.cols,
                    3,
                );
            } else {
                // Letter-spaced text masthead in the middle wordmark row.
                centered_parts(surface, y0 + 1, &[("T H E G N", accent)]);
            }
            centered_parts(
                surface,
                y0 + 4,
                &[(version, col(S::Dim)), (tagline, col(S::Faint))],
            );
            if loading {
                crate::loading::screen::draw_body(
                    surface,
                    rect,
                    y0 + 6,
                    &model.load_steps,
                    &model.load_context,
                    accent,
                    bg,
                );
            } else {
                draw_idle_body(surface, rect, y0 + 6, model, bg, &centered_parts);
            }
        }
        SplashVariant::Small => {
            let y0 = rect.y + rect.rows.saturating_sub(6) / 2;
            if pixel_ok {
                let (w, _) = measure(Face::Small, WORDMARK);
                let x = rect.x + rect.cols.saturating_sub(w) / 2;
                draw(
                    surface,
                    x,
                    y0,
                    WORDMARK,
                    Face::Small,
                    accent,
                    bg,
                    rect.cols,
                    2,
                );
            } else {
                centered_parts(surface, y0, &[("T H E G N", accent)]);
            }
            centered_parts(
                surface,
                y0 + 3,
                &[(version, col(S::Dim)), (tagline, col(S::Faint))],
            );
            if loading {
                // Compact one-line status: `◐ 3/5 pull image · 37% · 38s`.
                let parts = crate::loading::screen::compact_line(&model.load_steps, accent);
                let borrowed: Vec<(&str, ColorAttribute)> =
                    parts.iter().map(|(t, fg)| (t.as_str(), *fg)).collect();
                centered_parts(surface, y0 + 5, &borrowed);
            } else {
                centered_parts(
                    surface,
                    y0 + 5,
                    &[
                        ("Ctrl-Space", col(S::Dim)),
                        (" palette ", col(S::Faint)),
                        ("·", col(S::Ghost)),
                        (" Ctrl-g", col(S::Dim)),
                        (" lock", col(S::Faint)),
                    ],
                );
            }
        }
        SplashVariant::Text => {
            let y = rect.y + rect.rows.saturating_sub(1) / 2;
            if loading {
                let parts = crate::loading::screen::compact_line(&model.load_steps, accent);
                let mut all: Vec<(&str, ColorAttribute)> = vec![("thegn ", accent)];
                all.extend(parts.iter().map(|(t, fg)| (t.as_str(), *fg)));
                centered_parts(surface, y, &all);
            } else {
                centered_parts(surface, y, &[("thegn ", accent), (version, col(S::Dim))]);
            }
        }
        SplashVariant::None => {}
    }
}

/// A line painter: centers `(text, fg)` parts on a row (see `draw_splash`'s
/// local `centered_parts`).
type PartsPainter<'a> = dyn Fn(&mut Surface, usize, &[(&str, ColorAttribute)]) + 'a;

/// The idle splash body below the header: the active worktree's identity and
/// the keybind-hint block bracketed by thin rules. Occupies [`IDLE_BODY_ROWS`].
fn draw_idle_body(
    surface: &mut Surface,
    rect: Rect,
    y0: usize,
    model: &crate::chrome::FrameModel,
    bg: ColorAttribute,
    centered_parts: &PartsPainter<'_>,
) {
    let g = crate::caps::active_glyphs();
    let hints = [
        ("Ctrl-Space", "command palette"),
        ("Alt-↑↓", "prev/next worktree"),
        ("Ctrl-g", "lock keys to pane"),
    ];
    let key_w = 10;
    let block_w = key_w + 2 + 18;
    let hx = rect.x + rect.cols.saturating_sub(block_w) / 2;
    // Row 0: worktree identity (blank when unknown — the height is constant).
    if !model.worktree.is_empty() {
        centered_parts(surface, y0, &[(model.worktree.as_str(), col(S::Dim))]);
    }
    // Row 1: rule · rows 2..4: hints · row 5: rule.
    let rule = g.box_h.repeat(block_w);
    chrome::draw_text(surface, hx, y0 + 1, &rule, col(S::Ghost), bg, block_w);
    for (i, (key, label)) in hints.iter().enumerate() {
        let y = y0 + 2 + i;
        chrome::draw_text(surface, hx, y, key, col(S::Dim), bg, rect.cols);
        let lx = hx + key_w + 2;
        chrome::draw_text(
            surface,
            lx,
            y,
            label,
            col(S::Faint),
            bg,
            (rect.x + rect.cols).saturating_sub(lx),
        );
    }
    chrome::draw_text(surface, hx, y0 + 5, &rule, col(S::Ghost), bg, block_w);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lines(s: &mut Surface) -> Vec<String> {
        let cells = s.screen_cells();
        cells
            .iter()
            .map(|row| row.iter().map(|c| c.str()).collect::<String>())
            .collect()
    }

    #[test]
    fn measure_wordmark_sizes() {
        assert_eq!(measure(Face::Small, WORDMARK), (19, 2));
        assert_eq!(measure(Face::Large, WORDMARK), (29, 3));
        // Case-insensitive; unknown chars contribute nothing.
        assert_eq!(measure(Face::Small, "thegn"), (19, 2));
        assert_eq!(measure(Face::Small, "S!Z"), (7, 2));
        assert_eq!(measure(Face::Small, "!?"), (0, 0));
    }

    #[test]
    fn small_face_renders_sz_half_blocks() {
        let mut s = Surface::new(7, 2);
        draw(
            &mut s,
            0,
            0,
            "SZ",
            Face::Small,
            ColorAttribute::Default,
            ColorAttribute::Default,
            7,
            2,
        );
        assert_eq!(lines(&mut s), vec!["█▀▀ ▀▀█", "▄▄█ █▄▄"]);
    }

    #[test]
    fn large_face_renders_j() {
        let mut s = Surface::new(5, 3);
        draw(
            &mut s,
            0,
            0,
            "J",
            Face::Large,
            ColorAttribute::Default,
            ColorAttribute::Default,
            5,
            3,
        );
        assert_eq!(lines(&mut s), vec!["  ▀█▀", "   █ ", "▀▄▄▀ "]);
    }

    #[test]
    fn draw_clips_to_max_cols_and_rows() {
        let mut s = Surface::new(10, 2);
        draw(
            &mut s,
            0,
            0,
            WORDMARK,
            Face::Small,
            ColorAttribute::Default,
            ColorAttribute::Default,
            10,
            1,
        );
        let l = lines(&mut s);
        assert_eq!(l[0].chars().count(), 10, "clipped to max_cols");
        assert_eq!(l[1].trim(), "", "second row clipped by max_rows");
    }

    #[test]
    fn splash_variant_thresholds() {
        // Derived thresholds: Large = 29-col wordmark + 4 margin = 33;
        // Small = 19 + 4 = 23.
        assert_eq!(splash_variant(33, 11), SplashVariant::Large);
        assert_eq!(splash_variant(32, 11), SplashVariant::Small);
        assert_eq!(splash_variant(33, 10), SplashVariant::Small);
        assert_eq!(splash_variant(23, 6), SplashVariant::Small);
        assert_eq!(splash_variant(22, 6), SplashVariant::Text);
        assert_eq!(splash_variant(23, 5), SplashVariant::Text);
        assert_eq!(splash_variant(12, 1), SplashVariant::Text);
        assert_eq!(splash_variant(11, 1), SplashVariant::None);
        assert_eq!(splash_variant(0, 0), SplashVariant::None);
    }

    #[test]
    fn draw_splash_large_centers_content() {
        // 14 rows: enough for the Large splash (≥11) but not the mascot block
        // (12 + 12 + 2 = 26), so the compact 12-row idle layout renders.
        let mut s = Surface::new(80, 14);
        let rect = Rect {
            x: 0,
            y: 0,
            cols: 80,
            rows: 14,
        };
        let model = crate::chrome::FrameModel::default();
        draw_splash(&mut s, rect, &model);
        let l = lines(&mut s);
        // Block of 12 rows centered: wordmark starts at (14-12)/2 = 1.
        assert!(
            l[1].contains('▀') || l[1].contains('▄'),
            "wordmark row: {:?}",
            l[1]
        );
        assert!(l[5].contains(env!("CARGO_PKG_VERSION")));
        assert!(l[5].contains("git worktree IDE"));
        // Idle body: rule(8), hints(9..11), rule(12).
        assert!(l[8].contains(crate::caps::active_glyphs().box_h), "top rule");
        assert!(l[9].contains("Ctrl-Space"));
        assert!(l[11].contains("Ctrl-g"));
        assert!(l[12].contains(crate::caps::active_glyphs().box_h), "bottom rule");
        // Wordmark horizontally centered: 29 cols in 80 → starts near col 25.
        let start = l[1].find(['▀', '▄', '█']).unwrap();
        assert!((24..=26).contains(&start), "start {start}");
    }

    #[test]
    fn draw_splash_large_shows_worktree_identity_when_known() {
        let mut s = Surface::new(80, 14);
        let rect = Rect {
            x: 0,
            y: 0,
            cols: 80,
            rows: 14,
        };
        let model = crate::chrome::FrameModel {
            worktree: "repo/sz-vivid-eagle".into(),
            ..Default::default()
        };
        draw_splash(&mut s, rect, &model);
        let l = lines(&mut s);
        // Identity on the body's first row (y0+6 = 7); geometry otherwise
        // identical to the anonymous splash (constant IDLE_BODY_ROWS).
        assert!(l[7].contains("repo/sz-vivid-eagle"), "{:?}", l[7]);
        assert!(l[9].contains("Ctrl-Space"));
    }

    #[test]
    fn draw_splash_large_shows_mascot_when_tall() {
        // 28 rows fits base(12) + mascot block(11+1) + margin(2): mascot on
        // top, wordmark shifted below it.
        let mut s = Surface::new(80, 28);
        let rect = Rect {
            x: 0,
            y: 0,
            cols: 80,
            rows: 28,
        };
        let model = crate::chrome::FrameModel::default();
        draw_splash(&mut s, rect, &model);
        let l = lines(&mut s);
        // total 24 rows centered at top=(28-24)/2=2: mascot rows 2..12.
        assert!(
            l[2].contains('▀') || l[2].contains('▄'),
            "mascot crest row: {:?}",
            l[2]
        );
        // Mascot horizontally centered: 28 sprite cols in 80 → cell col 26;
        // this row's leftmost opaque pixel is inset 3, so blocks start at 29.
        let mstart = l[5].find(['▀', '▄', '█']).unwrap();
        assert!((28..=30).contains(&mstart), "mascot start {mstart}");
        // Wordmark lands after the mascot block: rows 14..16.
        assert!(
            l[14].contains('▀') || l[14].contains('▄'),
            "wordmark row: {:?}",
            l[14]
        );
        assert!(l[18].contains(env!("CARGO_PKG_VERSION")));
        assert!(l[22].contains("Ctrl-Space"));
    }

    #[test]
    fn draw_splash_large_loading_renders_the_timeline_body() {
        use crate::chrome::LoadStep;
        let mut s = Surface::new(80, 24);
        let rect = Rect {
            x: 0,
            y: 0,
            cols: 80,
            rows: 24,
        };
        let model = crate::chrome::FrameModel {
            load_steps: vec![
                LoadStep::done("sandbox"),
                LoadStep::active("image debian:stable"),
                LoadStep::pending("container (podman)"),
                LoadStep::pending("shell"),
            ],
            load_context: vec![("env".into(), "local".into())],
            ..Default::default()
        };
        draw_splash(&mut s, rect, &model);
        let all = lines(&mut s).join("\n");
        assert!(all.contains("1/4"), "gauge: {all}");
        assert!(all.contains("image debian:stable"), "steps: {all}");
        assert!(all.contains("env"), "context block");
        assert!(!all.contains("Ctrl-Space"), "no idle hints while loading");
    }

    #[test]
    fn draw_splash_text_fallback() {
        let mut s = Surface::new(20, 4);
        let rect = Rect {
            x: 0,
            y: 0,
            cols: 20,
            rows: 4,
        };
        let model = crate::chrome::FrameModel::default();
        draw_splash(&mut s, rect, &model);
        let all = lines(&mut s).join("\n");
        assert!(all.contains("thegn"));
        assert!(all.contains(env!("CARGO_PKG_VERSION")));
        assert!(!all.contains('▀'), "no pixel wordmark in text fallback");
    }

    #[test]
    fn ascii_terminal_swaps_masthead_but_keeps_the_layout() {
        use thegn_core::termcaps::UnicodeLevel;
        crate::caps::test_override::with_unicode(UnicodeLevel::Ascii, || {
            // A generously large area: the pixel wordmark degrades to the
            // letter-spaced text masthead, but the LAYOUT survives — this
            // locks the fix for the old behavior of collapsing the whole
            // splash (and every loading step) to a single text line.
            let mut s = Surface::new(80, 24);
            let rect = Rect {
                x: 0,
                y: 0,
                cols: 80,
                rows: 24,
            };
            let model = crate::chrome::FrameModel {
                load_steps: vec![
                    crate::chrome::LoadStep::done("sandbox"),
                    crate::chrome::LoadStep::active("image debian:stable"),
                    crate::chrome::LoadStep::pending("shell"),
                ],
                ..Default::default()
            };
            draw_splash(&mut s, rect, &model);
            let all = lines(&mut s).join("\n");
            assert!(all.contains("T H E G N"), "text masthead present");
            assert!(
                !all.contains('▀') && !all.contains('▄') && !all.contains('█'),
                "no half-block pixel font on an ASCII terminal"
            );
            assert!(
                all.contains("image debian:stable"),
                "step progress stays visible on ASCII terminals: {all}"
            );
        });
    }
}
