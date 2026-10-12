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
//! and receives cell dimensions bounded by the content rect. Delete/continuation
//! APCs are also rebuilt with quiet mode. A query (`a=q`) is answered locally
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
    /// Any other accepted APC-`G` (delete, continuation chunk): rebuilt, no CUP.
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
/// Text is emitted incrementally even when the caller supplies a huge slice.
const TEXT_PIECE_BYTES: usize = 8192;

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
    #[cfg(test)]
    scanned: usize,
}

impl KittyRelay {
    pub fn new() -> Self {
        Self::default()
    }

    /// A new pane must never inherit the previous pane's partial sequence.
    pub fn reset(&mut self) {
        self.state = ScanState::Text;
        self.partial = Vec::new();
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
                            emit(classify(seq, content_size));
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

    #[cfg(test)]
    fn feed(&mut self, input: &[u8]) -> Vec<Piece> {
        let mut pieces = Vec::new();
        self.feed_with(input, |piece| pieces.push(piece));
        pieces
    }
}

/// Classify a complete APC sequence (`ESC _ … ESC \`). Non-graphics APCs (not
/// `ESC _ G`) pass through to the emulator verbatim.
fn classify(seq: Vec<u8>, content_size: (usize, usize)) -> Piece {
    // seq = ESC _ <body> ESC \  → body is seq[2 .. len-2].
    if seq.len() < 4 || seq[2] != b'G' {
        return Piece::Emulator(seq);
    }
    let body = &seq[2..seq.len() - 2]; // starts with 'G'
    // Control keys are up to the first ';' (payload separator), after the leading 'G'.
    let ctrl_end = body.iter().position(|&b| b == b';').unwrap_or(body.len());
    let ctrl = &body[1..ctrl_end];
    let payload = if ctrl_end < body.len() {
        &seq[ctrl_end + 3..seq.len() - 2]
    } else {
        &[]
    };
    match canonical_graphics(ctrl, payload, content_size) {
        Some(Canonical::Query) => Piece::GfxAnswer(answer_for(ctrl)),
        Some(Canonical::Display(bytes)) => Piece::GfxDisplay(bytes),
        Some(Canonical::Other(bytes)) => Piece::GfxOther(bytes),
        None => Piece::Rejected,
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
            b"i" | b"s" | b"v" | b"m" | b"x" | b"y" | b"w" | b"h" | b"X" | b"Y" => {
                out.extend_from_slice(parse_num(value)?.to_string().as_bytes())
            }
            _ => out.extend_from_slice(value),
        }
    }
    let display = matches!(action, b"T" | b"p");
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

/// Delete all images on the outer terminal (matches mpv's own per-frame `a=d`).
/// Emitted on dismiss/exit/resize/occlude so no frame lingers.
pub fn delete_all() -> &'static [u8] {
    b"\x1b_Ga=d\x1b\\"
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
                    b"hello\x1b_Ga=T,f=24,s=1,v=1,q=2,C=1,z=-1,c=65535,r=65535;AQID\x1b\\tail"
                ),
                "split {split}"
            );
            assert_eq!(relay.scanned, seq.len());
        }
    }

    #[test]
    fn near_cap_bytewise_input_has_linear_scan_work_and_no_prefix_copy() {
        let mut seq = b"\x1b_Ga=T,m=1,f=24,s=1,v=1;".to_vec();
        seq.resize(MAX_APC_BYTES - 2, b'A');
        seq.extend_from_slice(ST);
        for chunk in [1, 17, 8192, MAX_APC_BYTES] {
            let mut relay = KittyRelay::new();
            let mut count = 0;
            for bytes in seq.chunks(chunk) {
                relay.feed_with(bytes, |piece| {
                    assert!(matches!(piece, Piece::GfxDisplay(ref b) if b.starts_with(b"\x1b_Ga=T,m=1,f=24,s=1,v=1,q=2,C=1,z=-1,c=65535,r=65535;")));
                    count += 1;
                });
                assert!(relay.partial.len() <= MAX_APC_BYTES);
                assert!(relay.partial.capacity() <= MAX_APC_BYTES);
            }
            assert_eq!(count, 1);
            // Framing scans every byte once. Classification walks only the
            // complete control prefix and never revisits partial input.
            assert_eq!(relay.scanned, seq.len());
        }
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
            assert_eq!(
                relay.feed(&apc("Ga=d")),
                vec![Piece::GfxOther(apc("Ga=d,q=2;"))]
            );
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
        stream.extend(apc("Ga=T,f=24,s=320,v=240,C=1,q=2,m=1;AAAA"));
        stream.extend(apc("Gm=0;BBBB"));
        let pieces = r.feed(&stream);
        assert_eq!(
            pieces,
            vec![
                Piece::GfxOther(apc("Ga=d,q=2;")),
                Piece::Emulator(b"\x1b[2J\x1b[0;0f".to_vec()),
                Piece::GfxDisplay(apc(
                    "Ga=T,f=24,s=320,v=240,C=1,q=2,m=1,z=-1,c=65535,r=65535;AAAA"
                )),
                Piece::GfxOther(apc("Gm=0,q=2;BBBB")),
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
        assert!(
            matches!(&second[..], [Piece::GfxDisplay(bytes)] if bytes.starts_with(b"\x1b_Ga=T,m=1,f=24,s=1,v=1,q=2,C=1,z=-1,c=65535,r=65535;"))
        );
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
        assert_eq!(p2, vec![Piece::GfxOther(apc("Ga=d,q=2;"))]);
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
        assert!(matches!(&pieces[..], [Piece::GfxDisplay(bytes)] if bytes.ends_with(b";AQ\x1b\\")));
        assert_eq!(
            relayed("Ga=T,m=1,f=24,s=1,v=1;AQ?", (8, 5)),
            [Piece::Rejected]
        );
    }

    #[test]
    fn transmit_then_place_rebuilds_the_placement_with_corner_limits() {
        let pieces = relayed("Ga=p,i=41,c=3,r=2,q=0,C=0,z=5;", (8, 5));
        let [Piece::GfxDisplay(bytes)] = pieces.as_slice() else {
            panic!("placement expected");
        };
        assert_eq!(bytes, b"\x1b_Ga=p,i=41,c=3,r=2,q=2,C=1,z=-1;\x1b\\");
        assert_eq!(relayed("Ga=p,c=2,r=2;", (8, 5)), [Piece::Rejected]);
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
        assert_eq!(
            relayed("Ga=d,i=4294967295;", (8, 5)),
            [Piece::GfxOther(apc("Ga=d,i=4294967295,q=2;"))]
        );
    }
}
