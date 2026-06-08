//! Compose a pane's emulator grid into a termwiz `Surface`. The caller flushes
//! the surface through a `BufferedTerminal`, which diffs against the previous
//! frame and emits only changed cells — the "no-flash" mechanism. Chrome widgets
//! (Phase 2) draw into the same surface around the pane rect.

use termwiz::cell::{AttributeChange, Intensity, Underline};
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct CellStyle {
    fg: CellColor,
    bg: CellColor,
    bold: bool,
    italic: bool,
    underline: bool,
}

fn emit_style(surface: &mut Surface, style: CellStyle) {
    surface.add_change(Change::Attribute(AttributeChange::Foreground(color_attr(
        style.fg,
    ))));
    surface.add_change(Change::Attribute(AttributeChange::Background(color_attr(
        style.bg,
    ))));
    surface.add_change(Change::Attribute(AttributeChange::Intensity(
        if style.bold {
            Intensity::Bold
        } else {
            Intensity::Normal
        },
    )));
    surface.add_change(Change::Attribute(AttributeChange::Italic(style.italic)));
    surface.add_change(Change::Attribute(AttributeChange::Underline(
        if style.underline {
            Underline::Single
        } else {
            Underline::None
        },
    )));
}

fn flush_run(surface: &mut Surface, run: &mut String) {
    if !run.is_empty() {
        surface.add_change(Change::Text(std::mem::take(run)));
    }
}

/// Paint `emu`'s visible grid into `surface` at `rect`. Cells beyond the
/// emulator's size are left untouched (chrome owns them).
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
            let cell = emu.cell(row as u16, col as u16).unwrap_or_default();
            let style = CellStyle {
                fg: if cell.inverse { cell.bg } else { cell.fg },
                bg: if cell.inverse { cell.fg } else { cell.bg },
                bold: cell.bold,
                italic: cell.italic,
                underline: cell.underline,
            };
            if current_style != Some(style) {
                flush_run(surface, &mut run);
                emit_style(surface, style);
                current_style = Some(style);
            }
            if cell.text.is_empty() {
                run.push(' ');
            } else {
                run.push_str(&cell.text);
            }
        }
    }
    flush_run(surface, &mut run);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::emulator::Vt100Emulator;

    #[test]
    fn composing_a_grid_reproduces_its_text() {
        let mut emu = Vt100Emulator::new(3, 20, 0);
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
    fn composing_into_a_subrect_leaves_other_cells_blank() {
        let mut emu = Vt100Emulator::new(1, 5, 0);
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
