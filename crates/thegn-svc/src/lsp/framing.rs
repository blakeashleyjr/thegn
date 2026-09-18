//! Shared LSP/bridge framing. Both transports use identical limits and fail
//! closed on malformed framing; neither searches attacker-controlled bodies for
//! a new header after losing synchronization.

use std::io::{self, Read};

/// Includes the CRLFCRLF delimiter.
pub const MAX_HEADER_BYTES: usize = 8192;
pub const MAX_FRAME_LEN: usize = 64 * 1024 * 1024;
pub const READ_BYTES: usize = 8192;
pub const MAX_BUFFER_BYTES: usize = MAX_FRAME_LEN + MAX_HEADER_BYTES + READ_BYTES;
pub const FRAMES_PER_TURN: usize = 64;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProtocolError {
    HeaderTooLong,
    BufferTooLong,
    MalformedHeader,
    BodyTooLong,
    InvalidUtf8,
    TruncatedFrame,
}

impl std::fmt::Display for ProtocolError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "invalid framed stream: {self:?}")
    }
}
impl std::error::Error for ProtocolError {}

pub fn encode(body: &str) -> Vec<u8> {
    let mut out = Vec::with_capacity(body.len() + 32);
    out.extend_from_slice(format!("Content-Length: {}\r\n\r\n", body.len()).as_bytes());
    out.extend_from_slice(body.as_bytes());
    out
}

/// Scan and consume offsets avoid prefix rescanning and repeated whole-buffer
/// shifts. Compaction only moves a suffix after at least as many bytes have been
/// consumed (or when required by the hard allocation bound).
#[derive(Debug)]
pub struct FrameDecoder {
    buf: Vec<u8>,
    head: usize,
    scan: usize,
    body: Option<(usize, usize)>,
    error: Option<ProtocolError>,
    max_frame_len: usize,
    #[cfg(test)]
    scanned: usize,
    #[cfg(test)]
    moved: usize,
}

impl Default for FrameDecoder {
    fn default() -> Self {
        Self {
            buf: Vec::new(),
            head: 0,
            scan: 0,
            body: None,
            error: None,
            max_frame_len: MAX_FRAME_LEN,
            #[cfg(test)]
            scanned: 0,
            #[cfg(test)]
            moved: 0,
        }
    }
}

impl FrameDecoder {
    pub fn new() -> Self {
        Self::with_limit(MAX_FRAME_LEN)
    }

    /// Construct a decoder with a caller-owned body limit.  The bridge keeps
    /// the historical 64 MiB limit; LSP admits only its 8 MiB budget before it
    /// starts buffering a body.  The limit is part of the decoder rather than
    /// a post-parse check so a hostile Content-Length cannot reserve the bridge
    /// allowance first.
    pub fn with_limit(max_frame_len: usize) -> Self {
        Self {
            max_frame_len: max_frame_len.min(MAX_FRAME_LEN),
            ..Self::default()
        }
    }

    fn fail<T>(&mut self, error: ProtocolError) -> Result<T, ProtocolError> {
        self.buf = Vec::new();
        self.head = 0;
        self.scan = 0;
        self.body = None;
        self.error = Some(error);
        Err(error)
    }

    fn compact(&mut self) {
        if self.head == 0 {
            return;
        }
        #[cfg(test)]
        {
            self.moved += self.buf.len() - self.head;
        }
        self.buf.copy_within(self.head.., 0);
        self.buf.truncate(self.buf.len() - self.head);
        self.scan -= self.head;
        if let Some((start, _)) = self.body.as_mut() {
            *start -= self.head;
        }
        self.head = 0;
    }

    /// Reject the complete append before allocation; an error is terminal.
    /// Drain complete messages before appending more reads. Near the allocation
    /// cap, undrained messages may cause BufferTooLong even when total unread
    /// bytes would fit, rather than trigger expensive tiny-prefix compaction.
    pub fn push(&mut self, bytes: &[u8]) -> Result<(), ProtocolError> {
        if let Some(error) = self.error {
            return Err(error);
        }
        let unread = self.buf.len() - self.head;
        let max_buffer_bytes = self.max_frame_len + MAX_HEADER_BYTES + READ_BYTES;
        if bytes.len() > max_buffer_bytes - unread {
            return self.fail(ProtocolError::BufferTooLong);
        }
        if bytes.len() > max_buffer_bytes - self.buf.len() {
            // A caller must drain queued messages before appending another
            // read. Moving a near-full suffix after each tiny pop would make
            // a sliding window quadratic. Only compact after enough consumed
            // bytes pay for every moved byte; otherwise fail before copying.
            if self.head < unread {
                return self.fail(ProtocolError::BufferTooLong);
            }
            self.compact();
        }
        let needed = self.buf.len() + bytes.len();
        if needed > self.buf.capacity() {
            let capacity = needed
                .max(self.buf.capacity().saturating_mul(2))
                .min(max_buffer_bytes);
            self.buf.reserve_exact(capacity - self.buf.len());
        }
        self.buf.extend_from_slice(bytes);
        Ok(())
    }

    pub fn next_message(&mut self) -> Result<Option<String>, ProtocolError> {
        if let Some(error) = self.error {
            return Err(error);
        }
        if self.body.is_none() {
            while self.scan + 4 <= self.buf.len() {
                #[cfg(test)]
                {
                    self.scanned += 1;
                }
                if self.scan + 4 - self.head > MAX_HEADER_BYTES {
                    return self.fail(ProtocolError::HeaderTooLong);
                }
                if &self.buf[self.scan..self.scan + 4] == b"\r\n\r\n" {
                    let len = match parse_content_length(
                        &self.buf[self.head..self.scan],
                        self.max_frame_len,
                    ) {
                        Ok(len) => len,
                        Err(error) => return self.fail(error),
                    };
                    self.body = Some((self.scan + 4, len));
                    break;
                }
                self.scan += 1;
            }
            if self.body.is_none() {
                if self.buf.len() - self.head >= MAX_HEADER_BYTES {
                    return self.fail(ProtocolError::HeaderTooLong);
                }
                return Ok(None);
            }
        }
        let (start, len) = self.body.expect("parsed above");
        if self.buf.len() - start < len {
            return Ok(None);
        }
        let body = match std::str::from_utf8(&self.buf[start..start + len]) {
            Ok(body) => body.to_owned(),
            Err(_) => return self.fail(ProtocolError::InvalidUtf8),
        };
        self.head = start + len;
        self.scan = self.head;
        self.body = None;
        if self.head == self.buf.len() {
            self.buf.clear();
            self.head = 0;
            self.scan = 0;
        } else if self.head >= READ_BYTES && self.head >= self.buf.len() / 2 {
            self.compact();
        }
        Ok(Some(body))
    }

    pub fn finish(&mut self) -> Result<(), ProtocolError> {
        if let Some(error) = self.error {
            return Err(error);
        }
        if self.buf.len() != self.head {
            return self.fail(ProtocolError::TruncatedFrame);
        }
        Ok(())
    }
}

fn parse_content_length(header: &[u8], max_frame_len: usize) -> Result<usize, ProtocolError> {
    let text = std::str::from_utf8(header).map_err(|_| ProtocolError::MalformedHeader)?;
    let mut length = None;
    for line in text.split("\r\n") {
        let (key, value) = line.split_once(':').ok_or(ProtocolError::MalformedHeader)?;
        if key.is_empty()
            || !key.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
            || value.bytes().any(|b| b.is_ascii_control() && b != b'\t')
        {
            return Err(ProtocolError::MalformedHeader);
        }
        if key.eq_ignore_ascii_case("content-length") {
            let value = value.trim_matches([' ', '\t']);
            if length.is_some() || value.is_empty() || !value.bytes().all(|b| b.is_ascii_digit()) {
                return Err(ProtocolError::MalformedHeader);
            }
            let len = value
                .parse::<usize>()
                .map_err(|_| ProtocolError::BodyTooLong)?;
            if len > max_frame_len {
                return Err(ProtocolError::BodyTooLong);
            }
            length = Some(len);
        }
    }
    length.ok_or(ProtocolError::MalformedHeader)
}

/// Blocking reader for the existing dedicated LSP/bridge reader threads. Never
/// use this on an async runtime worker. Each call dispatches at most one frame;
/// after 64 consecutive buffered frames the thread yields before continuing.
/// A yield never performs a read while complete messages remain buffered.
pub struct FramedReader<R> {
    reader: R,
    decoder: FrameDecoder,
    turn_frames: usize,
}

impl<R: Read> FramedReader<R> {
    pub fn new(reader: R) -> Self {
        Self::with_limit(reader, MAX_FRAME_LEN)
    }

    pub fn with_limit(reader: R, max_frame_len: usize) -> Self {
        Self {
            reader,
            decoder: FrameDecoder::with_limit(max_frame_len),
            turn_frames: 0,
        }
    }

    pub fn read_message(&mut self) -> io::Result<Option<String>> {
        if self.turn_frames == FRAMES_PER_TURN {
            std::thread::yield_now();
            self.turn_frames = 0;
        }
        loop {
            match self.decoder.next_message().map_err(io::Error::other)? {
                Some(body) => {
                    self.turn_frames += 1;
                    return Ok(Some(body));
                }
                None => {
                    let mut chunk = [0; READ_BYTES];
                    let n = match self.reader.read(&mut chunk) {
                        Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                        other => other?,
                    };
                    self.turn_frames = 0;
                    if n == 0 {
                        self.decoder.finish().map_err(io::Error::other)?;
                        return Ok(None);
                    }
                    self.decoder.push(&chunk[..n]).map_err(io::Error::other)?;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn pop(d: &mut FrameDecoder) -> Option<String> {
        d.next_message().unwrap()
    }

    #[test]
    fn encode_and_arbitrary_splits() {
        let body = "{\"s\":\"café→\"}";
        let bytes = encode(body);
        for split in 0..=bytes.len() {
            let mut d = FrameDecoder::new();
            d.push(&bytes[..split]).unwrap();
            let first = pop(&mut d);
            d.push(&bytes[split..]).unwrap();
            assert_eq!(first.or_else(|| pop(&mut d)).as_deref(), Some(body));
            assert_eq!(pop(&mut d), None);
            d.finish().unwrap();
        }
    }

    #[test]
    fn bytewise_header_has_linear_work_and_exact_limit() {
        let prefix = b"Content-Length: 0\r\nX-Pad: ";
        let mut bytes = prefix.to_vec();
        bytes.resize(MAX_HEADER_BYTES - 4, b'x');
        bytes.extend_from_slice(b"\r\n\r\n");
        let mut d = FrameDecoder::new();
        for (i, b) in bytes.iter().enumerate() {
            d.push(&[*b]).unwrap();
            assert_eq!(pop(&mut d), (i == bytes.len() - 1).then(String::new));
        }
        assert!(d.scanned <= bytes.len());
        assert_eq!(d.moved, 0);
    }

    #[test]
    fn unterminated_header_fails_at_cap_and_releases_allocation() {
        let mut d = FrameDecoder::new();
        for _ in 0..MAX_HEADER_BYTES - 1 {
            d.push(b"x").unwrap();
            assert_eq!(pop(&mut d), None);
        }
        d.push(b"x").unwrap();
        assert_eq!(d.next_message(), Err(ProtocolError::HeaderTooLong));
        assert!(d.scanned <= MAX_HEADER_BYTES);
        assert_eq!(d.buf.capacity(), 0);
        assert_eq!(d.push(&encode("{}")), Err(ProtocolError::HeaderTooLong));
        assert_eq!(d.finish(), Err(ProtocolError::HeaderTooLong));
    }

    #[test]
    fn malformed_lengths_and_utf8_are_sticky() {
        for header in [
            "",
            "Content-Length: -1",
            "Content-Length: +1",
            "Content-Length: 1\r\nContent-Length: 1",
            "X: yes",
            "bad",
            "Content-Length: 1\nOther: 1",
        ] {
            let mut d = FrameDecoder::new();
            d.push(format!("{header}\r\n\r\n").as_bytes()).unwrap();
            assert!(d.next_message().is_err(), "{header:?}");
            assert_eq!(d.buf.capacity(), 0);
        }
        let mut d = FrameDecoder::new();
        d.push(b"Content-Length: 1\r\n\r\n\xff").unwrap();
        assert_eq!(d.next_message(), Err(ProtocolError::InvalidUtf8));
    }

    #[test]
    fn body_and_buffer_bounds_are_checked_before_append() {
        for length in [MAX_FRAME_LEN + 1, usize::MAX] {
            let mut d = FrameDecoder::new();
            d.push(format!("Content-Length: {length}\r\n\r\n").as_bytes())
                .unwrap();
            assert_eq!(d.next_message(), Err(ProtocolError::BodyTooLong));
        }
        let mut d = FrameDecoder::new();
        d.push(&vec![b'x'; MAX_BUFFER_BYTES]).unwrap();
        assert!(d.buf.capacity() <= MAX_BUFFER_BYTES);
        assert_eq!(d.push(b"x"), Err(ProtocolError::BufferTooLong));
        assert_eq!(d.buf.capacity(), 0);
        let body = "x".repeat(MAX_FRAME_LEN);
        let mut d = FrameDecoder::new();
        d.push(&encode(&body)).unwrap();
        assert_eq!(pop(&mut d).as_deref(), Some(body.as_str()));
    }

    #[test]
    fn caller_limit_is_checked_at_header_admission() {
        let mut d = FrameDecoder::with_limit(8);
        d.push(b"Content-Length: 9\r\n\r\n").unwrap();
        assert_eq!(d.next_message(), Err(ProtocolError::BodyTooLong));
    }

    #[test]
    fn many_tiny_frames_have_linear_moves_and_no_read_after_budget_yield() {
        struct OneRead(Option<Vec<u8>>);
        impl Read for OneRead {
            fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
                let bytes = self
                    .0
                    .take()
                    .expect("buffered frames must precede another read");
                out[..bytes.len()].copy_from_slice(&bytes);
                Ok(bytes.len())
            }
        }
        let bytes = encode("{}").repeat(200);
        let mut reader = FramedReader::new(OneRead(Some(bytes.clone())));
        for _ in 0..200 {
            assert_eq!(reader.read_message().unwrap().as_deref(), Some("{}"));
        }
        assert!(reader.decoder.scanned <= bytes.len());
        assert!(reader.decoder.moved <= bytes.len());
        let mut d = FrameDecoder::new();
        let bytes = encode("{}").repeat(10000);
        d.push(&bytes).unwrap();
        for _ in 0..10000 {
            assert_eq!(pop(&mut d).as_deref(), Some("{}"));
        }
        assert!(d.scanned <= bytes.len());
        assert!(d.moved <= bytes.len());
    }

    #[test]
    fn hostile_full_buffer_sliding_window_cannot_force_repeated_suffix_moves() {
        let frame = encode("");
        let bytes = frame.repeat(MAX_BUFFER_BYTES / frame.len());
        let mut d = FrameDecoder::new();
        d.push(&bytes).unwrap();
        for _ in 0..10 {
            assert_eq!(pop(&mut d).as_deref(), Some(""));
            match d.push(&frame) {
                Ok(()) => {}
                Err(ProtocolError::BufferTooLong) => {
                    assert_eq!(d.moved, 0);
                    assert_eq!(d.buf.capacity(), 0);
                    return;
                }
                Err(other) => panic!("unexpected {other:?}"),
            }
        }
        panic!("undrained near-cap sliding window must fail before compaction");
    }

    #[test]
    fn reader_closes_on_invalid_or_truncated_stream_without_resync() {
        for bytes in [
            b"bad\r\n\r\n".to_vec(),
            b"Content-Length: 2\r\n\r\n{".to_vec(),
            vec![b'x'; MAX_HEADER_BYTES],
        ] {
            let mut reader = FramedReader::new(io::Cursor::new(bytes));
            assert!(reader.read_message().is_err());
            assert!(reader.read_message().is_err());
        }
        let mut reader = FramedReader::new(io::Cursor::new(encode("{}")));
        assert_eq!(reader.read_message().unwrap().as_deref(), Some("{}"));
        assert_eq!(reader.read_message().unwrap(), None);
    }
}
