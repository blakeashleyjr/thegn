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
//! Placement is the cursor position at display time: mpv homes the cursor (a text
//! escape the emulator processes), `C=1` means "don't move the cursor after", and
//! `q=2` suppresses responses. With `--vo=kitty` set explicitly mpv sends **no
//! `a=q` probe** — it just draws. Images are chunked far larger than one PTY read,
//! so APC sequences MUST be buffered across reads.
//!
//! ## What the relay does
//!
//! [`KittyRelay::feed`] splits a PTY chunk into [`Piece`]s: non-graphics bytes go
//! to the emulator (so its cursor tracks mpv's home); APC-`G` commands are pulled
//! out and forwarded to the outer terminal — a DISPLAY command (`a=T`/`a=p`) is
//! prefixed with an absolute CUP to `corner_origin + emulator_cursor` ([`cup`]) so
//! the image lands in the corner instead of at screen home; delete/continuation
//! APCs forward verbatim. A query (`a=q`) is answered locally and never forwarded
//! (forwarding it would make the OUTER terminal reply to us).

const ESC: u8 = 0x1b;
const ST: &[u8] = b"\x1b\\"; // String Terminator (ESC \)

/// One classified slice of split PTY output.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Piece {
    /// Non-graphics bytes — feed to the pane emulator (and the normal query path).
    Emulator(Vec<u8>),
    /// An APC-`G` command that DISPLAYS at the cursor (`a=T` / `a=p`): the caller
    /// prefixes a [`cup`] to the corner before forwarding to the outer terminal.
    GfxDisplay(Vec<u8>),
    /// Any other APC-`G` (delete, continuation chunk): forward verbatim, no CUP.
    GfxOther(Vec<u8>),
    /// An APC-`G` query (`a=q`): write this reply back to the pane; never forward.
    GfxAnswer(Vec<u8>),
}

/// Splits a corner pane's PTY stream into emulator text and kitty graphics
/// commands, buffering APC sequences that straddle PTY reads.
#[derive(Debug, Default)]
pub struct KittyRelay {
    /// An APC sequence (or a lone trailing `ESC`) not yet terminated; carried into
    /// the next [`Self::feed`].
    partial: Vec<u8>,
}

impl KittyRelay {
    pub fn new() -> Self {
        Self::default()
    }

    /// Drop any buffered partial — call when (re)spawning the corner pane so a new
    /// child never inherits a stale half-sequence.
    pub fn reset(&mut self) {
        self.partial.clear();
    }

    /// Split `input` (prepended with any buffered partial) into ordered pieces. A
    /// trailing incomplete APC (or lone `ESC`) is retained for the next call.
    pub fn feed(&mut self, input: &[u8]) -> Vec<Piece> {
        let mut buf = std::mem::take(&mut self.partial);
        buf.extend_from_slice(input);
        let mut out = Vec::new();
        let mut i = 0;
        let mut text_start = 0;
        while i < buf.len() {
            if buf[i] != ESC {
                i += 1;
                continue;
            }
            // A bare trailing ESC may begin an APC on the next read — buffer it.
            if i + 1 >= buf.len() {
                flush_text(&mut out, &buf[text_start..i]);
                self.partial = buf[i..].to_vec();
                return out;
            }
            // Only APC (`ESC _`) is special; CSI/OSC/etc. stay in the text stream.
            if buf[i + 1] != b'_' {
                i += 2;
                continue;
            }
            match find_st(&buf, i + 2) {
                Some(end) => {
                    // `end` is the index just past the ST.
                    flush_text(&mut out, &buf[text_start..i]);
                    out.push(classify(&buf[i..end]));
                    i = end;
                    text_start = end;
                }
                None => {
                    // Incomplete APC: emit text before it, buffer the rest.
                    flush_text(&mut out, &buf[text_start..i]);
                    self.partial = buf[i..].to_vec();
                    return out;
                }
            }
        }
        flush_text(&mut out, &buf[text_start..]);
        out
    }
}

fn flush_text(out: &mut Vec<Piece>, bytes: &[u8]) {
    if !bytes.is_empty() {
        out.push(Piece::Emulator(bytes.to_vec()));
    }
}

/// Find the index just past the next String Terminator (`ESC \`) at or after `from`.
fn find_st(buf: &[u8], from: usize) -> Option<usize> {
    buf[from..]
        .windows(2)
        .position(|w| w == ST)
        .map(|p| from + p + 2)
}

/// Classify a complete APC sequence (`ESC _ … ESC \`). Non-graphics APCs (not
/// `ESC _ G`) pass through to the emulator verbatim.
fn classify(seq: &[u8]) -> Piece {
    // seq = ESC _ <body> ESC \  → body is seq[2 .. len-2].
    if seq.len() < 4 || seq[2] != b'G' {
        return Piece::Emulator(seq.to_vec());
    }
    let body = &seq[2..seq.len() - 2]; // starts with 'G'
    // Control keys are up to the first ';' (payload separator), after the leading 'G'.
    let ctrl_end = body.iter().position(|&b| b == b';').unwrap_or(body.len());
    let ctrl = &body[1..ctrl_end];
    let action = key_value(ctrl, b"a=");
    match action {
        Some(b"q") => Piece::GfxAnswer(answer_for(ctrl)),
        Some(b"T") | Some(b"p") => Piece::GfxDisplay(seq.to_vec()),
        _ => Piece::GfxOther(seq.to_vec()),
    }
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
    use super::*;

    fn apc(body: &str) -> Vec<u8> {
        let mut v = Vec::from(&b"\x1b_"[..]);
        v.extend_from_slice(body.as_bytes());
        v.extend_from_slice(ST);
        v
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
                Piece::GfxOther(apc("Ga=d")),
                Piece::Emulator(b"\x1b[2J\x1b[0;0f".to_vec()),
                Piece::GfxDisplay(apc("Ga=T,f=24,s=320,v=240,C=1,q=2,m=1;AAAA")),
                Piece::GfxOther(apc("Gm=0;BBBB")),
            ]
        );
    }

    #[test]
    fn buffers_apc_split_across_feeds() {
        let mut r = KittyRelay::new();
        let full = apc("Ga=T,m=1;PAYLOAD");
        let (head, tail) = full.split_at(10); // cut mid-sequence
        let first = r.feed(head);
        assert!(first.is_empty(), "incomplete APC yields nothing yet");
        let second = r.feed(tail);
        assert_eq!(second, vec![Piece::GfxDisplay(full)]);
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
        assert_eq!(p2, vec![Piece::GfxOther(apc("Ga=d"))]);
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
}
