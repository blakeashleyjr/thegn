//! Compose a pane's emulator grid into a termwiz `Surface`. The caller flushes
//! the surface through a `BufferedTerminal`, which diffs against the previous
//! frame and emits only changed cells — the "no-flash" mechanism. Chrome widgets
//! (Phase 2) draw into the same surface around the pane rect.

use termwiz::cell::{AttributeChange, CellAttributes, Intensity, Underline};
use termwiz::color::{ColorAttribute, SrgbaTuple};
use termwiz::surface::{Change, Position, Surface};

use crate::emulator::{CellColor, PaneEmulator};

/// A rectangle in surface cells (origin + size).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rect {
    pub x: usize,
    pub y: usize,
    pub cols: usize,
    pub rows: usize,
}

fn color_attr(c: CellColor) -> ColorAttribute {
    match c {
        CellColor::Default => ColorAttribute::Default,
        CellColor::Indexed(i) => ColorAttribute::PaletteIndex(i),
        CellColor::Rgb(r, g, b) => ColorAttribute::TrueColorWithDefaultFallback(SrgbaTuple(
            r as f32 / 255.0,
            g as f32 / 255.0,
            b as f32 / 255.0,
            1.0,
        )),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
struct CellStyle {
    fg: CellColor,
    bg: CellColor,
    bold: bool,
    italic: bool,
    underline: bool,
}

fn emit_style(surface: &mut Surface, style: CellStyle) {
    // One `AllAttributes` instead of five `Attribute` changes per style run:
    // fewer change objects in the surface's per-frame change log, and it resets
    // to a known-clean baseline (reverse/strikethrough/blink off) each run. The
    // resulting cells are identical — the compositor only ever sets these five.
    let mut attrs = CellAttributes::default();
    attrs
        .set_foreground(color_attr(style.fg))
        .set_background(color_attr(style.bg))
        .set_intensity(if style.bold {
            Intensity::Bold
        } else {
            Intensity::Normal
        })
        .set_italic(style.italic)
        .set_underline(if style.underline {
            Underline::Single
        } else {
            Underline::None
        });
    surface.add_change(Change::AllAttributes(attrs));
}

fn flush_run(surface: &mut Surface, run: &mut String) {
    if !run.is_empty() {
        surface.add_change(Change::Text(std::mem::take(run)));
    }
}

/// Paint `emu`'s visible grid into `surface` at `rect`. Cells beyond the
/// emulator's size are left untouched (chrome owns them).
///
/// A single cell-by-cell pass per row, coalescing same-style cells into one
/// `Change::Text` run — so an all-default row still emits as a single blit, but
/// without the extra full-row `row_text` pre-scan that used to run (and allocate
/// a `String` per cell) only to be discarded on the first styled cell. Styled
/// content is the common case, so that pre-scan doubled the hot-path cost.
pub fn compose_pane(surface: &mut Surface, emu: &dyn PaneEmulator, rect: Rect) {
    let (erows, ecols) = emu.size();
    let mut current_style: Option<CellStyle> = None;
    let mut run = String::new();
    for row in 0..rect.rows.min(erows as usize) {
        flush_run(surface, &mut run);
        surface.add_change(Change::CursorPosition {
            x: Position::Absolute(rect.x),
            y: Position::Absolute(rect.y + row),
        });
        for col in 0..rect.cols.min(ecols as usize) {
            // Prefer the borrowing accessor (no per-cell `String` alloc); fall
            // back to the owning `cell()` for any emulator that doesn't implement
            // `cell_ref`. Indices are in-bounds, so `cell_ref` is `Some` for the
            // real (vt100) emulator and the fallback never runs in production.
            let owned;
            let (text, fg, bg, bold, italic, underline, inverse): (&str, _, _, _, _, _, _) =
                if let Some(c) = emu.cell_ref(row as u16, col as u16) {
                    (c.text, c.fg, c.bg, c.bold, c.italic, c.underline, c.inverse)
                } else {
                    owned = emu.cell(row as u16, col as u16).unwrap_or_default();
                    (
                        owned.text.as_str(),
                        owned.fg,
                        owned.bg,
                        owned.bold,
                        owned.italic,
                        owned.underline,
                        owned.inverse,
                    )
                };
            let style = CellStyle {
                fg: if inverse { bg } else { fg },
                bg: if inverse { fg } else { bg },
                bold,
                italic,
                underline,
            };
            if current_style != Some(style) {
                flush_run(surface, &mut run);
                emit_style(surface, style);
                current_style = Some(style);
            }
            if text.is_empty() {
                run.push(' ');
            } else {
                run.push_str(text);
            }
        }
    }
    flush_run(surface, &mut run);
}

/// Paint the mouse-selection highlight over a pane's `content` rect: selected
/// cells keep their glyph and foreground, on `bg`. Extract-style spans (first
/// row from the anchor column, middle rows full, last row to the cursor) so
/// the highlight matches exactly what auto-copy yields. Call after
/// [`compose_pane`]; never paints outside `content`.
pub fn overlay_selection(
    surface: &mut Surface,
    content: Rect,
    sel: &crate::copymode::Selection,
    bg: termwiz::color::ColorAttribute,
) {
    let (sr, sc, er, ec) = sel.ordered();
    let last_col = content.cols.saturating_sub(1);
    // Read the composed cells back first (screen_cells borrows mutably).
    let mut patches: Vec<(usize, usize, String, termwiz::color::ColorAttribute)> = Vec::new();
    {
        let cells = surface.screen_cells();
        for r in sr..=er.min(content.rows.saturating_sub(1) as u16) {
            let (from, to) = if sr == er {
                (sc, ec)
            } else if r == sr {
                (sc, last_col as u16)
            } else if r == er {
                (0, ec)
            } else {
                (0, last_col as u16)
            };
            let y = content.y + r as usize;
            for c in from..=to.min(last_col as u16) {
                let x = content.x + c as usize;
                if let Some(cell) = cells.get(y).and_then(|row| row.get(x)) {
                    patches.push((x, y, cell.str().to_string(), cell.attrs().foreground()));
                }
            }
        }
    }
    for (x, y, text, fg) in patches {
        surface.add_change(Change::CursorPosition {
            x: Position::Absolute(x),
            y: Position::Absolute(y),
        });
        surface.add_change(Change::Attribute(AttributeChange::Foreground(fg)));
        surface.add_change(Change::Attribute(AttributeChange::Background(bg)));
        surface.add_change(Change::Text(if text.is_empty() {
            " ".into()
        } else {
            text
        }));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::emulator::AlacrittyEmulator;

    #[test]
    fn composing_a_grid_reproduces_its_text() {
        let mut emu = AlacrittyEmulator::new(3, 20, 0);
        emu.advance(b"alpha\r\nbravo\r\ncharlie");

        let mut surface = Surface::new(20, 3);
        compose_pane(
            &mut surface,
            &emu,
            Rect {
                x: 0,
                y: 0,
                cols: 20,
                rows: 3,
            },
        );

        let text = surface.screen_chars_to_string();
        assert!(text.contains("alpha"), "got: {text:?}");
        assert!(text.contains("bravo"), "got: {text:?}");
        assert!(text.contains("charlie"), "got: {text:?}");
    }

    #[test]
    #[ignore]
    fn cell_ref_matches_cell() {
        // The borrowing accessor must agree with the owning one on glyph + style
        // for plain, styled, and wide-glyph cells (compose_pane relies on this).
        let mut emu = AlacrittyEmulator::new(1, 6, 0);
        emu.advance("a\x1b[1;31mB\x1b[0m世".as_bytes());
        for col in 0..6u16 {
            let owned = emu.cell(0, col);
            let borrowed = emu.cell_ref(0, col);
            match (owned, borrowed) {
                (Some(o), Some(b)) => {
                    assert_eq!(o.text, b.text, "glyph mismatch at col {col}");
                    assert_eq!(o.fg, b.fg, "fg mismatch at col {col}");
                    assert_eq!(o.bg, b.bg, "bg mismatch at col {col}");
                    assert_eq!(o.bold, b.bold, "bold mismatch at col {col}");
                    assert_eq!(o.italic, b.italic, "italic mismatch at col {col}");
                    assert_eq!(o.underline, b.underline, "underline at col {col}");
                    assert_eq!(o.inverse, b.inverse, "inverse mismatch at col {col}");
                }
                (None, None) => {}
                (o, b) => panic!("cell/cell_ref presence differ at col {col}: {o:?} vs {b:?}"),
            }
        }
    }

    #[test]
    fn composing_preserves_cell_styling() {
        // The single pass must carry color/attrs through. (The old fast path
        // blitted unstyled rows as plain text and bailed to cell-by-cell only
        // for styled rows; now every row is composed cell-by-cell, so guard
        // that styling still survives.)
        let mut emu = AlacrittyEmulator::new(1, 4, 0);
        emu.advance(b"\x1b[31mRED\x1b[0m");
        let mut surface = Surface::new(4, 1);
        compose_pane(
            &mut surface,
            &emu,
            Rect {
                x: 0,
                y: 0,
                cols: 4,
                rows: 1,
            },
        );
        let cells = surface.screen_cells();
        assert_eq!(cells[0][0].str(), "R");
        assert_eq!(
            cells[0][0].attrs().foreground(),
            ColorAttribute::PaletteIndex(1),
            "red SGR must survive compose",
        );
    }

    #[test]
    fn composing_into_a_subrect_leaves_other_cells_blank() {
        let mut emu = AlacrittyEmulator::new(1, 5, 0);
        emu.advance(b"XXXXX");
        let mut surface = Surface::new(20, 3);
        compose_pane(
            &mut surface,
            &emu,
            Rect {
                x: 2,
                y: 1,
                cols: 5,
                rows: 1,
            },
        );
        let lines: Vec<String> = surface
            .screen_chars_to_string()
            .lines()
            .map(|s| s.to_string())
            .collect();
        // Row 0 untouched (blank), row 1 has the X's starting at column 2.
        assert_eq!(lines[1].trim_end(), "  XXXXX");
        assert_eq!(lines[0].trim_end(), "");
    }
}
