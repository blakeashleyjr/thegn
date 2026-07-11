//! The loading-splash mascot: a Sutton Hoo-style helmeted warrior bust,
//! hand-authored as an indexed-palette pixel sprite and rendered with the same
//! half-block mosaic trick as [`crate::media_art`] — each cell is a `▀` whose
//! fg is the upper pixel and bg the lower pixel, so one cell carries two
//! vertical pixels. Colors go on the wire as `Tok::Rgb` and quantize to
//! 256/16/mono at the usual seg color layer; ASCII terminals never reach this
//! (the splash falls back to its text variant before drawing pixel art).
//!
//! The sprite is data, not an image asset: rows of palette-index chars,
//! reviewable in a diff and cheap to tweak. `'.'` is transparent — the splash
//! background shows through. Static by construction: no timers, no animation.

use termwiz::surface::Surface;

use crate::chrome::S;
use crate::seg::{Line, Tok, seg};

/// Sprite palette, indexed by the row chars below. Iron blues for the helmet,
/// gilt bronze for the crest / brows / nose / mustache (the mask's "flying
/// dragon" fittings), near-black for the eye and mouth openings.
const PALETTE: &[(char, (u8, u8, u8))] = &[
    ('a', (56, 58, 76)),    // dark iron (outline)
    ('b', (110, 114, 138)), // mid iron (dome, mask)
    ('c', (162, 168, 192)), // light iron (dome highlight)
    ('d', (198, 152, 64)),  // gilt bronze (crest, brows, nose, mustache)
    ('e', (236, 200, 110)), // bright gold (catchlights)
    ('f', (20, 20, 28)),    // near-black (eye holes, mouth slit)
    ('g', (80, 82, 100)),   // shadow iron (dome base, cheek guards, mail)
    ('h', (146, 104, 44)),  // dark bronze (fitting shadow)
];

/// Pixel grid: [`PX_W`] columns × [`PX_H`] rows, `PX_H` even so two pixel rows
/// always fill one terminal cell (same invariant as the logotype faces).
const PX_W: usize = 28;
const PX_H: usize = 20;

#[rustfmt::skip]
const SPRITE: [&str; PX_H] = [
    "............eeee............", // crest cap
    "...........deeeed...........",
    ".......cccccdhhdccccc.......", // dome top, crest cross-section
    ".....acccccccccccccccca.....",
    "....abbbbbbbbbbbbbbbbbba....",
    "...abbbbbbbbbbbbbbbbbbbba...",
    "...abbbbbbbbbbbbbbbbbbbba...",
    "...agggggggggggggggggggga...", // dome base shadow
    "...adddddddddeeddddddddda...", // gilt browband, brows meet at the bridge
    "...abbffffffddddffffffbba...", // eye holes flank the nose
    "...abbffffffdhhdffffffbba...",
    "...abbbbggggdhhdggggbbbba...", // under-eye mask shadow
    "...abbbbgggddhhddgggbbbba...", // nose flare
    "...abbdddddddeedddddddbba...", // mustache
    "....abbbbddffffffddbbbba....", // mouth slit between mustache tips
    ".....abbbbbggggggbbbbba.....", // chin / cheek-guard taper
    ".......agggggggggggga.......", // guard bottoms
    "..........gagagaga..........", // mail neck
    "....agagagagagagagagagag....", // mail shoulders
    "..gagagagagagagagagagagaga..",
];

/// Cell dimensions of the rendered mosaic.
pub const COLS: usize = PX_W;
pub const ROWS: usize = PX_H / 2;

fn color(ch: char) -> Option<(u8, u8, u8)> {
    PALETTE.iter().find(|(c, _)| *c == ch).map(|(_, rgb)| *rgb)
}

/// Fold two pixel rows into one seg line of `▀`/`▄` cells. Transparent halves
/// take the splash background (`S::Bg0`) so the sprite sits on the filled
/// splash without a box around it.
fn mosaic_line(top_row: &str, bot_row: &str) -> Line {
    let bg0 = Tok::Slot(S::Bg0);
    let cells = top_row
        .chars()
        .zip(bot_row.chars())
        .map(|(t, b)| match (color(t), color(b)) {
            (Some(t), Some(b)) => {
                seg(Tok::Rgb(t.0, t.1, t.2), "\u{2580}").bg(Tok::Rgb(b.0, b.1, b.2))
            }
            (Some(t), None) => seg(Tok::Rgb(t.0, t.1, t.2), "\u{2580}").bg(bg0),
            (None, Some(b)) => seg(Tok::Rgb(b.0, b.1, b.2), "\u{2584}").bg(bg0),
            (None, None) => seg(bg0, " ").bg(bg0),
        });
    Line::segs(cells.collect::<Vec<_>>())
}

/// Draw the mascot with its top-left cell at `(x, y)`, clipped to `max_cols`
/// columns (measured from `x`). Row clipping is the caller's job (the splash
/// gates on available rows before reserving space for the sprite).
pub fn draw(surface: &mut Surface, x: usize, y: usize, max_cols: usize) {
    for row in 0..ROWS {
        let line = mosaic_line(SPRITE[row * 2], SPRITE[row * 2 + 1]);
        crate::seg::draw_line(
            surface,
            x,
            y + row,
            COLS.min(max_cols),
            &line,
            Tok::Slot(S::Bg0),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sprite_dimensions_and_palette() {
        assert_eq!(SPRITE.len(), PX_H);
        assert_eq!(PX_H % 2, 0, "even pixel height: two px rows per cell");
        for (i, row) in SPRITE.iter().enumerate() {
            assert_eq!(row.chars().count(), PX_W, "row {i} width");
            for ch in row.chars() {
                assert!(
                    ch == '.' || color(ch).is_some(),
                    "row {i}: unknown palette char {ch:?}"
                );
            }
        }
    }

    #[test]
    fn palette_chars_unique() {
        for (i, (c, _)) in PALETTE.iter().enumerate() {
            assert!(
                PALETTE.iter().skip(i + 1).all(|(o, _)| o != c),
                "duplicate palette char {c:?}"
            );
        }
    }

    #[test]
    fn draw_writes_half_blocks() {
        let mut s = Surface::new(COLS, ROWS);
        draw(&mut s, 0, 0, COLS);
        let text: Vec<String> = s
            .screen_cells()
            .iter()
            .map(|row| row.iter().map(|c| c.str()).collect())
            .collect();
        // Crest row: transparent top halves except the gold cap.
        assert!(
            text[0].contains('▀') || text[0].contains('▄'),
            "{:?}",
            text[0]
        );
        // Mid-face row is fully opaque half-blocks between the outlines.
        assert!(
            text[4].trim_start_matches(' ').starts_with('▀'),
            "{:?}",
            text[4]
        );
        // Corners stay transparent (space over splash bg).
        assert_eq!(text[0].chars().next(), Some(' '));
        assert_eq!(text[ROWS - 1].chars().last(), Some(' '));
    }
}
