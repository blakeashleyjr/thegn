//! The render bridge: blit a ratatui [`Buffer`] (what an embedded [`AppTile`]
//! produces) into the host's termwiz [`Surface`].
//!
//! Modeled on [`crate::compositor::compose_pane`] — same run-batching
//! discipline (position the cursor once per row, accumulate same-style glyphs
//! into a single `Change::Text`, flush on style change) so a tile costs about
//! the same to paint as a PTY pane. `BufferedTerminal` then diffs the surface
//! to the wire, so only changed cells hit the terminal.
//!
//! [`AppTile`]: sz_kit::AppTile

use sz_kit::ratatui::buffer::Buffer;
use sz_kit::ratatui::style::{Color, Modifier};
use termwiz::cell::{AttributeChange, Blink, Intensity, Underline};
use termwiz::color::{ColorAttribute, SrgbaTuple};
use termwiz::surface::{Change, Position, Surface};

use crate::compositor::Rect;

/// Map a ratatui [`Color`] to a termwiz [`ColorAttribute`].
fn color_attr(c: Color) -> ColorAttribute {
    match c {
        Color::Reset => ColorAttribute::Default,
        Color::Rgb(r, g, b) => ColorAttribute::TrueColorWithDefaultFallback(SrgbaTuple(
            r as f32 / 255.0,
            g as f32 / 255.0,
            b as f32 / 255.0,
            1.0,
        )),
        Color::Indexed(i) => ColorAttribute::PaletteIndex(i),
        // The 16 named ANSI colors map to their palette slots so the terminal's
        // own theme recolors them (matches how chrome treats indexed colors).
        Color::Black => ColorAttribute::PaletteIndex(0),
        Color::Red => ColorAttribute::PaletteIndex(1),
        Color::Green => ColorAttribute::PaletteIndex(2),
        Color::Yellow => ColorAttribute::PaletteIndex(3),
        Color::Blue => ColorAttribute::PaletteIndex(4),
        Color::Magenta => ColorAttribute::PaletteIndex(5),
        Color::Cyan => ColorAttribute::PaletteIndex(6),
        Color::Gray => ColorAttribute::PaletteIndex(7),
        Color::DarkGray => ColorAttribute::PaletteIndex(8),
        Color::LightRed => ColorAttribute::PaletteIndex(9),
        Color::LightGreen => ColorAttribute::PaletteIndex(10),
        Color::LightYellow => ColorAttribute::PaletteIndex(11),
        Color::LightBlue => ColorAttribute::PaletteIndex(12),
        Color::LightMagenta => ColorAttribute::PaletteIndex(13),
        Color::LightCyan => ColorAttribute::PaletteIndex(14),
        Color::White => ColorAttribute::PaletteIndex(15),
    }
}

/// The resolved attributes of one cell, in termwiz terms. `fg`/`bg` already
/// account for the REVERSED modifier (swapped at build time) so the host never
/// has to emit `Reverse`.
#[derive(Debug, Clone, Copy, PartialEq)]
struct CellStyle {
    fg: ColorAttribute,
    bg: ColorAttribute,
    intensity: Intensity,
    underline: Underline,
    italic: bool,
    strikethrough: bool,
    blink: Blink,
    invisible: bool,
}

impl CellStyle {
    fn of(cell: &sz_kit::ratatui::buffer::Cell) -> CellStyle {
        let m = cell.modifier;
        let mut fg = color_attr(cell.fg);
        let mut bg = color_attr(cell.bg);
        if m.contains(Modifier::REVERSED) {
            std::mem::swap(&mut fg, &mut bg);
        }
        let intensity = if m.contains(Modifier::BOLD) {
            Intensity::Bold
        } else if m.contains(Modifier::DIM) {
            Intensity::Half
        } else {
            Intensity::Normal
        };
        let blink = if m.contains(Modifier::RAPID_BLINK) {
            Blink::Rapid
        } else if m.contains(Modifier::SLOW_BLINK) {
            Blink::Slow
        } else {
            Blink::None
        };
        CellStyle {
            fg,
            bg,
            intensity,
            underline: if m.contains(Modifier::UNDERLINED) {
                Underline::Single
            } else {
                Underline::None
            },
            italic: m.contains(Modifier::ITALIC),
            strikethrough: m.contains(Modifier::CROSSED_OUT),
            blink,
            invisible: m.contains(Modifier::HIDDEN),
        }
    }
}

fn emit_style(surface: &mut Surface, s: CellStyle) {
    surface.add_change(Change::Attribute(AttributeChange::Foreground(s.fg)));
    surface.add_change(Change::Attribute(AttributeChange::Background(s.bg)));
    surface.add_change(Change::Attribute(AttributeChange::Intensity(s.intensity)));
    surface.add_change(Change::Attribute(AttributeChange::Underline(s.underline)));
    surface.add_change(Change::Attribute(AttributeChange::Italic(s.italic)));
    surface.add_change(Change::Attribute(AttributeChange::StrikeThrough(
        s.strikethrough,
    )));
    surface.add_change(Change::Attribute(AttributeChange::Blink(s.blink)));
    surface.add_change(Change::Attribute(AttributeChange::Invisible(s.invisible)));
}

fn flush_run(surface: &mut Surface, run: &mut String) {
    if !run.is_empty() {
        surface.add_change(Change::Text(std::mem::take(run)));
    }
}

/// Paint `buf` into `surface` at `rect`. Buffer cell `(col,row)` maps to
/// surface `(rect.x+col, rect.y+row)`, clipped to `rect`. Cells beyond the
/// buffer or the rect are left untouched (chrome owns them).
///
/// The buffer is expected to have been rendered at origin `(0,0)`; any nonzero
/// `buf.area` origin is subtracted so the top-left cell lands at `rect`.
pub fn blit(surface: &mut Surface, buf: &Buffer, rect: Rect) {
    let area = buf.area;
    let ox = area.x;
    let oy = area.y;
    let rows = (area.height as usize).min(rect.rows);
    let cols = (area.width as usize).min(rect.cols);

    let mut current: Option<CellStyle> = None;
    let mut run = String::new();
    for row in 0..rows {
        flush_run(surface, &mut run);
        surface.add_change(Change::CursorPosition {
            x: Position::Absolute(rect.x),
            y: Position::Absolute(rect.y + row),
        });
        // Re-assert the row's first style after the cursor jump.
        let mut row_started = false;
        for col in 0..cols {
            let Some(cell) = buf.cell((ox + col as u16, oy + row as u16)) else {
                continue;
            };
            let sym = cell.symbol();
            // A wide glyph's trailing cell carries an empty symbol; the glyph
            // itself already consumed that column, so skip it (don't pad).
            if sym.is_empty() {
                continue;
            }
            let style = CellStyle::of(cell);
            if current != Some(style) || !row_started {
                flush_run(surface, &mut run);
                emit_style(surface, style);
                current = Some(style);
                row_started = true;
            }
            run.push_str(sym);
        }
    }
    flush_run(surface, &mut run);
}

#[cfg(test)]
mod tests {
    use super::*;
    use sz_kit::ratatui::layout::Rect as RRect;
    use sz_kit::ratatui::style::{Color as RColor, Style};
    use sz_kit::ratatui::text::Line;
    use sz_kit::ratatui::widgets::{Paragraph, Widget};

    fn buf_with(lines: &[&str], w: u16, h: u16) -> Buffer {
        let mut buf = Buffer::empty(RRect::new(0, 0, w, h));
        let text: Vec<Line> = lines.iter().map(|s| Line::from(*s)).collect();
        Paragraph::new(text).render(buf.area, &mut buf);
        buf
    }

    #[test]
    fn blit_reproduces_buffer_text() {
        let buf = buf_with(&["alpha", "bravo"], 10, 2);
        let mut surface = Surface::new(10, 2);
        blit(
            &mut surface,
            &buf,
            Rect {
                x: 0,
                y: 0,
                cols: 10,
                rows: 2,
            },
        );
        let text = surface.screen_chars_to_string();
        assert!(text.contains("alpha"), "got {text:?}");
        assert!(text.contains("bravo"), "got {text:?}");
    }

    #[test]
    fn blit_into_subrect_offsets_and_leaves_rest_blank() {
        let buf = buf_with(&["XX"], 2, 1);
        let mut surface = Surface::new(10, 3);
        blit(
            &mut surface,
            &buf,
            Rect {
                x: 3,
                y: 1,
                cols: 2,
                rows: 1,
            },
        );
        let lines: Vec<String> = surface
            .screen_chars_to_string()
            .lines()
            .map(|s| s.trim_end().to_string())
            .collect();
        assert_eq!(lines[0], "");
        assert_eq!(lines[1], "   XX");
    }

    #[test]
    fn color_mapping_covers_every_arm() {
        // Named, indexed, rgb, reset all resolve without panicking.
        assert_eq!(color_attr(RColor::Reset), ColorAttribute::Default);
        assert_eq!(color_attr(RColor::Black), ColorAttribute::PaletteIndex(0));
        assert_eq!(color_attr(RColor::White), ColorAttribute::PaletteIndex(15));
        assert_eq!(
            color_attr(RColor::Indexed(200)),
            ColorAttribute::PaletteIndex(200)
        );
        match color_attr(RColor::Rgb(255, 0, 0)) {
            ColorAttribute::TrueColorWithDefaultFallback(t) => assert!(t.0 > 0.99),
            other => panic!("rgb mapped to {other:?}"),
        }
    }

    #[test]
    fn reversed_modifier_swaps_fg_and_bg() {
        let mut buf = Buffer::empty(RRect::new(0, 0, 1, 1));
        buf.set_string(
            0,
            0,
            "x",
            Style::default()
                .fg(RColor::Rgb(10, 20, 30))
                .bg(RColor::Rgb(200, 100, 50))
                .add_modifier(Modifier::REVERSED),
        );
        let style = CellStyle::of(buf.cell((0, 0)).unwrap());
        assert_eq!(
            style.fg,
            color_attr(RColor::Rgb(200, 100, 50)),
            "fg should be the original bg"
        );
        assert_eq!(style.bg, color_attr(RColor::Rgb(10, 20, 30)));
    }

    #[test]
    fn bold_dim_and_underline_map() {
        let mut buf = Buffer::empty(RRect::new(0, 0, 3, 1));
        buf.set_string(0, 0, "b", Style::default().add_modifier(Modifier::BOLD));
        buf.set_string(1, 0, "d", Style::default().add_modifier(Modifier::DIM));
        buf.set_string(
            2,
            0,
            "u",
            Style::default().add_modifier(Modifier::UNDERLINED),
        );
        assert_eq!(
            CellStyle::of(buf.cell((0, 0)).unwrap()).intensity,
            Intensity::Bold
        );
        assert_eq!(
            CellStyle::of(buf.cell((1, 0)).unwrap()).intensity,
            Intensity::Half
        );
        assert_eq!(
            CellStyle::of(buf.cell((2, 0)).unwrap()).underline,
            Underline::Single
        );
    }
}
