//! Terminal query responder: programs inside panes probe their "terminal"
//! (DA1/DA2, cursor position, OSC color queries, kitty protocol checks) and
//! hang or warn when nothing answers — the host's emulator only parses output,
//! it never replies. This module scans a pane's output chunk for the common
//! queries and produces the byte responses to write back into the PTY, as the
//! terminal superzej impersonates would.
//!
//! Pure (bytes in → bytes out) and unit-tested; the event loop calls it right
//! after feeding pane output.

/// Scan `bytes` for terminal queries; produce the responses to write back.
/// `cursor` is the emulator's current (row, col), 0-based; `size` is
/// (rows, cols). Best-effort: queries split across read chunks are missed,
/// which matches how most terminals' replies race anyway.
pub fn query_responses(bytes: &[u8], cursor: (u16, u16), size: (u16, u16)) -> Vec<u8> {
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
                    respond_osc(seq, &mut out);
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
                out,
                format_args!("\x1b[{};{}R", cursor.0 + 1, cursor.1 + 1),
            );
        }
        // Kitty keyboard protocol query: no flags pushed inside panes.
        b"?u" => out.extend_from_slice(b"\x1b[?0u"),
        // XTVERSION.
        b">q" | b">0q" => {
            let _ = std::io::Write::write_fmt(
                out,
                format_args!("\x1bP>|superzej {}\x1b\\", env!("CARGO_PKG_VERSION")),
            );
        }
        // XTWINOPS 18: text-area size in cells.
        b"18t" => {
            let _ = std::io::Write::write_fmt(out, format_args!("\x1b[8;{};{}t", size.0, size.1));
        }
        // XTWINOPS 14: text-area size in pixels (approximate cell metrics —
        // image-preview probes only need a plausible ratio).
        b"14t" => {
            let _ = std::io::Write::write_fmt(
                out,
                format_args!("\x1b[4;{};{}t", (size.0 as u32) * 16, (size.1 as u32) * 8),
            );
        }
        _ => {}
    }
}

fn respond_osc(seq: &[u8], out: &mut Vec<u8>) {
    // OSC 10/11 color queries: report the chrome's text / background colors
    // so apps that theme against the terminal blend with the palette.
    let rgb = |triple: &str| -> String {
        let mut it = triple.split(';').filter_map(|s| s.parse::<u8>().ok());
        let (r, g, b) = (
            it.next().unwrap_or(0),
            it.next().unwrap_or(0),
            it.next().unwrap_or(0),
        );
        format!("rgb:{r:02x}{r:02x}/{g:02x}{g:02x}/{b:02x}{b:02x}")
    };
    if seq == b"10;?" {
        let _ = std::io::Write::write_fmt(
            out,
            format_args!("\x1b]10;{}\x1b\\", rgb(superzej_core::theme::TEXT)),
        );
    } else if seq == b"11;?" {
        let _ = std::io::Write::write_fmt(
            out,
            format_args!("\x1b]11;{}\x1b\\", rgb(superzej_core::theme::BG0)),
        );
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
        let _ = std::io::Write::write_fmt(out, format_args!("\x1b_Gi={id};ENOTSUPPORTED:\x1b\\"));
    }
}

/// Collect OSC sequences an inner app emits that must be forwarded VERBATIM
/// to the outer terminal: OSC 52 (clipboard set — e.g. `vim "+y` inside a
/// pane) — the host's emulator would otherwise swallow them.
pub fn osc_passthrough(bytes: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    let mut i = 0;
    while i + 1 < bytes.len() {
        if bytes[i] == 0x1b && bytes[i + 1] == b']' {
            let body = &bytes[i + 2..];
            if let Some((seq, len)) = osc_seq(body) {
                if seq.starts_with(b"52;") {
                    out.extend_from_slice(&bytes[i..i + 2 + len]);
                }
                i += 2 + len;
                continue;
            }
        }
        i += 1;
    }
    out
}

/// A private control message the bundled yazi drawer emits back to the host on
/// its own PTY stream via `OSC 5379`. yazi owns all its keys (so `q`/`Esc` stay
/// literal in its input fields); these commands let a yazi keybind drive the
/// host chrome without the host having to intercept — and mis-steal — keys.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DrawerCmd {
    /// Hide the drawer into the keep-alive pool (yazi keeps running; position
    /// survives the reopen). Bound to `q` in yazi's manager mode.
    Close,
    /// Open this (absolute) path in the center editor tab. Bound to `<C-e>`.
    Editor(String),
}

/// Private OSC number for the drawer→host control channel. Chosen high to avoid
/// colliding with any standard OSC; the vt100 emulator ignores it on `feed`.
const DRAWER_OSC: &[u8] = b"5379;";

/// Scan a drawer pane's output chunk for the first `OSC 5379;<cmd>` control
/// message and decode it. Returns `None` for ordinary output. Only called for
/// bytes from the drawer pane, so unrelated OSCs never reach here.
pub fn drawer_command(bytes: &[u8]) -> Option<DrawerCmd> {
    let mut i = 0;
    while i + 1 < bytes.len() {
        if bytes[i] == 0x1b && bytes[i + 1] == b']' {
            let body = &bytes[i + 2..];
            if let Some((seq, len)) = osc_seq(body) {
                if let Some(rest) = seq.strip_prefix(DRAWER_OSC) {
                    return decode_drawer_cmd(rest);
                }
                i += 2 + len;
                continue;
            }
        }
        i += 1;
    }
    None
}

fn decode_drawer_cmd(rest: &[u8]) -> Option<DrawerCmd> {
    if rest == b"close" {
        return Some(DrawerCmd::Close);
    }
    if let Some(path) = rest.strip_prefix(b"editor;") {
        let path = String::from_utf8_lossy(path).into_owned();
        if !path.is_empty() {
            return Some(DrawerCmd::Editor(path));
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn resp(bytes: &[u8]) -> Vec<u8> {
        query_responses(bytes, (4, 9), (24, 80))
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
    fn osc_color_queries_report_theme_colors() {
        let bg = resp(b"\x1b]11;?\x07");
        let s = String::from_utf8(bg).unwrap();
        assert!(s.starts_with("\x1b]11;rgb:"), "{s:?}");
        let fg = resp(b"\x1b]10;?\x1b\\");
        assert!(String::from_utf8(fg).unwrap().starts_with("\x1b]10;rgb:"));
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

    #[test]
    fn drawer_command_decodes_close_and_editor() {
        // BEL-terminated close.
        assert_eq!(
            drawer_command(b"\x1b]5379;close\x07"),
            Some(DrawerCmd::Close)
        );
        // ST-terminated editor with an absolute path, framed by ordinary output.
        assert_eq!(
            drawer_command(b"noise\x1b]5379;editor;/home/u/a q.rs\x1b\\more"),
            Some(DrawerCmd::Editor("/home/u/a q.rs".into()))
        );
    }

    #[test]
    fn drawer_command_ignores_unrelated_or_malformed() {
        // A different OSC number is not ours.
        assert_eq!(drawer_command(b"\x1b]52;c;aGk=\x07"), None);
        // Unterminated sequence: no command yet.
        assert_eq!(drawer_command(b"\x1b]5379;close"), None);
        // Unknown verb / empty editor path.
        assert_eq!(drawer_command(b"\x1b]5379;bogus\x07"), None);
        assert_eq!(drawer_command(b"\x1b]5379;editor;\x07"), None);
        // Ordinary output.
        assert_eq!(drawer_command(b"just some text\r\n"), None);
    }

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
}
