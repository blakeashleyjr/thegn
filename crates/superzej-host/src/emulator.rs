//! The pane terminal-emulator seam.
//!
//! A `PaneEmulator` turns a PTY byte stream into a readable grid of styled
//! cells. The compositor reads that grid to paint the focused pane; background
//! panes still `advance()` (drain-without-render) so a backgrounded agent keeps
//! progressing.
//!
//! The spike impl is [`Vt100Emulator`] (the `vt100` crate — a full, simple
//! emulator). It is intentionally behind a trait: high-fidelity + image-protocol
//! support (sixel/kitty) swaps in a different impl (`alacritty_terminal` + an
//! escape-interception passthrough layer, or a `wezterm-term` git dep — the
//! latter is unpublished on crates.io) without touching the compositor.

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
    /// Borrow the underlying row text as a single string if supported.
    /// Returns `None` if the emulator cannot provide a fast-path row string,
    /// in which case the compositor will fall back to cell-by-cell iteration.
    fn row_text(&self, _row: u16) -> Option<String> {
        None
    }
    /// DECCKM: when set, arrows/Home/End must be sent SS3-encoded (`ESC O A`).
    fn application_cursor(&self) -> bool {
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

/// Captures the OSC window title (OSC 0/2) the app sets. vt100 surfaces titles
/// through a `Callbacks` impl rather than a `Screen` getter, so we sink the
/// latest one here and read it back via `Parser::callbacks()`.
#[derive(Debug, Default)]
pub struct TitleSink {
    title: String,
}

impl vt100::Callbacks for TitleSink {
    fn set_window_title(&mut self, _: &mut vt100::Screen, title: &[u8]) {
        self.title = String::from_utf8_lossy(title).into_owned();
    }
}

/// The `vt100`-backed spike emulator.
pub struct Vt100Emulator {
    parser: vt100::Parser<TitleSink>,
    /// Partial CSI carried between `advance` chunks for the HVP rewrite.
    hvp_carry: Vec<u8>,
}

impl Vt100Emulator {
    pub fn new(rows: u16, cols: u16, scrollback: usize) -> Self {
        Self {
            parser: vt100::Parser::new_with_callbacks(rows, cols, scrollback, TitleSink::default()),
            hvp_carry: Vec::new(),
        }
    }
}

/// vt100 0.16 implements CUP (`CSI r;c H`) but not its ANSI twin HVP
/// (`CSI r;c f`) — which btop uses EXCLUSIVELY for positioning, so its whole
/// frame collapses into a garble. Rewrite `f` finals (digit/`;` params only)
/// to `H` before parsing. Stateful: a CSI split across PTY read chunks is
/// carried into the next call (capped so binary noise can't grow it).
pub(crate) fn rewrite_hvp(input: &[u8], carry: &mut Vec<u8>) -> Vec<u8> {
    let mut data = std::mem::take(carry);
    data.extend_from_slice(input);
    let mut out = Vec::with_capacity(data.len());
    let mut i = 0;
    while i < data.len() {
        if data[i] == 0x1b && data.get(i + 1) == Some(&b'[') {
            let mut j = i + 2;
            while j < data.len() && matches!(data[j], 0x20..=0x3f) {
                j += 1;
            }
            if j >= data.len() {
                // Incomplete CSI at the chunk edge: carry it (bounded).
                if data.len() - i <= 64 {
                    carry.extend_from_slice(&data[i..]);
                } else {
                    out.extend_from_slice(&data[i..]);
                }
                break;
            }
            let fin = data[j];
            out.extend_from_slice(&data[i..j]);
            if fin == b'f'
                && data[i + 2..j]
                    .iter()
                    .all(|b| matches!(b, b'0'..=b'9' | b';'))
            {
                out.push(b'H');
            } else {
                out.push(fin);
            }
            i = j + 1;
        } else {
            out.push(data[i]);
            i += 1;
        }
    }
    out
}

fn conv_color(c: vt100::Color) -> CellColor {
    match c {
        vt100::Color::Default => CellColor::Default,
        vt100::Color::Idx(i) => CellColor::Indexed(i),
        vt100::Color::Rgb(r, g, b) => CellColor::Rgb(r, g, b),
    }
}

impl PaneEmulator for Vt100Emulator {
    fn advance(&mut self, bytes: &[u8]) {
        let fixed = rewrite_hvp(bytes, &mut self.hvp_carry);
        self.parser.process(&fixed);
    }

    fn resize(&mut self, rows: u16, cols: u16) {
        self.parser.screen_mut().set_size(rows, cols);
    }

    fn size(&self) -> (u16, u16) {
        self.parser.screen().size()
    }

    fn cell(&self, row: u16, col: u16) -> Option<GridCell> {
        let cell = self.parser.screen().cell(row, col)?;
        Some(GridCell {
            text: cell.contents().to_string(),
            fg: conv_color(cell.fgcolor()),
            bg: conv_color(cell.bgcolor()),
            bold: cell.bold(),
            italic: cell.italic(),
            underline: cell.underline(),
            inverse: cell.inverse(),
        })
    }

    fn title(&self) -> Option<String> {
        let t = &self.parser.callbacks().title;
        (!t.is_empty()).then(|| t.clone())
    }

    fn cursor(&self) -> (u16, u16) {
        self.parser.screen().cursor_position()
    }

    fn cursor_visible(&self) -> bool {
        !self.parser.screen().hide_cursor()
    }

    fn scroll_up(&mut self, n: usize) {
        let cur = self.parser.screen().scrollback();
        self.parser.screen_mut().set_scrollback(cur + n);
    }

    fn scroll_down(&mut self, n: usize) {
        let cur = self.parser.screen().scrollback();
        self.parser
            .screen_mut()
            .set_scrollback(cur.saturating_sub(n));
    }

    fn scroll_reset(&mut self) {
        self.parser.screen_mut().set_scrollback(0);
    }

    fn scrollback(&self) -> usize {
        self.parser.screen().scrollback()
    }

    fn row_text(&self, row: u16) -> Option<String> {
        let (_, cols) = self.size();
        let mut s = String::new();
        for col in 0..cols {
            match self.cell(row, col) {
                Some(c) => {
                    // The fast path blits plain text with no attributes — any
                    // styling on the row (colors, bold, …) must take the
                    // cell-by-cell path or the styling is silently dropped.
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
        // Full width — a trimmed blit would leave stale cells from a previous
        // frame visible past the new text (garbled htop/btop).
        Some(s)
    }

    fn application_cursor(&self) -> bool {
        self.parser.screen().application_cursor()
    }

    fn bracketed_paste(&self) -> bool {
        self.parser.screen().bracketed_paste()
    }

    fn mouse_mode(&self) -> (MouseMode, bool) {
        use vt100::MouseProtocolEncoding as E;
        use vt100::MouseProtocolMode as M;
        let screen = self.parser.screen();
        let mode = match screen.mouse_protocol_mode() {
            M::None => MouseMode::None,
            M::Press => MouseMode::Press,
            M::PressRelease => MouseMode::PressRelease,
            M::ButtonMotion => MouseMode::ButtonMotion,
            M::AnyMotion => MouseMode::AnyMotion,
        };
        let sgr = matches!(screen.mouse_protocol_encoding(), E::Sgr);
        (mode, sgr)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_text_lands_in_the_grid() {
        let mut e = Vt100Emulator::new(24, 80, 0);
        e.advance(b"hello world");
        assert_eq!(
            e.row_text(0).map(|r| r.trim_end().to_string()),
            Some("hello world".to_string())
        );
        assert_eq!(e.cursor(), (0, 11));
    }

    #[test]
    fn styled_rows_refuse_the_fast_path_so_color_survives() {
        let mut e = Vt100Emulator::new(24, 80, 0);
        e.advance(b"plain\r\n\x1b[31mred text\x1b[0m\r\n\x1b[1mbold\x1b[0m");
        // Unstyled rows blit fast…
        assert_eq!(
            e.row_text(0).map(|r| r.trim_end().to_string()),
            Some("plain".to_string())
        );
        assert_eq!(
            e.row_text(0).map(|r| r.chars().count()),
            Some(80),
            "fast-path rows are full width so stale cells get overwritten"
        );
        // …but any colored/bold row must go cell-by-cell (the fast path would
        // strip its attributes).
        assert_eq!(e.row_text(1), None, "colored row must not fast-path");
        assert_eq!(e.row_text(2), None, "bold row must not fast-path");
        // The styling is intact on the cells themselves.
        let c = e.cell(1, 0).unwrap();
        assert_eq!(c.fg, CellColor::Indexed(1));
    }

    #[test]
    fn osc_window_title_is_captured() {
        let mut e = Vt100Emulator::new(24, 80, 0);
        // No title set yet → None (so callers fall back to a derived name).
        assert_eq!(e.title(), None);
        // OSC 2 (BEL-terminated) sets the window title.
        e.advance(b"\x1b]2;my-title\x07");
        assert_eq!(e.title(), Some("my-title".to_string()));
        // OSC 0 sets both icon name and title; a later title overwrites.
        e.advance(b"\x1b]0;newer\x07");
        assert_eq!(e.title(), Some("newer".to_string()));
    }

    #[test]
    fn newline_advances_row() {
        let mut e = Vt100Emulator::new(24, 80, 0);
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
        let mut e = Vt100Emulator::new(24, 80, 0);
        // bold + red foreground, one char, then reset.
        e.advance(b"\x1b[1;31mX\x1b[0m");
        let c = e.cell(0, 0).unwrap();
        assert_eq!(c.text, "X");
        assert!(c.bold);
        assert_eq!(c.fg, CellColor::Indexed(1));
    }

    #[test]
    fn scrollback_view_reveals_history() {
        // A 3-row screen with scrollback; print 6 lines so 3 scroll off-screen.
        let mut e = Vt100Emulator::new(3, 20, 100);
        for i in 1..=6 {
            e.advance(format!("line{i}\r\n").as_bytes());
        }
        // Live tail: the last lines are visible, line1 is gone.
        assert_eq!(e.scrollback(), 0);
        let tail: Vec<String> = (0..3)
            .map(|r| e.row_text(r).unwrap_or_default().trim_end().to_string())
            .collect();
        assert!(
            tail.iter().any(|l| l == "line5"),
            "tail shows recent: {tail:?}"
        );
        assert!(!tail.iter().any(|l| l == "line1"));

        // Scroll all the way up into history — the oldest line comes into view
        // (vt100 clamps the offset to the available scrollback).
        e.scroll_up(100);
        assert!(e.scrollback() > 0, "offset advanced into history");
        let hist: Vec<String> = (0..3)
            .map(|r| e.row_text(r).unwrap_or_default().trim_end().to_string())
            .collect();
        assert!(
            hist.iter().any(|l| l == "line1"),
            "history shows line1: {hist:?}"
        );

        // Reset returns to the live tail.
        e.scroll_reset();
        assert_eq!(e.scrollback(), 0);
    }

    #[test]
    fn resize_changes_reported_size() {
        let mut e = Vt100Emulator::new(24, 80, 0);
        e.resize(40, 100);
        assert_eq!(e.size(), (40, 100));
    }

    #[test]
    fn hvp_rewrites_to_cup_including_split_chunks() {
        let mut carry = Vec::new();
        // Plain rewrite, params preserved; H and other finals untouched.
        let out = rewrite_hvp(b"\x1b[14;5fX\x1b[2;3Hy\x1b[1C", &mut carry);
        assert_eq!(out, b"\x1b[14;5HX\x1b[2;3Hy\x1b[1C");
        assert!(carry.is_empty());
        // Non-numeric params (private modes) keep their final byte.
        let out = rewrite_hvp(b"\x1b[?25f", &mut carry);
        assert_eq!(out, b"\x1b[?25f");
        // Split across chunks: the partial CSI carries over.
        let out1 = rewrite_hvp(b"ab\x1b[40;", &mut carry);
        assert_eq!(out1, b"ab");
        assert!(!carry.is_empty());
        let out2 = rewrite_hvp(b"12f!", &mut carry);
        assert_eq!(out2, b"\x1b[40;12H!");
        assert!(carry.is_empty());
    }

    #[test]
    fn emulator_positions_via_hvp_like_btop() {
        let mut emu = Vt100Emulator::new(10, 40, 0);
        emu.advance(b"\x1b[3;5fBTOP");
        assert_eq!(emu.cell(2, 4).unwrap().text, "B");
        assert_eq!(emu.cell(2, 7).unwrap().text, "P");
    }
}
