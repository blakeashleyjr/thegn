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
use std::sync::Arc;
use std::time::SystemTime;

// ── ANSI stripping ────────────────────────────────────────────────────────────

#[derive(Default, Clone, Copy, PartialEq, Eq)]
enum AnsiState {
    #[default]
    Normal,
    /// Saw ESC.
    Esc,
    /// Saw an ESC intermediate introducer; swallow one more byte.
    EscIntermediate,
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
    param: u32,
    private: bool,
    cr_pending: bool,
    rewind: usize,
    alt: bool,
}

#[derive(Clone, Copy)]
enum AnsiEvent {
    Byte(u8),
    Spaces(usize),
    CursorUp(usize),
    EraseLine,
    CarriageReturn,
    Newline,
    Alt(bool),
}

/// Length of `bytes` minus an incomplete trailing UTF-8 sequence (at most 3 bytes).
fn utf8_flush_cut(bytes: &[u8]) -> usize {
    let len = bytes.len();
    for back in 1..=len.min(3) {
        let b = bytes[len - back];
        if b & 0xC0 == 0x80 {
            continue;
        }
        // 0xF8..=0xFF can never start a sequence: nothing to wait for.
        let need = if b >= 0xF8 {
            1
        } else if b >= 0xF0 {
            4
        } else if b >= 0xE0 {
            3
        } else if b >= 0xC0 {
            2
        } else {
            1
        };
        return if need > back { len - back } else { len };
    }
    len
}

impl AnsiStripper {
    /// Strip ANSI escape sequences from `input`, appending visible bytes to
    /// `out`. Call repeatedly with successive chunks; state carries over.
    pub fn strip_into(&mut self, input: &[u8], out: &mut Vec<u8>) {
        for &b in input {
            if let Some(event) = self.consume_byte(b) {
                match event {
                    AnsiEvent::Byte(byte) => out.push(byte),
                    AnsiEvent::Spaces(count) => {
                        out.extend(std::iter::repeat_n(b' ', count));
                    }
                    AnsiEvent::Newline => out.push(b'\n'),
                    AnsiEvent::CarriageReturn => out.push(b'\r'),
                    _ => {}
                }
            }
        }
    }

    fn consume_byte(&mut self, b: u8) -> Option<AnsiEvent> {
        match self.state {
            AnsiState::Normal => {
                if b == 0x1b {
                    self.state = AnsiState::Esc;
                    None
                } else if b == b'\n' {
                    Some(AnsiEvent::Newline)
                } else if b == b'\r' {
                    Some(AnsiEvent::CarriageReturn)
                } else if b >= 0x20 || b == b'\t' {
                    Some(AnsiEvent::Byte(b))
                } else {
                    // Drop other C0 controls (e.g. BEL 0x07, BS 0x08, …).
                    None
                }
            }
            AnsiState::Esc => match b {
                b'[' => {
                    self.param = 0;
                    self.private = false;
                    self.state = AnsiState::Csi;
                    None
                }
                b'P' | b']' | b'^' | b'_' | b'X' => {
                    self.state = AnsiState::Other;
                    None
                }
                b'(' | b')' | b'*' | b'+' | b'-' | b'.' | b'/' | b'#' | b'%' => {
                    self.state = AnsiState::EscIntermediate;
                    None
                }
                byte if !(0x20..0x80).contains(&byte) => {
                    self.state = AnsiState::Normal;
                    self.consume_byte(byte)
                }
                _ => {
                    // Two-byte sequence (ESC + final) fully consumed.
                    self.state = AnsiState::Normal;
                    None
                }
            },
            AnsiState::EscIntermediate => {
                self.state = AnsiState::Normal;
                None
            }
            AnsiState::Csi => {
                if b.is_ascii_digit() {
                    self.param = self
                        .param
                        .saturating_mul(10)
                        .saturating_add(u32::from(b - b'0'))
                        .min(9999);
                } else if b == b'?' {
                    self.private = true;
                } else if (0x40..=0x7e).contains(&b) {
                    let param = self.param;
                    let private = self.private;
                    self.state = AnsiState::Normal;
                    self.param = 0;
                    self.private = false;
                    return match b {
                        b'C' => Some(AnsiEvent::Spaces(param.clamp(1, 256) as usize)),
                        b'A' => Some(AnsiEvent::CursorUp(param.max(1) as usize)),
                        b'K' if param == 2 => Some(AnsiEvent::EraseLine),
                        b'h' if private && matches!(param, 47 | 1047 | 1049) => {
                            Some(AnsiEvent::Alt(true))
                        }
                        b'l' if private && matches!(param, 47 | 1047 | 1049) => {
                            Some(AnsiEvent::Alt(false))
                        }
                        _ => None,
                    };
                }
                None
            }
            AnsiState::Other => {
                // OSC/DCS: terminated by ST (ESC \) or BEL (0x07).
                if b == 0x07 {
                    self.state = AnsiState::Normal;
                } else if b == 0x1b {
                    // Could be the ESC of ST (ESC \) — transition to Esc.
                    self.state = AnsiState::Esc;
                }
                None
            }
        }
    }

    /// Strip input while applying terminal line-editing events to history.
    /// Every line this chunk completes is stamped with `captured_at`.
    fn feed_lines(
        &mut self,
        input: &[u8],
        partial: &mut Vec<u8>,
        buf: &mut HistoryBuffer,
        captured_at: SystemTime,
    ) {
        for &b in input {
            let Some(event) = self.consume_byte(b) else {
                continue;
            };

            match event {
                AnsiEvent::Alt(alt) => {
                    self.alt = alt;
                    partial.clear();
                    self.cr_pending = false;
                }
                _ if self.alt => {}
                AnsiEvent::Byte(byte) => self.append_byte(byte, partial, buf, captured_at),
                AnsiEvent::Spaces(count) => {
                    for _ in 0..count {
                        self.append_byte(b' ', partial, buf, captured_at);
                    }
                }
                AnsiEvent::CarriageReturn => {
                    self.cr_pending = true;
                }
                AnsiEvent::Newline => {
                    self.cr_pending = false;
                    self.finish_line(partial, buf, captured_at);
                }
                AnsiEvent::CursorUp(count) => {
                    self.rewind = self.rewind.saturating_add(count).min(buf.len()).min(64);
                    partial.clear();
                    self.cr_pending = false;
                }
                AnsiEvent::EraseLine => {
                    partial.clear();
                    self.cr_pending = false;
                }
            }
        }
    }

    fn append_byte(
        &mut self,
        byte: u8,
        partial: &mut Vec<u8>,
        buf: &mut HistoryBuffer,
        captured_at: SystemTime,
    ) {
        const MAX_PARTIAL: usize = 4096;

        if self.cr_pending {
            partial.clear();
            self.cr_pending = false;
        }
        partial.push(byte);
        if partial.len() >= MAX_PARTIAL {
            self.flush_partial(partial, buf, captured_at);
        }
    }

    fn finish_line(
        &mut self,
        partial: &mut Vec<u8>,
        buf: &mut HistoryBuffer,
        captured_at: SystemTime,
    ) {
        let text = String::from_utf8_lossy(partial);
        let text = text.strip_suffix('\r').unwrap_or(&text);
        let line: Arc<str> = Arc::from(text);
        self.store_line(line, buf, captured_at);
        partial.clear();
    }

    fn flush_partial(
        &mut self,
        partial: &mut Vec<u8>,
        buf: &mut HistoryBuffer,
        captured_at: SystemTime,
    ) {
        let cut = utf8_flush_cut(partial);
        if cut == 0 {
            return;
        }

        let text = String::from_utf8_lossy(&partial[..cut]);
        let text = text.strip_suffix('\r').unwrap_or(&text);
        let line: Arc<str> = Arc::from(text);
        self.store_line(line, buf, captured_at);
        partial.drain(..cut);
    }

    fn store_line(&mut self, line: Arc<str>, buf: &mut HistoryBuffer, captured_at: SystemTime) {
        if self.rewind > 0 && self.rewind <= buf.len() {
            buf.replace_from_end_at(self.rewind, line, captured_at);
            self.rewind -= 1;
        } else {
            self.rewind = 0;
            buf.push_line_at(line, captured_at);
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
    /// `Arc<str>` so cloning a buffer (the search snapshot) bumps one refcount
    /// per line instead of copying every line's bytes.
    lines: VecDeque<Arc<str>>,
    /// Capture time of each stored line, parallel to `lines`. Only the search
    /// overlay reads it (relative age in the result rows); nothing derives a
    /// time from line order.
    line_times: VecDeque<SystemTime>,
    capacity: usize,
    /// Monotonically increasing count of all lines ever pushed (even after eviction).
    total: u64,
}

impl HistoryBuffer {
    pub fn new(capacity: usize) -> Self {
        let cap = capacity.max(1);
        Self {
            lines: VecDeque::with_capacity(cap.min(4096)),
            line_times: VecDeque::with_capacity(cap.min(4096)),
            capacity: cap,
            total: 0,
        }
    }

    /// Push one already-stripped line, stamped with the wall clock. Empty lines
    /// are stored as-is so that blank lines in output are searchable and the
    /// line indices stay in sync with the vt100 scrollback.
    pub fn push_line(&mut self, line: impl Into<Arc<str>>) {
        self.push_line_at(line, SystemTime::now());
    }

    /// [`Self::push_line`] with an explicit capture time, for a caller that
    /// owns a pinned clock (the host's `THEGN_E2E` freeze) or a test.
    pub fn push_line_at(&mut self, line: impl Into<Arc<str>>, captured_at: SystemTime) {
        if self.lines.len() >= self.capacity {
            self.lines.pop_front();
            self.line_times.pop_front();
        }
        self.lines.push_back(line.into());
        self.line_times.push_back(captured_at);
        self.total += 1;
    }

    /// Replace the `k`-th stored line from the end (`k = 1` is newest).
    /// Returns `false` when `k` is out of range. A replacement counts as one
    /// line write in `total_pushed`, so new-line consumers rescan it.
    pub fn replace_from_end(&mut self, k: usize, line: impl Into<Arc<str>>) -> bool {
        self.replace_from_end_at(k, line, SystemTime::now())
    }

    /// [`Self::replace_from_end`] with an explicit capture time. A spinner
    /// redraw is a new capture of that line, so its age restarts.
    pub fn replace_from_end_at(
        &mut self,
        k: usize,
        line: impl Into<Arc<str>>,
        captured_at: SystemTime,
    ) -> bool {
        if k == 0 || k > self.lines.len() {
            return false;
        }
        let index = self.lines.len() - k;
        self.lines[index] = line.into();
        self.line_times[index] = captured_at;
        self.total += 1;
        true
    }

    /// Iterate lines oldest-first.
    pub fn iter(&self) -> impl Iterator<Item = &str> {
        self.lines.iter().map(|line| &**line)
    }

    /// Lines currently stored (≤ capacity).
    pub fn len(&self) -> usize {
        self.lines.len()
    }

    pub fn is_empty(&self) -> bool {
        self.lines.is_empty()
    }

    /// Monotonic count of completed line writes (pushes and replacements);
    /// survives ring eviction.
    pub fn total_pushed(&self) -> u64 {
        self.total
    }

    /// The `idx`-th stored line (0 = oldest surviving). Returns `None` if out
    /// of range.
    pub fn get(&self, idx: usize) -> Option<&str> {
        self.lines.get(idx).map(|line| &**line)
    }

    /// Capture time of the `idx`-th stored line (0 = oldest surviving), stamped
    /// when that line was written. Returns `None` if out of range.
    pub fn captured_at(&self, idx: usize) -> Option<SystemTime> {
        self.line_times.get(idx).copied()
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
    feed_bytes_to_history_at(bytes, buf, partial, stripper, SystemTime::now());
}

/// [`feed_bytes_to_history`] with an explicit capture time for every line this
/// chunk completes. The host passes its freezable clock so `THEGN_E2E` frames
/// carry pinned line timestamps.
pub fn feed_bytes_to_history_at(
    bytes: &[u8],
    buf: &mut HistoryBuffer,
    partial: &mut Vec<u8>,
    stripper: &mut AnsiStripper,
    captured_at: SystemTime,
) {
    stripper.feed_lines(bytes, partial, buf, captured_at);
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
        assert_eq!(
            strip("\x1b[1;32mbold green\x1b[0m world"),
            "bold green world"
        );
    }

    #[test]
    fn ansi_stripper_passes_plain_text_unchanged() {
        assert_eq!(strip("hello world"), "hello world");
        assert_eq!(
            strip("no escape sequences here"),
            "no escape sequences here"
        );
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

    #[test]
    fn ansi_stripper_cursor_forward_emits_clamped_spaces() {
        assert_eq!(strip("hello\x1b[1Cworld"), "hello world");
        assert_eq!(strip("a\x1b[9999Cb"), format!("a{}b", " ".repeat(256)));
    }

    #[test]
    fn ansi_stripper_intermediate_escape_consumes_one_byte() {
        assert_eq!(strip("\x1b(Bplain\n"), "plain\n");
    }

    #[test]
    fn ansi_stripper_reprocesses_non_ascii_after_escape() {
        assert_eq!(strip("\x1bé\n"), "é\n");
    }

    // ── HistoryBuffer ─────────────────────────────────────────────────────────

    fn make_buf(cap: usize) -> HistoryBuffer {
        HistoryBuffer::new(cap)
    }

    #[test]
    fn push_and_iterate_preserves_order() {
        let mut buf = make_buf(10);
        buf.push_line("line 1");
        buf.push_line("line 2");
        buf.push_line("line 3");
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
        buf.push_line("a");
        buf.push_line("b");
        assert_eq!(buf.len(), 1);
        assert_eq!(buf.get(0), Some("b"));
    }

    #[test]
    fn get_returns_none_out_of_range() {
        let mut buf = make_buf(10);
        buf.push_line("only");
        assert_eq!(buf.get(0), Some("only"));
        assert_eq!(buf.get(1), None);
    }

    #[test]
    fn replace_from_end_replaces_and_counts_as_a_line_write() {
        let mut buf = make_buf(10);
        buf.push_line("first");
        buf.push_line("second");
        let total = buf.total_pushed();
        assert!(buf.replace_from_end(1, "new second"));
        assert_eq!(buf.get(1), Some("new second"));
        assert_eq!(buf.total_pushed(), total + 1);
        assert!(!buf.replace_from_end(0, "nope"));
        assert!(!buf.replace_from_end(3, "nope"));
        assert_eq!(buf.total_pushed(), total + 1);
    }

    #[test]
    fn capture_times_follow_eviction_and_replacement() {
        use std::time::Duration;

        let base = SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000);
        let mut buf = make_buf(2);
        buf.push_line_at("old", base);
        buf.push_line_at("kept", base + Duration::from_secs(1));
        buf.push_line_at("new", base + Duration::from_secs(2));

        // The evicted line took its stamp with it: index 0 is `kept`.
        assert_eq!(buf.get(0), Some("kept"));
        assert_eq!(buf.captured_at(0), Some(base + Duration::from_secs(1)));
        assert_eq!(buf.captured_at(1), Some(base + Duration::from_secs(2)));
        assert_eq!(buf.captured_at(2), None);

        // A replacement is a new capture of that line.
        let rewritten = base + Duration::from_secs(30);
        assert!(buf.replace_from_end_at(1, "rewritten", rewritten));
        assert_eq!(buf.captured_at(1), Some(rewritten));
        assert_eq!(buf.captured_at(0), Some(base + Duration::from_secs(1)));
    }

    #[test]
    fn feed_stamps_every_completed_line_with_the_given_time() {
        use std::time::Duration;

        let base = SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000);
        let mut buf = make_buf(10);
        let mut partial = Vec::new();
        let mut st = AnsiStripper::default();
        feed_bytes_to_history_at(b"one\ntwo\n", &mut buf, &mut partial, &mut st, base);
        assert_eq!(buf.get(0), Some("one"));
        assert_eq!(buf.captured_at(0), Some(base));
        assert_eq!(buf.captured_at(1), Some(base));

        // A later chunk carries its own time.
        let later = base + Duration::from_secs(5);
        feed_bytes_to_history_at(b"three\n", &mut buf, &mut partial, &mut st, later);
        assert_eq!(buf.captured_at(2), Some(later));
    }

    // ── feed_bytes_to_history ─────────────────────────────────────────────────

    fn feed_str(s: &str, buf: &mut HistoryBuffer, partial: &mut Vec<u8>, st: &mut AnsiStripper) {
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
    fn non_utf8_output_still_flushes_in_4096_byte_lines() {
        let mut buf = make_buf(10);
        let mut partial = Vec::new();
        let mut st = AnsiStripper::default();
        feed_bytes_to_history(&[0xffu8; 5000], &mut buf, &mut partial, &mut st);
        assert_eq!(buf.len(), 1);
        assert_eq!(partial.len(), 5000 - 4096);
    }

    #[test]
    fn partial_line_force_flush_preserves_utf8_boundaries() {
        let mut buf = make_buf(10);
        let mut partial = Vec::new();
        let mut st = AnsiStripper::default();
        feed_str(&"é".repeat(5000), &mut buf, &mut partial, &mut st);
        assert!(buf.iter().all(|line| !line.contains('\u{fffd}')));
        assert!(String::from_utf8(partial).is_ok());
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

    #[test]
    fn carriage_return_overwrites_previous_partial_line() {
        let mut buf = make_buf(10);
        let mut partial = Vec::new();
        let mut st = AnsiStripper::default();
        feed_str(
            "\rthinking 1\rthinking 50\n",
            &mut buf,
            &mut partial,
            &mut st,
        );
        assert_eq!(buf.iter().collect::<Vec<_>>(), ["thinking 50"]);
    }

    #[test]
    fn ink_style_redraw_replaces_lines_from_the_end() {
        let mut buf = make_buf(10);
        let mut partial = Vec::new();
        let mut st = AnsiStripper::default();
        feed_str(
            "a\nb\n\x1b[2A\x1b[Ja2\nb2\n",
            &mut buf,
            &mut partial,
            &mut st,
        );
        assert_eq!(buf.iter().collect::<Vec<_>>(), ["a2", "b2"]);
    }

    #[test]
    fn erase_line_clears_partial_output() {
        let mut buf = make_buf(10);
        let mut partial = Vec::new();
        let mut st = AnsiStripper::default();
        feed_str("stale\x1b[2Kfresh\n", &mut buf, &mut partial, &mut st);
        assert_eq!(buf.get(0), Some("fresh"));
    }

    #[test]
    fn alternate_screen_output_is_not_recorded() {
        let mut buf = make_buf(10);
        let mut partial = Vec::new();
        let mut st = AnsiStripper::default();
        feed_str(
            "\x1b[?1049halt-screen line\nsecond line\x1b[?1049l",
            &mut buf,
            &mut partial,
            &mut st,
        );
        assert!(buf.is_empty());
        assert!(partial.is_empty());
    }
}
