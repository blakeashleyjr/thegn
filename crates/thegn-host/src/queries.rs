//! Terminal query responder: programs inside panes probe their "terminal"
//! (DA1/DA2, cursor position, OSC color queries, kitty protocol checks) and
//! hang or warn when nothing answers — the host's emulator only parses output,
//! it never replies. This module scans a pane's output chunk for the common
//! queries and produces the byte responses to write back into the PTY, as the
//! terminal thegn impersonates would.
//!
//! Pure (bytes in → bytes out) and unit-tested; the event loop calls it right
//! after feeding pane output.

/// The pane's foreground/background, as OSC 10/11 must report them. Passed in
/// rather than read from the palette global so this module stays pure.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PaneColors {
    pub fg: (u8, u8, u8),
    pub bg: (u8, u8, u8),
}

/// Maximum retained body of one incomplete terminal control sequence. The
/// parser continues discarding an oversized sequence through its terminator,
/// so its suffix can never be mistaken for a top-level query.
pub(crate) const MAX_QUERY_CONTROL: usize = 4096;

#[derive(Debug, Default)]
enum StreamState {
    #[default]
    Text,
    Escape,
    Csi,
    Osc {
        escape: bool,
    },
    Apc {
        escape: bool,
    },
    DiscardCsi,
    DiscardOsc {
        escape: bool,
    },
    DiscardApc {
        escape: bool,
    },
}

/// Per-pane incremental query scanner. Its bounded sequence retention follows
/// the same lifecycle and discard-through-terminator contract as the OSC 52
/// clipboard stream parser, while keeping query matching and clipboard
/// admission separate (queries answer immediately; clipboard sets may remain
/// pending under writer backpressure). Reset it only through
/// `pty_drain::reset_terminal_streams`, which also resets the clipboard
/// parser; register any third streaming parser there.
#[derive(Debug, Default)]
pub(crate) struct QueryParser {
    state: StreamState,
    body: Vec<u8>,
}

impl QueryParser {
    pub(crate) fn reset(&mut self) {
        self.state = StreamState::Text;
        self.body.clear();
    }

    pub(crate) fn feed(
        &mut self,
        bytes: &[u8],
        cursor: (u16, u16),
        size: (u16, u16),
        colors: PaneColors,
    ) -> Vec<u8> {
        let mut out = Vec::new();
        for &byte in bytes {
            let state = std::mem::take(&mut self.state);
            self.state = match state {
                StreamState::Text => {
                    if byte == 0x1b {
                        StreamState::Escape
                    } else {
                        StreamState::Text
                    }
                }
                StreamState::Escape => match byte {
                    b'[' => {
                        self.body.clear();
                        StreamState::Csi
                    }
                    b']' => {
                        self.body.clear();
                        StreamState::Osc { escape: false }
                    }
                    b'_' => {
                        self.body.clear();
                        StreamState::Apc { escape: false }
                    }
                    b'P' | b'^' | b'X' => StreamState::DiscardApc { escape: false },
                    0x1b => StreamState::Escape,
                    _ => StreamState::Text,
                },
                StreamState::Csi => {
                    if (0x40..=0x7e).contains(&byte) {
                        if self.body.len() < MAX_QUERY_CONTROL {
                            self.body.push(byte);
                            respond_csi(&self.body, cursor, size, &mut out);
                        }
                        self.body.clear();
                        StreamState::Text
                    } else if byte == 0x1b {
                        self.body.clear();
                        StreamState::Escape
                    } else if self.body.len() == MAX_QUERY_CONTROL {
                        self.body.clear();
                        StreamState::DiscardCsi
                    } else {
                        self.body.push(byte);
                        StreamState::Csi
                    }
                }
                StreamState::Osc { escape } => {
                    let ended = byte == 7 || escape && byte == b'\\';
                    if self.body.len() == MAX_QUERY_CONTROL {
                        self.body.clear();
                        if ended {
                            StreamState::Text
                        } else {
                            StreamState::DiscardOsc {
                                escape: byte == 0x1b,
                            }
                        }
                    } else {
                        self.body.push(byte);
                        if ended {
                            let end = self.body.len() - if byte == 7 { 1 } else { 2 };
                            respond_osc(&self.body[..end], colors, &mut out);
                            self.body.clear();
                            StreamState::Text
                        } else if byte.is_ascii_control() && byte != 0x1b {
                            self.body.clear();
                            StreamState::DiscardOsc { escape: false }
                        } else {
                            StreamState::Osc {
                                escape: byte == 0x1b,
                            }
                        }
                    }
                }
                StreamState::Apc { escape } => {
                    let ended = escape && byte == b'\\';
                    if self.body.len() == MAX_QUERY_CONTROL {
                        self.body.clear();
                        if ended {
                            StreamState::Text
                        } else {
                            StreamState::DiscardApc {
                                escape: byte == 0x1b,
                            }
                        }
                    } else {
                        self.body.push(byte);
                        if ended {
                            let end = self.body.len() - 2;
                            respond_apc(&self.body[..end], &mut out);
                            self.body.clear();
                            StreamState::Text
                        } else {
                            StreamState::Apc {
                                escape: byte == 0x1b,
                            }
                        }
                    }
                }
                StreamState::DiscardCsi => {
                    if (0x40..=0x7e).contains(&byte) {
                        StreamState::Text
                    } else {
                        StreamState::DiscardCsi
                    }
                }
                StreamState::DiscardOsc { escape } => {
                    if byte == 7 || escape && byte == b'\\' {
                        StreamState::Text
                    } else {
                        StreamState::DiscardOsc {
                            escape: byte == 0x1b,
                        }
                    }
                }
                StreamState::DiscardApc { escape } => {
                    if escape && byte == b'\\' {
                        StreamState::Text
                    } else {
                        StreamState::DiscardApc {
                            escape: byte == 0x1b,
                        }
                    }
                }
            };
        }
        out
    }
}

/// Scan `bytes` for terminal queries; produce the responses to write back.
/// `cursor` is the emulator's current (row, col), 0-based; `size` is
/// (rows, cols); `colors` is what OSC 10/11 report. The PTY drain uses a
/// pane-owned [`QueryParser`] for cross-slice recognition.
pub fn query_responses(
    bytes: &[u8],
    cursor: (u16, u16),
    size: (u16, u16),
    colors: PaneColors,
) -> Vec<u8> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] != 0x1b {
            i += 1;
            continue;
        }
        let rest = &bytes[i + 1..];
        match rest.first() {
            Some(b'[') => {
                let body = &rest[1..];
                if let Some((seq, len)) = csi_seq(body) {
                    respond_csi(seq, cursor, size, &mut out);
                    i += 2 + len;
                    continue;
                }
            }
            Some(b']') => {
                let body = &rest[1..];
                if let Some((seq, len)) = osc_seq(body) {
                    respond_osc(seq, colors, &mut out);
                    i += 2 + len;
                    continue;
                }
            }
            Some(b'_') => {
                // APC (kitty graphics et al): `ESC _ G ... ESC \`.
                let body = &rest[1..];
                if let Some(end) = find_st(body) {
                    respond_apc(&body[..end], &mut out);
                    i += 2 + end + 2;
                    continue;
                }
            }
            _ => {}
        }
        i += 1;
    }
    out
}

/// Slice a CSI body up to (exclusive) its final byte; returns (full seq incl.
/// final, consumed length).
fn csi_seq(body: &[u8]) -> Option<(&[u8], usize)> {
    let end = body
        .iter()
        .position(|&b| (0x40..=0x7e).contains(&b) && !matches!(b, b'[' | b']'))?;
    Some((&body[..=end], end + 1))
}

/// Slice an OSC body up to its BEL / ST terminator.
fn osc_seq(body: &[u8]) -> Option<(&[u8], usize)> {
    for (i, &b) in body.iter().enumerate() {
        if b == 0x07 {
            return Some((&body[..i], i + 1));
        }
        if b == 0x1b && body.get(i + 1) == Some(&b'\\') {
            return Some((&body[..i], i + 2));
        }
    }
    None
}

fn find_st(body: &[u8]) -> Option<usize> {
    body.windows(2).position(|w| w == b"\x1b\\")
}

fn respond_csi(seq: &[u8], cursor: (u16, u16), size: (u16, u16), out: &mut Vec<u8>) {
    match seq {
        // DA1: "what are you?" — a VT220-class color terminal.
        b"c" | b"0c" => out.extend_from_slice(b"\x1b[?62;4;6;22c"),
        // DA2: secondary attributes (type;version;rom).
        b">c" | b">0c" => out.extend_from_slice(b"\x1b[>1;10;0c"),
        // DSR 5: status report — OK.
        b"5n" => out.extend_from_slice(b"\x1b[0n"),
        // DSR 6: cursor position report (1-based).
        b"6n" => {
            let _ = std::io::Write::write_fmt(
                // best-effort: infallible: writing into a Vec<u8> cannot fail
                out,
                format_args!("\x1b[{};{}R", cursor.0 + 1, cursor.1 + 1),
            );
        }
        // Kitty keyboard protocol query: no flags pushed inside panes.
        b"?u" => out.extend_from_slice(b"\x1b[?0u"),
        // XTVERSION.
        b">q" | b">0q" => {
            let _ = std::io::Write::write_fmt(
                // best-effort: infallible: writing into a Vec<u8> cannot fail
                out,
                format_args!("\x1bP>|thegn {}\x1b\\", env!("CARGO_PKG_VERSION")),
            );
        }
        // XTWINOPS 18: text-area size in cells.
        b"18t" => {
            let _ = std::io::Write::write_fmt(out, format_args!("\x1b[8;{};{}t", size.0, size.1)); // best-effort: infallible: writing into a Vec<u8> cannot fail
        }
        // XTWINOPS 14: text-area size in pixels (approximate cell metrics —
        // image-preview probes only need a plausible ratio).
        b"14t" => {
            let _ = std::io::Write::write_fmt(
                // best-effort: infallible: writing into a Vec<u8> cannot fail
                out,
                format_args!("\x1b[4;{};{}t", (size.0 as u32) * 16, (size.1 as u32) * 8),
            );
        }
        _ => {}
    }
}

fn respond_osc(seq: &[u8], colors: PaneColors, out: &mut Vec<u8>) {
    // OSC 10/11 color queries: report the pane's text / background colors so
    // apps that theme against the terminal blend with the palette.
    //
    // These used to be the hardcoded `theme::TEXT` / `theme::BG0` — the
    // *legacy* pre-prism constants (`#14161f`), which are not the live
    // palette's `bg0` (`#0b0e16`) and were not what the compositor painted
    // either. Three different answers to "what colour is this pane". The
    // caller now passes the same tokens `compositor::default_pair` resolves
    // with, so what we advertise is what we draw.
    let rgb = |(r, g, b): (u8, u8, u8)| -> String {
        format!("rgb:{r:02x}{r:02x}/{g:02x}{g:02x}/{b:02x}{b:02x}")
    };
    if seq == b"10;?" {
        let _ = std::io::Write::write_fmt(out, format_args!("\x1b]10;{}\x1b\\", rgb(colors.fg))); // best-effort: infallible: writing into a Vec<u8> cannot fail
    } else if seq == b"11;?" {
        let _ = std::io::Write::write_fmt(out, format_args!("\x1b]11;{}\x1b\\", rgb(colors.bg))); // best-effort: infallible: writing into a Vec<u8> cannot fail
    }
}

fn respond_apc(body: &[u8], out: &mut Vec<u8>) {
    // Kitty graphics probe (`a=q`): reply with an error for the probed image
    // id so clients conclude "no graphics support" instead of timing out.
    if body.first() != Some(&b'G') || !body.windows(3).any(|w| w == b"a=q") {
        return;
    }
    let id: String = body
        .windows(2)
        .position(|w| w == b"i=")
        .map(|p| {
            body[p + 2..]
                .iter()
                .take_while(|b| b.is_ascii_digit())
                .map(|&b| b as char)
                .collect()
        })
        .unwrap_or_default();
    if id.is_empty() {
        out.extend_from_slice(b"\x1b_GENOTSUPPORTED:\x1b\\");
    } else {
        let _ = std::io::Write::write_fmt(out, format_args!("\x1b_Gi={id};ENOTSUPPORTED:\x1b\\")); // best-effort: infallible: writing into a Vec<u8> cannot fail
    }
}

#[path = "osc_clipboard.rs"]
pub(crate) mod clipboard;

#[cfg(test)]
fn osc_passthrough(bytes: &[u8]) -> Vec<u8> {
    let mut parser = clipboard::Clipboard::default();
    parser.feed(bytes);
    let mut out = Vec::new();
    parser.submit(|bytes| {
        out = bytes;
        Ok(())
    });
    out
}

// The drawer→host control channel (`OSC 5379`) grammar moved to
// `thegn_core::file_manager` — it is the file-manager seam's control
// vocabulary, decoded only for providers whose caps declare a control channel.

#[cfg(test)]
mod tests {
    use super::*;

    const COLORS: PaneColors = PaneColors {
        fg: (237, 240, 248),
        bg: (11, 14, 22),
    };

    fn resp(bytes: &[u8]) -> Vec<u8> {
        query_responses(bytes, (4, 9), (24, 80), COLORS)
    }

    #[test]
    fn da_and_dsr_queries_get_answers() {
        assert_eq!(resp(b"\x1b[c"), b"\x1b[?62;4;6;22c");
        assert_eq!(resp(b"\x1b[>c"), b"\x1b[>1;10;0c");
        assert_eq!(resp(b"\x1b[5n"), b"\x1b[0n");
        // CPR is 1-based.
        assert_eq!(resp(b"\x1b[6n"), b"\x1b[5;10R");
        assert_eq!(resp(b"\x1b[?u"), b"\x1b[?0u");
    }

    #[test]
    fn window_size_reports_cells_and_pixels() {
        assert_eq!(resp(b"\x1b[18t"), b"\x1b[8;24;80t");
        assert_eq!(resp(b"\x1b[14t"), b"\x1b[4;384;640t");
    }

    #[test]
    fn osc_color_queries_report_the_callers_pane_colors() {
        // Not a hardcoded constant: the answer must be the pair the compositor
        // actually paints, or a pane's apps theme against a background nothing
        // draws. `rgb:` doubles each byte to the 16-bit-per-channel form.
        let bg = String::from_utf8(resp(b"\x1b]11;?\x07")).unwrap();
        assert_eq!(bg, "\x1b]11;rgb:0b0b/0e0e/1616\x1b\\", "{bg:?}");
        let fg = String::from_utf8(resp(b"\x1b]10;?\x1b\\")).unwrap();
        assert_eq!(fg, "\x1b]10;rgb:eded/f0f0/f8f8\x1b\\", "{fg:?}");
    }

    #[test]
    fn osc_color_queries_follow_a_recoloured_palette() {
        let themed = PaneColors {
            fg: (1, 2, 3),
            bg: (4, 5, 6),
        };
        let out = query_responses(b"\x1b]11;?\x07", (0, 0), (24, 80), themed);
        assert_eq!(
            String::from_utf8(out).unwrap(),
            "\x1b]11;rgb:0404/0505/0606\x1b\\"
        );
    }

    #[test]
    fn kitty_graphics_probe_gets_an_error_reply() {
        let r = resp(b"\x1b_Gi=31,s=1,v=1,a=q,t=d,f=24;AAAA\x1b\\");
        let s = String::from_utf8(r).unwrap();
        assert!(s.contains("i=31;ENOTSUPPORTED"), "{s:?}");
    }

    #[test]
    fn osc52_clipboard_sets_forward_verbatim() {
        let seq = b"before\x1b]52;c;aGVsbG8=\x07after";
        let fwd = osc_passthrough(seq);
        assert_eq!(fwd, b"\x1b]52;c;aGVsbG8=\x07");
        assert!(
            osc_passthrough(b"\x1b]11;?\x07").is_empty(),
            "queries are not clipboard sets"
        );
        // ST-terminated form too.
        let st = b"\x1b]52;c;eA==\x1b\\";
        assert_eq!(osc_passthrough(st), st);
    }

    // The drawer control-channel decode moved to `thegn_core::file_manager`
    // (the seam owns its grammar); its tests live there.

    #[test]
    fn ordinary_output_produces_no_responses() {
        assert!(resp(b"hello \x1b[31mred\x1b[0m world\r\n").is_empty());
        // A DA-looking final byte inside ordinary SGR must not trigger.
        assert!(resp(b"\x1b[1;31m").is_empty());
        // Multiple queries in one chunk all answer.
        let r = resp(b"\x1b[c\x1b[6n");
        assert!(r.starts_with(b"\x1b[?62"));
        assert!(r.ends_with(b"\x1b[5;10R"));
    }

    #[test]
    fn every_supported_query_survives_every_two_slice_boundary_once() {
        let queries: &[&[u8]] = &[
            b"\x1b[c",
            b"\x1b[0c",
            b"\x1b[>c",
            b"\x1b[>0c",
            b"\x1b[5n",
            b"\x1b[6n",
            b"\x1b[?u",
            b"\x1b[>q",
            b"\x1b[>0q",
            b"\x1b[18t",
            b"\x1b[14t",
            b"\x1b]10;?\x07",
            b"\x1b]11;?\x07",
            b"\x1b]10;?\x1b\\",
            b"\x1b]11;?\x1b\\",
            b"\x1b_Gi=31,s=1,a=q\x1b\\",
            b"\x1b_Ga=q\x1b\\",
        ];
        for query in queries {
            let expected = resp(query);
            assert!(!expected.is_empty(), "fixture has no reply: {query:?}");
            for split in 0..=query.len() {
                let mut parser = QueryParser::default();
                let mut actual = parser.feed(&query[..split], (4, 9), (24, 80), COLORS);
                actual.extend(parser.feed(&query[split..], (4, 9), (24, 80), COLORS));
                assert_eq!(actual, expected, "split {split} of {query:?}");
            }
        }
    }

    #[test]
    fn oversized_controls_discard_through_their_semantic_terminator() {
        let valid = b"\x1b[5n";
        let cases: &[(&[u8], &[u8])] =
            &[(b"\x1b[", b"n"), (b"\x1b]", b"\x07"), (b"\x1b_", b"\x1b\\")];
        for (prefix, terminator) in cases {
            let mut oversized = prefix.to_vec();
            oversized.extend(std::iter::repeat(b'x').take(MAX_QUERY_CONTROL + 20));
            oversized.extend_from_slice(terminator);
            let mut parser = QueryParser::default();
            let mut out = parser.feed(&oversized, (4, 9), (24, 80), COLORS);
            assert!(parser.body.len() <= MAX_QUERY_CONTROL);
            assert!(parser.body.capacity() <= MAX_QUERY_CONTROL);
            out.extend(parser.feed(valid, (4, 9), (24, 80), COLORS));
            assert_eq!(out, b"\x1b[0n");
        }
    }

    #[test]
    fn parser_reset_and_pane_isolation_prevent_cross_stream_completion() {
        let mut a = QueryParser::default();
        let mut b = QueryParser::default();
        assert!(a.feed(b"\x1b[", (4, 9), (24, 80), COLORS).is_empty());
        assert!(b.feed(b"5n", (4, 9), (24, 80), COLORS).is_empty());
        a.reset();
        assert!(a.feed(b"5n", (4, 9), (24, 80), COLORS).is_empty());
        assert_eq!(a.feed(b"\x1b[5n", (4, 9), (24, 80), COLORS), b"\x1b[0n");
    }

    #[test]
    fn queries_inside_osc_apc_and_other_strings_are_not_top_level() {
        for input in [
            b"\x1b]x\x1b[5n\x07".as_slice(),
            b"\x1b_Gx\x1b[5n\x1b\\".as_slice(),
            b"\x1bPx\x1b[5n\x1b\\".as_slice(),
        ] {
            let mut parser = QueryParser::default();
            assert!(parser.feed(input, (4, 9), (24, 80), COLORS).is_empty());
        }
    }

    #[test]
    fn corner_relay_probe_is_not_answered_again_by_the_query_scanner() {
        let mut relay = crate::kitty_relay::KittyRelay::default();
        let mut parser = QueryParser::default();
        let mut relay_replies = Vec::new();
        let mut query_replies = Vec::new();
        relay.feed_with(
            b"\x1b_Gi=31,s=1,v=1,a=q,t=d,f=24;AAAA\x1b\\",
            |piece| match piece {
                crate::kitty_relay::Piece::Emulator(bytes) => {
                    query_replies.extend(parser.feed(&bytes, (4, 9), (24, 80), COLORS))
                }
                crate::kitty_relay::Piece::GfxAnswer(bytes) => relay_replies.extend(bytes),
                crate::kitty_relay::Piece::GfxDisplay(bytes)
                | crate::kitty_relay::Piece::GfxOther(bytes) => {
                    query_replies.extend(parser.feed(&bytes, (4, 9), (24, 80), COLORS));
                }
            },
        );
        assert!(relay_replies.starts_with(b"\x1b_Gi=31;"));
        assert!(query_replies.is_empty());
    }
}
