//! Copy-mode primitives: a cell selection over a pane grid, text extraction,
//! and OSC 52 clipboard encoding. The selection model + extraction + base64 are
//! pure and unit-tested; the host wires a "copy" action that emits OSC 52 to the
//! outer terminal (the mouse-drag / keyboard-cursor UX that *builds* a selection
//! is the terminal-verified layer on top).

use crate::emulator::PaneEmulator;

/// A half-open-free inclusive cell selection. The row is an **absolute grid
/// line** (alacritty `Line`: 0 = top of the live screen, negative = scrollback
/// history) so the selection stays glued to its content as the viewport
/// scrolls; the col is a plain screen column.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Selection {
    pub anchor: (i32, u16),
    pub cursor: (i32, u16),
}

impl Selection {
    /// Begin a selection at `anchor` (the mouse-down point, in absolute lines).
    pub fn new(anchor: (i32, u16)) -> Self {
        Self {
            anchor,
            cursor: anchor,
        }
    }

    /// Ordered bounds for the mouse-selection overlay renderer.
    pub fn ordered(&self) -> (i32, u16, i32, u16) {
        self.bounds()
    }

    /// Ordered bounds `(start_row, start_col, end_row, end_col)` with start ≤ end.
    fn bounds(&self) -> (i32, u16, i32, u16) {
        let (a, c) = (self.anchor, self.cursor);
        if (a.0, a.1) <= (c.0, c.1) {
            (a.0, a.1, c.0, c.1)
        } else {
            (c.0, c.1, a.0, a.1)
        }
    }
}

/// A full-grid selection (used by "copy whole pane") — the currently visible
/// screen, expressed in absolute lines so it copies what's on screen even when
/// scrolled into history.
pub fn whole(emu: &dyn PaneEmulator) -> Selection {
    let (rows, cols) = emu.size();
    let off = emu.scrollback() as i32;
    Selection {
        anchor: (-off, 0),
        cursor: (rows.saturating_sub(1) as i32 - off, cols.saturating_sub(1)),
    }
}

/// Extract the selected text from `emu`'s grid, linewise with first/last column
/// bounds, trailing blanks per line trimmed, rows joined by `\n`.
pub fn extract(emu: &dyn PaneEmulator, sel: &Selection) -> String {
    let (_, cols) = emu.size();
    let (sr, sc, er, ec) = sel.bounds();
    let mut lines = Vec::new();
    for r in sr..=er {
        let (from, to) = if sr == er {
            (sc, ec.saturating_add(1))
        } else if r == sr {
            (sc, cols)
        } else if r == er {
            (0, ec.saturating_add(1))
        } else {
            (0, cols)
        };
        let mut line = String::new();
        for col in from..to.min(cols) {
            match emu.cell_abs(r, col) {
                Some(c) if !c.text.is_empty() => line.push_str(&c.text),
                _ => line.push(' '),
            }
        }
        lines.push(line.trim_end().to_string());
    }
    lines.join("\n")
}

/// Standard base64 (with padding) — a tiny encoder so we don't pull a dep just
/// for OSC 52.
fn base64(data: &[u8]) -> String {
    const A: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let b0 = chunk[0];
        let b1 = *chunk.get(1).unwrap_or(&0);
        let b2 = *chunk.get(2).unwrap_or(&0);
        out.push(A[(b0 >> 2) as usize] as char);
        out.push(A[(((b0 & 0x03) << 4) | (b1 >> 4)) as usize] as char);
        out.push(if chunk.len() > 1 {
            A[(((b1 & 0x0f) << 2) | (b2 >> 6)) as usize] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            A[(b2 & 0x3f) as usize] as char
        } else {
            '='
        });
    }
    out
}

/// Encode an OSC 52 "set clipboard" sequence for `text` (terminator BEL). The
/// outer terminal, if it supports OSC 52, copies this to the system clipboard.
pub fn osc52(text: &str) -> Vec<u8> {
    format!("\x1b]52;c;{}\x07", base64(text.as_bytes())).into_bytes()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::emulator::AlacrittyEmulator;

    #[test]
    fn base64_matches_known_vectors() {
        assert_eq!(base64(b""), "");
        assert_eq!(base64(b"f"), "Zg==");
        assert_eq!(base64(b"fo"), "Zm8=");
        assert_eq!(base64(b"foo"), "Zm9v");
        assert_eq!(base64(b"foob"), "Zm9vYg==");
        assert_eq!(base64(b"hi"), "aGk=");
    }

    #[test]
    fn osc52_wraps_base64_in_the_escape() {
        let seq = String::from_utf8(osc52("hi")).unwrap();
        assert_eq!(seq, "\x1b]52;c;aGk=\x07");
    }

    #[test]
    fn extract_single_line_respects_column_bounds() {
        let mut e = AlacrittyEmulator::new(3, 20, 0);
        e.advance(b"hello world");
        // Select columns 6..=10 of row 0 -> "world".
        let sel = Selection {
            anchor: (0, 6),
            cursor: (0, 10),
        };
        assert_eq!(extract(&e, &sel), "world");
    }

    #[test]
    fn extract_multiline_takes_partial_first_last_rows() {
        let mut e = AlacrittyEmulator::new(3, 20, 0);
        e.advance(b"abcdef\r\nghijkl\r\nmnopqr");
        // From row0 col3 to row2 col2: "def" / "ghijkl" / "mno".
        let sel = Selection {
            anchor: (0, 3),
            cursor: (2, 2),
        };
        assert_eq!(extract(&e, &sel), "def\nghijkl\nmno");
    }

    #[test]
    fn selection_bounds_normalize_regardless_of_drag_direction() {
        // Dragging up-left still yields ordered bounds.
        let s = Selection {
            anchor: (2, 5),
            cursor: (0, 1),
        };
        assert_eq!(s.bounds(), (0, 1, 2, 5));
    }

    #[test]
    fn extract_reaches_rows_scrolled_off_into_history() {
        // 3 visible rows, 10-line scrollback. After 5 lines, `aaa`/`bbb` have
        // scrolled off the tail into history (absolute Lines -2 / -1); `ccc`/
        // `ddd`/`eee` are the visible Lines 0..2.
        let mut e = AlacrittyEmulator::new(3, 20, 10);
        e.advance(b"aaa\r\nbbb\r\nccc\r\nddd\r\neee");
        // Scroll the viewport up into history — the bug was that copying then
        // only kept on-screen rows. Absolute-line extraction must still reach
        // the scrolled-off lines regardless of viewport offset.
        e.scroll_up(2);
        let sel = Selection {
            anchor: (-2, 0), // history line `aaa`
            cursor: (0, 2),  // first visible line `ccc`
        };
        assert_eq!(extract(&e, &sel), "aaa\nbbb\nccc");
    }

    #[test]
    fn whole_pane_selection_copies_all_visible_rows() {
        let mut e = AlacrittyEmulator::new(2, 10, 0);
        e.advance(b"top\r\nbottom");
        let txt = extract(&e, &whole(&e));
        assert_eq!(txt, "top\nbottom");
    }
}
