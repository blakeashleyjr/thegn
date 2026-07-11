//! Warm-reattach screen snapshots: serialize a terminal emulator's current
//! screen to an **ANSI repaint sequence** a client feeds straight into its own
//! emulator.
//!
//! ANSI is the IR the whole stack already speaks — the compositor applies a
//! snapshot through the same `feed()` path as live PTY bytes, so no client
//! grows a second "apply a grid" code path (the tmux model: the server redraws
//! the pane from its authoritative grid on attach). This module is **pure**
//! (no emulator dep — the host adapter lowers its grid into [`ScreenSnapshot`])
//! so the encoding is golden-byte unit-tested here under the coverage gate.
//!
//! Encoding order: attribute reset → alt-screen enter *or* scrollback context
//! (the plain-text history tail, CRLF-normalized exactly like resurrect's
//! `repaint_scrollback`) → clear+home → per-row SGR-run-coalesced cell paint →
//! mode restores (app-cursor, bracketed paste) → cursor place + visibility.
//! The history tail ends with a plain-text copy of roughly the current screen
//! (the history ring records all output), which lands in the client's
//! scrollback just above the repainted grid — the same benign duplication the
//! existing session-resurrect path has.

use serde::{Deserialize, Serialize};

/// A color in terminal terms, mirroring the host emulator's `CellColor`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum SnapColor {
    #[default]
    Default,
    /// One of the 256 indexed colors.
    Indexed(u8),
    /// A 24-bit truecolor value.
    Rgb(u8, u8, u8),
}

/// One styled cell. `text` is usually one grapheme; empty ⇒ blank.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct SnapCell {
    pub text: String,
    pub fg: SnapColor,
    pub bg: SnapColor,
    pub bold: bool,
    pub italic: bool,
    pub underline: bool,
    pub inverse: bool,
    /// Set by the producer when `text` occupies two columns (the producer has
    /// the width tables; core stays dep-free). The cell *after* a wide cell is
    /// its spacer and is skipped by the encoder.
    pub wide: bool,
}

/// A full screen capture at a known output sequence point.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScreenSnapshot {
    pub rows: u16,
    pub cols: u16,
    /// Cursor as (row, col), zero-based.
    pub cursor: (u16, u16),
    pub cursor_visible: bool,
    pub alt_screen: bool,
    pub app_cursor: bool,
    pub bracketed_paste: bool,
    /// Bounded plain-text scrollback context (empty when `alt_screen` — the
    /// alternate screen has no scrollback).
    pub history_tail: String,
    /// Row-major, `rows * cols` cells.
    pub cells: Vec<SnapCell>,
    /// The last PTY output chunk folded into this grid; the first live delta
    /// after the snapshot carries `seq + 1`.
    pub seq: u64,
}

/// The SGR sequence selecting a cell's full style (always from a reset, so
/// runs never inherit stale attributes).
fn sgr(cell: &SnapCell) -> String {
    let mut params = String::from("0");
    if cell.bold {
        params.push_str(";1");
    }
    if cell.italic {
        params.push_str(";3");
    }
    if cell.underline {
        params.push_str(";4");
    }
    if cell.inverse {
        params.push_str(";7");
    }
    match cell.fg {
        SnapColor::Default => {}
        SnapColor::Indexed(n) => params.push_str(&format!(";38;5;{n}")),
        SnapColor::Rgb(r, g, b) => params.push_str(&format!(";38;2;{r};{g};{b}")),
    }
    match cell.bg {
        SnapColor::Default => {}
        SnapColor::Indexed(n) => params.push_str(&format!(";48;5;{n}")),
        SnapColor::Rgb(r, g, b) => params.push_str(&format!(";48;2;{r};{g};{b}")),
    }
    format!("\x1b[{params}m")
}

fn style_key(c: &SnapCell) -> (SnapColor, SnapColor, bool, bool, bool, bool) {
    (c.fg, c.bg, c.bold, c.italic, c.underline, c.inverse)
}

fn is_default_blank(c: &SnapCell) -> bool {
    c.text.is_empty() && style_key(c) == style_key(&SnapCell::default())
}

/// Serialize the snapshot to the ANSI repaint sequence.
pub fn encode_ansi(s: &ScreenSnapshot) -> Vec<u8> {
    let mut out: Vec<u8> = Vec::new();
    out.extend_from_slice(b"\x1b[0m");
    if s.alt_screen {
        out.extend_from_slice(b"\x1b[?1049h");
    } else if !s.history_tail.is_empty() {
        // The resurrect recipe: bare '\n' → CRLF, plus a trailing CRLF so the
        // repainted screen starts below the restored history.
        out.extend_from_slice(s.history_tail.replace('\n', "\r\n").as_bytes());
        out.extend_from_slice(b"\r\n");
    }
    out.extend_from_slice(b"\x1b[2J\x1b[H");

    for row in 0..s.rows {
        let base = row as usize * s.cols as usize;
        let cells = &s.cells[base..(base + s.cols as usize).min(s.cells.len())];
        // The screen was just cleared: trailing default blanks are already there.
        let mut last = cells.len();
        while last > 0 && is_default_blank(&cells[last - 1]) {
            last -= 1;
        }
        if last == 0 {
            continue;
        }
        out.extend_from_slice(format!("\x1b[{};1H", row + 1).as_bytes());
        let mut cur_style: Option<(SnapColor, SnapColor, bool, bool, bool, bool)> = None;
        let mut skip_spacer = false;
        for cell in &cells[..last] {
            if skip_spacer {
                skip_spacer = false;
                continue;
            }
            let key = style_key(cell);
            if cur_style != Some(key) {
                out.extend_from_slice(sgr(cell).as_bytes());
                cur_style = Some(key);
            }
            if cell.text.is_empty() {
                out.push(b' ');
            } else {
                out.extend_from_slice(cell.text.as_bytes());
            }
            skip_spacer = cell.wide;
        }
        out.extend_from_slice(b"\x1b[0m");
    }

    if s.app_cursor {
        out.extend_from_slice(b"\x1b[?1h");
    }
    if s.bracketed_paste {
        out.extend_from_slice(b"\x1b[?2004h");
    }
    out.extend_from_slice(format!("\x1b[{};{}H", s.cursor.0 + 1, s.cursor.1 + 1).as_bytes());
    out.extend_from_slice(if s.cursor_visible {
        b"\x1b[?25h"
    } else {
        b"\x1b[?25l"
    });
    out.extend_from_slice(b"\x1b[0m");
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn blank_snapshot(rows: u16, cols: u16) -> ScreenSnapshot {
        ScreenSnapshot {
            rows,
            cols,
            cursor: (0, 0),
            cursor_visible: true,
            alt_screen: false,
            app_cursor: false,
            bracketed_paste: false,
            history_tail: String::new(),
            cells: vec![SnapCell::default(); rows as usize * cols as usize],
            seq: 0,
        }
    }

    fn text_cell(t: &str) -> SnapCell {
        SnapCell {
            text: t.into(),
            ..Default::default()
        }
    }

    fn ansi(s: &ScreenSnapshot) -> String {
        String::from_utf8(encode_ansi(s)).unwrap()
    }

    #[test]
    fn empty_grid_is_just_clear_home_and_cursor() {
        let s = blank_snapshot(3, 4);
        assert_eq!(ansi(&s), "\x1b[0m\x1b[2J\x1b[H\x1b[1;1H\x1b[?25h\x1b[0m");
    }

    #[test]
    fn plain_text_paints_with_one_sgr_run() {
        let mut s = blank_snapshot(2, 4);
        s.cells[0] = text_cell("h");
        s.cells[1] = text_cell("i");
        s.cursor = (0, 2);
        let out = ansi(&s);
        // One row addressed, one default SGR for the whole run, trailing blanks
        // skipped, row closed with a reset.
        assert_eq!(
            out,
            "\x1b[0m\x1b[2J\x1b[H\x1b[1;1H\x1b[0mhi\x1b[0m\x1b[1;3H\x1b[?25h\x1b[0m"
        );
    }

    #[test]
    fn style_runs_coalesce_sgr() {
        let mut s = blank_snapshot(1, 6);
        let red = SnapCell {
            text: "r".into(),
            fg: SnapColor::Indexed(1),
            ..Default::default()
        };
        // Three same-style cells → one SGR; the style flip → a second.
        s.cells[0] = red.clone();
        s.cells[1] = SnapCell {
            text: "e".into(),
            ..red.clone()
        };
        s.cells[2] = SnapCell {
            text: "d".into(),
            ..red
        };
        s.cells[3] = SnapCell {
            text: "B".into(),
            bold: true,
            bg: SnapColor::Rgb(0, 10, 20),
            ..Default::default()
        };
        let out = ansi(&s);
        assert_eq!(out.matches("\x1b[0;38;5;1m").count(), 1);
        assert!(out.contains("\x1b[0;38;5;1mred"));
        assert!(out.contains("\x1b[0;1;48;2;0;10;20mB"));
    }

    #[test]
    fn styled_blank_cells_are_not_skipped() {
        // A bg-colored blank at end-of-row is real content (e.g. a status bar).
        let mut s = blank_snapshot(1, 3);
        s.cells[2] = SnapCell {
            bg: SnapColor::Indexed(4),
            ..Default::default()
        };
        let out = ansi(&s);
        assert!(
            out.contains("\x1b[0;48;5;4m "),
            "styled blank must paint: {out:?}"
        );
    }

    #[test]
    fn wide_glyph_spacer_is_skipped() {
        let mut s = blank_snapshot(1, 4);
        s.cells[0] = SnapCell {
            text: "漢".into(),
            wide: true,
            ..Default::default()
        };
        // cells[1] is the spacer; cells[2] carries the next glyph.
        s.cells[2] = text_cell("x");
        let out = ansi(&s);
        // The spacer contributes no space — the glyph is followed directly by "x".
        assert!(out.contains("漢x"), "spacer must be skipped: {out:?}");
    }

    #[test]
    fn history_tail_is_crlf_normalized_before_the_clear() {
        let mut s = blank_snapshot(1, 2);
        s.history_tail = "one\ntwo".into();
        let out = ansi(&s);
        let hist = out.find("one\r\ntwo\r\n").expect("history present");
        let clear = out.find("\x1b[2J").expect("clear present");
        assert!(hist < clear, "history must precede the clear");
    }

    #[test]
    fn alt_screen_enters_and_suppresses_history() {
        let mut s = blank_snapshot(1, 2);
        s.alt_screen = true;
        s.history_tail = "must not appear".into();
        let out = ansi(&s);
        assert!(out.contains("\x1b[?1049h"));
        assert!(!out.contains("must not appear"));
        // Alt-screen enter precedes the clear.
        assert!(out.find("\x1b[?1049h").unwrap() < out.find("\x1b[2J").unwrap());
    }

    #[test]
    fn mode_and_cursor_restores() {
        let mut s = blank_snapshot(4, 4);
        s.app_cursor = true;
        s.bracketed_paste = true;
        s.cursor = (2, 3);
        s.cursor_visible = false;
        let out = ansi(&s);
        assert!(out.contains("\x1b[?1h"));
        assert!(out.contains("\x1b[?2004h"));
        assert!(out.ends_with("\x1b[3;4H\x1b[?25l\x1b[0m"));
    }

    #[test]
    fn rows_are_absolutely_addressed() {
        // Content on row 3 with rows 1-2 blank: only row 3 is addressed, so
        // blank rows cost zero bytes.
        let mut s = blank_snapshot(3, 2);
        s.cells[4] = text_cell("z");
        let out = ansi(&s);
        assert!(out.contains("\x1b[3;1H\x1b[0mz"));
        assert!(!out.contains("\x1b[2;1H"));
    }
}
