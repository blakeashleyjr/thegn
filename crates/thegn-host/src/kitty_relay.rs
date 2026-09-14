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
    pub fn feed_with(&mut self, input: &[u8], mut emit: impl FnMut(Piece)) {
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
                            emit(classify(seq));
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
fn classify(seq: Vec<u8>) -> Piece {
    // seq = ESC _ <body> ESC \  → body is seq[2 .. len-2].
    if seq.len() < 4 || seq[2] != b'G' {
        return Piece::Emulator(seq);
    }
    let body = &seq[2..seq.len() - 2]; // starts with 'G'
    // Control keys are up to the first ';' (payload separator), after the leading 'G'.
    let ctrl_end = body.iter().position(|&b| b == b';').unwrap_or(body.len());
    let ctrl = &body[1..ctrl_end];
    let action = key_value(ctrl, b"a=");
    match action {
        Some(b"q") => Piece::GfxAnswer(answer_for(ctrl)),
        Some(b"T") | Some(b"p") => Piece::GfxDisplay(seq),
        _ => Piece::GfxOther(seq),
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
    fn every_byte_split_preserves_st_and_non_graphics_text() {
        let seq = b"hello\x1b_Ga=T;AAAA\x1b\\tail\x1b[31m";
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
            assert_eq!(wire, seq, "split {split}");
            assert_eq!(relay.scanned, seq.len());
        }
    }

    #[test]
    fn near_cap_bytewise_input_has_linear_scan_work_and_no_prefix_copy() {
        let mut seq = b"\x1b_Ga=T;".to_vec();
        seq.resize(MAX_APC_BYTES - 2, b'A');
        seq.extend_from_slice(ST);
        for chunk in [1, 17, 8192, MAX_APC_BYTES] {
            let mut relay = KittyRelay::new();
            let mut count = 0;
            for bytes in seq.chunks(chunk) {
                relay.feed_with(bytes, |piece| {
                    assert!(matches!(piece, Piece::GfxDisplay(ref b) if b == &seq));
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
            assert_eq!(relay.feed(&apc("Ga=d")), vec![Piece::GfxOther(apc("Ga=d"))]);
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
