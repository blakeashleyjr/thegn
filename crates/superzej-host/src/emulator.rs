//! The pane terminal-emulator seam.
//!
//! A `PaneEmulator` turns a PTY byte stream into a readable grid of styled
//! cells. The compositor reads that grid to paint the focused pane; background
//! panes still `advance()` (drain-without-render) so a backgrounded agent keeps
//! progressing.
//!
//! The spike impl is `Vt100Emulator` (the `vt100` crate — a full, simple
//! emulator). It is intentionally behind a trait: high-fidelity + image-protocol
//! support (sixel/kitty) swaps in a different impl (`alacritty_terminal` + an
//! escape-interception passthrough layer, or a `wezterm-term` git dep — the
//! latter is unpublished on crates.io) without touching the compositor.

use alacritty_terminal::event::{Event as AlacrittyEvent, EventListener};
use alacritty_terminal::grid::{Dimensions, Scroll};
use alacritty_terminal::sync::FairMutex;
use alacritty_terminal::term::Term;
use alacritty_terminal::term::{Config, TermMode};
use alacritty_terminal::vte::ansi::Processor;
use std::sync::Arc;

/// One styled cell, renderer-agnostic.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct GridCell {
    /// Cell contents (usually one grapheme; empty == blank).
    pub text: String,
    pub fg: CellColor,
    pub bg: CellColor,
    pub bold: bool,
    pub italic: bool,
    pub underline: bool,
    pub inverse: bool,
}

/// A borrowing view of a cell — same fields as [`GridCell`] but the glyph is a
/// `&str` into the emulator's own grid instead of an owned `String`. The render
/// hot path ([`crate::compositor::compose_pane`]) reads every visible cell every
/// frame and only ever appends the glyph to a run buffer, so borrowing here
/// avoids a heap allocation per cell per frame (vt100 stores glyphs inline, so
/// `Cell::contents()` is already a cheap borrow).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CellRef<'a> {
    pub text: &'a str,
    pub fg: CellColor,
    pub bg: CellColor,
    pub bold: bool,
    pub italic: bool,
    pub underline: bool,
    pub inverse: bool,
}

/// A color in terminal terms, normalized away from any one library's enum.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum CellColor {
    #[default]
    Default,
    /// One of the 256 indexed colors.
    Indexed(u8),
    /// A 24-bit truecolor value.
    Rgb(u8, u8, u8),
}

/// A terminal emulator for a single pane.
pub trait PaneEmulator: Send {
    /// Feed PTY output bytes (advances the screen; never renders).
    fn advance(&mut self, bytes: &[u8]);
    /// Resize the screen to `rows` x `cols`.
    fn resize(&mut self, rows: u16, cols: u16);
    /// Current grid size as `(rows, cols)`.
    fn size(&self) -> (u16, u16);
    /// Cell at `(row, col)`, or `None` if out of range.
    fn cell(&self, row: u16, col: u16) -> Option<GridCell>;
    /// Borrowing view of the cell at `(row, col)` — the allocation-free path the
    /// compositor uses. Defaults to `None`; emulators that can expose the glyph
    /// as a borrow override it (the compositor falls back to [`Self::cell`] when
    /// this returns `None`).
    fn cell_ref(&self, _row: u16, _col: u16) -> Option<CellRef<'_>> {
        None
    }
    /// The OSC window title (OSC 0/2) the app last set, if any. `None` when the
    /// app has set no title, so callers can fall back to a derived name.
    fn title(&self) -> Option<String> {
        None
    }
    /// Cursor position as `(row, col)`.
    fn cursor(&self) -> (u16, u16);
    /// Whether the cursor should be drawn (hidden in some modes).
    #[allow(dead_code)]
    fn cursor_visible(&self) -> bool;
    /// Scroll the viewport up into history by `n` rows (copy-mode / scrollback).
    fn scroll_up(&mut self, _n: usize) {}
    /// Scroll the viewport back down toward the live tail by `n` rows.
    fn scroll_down(&mut self, _n: usize) {}
    /// Jump back to the live tail (offset 0).
    fn scroll_reset(&mut self) {}
    /// Current scrollback offset in rows (0 == live tail).
    fn scrollback(&self) -> usize {
        0
    }
    /// Borrow a visible row as a single plain string when it carries no
    /// styling, else `None`. The compositor composes cell-by-cell (so this is
    /// no longer on its hot path), but it stays as a cheap accessor that tests
    /// use to assert PTY output landed in the grid.
    #[allow(dead_code)]
    fn row_text(&self, _row: u16) -> Option<String> {
        None
    }
    /// DECCKM: when set, arrows/Home/End must be sent SS3-encoded (`ESC O A`).
    fn application_cursor(&self) -> bool {
        false
    }
    /// Alternate-screen active (a full-screen TUI). Predictive echo never fires
    /// here. Default `false` for non-alacritty emulators.
    fn alt_screen(&self) -> bool {
        false
    }
    /// Bracketed paste: when set, pastes are wrapped in `ESC[200~ … ESC[201~`.
    fn bracketed_paste(&self) -> bool {
        false
    }
    /// The mouse reporting the app requested: `(mode, SGR encoding?)`. The
    /// host forwards matching mouse events into the pane instead of using
    /// them for its own selection (hold Shift to force host selection).
    fn mouse_mode(&self) -> (MouseMode, bool) {
        (MouseMode::None, false)
    }
    /// Number of lines currently stored in the parallel history ring. Used by
    /// `apply_search_jump` to compute the scroll offset needed to bring a
    /// matched line into view. Returns `None` when the emulator has no attached
    /// history ring (e.g. in unit tests that use a stub emulator).
    #[allow(dead_code)] // used by search jump scroll-offset calculation
    fn history_len(&self) -> Option<usize> {
        None
    }
}

/// Mouse reporting level an app can request (DECSET 9/1000/1002/1003),
/// normalized away from any one library's enum.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum MouseMode {
    #[default]
    None,
    Press,
    PressRelease,
    ButtonMotion,
    AnyMotion,
}

#[derive(Clone)]
pub struct EventProxy;

impl EventListener for EventProxy {
    fn send_event(&self, _event: AlacrittyEvent) {}
}

pub struct AlacrittyEmulator {
    term: Arc<FairMutex<Term<EventProxy>>>,
    parser: Processor,
}

#[derive(Clone, Copy)]
struct PaneSize {
    cols: usize,
    rows: usize,
}

impl Dimensions for PaneSize {
    fn total_lines(&self) -> usize {
        self.rows
    }
    fn screen_lines(&self) -> usize {
        self.rows
    }
    fn columns(&self) -> usize {
        self.cols
    }
}

impl AlacrittyEmulator {
    pub fn new(rows: u16, cols: u16, scrollback: usize) -> Self {
        let size = PaneSize {
            cols: cols as usize,
            rows: rows as usize,
        };
        let config = Config {
            scrolling_history: scrollback,
            ..Default::default()
        };

        let term = Term::new(config, &size, EventProxy);
        Self {
            term: Arc::new(FairMutex::new(term)),
            parser: Processor::new(),
        }
    }
}

fn conv_color(c: alacritty_terminal::vte::ansi::Color) -> CellColor {
    use alacritty_terminal::vte::ansi::Color;
    use alacritty_terminal::vte::ansi::NamedColor;
    match c {
        Color::Indexed(i) => CellColor::Indexed(i),
        Color::Spec(rgb) => CellColor::Rgb(rgb.r, rgb.g, rgb.b),
        Color::Named(NamedColor::Foreground) | Color::Named(NamedColor::Background) => {
            CellColor::Default
        }
        Color::Named(n) => CellColor::Indexed(n as u8),
    }
}

impl PaneEmulator for AlacrittyEmulator {
    fn advance(&mut self, bytes: &[u8]) {
        let mut term = self.term.lock();
        self.parser.advance(&mut *term, bytes);
    }

    fn resize(&mut self, rows: u16, cols: u16) {
        let mut term = self.term.lock();
        let size = PaneSize {
            cols: cols as usize,
            rows: rows as usize,
        };
        term.resize(size);
    }

    fn size(&self) -> (u16, u16) {
        let term = self.term.lock();
        (term.screen_lines() as u16, term.columns() as u16)
    }

    fn cell(&self, row: u16, col: u16) -> Option<GridCell> {
        let term = self.term.lock();
        if row >= term.screen_lines() as u16 || col >= term.columns() as u16 {
            return None;
        }
        let display_offset = term.grid().display_offset();
        let point = alacritty_terminal::index::Point::new(
            alacritty_terminal::index::Line(row as i32 - display_offset as i32),
            alacritty_terminal::index::Column(col as usize),
        );
        let cell = &term.grid()[point];
        Some(GridCell {
            text: cell.c.to_string(),
            fg: conv_color(cell.fg),
            bg: conv_color(cell.bg),
            bold: cell
                .flags
                .contains(alacritty_terminal::term::cell::Flags::BOLD),
            italic: cell
                .flags
                .contains(alacritty_terminal::term::cell::Flags::ITALIC),
            underline: cell
                .flags
                .contains(alacritty_terminal::term::cell::Flags::UNDERLINE),
            inverse: cell
                .flags
                .contains(alacritty_terminal::term::cell::Flags::INVERSE),
        })
    }

    fn title(&self) -> Option<String> {
        // OSC titles come through EventListener (AlacrittyEvent::Title(t)).
        // For now, we skip capturing it to get it compiling cleanly.
        None
    }

    fn cursor(&self) -> (u16, u16) {
        let term = self.term.lock();
        let point = term.grid().cursor.point;
        (point.line.0 as u16, point.column.0 as u16)
    }

    fn cursor_visible(&self) -> bool {
        let term = self.term.lock();
        term.mode().contains(TermMode::SHOW_CURSOR)
    }

    fn scroll_up(&mut self, n: usize) {
        let n_i32 = (n as isize).try_into().unwrap_or(i32::MAX);
        self.term.lock().scroll_display(Scroll::Delta(n_i32));
    }

    fn scroll_down(&mut self, n: usize) {
        let n_i32 = (-(n as isize)).try_into().unwrap_or(i32::MIN);
        self.term.lock().scroll_display(Scroll::Delta(n_i32));
    }

    fn scroll_reset(&mut self) {
        self.term.lock().scroll_display(Scroll::Bottom);
    }

    fn scrollback(&self) -> usize {
        self.term.lock().grid().display_offset()
    }

    fn row_text(&self, row: u16) -> Option<String> {
        let (_, cols) = self.size();
        let mut s = String::new();
        for col in 0..cols {
            match self.cell(row, col) {
                Some(c) => {
                    if c.bold
                        || c.italic
                        || c.underline
                        || c.inverse
                        || c.fg != CellColor::Default
                        || c.bg != CellColor::Default
                    {
                        return None;
                    }
                    if c.text.is_empty() {
                        s.push(' ');
                    } else {
                        s.push_str(&c.text);
                    }
                }
                _ => s.push(' '),
            }
        }
        Some(s)
    }

    fn application_cursor(&self) -> bool {
        self.term.lock().mode().contains(TermMode::APP_CURSOR)
    }

    fn alt_screen(&self) -> bool {
        self.term.lock().mode().contains(TermMode::ALT_SCREEN)
    }

    fn bracketed_paste(&self) -> bool {
        self.term.lock().mode().contains(TermMode::BRACKETED_PASTE)
    }

    fn mouse_mode(&self) -> (MouseMode, bool) {
        let term = self.term.lock();
        let mode = term.mode();
        let mm = if mode.contains(TermMode::MOUSE_MOTION) {
            MouseMode::AnyMotion
        } else if mode.contains(TermMode::MOUSE_DRAG) {
            MouseMode::ButtonMotion
        } else if mode.contains(TermMode::MOUSE_REPORT_CLICK) {
            MouseMode::PressRelease
        } else {
            MouseMode::None
        };
        let sgr = mode.contains(TermMode::SGR_MOUSE);
        (mm, sgr)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_text_lands_in_the_grid() {
        let mut e = AlacrittyEmulator::new(24, 80, 0);
        e.advance(b"hello world");
        assert_eq!(
            e.row_text(0).map(|r| r.trim_end().to_string()),
            Some("hello world".to_string())
        );
        assert_eq!(e.cursor(), (0, 11));
    }

    #[test]
    fn styled_rows_refuse_the_fast_path_so_color_survives() {
        let mut e = AlacrittyEmulator::new(24, 80, 0);
        e.advance(b"plain\r\n\x1b[31mred text\x1b[0m\r\n\x1b[1mbold\x1b[0m");
        // Unstyled rows blit fast...
        assert_eq!(
            e.row_text(0).map(|r| r.trim_end().to_string()),
            Some("plain".to_string())
        );
        assert_eq!(
            e.row_text(0).map(|r| r.chars().count()),
            Some(80),
            "fast-path rows are full width so stale cells get overwritten"
        );
        // ...but any colored/bold row must go cell-by-cell.
        assert_eq!(e.row_text(1), None, "colored row must not fast-path");
        assert_eq!(e.row_text(2), None, "bold row must not fast-path");
        let c = e.cell(1, 0).unwrap();
        assert_eq!(c.fg, CellColor::Indexed(1));
    }

    #[test]
    fn newline_advances_row() {
        let mut e = AlacrittyEmulator::new(24, 80, 0);
        e.advance(b"line1\r\nline2");
        assert_eq!(
            e.row_text(0).map(|r| r.trim_end().to_string()),
            Some("line1".to_string())
        );
        assert_eq!(
            e.row_text(1).map(|r| r.trim_end().to_string()),
            Some("line2".to_string())
        );
    }

    #[test]
    fn sgr_bold_and_color_are_captured() {
        let mut e = AlacrittyEmulator::new(24, 80, 0);
        e.advance(b"\x1b[1;31mX\x1b[0m");
        let c = e.cell(0, 0).unwrap();
        assert_eq!(c.text, "X");
        assert!(c.bold);
        assert_eq!(c.fg, CellColor::Indexed(1));
    }

    #[test]
    fn scrollback_view_reveals_history() {
        let mut e = AlacrittyEmulator::new(3, 20, 100);
        for i in 1..=6 {
            e.advance(format!("line{i}\r\n").as_bytes());
        }
        assert_eq!(e.scrollback(), 0);
        let tail: Vec<String> = (0..3)
            .map(|r| e.row_text(r).unwrap_or_default().trim_end().to_string())
            .collect();
        assert!(
            tail.iter().any(|l| l == "line5"),
            "tail shows recent: {tail:?}"
        );
        assert!(!tail.iter().any(|l| l == "line1"));

        e.scroll_up(100);
        assert!(e.scrollback() > 0, "offset advanced into history");
        let hist: Vec<String> = (0..3)
            .map(|r| e.row_text(r).unwrap_or_default().trim_end().to_string())
            .collect();
        assert!(
            hist.iter().any(|l| l == "line1"),
            "history shows line1: {hist:?}"
        );

        e.scroll_reset();
        assert_eq!(e.scrollback(), 0);
    }

    #[test]
    fn resize_changes_reported_size() {
        let mut e = AlacrittyEmulator::new(24, 80, 0);
        e.resize(40, 100);
        assert_eq!(e.size(), (40, 100));
    }

    #[test]
    fn emulator_positions_via_hvp_like_btop() {
        let mut emu = AlacrittyEmulator::new(10, 40, 0);
        emu.advance(b"\x1b[3;5fBTOP");
        assert_eq!(emu.cell(2, 4).unwrap().text, "B");
        assert_eq!(emu.cell(2, 7).unwrap().text, "P");
    }
}
