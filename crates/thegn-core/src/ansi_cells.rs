//! A pure, substrate-free parser for the SGR-ANSI subset an external structural
//! differ (difftastic) emits, turning it into styled cell runs the compositor
//! can render.
//!
//! Why a bespoke parser rather than feeding difft's bytes to the terminal: the
//! diff view composes in truecolor and quantizes once at the `wire.rs`
//! chokepoint, and — crucially — **untrusted file content flows through difft**.
//! Any escape sequence this parser does not understand is *stripped*, never
//! forwarded, so a malicious file cannot smuggle cursor moves, OSC title writes,
//! or other terminal control through the diff surface. Colors are resolved to
//! RGB here (ANSI-16 and the xterm-256 cube by fixed formula, truecolor
//! verbatim) so the host maps a run to a `Tok::Rgb` seg with no color literal at
//! a draw site.
//!
//! This module is I/O-free and under the core 95% line gate; fixtures below are
//! recorded difft-shaped output.

/// A resolved 24-bit colour.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rgb {
    pub r: u8,
    pub g: u8,
    pub b: u8,
}

impl Rgb {
    pub const fn new(r: u8, g: u8, b: u8) -> Rgb {
        Rgb { r, g, b }
    }
    /// `(r, g, b)` for the host's `Tok::Rgb`.
    pub fn tuple(self) -> (u8, u8, u8) {
        (self.r, self.g, self.b)
    }
}

/// The visual style of a run of cells. `None` colours mean "the surface default"
/// (the compositor's fg/bg), never a literal.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct CellStyle {
    pub fg: Option<Rgb>,
    pub bg: Option<Rgb>,
    pub bold: bool,
    pub dim: bool,
    pub italic: bool,
    pub underline: bool,
}

/// One styled run of text (no embedded newline).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StyledRun {
    pub text: String,
    pub style: CellStyle,
}

/// A rendered line: zero or more styled runs. An empty `Vec` is a blank line.
pub type StyledLine = Vec<StyledRun>;

/// Parse SGR-ANSI `input` into styled lines. Newlines split lines; carriage
/// returns and every other C0 control (and every non-SGR escape) are stripped.
pub fn parse_ansi(input: &str) -> Vec<StyledLine> {
    let mut lines: Vec<StyledLine> = Vec::new();
    let mut cur: StyledLine = Vec::new();
    let mut style = CellStyle::default();
    let mut run = String::new();

    // Flush the pending text into the current line under the *current* style.
    let flush = |run: &mut String, cur: &mut StyledLine, style: &CellStyle| {
        if !run.is_empty() {
            cur.push(StyledRun {
                text: std::mem::take(run),
                style: *style,
            });
        }
    };

    let mut chars = input.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\u{1b}' => {
                // An escape: only CSI-SGR (`ESC [ … m`) changes style; everything
                // else (other CSI, OSC, 2-char escapes) is consumed and dropped.
                match chars.peek() {
                    Some('[') => {
                        chars.next();
                        let (params, final_byte) = read_csi(&mut chars);
                        if final_byte == Some('m') {
                            flush(&mut run, &mut cur, &style);
                            apply_sgr(&params, &mut style);
                        }
                        // Non-`m` CSI (cursor moves, erases, …) is dropped.
                    }
                    Some(']') => {
                        chars.next();
                        consume_osc(&mut chars);
                    }
                    Some(_) => {
                        chars.next(); // drop the single following byte (e.g. ESC c)
                    }
                    None => {}
                }
            }
            '\n' => {
                flush(&mut run, &mut cur, &style);
                lines.push(std::mem::take(&mut cur));
            }
            '\r' => {} // strip
            '\t' => run.push('\t'),
            c if (c as u32) < 0x20 => {} // other C0 controls: strip
            c => run.push(c),
        }
    }
    flush(&mut run, &mut cur, &style);
    if !cur.is_empty() {
        lines.push(cur);
    }
    lines
}

/// Read a CSI parameter/intermediate sequence after the `ESC [`, returning the
/// numeric-ish parameter string and the final byte (0x40..=0x7e). Stops at the
/// final byte (consumed) or at input end.
fn read_csi(chars: &mut std::iter::Peekable<std::str::Chars>) -> (String, Option<char>) {
    let mut params = String::new();
    for c in chars.by_ref() {
        if ('\u{40}'..='\u{7e}').contains(&c) {
            return (params, Some(c));
        }
        // Parameter (0x30–0x3f) and intermediate (0x20–0x2f) bytes.
        params.push(c);
    }
    (params, None)
}

/// Consume an OSC string (`ESC ] … BEL` or `… ESC \`), dropping it entirely.
fn consume_osc(chars: &mut std::iter::Peekable<std::str::Chars>) {
    while let Some(c) = chars.next() {
        if c == '\u{07}' {
            return; // BEL terminator
        }
        if c == '\u{1b}' {
            // ST is `ESC \`; swallow the backslash if present.
            if chars.peek() == Some(&'\\') {
                chars.next();
            }
            return;
        }
    }
}

/// Apply one SGR parameter string (the part before `m`) to `style`.
fn apply_sgr(params: &str, style: &mut CellStyle) {
    // Empty params (`ESC [ m`) means reset, same as `0`.
    let nums: Vec<i64> = if params.is_empty() {
        vec![0]
    } else {
        params
            .split(';')
            .map(|p| p.trim().parse::<i64>().unwrap_or(0))
            .collect()
    };
    let mut i = 0;
    while i < nums.len() {
        match nums[i] {
            0 => *style = CellStyle::default(),
            1 => style.bold = true,
            2 => style.dim = true,
            3 => style.italic = true,
            4 => style.underline = true,
            22 => {
                style.bold = false;
                style.dim = false;
            }
            23 => style.italic = false,
            24 => style.underline = false,
            30..=37 => style.fg = Some(ansi16((nums[i] - 30) as u8)),
            39 => style.fg = None,
            40..=47 => style.bg = Some(ansi16((nums[i] - 40) as u8)),
            49 => style.bg = None,
            90..=97 => style.fg = Some(ansi16((nums[i] - 90 + 8) as u8)),
            100..=107 => style.bg = Some(ansi16((nums[i] - 100 + 8) as u8)),
            38 => {
                if let Some((rgb, used)) = read_extended(&nums[i + 1..]) {
                    style.fg = Some(rgb);
                    i += used;
                } else {
                    break; // malformed extended colour → stop parsing this run
                }
            }
            48 => {
                if let Some((rgb, used)) = read_extended(&nums[i + 1..]) {
                    style.bg = Some(rgb);
                    i += used;
                } else {
                    break;
                }
            }
            _ => {} // unknown attribute: ignore
        }
        i += 1;
    }
}

/// Parse the tail of a `38`/`48` colour: `5;n` (256-colour) or `2;r;g;b`
/// (truecolor). Returns the colour and how many params it consumed.
fn read_extended(rest: &[i64]) -> Option<(Rgb, usize)> {
    match rest.first()? {
        5 => {
            let n = *rest.get(1)? as u8;
            Some((xterm256(n), 2))
        }
        2 => {
            let r = clamp8(*rest.get(1)?);
            let g = clamp8(*rest.get(2)?);
            let b = clamp8(*rest.get(3)?);
            Some((Rgb::new(r, g, b), 4))
        }
        _ => None,
    }
}

fn clamp8(v: i64) -> u8 {
    v.clamp(0, 255) as u8
}

/// The standard xterm palette for the 16 ANSI colours (indices 0–15).
fn ansi16(n: u8) -> Rgb {
    const P: [(u8, u8, u8); 16] = [
        (0, 0, 0),
        (128, 0, 0),
        (0, 128, 0),
        (128, 128, 0),
        (0, 0, 128),
        (128, 0, 128),
        (0, 128, 128),
        (192, 192, 192),
        (128, 128, 128),
        (255, 0, 0),
        (0, 255, 0),
        (255, 255, 0),
        (0, 0, 255),
        (255, 0, 255),
        (0, 255, 255),
        (255, 255, 255),
    ];
    let (r, g, b) = P[(n & 0x0f) as usize];
    Rgb::new(r, g, b)
}

/// The xterm 256-colour cube → RGB (0–15 palette, 16–231 6×6×6 cube, 232–255
/// grayscale ramp) by the standard formula.
fn xterm256(n: u8) -> Rgb {
    match n {
        0..=15 => ansi16(n),
        16..=231 => {
            let n = n - 16;
            let comp = |v: u8| if v == 0 { 0 } else { 55 + 40 * v };
            Rgb::new(comp(n / 36 % 6), comp(n / 6 % 6), comp(n % 6))
        }
        _ => {
            let v = 8 + 10 * (n - 232);
            Rgb::new(v, v, v)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn one_line(s: &str) -> StyledLine {
        let ls = parse_ansi(s);
        assert_eq!(ls.len(), 1, "expected one line: {ls:?}");
        ls.into_iter().next().unwrap()
    }

    #[test]
    fn plain_text_is_one_unstyled_run() {
        let line = one_line("hello world");
        assert_eq!(line.len(), 1);
        assert_eq!(line[0].text, "hello world");
        assert_eq!(line[0].style, CellStyle::default());
    }

    #[test]
    fn basic_ansi_colours_resolve_to_rgb() {
        // difft-shaped: a red-fg run, reset, then a green-fg run.
        let line = one_line("\u{1b}[31mdel\u{1b}[0m \u{1b}[32madd\u{1b}[0m");
        assert_eq!(line.len(), 3);
        assert_eq!(line[0].text, "del");
        assert_eq!(line[0].style.fg, Some(Rgb::new(128, 0, 0)));
        assert_eq!(line[1].text, " ");
        assert_eq!(line[1].style, CellStyle::default());
        assert_eq!(line[2].text, "add");
        assert_eq!(line[2].style.fg, Some(Rgb::new(0, 128, 0)));
    }

    #[test]
    fn bright_and_attributes() {
        let line = one_line("\u{1b}[1;91mBOLD\u{1b}[0m");
        assert_eq!(line[0].text, "BOLD");
        assert!(line[0].style.bold);
        assert_eq!(line[0].style.fg, Some(Rgb::new(255, 0, 0))); // bright red
    }

    #[test]
    fn truecolor_and_256() {
        let tc = one_line("\u{1b}[38;2;10;20;30mX\u{1b}[0m");
        assert_eq!(tc[0].style.fg, Some(Rgb::new(10, 20, 30)));
        // 256-colour index 196 is pure red in the cube.
        let c = one_line("\u{1b}[38;5;196mR\u{1b}[0m");
        assert_eq!(c[0].style.fg, Some(Rgb::new(255, 0, 0)));
        // Grayscale ramp entry.
        let g = one_line("\u{1b}[48;5;232mG\u{1b}[0m");
        assert_eq!(g[0].style.bg, Some(Rgb::new(8, 8, 8)));
    }

    #[test]
    fn background_and_default_reset() {
        let line = one_line("\u{1b}[41ma\u{1b}[49mb");
        assert_eq!(line[0].style.bg, Some(Rgb::new(128, 0, 0)));
        assert_eq!(line[1].style.bg, None);
    }

    #[test]
    fn newlines_split_lines_and_style_carries() {
        // difft continues a colour across a newline until reset.
        let lines = parse_ansi("\u{1b}[32ma\nb\u{1b}[0m\nc");
        assert_eq!(lines.len(), 3);
        assert_eq!(lines[0][0].text, "a");
        assert_eq!(lines[0][0].style.fg, Some(Rgb::new(0, 128, 0)));
        // Style carries onto the next line's run.
        assert_eq!(lines[1][0].text, "b");
        assert_eq!(lines[1][0].style.fg, Some(Rgb::new(0, 128, 0)));
        // Reset then plain.
        assert_eq!(lines[2][0].text, "c");
        assert_eq!(lines[2][0].style, CellStyle::default());
    }

    #[test]
    fn blank_line_between_content() {
        let lines = parse_ansi("a\n\nb");
        assert_eq!(lines.len(), 3);
        assert!(lines[1].is_empty(), "middle line is blank");
    }

    #[test]
    fn unknown_escapes_are_stripped_never_forwarded() {
        // A cursor move, an erase, an OSC title write, and a bare 2-char escape:
        // all consumed, none surface as text or style.
        let evil = "\u{1b}[2J\u{1b}[10;5Hmove\u{1b}]0;pwned\u{07}safe\u{1b}c";
        let line = one_line(evil);
        assert_eq!(line.len(), 1);
        assert_eq!(line[0].text, "movesafe");
        assert_eq!(line[0].style, CellStyle::default());
    }

    #[test]
    fn c0_controls_are_dropped_but_tab_kept() {
        let line = one_line("a\u{0007}\r\tb");
        assert_eq!(line[0].text, "a\tb");
    }

    #[test]
    fn malformed_sgr_does_not_panic() {
        // Truncated extended colour, stray semicolons, non-numeric params.
        let _ = parse_ansi("\u{1b}[38;2;10mx");
        let _ = parse_ansi("\u{1b}[;;mx");
        let _ = parse_ansi("\u{1b}[38;5mx");
        let _ = parse_ansi("\u{1b}[99mx");
        // Empty SGR resets.
        let line = one_line("\u{1b}[31mred\u{1b}[mplain");
        assert_eq!(line[1].style, CellStyle::default());
    }

    #[test]
    fn recorded_difft_snippet_parses() {
        // A compact recording of the shape difft emits for a one-line change:
        // a magenta file header, a green added token, a red removed token.
        let recorded = concat!(
            "\u{1b}[1;35msrc/lib.rs\u{1b}[0m\n",
            "\u{1b}[38;5;40m1 fn add(a: i32) -> i32 {\u{1b}[0m\n",
            "\u{1b}[38;5;160m1 fn add(a: i64) -> i64 {\u{1b}[0m\n",
        );
        let lines = parse_ansi(recorded);
        assert_eq!(lines.len(), 3);
        assert!(lines[0][0].style.bold);
        assert_eq!(lines[0][0].text, "src/lib.rs");
        assert!(lines[1][0].style.fg.is_some());
        assert!(lines[2][0].text.contains("i64"));
    }
}
