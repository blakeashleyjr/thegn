//! The pure wire protocol for the control-plane event feed — the frames a pane
//! daemon streams to attached clients (compositor, CLI, thin/mobile clients).
//!
//! Split like [`crate::iroh_wire`]: this module is **pure** (no tokio, no
//! network) so the framing round-trips are exhaustively unit-tested; the async
//! pumps live in `thegn-svc::control` (WebSocket/gRPC adapters) and the
//! daemon (host side).
//!
//! One encoded frame per WebSocket **binary message** (WS supplies message
//! boundaries, but keeping the tag/len framing means one codec serves every
//! transport — a raw unix-socket stream needs it, and gRPC mirrors the enum
//! mechanically). Wire layout per frame: `[tag:u8][len:u32 BE][payload:len]`.
//! Byte-heavy frames (`PaneSnapshot`/`PaneDelta`) carry a compact JSON header
//! length-prefixed before the raw bytes (no base64 bloat); structured frames
//! are compact JSON.

use serde::{Deserialize, Serialize};

use crate::control::Scope;

/// Protocol version advertised in [`EventFrame::Hello`]. Bumped on any
/// incompatible framing change.
pub const PROTO_VERSION: u32 = 1;

/// Max payload of a single frame (1 MiB), matching [`crate::iroh_wire`].
pub const MAX_WIRE_PAYLOAD: usize = 1 << 20;

const T_HELLO: u8 = 0;
const T_SNAPSHOT: u8 = 1;
const T_DELTA: u8 = 2;
const T_ACTIVITY: u8 = 3;
const T_LEASE: u8 = 4;
const T_PAIRING: u8 = 5;
const T_SESSIONS: u8 = 6;
const T_EXIT: u8 = 7;
const T_LAGGED: u8 = 8;

/// The maximum number of bytes accepted for a session filter.
pub const MAX_FEED_SESSION_LEN: usize = 256;

/// The stable names used by the control event feed.
pub const FEED_KINDS: &[&str] = &[
    "hello", "snapshot", "delta", "activity", "lease", "pairing", "sessions", "exit", "lagged",
];

/// Server greeting on a fresh event stream.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Hello {
    pub proto: u32,
    /// Human-readable server identity (hostname + version).
    pub server: String,
    /// The scopes the presented token holds (what this stream may see/do).
    pub scopes: Vec<Scope>,
}

/// Header of a byte-carrying pane frame (rides length-prefixed before the raw
/// bytes inside the frame payload).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct PaneHeader {
    session: String,
    /// Monotone per-session output sequence. A snapshot at `seq` folds bytes
    /// `..=seq`; the first delta after it carries `seq + 1`.
    seq: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    cols: Option<u16>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    rows: Option<u16>,
}

/// What a lease event reports.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LeaseEventKind {
    Opened,
    Refreshed,
    Released,
    Reaped,
}

/// Where a pairing sits in its lifecycle.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PairingState {
    Requested,
    Approved,
    Revoked,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct LeaseBody {
    session: String,
    kind: LeaseEventKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    expires_at: Option<i64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct ExitBody {
    session: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    code: Option<i32>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct LaggedBody {
    missed: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct PairingBody {
    pairing_id: String,
    label: String,
    scope: String,
    state: PairingState,
}

/// One decoded event frame. Direction is daemon → client only (client → daemon
/// traffic is plain RPC, not this feed).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EventFrame {
    Hello(Hello),
    /// Full emulator screen for warm-reattach (ANSI repaint bytes from
    /// [`crate::term_snapshot`]). `seq` tags the last output chunk folded in.
    PaneSnapshot {
        session: String,
        seq: u64,
        cols: u16,
        rows: u16,
        bytes: Vec<u8>,
    },
    /// Live raw PTY output.
    PaneDelta {
        session: String,
        seq: u64,
        bytes: Vec<u8>,
    },
    /// A serialized [`crate::activity`] event (opaque JSON to the wire).
    Activity {
        json: String,
    },
    Lease {
        session: String,
        kind: LeaseEventKind,
        expires_at: Option<i64>,
    },
    Pairing {
        pairing_id: String,
        label: String,
        scope: String,
        state: PairingState,
    },
    /// The session/worktree list changed — clients re-list.
    Sessions,
    /// A session's process exited (terminal — distinct from a dropped
    /// transport, which reconnects). `code` is `None` when unreapable.
    SessionExit {
        session: String,
        code: Option<i32>,
    },
    /// A subscriber fell behind the bounded broadcast by `missed` frames.
    /// This is emitted only when the subscription opts into lag signaling.
    Lagged {
        missed: u64,
    },
}

/// Per-connection narrowing options for the control event feed.
///
/// An absent `kinds` or `session` field means no narrowing. The transport
/// adapters parse their query/request representation through [`Self::parse`]
/// or [`Self::from_parts`] before creating a subscription.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct FeedFilter {
    /// A subset of [`FEED_KINDS`]. `None` means every kind.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kinds: Option<Vec<String>>,
    /// Limit session-keyed frames to this session. Unkeyed frames still pass.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session: Option<String>,
    /// Emit [`EventFrame::Lagged`] when the bounded broadcast reports loss.
    #[serde(default)]
    pub signal_lag: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FeedFilterError {
    EmptyKind,
    UnknownKind(String),
    TooManyKinds,
    EmptySession,
    SessionTooLong,
}

impl std::fmt::Display for FeedFilterError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::EmptyKind => write!(f, "event kind must not be empty"),
            Self::UnknownKind(kind) => write!(f, "unknown event kind {kind:?}"),
            Self::TooManyKinds => write!(f, "too many event kinds"),
            Self::EmptySession => write!(f, "session filter must not be empty"),
            Self::SessionTooLong => write!(
                f,
                "session filter exceeds {MAX_FEED_SESSION_LEN}-byte limit"
            ),
        }
    }
}

impl std::error::Error for FeedFilterError {}

impl FeedFilter {
    /// Parse HTTP-style comma-separated kinds and optional session id.
    pub fn parse(
        kinds: Option<&str>,
        session: Option<&str>,
        signal_lag: bool,
    ) -> Result<Self, FeedFilterError> {
        let kinds = kinds.map(Self::parse_kinds).transpose()?.flatten();
        Self::from_parts(kinds, session.map(str::to_string), signal_lag)
    }

    /// Validate gRPC-style repeated kind names and an optional session id.
    pub fn from_parts(
        kinds: Option<Vec<String>>,
        session: Option<String>,
        signal_lag: bool,
    ) -> Result<Self, FeedFilterError> {
        let kinds = kinds.map(Self::validated_kinds).transpose()?.flatten();
        if let Some(session) = &session {
            if session.is_empty() {
                return Err(FeedFilterError::EmptySession);
            }
            if session.len() > MAX_FEED_SESSION_LEN {
                return Err(FeedFilterError::SessionTooLong);
            }
        }
        Ok(Self {
            kinds,
            session,
            signal_lag,
        })
    }

    fn validated_kinds(kinds: Vec<String>) -> Result<Option<Vec<String>>, FeedFilterError> {
        if kinds.is_empty() {
            return Err(FeedFilterError::EmptyKind);
        }
        if kinds.len() > FEED_KINDS.len() {
            return Err(FeedFilterError::TooManyKinds);
        }
        let mut validated = Vec::with_capacity(kinds.len());
        for kind in kinds {
            if kind.is_empty() {
                return Err(FeedFilterError::EmptyKind);
            }
            if !FEED_KINDS.contains(&kind.as_str()) {
                return Err(FeedFilterError::UnknownKind(kind));
            }
            if !validated.iter().any(|seen| seen == &kind) {
                validated.push(kind);
            }
        }
        Ok(Some(validated))
    }

    fn parse_kinds(kinds: &str) -> Result<Option<Vec<String>>, FeedFilterError> {
        let mut parsed = Vec::new();
        for (index, kind) in kinds.split(',').map(str::trim).enumerate() {
            if index >= FEED_KINDS.len() {
                return Err(FeedFilterError::TooManyKinds);
            }
            if kind.is_empty() {
                return Err(FeedFilterError::EmptyKind);
            }
            if !FEED_KINDS.contains(&kind) {
                return Err(FeedFilterError::UnknownKind(kind.to_string()));
            }
            if !parsed.iter().any(|seen| seen == kind) {
                parsed.push(kind.to_string());
            }
        }
        Ok(Some(parsed))
    }

    /// Whether a frame should be delivered to this subscription.
    pub fn matches(&self, frame: &EventFrame) -> bool {
        // Every subscription receives the greeting, including a narrowed one.
        if frame.kind() == "hello" {
            return true;
        }
        if let Some(kinds) = &self.kinds
            && !kinds.iter().any(|kind| kind == frame.kind())
        {
            return false;
        }
        let Some(wanted) = &self.session else {
            return true;
        };
        match frame.session_id() {
            Some(session) => session == wanted,
            None => match frame {
                // Activity payloads are intentionally opaque on the core wire,
                // but the current event contract carries their session id.
                EventFrame::Activity { json } => serde_json::from_str::<serde_json::Value>(json)
                    .ok()
                    .and_then(|value| value.get("session")?.as_str().map(str::to_string))
                    .is_none_or(|session| session == *wanted),
                // Unkeyed event families pass a session narrowing filter.
                _ => true,
            },
        }
    }
}

/// Why the decoder rejected the stream — fatal; tear the connection down.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WireError {
    UnknownTag(u8),
    PayloadTooLarge(usize),
    /// A frame's payload didn't parse. Carries the offending tag.
    BadPayload(u8),
}

impl std::fmt::Display for WireError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            WireError::UnknownTag(t) => write!(f, "unknown control wire tag {t}"),
            WireError::PayloadTooLarge(n) => write!(f, "control wire payload too large: {n}"),
            WireError::BadPayload(t) => write!(f, "malformed control wire payload for tag {t}"),
        }
    }
}

impl std::error::Error for WireError {}

/// Prepended when a snapshot's bytes get truncated (below), so a cut that lands
/// mid-escape doesn't leave the reattached terminal in a broken SGR/mode state:
/// SGR reset + reset-to-ground for any half-consumed CSI/OSC. It also visibly
/// clears anything stale. Small, so it never itself pushes over the cap.
const TRUNCATE_PREFIX: &[u8] = b"\x1b[0m\x1b\\\x1b[2J\x1b[H";

/// `[header-len:u16 BE][header JSON][raw bytes]` inside a pane frame's payload.
/// The whole frame is `[tag:u8][len:u32][payload]`, and the decoder fatals on
/// `len > MAX_WIRE_PAYLOAD` (tearing the stream down permanently on reconnect),
/// so a snapshot whose bytes overflow the cap must be bounded *here* rather than
/// trusting the producer — the producer path (daemon `snapshot_frame`) has no
/// per-line or total size guard. We keep the **tail** (most recent output) and
/// prepend a reset so a mid-sequence cut is harmless.
fn encode_pane_payload(header: &PaneHeader, bytes: &[u8]) -> Vec<u8> {
    let h = serde_json::to_vec(header).expect("pane header json");
    // Bytes budget so `[tag][len][hdr-len][hdr][bytes]` fits MAX_WIRE_PAYLOAD.
    let overhead = 5 + 2 + h.len();
    let budget = MAX_WIRE_PAYLOAD.saturating_sub(overhead);
    let bytes: std::borrow::Cow<'_, [u8]> = if bytes.len() > budget {
        // Keep the tail; reserve room for the reset prefix we prepend.
        let keep = budget.saturating_sub(TRUNCATE_PREFIX.len());
        let tail = &bytes[bytes.len() - keep..];
        let mut trunc = Vec::with_capacity(TRUNCATE_PREFIX.len() + tail.len());
        trunc.extend_from_slice(TRUNCATE_PREFIX);
        trunc.extend_from_slice(tail);
        std::borrow::Cow::Owned(trunc)
    } else {
        std::borrow::Cow::Borrowed(bytes)
    };
    let mut out = Vec::with_capacity(2 + h.len() + bytes.len());
    out.extend_from_slice(&(h.len() as u16).to_be_bytes());
    out.extend_from_slice(&h);
    out.extend_from_slice(&bytes);
    out
}

fn decode_pane_payload(tag: u8, payload: &[u8]) -> Result<(PaneHeader, Vec<u8>), WireError> {
    if payload.len() < 2 {
        return Err(WireError::BadPayload(tag));
    }
    let hlen = u16::from_be_bytes([payload[0], payload[1]]) as usize;
    if payload.len() < 2 + hlen {
        return Err(WireError::BadPayload(tag));
    }
    let header: PaneHeader =
        serde_json::from_slice(&payload[2..2 + hlen]).map_err(|_| WireError::BadPayload(tag))?;
    Ok((header, payload[2 + hlen..].to_vec()))
}

impl EventFrame {
    /// Stable frame-kind name shared by filters and all transport formatters.
    pub fn kind(&self) -> &'static str {
        match self {
            Self::Hello(_) => "hello",
            Self::PaneSnapshot { .. } => "snapshot",
            Self::PaneDelta { .. } => "delta",
            Self::Activity { .. } => "activity",
            Self::Lease { .. } => "lease",
            Self::Pairing { .. } => "pairing",
            Self::Sessions => "sessions",
            Self::SessionExit { .. } => "exit",
            Self::Lagged { .. } => "lagged",
        }
    }

    /// Return the explicit session key carried by a frame, if it has one.
    pub fn session_id(&self) -> Option<&str> {
        match self {
            Self::PaneSnapshot { session, .. }
            | Self::PaneDelta { session, .. }
            | Self::Lease { session, .. }
            | Self::SessionExit { session, .. } => Some(session),
            _ => None,
        }
    }

    /// Serialize a frame to its wire bytes.
    pub fn encode(&self) -> Vec<u8> {
        let (tag, payload): (u8, Vec<u8>) = match self {
            EventFrame::Hello(h) => (T_HELLO, serde_json::to_vec(h).expect("hello json")),
            EventFrame::PaneSnapshot {
                session,
                seq,
                cols,
                rows,
                bytes,
            } => (
                T_SNAPSHOT,
                encode_pane_payload(
                    &PaneHeader {
                        session: session.clone(),
                        seq: *seq,
                        cols: Some(*cols),
                        rows: Some(*rows),
                    },
                    bytes,
                ),
            ),
            EventFrame::PaneDelta {
                session,
                seq,
                bytes,
            } => (
                T_DELTA,
                encode_pane_payload(
                    &PaneHeader {
                        session: session.clone(),
                        seq: *seq,
                        cols: None,
                        rows: None,
                    },
                    bytes,
                ),
            ),
            EventFrame::Activity { json } => (T_ACTIVITY, json.as_bytes().to_vec()),
            EventFrame::Lease {
                session,
                kind,
                expires_at,
            } => (
                T_LEASE,
                serde_json::to_vec(&LeaseBody {
                    session: session.clone(),
                    kind: *kind,
                    expires_at: *expires_at,
                })
                .expect("lease json"),
            ),
            EventFrame::Pairing {
                pairing_id,
                label,
                scope,
                state,
            } => (
                T_PAIRING,
                serde_json::to_vec(&PairingBody {
                    pairing_id: pairing_id.clone(),
                    label: label.clone(),
                    scope: scope.clone(),
                    state: *state,
                })
                .expect("pairing json"),
            ),
            EventFrame::Sessions => (T_SESSIONS, Vec::new()),
            EventFrame::SessionExit { session, code } => (
                T_EXIT,
                serde_json::to_vec(&ExitBody {
                    session: session.clone(),
                    code: *code,
                })
                .expect("exit json"),
            ),
            EventFrame::Lagged { missed } => (
                T_LAGGED,
                serde_json::to_vec(&LaggedBody { missed: *missed }).expect("lagged json"),
            ),
        };
        let mut out = Vec::with_capacity(5 + payload.len());
        out.push(tag);
        out.extend_from_slice(&(payload.len() as u32).to_be_bytes());
        out.extend_from_slice(&payload);
        out
    }
}

/// Streaming frame decoder: feed arbitrary byte chunks (split at any boundary)
/// and drain whole [`EventFrame`]s. Holds the partial trailing frame between calls.
#[derive(Debug, Default)]
pub struct EventDecoder {
    buf: Vec<u8>,
}

impl EventDecoder {
    pub fn new() -> Self {
        Self::default()
    }

    /// Append freshly-read bytes. Pair with [`next_frame`](Self::next_frame).
    pub fn push(&mut self, bytes: &[u8]) {
        self.buf.extend_from_slice(bytes);
    }

    /// Pop the next complete frame, or `Ok(None)` if more bytes are needed.
    pub fn next_frame(&mut self) -> Result<Option<EventFrame>, WireError> {
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
        let frame = match tag {
            T_HELLO => EventFrame::Hello(
                serde_json::from_slice(&payload).map_err(|_| WireError::BadPayload(T_HELLO))?,
            ),
            T_SNAPSHOT => {
                let (h, bytes) = decode_pane_payload(T_SNAPSHOT, &payload)?;
                let (Some(cols), Some(rows)) = (h.cols, h.rows) else {
                    return Err(WireError::BadPayload(T_SNAPSHOT));
                };
                EventFrame::PaneSnapshot {
                    session: h.session,
                    seq: h.seq,
                    cols,
                    rows,
                    bytes,
                }
            }
            T_DELTA => {
                let (h, bytes) = decode_pane_payload(T_DELTA, &payload)?;
                EventFrame::PaneDelta {
                    session: h.session,
                    seq: h.seq,
                    bytes,
                }
            }
            T_ACTIVITY => EventFrame::Activity {
                json: String::from_utf8(payload).map_err(|_| WireError::BadPayload(T_ACTIVITY))?,
            },
            T_LEASE => {
                let b: LeaseBody =
                    serde_json::from_slice(&payload).map_err(|_| WireError::BadPayload(T_LEASE))?;
                EventFrame::Lease {
                    session: b.session,
                    kind: b.kind,
                    expires_at: b.expires_at,
                }
            }
            T_PAIRING => {
                let b: PairingBody = serde_json::from_slice(&payload)
                    .map_err(|_| WireError::BadPayload(T_PAIRING))?;
                EventFrame::Pairing {
                    pairing_id: b.pairing_id,
                    label: b.label,
                    scope: b.scope,
                    state: b.state,
                }
            }
            T_SESSIONS => EventFrame::Sessions,
            T_EXIT => {
                let b: ExitBody =
                    serde_json::from_slice(&payload).map_err(|_| WireError::BadPayload(T_EXIT))?;
                EventFrame::SessionExit {
                    session: b.session,
                    code: b.code,
                }
            }
            T_LAGGED => {
                let b: LaggedBody = serde_json::from_slice(&payload)
                    .map_err(|_| WireError::BadPayload(T_LAGGED))?;
                EventFrame::Lagged { missed: b.missed }
            }
            other => return Err(WireError::UnknownTag(other)),
        };
        Ok(Some(frame))
    }

    /// Drain all currently-complete frames.
    pub fn drain(&mut self) -> Result<Vec<EventFrame>, WireError> {
        let mut out = Vec::new();
        while let Some(f) = self.next_frame()? {
            out.push(f);
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn roundtrip(f: EventFrame) {
        let bytes = f.encode();
        let mut d = EventDecoder::new();
        d.push(&bytes);
        assert_eq!(d.next_frame().unwrap(), Some(f));
        assert_eq!(d.next_frame().unwrap(), None);
    }

    #[test]
    fn every_variant_round_trips() {
        roundtrip(EventFrame::Hello(Hello {
            proto: PROTO_VERSION,
            server: "testhost thegn 0.1".into(),
            scopes: vec![Scope::Read, Scope::Git],
        }));
        roundtrip(EventFrame::PaneSnapshot {
            session: "sess-1".into(),
            seq: 42,
            cols: 120,
            rows: 40,
            bytes: b"\x1b[2J\x1b[Hhello".to_vec(),
        });
        roundtrip(EventFrame::PaneDelta {
            session: "sess-1".into(),
            seq: 43,
            bytes: vec![0u8, 255, 27, b'[', b'm'],
        });
        roundtrip(EventFrame::Activity {
            json: r#"{"kind":"agent_idle","worktree":"wt"}"#.into(),
        });
        roundtrip(EventFrame::Lease {
            session: "sess-1".into(),
            kind: LeaseEventKind::Reaped,
            expires_at: None,
        });
        roundtrip(EventFrame::Lease {
            session: "sess-2".into(),
            kind: LeaseEventKind::Opened,
            expires_at: Some(99_000),
        });
        roundtrip(EventFrame::Pairing {
            pairing_id: "0123abcd".into(),
            label: "phone".into(),
            scope: "read,git".into(),
            state: PairingState::Requested,
        });
        roundtrip(EventFrame::Sessions);
        roundtrip(EventFrame::SessionExit {
            session: "sess-1".into(),
            code: Some(137),
        });
        roundtrip(EventFrame::SessionExit {
            session: "sess-2".into(),
            code: None,
        });
        roundtrip(EventFrame::Lagged { missed: 17 });
    }

    #[test]
    fn decodes_across_arbitrary_chunk_boundaries() {
        let mut bytes = EventFrame::PaneDelta {
            session: "s".into(),
            seq: 1,
            bytes: b"chunk".to_vec(),
        }
        .encode();
        bytes.extend(EventFrame::Sessions.encode());
        let mut d = EventDecoder::new();
        let mut got = Vec::new();
        // One byte at a time — the decoder must buffer partial frames.
        for b in &bytes {
            d.push(&[*b]);
            while let Some(f) = d.next_frame().unwrap() {
                got.push(f);
            }
        }
        assert_eq!(got.len(), 2);
        assert_eq!(got[1], EventFrame::Sessions);
    }

    #[test]
    fn drain_returns_all_ready_frames() {
        let mut bytes = EventFrame::Sessions.encode();
        bytes.extend(EventFrame::Activity { json: "{}".into() }.encode());
        let mut d = EventDecoder::new();
        d.push(&bytes);
        assert_eq!(d.drain().unwrap().len(), 2);
    }

    #[test]
    fn unknown_tag_is_fatal() {
        let mut d = EventDecoder::new();
        d.push(&[99, 0, 0, 0, 0]);
        assert_eq!(d.next_frame(), Err(WireError::UnknownTag(99)));
    }

    #[test]
    fn oversized_len_is_fatal() {
        let mut d = EventDecoder::new();
        let huge = (MAX_WIRE_PAYLOAD as u32 + 1).to_be_bytes();
        d.push(&[T_DELTA, huge[0], huge[1], huge[2], huge[3]]);
        assert_eq!(
            d.next_frame(),
            Err(WireError::PayloadTooLarge(MAX_WIRE_PAYLOAD + 1))
        );
    }

    #[test]
    fn truncated_or_bad_pane_header_is_bad_payload() {
        // Payload shorter than its header-length prefix.
        let mut d = EventDecoder::new();
        d.push(&[T_DELTA, 0, 0, 0, 3, 0, 200, b'x']);
        assert_eq!(d.next_frame(), Err(WireError::BadPayload(T_DELTA)));
        // Header that isn't JSON.
        let mut payload = vec![0u8, 3];
        payload.extend_from_slice(b"???bytes");
        let mut framed = vec![T_DELTA];
        framed.extend_from_slice(&(payload.len() as u32).to_be_bytes());
        framed.extend_from_slice(&payload);
        let mut d = EventDecoder::new();
        d.push(&framed);
        assert_eq!(d.next_frame(), Err(WireError::BadPayload(T_DELTA)));
        // A snapshot without geometry is malformed.
        let delta_shaped = EventFrame::PaneDelta {
            session: "s".into(),
            seq: 1,
            bytes: b"x".to_vec(),
        }
        .encode();
        let mut as_snapshot = delta_shaped.clone();
        as_snapshot[0] = T_SNAPSHOT;
        let mut d = EventDecoder::new();
        d.push(&as_snapshot);
        assert_eq!(d.next_frame(), Err(WireError::BadPayload(T_SNAPSHOT)));
    }

    #[test]
    fn non_utf8_activity_is_bad_payload() {
        let mut framed = vec![T_ACTIVITY];
        framed.extend_from_slice(&2u32.to_be_bytes());
        framed.extend_from_slice(&[0xff, 0xfe]);
        let mut d = EventDecoder::new();
        d.push(&framed);
        assert_eq!(d.next_frame(), Err(WireError::BadPayload(T_ACTIVITY)));
    }

    #[test]
    fn oversized_snapshot_is_bounded_and_still_decodes() {
        // A snapshot whose repaint bytes exceed the 1 MiB cap must not produce a
        // frame the decoder fatals on (which would break warm attach forever).
        let big = vec![b'x'; MAX_WIRE_PAYLOAD + 64 * 1024];
        let f = EventFrame::PaneSnapshot {
            session: "sess-huge".into(),
            seq: 9,
            cols: 200,
            rows: 50,
            bytes: big,
        };
        let encoded = f.encode();
        assert!(
            encoded.len() <= MAX_WIRE_PAYLOAD + 5,
            "encoded frame {} exceeds cap",
            encoded.len()
        );
        let mut d = EventDecoder::new();
        d.push(&encoded);
        // Decodes cleanly — no PayloadTooLarge tear-down.
        let got = d.next_frame().unwrap().expect("frame present");
        match got {
            EventFrame::PaneSnapshot {
                session,
                seq,
                cols,
                rows,
                bytes,
            } => {
                assert_eq!(session, "sess-huge");
                assert_eq!((seq, cols, rows), (9, 200, 50));
                // Truncated to fit, with the reset prefix preserving the tail.
                assert!(bytes.starts_with(TRUNCATE_PREFIX));
                assert!(bytes.ends_with(b"xxxx"));
                assert!(bytes.len() < MAX_WIRE_PAYLOAD);
            }
            other => panic!("expected PaneSnapshot, got {other:?}"),
        }
        assert_eq!(d.next_frame().unwrap(), None);
    }

    #[test]
    fn at_cap_snapshot_is_untouched() {
        // A snapshot comfortably under the cap must pass through byte-for-byte.
        let bytes = vec![b'y'; 4096];
        roundtrip(EventFrame::PaneSnapshot {
            session: "s".into(),
            seq: 1,
            cols: 80,
            rows: 24,
            bytes,
        });
    }

    #[test]
    fn pane_bytes_survive_binary_content() {
        // Raw bytes ride unencoded — every byte value must survive.
        let all: Vec<u8> = (0u8..=255).collect();
        roundtrip(EventFrame::PaneDelta {
            session: "bin".into(),
            seq: 7,
            bytes: all,
        });
    }

    #[test]
    fn frame_kinds_are_the_filter_vocabulary() {
        let frames = [
            EventFrame::Hello(Hello {
                proto: PROTO_VERSION,
                server: "s".into(),
                scopes: vec![],
            }),
            EventFrame::PaneSnapshot {
                session: "s".into(),
                seq: 1,
                cols: 80,
                rows: 24,
                bytes: vec![],
            },
            EventFrame::PaneDelta {
                session: "s".into(),
                seq: 2,
                bytes: vec![],
            },
            EventFrame::Activity { json: "{}".into() },
            EventFrame::Lease {
                session: "s".into(),
                kind: LeaseEventKind::Opened,
                expires_at: None,
            },
            EventFrame::Pairing {
                pairing_id: "p".into(),
                label: "l".into(),
                scope: "read".into(),
                state: PairingState::Requested,
            },
            EventFrame::Sessions,
            EventFrame::SessionExit {
                session: "s".into(),
                code: None,
            },
            EventFrame::Lagged { missed: 1 },
        ];
        for frame in frames {
            assert!(FEED_KINDS.contains(&frame.kind()));
        }
    }

    #[test]
    fn feed_filter_defaults_to_every_frame() {
        let filter = FeedFilter::default();
        assert!(filter.matches(&EventFrame::Sessions));
        assert!(filter.matches(&EventFrame::PaneDelta {
            session: "s".into(),
            seq: 1,
            bytes: vec![],
        }));
    }

    #[test]
    fn feed_filter_narrows_kinds_and_sessions() {
        let filter = FeedFilter::parse(Some("activity,exit"), Some("s1"), false).unwrap();
        assert!(filter.matches(&EventFrame::Activity {
            json: r#"{"session":"s1","state":"working"}"#.into(),
        }));
        assert!(filter.matches(&EventFrame::SessionExit {
            session: "s1".into(),
            code: Some(0),
        }));
        assert!(!filter.matches(&EventFrame::SessionExit {
            session: "s2".into(),
            code: Some(0),
        }));
        assert!(!filter.matches(&EventFrame::Lease {
            session: "s1".into(),
            kind: LeaseEventKind::Opened,
            expires_at: None,
        }));
        // The greeting is part of every subscription, even when narrowed.
        assert!(filter.matches(&EventFrame::Hello(Hello {
            proto: PROTO_VERSION,
            server: "s".into(),
            scopes: vec![],
        })));
    }

    #[test]
    fn feed_filter_rejects_unknown_empty_and_oversized_inputs() {
        assert_eq!(
            FeedFilter::parse(Some("activty"), None, false),
            Err(FeedFilterError::UnknownKind("activty".into()))
        );
        assert_eq!(
            FeedFilter::parse(Some("activity,"), None, false),
            Err(FeedFilterError::EmptyKind)
        );
        assert_eq!(
            FeedFilter::parse(
                Some(
                    "activity,activity,activity,activity,activity,activity,activity,activity,activity,activity",
                ),
                None,
                false
            ),
            Err(FeedFilterError::TooManyKinds)
        );
        assert_eq!(
            FeedFilter::parse(None, Some(""), false),
            Err(FeedFilterError::EmptySession)
        );
        let long = "x".repeat(MAX_FEED_SESSION_LEN + 1);
        assert_eq!(
            FeedFilter::parse(None, Some(&long), false),
            Err(FeedFilterError::SessionTooLong)
        );
    }

    #[test]
    fn feed_filter_handles_unkeyed_frames_and_lag_opt_in() {
        let filter = FeedFilter::parse(Some("sessions"), Some("s"), true).unwrap();
        assert!(filter.signal_lag);
        assert!(filter.matches(&EventFrame::Sessions));
        assert!(!filter.matches(&EventFrame::Lagged { missed: 3 }));
    }
}
