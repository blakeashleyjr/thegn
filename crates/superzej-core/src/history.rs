//! Per-pane terminal history ring.
//!
//! Each `PtyPane` owns a `HistoryBuffer` that records every line of output as
//! plain text, parallel to the vt100 styled-cell grid. The buffer is the fast
//! path for incremental search: nucleo scores `&str` rows directly, avoiding
//! per-cell extraction from the grid on every keystroke.
//!
//! Lines are stored ANSI-stripped so queries match the visible content, not the
//! escape sequences that colorize it. The stripper is a small state machine that
//! handles sequences that arrive split across PTY read chunks (the `partial`
//! carry buffer in `PtyPane` deals with line boundaries; this deals with escape
//! boundaries within a line).

use std::collections::VecDeque;

// ── ANSI stripping ────────────────────────────────────────────────────────────

#[derive(Default, Clone, Copy, PartialEq, Eq)]
enum AnsiState {
    #[default]
    Normal,
    /// Saw ESC.
    Esc,
    /// Inside a CSI sequence (`ESC [`); accumulating parameter/intermediate bytes.
    Csi,
    /// Inside a non-CSI escape sequence (e.g. OSC, DCS); drop until ST or BEL.
    Other,
}

/// Stateful ANSI escape-sequence stripper. Keeps state across calls so a
/// sequence split at a chunk boundary is handled correctly.
#[derive(Default, Clone)]
pub struct AnsiStripper {
    state: AnsiState,
}

impl AnsiStripper {
    /// Strip ANSI escape sequences from `input`, appending visible bytes to
    /// `out`. Call repeatedly with successive chunks; state carries over.
    pub fn strip_into(&mut self, input: &[u8], out: &mut Vec<u8>) {
        for &b in input {
            match self.state {
                AnsiState::Normal => {
                    if b == 0x1b {
                        self.state = AnsiState::Esc;
                    } else if b >= 0x20 || matches!(b, b'\n' | b'\r' | b'\t') {
                        // Printable or significant whitespace.
                        out.push(b);
                    }
                    // Drop other C0 controls (e.g. BEL 0x07, BS 0x08, …).
                }
                AnsiState::Esc => {
                    match b {
                        b'[' => self.state = AnsiState::Csi,
                        // ESC followed by anything else: private two-byte seq or
                        // OSC/DCS/PM/APC all start here — drop until we understand
                        // the sequence terminator.
                        b'P' | b']' | b'^' | b'_' | b'X' => self.state = AnsiState::Other,
                        _ => {
                            // Two-byte sequence (ESC + final) fully consumed.
                            self.state = AnsiState::Normal;
                        }
                    }
                }
                AnsiState::Csi => {
                    // CSI parameter/intermediate: 0x20–0x3F; final: 0x40–0x7E.
                    if (0x40..=0x7e).contains(&b) {
                        self.state = AnsiState::Normal;
                    }
                    // Drop the byte either way.
                }
                AnsiState::Other => {
                    // OSC/DCS: terminated by ST (ESC \) or BEL (0x07).
                    if b == 0x07 {
                        self.state = AnsiState::Normal;
                    } else if b == 0x1b {
                        // Could be the ESC of ST (ESC \) — transition to Esc.
                        self.state = AnsiState::Esc;
                    }
                    // Drop content bytes.
                }
            }
        }
    }

    /// Convenience: strip a `&str` and return the clean `String`.
    pub fn strip_str(s: &str) -> String {
        let mut st = AnsiStripper::default();
        let mut out = Vec::with_capacity(s.len());
        st.strip_into(s.as_bytes(), &mut out);
        String::from_utf8_lossy(&out).into_owned()
    }
}

// ── HistoryBuffer ─────────────────────────────────────────────────────────────

/// Fixed-capacity ring of plain-text lines from a pane's PTY output.
///
/// Owned by `PtyPane`. Written as bytes arrive (via `PtyPane::feed_history`);
/// read by the search engine without locking — the event loop is single-threaded
/// on the read side, and the write side is also the event loop (feed happens
/// synchronously in the PTY-output drain path).
#[derive(Clone)]
pub struct HistoryBuffer {
    lines: VecDeque<String>,
    capacity: usize,
    /// Monotonically increasing count of all lines ever pushed (even after eviction).
    total: u64,
}

impl HistoryBuffer {
    pub fn new(capacity: usize) -> Self {
        let cap = capacity.max(1);
        Self {
            lines: VecDeque::with_capacity(cap.min(4096)),
            capacity: cap,
            total: 0,
        }
    }

    /// Push one already-stripped line. Empty lines are stored as-is so that
    /// blank lines in output are searchable and the line indices stay in sync
    /// with the vt100 scrollback.
    pub fn push_line(&mut self, line: String) {
        if self.lines.len() >= self.capacity {
            self.lines.pop_front();
        }
        self.lines.push_back(line);
        self.total += 1;
    }

    /// Iterate lines oldest-first.
    pub fn iter(&self) -> impl Iterator<Item = &str> {
        self.lines.iter().map(String::as_str)
    }

    /// Lines currently stored (≤ capacity).
    pub fn len(&self) -> usize {
        self.lines.len()
    }

    pub fn is_empty(&self) -> bool {
        self.lines.is_empty()
    }

    /// Monotonic count of all lines ever pushed (survives ring eviction).
    pub fn total_pushed(&self) -> u64 {
        self.total
    }

    /// The `idx`-th stored line (0 = oldest surviving). Returns `None` if out
    /// of range.
    pub fn get(&self, idx: usize) -> Option<&str> {
        self.lines.get(idx).map(String::as_str)
    }

    /// Capacity (max lines stored at once).
    pub fn capacity(&self) -> usize {
        self.capacity
    }
}

// ── Line extractor (used by PtyPane::feed) ────────────────────────────────────

/// Splits raw PTY bytes into lines, strips ANSI, and pushes to `buf`.
/// `partial` carries an incomplete line across chunk boundaries.
///
/// This is a free function so `PtyPane` doesn't need to own an `AnsiStripper`
/// as separate state — the stripper is a simple value embedded in `PtyPane`.
pub fn feed_bytes_to_history(
    bytes: &[u8],
    buf: &mut HistoryBuffer,
    partial: &mut Vec<u8>,
    stripper: &mut AnsiStripper,
) {
    // Force-flush a partial that has grown unreasonably large (binary / no-LF output).
    const MAX_PARTIAL: usize = 4096;

    // We strip ANSI in-place into `stripped`, then split on '\n'.
    let mut stripped: Vec<u8> = Vec::with_capacity(bytes.len());
    stripper.strip_into(bytes, &mut stripped);

    for &b in &stripped {
        if b == b'\n' {
            // Complete the current line.
            let line = String::from_utf8_lossy(partial).into_owned();
            // Trim trailing '\r' (CR LF).
            let line = line.trim_end_matches('\r').to_string();
            buf.push_line(line);
            partial.clear();
        } else {
            partial.push(b);
            if partial.len() >= MAX_PARTIAL {
                let line = String::from_utf8_lossy(partial).into_owned();
                buf.push_line(line);
                partial.clear();
            }
        }
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // ── AnsiStripper ─────────────────────────────────────────────────────────

    fn strip(s: &str) -> String {
        AnsiStripper::strip_str(s)
    }

    #[test]
    fn ansi_stripper_removes_sgr_sequences() {
        assert_eq!(strip("\x1b[31mhello\x1b[0m"), "hello");
        assert_eq!(strip("\x1b[1;32mbold green\x1b[0m world"), "bold green world");
    }

    #[test]
    fn ansi_stripper_passes_plain_text_unchanged() {
        assert_eq!(strip("hello world"), "hello world");
        assert_eq!(strip("no escape sequences here"), "no escape sequences here");
    }

    #[test]
    fn ansi_stripper_handles_truncated_escape_at_chunk_boundary() {
        // Sequence split across two calls.
        let mut st = AnsiStripper::default();
        let mut out = Vec::new();
        // First chunk ends mid-CSI.
        st.strip_into(b"before\x1b[31m", &mut out);
        // Second chunk contains the final byte and more text.
        st.strip_into(b"after", &mut out);
        assert_eq!(String::from_utf8(out).unwrap(), "beforeafter");
    }

    #[test]
    fn ansi_stripper_handles_osc_terminated_by_bel() {
        // OSC 0 ; title BEL — used for window title setting.
        assert_eq!(strip("\x1b]0;My Title\x07visible"), "visible");
    }

    #[test]
    fn ansi_stripper_handles_osc_terminated_by_st() {
        // OSC terminated by ESC \.
        assert_eq!(strip("\x1b]0;title\x1b\\after"), "after");
    }

    #[test]
    fn ansi_stripper_handles_two_byte_escape_sequences() {
        // ESC M (reverse index) — a two-byte sequence; just drops it.
        assert_eq!(strip("a\x1bMb"), "ab");
    }

    #[test]
    fn ansi_stripper_csi_split_across_calls() {
        let mut st = AnsiStripper::default();
        let mut out = Vec::new();
        // ESC [ arrives, then parameter bytes, then final byte in separate call.
        st.strip_into(b"\x1b[1;", &mut out);
        st.strip_into(b"32mtext\x1b[0m", &mut out);
        assert_eq!(String::from_utf8(out).unwrap(), "text");
    }

    // ── HistoryBuffer ─────────────────────────────────────────────────────────

    fn make_buf(cap: usize) -> HistoryBuffer {
        HistoryBuffer::new(cap)
    }

    #[test]
    fn push_and_iterate_preserves_order() {
        let mut buf = make_buf(10);
        buf.push_line("line 1".into());
        buf.push_line("line 2".into());
        buf.push_line("line 3".into());
        let lines: Vec<_> = buf.iter().collect();
        assert_eq!(lines, ["line 1", "line 2", "line 3"]);
    }

    #[test]
    fn ring_evicts_oldest_at_capacity() {
        let mut buf = make_buf(3);
        for i in 0..5 {
            buf.push_line(format!("line {i}"));
        }
        assert_eq!(buf.len(), 3);
        let lines: Vec<_> = buf.iter().collect();
        assert_eq!(lines, ["line 2", "line 3", "line 4"]);
    }

    #[test]
    fn total_pushed_is_monotonic_regardless_of_eviction() {
        let mut buf = make_buf(3);
        for i in 0..10 {
            buf.push_line(format!("line {i}"));
        }
        assert_eq!(buf.total_pushed(), 10);
        assert_eq!(buf.len(), 3);
    }

    #[test]
    fn empty_buffer_reports_zero_len() {
        let buf = make_buf(100);
        assert_eq!(buf.len(), 0);
        assert!(buf.is_empty());
        assert_eq!(buf.total_pushed(), 0);
        assert!(buf.iter().next().is_none());
    }

    #[test]
    fn capacity_one_always_has_at_most_one_line() {
        let mut buf = make_buf(1);
        buf.push_line("a".into());
        buf.push_line("b".into());
        assert_eq!(buf.len(), 1);
        assert_eq!(buf.get(0), Some("b"));
    }

    #[test]
    fn get_returns_none_out_of_range() {
        let mut buf = make_buf(10);
        buf.push_line("only".into());
        assert_eq!(buf.get(0), Some("only"));
        assert_eq!(buf.get(1), None);
    }

    // ── feed_bytes_to_history ─────────────────────────────────────────────────

    fn feed_str(
        s: &str,
        buf: &mut HistoryBuffer,
        partial: &mut Vec<u8>,
        st: &mut AnsiStripper,
    ) {
        feed_bytes_to_history(s.as_bytes(), buf, partial, st);
    }

    #[test]
    fn partial_line_held_until_newline() {
        let mut buf = make_buf(10);
        let mut partial = Vec::new();
        let mut st = AnsiStripper::default();
        feed_str("hello", &mut buf, &mut partial, &mut st);
        assert_eq!(buf.len(), 0, "no newline → nothing flushed yet");
        feed_str(" world\n", &mut buf, &mut partial, &mut st);
        assert_eq!(buf.len(), 1);
        assert_eq!(buf.get(0), Some("hello world"));
    }

    #[test]
    fn partial_line_force_flushed_at_4096() {
        let mut buf = make_buf(10);
        let mut partial = Vec::new();
        let mut st = AnsiStripper::default();
        // Feed a line of exactly 4096 bytes with no newline.
        let big = "x".repeat(4096);
        feed_str(&big, &mut buf, &mut partial, &mut st);
        assert_eq!(buf.len(), 1, "force-flushed at 4096");
        assert!(partial.is_empty(), "partial cleared after force-flush");
    }

    #[test]
    fn crlf_line_endings_stripped() {
        let mut buf = make_buf(10);
        let mut partial = Vec::new();
        let mut st = AnsiStripper::default();
        feed_str("hello\r\n", &mut buf, &mut partial, &mut st);
        assert_eq!(buf.get(0), Some("hello"), "\\r stripped");
    }

    #[test]
    fn multiple_lines_in_one_chunk() {
        let mut buf = make_buf(10);
        let mut partial = Vec::new();
        let mut st = AnsiStripper::default();
        feed_str("a\nb\nc\n", &mut buf, &mut partial, &mut st);
        assert_eq!(buf.len(), 3);
        assert_eq!(buf.get(0), Some("a"));
        assert_eq!(buf.get(1), Some("b"));
        assert_eq!(buf.get(2), Some("c"));
    }

    #[test]
    fn ansi_stripped_before_storage() {
        let mut buf = make_buf(10);
        let mut partial = Vec::new();
        let mut st = AnsiStripper::default();
        feed_str("\x1b[31mred text\x1b[0m\n", &mut buf, &mut partial, &mut st);
        assert_eq!(buf.get(0), Some("red text"));
    }

    #[test]
    fn ansi_escape_spanning_chunk_boundary_in_feed() {
        let mut buf = make_buf(10);
        let mut partial = Vec::new();
        let mut st = AnsiStripper::default();
        // First chunk ends mid-escape, second chunk finishes it + newline.
        feed_bytes_to_history(b"vis\x1b[31m", &mut buf, &mut partial, &mut st);
        feed_bytes_to_history(b"ible\x1b[0m\n", &mut buf, &mut partial, &mut st);
        assert_eq!(buf.get(0), Some("visible"));
    }
}
