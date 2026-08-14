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

impl Rect {
    /// Whether cell `(x, y)` falls inside this rect.
    pub fn contains(&self, x: usize, y: usize) -> bool {
        x >= self.x && x < self.x + self.cols && y >= self.y && y < self.y + self.rows
    }

    /// The whole screen as a rect (origin 0,0).
    pub fn full(cols: usize, rows: usize) -> Rect {
        Rect {
            x: 0,
            y: 0,
            cols,
            rows,
        }
    }
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
    use unicode_width::UnicodeWidthStr;
    let (erows, ecols) = emu.size();
    let last_col = rect.cols.min(ecols as usize).saturating_sub(1);
    // One grid lock for the whole compose: with the feed on the pane's reader
    // thread, per-cell accessors would contend the grid lock ~10k times per
    // pane against a flooding parser. Falls back to per-cell reads for
    // emulators without a snapshot.
    let snapshot = emu.grid_snapshot();
    let mut current_style: Option<CellStyle> = None;
    let mut run = String::new();
    for row in 0..rect.rows.min(erows as usize) {
        flush_run(surface, &mut run);
        surface.add_change(Change::CursorPosition {
            x: Position::Absolute(rect.x),
            y: Position::Absolute(rect.y + row),
        });
        // A double-width glyph occupies TWO grid columns: the glyph cell plus a
        // spacer cell the emulator fills with a blank. The glyph already advances
        // the terminal cursor across both columns, so the spacer must emit nothing
        // — otherwise every wide glyph pushes the rest of the row one cell right
        // and overruns the pane rect. Skip the column immediately after a wide
        // glyph (emulator-agnostic: keyed on the glyph's display width, not the
        // emulator's spacer flag, which `conv_cell` doesn't carry through).
        let mut skip_spacer = false;
        for col in 0..rect.cols.min(ecols as usize) {
            if skip_spacer {
                skip_spacer = false;
                continue;
            }
            // Snapshot first (one lock), then the borrowing accessor, then the
            // owning `cell()` — the fallbacks never run for the real emulator.
            let owned;
            let (text, fg, bg, bold, italic, underline, inverse): (&str, _, _, _, _, _, _) =
                if let Some(c) = snapshot
                    .as_ref()
                    .and_then(|s| s.get(row))
                    .and_then(|r| r.get(col))
                {
                    (
                        c.text.as_str(),
                        c.fg,
                        c.bg,
                        c.bold,
                        c.italic,
                        c.underline,
                        c.inverse,
                    )
                } else if let Some(c) = emu.cell_ref(row as u16, col as u16) {
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
            } else if col == last_col && text.width() > 1 {
                // A double-width glyph at the final column would make the
                // terminal advance into the cell *right* of the rect — the pane
                // card's border column — leaving a gap in the `│` on the next
                // diff. It can't be shown in one cell anyway, so blank it; the
                // emulator already wraps wide glyphs off its own last column, so
                // this only fires in the brief window where the emulator is still
                // wider than a just-shrunk rect (e.g. opening the right panel).
                run.push(' ');
            } else {
                if text.width() > 1 {
                    skip_spacer = true;
                }
                run.push_str(text);
            }
        }
    }
    flush_run(surface, &mut run);
}

/// Emit a change list that repaints EVERY cell of `surface` (blanks written as
/// spaces), against no baseline. Used by the flash-free periodic resync heal.
///
/// `Surface::diff_screens` against a fresh (blank) baseline only emits cells that
/// DIFFER from the blank default — a scratch cell that is a default space equals
/// that baseline and is skipped. So a diff-against-blank resync heals drifted
/// non-blank cells but can never clear an orphaned physical ghost wherever the
/// current frame is blank (the "doubled/ghosted rows" that stick until the app
/// overwrites them). Writing every cell explicitly overwrites those ghosts too,
/// and — because it emits no `ClearScreen` — stays flash-free (each cell is
/// overwritten in place with its correct content, the screen is never blanked).
///
/// Applying the returned changes to a blank `Surface` reproduces `surface`
/// cell-for-cell, so the caller can keep its `front` baseline in sync by replaying
/// them. Mirrors `compose_pane`'s style-run coalescing + wide-glyph spacer skip.
pub fn full_repaint_changes(surface: &mut Surface) -> Vec<Change> {
    let (cols, rows) = surface.dimensions();
    let cells = surface.screen_cells();
    let mut out: Vec<Change> = Vec::new();
    let mut run = String::new();
    for (y, line) in cells.iter().enumerate().take(rows) {
        flush_run_to(&mut out, &mut run);
        out.push(Change::CursorPosition {
            x: Position::Absolute(0),
            y: Position::Absolute(y),
        });
        // Reset the attribute baseline at each row start: the first cell always
        // emits its attributes, so a blank front reconstructs the row exactly.
        let mut current: Option<CellAttributes> = None;
        let mut skip_spacer = false;
        let last_col = cols.saturating_sub(1);
        for (x, cell) in line.iter().enumerate().take(cols) {
            if skip_spacer {
                // The blank placeholder termwiz keeps right of a wide glyph; the
                // glyph already advanced the terminal cursor across it.
                skip_spacer = false;
                continue;
            }
            let attrs = cell.attrs();
            if current.as_ref() != Some(attrs) {
                flush_run_to(&mut out, &mut run);
                out.push(Change::AllAttributes(attrs.clone()));
                current = Some(attrs.clone());
            }
            let text = cell.str();
            let width = unicode_width::UnicodeWidthStr::width(text);
            if text.is_empty() || (x == last_col && width > 1) {
                // Empty cell → a space; a wide glyph with no room at the last
                // column can't be drawn there (would advance past the surface).
                run.push(' ');
            } else {
                if width > 1 {
                    skip_spacer = true;
                }
                run.push_str(text);
            }
        }
    }
    flush_run_to(&mut out, &mut run);
    out
}

/// Push a coalesced text run onto a raw change list (the `flush_run` shape, but
/// appending to a `Vec<Change>` rather than a `Surface`).
fn flush_run_to(out: &mut Vec<Change>, run: &mut String) {
    if !run.is_empty() {
        out.push(Change::Text(std::mem::take(run)));
    }
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
    display_offset: usize,
    bg: termwiz::color::ColorAttribute,
) {
    let (sr, sc, er, ec) = sel.ordered();
    let last_col = content.cols.saturating_sub(1);
    // Selection rows are absolute grid lines; the highlight is screen-relative,
    // so map back through the current viewport offset and skip lines scrolled
    // out of view (a partial highlight when the selection spans off-screen).
    let last_row = content.rows.saturating_sub(1) as i32;
    // Read the composed cells back first (screen_cells borrows mutably).
    let mut patches: Vec<(usize, usize, String, termwiz::color::ColorAttribute)> = Vec::new();
    {
        let cells = surface.screen_cells();
        for r in sr..=er {
            let (from, to) = if sr == er {
                (sc, ec)
            } else if r == sr {
                (sc, last_col as u16)
            } else if r == er {
                (0, ec)
            } else {
                (0, last_col as u16)
            };
            let screen_row = r + display_offset as i32;
            if screen_row < 0 || screen_row > last_row {
                continue;
            }
            let y = content.y + screen_row as usize;
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

/// Overlay predicted (not-yet-confirmed) keystrokes at a pane's cursor — dim +
/// underlined, mosh-style — so local echo appears instantly on a high-latency
/// link. Written onto `scratch` just before the diff so the front buffer stays in
/// sync and the prediction clears on the next frame when the server's
/// authoritative output lands. Clipped to `rect`.
pub fn overlay_predicted(
    surface: &mut Surface,
    rect: Rect,
    cur_row: u16,
    cur_col: u16,
    predicted: &[char],
) {
    if predicted.is_empty() || cur_row as usize >= rect.rows {
        return;
    }
    let y = rect.y + cur_row as usize;
    let x = rect.x + cur_col as usize;
    surface.add_change(Change::CursorPosition {
        x: Position::Absolute(x),
        y: Position::Absolute(y),
    });
    surface.add_change(Change::Attribute(AttributeChange::Foreground(
        ColorAttribute::PaletteIndex(8), // dim gray — reads as provisional
    )));
    surface.add_change(Change::Attribute(AttributeChange::Underline(
        Underline::Single,
    )));
    // Clip to the pane's right edge — only as many predictions as fit.
    let room = (rect.x + rect.cols).saturating_sub(x);
    for &c in predicted.iter().take(room) {
        surface.add_change(Change::Text(c.to_string()));
    }
    surface.add_change(Change::Attribute(AttributeChange::Underline(
        Underline::None,
    )));
    surface.add_change(Change::Attribute(AttributeChange::Foreground(
        ColorAttribute::Default,
    )));
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
    fn wide_glyph_does_not_shift_following_content_right() {
        // Regression: the spacer cell after a double-width glyph used to be
        // emitted as a blank, pushing everything after it one column right and
        // overrunning the pane. "世" (width 2) then "ok" must land at cols 0-1
        // (the glyph) and 2-3 (o, k), not 0, 2, 3, 4.
        let mut emu = AlacrittyEmulator::new(1, 6, 0);
        emu.advance("世ok".as_bytes());
        let mut surface = Surface::new(6, 1);
        compose_pane(
            &mut surface,
            &emu,
            Rect {
                x: 0,
                y: 0,
                cols: 6,
                rows: 1,
            },
        );
        let cells = surface.screen_cells();
        assert_eq!(cells[0][0].str(), "世", "wide glyph at col 0");
        // col 1 is the spacer the wide glyph occupies; termwiz leaves it empty.
        assert_eq!(cells[0][2].str(), "o", "content must not be shifted right");
        assert_eq!(cells[0][3].str(), "k");
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
    fn full_repaint_emits_every_cell_including_blanks() {
        // A resync heal must re-emit blank cells (as spaces) too — that's the
        // property `diff_screens` against a blank baseline lacks, and the reason
        // orphaned ghosts in now-blank regions stuck on screen. Compose a styled
        // row plus a wide glyph into a surface with trailing blank columns/rows,
        // replay `full_repaint_changes` onto a fresh surface, and require an exact
        // cell-for-cell reproduction (glyph + attributes), including the blanks.
        let mut emu = AlacrittyEmulator::new(2, 8, 0);
        emu.advance("\x1b[31mRED\x1b[0m 世x".as_bytes()); // red run, space, wide glyph, x
        let (cols, rows) = (10, 3); // wider + taller than the emulator → trailing blanks
        let mut src = Surface::new(cols, rows);
        compose_pane(
            &mut src,
            &emu,
            Rect {
                x: 0,
                y: 0,
                cols,
                rows,
            },
        );

        let changes = full_repaint_changes(&mut src);
        let mut rebuilt = Surface::new(cols, rows);
        rebuilt.add_changes(changes);

        let a = src.screen_cells();
        let b = rebuilt.screen_cells();
        for y in 0..rows {
            for x in 0..cols {
                assert_eq!(a[y][x].str(), b[y][x].str(), "glyph mismatch at ({x},{y})");
                assert_eq!(
                    a[y][x].attrs().foreground(),
                    b[y][x].attrs().foreground(),
                    "fg mismatch at ({x},{y})"
                );
                assert_eq!(
                    a[y][x].attrs().background(),
                    b[y][x].attrs().background(),
                    "bg mismatch at ({x},{y})"
                );
            }
        }
        // The red run survived, and a trailing blank cell is a space (not skipped).
        assert_eq!(b[0][0].str(), "R");
        assert_eq!(
            b[0][0].attrs().foreground(),
            ColorAttribute::PaletteIndex(1),
        );
        assert_eq!(
            b[2][9].str(),
            " ",
            "trailing blank must be re-emitted as space"
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

    #[test]
    fn wide_glyph_at_last_col_does_not_nibble_the_border() {
        // The emulator is wider than the rect we compose into — the brief window
        // after the right panel opens and shrinks the center before the PTY has
        // resized. A double-width glyph sitting at the rect's last content column
        // must NOT advance the terminal into the cell to its right (the pane
        // card's border column), which is the gap reported on the right edge.
        let mut emu = AlacrittyEmulator::new(1, 6, 0);
        emu.advance("aaaa\u{6f22}".as_bytes()); // 'a'×4 then a wide glyph at cols 4-5
        let mut surface = Surface::new(8, 1);
        // Sentinel border in the column just right of the 5-wide content rect.
        surface.add_change(Change::CursorPosition {
            x: Position::Absolute(5),
            y: Position::Absolute(0),
        });
        surface.add_change(Change::Text("\u{2502}".into()));
        compose_pane(
            &mut surface,
            &emu,
            Rect {
                x: 0,
                y: 0,
                cols: 5,
                rows: 1,
            },
        );
        let cells = surface.screen_cells();
        assert_eq!(
            cells[0][5].str(),
            "\u{2502}",
            "content must not nibble the border column"
        );
        // A wide glyph that can't fit the last cell is blanked, not half-drawn.
        assert_eq!(cells[0][4].str(), " ", "edge wide glyph blanked");
    }
}
