//! A substrate-free [asciicast v2] encoder.
//!
//! asciicast v2 is a newline-delimited stream: a single JSON header object,
//! then one JSON array per event — `[time, code, data]` where `code` is `"o"`
//! (output), `"i"` (input), `"r"` (resize) or `"m"` (marker). Two producers in
//! thegn share this encoder: the daemon's per-session recorder (teeing PTY
//! output live) and the time-travel replay export (rebasing a retained ring to
//! zero). Keeping it here — pure, clock-free, `std::io::Write`-generic — means
//! both are unit-tested against the same wire format and neither reimplements
//! JSON escaping.
//!
//! The encoder holds no clock: every event carries a caller-supplied
//! `time_secs` (seconds since the recording's `timestamp`), so the daemon can
//! pass elapsed wall-clock and the exporter can pass rebased ring times without
//! this module depending on a time source.
//!
//! [asciicast v2]: https://docs.asciinema.org/manual/asciicast/v2/

use std::io::{self, Write};

use serde::Serialize;

/// The header object written once at the top of a cast file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Header {
    /// Initial terminal width in columns.
    pub width: u16,
    /// Initial terminal height in rows.
    pub height: u16,
    /// Unix timestamp (seconds) of the recording start, if known.
    pub timestamp: Option<i64>,
    /// Optional human title for the recording.
    pub title: Option<String>,
    /// `env.TERM`, if recorded.
    pub term: Option<String>,
    /// `env.SHELL`, if recorded.
    pub shell: Option<String>,
}

impl Header {
    /// A header with just the required geometry; everything else absent.
    pub fn new(width: u16, height: u16) -> Self {
        Header {
            width,
            height,
            timestamp: None,
            title: None,
            term: None,
            shell: None,
        }
    }

    /// Serialize the header to its one-line JSON form (no trailing newline).
    pub fn to_json(&self) -> String {
        // Build with serde_json so field escaping is never hand-rolled. The
        // shape matches asciinema's own writer: version 2, width, height, then
        // optional timestamp/title/env.
        let mut obj = serde_json::Map::new();
        obj.insert("version".into(), serde_json::json!(2));
        obj.insert("width".into(), serde_json::json!(self.width));
        obj.insert("height".into(), serde_json::json!(self.height));
        if let Some(ts) = self.timestamp {
            obj.insert("timestamp".into(), serde_json::json!(ts));
        }
        if let Some(title) = &self.title {
            obj.insert("title".into(), serde_json::json!(title));
        }
        let mut env = serde_json::Map::new();
        if let Some(term) = &self.term {
            env.insert("TERM".into(), serde_json::json!(term));
        }
        if let Some(shell) = &self.shell {
            env.insert("SHELL".into(), serde_json::json!(shell));
        }
        if !env.is_empty() {
            obj.insert("env".into(), serde_json::Value::Object(env));
        }
        serde_json::Value::Object(obj).to_string()
    }
}

/// One asciicast event row, `[time_secs, code, data]`. `code` is a short static
/// string so serialization never allocates it.
#[derive(Serialize)]
struct Event<'a>(f64, &'a str, &'a str);

/// Encode a single event row to its one-line JSON form (no trailing newline).
/// Exposed for callers that manage their own sink; [`CastWriter`] wraps this.
pub fn event_line(time_secs: f64, code: &str, data: &str) -> String {
    // A tuple serializes as a JSON array with correct string escaping for
    // `data` (quotes, backslashes, control chars, non-ASCII).
    serde_json::to_string(&Event(time_secs.max(0.0), code, data))
        .unwrap_or_else(|_| String::from("[0,\"o\",\"\"]"))
}

/// Accumulates raw terminal bytes and yields the maximal *decodable* UTF-8
/// prefix, holding back an incomplete trailing multi-byte sequence for the next
/// chunk. PTY output arrives in arbitrary-sized reads that can split a glyph, so
/// both the daemon recorder and the replay export feed bytes through this before
/// emitting asciicast `"o"` data — otherwise a split glyph decodes to U+FFFD.
/// Invalid (never-completable) bytes still become U+FFFD via lossy decoding; the
/// retained tail is bounded to ≤3 bytes.
#[derive(Default)]
pub struct Utf8Carry {
    buf: Vec<u8>,
}

impl Utf8Carry {
    /// Append `bytes` and return the decodable prefix (may be empty when the
    /// whole chunk is an incomplete tail).
    pub fn push(&mut self, bytes: &[u8]) -> String {
        self.buf.extend_from_slice(bytes);
        let keep = incomplete_tail_len(&self.buf);
        let split = self.buf.len() - keep;
        let head: Vec<u8> = self.buf.drain(..split).collect();
        if head.is_empty() {
            String::new()
        } else {
            String::from_utf8_lossy(&head).into_owned()
        }
    }

    /// Return any remaining held bytes (lossily decoded) and clear the buffer —
    /// call once at the end so a trailing incomplete sequence is not dropped.
    pub fn flush(&mut self) -> String {
        if self.buf.is_empty() {
            String::new()
        } else {
            let s = String::from_utf8_lossy(&self.buf).into_owned();
            self.buf.clear();
            s
        }
    }

    /// Whether anything is currently held back.
    pub fn is_empty(&self) -> bool {
        self.buf.is_empty()
    }
}

/// How many trailing bytes of `buf` form the start of a multi-byte UTF-8
/// sequence that isn't complete yet (so they must wait for the next chunk).
/// Returns 0 when the buffer ends on a char boundary (or on garbage that will
/// never complete — that is left for lossy decoding).
fn incomplete_tail_len(buf: &[u8]) -> usize {
    let n = buf.len();
    let max_back = 3.min(n);
    for back in 1..=max_back {
        let b = buf[n - back];
        if b < 0x80 {
            return 0; // ASCII byte: everything up to here is complete.
        }
        if b >= 0xC0 {
            // A lead byte `back` bytes from the end; it needs `expected` bytes.
            let expected = if b >= 0xF0 {
                4
            } else if b >= 0xE0 {
                3
            } else {
                2
            };
            return if back < expected { back } else { 0 };
        }
        // 0x80..=0xBF: a continuation byte — keep scanning back for its lead.
    }
    0
}

/// A streaming asciicast v2 writer over any `std::io::Write` sink. It tracks the
/// number of bytes emitted so a caller can enforce a size cap, and finalizes to
/// a valid file at any point (every line written is already complete).
pub struct CastWriter<W: Write> {
    inner: W,
    bytes_written: u64,
}

impl<W: Write> CastWriter<W> {
    /// Create a writer, emitting the header line immediately.
    pub fn new(mut inner: W, header: &Header) -> io::Result<Self> {
        let line = header.to_json();
        writeln!(inner, "{line}")?;
        Ok(CastWriter {
            inner,
            bytes_written: line.len() as u64 + 1,
        })
    }

    fn write_line(&mut self, line: &str) -> io::Result<()> {
        writeln!(self.inner, "{line}")?;
        self.bytes_written += line.len() as u64 + 1;
        Ok(())
    }

    /// Emit an output event (`"o"`). `data` is terminal output as text; the
    /// caller is responsible for decoding raw PTY bytes to a `&str` (lossy is
    /// fine — invalid sequences become U+FFFD, which players tolerate).
    pub fn output(&mut self, time_secs: f64, data: &str) -> io::Result<()> {
        if data.is_empty() {
            return Ok(());
        }
        self.write_line(&event_line(time_secs, "o", data))
    }

    /// Emit a resize event (`"r"`), data `"{cols}x{rows}"`.
    pub fn resize(&mut self, time_secs: f64, cols: u16, rows: u16) -> io::Result<()> {
        self.write_line(&event_line(time_secs, "r", &format!("{cols}x{rows}")))
    }

    /// Emit a marker event (`"m"`) with an optional label.
    pub fn marker(&mut self, time_secs: f64, label: &str) -> io::Result<()> {
        self.write_line(&event_line(time_secs, "m", label))
    }

    /// Total bytes written so far (header + every event line, newlines
    /// included) — the number a size cap compares against.
    pub fn bytes_written(&self) -> u64 {
        self.bytes_written
    }

    /// Flush and recover the underlying sink.
    pub fn finish(mut self) -> io::Result<W> {
        self.inner.flush()?;
        Ok(self.inner)
    }

    /// Flush without consuming the writer.
    pub fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse_line(line: &str) -> serde_json::Value {
        serde_json::from_str(line).expect("valid JSON line")
    }

    #[test]
    fn header_has_required_fields_and_omits_absent_ones() {
        let h = Header::new(80, 24);
        let v = parse_line(&h.to_json());
        assert_eq!(v["version"], 2);
        assert_eq!(v["width"], 80);
        assert_eq!(v["height"], 24);
        assert!(v.get("timestamp").is_none());
        assert!(v.get("title").is_none());
        assert!(v.get("env").is_none());
    }

    #[test]
    fn header_includes_optional_fields_when_set() {
        let h = Header {
            width: 100,
            height: 40,
            timestamp: Some(1_700_000_000),
            title: Some("demo".into()),
            term: Some("xterm-256color".into()),
            shell: Some("/bin/zsh".into()),
        };
        let v = parse_line(&h.to_json());
        assert_eq!(v["timestamp"], 1_700_000_000_i64);
        assert_eq!(v["title"], "demo");
        assert_eq!(v["env"]["TERM"], "xterm-256color");
        assert_eq!(v["env"]["SHELL"], "/bin/zsh");
    }

    #[test]
    fn env_object_appears_when_only_one_var_is_set() {
        let h = Header {
            term: Some("thegn".into()),
            ..Header::new(80, 24)
        };
        let v = parse_line(&h.to_json());
        assert_eq!(v["env"]["TERM"], "thegn");
        assert!(v["env"].get("SHELL").is_none());
    }

    #[test]
    fn event_line_escapes_and_round_trips() {
        // Quotes, backslashes, newlines and a non-ASCII glyph survive as data.
        let payload = "he said \"hi\"\n\t\\end café";
        let line = event_line(1.5, "o", payload);
        let v = parse_line(&line);
        assert_eq!(v[0], 1.5);
        assert_eq!(v[1], "o");
        assert_eq!(v[2], payload);
    }

    #[test]
    fn negative_time_clamps_to_zero() {
        let v = parse_line(&event_line(-3.0, "o", "x"));
        assert_eq!(v[0], 0.0);
    }

    #[test]
    fn writer_emits_header_then_events_and_counts_bytes() {
        let mut buf: Vec<u8> = Vec::new();
        {
            let mut w = CastWriter::new(&mut buf, &Header::new(80, 24)).unwrap();
            w.output(0.0, "hello").unwrap();
            w.output(0.25, "").unwrap(); // empty output is skipped
            w.resize(0.5, 120, 40).unwrap();
            w.marker(0.75, "checkpoint").unwrap();
            assert!(w.bytes_written() > 0);
            w.flush().unwrap();
        }
        let text = String::from_utf8(buf).unwrap();
        let mut lines = text.lines();
        // Header first.
        assert_eq!(parse_line(lines.next().unwrap())["version"], 2);
        // Output event.
        let out = parse_line(lines.next().unwrap());
        assert_eq!(
            (out[1].as_str().unwrap(), out[2].as_str().unwrap()),
            ("o", "hello")
        );
        // Resize event (empty output was skipped, so this is next).
        let rz = parse_line(lines.next().unwrap());
        assert_eq!(
            (rz[1].as_str().unwrap(), rz[2].as_str().unwrap()),
            ("r", "120x40")
        );
        // Marker event.
        let mk = parse_line(lines.next().unwrap());
        assert_eq!(
            (mk[1].as_str().unwrap(), mk[2].as_str().unwrap()),
            ("m", "checkpoint")
        );
        assert!(lines.next().is_none());
    }

    #[test]
    fn bytes_written_tracks_the_sink_length() {
        let mut buf: Vec<u8> = Vec::new();
        let w = CastWriter::new(&mut buf, &Header::new(80, 24)).unwrap();
        let counted = w.bytes_written();
        assert_eq!(counted, buf.len() as u64);
    }

    #[test]
    fn finish_recovers_the_sink() {
        let buf: Vec<u8> = Vec::new();
        let mut w = CastWriter::new(buf, &Header::new(80, 24)).unwrap();
        w.output(0.1, "x").unwrap();
        let recovered = w.finish().unwrap();
        assert!(!recovered.is_empty());
    }

    #[test]
    fn incomplete_tail_len_finds_split_sequences_only() {
        // "é" is 0xC3 0xA9: a buffer ending on the lead carries 1.
        assert_eq!(incomplete_tail_len(&[b'a', 0xC3]), 1);
        // Complete 2-byte char: nothing carried.
        assert_eq!(incomplete_tail_len(&[b'a', 0xC3, 0xA9]), 0);
        // 3-byte lead + one continuation, missing one: carry 2.
        assert_eq!(incomplete_tail_len(&[0xE2, 0x82]), 2);
        // 4-byte lead alone: carry 1.
        assert_eq!(incomplete_tail_len(&[0xF0]), 1);
        // Pure ASCII / empty: nothing carried.
        assert_eq!(incomplete_tail_len(b"hello"), 0);
        assert_eq!(incomplete_tail_len(&[]), 0);
        // Three continuation bytes with no lead in range: left to lossy decode.
        assert_eq!(incomplete_tail_len(&[0x80, 0x80, 0x80]), 0);
    }

    #[test]
    fn utf8_carry_reassembles_a_split_glyph() {
        let mut c = Utf8Carry::default();
        // "hi" + the first byte of "é".
        assert_eq!(c.push(&[b'h', b'i', 0xC3]), "hi");
        assert!(!c.is_empty());
        // The second byte completes it.
        assert_eq!(c.push(&[0xA9]), "é");
        assert!(c.is_empty());
        assert_eq!(c.flush(), "");
    }

    #[test]
    fn utf8_carry_flush_emits_leftover_lossily() {
        let mut c = Utf8Carry::default();
        assert_eq!(c.push(&[0xF0]), ""); // dangling 4-byte lead is held
        assert!(!c.is_empty());
        // flush turns the incomplete tail into a replacement char.
        assert_eq!(c.flush(), "\u{FFFD}");
        assert!(c.is_empty());
    }
}
