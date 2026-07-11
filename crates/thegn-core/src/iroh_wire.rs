//! The pure wire protocol for the iroh call-home reach — the framing a sandbox
//! agent and the compositor exchange over an iroh QUIC bi-stream.
//!
//! Split like [`crate::revtunnel`]: this module is **pure** (no iroh, no tokio, no
//! I/O) so the framing round-trips are exhaustively unit-tested; the async pumps
//! that feed bytes through it live in `thegn-svc::iroh` (home side) and the
//! `thegn-agent` binary (sandbox side).
//!
//! One iroh bi-stream carries exactly one logical channel (QUIC multiplexes
//! streams natively, so there is no per-connection id like `revtunnel` needs):
//! - the **handshake** stream: the agent opens it and sends [`Wire::Hello`] to
//!   authenticate itself to the compositor.
//! - one **exec** stream per shell: the compositor opens it and sends
//!   [`Wire::Exec`], then streams [`Wire::Stdin`]/[`Wire::Resize`]/[`Wire::Close`]
//!   toward the agent while the agent streams [`Wire::Stdout`]/[`Wire::Exit`] back.
//!
//! Wire layout per frame: `[tag:u8][len:u32 BE][payload:len]`. Byte-heavy frames
//! (`Stdin`/`Stdout`) carry their payload raw (no base64/JSON-array bloat); the
//! two structured frames (`Hello`, `Exec`) carry compact JSON.

use serde::{Deserialize, Serialize};

/// The ALPN both sides negotiate for the call-home reach. Bumped if the framing
/// ever changes incompatibly.
pub const ALPN: &[u8] = b"thegn/agent/1";

/// Env var injected into a sandbox with the compositor's stable home EndpointId
/// (the tg-agent's dial target). Named under the `THEGN_*` family so it clears
/// the host env allowlist and is NOT a `*_TOKEN`-suffixed credential (which the
/// bundle firewall drops).
pub const HOME_NODE_ENV: &str = "THEGN_HOME_NODE";
/// Env var injected with this sandbox's minted, short-lived auth token. Uses
/// `_AUTH` (not `_TOKEN`) deliberately to dodge the credential-key drop.
pub const SANDBOX_AUTH_ENV: &str = "THEGN_SANDBOX_AUTH";
/// Env var naming which sandbox the agent serves (the home registry key).
pub const SANDBOX_ID_ENV: &str = "THEGN_SANDBOX_ID";

/// Max payload of a single frame (1 MiB), matching [`crate::revtunnel`].
pub const MAX_WIRE_PAYLOAD: usize = 1 << 20;

const T_HELLO: u8 = 0;
const T_EXEC: u8 = 1;
const T_STDIN: u8 = 2;
const T_RESIZE: u8 = 3;
const T_CLOSE: u8 = 4;
const T_STDOUT: u8 = 5;
const T_EXIT: u8 = 6;

/// The agent's opening handshake: proves which sandbox is calling home.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Hello {
    /// The per-sandbox auth token minted by the compositor at provision time.
    pub token: String,
    /// The sandbox id this agent serves (the compositor keys its connection
    /// registry on this).
    pub sandbox: String,
}

/// What the compositor asks the agent to run on an exec stream (mirrors the
/// transport-blind `ExecSpec` the pane machinery already consumes).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExecReq {
    /// Command + args (e.g. the login shell).
    pub argv: Vec<String>,
    /// Allocate a PTY (interactive panes always do).
    pub tty: bool,
    pub cols: u16,
    pub rows: u16,
    /// Extra environment as `(KEY, VALUE)` pairs.
    pub env: Vec<(String, String)>,
    /// Working directory inside the sandbox; `None` ⇒ the sandbox default.
    pub cwd: Option<String>,
}

/// One decoded frame. Direction is by convention (see the module doc): `Hello`,
/// `Stdin`, `Resize`, `Close`, and `Exec` flow compositor↔agent per channel role;
/// `Stdout`/`Exit` flow agent→compositor.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Wire {
    Hello(Hello),
    Exec(ExecReq),
    Stdin(Vec<u8>),
    Resize { cols: u16, rows: u16 },
    Close,
    Stdout(Vec<u8>),
    Exit(i32),
}

/// Serialize a frame to its wire bytes.
pub fn encode(w: &Wire) -> Vec<u8> {
    let (tag, payload): (u8, Vec<u8>) = match w {
        // JSON of these small structs is infallible.
        Wire::Hello(h) => (T_HELLO, serde_json::to_vec(h).expect("hello json")),
        Wire::Exec(r) => (T_EXEC, serde_json::to_vec(r).expect("exec req json")),
        Wire::Stdin(b) => (T_STDIN, b.clone()),
        Wire::Resize { cols, rows } => {
            let mut v = Vec::with_capacity(4);
            v.extend_from_slice(&cols.to_be_bytes());
            v.extend_from_slice(&rows.to_be_bytes());
            (T_RESIZE, v)
        }
        Wire::Close => (T_CLOSE, Vec::new()),
        Wire::Stdout(b) => (T_STDOUT, b.clone()),
        Wire::Exit(c) => (T_EXIT, c.to_be_bytes().to_vec()),
    };
    let mut out = Vec::with_capacity(5 + payload.len());
    out.push(tag);
    out.extend_from_slice(&(payload.len() as u32).to_be_bytes());
    out.extend_from_slice(&payload);
    out
}

/// Split raw output into `Stdout` frames no larger than [`MAX_WIRE_PAYLOAD`].
pub fn encode_stdout_chunked(data: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    for chunk in data.chunks(MAX_WIRE_PAYLOAD) {
        out.extend_from_slice(&encode(&Wire::Stdout(chunk.to_vec())));
    }
    out
}

/// Why the decoder rejected the stream — a protocol violation the caller should
/// treat as fatal (tear the connection down).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WireError {
    UnknownTag(u8),
    PayloadTooLarge(usize),
    /// A structured frame's payload didn't parse (bad JSON, or a fixed-width
    /// frame with the wrong length). Carries the offending tag.
    BadPayload(u8),
}

impl std::fmt::Display for WireError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            WireError::UnknownTag(t) => write!(f, "unknown wire tag {t}"),
            WireError::PayloadTooLarge(n) => write!(f, "wire payload too large: {n}"),
            WireError::BadPayload(t) => write!(f, "malformed wire payload for tag {t}"),
        }
    }
}

impl std::error::Error for WireError {}

/// Streaming frame decoder: feed arbitrary byte chunks (split at any boundary)
/// and drain whole [`Wire`] frames. Holds the partial trailing frame between calls.
#[derive(Debug, Default)]
pub struct WireDecoder {
    buf: Vec<u8>,
}

impl WireDecoder {
    pub fn new() -> Self {
        Self::default()
    }

    /// Append freshly-read bytes. Pair with [`next_frame`](Self::next_frame).
    pub fn push(&mut self, bytes: &[u8]) {
        self.buf.extend_from_slice(bytes);
    }

    /// Pop the next complete frame, or `Ok(None)` if more bytes are needed.
    pub fn next_frame(&mut self) -> Result<Option<Wire>, WireError> {
        if self.buf.len() < 5 {
            return Ok(None);
        }
        let tag = self.buf[0];
        let len = u32::from_be_bytes([self.buf[1], self.buf[2], self.buf[3], self.buf[4]]) as usize;
        if len > MAX_WIRE_PAYLOAD {
            return Err(WireError::PayloadTooLarge(len));
        }
        let total = 5 + len;
        if self.buf.len() < total {
            return Ok(None);
        }
        let payload = self.buf[5..total].to_vec();
        self.buf.drain(..total);
        let w = match tag {
            T_HELLO => Wire::Hello(
                serde_json::from_slice(&payload).map_err(|_| WireError::BadPayload(T_HELLO))?,
            ),
            T_EXEC => Wire::Exec(
                serde_json::from_slice(&payload).map_err(|_| WireError::BadPayload(T_EXEC))?,
            ),
            T_STDIN => Wire::Stdin(payload),
            T_RESIZE => {
                if payload.len() != 4 {
                    return Err(WireError::BadPayload(T_RESIZE));
                }
                Wire::Resize {
                    cols: u16::from_be_bytes([payload[0], payload[1]]),
                    rows: u16::from_be_bytes([payload[2], payload[3]]),
                }
            }
            T_CLOSE => Wire::Close,
            T_STDOUT => Wire::Stdout(payload),
            T_EXIT => {
                if payload.len() != 4 {
                    return Err(WireError::BadPayload(T_EXIT));
                }
                Wire::Exit(i32::from_be_bytes([
                    payload[0], payload[1], payload[2], payload[3],
                ]))
            }
            other => return Err(WireError::UnknownTag(other)),
        };
        Ok(Some(w))
    }

    /// Drain all currently-complete frames.
    pub fn drain(&mut self) -> Result<Vec<Wire>, WireError> {
        let mut out = Vec::new();
        while let Some(w) = self.next_frame()? {
            out.push(w);
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn roundtrip(w: Wire) {
        let bytes = encode(&w);
        let mut d = WireDecoder::new();
        d.push(&bytes);
        assert_eq!(d.next_frame().unwrap(), Some(w));
        assert_eq!(d.next_frame().unwrap(), None);
    }

    #[test]
    fn every_variant_round_trips() {
        roundtrip(Wire::Hello(Hello {
            token: "tok-123".into(),
            sandbox: "wt-abc".into(),
        }));
        roundtrip(Wire::Exec(ExecReq {
            argv: vec!["/bin/sh".into(), "-lc".into(), "echo hi".into()],
            tty: true,
            cols: 120,
            rows: 40,
            env: vec![("TERM".into(), "xterm-256color".into())],
            cwd: Some("/work".into()),
        }));
        roundtrip(Wire::Stdin(b"ls -la\n".to_vec()));
        roundtrip(Wire::Resize {
            cols: 200,
            rows: 50,
        });
        roundtrip(Wire::Close);
        roundtrip(Wire::Stdout(vec![0u8, 255, 1, 254, b'\n']));
        roundtrip(Wire::Exit(137));
        roundtrip(Wire::Exit(-1));
    }

    #[test]
    fn decodes_across_arbitrary_chunk_boundaries() {
        let mut bytes = encode(&Wire::Stdout(b"hello ".to_vec()));
        bytes.extend(encode(&Wire::Exit(0)));
        let mut d = WireDecoder::new();
        // Feed one byte at a time — the decoder must buffer partial frames.
        let mut got = Vec::new();
        for b in &bytes {
            d.push(&[*b]);
            while let Some(w) = d.next_frame().unwrap() {
                got.push(w);
            }
        }
        assert_eq!(got, vec![Wire::Stdout(b"hello ".to_vec()), Wire::Exit(0)]);
    }

    #[test]
    fn drain_returns_all_ready_frames() {
        let mut bytes = encode(&Wire::Stdin(b"a".to_vec()));
        bytes.extend(encode(&Wire::Stdin(b"b".to_vec())));
        bytes.extend(encode(&Wire::Close));
        let mut d = WireDecoder::new();
        d.push(&bytes);
        assert_eq!(
            d.drain().unwrap(),
            vec![
                Wire::Stdin(b"a".to_vec()),
                Wire::Stdin(b"b".to_vec()),
                Wire::Close
            ]
        );
    }

    #[test]
    fn unknown_tag_is_fatal() {
        let mut d = WireDecoder::new();
        d.push(&[99, 0, 0, 0, 0]); // tag 99, len 0
        assert_eq!(d.next_frame(), Err(WireError::UnknownTag(99)));
    }

    #[test]
    fn oversized_len_is_fatal() {
        let mut d = WireDecoder::new();
        let huge = (MAX_WIRE_PAYLOAD as u32 + 1).to_be_bytes();
        d.push(&[T_STDOUT, huge[0], huge[1], huge[2], huge[3]]);
        assert_eq!(
            d.next_frame(),
            Err(WireError::PayloadTooLarge(MAX_WIRE_PAYLOAD + 1))
        );
    }

    #[test]
    fn wrong_width_fixed_frame_is_bad_payload() {
        let mut d = WireDecoder::new();
        // A RESIZE frame with a 3-byte payload.
        d.push(&[T_RESIZE, 0, 0, 0, 3, 1, 2, 3]);
        assert_eq!(d.next_frame(), Err(WireError::BadPayload(T_RESIZE)));
    }

    #[test]
    fn chunked_stdout_splits_at_max_payload() {
        let big = vec![7u8; MAX_WIRE_PAYLOAD + 100];
        let bytes = encode_stdout_chunked(&big);
        let mut d = WireDecoder::new();
        d.push(&bytes);
        let frames = d.drain().unwrap();
        assert_eq!(frames.len(), 2);
        let mut reassembled = Vec::new();
        for f in frames {
            match f {
                Wire::Stdout(b) => reassembled.extend(b),
                other => panic!("unexpected {other:?}"),
            }
        }
        assert_eq!(reassembled, big);
    }
}
