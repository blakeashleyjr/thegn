//! Kitty graphics-protocol relay for the corner overlay pane.
//!
//! The pane emulator ([`crate::emulator`], `alacritty_terminal`) parses and
//! discards image escapes, so a child that draws with the kitty graphics protocol
//! (e.g. `mpv --vo=kitty`) shows nothing. This relay gives the corner pane — and
//! only the corner pane — crisp images by intercepting its kitty escapes and
//! re-emitting them to the OUTER terminal, offset to the corner's screen rect.
//!
//! Why this is tractable only for the corner: it is a single, fixed-geometry pane
//! that never scrolls and is normally on top, so placement is a constant offset
//! and the image lifecycle is a handful of events. The general "graphics for every
//! pane" case (scrollback, arbitrary occlusion, z-order) is out of scope — that
//! would need an image-capable emulator. See the Phase-2 plan.
//!
//! ## Captured `mpv --vo=kitty` choreography (Ghostty, 2026-06)
//!
//! Per video frame mpv emits, in order:
//! ```text
//!   APC  Ga=d                                    delete all images (clear prev)
//!   CSI  2J                                       clear screen  ─┐ text stream →
//!   CSI  0;0f                                      cursor home  ─┘ the emulator
//!   APC  Ga=T,f=24,s=W,v=H,C=1,q=2,m=1  + payload  transmit + DISPLAY (chunk 1)
//!   APC  Gm=1  + payload                  × N      continuation chunks
//!   APC  Gm=0  + payload                            final chunk
//! ```
//! mpv homes the cursor (a text escape the emulator processes), while the relay
//! binds outer placement to the corner content origin. `C=1` means "don't move
//! the cursor after", and
//! `q=2` suppresses responses. With `--vo=kitty` set explicitly mpv sends **no
//! `a=q` probe** — it just draws. Images are chunked far larger than one PTY read,
//! so APC sequences MUST be buffered across reads.
//!
//! ## What the relay does
//!
//! [`KittyRelay::feed_with`] splits a PTY chunk into [`Piece`]s: non-graphics bytes go
//! to the emulator (so its cursor tracks mpv's home); APC-`G` commands are pulled
//! out, validated, and rebuilt for the outer terminal. A DISPLAY command
//! (`a=T`/`a=p`) is prefixed with an absolute CUP to the corner origin ([`cup`])
//! and receives cell dimensions bounded by the content rect. Continuation APCs
//! are buffered into their transfer, then one ordered unit is emitted at `m=0`
//! with a single CUP. Child IDs are translated through a bounded reserved ID
//! block, and delete APCs are rebuilt with an explicit scope and quiet mode. An orphan `Gm=`
//! is rejected until a fresh `a=T` or `a=d`. A query (`a=q`) is answered locally
//! and never forwarded (forwarding it would make the OUTER terminal reply to us).
//!
//! The child acknowledgement contract is deliberately limited to local `a=q`
//! capability replies. Transmit and placement commands never receive synthetic
//! acknowledgements. Every command sent outward is rebuilt with `q=2`; child
//! quiet-mode choices cannot cause replies to enter the host input stream.
//! Images are scaled to the full content rect when the child omits `c`/`r`;
//! child cursor offsets are ignored to keep placement inside that rect.

const ESC: u8 = 0x1b;
const ST: &[u8] = b"\x1b\\"; // String Terminator (ESC \)

/// One classified slice of split PTY output.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Piece {
    /// Non-graphics bytes — feed to the pane emulator (and the normal query path).
    Emulator(Vec<u8>),
    /// An APC-`G` command that DISPLAYS (`a=T` / `a=p`): the caller prefixes a
    /// [`cup`] to the corner before forwarding to the outer terminal.
    GfxDisplay(Vec<u8>),
    /// Any other accepted APC-`G` (for example delete): no CUP.
    GfxOther(Vec<u8>),
    /// An APC-`G` query (`a=q`): write this reply back to the pane; never forward.
    GfxAnswer(Vec<u8>),
    /// A malformed or unsupported graphics command; the caller drops it.
    Rejected,
}

/// Merge the frame's queued graphics sequences into one OOB blob. A single
/// sequence (the common video case, up to several MiB) is moved through with
/// no copy; several are concatenated into one exactly-sized allocation.
pub(crate) fn coalesce_gfx(mut seqs: Vec<Vec<u8>>) -> Vec<u8> {
    if seqs.len() == 1 {
        return seqs.pop().unwrap_or_default();
    }
    let total = seqs.iter().map(Vec::len).sum();
    let mut blob = Vec::with_capacity(total);
    for g in &seqs {
        blob.extend_from_slice(g);
    }
    blob
}

/// Hard limit on a complete APC, including introducer and ST. Oversize APCs
/// are discarded through their terminator; their tail is never ordinary text.
const MAX_APC_BYTES: usize = 4 * 1024 * 1024;
/// Leave room for the CUP prefix added by `pty_drain` so an admitted logical
/// transfer always fits the writer's 4 MiB OOB message ceiling.
const MAX_TRANSFER_BYTES: usize = MAX_APC_BYTES - 64;
const MAX_TRANSFER_CHUNKS: usize = 4096;
/// Text is emitted incrementally even when the caller supplies a huge slice.
const TEXT_PIECE_BYTES: usize = 8192;
/// Reserved outer-terminal IDs used only by the corner relay.
pub const CORNER_ID_BASE: u32 = 0x7fff_0000;
pub const CORNER_IDS: usize = 16;

#[derive(Debug, Default)]
enum ScanState {
    #[default]
    Text,
    Escape,
    Apc {
        escape: bool,
    },
    Discard {
        escape: bool,
    },
}

/// Streaming corner parser. Each input byte is examined once for framing;
/// only completed APC control fields are parsed for classification.
#[derive(Debug, Default)]
pub struct KittyRelay {
    state: ScanState,
    partial: Vec<u8>,
    transfer: Option<Transfer>,
    ids: CornerIds,
    #[cfg(test)]
    scanned: usize,
}

/// Child IDs are reserved when a first chunk is classified. A reservation made
/// only for an incomplete transfer is released if that transfer is rejected.
#[derive(Debug, Default)]
struct CornerIds {
    child: [Option<u32>; CORNER_IDS],
    anonymous_live: bool,
}

impl CornerIds {
    fn host_id(slot: usize) -> u32 {
        CORNER_ID_BASE + slot as u32
    }

    fn slot_for(&self, child_id: u32) -> Option<usize> {
        self.child.iter().position(|id| *id == Some(child_id))
    }

    fn allocate(&mut self, child_id: u32) -> Option<(usize, bool)> {
        if let Some(slot) = self.slot_for(child_id) {
            return Some((slot, false));
        }
        let slot = self
            .child
            .iter()
            .enumerate()
            .skip(1)
            .find_map(|(slot, id)| id.is_none().then_some(slot))?;
        self.child[slot] = Some(child_id);
        Some((slot, true))
    }

    fn release(&mut self, reservation: Reservation) {
        if !reservation.fresh {
            return;
        }
        match reservation.id {
            ReservationId::Child(child_id) => {
                if let Some(slot) = self.slot_for(child_id) {
                    self.child[slot] = None;
                }
            }
            ReservationId::Anonymous => self.anonymous_live = false,
        }
    }

    fn delete_live(&mut self) -> Vec<u8> {
        let mut bytes = Vec::new();
        if self.anonymous_live {
            bytes.extend_from_slice(&delete_id(Self::host_id(0)));
        }
        for (slot, child_id) in self.child.iter_mut().enumerate() {
            if child_id.take().is_some() {
                bytes.extend_from_slice(&delete_id(Self::host_id(slot)));
            }
        }
        self.anonymous_live = false;
        bytes
    }
}

#[derive(Debug)]
struct Transfer {
    bytes: Vec<u8>,
    chunks: usize,
    content_size: (usize, usize),
    reservation: Option<Reservation>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ReservationId {
    Child(u32),
    Anonymous,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct Reservation {
    id: ReservationId,
    fresh: bool,
}

impl KittyRelay {
    pub fn new() -> Self {
        Self::default()
    }

    /// A new pane must never inherit the previous pane's partial sequence.
    pub fn reset(&mut self) {
        self.state = ScanState::Text;
        self.partial = Vec::new();
        self.transfer = None;
        self.ids = CornerIds::default();
    }

    /// Emit pieces as they complete instead of accumulating an input-sized
    /// output vector. At most one 4 MiB APC and one 8 KiB text slice are owned
    /// by the parser; the consumer owns each emitted piece immediately.
    #[cfg(test)]
    pub fn feed_with(&mut self, input: &[u8], emit: impl FnMut(Piece)) {
        self.feed_with_rect(input, (u16::MAX as usize, u16::MAX as usize), emit);
    }

    /// Feed bytes while constraining image placements to the corner content size.
    pub fn feed_with_rect(
        &mut self,
        input: &[u8],
        content_size: (usize, usize),
        mut emit: impl FnMut(Piece),
    ) {
        let mut text = Vec::new();
        for &byte in input {
            #[cfg(test)]
            {
                self.scanned += 1;
            }
            match self.state {
                ScanState::Text => {
                    if byte == ESC {
                        self.state = ScanState::Escape;
                    } else {
                        text.push(byte);
                    }
                }
                ScanState::Escape => {
                    if byte == b'_' {
                        if !text.is_empty() {
                            emit(Piece::Emulator(std::mem::take(&mut text)));
                        }
                        self.partial.extend_from_slice(b"\x1b_");
                        self.state = ScanState::Apc { escape: false };
                    } else {
                        text.push(ESC);
                        if text.len() == TEXT_PIECE_BYTES {
                            emit(Piece::Emulator(std::mem::take(&mut text)));
                        }
                        if byte == ESC {
                            self.state = ScanState::Escape;
                        } else {
                            text.push(byte);
                            self.state = ScanState::Text;
                        }
                    }
                }
                ScanState::Apc { escape } => {
                    let ended = escape && byte == b'\\';
                    if self.partial.len() == MAX_APC_BYTES {
                        // Release capacity too: a discarded oversized command
                        // does not retain its 4 MiB allocation indefinitely.
                        self.partial = Vec::new();
                        // An oversized/malformed APC inside a logical transfer
                        // invalidates the whole unit; later Gm chunks are orphans.
                        self.drop_transfer(None, &mut None);
                        self.state = if ended {
                            ScanState::Text
                        } else {
                            ScanState::Discard {
                                escape: byte == ESC,
                            }
                        };
                    } else {
                        self.partial.push(byte);
                        if ended {
                            let seq = std::mem::take(&mut self.partial);
                            let mut reservation = None;
                            let piece =
                                classify(seq, content_size, &mut self.ids, &mut reservation);
                            self.accept_piece(piece, content_size, reservation, &mut emit);
                            self.state = ScanState::Text;
                        } else {
                            self.state = ScanState::Apc {
                                escape: byte == ESC,
                            };
                        }
                    }
                }
                ScanState::Discard { escape } => {
                    self.state = if escape && byte == b'\\' {
                        ScanState::Text
                    } else {
                        ScanState::Discard {
                            escape: byte == ESC,
                        }
                    };
                }
            }
            if text.len() >= TEXT_PIECE_BYTES {
                emit(Piece::Emulator(std::mem::take(&mut text)));
            }
        }
        if !text.is_empty() {
            emit(Piece::Emulator(text));
        }
    }

    fn drop_transfer(
        &mut self,
        preserve: Option<Reservation>,
        inherited: &mut Option<Reservation>,
    ) {
        let Some(transfer) = self.transfer.take() else {
            return;
        };
        if let Some(reservation) = transfer.reservation {
            if preserve.is_some_and(|current| current.id == reservation.id) {
                *inherited = Some(reservation);
                return;
            }
            self.ids.release(reservation);
        }
    }

    fn accept_piece(
        &mut self,
        piece: Piece,
        content_size: (usize, usize),
        reservation: Option<Reservation>,
        emit: &mut impl FnMut(Piece),
    ) {
        match piece {
            Piece::GfxDisplay(bytes) if is_multi_transmit(&bytes) => {
                // A fresh transmit is an unambiguous boundary. It supersedes
                // any incomplete transfer and lets recovery start again.
                let mut inherited = None;
                self.drop_transfer(reservation, &mut inherited);
                let mut transfer = Transfer {
                    bytes: Vec::new(),
                    chunks: 1,
                    content_size,
                    reservation: reservation.filter(|slot| slot.fresh).or(inherited),
                };
                if append_transfer(&mut transfer, bytes) {
                    self.transfer = Some(transfer);
                } else {
                    self.transfer = Some(transfer);
                    self.drop_transfer(None, &mut None);
                    emit(Piece::Rejected);
                }
            }
            Piece::GfxOther(bytes) if is_continuation(&bytes) => {
                let final_chunk = has_multi(&bytes, b"0");
                let Some(mut transfer) = self.transfer.take() else {
                    emit(Piece::Rejected);
                    return;
                };
                if transfer.content_size != content_size {
                    self.transfer = Some(transfer);
                    self.drop_transfer(None, &mut None);
                    emit(Piece::Rejected);
                    return;
                }
                transfer.chunks += 1;
                if transfer.chunks > MAX_TRANSFER_CHUNKS || !append_transfer(&mut transfer, bytes) {
                    self.transfer = Some(transfer);
                    self.drop_transfer(None, &mut None);
                    emit(Piece::Rejected);
                    return;
                }
                if final_chunk {
                    if valid_transfer(&transfer.bytes) {
                        emit(Piece::GfxDisplay(transfer.bytes));
                    } else {
                        self.transfer = Some(transfer);
                        self.drop_transfer(None, &mut None);
                        emit(Piece::Rejected);
                    }
                } else {
                    self.transfer = Some(transfer);
                }
            }
            Piece::GfxOther(bytes) if bytes.is_empty() => {
                // A child delete-all with no live IDs still terminates any
                // incomplete transfer, but has no outer-terminal command.
                self.drop_transfer(None, &mut None);
            }
            Piece::GfxOther(bytes) if is_delete(&bytes) => {
                self.drop_transfer(None, &mut None);
                emit(Piece::GfxOther(bytes));
            }
            Piece::GfxDisplay(bytes) => {
                self.drop_transfer(reservation, &mut None);
                emit(Piece::GfxDisplay(bytes));
            }
            Piece::Rejected => {
                self.drop_transfer(None, &mut None);
                emit(Piece::Rejected);
            }
            other => emit(other),
        }
    }

    #[cfg(test)]
    fn feed(&mut self, input: &[u8]) -> Vec<Piece> {
        let mut pieces = Vec::new();
        self.feed_with(input, |piece| pieces.push(piece));
        pieces
    }
}

/// Classify a complete APC sequence (`ESC _ … ESC \`). Non-graphics APCs (not
/// `ESC _ G`) pass through to the emulator verbatim.
fn classify(
    seq: Vec<u8>,
    content_size: (usize, usize),
    ids: &mut CornerIds,
    reservation: &mut Option<Reservation>,
) -> Piece {
    // seq = ESC _ <body> ESC \  → body is seq[2 .. len-2].
    if seq.len() < 4 || seq[2] != b'G' {
        return Piece::Emulator(seq);
    }
    let body = &seq[2..seq.len() - 2]; // starts with 'G'
    // Control keys are up to the first ';' (payload separator), after the leading 'G'.
    let ctrl_end = body.iter().position(|&b| b == b';').unwrap_or(body.len());
    let ctrl = &body[1..ctrl_end];
    let payload = if ctrl_end < body.len() {
        let payload_start = ctrl_end + 3;
        &seq[payload_start..seq.len() - 2]
    } else {
        &[]
    };
    match canonical_graphics(ctrl, payload, content_size, ids, reservation) {
        Some(Canonical::Query) => Piece::GfxAnswer(answer_for(ctrl)),
        Some(Canonical::Display(bytes)) => Piece::GfxDisplay(bytes),
        Some(Canonical::Other(bytes)) => Piece::GfxOther(bytes),
        None => Piece::Rejected,
    }
}

fn append_transfer(transfer: &mut Transfer, bytes: Vec<u8>) -> bool {
    let Some(total) = transfer.bytes.len().checked_add(bytes.len()) else {
        return false;
    };
    if total > MAX_TRANSFER_BYTES {
        return false;
    }
    transfer.bytes.extend_from_slice(&bytes);
    true
}

fn control_and_payload(seq: &[u8]) -> Option<(&[u8], &[u8])> {
    if !seq.starts_with(b"\x1b_G") || !seq.ends_with(ST) || seq.len() < 5 {
        return None;
    }
    let body = &seq[2..seq.len() - 2];
    let separator = body.iter().position(|&byte| byte == b';')?;
    Some((&body[1..separator], &body[separator + 1..]))
}

fn has_multi(seq: &[u8], value: &[u8]) -> bool {
    control_and_payload(seq)
        .and_then(|(ctrl, _)| Control::parse(ctrl))
        .and_then(|control| control.get(b"m").map(|found| found == value))
        .unwrap_or(false)
}

fn is_multi_transmit(seq: &[u8]) -> bool {
    control_and_payload(seq)
        .and_then(|(ctrl, _)| Control::parse(ctrl))
        .is_some_and(|control| {
            control.get(b"a").is_some_and(|action| action == b"T")
                && control.get(b"m") == Some(b"1".as_slice())
        })
}

fn is_continuation(seq: &[u8]) -> bool {
    control_and_payload(seq)
        .and_then(|(ctrl, _)| Control::parse(ctrl))
        .is_some_and(|control| control.get(b"a").is_none() && control.get(b"m").is_some())
}

fn is_delete(seq: &[u8]) -> bool {
    control_and_payload(seq)
        .and_then(|(ctrl, _)| Control::parse(ctrl))
        .is_some_and(|control| control.get(b"a") == Some(b"d".as_slice()))
}

/// Validate the reassembled payload once at the semantic boundary. The input
/// is capped by `MAX_TRANSFER_BYTES`; this second bounded buffer is at most the
/// encoded payload size and decoding is capped by `MAX_DECODED`.
fn valid_transfer(bytes: &[u8]) -> bool {
    let mut cursor = 0;
    let mut encoded = Vec::new();
    let mut first_control = None;
    while cursor < bytes.len() {
        let mut end = cursor;
        while end + ST.len() <= bytes.len() && &bytes[end..end + ST.len()] != ST {
            end += 1;
        }
        if end + ST.len() > bytes.len() {
            return false;
        }
        end += ST.len();
        let Some((control, payload)) = control_and_payload(&bytes[cursor..end]) else {
            return false;
        };
        if first_control.is_none() {
            first_control = Control::parse(control);
        }
        if encoded.len().saturating_add(payload.len()) > MAX_TRANSFER_BYTES {
            return false;
        }
        encoded.extend_from_slice(payload);
        cursor = end;
    }
    let Some(control) = first_control else {
        return false;
    };
    let Some(format) = control.get(b"f") else {
        return false;
    };
    let Some(width) = control.get(b"s").and_then(parse_positive) else {
        return false;
    };
    let Some(height) = control.get(b"v").and_then(parse_positive) else {
        return false;
    };
    let Some(pixels) = width.checked_mul(height) else {
        return false;
    };
    if pixels > MAX_DECODED {
        return false;
    }
    let Some(decoded) = decode_base64(&encoded) else {
        return false;
    };
    match format {
        b"24" => pixels.checked_mul(3) == Some(decoded.len()),
        b"32" => pixels.checked_mul(4) == Some(decoded.len()),
        b"100" => valid_png(&decoded, width, height),
        _ => false,
    }
}

enum Canonical {
    Query,
    Display(Vec<u8>),
    Other(Vec<u8>),
}

const MAX_DECODED: usize = 16 * 1024 * 1024;

struct Control<'a> {
    fields: Vec<(&'a [u8], &'a [u8])>,
}

impl<'a> Control<'a> {
    fn parse(bytes: &'a [u8]) -> Option<Self> {
        if bytes.is_empty() {
            return None;
        }
        let mut fields = Vec::new();
        for field in bytes.split(|&b| b == b',') {
            let equals = field.iter().position(|&b| b == b'=')?;
            let (key, rest) = field.split_at(equals);
            let value = &rest[1..];
            if key.is_empty() || value.is_empty() || fields.iter().any(|(old, _)| *old == key) {
                return None;
            }
            fields.push((key, value));
        }
        Some(Self { fields })
    }

    fn get(&self, key: &[u8]) -> Option<&'a [u8]> {
        self.fields
            .iter()
            .find(|(field, _)| *field == key)
            .map(|(_, value)| *value)
    }
}

/// Parse controls into a bounded map, validate supported semantics and payload,
/// then construct a fresh APC. Unknown controls are rejected rather than passed
/// through to the outer terminal.
fn canonical_graphics(
    ctrl: &[u8],
    payload: &[u8],
    content_size: (usize, usize),
    ids: &mut CornerIds,
    reservation: &mut Option<Reservation>,
) -> Option<Canonical> {
    let control = Control::parse(ctrl)?;
    let get = |key: &[u8]| control.get(key);
    let action = get(b"a").unwrap_or(b"");
    if get(b"i").is_some_and(|id| parse_u32_positive(id).is_none()) {
        return None;
    }
    if action == b"q" {
        if get(b"q").is_some_and(|q| !matches!(q, b"0" | b"1" | b"2")) {
            return None;
        }
        if control
            .fields
            .iter()
            .any(|(k, _)| !matches!(*k, b"a" | b"i" | b"s" | b"v" | b"t" | b"f"))
        {
            return None;
        }
        if get(b"t").is_some_and(|t| t != b"d")
            || get(b"f").is_some_and(|f| !matches!(f, b"24" | b"32" | b"100"))
            || get(b"s").is_some_and(|v| parse_positive(v).is_none())
            || get(b"v").is_some_and(|v| parse_positive(v).is_none())
        {
            return None;
        }
        return Some(Canonical::Query);
    }
    if get(b"q").is_some_and(|q| !matches!(q, b"0" | b"1" | b"2"))
        || get(b"C").is_some_and(|c| !matches!(c, b"0" | b"1"))
        || get(b"z").is_some_and(|z| parse_signed(z).is_none())
    {
        return None;
    }
    if control.fields.iter().any(|(k, _)| {
        !matches!(
            *k,
            b"a" | b"i"
                | b"f"
                | b"s"
                | b"v"
                | b"t"
                | b"m"
                | b"c"
                | b"r"
                | b"x"
                | b"y"
                | b"w"
                | b"h"
                | b"X"
                | b"Y"
                | b"z"
                | b"C"
                | b"q"
        )
    }) {
        return None;
    }
    if get(b"t").is_some_and(|t| t != b"d")
        || get(b"o").is_some()
        || get(b"U") == Some(b"1")
        || get(b"P").is_some()
        || get(b"Q").is_some()
        || matches!(action, b"f" | b"a" | b"c")
    {
        return None;
    }
    let multi = get(b"m").unwrap_or(b"0");
    if multi != b"0" && multi != b"1" {
        return None;
    }
    let continuation = action.is_empty();
    if continuation {
        if get(b"m").is_none() || get(b"a").is_some() {
            return None;
        }
        if control
            .fields
            .iter()
            .any(|(key, _)| !matches!(*key, b"m" | b"q"))
        {
            return None;
        }
        if !valid_base64_alphabet(payload) {
            return None;
        }
    } else if !matches!(action, b"T" | b"p" | b"d") {
        return None;
    }
    if action == b"d" {
        if control
            .fields
            .iter()
            .any(|(key, _)| !matches!(*key, b"a" | b"i" | b"q"))
        {
            return None;
        }
        if !payload.is_empty() {
            return None;
        }
        let bytes = if let Some(child_id) = get(b"i") {
            let child_id = parse_u32_positive(child_id)?;
            let slot = ids.slot_for(child_id)?;
            ids.child[slot] = None;
            delete_id(CornerIds::host_id(slot))
        } else {
            ids.delete_live()
        };
        return Some(Canonical::Other(bytes));
    } else if action == b"p" {
        if control.fields.iter().any(|(key, _)| {
            !matches!(
                *key,
                b"a" | b"i"
                    | b"s"
                    | b"v"
                    | b"t"
                    | b"c"
                    | b"r"
                    | b"x"
                    | b"y"
                    | b"w"
                    | b"h"
                    | b"X"
                    | b"Y"
                    | b"z"
                    | b"C"
                    | b"q"
            )
        }) {
            return None;
        }
        if !payload.is_empty() || get(b"i").is_none() || multi != b"0" {
            return None;
        }
        if get(b"s").is_some() != get(b"v").is_some() {
            return None;
        }
        if let (Some(width), Some(height)) = (get(b"s"), get(b"v")) {
            let width = parse_positive(width)?;
            let height = parse_positive(height)?;
            let x = parse_num(get(b"x").unwrap_or(b"0"))?;
            let y = parse_num(get(b"y").unwrap_or(b"0"))?;
            let w = parse_num(get(b"w").unwrap_or(b"0"))?;
            let h = parse_num(get(b"h").unwrap_or(b"0"))?;
            if x > width
                || y > height
                || (w != 0 && x.checked_add(w)? > width)
                || (h != 0 && y.checked_add(h)? > height)
            {
                return None;
            }
        } else if [b"x".as_slice(), b"y", b"w", b"h"]
            .iter()
            .any(|key| get(key).is_some())
        {
            return None;
        }
        if parse_num(get(b"X").unwrap_or(b"0"))? != 0 || parse_num(get(b"Y").unwrap_or(b"0"))? != 0
        {
            return None;
        }
    } else if action == b"T" {
        let format = get(b"f")?;
        let width = parse_positive(get(b"s")?)?;
        let height = parse_positive(get(b"v")?)?;
        let pixels = width.checked_mul(height)?;
        if pixels > MAX_DECODED {
            return None;
        }
        let crop_x = parse_num(get(b"x").unwrap_or(b"0"))?;
        let crop_y = parse_num(get(b"y").unwrap_or(b"0"))?;
        let crop_w = parse_num(get(b"w").unwrap_or(b"0"))?;
        let crop_h = parse_num(get(b"h").unwrap_or(b"0"))?;
        if crop_x > width
            || crop_y > height
            || (crop_w != 0 && crop_x.checked_add(crop_w)? > width)
            || (crop_h != 0 && crop_y.checked_add(crop_h)? > height)
        {
            return None;
        }
        if parse_num(get(b"X").unwrap_or(b"0"))? != 0 || parse_num(get(b"Y").unwrap_or(b"0"))? != 0
        {
            return None;
        }
        if multi == b"0" {
            let decoded = decode_base64(payload)?;
            match format {
                b"24" if decoded.len() == pixels.checked_mul(3)? => {}
                b"32" if decoded.len() == pixels.checked_mul(4)? => {}
                b"100" if valid_png(&decoded, width, height) => {}
                _ => return None,
            }
        } else if !valid_base64_alphabet(payload) {
            return None;
        }
    }
    let (cols, rows) = content_size;
    if cols == 0 || rows == 0 {
        return None;
    }
    let cell_cols = parse_num(get(b"c").unwrap_or(b"0"))?;
    let cell_rows = parse_num(get(b"r").unwrap_or(b"0"))?;
    if cell_cols > cols || cell_rows > rows {
        return None;
    }
    let mapped_id = if action == b"T" {
        if let Some(child_id) = get(b"i") {
            let child_id = parse_u32_positive(child_id)?;
            let (slot, fresh) = ids.allocate(child_id)?;
            *reservation = Some(Reservation {
                id: ReservationId::Child(child_id),
                fresh,
            });
            Some(CornerIds::host_id(slot))
        } else {
            let fresh = !ids.anonymous_live;
            ids.anonymous_live = true;
            *reservation = Some(Reservation {
                id: ReservationId::Anonymous,
                fresh,
            });
            Some(CornerIds::host_id(0))
        }
    } else if action == b"p" {
        let child_id = parse_u32_positive(get(b"i")?)?;
        Some(CornerIds::host_id(ids.slot_for(child_id)?))
    } else {
        None
    };
    let mut out = Vec::from(&b"\x1b_G"[..]);
    let mut emitted = false;
    for (key, value) in &control.fields {
        if emitted {
            out.push(b',');
        }
        emitted = true;
        out.extend_from_slice(key);
        out.push(b'=');
        match *key {
            b"q" => out.extend_from_slice(b"2"),
            b"C" => out.extend_from_slice(b"1"),
            b"z" => out.extend_from_slice(b"-1"),
            b"c" => {
                let cols_s = cols.to_string();
                out.extend_from_slice(if cell_cols == 0 {
                    cols_s.as_bytes()
                } else {
                    value
                });
            }
            b"r" => {
                let rows_s = rows.to_string();
                out.extend_from_slice(if cell_rows == 0 {
                    rows_s.as_bytes()
                } else {
                    value
                });
            }
            b"i" => out.extend_from_slice(mapped_id?.to_string().as_bytes()),
            b"s" | b"v" | b"m" | b"x" | b"y" | b"w" | b"h" | b"X" | b"Y" => {
                out.extend_from_slice(parse_num(value)?.to_string().as_bytes())
            }
            _ => out.extend_from_slice(value),
        }
    }
    let display = matches!(action, b"T" | b"p");
    if display && get(b"i").is_none() {
        if emitted {
            out.push(b',');
        }
        emitted = true;
        out.extend_from_slice(b"i=");
        out.extend_from_slice(mapped_id?.to_string().as_bytes());
    }
    for (key, value) in [(b"q".as_slice(), b"2".as_slice())]
        .into_iter()
        .chain(display.then_some((b"C".as_slice(), b"1".as_slice())))
        .chain(display.then_some((b"z".as_slice(), b"-1".as_slice())))
    {
        if get(key).is_none() {
            if emitted {
                out.push(b',');
            }
            emitted = true;
            out.extend_from_slice(key);
            out.push(b'=');
            out.extend_from_slice(value);
        }
    }
    if display && get(b"c").is_none() {
        out.extend_from_slice(format!(",c={cols}").as_bytes());
    }
    if display && get(b"r").is_none() {
        out.extend_from_slice(format!(",r={rows}").as_bytes());
    }
    out.push(b';');
    out.extend_from_slice(payload);
    out.extend_from_slice(ST);
    Some(if matches!(action, b"T" | b"p") {
        Canonical::Display(out)
    } else {
        Canonical::Other(out)
    })
}

fn parse_num(value: &[u8]) -> Option<usize> {
    if value.is_empty() || !value.iter().all(u8::is_ascii_digit) {
        return None;
    }
    std::str::from_utf8(value).ok()?.parse().ok()
}
fn parse_positive(value: &[u8]) -> Option<usize> {
    let n = parse_num(value)?;
    (n > 0).then_some(n)
}
fn parse_u32_positive(value: &[u8]) -> Option<u32> {
    let n = u32::try_from(parse_positive(value)?).ok()?;
    (n > 0).then_some(n)
}
fn parse_signed(value: &[u8]) -> Option<i64> {
    std::str::from_utf8(value).ok()?.parse().ok()
}
fn valid_base64_alphabet(bytes: &[u8]) -> bool {
    bytes
        .iter()
        .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'+' | b'/' | b'='))
}
fn decode_base64(bytes: &[u8]) -> Option<Vec<u8>> {
    if bytes.len() > MAX_APC_BYTES || !bytes.len().is_multiple_of(4) {
        return None;
    }
    let mut out = Vec::with_capacity(bytes.len().saturating_mul(3) / 4);
    for (index, chunk) in bytes.chunks_exact(4).enumerate() {
        let val = |b: u8| -> Option<u8> {
            match b {
                b'A'..=b'Z' => Some(b - b'A'),
                b'a'..=b'z' => Some(b - b'a' + 26),
                b'0'..=b'9' => Some(b - b'0' + 52),
                b'+' => Some(62),
                b'/' => Some(63),
                _ => None,
            }
        };
        let a = val(chunk[0])?;
        let b = val(chunk[1])?;
        let c = if chunk[2] == b'=' { 0 } else { val(chunk[2])? };
        let d = if chunk[3] == b'=' { 0 } else { val(chunk[3])? };
        if chunk[2] == b'=' && chunk[3] != b'='
            || (chunk[2] == b'=' && b & 0x0f != 0)
            || (chunk[3] == b'=' && chunk[2] != b'=' && c & 0x03 != 0)
            || (chunk[2..].contains(&b'=') && index + 1 != bytes.len() / 4)
        {
            return None;
        }
        out.push((a << 2) | (b >> 4));
        if chunk[2] != b'=' {
            out.push((b << 4) | (c >> 2));
        }
        if chunk[3] != b'=' {
            out.push((c << 6) | d);
        }
        if out.len() > MAX_DECODED {
            return None;
        }
    }
    Some(out)
}
fn valid_png(bytes: &[u8], width: usize, height: usize) -> bool {
    bytes.len() >= 24
        && bytes.starts_with(b"\x89PNG\r\n\x1a\n")
        && bytes[8..12] == 13u32.to_be_bytes()
        && &bytes[12..16] == b"IHDR"
        && u32::from_be_bytes([bytes[16], bytes[17], bytes[18], bytes[19]]) as usize == width
        && u32::from_be_bytes([bytes[20], bytes[21], bytes[22], bytes[23]]) as usize == height
}

/// Extract the value of a `key=` (e.g. `a=`, `i=`) from comma-separated control
/// data. Returns the raw value slice up to the next comma.
fn key_value<'a>(ctrl: &'a [u8], key: &[u8]) -> Option<&'a [u8]> {
    let mut i = 0;
    while i < ctrl.len() {
        // Each field is `k=v` separated by commas.
        let end = ctrl[i..]
            .iter()
            .position(|&b| b == b',')
            .map(|p| i + p)
            .unwrap_or(ctrl.len());
        let field = &ctrl[i..end];
        if let Some(rest) = field.strip_prefix(key) {
            return Some(rest);
        }
        i = end + 1;
    }
    None
}

/// Build the local reply to an `a=q` capability query: `ESC _ G i=<id>;OK ESC \`
/// (echoing the queried image id when present). This tells the child "graphics
/// supported" without bothering the outer terminal.
fn answer_for(ctrl: &[u8]) -> Vec<u8> {
    let mut out = Vec::from(&b"\x1b_G"[..]);
    if let Some(id) = key_value(ctrl, b"i=") {
        out.extend_from_slice(b"i=");
        out.extend_from_slice(id);
    }
    out.extend_from_slice(b";OK");
    out.extend_from_slice(ST);
    out
}

/// Absolute cursor-position (1-based CUP) that places the corner image: the corner
/// content rect's top-left (`origin` = `(row, col)`, 0-based screen cells) plus the
/// pane emulator's current cursor (`cursor` = `(row, col)`, 0-based).
pub fn cup(origin: (u16, u16), cursor: (u16, u16)) -> Vec<u8> {
    let row = origin.0 as usize + cursor.0 as usize + 1;
    let col = origin.1 as usize + cursor.1 as usize + 1;
    format!("\x1b[{row};{col}H").into_bytes()
}

/// Delete only the reserved corner ID block on the outer terminal.
/// Emitted on dismiss/exit/resize/occlude so no corner frame lingers.
pub fn delete_all() -> Vec<u8> {
    (0..CORNER_IDS).fold(Vec::new(), |mut bytes, slot| {
        bytes.extend_from_slice(&delete_id(CornerIds::host_id(slot)));
        bytes
    })
}

fn delete_id(host_id: u32) -> Vec<u8> {
    format!("\x1b_Ga=d,d=I,i={host_id},q=2;\x1b\\").into_bytes()
}

/// Whether the outer terminal speaks the kitty graphics protocol, from the
/// environment. Pure over the inputs so it is unit-testable.
fn detect_kitty_graphics(
    term: Option<&str>,
    term_program: Option<&str>,
    kitty_window_id: bool,
) -> bool {
    if kitty_window_id {
        return true;
    }
    let needles = ["kitty", "ghostty", "wezterm"];
    let hit = |s: Option<&str>| {
        s.map(|v| v.to_ascii_lowercase())
            .is_some_and(|v| needles.iter().any(|n| v.contains(n)))
    };
    hit(term) || hit(term_program)
}

/// Runtime check of the outer terminal's kitty-graphics support (reads env once).
pub fn outer_supports_kitty_graphics() -> bool {
    detect_kitty_graphics(
        std::env::var("TERM").ok().as_deref(),
        std::env::var("TERM_PROGRAM").ok().as_deref(),
        std::env::var_os("KITTY_WINDOW_ID").is_some(),
    )
}

#[cfg(test)]
mod tests {
    #[test]
    fn coalesce_gfx_moves_single_and_sizes_multi_exactly() {
        let one = vec![1u8, 2, 3];
        let ptr = one.as_ptr();
        let out = super::coalesce_gfx(vec![one]);
        assert_eq!(out.as_ptr(), ptr, "single sequence must not be copied");
        let out = super::coalesce_gfx(vec![vec![1, 2], vec![3], vec![4, 5]]);
        assert_eq!(out, [1, 2, 3, 4, 5]);
        assert_eq!(out.capacity(), 5, "one exactly-sized allocation");
        assert!(super::coalesce_gfx(Vec::new()).is_empty());
    }

    use super::*;

    fn apc(body: &str) -> Vec<u8> {
        let mut v = Vec::from(&b"\x1b_"[..]);
        v.extend_from_slice(body.as_bytes());
        v.extend_from_slice(ST);
        v
    }

    #[test]
    fn every_byte_split_preserves_st_and_non_graphics_text() {
        let seq = b"hello\x1b_Ga=T,f=24,s=1,v=1;AQID\x1b\\tail\x1b[31m";
        for split in 0..=seq.len() {
            let mut relay = KittyRelay::new();
            let mut got = relay.feed(&seq[..split]);
            got.extend(relay.feed(&seq[split..]));
            let mut wire = Vec::new();
            for piece in got {
                match piece {
                    Piece::Emulator(b) | Piece::GfxDisplay(b) => wire.extend(b),
                    other => panic!("unexpected {other:?}"),
                }
            }
            assert!(
                wire.starts_with(
                    b"hello\x1b_Ga=T,f=24,s=1,v=1,i=2147418112,q=2,C=1,z=-1,c=65535,r=65535;AQID\x1b\\tail"
                ),
                "split {split}"
            );
            assert_eq!(relay.scanned, seq.len());
        }
    }

    #[test]
    fn near_cap_bytewise_input_has_linear_scan_work_and_bounded_transfer_state() {
        let mut seq = b"\x1b_Ga=T,m=1,f=24,s=1,v=1;".to_vec();
        seq.resize(MAX_TRANSFER_BYTES - 128, b'A');
        seq.extend_from_slice(ST);
        for chunk in [1, 17, 8192, MAX_APC_BYTES] {
            let mut relay = KittyRelay::new();
            let mut count = 0;
            for bytes in seq.chunks(chunk) {
                relay.feed_with(bytes, |_| count += 1);
                assert!(relay.partial.len() <= MAX_APC_BYTES);
                assert!(relay.partial.capacity() <= MAX_APC_BYTES);
            }
            assert_eq!(count, 0, "m=1 is retained until its semantic boundary");
            assert!(relay.transfer.is_some());
            // Framing scans every byte once. Classification walks only the
            // complete control prefix and never revisits partial input.
            assert_eq!(relay.scanned, seq.len());
        }
    }

    #[test]
    fn near_apc_cap_single_chunk_is_classified() {
        let mut sequence = b"\x1b_Ga=T,f=24,s=1,v=1;".to_vec();
        sequence.resize(MAX_APC_BYTES - 4, b'A');
        sequence.extend_from_slice(ST);
        assert_eq!(sequence.len(), MAX_APC_BYTES - 2);

        let mut relay = KittyRelay::new();
        assert_eq!(relay.feed(&sequence), [Piece::Rejected]);
        assert_eq!(relay.scanned, sequence.len());
        assert!(relay.partial.is_empty());
        assert!(matches!(relay.state, ScanState::Text));
    }

    #[test]
    fn oversized_apc_discards_through_split_st_then_recovers() {
        for graphics in [true, false] {
            let mut relay = KittyRelay::new();
            relay.feed_with(if graphics { b"\x1b_G" } else { b"\x1b_X" }, |_| panic!());
            for _ in 0..=MAX_APC_BYTES / 8192 {
                relay.feed_with(&[b'x'; 8192], |_| panic!());
            }
            assert!(relay.partial.is_empty());
            relay.feed_with(b"still discarded\x1b", |_| panic!());
            assert_eq!(
                relay.feed(b"\\safe"),
                vec![Piece::Emulator(b"safe".to_vec())]
            );
            assert!(relay.feed(&apc("Ga=d")).is_empty());
        }
    }

    #[test]
    fn reset_drops_partial_and_discard_state() {
        let mut relay = KittyRelay::new();
        assert!(relay.feed(b"\x1b_Ga=T;old").is_empty());
        relay.reset();
        assert_eq!(relay.feed(b"new"), vec![Piece::Emulator(b"new".to_vec())]);
        relay.feed_with(b"\x1b_", |_| panic!());
        relay.feed_with(&vec![b'x'; MAX_APC_BYTES], |_| panic!());
        relay.reset();
        assert_eq!(relay.feed(b"new"), vec![Piece::Emulator(b"new".to_vec())]);
    }

    #[test]
    fn escape_pair_at_text_boundary_does_not_grow_output_past_limit() {
        let mut relay = KittyRelay::new();
        let mut input = vec![b'x'; TEXT_PIECE_BYTES - 1];
        input.extend_from_slice(b"\x1b[");
        let mut output = Vec::new();
        relay.feed_with(&input, |piece| {
            let Piece::Emulator(bytes) = piece else {
                panic!()
            };
            assert!(bytes.len() <= TEXT_PIECE_BYTES);
            assert!(bytes.capacity() <= TEXT_PIECE_BYTES);
            output.extend(bytes);
        });
        assert_eq!(output, input);
    }

    #[test]
    fn huge_single_feed_emits_bounded_text_pieces_without_retained_output() {
        let mut relay = KittyRelay::new();
        let input = vec![b'x'; MAX_APC_BYTES * 2];
        let mut total = 0;
        relay.feed_with(&input, |piece| {
            let Piece::Emulator(bytes) = piece else {
                panic!()
            };
            assert!(bytes.len() <= TEXT_PIECE_BYTES);
            total += bytes.len();
        });
        assert_eq!(total, input.len());
        assert!(relay.partial.is_empty());
    }

    #[test]
    fn splits_text_and_graphics_in_order() {
        let mut r = KittyRelay::new();
        // mpv's per-frame shape: delete, clear+home (text), transmit+display, chunks.
        let mut stream = apc("Ga=d");
        stream.extend_from_slice(b"\x1b[2J\x1b[0;0f");
        stream.extend(apc("Ga=T,f=24,s=1,v=1,C=1,q=2,m=1;AQ"));
        stream.extend(apc("Gm=0;ID"));
        let pieces = r.feed(&stream);
        assert_eq!(
            pieces,
            vec![
                Piece::Emulator(b"\x1b[2J\x1b[0;0f".to_vec()),
                Piece::GfxDisplay({
                    let mut unit =
                        apc("Ga=T,f=24,s=1,v=1,C=1,q=2,m=1,i=2147418112,z=-1,c=65535,r=65535;AQ");
                    unit.extend(apc("Gm=0,q=2;ID"));
                    unit
                }),
            ]
        );
    }

    #[test]
    fn buffers_apc_split_across_feeds() {
        let mut r = KittyRelay::new();
        let full = apc("Ga=T,m=1,f=24,s=1,v=1;PAYLOAD");
        let (head, tail) = full.split_at(10); // cut mid-sequence
        let first = r.feed(head);
        assert!(first.is_empty(), "incomplete APC yields nothing yet");
        let second = r.feed(tail);
        assert!(second.is_empty(), "m=1 remains buffered across feeds");
    }

    #[test]
    fn separate_read_transfer_is_emitted_as_one_ordered_display_unit() {
        let mut relay = KittyRelay::new();
        let mut got = relay.feed(&apc("Ga=T,m=1,f=24,s=1,v=1;AQ"));
        assert!(got.is_empty());
        got.extend(relay.feed(&apc("Gm=0;ID")));
        let mut expected =
            apc("Ga=T,m=1,f=24,s=1,v=1,i=2147418112,q=2,C=1,z=-1,c=65535,r=65535;AQ");
        expected.extend(apc("Gm=0,q=2;ID"));
        assert_eq!(got, [Piece::GfxDisplay(expected)]);
    }

    #[test]
    fn orphan_continuations_are_rejected_until_a_fresh_transmit_boundary() {
        let mut relay = KittyRelay::new();
        for continuation in ["Gm=0;AQID", "Gm=1;AQID"] {
            let pieces = relay.feed(&apc(continuation));
            assert_eq!(pieces, [Piece::Rejected]);
            assert!(!pieces.iter().any(|piece| matches!(piece, Piece::GfxDisplay(b) | Piece::GfxOther(b) if b.starts_with(b"\x1b_Gm="))));
        }
        assert_eq!(relay.feed(&apc("Ga=T,m=1,f=24,s=1,v=1;AQ")), []);
        assert_eq!(relay.feed(&apc("Gm=0;ID")).len(), 1);
    }

    #[test]
    fn explicit_transmit_only_is_rejected_and_its_continuations_are_orphans() {
        let mut relay = KittyRelay::new();
        assert_eq!(relay.feed(&apc("Gm=0;AQID")), [Piece::Rejected]);
        assert_eq!(
            relay.feed(&apc("Ga=t,f=24,s=1,v=1;AQID")),
            [Piece::Rejected]
        );
        assert_eq!(
            relay.feed(&apc("Ga=t,m=1,f=24,s=1,v=1;AQ")),
            [Piece::Rejected]
        );
        assert_eq!(relay.feed(&apc("Gm=0;ID")), [Piece::Rejected]);
    }

    #[test]
    fn transfer_byte_and_chunk_limits_discard_and_recover_at_transmit() {
        let mut relay = KittyRelay::new();
        assert!(relay.feed(&apc("Ga=T,m=1,f=24,s=1,v=1;AQ")).is_empty());
        let oversized = apc(&format!("Gm=1;{}", "A".repeat(MAX_TRANSFER_BYTES - 32)));
        assert_eq!(relay.feed(&oversized), [Piece::Rejected]);
        assert_eq!(relay.feed(&apc("Gm=0;ID")), [Piece::Rejected]);
        let complete = relay.feed(&apc("Ga=T,f=24,s=1,v=1;AQID"));
        assert!(matches!(complete.as_slice(), [Piece::GfxDisplay(_)]));

        let mut relay = KittyRelay::new();
        assert!(relay.feed(&apc("Ga=T,m=1,f=24,s=1,v=1;AQ")).is_empty());
        for _ in 0..MAX_TRANSFER_CHUNKS - 1 {
            assert!(relay.feed(&apc("Gm=1;AA")).is_empty());
        }
        assert_eq!(relay.feed(&apc("Gm=1;AA")), [Piece::Rejected]);
        assert_eq!(relay.feed(&apc("Gm=0;ID")), [Piece::Rejected]);
        assert!(matches!(
            relay.feed(&apc("Ga=T,f=24,s=1,v=1;AQID")).as_slice(),
            [Piece::GfxDisplay(_)]
        ));
    }

    #[test]
    fn reset_discards_an_in_progress_logical_transfer() {
        let mut relay = KittyRelay::new();
        assert!(relay.feed(&apc("Ga=T,m=1,f=24,s=1,v=1;AQ")).is_empty());
        relay.reset();
        assert_eq!(relay.feed(&apc("Gm=0;ID")), [Piece::Rejected]);
    }

    #[test]
    fn multi_megabyte_transfer_spans_many_event_loop_feeds() {
        let mut relay = KittyRelay::new();
        // 2 MiB of base64 is a 1.5 MiB RGB payload, split at 64 KiB.
        let encoded_len = 2 * 1024 * 1024;
        let encoded = vec![b'A'; encoded_len];
        let chunk_size = 64 * 1024;
        let height = encoded_len / 4;
        let start_len = chunk_size;
        let mut first = format!("Ga=T,m=1,f=24,s=1,v={height};").into_bytes();
        first.extend_from_slice(&encoded[..start_len]);
        let mut first_apc = b"\x1b_".to_vec();
        first_apc.extend_from_slice(&first);
        first_apc.extend_from_slice(ST);
        assert!(relay.feed(&first_apc).is_empty());
        for chunk in encoded[start_len..].chunks(chunk_size) {
            let final_chunk = chunk.as_ptr() == encoded[encoded_len - chunk_size..].as_ptr();
            let mut apc_bytes = if final_chunk {
                b"\x1b_Gm=0;".to_vec()
            } else {
                b"\x1b_Gm=1;".to_vec()
            };
            apc_bytes.extend_from_slice(chunk);
            apc_bytes.extend_from_slice(ST);
            let pieces = relay.feed(&apc_bytes);
            if final_chunk {
                assert!(
                    matches!(pieces.as_slice(), [Piece::GfxDisplay(bytes)] if bytes.len() > 2 * 1024 * 1024)
                );
            } else {
                assert!(pieces.is_empty());
            }
        }
    }

    #[test]
    fn invalid_aggregated_rgb_length_is_rejected() {
        let mut relay = KittyRelay::new();
        assert!(relay.feed(&apc("Ga=T,m=1,f=24,s=1,v=1;AQ")).is_empty());
        assert_eq!(relay.feed(&apc("Gm=0;I")), [Piece::Rejected]);
    }

    #[test]
    fn rejected_transfers_release_fresh_id_slots() {
        let mut relay = KittyRelay::new();
        for child_id in 1..=15 {
            let first = format!("Ga=T,i={child_id},m=1,f=24,s=1,v=1;AQ");
            assert!(relay.feed(&apc(&first)).is_empty());
            assert_eq!(relay.feed(&apc("Gm=0;I")), [Piece::Rejected]);
        }

        assert!(matches!(
            relay.feed(&apc("Ga=T,i=999,f=24,s=1,v=1;AQID")).as_slice(),
            [Piece::GfxDisplay(_)]
        ));
    }

    #[test]
    fn trailing_lone_esc_is_buffered_then_completed() {
        let mut r = KittyRelay::new();
        // Text, then a lone ESC at the very end of the read.
        let mut chunk = b"hello".to_vec();
        chunk.push(ESC);
        let p1 = r.feed(&chunk);
        assert_eq!(p1, vec![Piece::Emulator(b"hello".to_vec())]);
        // Next read completes an APC begun by that ESC.
        let p2 = r.feed(b"_Ga=d\x1b\\");
        assert!(p2.is_empty());
    }

    #[test]
    fn child_delete_cancels_an_incomplete_transfer_before_its_final_chunk() {
        let mut relay = KittyRelay::new();
        assert!(relay.feed(&apc("Ga=T,i=2,m=1,f=24,s=1,v=1;AQ")).is_empty());
        assert!(matches!(
            relay.feed(&apc("Ga=d,i=2;")).as_slice(),
            [Piece::GfxOther(_)]
        ));
        assert_eq!(relay.feed(&apc("Gm=0;ID")), [Piece::Rejected]);
    }

    #[test]
    fn empty_delete_all_emits_no_graphics_piece_and_cancels_transfer() {
        let mut relay = KittyRelay::new();
        assert!(relay.feed(&apc("Ga=d;")).is_empty());

        assert!(relay.feed(&apc("Ga=T,i=2,m=1,f=24,s=1,v=1;AQ")).is_empty());
        assert_eq!(
            relay.feed(&apc("Ga=d;")),
            [Piece::GfxOther(apc("Ga=d,d=I,i=2147418113,q=2;"))]
        );
        assert_eq!(relay.feed(&apc("Gm=0;ID")), [Piece::Rejected]);
    }

    #[test]
    fn query_is_answered_not_forwarded() {
        let mut r = KittyRelay::new();
        let pieces = r.feed(&apc("Gi=31,a=q,s=1,v=1;AAAA"));
        assert_eq!(
            pieces,
            vec![Piece::GfxAnswer(b"\x1b_Gi=31;OK\x1b\\".to_vec())]
        );
    }

    #[test]
    fn non_graphics_apc_passes_to_emulator() {
        let mut r = KittyRelay::new();
        // APC that is not `ESC _ G …` stays in the emulator stream verbatim.
        let pieces = r.feed(&apc("0;something"));
        assert_eq!(pieces, vec![Piece::Emulator(apc("0;something"))]);
    }

    #[test]
    fn cup_offsets_origin_plus_cursor_one_based() {
        // corner content origin (row=28,col=71), emulator cursor home (0,0) → 29;72H
        assert_eq!(cup((28, 71), (0, 0)), b"\x1b[29;72H".to_vec());
        // non-home cursor adds on top
        assert_eq!(cup((28, 71), (3, 5)), b"\x1b[32;77H".to_vec());
    }

    #[test]
    fn key_value_extracts_fields() {
        assert_eq!(key_value(b"a=T,f=24,m=1", b"a="), Some(&b"T"[..]));
        assert_eq!(key_value(b"a=T,f=24,m=1", b"f="), Some(&b"24"[..]));
        assert_eq!(key_value(b"a=T,f=24,m=1", b"i="), None);
    }

    #[test]
    fn kitty_graphics_detection() {
        assert!(detect_kitty_graphics(Some("xterm-ghostty"), None, false));
        assert!(detect_kitty_graphics(Some("xterm-kitty"), None, false));
        assert!(detect_kitty_graphics(
            Some("xterm-256color"),
            Some("WezTerm"),
            false
        ));
        assert!(detect_kitty_graphics(None, None, true)); // KITTY_WINDOW_ID set
        assert!(!detect_kitty_graphics(
            Some("xterm-256color"),
            Some("Apple_Terminal"),
            false
        ));
        assert!(!detect_kitty_graphics(None, None, false));
    }

    fn relayed(input: &str, size: (usize, usize)) -> Vec<Piece> {
        let mut relay = KittyRelay::new();
        let mut pieces = Vec::new();
        relay.feed_with_rect(&apc(input), size, |piece| pieces.push(piece));
        pieces
    }

    #[test]
    fn all_child_quiet_modes_become_one_outer_q2() {
        for q in [None, Some("0"), Some("1"), Some("2")] {
            let ctrl = match q {
                Some(q) => format!("Ga=T,f=24,s=1,v=1,q={q};AQID"),
                None => "Ga=T,f=24,s=1,v=1;AQID".to_owned(),
            };
            let pieces = relayed(&ctrl, (8, 5));
            let [Piece::GfxDisplay(bytes)] = pieces.as_slice() else {
                panic!("accepted command expected")
            };
            let control = &bytes[2..bytes.iter().position(|b| *b == b';').unwrap()];
            assert_eq!(control.windows(2).filter(|pair| *pair == b"q=").count(), 1);
            assert!(control.windows(3).any(|field| field == b"q=2"));
            assert!(bytes.starts_with(b"\x1b_Ga=T,f=24,s=1,v=1"));
            assert!(control.windows(3).any(|field| field == b"c=8"));
            assert!(control.windows(3).any(|field| field == b"r=5"));
        }
    }

    #[test]
    fn malformed_duplicate_geometry_and_payload_controls_fail_closed() {
        for command in [
            "Ga=T,f=24,s=1,v=1,q=0,q=2;AQID",
            "Ga=T,f=24,s=1,v=1,q=x;AQID",
            "Ga=T,f=24,s=1,v=1,c=9;AQID",
            "Ga=T,f=24,s=1,v=1,X=1;AQID",
            "Ga=T,f=24,s=1,v=1,x=1,w=1;AQID",
            "Ga=T,f=24,s=1,v=1;AQI=",
            "Ga=T,f=24,s=2,v=1;AQID",
            "Ga=T,f=32,s=1,v=1;AQID",
            "Ga=T,f=24,s=1,v=1,o=z;AQID",
            "Ga=T,f=24,s=1,v=1,t=f;AQID",
            "Ga=T,f=24,s=1,v=1,z=nope;AQID",
            "Ga=T,f=24,s=1,v=1,m=1;AQID?",
            "Ga=T,f=24,s=18446744073709551615,v=2,m=1;AQID",
            "Ga=p,i=1,s=2,x=1;",
        ] {
            assert_eq!(relayed(command, (8, 5)), [Piece::Rejected], "{command}");
        }
    }

    #[test]
    fn split_payload_apcs_and_chunk_alphabet_are_bounded() {
        let mut relay = KittyRelay::new();
        let sequence = apc("Ga=T,m=1,f=24,s=1,v=1;AQ");
        let mut pieces = Vec::new();
        for byte in sequence.chunks(1) {
            relay.feed_with_rect(byte, (8, 5), |piece| pieces.push(piece));
        }
        assert!(pieces.is_empty(), "an unfinished transfer is retained");
        relay.feed_with_rect(&apc("Gm=0;ID"), (8, 5), |piece| pieces.push(piece));
        assert!(matches!(
            &pieces[..],
            [Piece::GfxDisplay(bytes)]
                if bytes.starts_with(b"\x1b_Ga=T,m=1,f=24,s=1,v=1,i=2147418112,q=2,C=1,z=-1,c=8,r=5;AQ\x1b\\")
                    && bytes.ends_with(b"\x1b_Gm=0,q=2;ID\x1b\\")
        ));
        assert_eq!(
            relayed("Ga=T,m=1,f=24,s=1,v=1;AQ?", (8, 5)),
            [Piece::Rejected]
        );
    }

    #[test]
    fn transmit_then_place_rebuilds_the_placement_with_corner_limits() {
        let mut relay = KittyRelay::new();
        relay.feed_with_rect(&apc("Ga=T,i=41,f=24,s=1,v=1;AQID"), (8, 5), |_| {});
        let mut pieces = Vec::new();
        relay.feed_with_rect(&apc("Ga=p,i=41,c=3,r=2,q=0,C=0,z=5;"), (8, 5), |piece| {
            pieces.push(piece)
        });
        let [Piece::GfxDisplay(bytes)] = pieces.as_slice() else {
            panic!("placement expected");
        };
        assert_eq!(bytes, b"\x1b_Ga=p,i=2147418113,c=3,r=2,q=2,C=1,z=-1;\x1b\\");
        assert_eq!(relayed("Ga=p,c=2,r=2;", (8, 5)), [Piece::Rejected]);
    }

    #[test]
    fn child_ids_are_rewritten_stable_distinct_and_placeable() {
        let mut relay = KittyRelay::new();
        let transmit = |relay: &mut KittyRelay, id| {
            let mut pieces = Vec::new();
            relay.feed_with_rect(
                &apc(&format!("Ga=T,i={id},f=24,s=1,v=1;AQID")),
                (8, 5),
                |p| pieces.push(p),
            );
            pieces
        };
        let first = transmit(&mut relay, 7);
        let again = transmit(&mut relay, 7);
        let other = transmit(&mut relay, 8);
        let [Piece::GfxDisplay(first)] = first.as_slice() else {
            panic!("transmit expected")
        };
        let [Piece::GfxDisplay(again)] = again.as_slice() else {
            panic!("transmit expected")
        };
        let [Piece::GfxDisplay(other)] = other.as_slice() else {
            panic!("transmit expected")
        };
        assert!(first.windows(12).any(|bytes| bytes == b"i=2147418113"));
        assert!(again.windows(12).any(|bytes| bytes == b"i=2147418113"));
        assert!(other.windows(12).any(|bytes| bytes == b"i=2147418114"));
        let mut placed = Vec::new();
        relay.feed_with_rect(&apc("Ga=p,i=7;"), (8, 5), |piece| placed.push(piece));
        assert!(
            matches!(placed.as_slice(), [Piece::GfxDisplay(bytes)] if bytes.starts_with(b"\x1b_Ga=p,i=2147418113,"))
        );
    }

    #[test]
    fn preview_reserved_id_is_namespaced_and_cannot_be_child_deleted() {
        let preview_id = crate::graphics::PREVIEW_IMAGE_ID;
        let mut relay = KittyRelay::new();
        let mut transmitted = Vec::new();
        relay.feed_with_rect(
            &apc(&format!("Ga=T,i={preview_id},f=24,s=1,v=1;AQID")),
            (8, 5),
            |piece| transmitted.push(piece),
        );
        let [Piece::GfxDisplay(bytes)] = transmitted.as_slice() else {
            panic!("transmit expected")
        };
        assert!(
            bytes.starts_with(
                format!("\x1b_Ga=T,i={},f=24,s=1,v=1,", CORNER_ID_BASE + 1).as_bytes()
            )
        );

        let mut fresh_relay = KittyRelay::new();
        let mut deleted = Vec::new();
        fresh_relay.feed_with_rect(
            &apc(&format!("Ga=d,d=I,i={preview_id};")),
            (8, 5),
            |piece| deleted.push(piece),
        );
        assert_eq!(deleted, [Piece::Rejected]);
    }

    #[test]
    fn anonymous_transmit_uses_the_default_reserved_id() {
        let pieces = relayed("Ga=T,f=24,s=1,v=1;AQID", (8, 5));
        let [Piece::GfxDisplay(bytes)] = pieces.as_slice() else {
            panic!("transmit expected")
        };
        assert!(bytes.starts_with(b"\x1b_Ga=T,f=24,s=1,v=1,i=2147418112,"));
    }

    #[test]
    fn child_deletes_are_scoped_and_never_reach_unmapped_ids() {
        let mut relay = KittyRelay::new();
        relay.feed_with_rect(&apc("Ga=T,i=5,f=24,s=1,v=1;AQID"), (8, 5), |_| {});
        let mut deleted = Vec::new();
        relay.feed_with_rect(&apc("Ga=d,i=5;"), (8, 5), |piece| deleted.push(piece));
        assert_eq!(
            deleted,
            [Piece::GfxOther(apc("Ga=d,d=I,i=2147418113,q=2;"))]
        );
        let mut unknown = Vec::new();
        relay.feed_with_rect(&apc("Ga=d,i=5;"), (8, 5), |piece| unknown.push(piece));
        assert_eq!(unknown, [Piece::Rejected]);

        let mut relay = KittyRelay::new();
        let mut emitted = Vec::new();
        relay.feed_with_rect(&apc("Ga=T,i=5,f=24,s=1,v=1;AQID"), (8, 5), |piece| {
            emitted.push(piece)
        });
        emitted.clear();
        relay.feed_with_rect(&apc("Ga=d;"), (8, 5), |piece| emitted.push(piece));
        let [Piece::GfxOther(bytes)] = emitted.as_slice() else {
            panic!("scoped deletes expected")
        };
        assert_eq!(*bytes, apc("Ga=d,d=I,i=2147418113,q=2;"));
        assert!(!bytes.windows(4).any(|part| part == b"a=d;"));
    }

    #[test]
    fn id_table_overflow_recovers_after_child_delete_all() {
        let mut relay = KittyRelay::new();
        for id in 1..CORNER_IDS {
            let mut pieces = Vec::new();
            relay.feed_with_rect(
                &apc(&format!("Ga=T,i={id},f=24,s=1,v=1;AQID")),
                (8, 5),
                |piece| pieces.push(piece),
            );
            assert!(matches!(pieces.as_slice(), [Piece::GfxDisplay(_)]));
        }
        let mut pieces = Vec::new();
        relay.feed_with_rect(
            &apc(&format!("Ga=T,i={CORNER_IDS},f=24,s=1,v=1;AQID")),
            (8, 5),
            |piece| pieces.push(piece),
        );
        assert_eq!(pieces, [Piece::Rejected]);
        relay.feed_with_rect(&apc("Ga=d;"), (8, 5), |_| {});
        pieces.clear();
        relay.feed_with_rect(&apc("Ga=T,i=99,f=24,s=1,v=1;AQID"), (8, 5), |piece| {
            pieces.push(piece)
        });
        assert!(matches!(pieces.as_slice(), [Piece::GfxDisplay(_)]));
    }

    #[test]
    fn reset_clears_child_id_mappings() {
        let mut relay = KittyRelay::new();
        relay.feed_with_rect(&apc("Ga=T,i=7,f=24,s=1,v=1;AQID"), (8, 5), |_| {});
        relay.reset();
        let mut pieces = Vec::new();
        relay.feed_with_rect(&apc("Ga=p,i=7;"), (8, 5), |piece| pieces.push(piece));
        assert_eq!(pieces, [Piece::Rejected]);
    }

    #[test]
    fn lifecycle_delete_all_only_targets_the_reserved_block() {
        let bytes = delete_all();
        assert_eq!(
            bytes.windows(4).filter(|part| *part == b"a=d,").count(),
            CORNER_IDS
        );
        assert!(!bytes.windows(4).any(|part| part == b"a=d;"));
        for slot in 0..CORNER_IDS {
            let expected = format!("d=I,i={},q=2", CORNER_ID_BASE + slot as u32);
            assert!(
                bytes
                    .windows(expected.len())
                    .any(|part| part == expected.as_bytes())
            );
        }
    }

    #[test]
    fn multi_chunk_transmit_rewrites_only_its_first_apc_id() {
        let mut relay = KittyRelay::new();
        assert!(relay.feed(&apc("Ga=T,i=19,m=1,f=24,s=1,v=1;AQ")).is_empty());
        let pieces = relay.feed(&apc("Gm=0;ID"));
        let [Piece::GfxDisplay(bytes)] = pieces.as_slice() else {
            panic!("complete transfer expected")
        };
        assert!(bytes.starts_with(b"\x1b_Ga=T,i=2147418113,m=1,"));
        assert!(bytes.ends_with(b"\x1b_Gm=0,q=2;ID\x1b\\"));
        assert_eq!(bytes.windows(2).filter(|part| *part == b"i=").count(), 1);
    }

    #[test]
    fn multi_chunk_transfer_is_rejected_when_corner_geometry_changes() {
        let mut relay = KittyRelay::new();
        let mut pieces = Vec::new();
        relay.feed_with_rect(&apc("Ga=T,i=19,m=1,f=24,s=1,v=1;AQ"), (40, 12), |piece| {
            pieces.push(piece)
        });
        assert!(pieces.is_empty());

        relay.feed_with_rect(&apc("Gm=0;ID"), (10, 4), |piece| pieces.push(piece));
        assert_eq!(pieces, [Piece::Rejected]);
        assert!(
            !pieces
                .iter()
                .any(|piece| matches!(piece, Piece::GfxDisplay(_)))
        );
    }

    #[test]
    fn multi_chunk_transfer_keeps_same_corner_geometry() {
        let mut relay = KittyRelay::new();
        let mut pieces = Vec::new();
        relay.feed_with_rect(&apc("Ga=T,i=19,m=1,f=24,s=1,v=1;AQ"), (40, 12), |piece| {
            pieces.push(piece)
        });
        assert!(pieces.is_empty());

        relay.feed_with_rect(&apc("Gm=0;ID"), (40, 12), |piece| pieces.push(piece));
        assert!(matches!(pieces.as_slice(), [Piece::GfxDisplay(_)]));
    }

    #[test]
    fn hostile_control_combinations_have_bounded_fail_closed_work() {
        let keys = [
            "c=999999999999999999999",
            "x=18446744073709551615,w=2",
            "z=999999999999999999999",
            "r=999",
            "X=0,Y=1",
            "q=1,q=2",
        ];
        for key in keys {
            for action in ["T", "p"] {
                let command = if action == "T" {
                    format!("Ga=T,f=24,s=1,v=1,{key};AQID")
                } else {
                    format!("Ga=p,i=1,s=1,v=1,{key};")
                };
                assert_eq!(relayed(&command, (8, 5)), [Piece::Rejected], "{command}");
            }
        }
        let pieces = relayed("Ga=T,f=24,s=1,v=1,C=0;AQID", (8, 5));
        let [Piece::GfxDisplay(bytes)] = pieces.as_slice() else {
            panic!("accepted command expected")
        };
        assert!(bytes.windows(3).any(|part| part == b"C=1"));
    }

    #[test]
    fn image_ids_must_fit_the_kitty_u32_protocol() {
        assert_eq!(relayed("Ga=d,i=4294967296;", (8, 5)), [Piece::Rejected]);
        assert_eq!(relayed("Ga=d,i=4294967295;", (8, 5)), [Piece::Rejected]);
    }
}
