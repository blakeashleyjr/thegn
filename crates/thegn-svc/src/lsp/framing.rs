//! Language Server Protocol base-protocol framing.
//!
//! LSP messages are JSON-RPC bodies prefixed with a `Content-Length` header and
//! a blank line — `Content-Length: 42\r\n\r\n{json}`. This module is the pure,
//! I/O-free codec: [`encode`] frames a body, and [`FrameDecoder`] turns an
//! arbitrarily-chunked byte stream back into whole JSON bodies (handling partial
//! reads and several messages arriving in one buffer). All of it is unit-tested;
//! the actual stdio lives in [`super::LspClient`].

/// Reject any `Content-Length` above this — a well-formed LSP body is a JSON-RPC
/// message, never gigabytes. A larger value is corruption or a hostile stream
/// (ssh transports interleave foreign bytes: shell rc/banner output on stdout is
/// a known real-world occurrence), so we treat it as a malformed header and
/// resync rather than buffer the stream unboundedly.
const MAX_FRAME_LEN: usize = 64 * 1024 * 1024;

/// Frame a JSON body with the LSP `Content-Length` header.
pub fn encode(body: &str) -> Vec<u8> {
    let mut out = Vec::with_capacity(body.len() + 32);
    out.extend_from_slice(format!("Content-Length: {}\r\n\r\n", body.len()).as_bytes());
    out.extend_from_slice(body.as_bytes());
    out
}

/// Incremental decoder: feed it bytes, pull out complete JSON bodies.
#[derive(Debug, Default)]
pub struct FrameDecoder {
    buf: Vec<u8>,
}

impl FrameDecoder {
    pub fn new() -> Self {
        FrameDecoder { buf: Vec::new() }
    }

    /// Append freshly-read bytes to the internal buffer.
    pub fn push(&mut self, bytes: &[u8]) {
        self.buf.extend_from_slice(bytes);
    }

    /// Pop the next complete message body, or `None` if one isn't buffered yet.
    ///
    /// Headers other than `Content-Length` (e.g. `Content-Type`) are tolerated
    /// and ignored. A malformed header block (no parseable `Content-Length`) is
    /// dropped up to and including its separator so the stream can resync.
    pub fn next_message(&mut self) -> Option<String> {
        loop {
            // Find the header/body separator.
            let sep = find_subslice(&self.buf, b"\r\n\r\n")?;
            let header = &self.buf[..sep];
            let len = parse_content_length(header);
            let body_start = sep + 4;

            let Some(len) = len else {
                // Unparseable header block — discard it and resync.
                self.buf.drain(..body_start);
                continue;
            };

            // `len` is attacker/corruption-controlled: a near-`usize::MAX` value
            // would wrap `body_start + len` (a debug panic; in release a wrapped
            // slice end that panics on the range), and even a huge non-wrapping
            // value would buffer the stream unboundedly. Treat an overflow or an
            // over-cap length as a malformed header and resync past it.
            let body_end = body_start.checked_add(len);
            match body_end {
                Some(end) if len <= MAX_FRAME_LEN => {
                    if self.buf.len() < end {
                        return None; // body not fully arrived yet
                    }
                    let body: Vec<u8> = self.buf[body_start..end].to_vec();
                    self.buf.drain(..end);
                    // Bodies are UTF-8 JSON; a non-UTF-8 body is protocol-broken.
                    return Some(String::from_utf8_lossy(&body).into_owned());
                }
                _ => {
                    // Overflowing or over-cap length — drop this header and resync.
                    self.buf.drain(..body_start);
                    continue;
                }
            }
        }
    }
}

/// Locate the first occurrence of `needle` in `haystack`.
fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || haystack.len() < needle.len() {
        return None;
    }
    haystack.windows(needle.len()).position(|w| w == needle)
}

/// Parse the `Content-Length` value out of a header block (case-insensitive key).
fn parse_content_length(header: &[u8]) -> Option<usize> {
    let text = std::str::from_utf8(header).ok()?;
    for line in text.split("\r\n") {
        let (key, value) = line.split_once(':')?;
        if key.trim().eq_ignore_ascii_case("content-length") {
            return value.trim().parse::<usize>().ok();
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encode_prefixes_content_length() {
        let framed = encode("{\"a\":1}");
        assert_eq!(framed, b"Content-Length: 7\r\n\r\n{\"a\":1}");
    }

    #[test]
    fn decodes_a_single_whole_message() {
        let mut d = FrameDecoder::new();
        d.push(&encode("{\"id\":1}"));
        assert_eq!(d.next_message().as_deref(), Some("{\"id\":1}"));
        assert_eq!(d.next_message(), None);
    }

    #[test]
    fn decodes_multiple_messages_in_one_buffer() {
        let mut d = FrameDecoder::new();
        let mut bytes = encode("{\"id\":1}");
        bytes.extend(encode("{\"id\":2}"));
        d.push(&bytes);
        assert_eq!(d.next_message().as_deref(), Some("{\"id\":1}"));
        assert_eq!(d.next_message().as_deref(), Some("{\"id\":2}"));
        assert_eq!(d.next_message(), None);
    }

    #[test]
    fn reassembles_across_partial_reads() {
        let mut d = FrameDecoder::new();
        let framed = encode("{\"hello\":\"world\"}");
        // Feed it one byte at a time; only the final byte completes the message.
        for (i, b) in framed.iter().enumerate() {
            d.push(&[*b]);
            if i + 1 < framed.len() {
                assert_eq!(d.next_message(), None, "completed early at byte {i}");
            }
        }
        assert_eq!(d.next_message().as_deref(), Some("{\"hello\":\"world\"}"));
    }

    #[test]
    fn tolerates_extra_headers() {
        let mut d = FrameDecoder::new();
        d.push(b"Content-Type: application/vscode-jsonrpc; charset=utf-8\r\nContent-Length: 2\r\n\r\n{}");
        assert_eq!(d.next_message().as_deref(), Some("{}"));
    }

    #[test]
    fn resyncs_past_a_malformed_header_block() {
        let mut d = FrameDecoder::new();
        d.push(b"garbage-without-length\r\n\r\n");
        d.push(&encode("{\"ok\":true}"));
        assert_eq!(d.next_message().as_deref(), Some("{\"ok\":true}"));
    }

    #[test]
    fn unicode_body_byte_length_not_char_length() {
        let body = "{\"s\":\"café→\"}"; // multibyte chars
        let mut d = FrameDecoder::new();
        d.push(&encode(body));
        assert_eq!(d.next_message().as_deref(), Some(body));
    }

    #[test]
    fn overflowing_content_length_resyncs_instead_of_panicking() {
        // A near-usize::MAX Content-Length would wrap `body_start + len`
        // (debug panic; release: a wrapped slice range that panics). It must be
        // treated as a malformed header: dropped, then the next real message
        // decodes.
        let mut d = FrameDecoder::new();
        d.push(format!("Content-Length: {}\r\n\r\n", usize::MAX).as_bytes());
        // No panic, and no body yet (the hostile header was dropped).
        assert_eq!(d.next_message(), None);
        d.push(&encode("{\"ok\":true}"));
        assert_eq!(d.next_message().as_deref(), Some("{\"ok\":true}"));
    }

    #[test]
    fn over_cap_content_length_is_rejected_not_buffered() {
        // A huge-but-not-overflowing length must not make the decoder buffer the
        // stream unboundedly; it's dropped and the stream resyncs.
        let mut d = FrameDecoder::new();
        d.push(format!("Content-Length: {}\r\n\r\n", MAX_FRAME_LEN + 1).as_bytes());
        assert_eq!(d.next_message(), None);
        d.push(&encode("{\"ok\":true}"));
        assert_eq!(d.next_message().as_deref(), Some("{\"ok\":true}"));
    }

    #[test]
    fn at_cap_content_length_still_decodes() {
        // The cap itself is a valid (if large) length — a body exactly at the
        // cap boundary must still be accepted once it arrives.
        let body = "x".repeat(MAX_FRAME_LEN);
        let mut d = FrameDecoder::new();
        d.push(&encode(&body));
        assert_eq!(d.next_message().as_deref(), Some(body.as_str()));
    }
}
