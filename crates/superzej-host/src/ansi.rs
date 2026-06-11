//! Minimal ANSI SGR parsing for chrome-rendered rich text (the syntect-
//! highlighted diff). Turns a line containing `ESC[…m` truecolor sequences
//! into plain-text spans with resolved colors so the renderer can place them
//! in cells — drawing the raw string would print the escapes literally.
//!
//! Only what `superzej_core::diff_highlight` emits is interpreted: `0` reset,
//! `1` bold (ignored), `38;2;r;g;b` / `48;2;r;g;b` truecolor, `39`/`49`
//! defaults. Unknown SGR params and non-SGR escape sequences are dropped.

use termwiz::color::ColorAttribute;

#[derive(Debug, Clone, PartialEq)]
pub struct AnsiSpan {
    pub text: String,
    /// `None` = inherit the caller's default for that slot.
    pub fg: Option<ColorAttribute>,
    pub bg: Option<ColorAttribute>,
}

fn rgb(r: u8, g: u8, b: u8) -> ColorAttribute {
    crate::chrome::theme_color(&format!("{r};{g};{b}"))
}

/// The standard xterm 256-color palette, resolved to truecolor: 16 base ANSI
/// colors, the 6×6×6 cube, then the 24-step grayscale ramp.
fn xterm256_rgb(n: u8) -> ColorAttribute {
    const BASE: [(u8, u8, u8); 16] = [
        (0, 0, 0),
        (205, 0, 0),
        (0, 205, 0),
        (205, 205, 0),
        (0, 0, 238),
        (205, 0, 205),
        (0, 205, 205),
        (229, 229, 229),
        (127, 127, 127),
        (255, 0, 0),
        (0, 255, 0),
        (255, 255, 0),
        (92, 92, 255),
        (255, 0, 255),
        (0, 255, 255),
        (255, 255, 255),
    ];
    let (r, g, b) = match n {
        0..=15 => BASE[n as usize],
        16..=231 => {
            let v = n - 16;
            let step = |c: u8| if c == 0 { 0 } else { 55 + c * 40 };
            (step(v / 36), step((v / 6) % 6), step(v % 6))
        }
        232..=255 => {
            let g = 8 + (n - 232) * 10;
            (g, g, g)
        }
    };
    rgb(r, g, b)
}

/// Split one line (no newlines) into colored spans. Always returns at least
/// one span for non-empty visible text; empty input yields an empty vec.
pub fn parse_spans(line: &str) -> Vec<AnsiSpan> {
    let mut spans: Vec<AnsiSpan> = Vec::new();
    let mut cur = String::new();
    let mut fg: Option<ColorAttribute> = None;
    let mut bg: Option<ColorAttribute> = None;
    let mut chars = line.chars().peekable();

    let mut push = |text: &mut String, fg: Option<ColorAttribute>, bg: Option<ColorAttribute>| {
        if !text.is_empty() {
            spans.push(AnsiSpan {
                text: std::mem::take(text),
                fg,
                bg,
            });
        }
    };

    while let Some(c) = chars.next() {
        if c != '\u{1b}' {
            if !c.is_control() {
                cur.push(c);
            }
            continue;
        }
        // ESC: only CSI sequences are interesting; drop anything else.
        if chars.peek() != Some(&'[') {
            continue;
        }
        chars.next(); // consume '['
        let mut params = String::new();
        let mut terminator = '\0';
        for t in chars.by_ref() {
            if t.is_ascii_alphabetic() {
                terminator = t;
                break;
            }
            params.push(t);
        }
        if terminator != 'm' {
            continue; // not SGR — ignored
        }
        push(&mut cur, fg, bg);
        let nums: Vec<u16> = params
            .split(';')
            .map(|p| p.parse::<u16>().unwrap_or(0))
            .collect();
        let mut i = 0;
        while i < nums.len() {
            match nums[i] {
                0 => {
                    fg = None;
                    bg = None;
                }
                // 256-color (`38;5;N`) BEFORE the truecolor arm — bat and
                // many tools emit the indexed form by default.
                38 | 48 if nums.get(i + 1) == Some(&5) && i + 2 < nums.len() => {
                    let c = xterm256_rgb(nums[i + 2] as u8);
                    if nums[i] == 38 {
                        fg = Some(c);
                    } else {
                        bg = Some(c);
                    }
                    i += 2;
                }
                38 | 48 if nums.get(i + 1) == Some(&2) && i + 4 < nums.len() => {
                    let c = rgb(nums[i + 2] as u8, nums[i + 3] as u8, nums[i + 4] as u8);
                    if nums[i] == 38 {
                        fg = Some(c);
                    } else {
                        bg = Some(c);
                    }
                    i += 4;
                }
                39 => fg = None,
                49 => bg = None,
                _ => {} // bold/italic/etc: not modeled in chrome text
            }
            i += 1;
        }
    }
    push(&mut cur, fg, bg);
    spans
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_text_is_one_uncolored_span() {
        let s = parse_spans("hello");
        assert_eq!(s.len(), 1);
        assert_eq!(s[0].text, "hello");
        assert_eq!(s[0].fg, None);
        assert_eq!(s[0].bg, None);
    }

    #[test]
    fn truecolor_fg_bg_and_reset() {
        let line = "\x1b[38;2;121;227;165m+\x1b[0m\x1b[48;2;28;46;40mcode\x1b[0m tail";
        let s = parse_spans(line);
        assert_eq!(
            s.iter().map(|x| x.text.as_str()).collect::<Vec<_>>(),
            vec!["+", "code", " tail"]
        );
        assert_eq!(s[0].fg, Some(rgb(121, 227, 165)));
        assert_eq!(s[0].bg, None);
        assert_eq!(s[1].bg, Some(rgb(28, 46, 40)));
        assert_eq!(s[2].fg, None);
        assert_eq!(s[2].bg, None);
    }

    #[test]
    fn indexed_256_colors_resolve_to_truecolor() {
        // bat's default theme uses indexed colors: cube + grayscale + base.
        let s = parse_spans("\x1b[38;5;196mred\x1b[48;5;244mgrey\x1b[0mplain");
        assert_eq!(s.len(), 3);
        assert_eq!(s[0].fg, Some(rgb(255, 0, 0))); // 196 = cube (5,0,0)
        assert_eq!(s[1].bg, Some(rgb(128, 128, 128))); // 244 = grayscale
        assert_eq!(s[2].fg, None);
        // Indexed params never mis-trigger the reset/default arms.
        let t = parse_spans("\x1b[38;5;0mzero\x1b[38;5;39mblue");
        assert_eq!(t[0].fg, Some(rgb(0, 0, 0)));
        assert!(t[1].fg.is_some());
    }

    #[test]
    fn unknown_sequences_are_dropped_not_printed() {
        // Bold, a non-SGR CSI, and a bare ESC must not leak into the text.
        let s = parse_spans("\x1b[1mbold\x1b[2Kx\x1by");
        let text: String = s.iter().map(|x| x.text.as_str()).collect();
        assert_eq!(text, "boldxy");
    }

    #[test]
    fn empty_and_escape_only_lines_yield_no_spans() {
        assert!(parse_spans("").is_empty());
        assert!(parse_spans("\x1b[0m\x1b[38;2;1;2;3m").is_empty());
    }
}
