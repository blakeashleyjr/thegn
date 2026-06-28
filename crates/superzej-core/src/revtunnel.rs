//! Reverse host→sandbox tunnel: the pure, transport-agnostic multiplexing
//! protocol.
//!
//! `[forward]` exposes a *sandbox* service to the *host* (the host binds a port
//! and bridges into the container over `exec` stdio). The **reverse** — a process
//! *inside* a remote sandbox (a sprite VM that can't reach host loopback) reaching
//! a *host* service (the local `szproxy`, a host `localhost` DB/API, a host-bound
//! MCP server) — needs the opposite: a forwarder agent in the sandbox listens on a
//! loopback port and multiplexes every accepted connection over the **single**
//! host-initiated exec byte stream back to the host, which demuxes and dials the
//! real target.
//!
//! This module is that wire protocol — pure, no I/O — so the framing + demux
//! routing is exhaustively unit-tested with in-memory mocks; the host/sandbox
//! sides just pump bytes through [`FrameDecoder`] / [`encode`]. One persistent
//! stream carries many concurrent connections, each keyed by a `u32` id.

/// A multiplexed tunnel frame. `id` keys a logical connection over the shared
/// stream; the host assigns ids when it dials, the sandbox echoes them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Frame {
    /// A new connection was accepted (sandbox→host) / should be dialed (host).
    Open(u32),
    /// Payload bytes for connection `id`, in either direction.
    Data(u32, Vec<u8>),
    /// Connection `id` closed (EOF / error) in the sending direction.
    Close(u32),
}

const T_OPEN: u8 = 1;
const T_DATA: u8 = 2;
const T_CLOSE: u8 = 3;

/// Max payload per `Data` frame the decoder will accept — a sanity bound so a
/// corrupt/garbage length can't trigger a huge allocation. Senders chunk to this.
pub const MAX_FRAME_PAYLOAD: usize = 1 << 20; // 1 MiB

/// Wire layout: `[type:u8][id:u32 BE][len:u32 BE][payload:len]`. `Open`/`Close`
/// carry `len = 0`.
pub fn encode(frame: &Frame) -> Vec<u8> {
    let (t, id, payload): (u8, u32, &[u8]) = match frame {
        Frame::Open(id) => (T_OPEN, *id, &[]),
        Frame::Close(id) => (T_CLOSE, *id, &[]),
        Frame::Data(id, d) => (T_DATA, *id, d),
    };
    let mut out = Vec::with_capacity(9 + payload.len());
    out.push(t);
    out.extend_from_slice(&id.to_be_bytes());
    out.extend_from_slice(&(payload.len() as u32).to_be_bytes());
    out.extend_from_slice(payload);
    out
}

/// Split `data` into one or more `Data` frames no larger than [`MAX_FRAME_PAYLOAD`]
/// and return the concatenated wire bytes. Empty input yields no frames.
pub fn encode_data_chunked(id: u32, data: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    for chunk in data.chunks(MAX_FRAME_PAYLOAD) {
        out.extend_from_slice(&encode(&Frame::Data(id, chunk.to_vec())));
    }
    out
}

/// Streaming frame decoder: feed arbitrary byte chunks (as they arrive off the
/// exec stream, split at any boundary) and drain whole [`Frame`]s. Holds a buffer
/// of the partial trailing frame between calls.
#[derive(Debug, Default)]
pub struct FrameDecoder {
    buf: Vec<u8>,
}

/// Why the decoder rejected the stream — a protocol violation the caller should
/// treat as fatal (tear the tunnel down) rather than skip.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DecodeError {
    UnknownType(u8),
    PayloadTooLarge(usize),
}

impl FrameDecoder {
    pub fn new() -> Self {
        Self::default()
    }

    /// Append freshly-read bytes. Pair with [`next_frame`](Self::next_frame) to drain.
    pub fn push(&mut self, bytes: &[u8]) {
        self.buf.extend_from_slice(bytes);
    }

    /// Pop the next complete frame, or `Ok(None)` if more bytes are needed.
    pub fn next_frame(&mut self) -> Result<Option<Frame>, DecodeError> {
        if self.buf.len() < 9 {
            return Ok(None);
        }
        let t = self.buf[0];
        let id = u32::from_be_bytes([self.buf[1], self.buf[2], self.buf[3], self.buf[4]]);
        let len = u32::from_be_bytes([self.buf[5], self.buf[6], self.buf[7], self.buf[8]]) as usize;
        if len > MAX_FRAME_PAYLOAD {
            return Err(DecodeError::PayloadTooLarge(len));
        }
        let total = 9 + len;
        if self.buf.len() < total {
            return Ok(None); // payload not fully arrived yet
        }
        let payload = self.buf[9..total].to_vec();
        // Consume the frame from the front of the buffer.
        self.buf.drain(..total);
        match t {
            T_OPEN => Ok(Some(Frame::Open(id))),
            T_CLOSE => Ok(Some(Frame::Close(id))),
            T_DATA => Ok(Some(Frame::Data(id, payload))),
            other => Err(DecodeError::UnknownType(other)),
        }
    }

    /// Drain all currently-complete frames.
    pub fn drain(&mut self) -> Result<Vec<Frame>, DecodeError> {
        let mut out = Vec::new();
        while let Some(f) = self.next_frame()? {
            out.push(f);
        }
        Ok(out)
    }

    /// Bytes buffered but not yet a complete frame (for tests / backpressure).
    pub fn pending(&self) -> usize {
        self.buf.len()
    }
}

/// Parse a reverse-forward spec into `(sandbox_port, host_target)` — i.e. "bind
/// this loopback port inside the sandbox; forward it to this host target". Forms:
/// - `"5432"`            → `(5432, "127.0.0.1:5432")` (same port both sides)
/// - `"8080:5432"`       → `(8080, "127.0.0.1:5432")` (host loopback port)
/// - `"8080:db.lan:5432"`→ `(8080, "db.lan:5432")` (explicit host:port)
///
/// `None` on a malformed/empty spec. Used for `[sandbox.home] reverse_forwards`
/// (host DB/API + host-bound MCP servers reachable from the sandbox).
pub fn parse_reverse_forward(spec: &str) -> Option<(u16, String)> {
    let s = spec.trim();
    if s.is_empty() {
        return None;
    }
    match s.split_once(':') {
        None => {
            let p: u16 = s.parse().ok()?;
            Some((p, format!("127.0.0.1:{p}")))
        }
        Some((sp, rest)) => {
            let sandbox_port: u16 = sp.trim().parse().ok()?;
            let rest = rest.trim();
            let host = if rest.contains(':') {
                rest.to_string()
            } else {
                let hp: u16 = rest.parse().ok()?;
                format!("127.0.0.1:{hp}")
            };
            Some((sandbox_port, host))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_reverse_forward_forms() {
        assert_eq!(
            parse_reverse_forward("5432"),
            Some((5432, "127.0.0.1:5432".into()))
        );
        assert_eq!(
            parse_reverse_forward("8080:5432"),
            Some((8080, "127.0.0.1:5432".into()))
        );
        assert_eq!(
            parse_reverse_forward("8080:db.lan:5432"),
            Some((8080, "db.lan:5432".into()))
        );
        assert_eq!(
            parse_reverse_forward("  3000 "),
            Some((3000, "127.0.0.1:3000".into()))
        );
        assert_eq!(parse_reverse_forward(""), None);
        assert_eq!(parse_reverse_forward("notaport"), None);
        assert_eq!(parse_reverse_forward("99999"), None); // > u16
    }

    fn roundtrip(f: Frame) {
        let mut d = FrameDecoder::new();
        d.push(&encode(&f));
        assert_eq!(d.next_frame().unwrap(), Some(f));
        assert_eq!(d.next_frame().unwrap(), None);
        assert_eq!(d.pending(), 0);
    }

    #[test]
    fn roundtrips_each_frame_kind() {
        roundtrip(Frame::Open(0));
        roundtrip(Frame::Open(4_000_000_000));
        roundtrip(Frame::Close(7));
        roundtrip(Frame::Data(3, b"hello".to_vec()));
        roundtrip(Frame::Data(1, vec![])); // empty data frame is valid
    }

    #[test]
    fn decodes_multiple_concatenated_frames_in_order() {
        let mut wire = Vec::new();
        wire.extend(encode(&Frame::Open(1)));
        wire.extend(encode(&Frame::Data(1, b"ab".to_vec())));
        wire.extend(encode(&Frame::Data(2, b"xy".to_vec())));
        wire.extend(encode(&Frame::Close(1)));
        let mut d = FrameDecoder::new();
        d.push(&wire);
        assert_eq!(
            d.drain().unwrap(),
            vec![
                Frame::Open(1),
                Frame::Data(1, b"ab".to_vec()),
                Frame::Data(2, b"xy".to_vec()),
                Frame::Close(1),
            ]
        );
    }

    #[test]
    fn reassembles_frames_split_across_arbitrary_chunk_boundaries() {
        let wire = {
            let mut w = Vec::new();
            w.extend(encode(&Frame::Open(9)));
            w.extend(encode(&Frame::Data(9, b"streamed-payload".to_vec())));
            w
        };
        // Feed one byte at a time — the decoder must hold partials and only yield
        // whole frames.
        let mut d = FrameDecoder::new();
        let mut got = Vec::new();
        for b in &wire {
            d.push(&[*b]);
            while let Some(f) = d.next_frame().unwrap() {
                got.push(f);
            }
        }
        assert_eq!(
            got,
            vec![Frame::Open(9), Frame::Data(9, b"streamed-payload".to_vec())]
        );
    }

    #[test]
    fn partial_header_yields_none_without_consuming() {
        let mut d = FrameDecoder::new();
        d.push(&[T_DATA, 0, 0]); // only 3 of 9 header bytes
        assert_eq!(d.next_frame().unwrap(), None);
        assert_eq!(d.pending(), 3, "partial header is retained");
    }

    #[test]
    fn chunked_data_respects_max_payload() {
        let big = vec![0xABu8; MAX_FRAME_PAYLOAD * 2 + 5];
        let wire = encode_data_chunked(42, &big);
        let mut d = FrameDecoder::new();
        d.push(&wire);
        let frames = d.drain().unwrap();
        assert_eq!(frames.len(), 3, "2 full + 1 remainder chunk");
        // Reassembled payload matches and never exceeds the per-frame bound.
        let mut reassembled = Vec::new();
        for f in frames {
            match f {
                Frame::Data(id, d) => {
                    assert_eq!(id, 42);
                    assert!(d.len() <= MAX_FRAME_PAYLOAD);
                    reassembled.extend(d);
                }
                _ => panic!("expected data frames"),
            }
        }
        assert_eq!(reassembled, big);
    }

    #[test]
    fn rejects_unknown_type_and_oversized_length() {
        let mut d = FrameDecoder::new();
        d.push(&[9u8, 0, 0, 0, 1, 0, 0, 0, 0]); // type 9, len 0
        assert_eq!(d.next_frame(), Err(DecodeError::UnknownType(9)));

        let mut d2 = FrameDecoder::new();
        let huge = (MAX_FRAME_PAYLOAD as u32 + 1).to_be_bytes();
        d2.push(&[T_DATA, 0, 0, 0, 1, huge[0], huge[1], huge[2], huge[3]]);
        assert_eq!(
            d2.next_frame(),
            Err(DecodeError::PayloadTooLarge(MAX_FRAME_PAYLOAD + 1))
        );
    }

    #[test]
    fn empty_input_is_none() {
        let mut d = FrameDecoder::new();
        assert_eq!(d.next_frame().unwrap(), None);
        assert!(d.drain().unwrap().is_empty());
    }
}
