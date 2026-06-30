//! The superzej logotype: a hand-rolled micro pixel font rendered with
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
        _ => return None,
    })
}

/// The wordmark text. One brand, one place.
pub const WORDMARK: &str = "SUPERZEJ";

/// Stable content-height reserve for the loading splash (steps + context rows).
/// Centering uses `max(actual, this)` so the wordmark holds a fixed position as
/// steps tick and the context block appears — no vertical "bounce". Sized for a
/// typical provision plan (~10 steps) plus its context block (~6 lines).
const LOADING_RESERVE_ROWS: usize = 16;

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

/// Which splash fits a center rect. Thresholds leave 2 cols of margin around
/// the Large (47-col) and Small (31-col) wordmarks and room for the text
/// stack beneath.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SplashVariant {
    Large,
    Small,
    Text,
    None,
}

pub fn splash_variant(cols: usize, rows: usize) -> SplashVariant {
    if cols >= 51 && rows >= 11 {
        SplashVariant::Large
    } else if cols >= 35 && rows >= 6 {
        SplashVariant::Small
    } else if cols >= 12 && rows >= 1 {
        SplashVariant::Text
    } else {
        SplashVariant::None
    }
}

/// The empty-center splash: wordmark in the accent color, version line, and
/// keybind hints, centered in `rect` on the deep background. Pure function of
/// `rect` + the live palette — resize re-centers it for free.
pub fn draw_splash(surface: &mut Surface, rect: Rect, model: &crate::chrome::FrameModel) {
    use crate::chrome::StepState;
    chrome::fill(surface, rect, col(S::Bg0));
    let accent = chrome::theme_color(model.accent_or_default());
    let bg = col(S::Bg0);
    let version = concat!("v", env!("CARGO_PKG_VERSION"));
    let tagline = " · git worktree IDE";
    let loading = !model.load_steps.is_empty();

    // Center one line made of (text, fg) parts.
    let centered_parts = |surface: &mut Surface, y: usize, parts: &[(&str, ColorAttribute)]| {
        let w: usize = parts.iter().map(|(t, _)| UnicodeWidthStr::width(*t)).sum();
        let mut x = rect.x + rect.cols.saturating_sub(w) / 2;
        for (t, fg) in parts {
            chrome::draw_text(surface, x, y, t, *fg, bg, rect.x + rect.cols - x);
            x += UnicodeWidthStr::width(*t);
        }
    };

    // The Large/Small wordmarks are a half-block (`▀▄█`) pixel font that an
    // ASCII-only terminal can't render; fall back to the plain text wordmark.
    let variant = match splash_variant(rect.cols, rect.rows) {
        v @ (SplashVariant::Large | SplashVariant::Small)
            if crate::caps::unicode_level() == superzej_core::termcaps::UnicodeLevel::Ascii =>
        {
            let _ = v;
            SplashVariant::Text
        }
        v => v,
    };
    match variant {
        SplashVariant::Large => {
            // Loading: wordmark(3) + gap(1) + version(1) + gap(1) + steps(N) +
            //          gap(1) + context(M). Idle: …hints(3) = 9 rows total.
            let ctx_rows = if model.load_context.is_empty() {
                0
            } else {
                1 + model.load_context.len()
            };
            let content_rows = if loading {
                // STABLE reserve: the wordmark must NOT re-center as steps tick
                // through (done→active→done), as a failed step's error sub-line
                // appears, or as the context block lands — a moving anchor reads as
                // the splash "bouncing". Reserve a fixed height that fits a typical
                // provision plan + its context, so `y0` is constant for the whole
                // session; only genuine overflow (rare, very many steps) grows it.
                (steps_rows(&model.load_steps) + ctx_rows).max(LOADING_RESERVE_ROWS)
            } else {
                3
            };
            let total_rows = 9.max(3 + 3 + content_rows); // always at least 9 for stable centering
            let y0 = rect.y + rect.rows.saturating_sub(total_rows) / 2;
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
            centered_parts(
                surface,
                y0 + 4,
                &[(version, col(S::Dim)), (tagline, col(S::Faint))],
            );
            if loading {
                let next = draw_steps(surface, rect, &model.load_steps, y0 + 6, bg, accent);
                // Context block (env / placement / sandbox / connect / workdir) a
                // row below the steps.
                draw_context(surface, rect, &model.load_context, next + 1, bg);
            } else {
                let hints = [
                    ("Ctrl-Space", "command palette"),
                    ("Alt-↑↓", "prev/next worktree"),
                    ("Ctrl-g", "lock keys to pane"),
                ];
                let key_w = 10;
                let block_w = key_w + 2 + 18;
                let hx = rect.x + rect.cols.saturating_sub(block_w) / 2;
                for (i, (key, label)) in hints.iter().enumerate() {
                    let y = y0 + 6 + i;
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
            }
        }
        SplashVariant::Small => {
            let y0 = rect.y + rect.rows.saturating_sub(6) / 2;
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
            centered_parts(
                surface,
                y0 + 3,
                &[(version, col(S::Dim)), (tagline, col(S::Faint))],
            );
            if loading {
                // Compact: show only the active step on y0+5.
                if let Some(step) = model
                    .load_steps
                    .iter()
                    .find(|s| s.state == StepState::Active)
                    .or_else(|| model.load_steps.last())
                {
                    let (glyph, fg) = step_glyph(step, accent);
                    let text = format!("{glyph} {}", step.label);
                    centered_parts(surface, y0 + 5, &[(&text, fg)]);
                }
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
            centered_parts(surface, y, &[("superzej ", accent), (version, col(S::Dim))]);
        }
        SplashVariant::None => {}
    }
}

/// Returns the glyph and color for a step based on its state.
fn step_glyph(
    step: &crate::chrome::LoadStep,
    accent: ColorAttribute,
) -> (&'static str, ColorAttribute) {
    use crate::chrome::StepState;
    let g = crate::caps::active_glyphs();
    match step.state {
        StepState::Done => (g.check, col(S::Dim)),
        // The in-progress step gets a distinct "working" glyph (↻ / ascii @), not
        // another dot/diamond, so it reads as actively loading vs the pending rows.
        StepState::Active => (g.refresh, accent),
        StepState::Pending => (g.diamond_hollow, col(S::Ghost)),
        StepState::Failed => (g.cross, col(S::Ghost)),
    }
}

/// Number of rows [`draw_steps`] will occupy: one per step plus one per step
/// that carries a `detail` sub-line. Used for vertical centering.
fn steps_rows(steps: &[crate::chrome::LoadStep]) -> usize {
    steps.len() + steps.iter().filter(|s| s.detail.is_some()).count()
}

/// Render a step list centered as a block below `y_start`. A step's optional
/// `detail` (a failed step's error / an active step's status) renders as a dim
/// indented sub-line right below it. Returns the next free row.
fn draw_steps(
    surface: &mut Surface,
    rect: Rect,
    steps: &[crate::chrome::LoadStep],
    y_start: usize,
    bg: ColorAttribute,
    accent: ColorAttribute,
) -> usize {
    use crate::chrome::StepState;
    // Find the width of the widest label to left-align the block as a whole.
    let max_label = steps
        .iter()
        .map(|s| UnicodeWidthStr::width(s.label.as_str()))
        .max()
        .unwrap_or(0);
    // glyph(1) + space(1) + label
    let block_w = 2 + max_label;
    let bx = rect.x + rect.cols.saturating_sub(block_w) / 2;
    let bottom = rect.y + rect.rows;

    let mut y = y_start;
    for step in steps {
        if y >= bottom {
            break;
        }
        let (glyph, glyph_fg) = step_glyph(step, accent);
        chrome::draw_text(surface, bx, y, glyph, glyph_fg, bg, 1);
        let label_fg = match step.state {
            StepState::Done => col(S::Dim),
            StepState::Active => col(S::Text),
            StepState::Pending => col(S::Ghost),
            StepState::Failed => col(S::Ghost),
        };
        chrome::draw_text(
            surface,
            bx + 2,
            y,
            &step.label,
            label_fg,
            bg,
            (rect.x + rect.cols).saturating_sub(bx + 2),
        );
        y += 1;
        // Detail sub-line (failed step's error / active step's status), dim, under
        // the label and clamped to the frame width.
        if let Some(detail) = &step.detail {
            if y >= bottom {
                break;
            }
            let fg = if step.state == StepState::Failed {
                chrome::theme_color(superzej_core::theme::RED)
            } else {
                col(S::Faint)
            };
            chrome::draw_text(
                surface,
                bx + 2,
                y,
                detail,
                fg,
                bg,
                (rect.x + rect.cols).saturating_sub(bx + 2),
            );
            y += 1;
        }
    }
    y
}

/// Render the `(key, value)` loading-context facts (env / placement / sandbox /
/// connect / workdir) as dim, right-of-key aligned lines centered below the
/// steps. Returns nothing; clamped to the frame.
fn draw_context(
    surface: &mut Surface,
    rect: Rect,
    ctx: &[(String, String)],
    y_start: usize,
    bg: ColorAttribute,
) {
    if ctx.is_empty() {
        return;
    }
    let key_w = ctx
        .iter()
        .map(|(k, _)| UnicodeWidthStr::width(k.as_str()))
        .max()
        .unwrap_or(0);
    let val_w = ctx
        .iter()
        .map(|(_, v)| UnicodeWidthStr::width(v.as_str()))
        .max()
        .unwrap_or(0);
    let block_w = key_w + 2 + val_w;
    let bx = rect.x + rect.cols.saturating_sub(block_w) / 2;
    let bottom = rect.y + rect.rows;
    for (i, (k, v)) in ctx.iter().enumerate() {
        let y = y_start + i;
        if y >= bottom {
            break;
        }
        chrome::draw_text(surface, bx, y, k, col(S::Ghost), bg, key_w);
        let vx = bx + key_w + 2;
        chrome::draw_text(
            surface,
            vx,
            y,
            v,
            col(S::Dim),
            bg,
            (rect.x + rect.cols).saturating_sub(vx),
        );
    }
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
        assert_eq!(measure(Face::Small, WORDMARK), (31, 2));
        assert_eq!(measure(Face::Large, WORDMARK), (47, 3));
        // Case-insensitive; unknown chars contribute nothing.
        assert_eq!(measure(Face::Small, "superzej"), (31, 2));
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
        assert_eq!(splash_variant(51, 11), SplashVariant::Large);
        assert_eq!(splash_variant(50, 11), SplashVariant::Small);
        assert_eq!(splash_variant(51, 10), SplashVariant::Small);
        assert_eq!(splash_variant(35, 6), SplashVariant::Small);
        assert_eq!(splash_variant(34, 6), SplashVariant::Text);
        assert_eq!(splash_variant(35, 5), SplashVariant::Text);
        assert_eq!(splash_variant(12, 1), SplashVariant::Text);
        assert_eq!(splash_variant(11, 1), SplashVariant::None);
        assert_eq!(splash_variant(0, 0), SplashVariant::None);
    }

    #[test]
    fn draw_splash_large_centers_content() {
        let mut s = Surface::new(80, 24);
        let rect = Rect {
            x: 0,
            y: 0,
            cols: 80,
            rows: 24,
        };
        let model = crate::chrome::FrameModel::default();
        draw_splash(&mut s, rect, &model);
        let l = lines(&mut s);
        // Block of 9 rows centered: wordmark starts at (24-9)/2 = 7.
        assert!(
            l[7].contains('▀') || l[7].contains('▄'),
            "wordmark row: {:?}",
            l[7]
        );
        assert!(l[11].contains(env!("CARGO_PKG_VERSION")));
        assert!(l[11].contains("git worktree IDE"));
        assert!(l[13].contains("Ctrl-Space"));
        assert!(l[15].contains("Ctrl-g"));
        // Wordmark horizontally centered: 47 cols in 80 → starts near col 16.
        let start = l[7].find(['▀', '▄', '█']).unwrap();
        assert!((15..=17).contains(&start), "start {start}");
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
        assert!(all.contains("superzej"));
        assert!(all.contains(env!("CARGO_PKG_VERSION")));
        assert!(!all.contains('▀'), "no pixel wordmark in text fallback");
    }

    #[test]
    fn ascii_terminal_forces_text_splash_even_when_large() {
        use superzej_core::termcaps::UnicodeLevel;
        crate::caps::test_override::with_unicode(UnicodeLevel::Ascii, || {
            // A generously large area would normally draw the half-block wordmark.
            let mut s = Surface::new(80, 24);
            let rect = Rect {
                x: 0,
                y: 0,
                cols: 80,
                rows: 24,
            };
            let model = crate::chrome::FrameModel::default();
            draw_splash(&mut s, rect, &model);
            let all = lines(&mut s).join("\n");
            assert!(all.contains("superzej"), "text wordmark present");
            assert!(
                !all.contains('▀') && !all.contains('▄') && !all.contains('█'),
                "no half-block pixel font on an ASCII terminal"
            );
        });
    }
}
