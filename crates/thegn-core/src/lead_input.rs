//! What a Lead-confined caller may type into a worker (THE-2187 follow-up).
//!
//! The Lead may answer a worker it launched that is waiting on a question
//! (`sessions.input`, see [`crate::lead_confine`]). It may only type plain
//! text and press Enter: a raw control sequence would let it drive the
//! worker's terminal (cursor and mode changes, OSC clipboard/title writes,
//! a bracketed-paste end marker smuggled inside a paste, a Ctrl-C/Ctrl-Z to
//! the foreground job). [`check`] is that filter, applied to the exact bytes
//! that would reach the PTY.
//!
//! Pure and platform-neutral.

/// The largest input a Lead-confined `sessions.input` may carry, in bytes.
pub const MAX_LEAD_INPUT_BYTES: usize = 16 * 1024;

/// Why Lead input was refused. Never carries the input itself: the text
/// may hold a secret, and refusals are audited and shown.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InputRefusal {
    TooLarge {
        len: usize,
    },
    NotUtf8,
    /// An ESC byte: the start of every CSI, OSC, DCS and bracketed-paste
    /// marker.
    Escape,
    /// Any other control character (C0, DEL or C1) but newline, carriage
    /// return and tab.
    Control(u32),
}

impl std::fmt::Display for InputRefusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            InputRefusal::TooLarge { len } => write!(
                f,
                "input is {len} bytes; the Lead's limit is {MAX_LEAD_INPUT_BYTES}"
            ),
            InputRefusal::NotUtf8 => f.write_str("the Lead may only send UTF-8 text"),
            InputRefusal::Escape => f.write_str(
                "escape sequences are refused for the Lead; send plain text (and Enter)",
            ),
            InputRefusal::Control(c) => write!(
                f,
                "control character U+{c:04X} is refused for the Lead; only newline, carriage \
                 return and tab are allowed"
            ),
        }
    }
}

impl std::error::Error for InputRefusal {}

/// Whether `c` is allowed in Lead input: printable text, newline, carriage
/// return and tab.
fn allowed(c: char) -> Result<(), InputRefusal> {
    match c {
        '\n' | '\r' | '\t' => Ok(()),
        '\u{1b}' => Err(InputRefusal::Escape),
        c if c.is_control() => Err(InputRefusal::Control(u32::from(c))),
        _ => Ok(()),
    }
}

/// Check the exact bytes a Lead-confined caller would write to a PTY: at most
/// [`MAX_LEAD_INPUT_BYTES`], valid UTF-8, no ESC and no control character but
/// `\n`, `\r` and `\t`. A C1 control (U+0080..U+009F, including the
/// single-character CSI U+009B and OSC U+009D) is refused like any other.
pub fn check(bytes: &[u8]) -> Result<(), InputRefusal> {
    if bytes.len() > MAX_LEAD_INPUT_BYTES {
        return Err(InputRefusal::TooLarge { len: bytes.len() });
    }
    let text = std::str::from_utf8(bytes).map_err(|_| InputRefusal::NotUtf8)?;
    text.chars().try_for_each(allowed)
}

#[cfg(test)]
mod tests {
    use super::{InputRefusal, MAX_LEAD_INPUT_BYTES, check};

    #[test]
    fn plain_text_newline_return_and_tab_are_allowed() {
        assert_eq!(check(b""), Ok(()));
        assert_eq!(check(b"yes, go ahead\r"), Ok(()));
        assert_eq!(check(b"line one\nline two\n"), Ok(()));
        assert_eq!(check(b"a\tb"), Ok(()));
        assert_eq!(check("caf\u{e9} \u{2713} \u{1f600}".as_bytes()), Ok(()));
    }

    #[test]
    fn escape_csi_osc_and_bracketed_paste_are_refused() {
        for input in [
            &b"\x1b"[..],
            b"\x1b[A",
            b"ok\x1b[2J",
            b"\x1b]52;c;aGk=\x07",
            b"\x1b]0;title\x1b\\",
            b"\x1b[201~rm -rf ~\r",
            b"\x1bP1$r\x1b\\",
        ] {
            assert_eq!(check(input), Err(InputRefusal::Escape), "{input:?}");
        }
    }

    #[test]
    fn nul_and_other_c0_controls_are_refused() {
        assert_eq!(check(b"a\0b"), Err(InputRefusal::Control(0)));
        assert_eq!(check(b"\x03"), Err(InputRefusal::Control(3)));
        assert_eq!(check(b"\x1a"), Err(InputRefusal::Control(0x1a)));
        assert_eq!(check(b"\x07"), Err(InputRefusal::Control(7)));
        assert_eq!(check(b"\x08"), Err(InputRefusal::Control(8)));
        assert_eq!(check(b"\x7f"), Err(InputRefusal::Control(0x7f)));
        for byte in 0u8..0x20 {
            let got = check(&[byte]);
            match byte {
                b'\n' | b'\r' | b'\t' => assert_eq!(got, Ok(()), "{byte:#x}"),
                _ => assert!(got.is_err(), "{byte:#x}"),
            }
        }
    }

    #[test]
    fn c1_controls_including_single_char_csi_and_osc_are_refused() {
        for c in ['\u{80}', '\u{9b}', '\u{9d}', '\u{90}', '\u{9f}'] {
            let mut buf = [0u8; 4];
            let encoded = c.encode_utf8(&mut buf).as_bytes();
            assert_eq!(
                check(encoded),
                Err(InputRefusal::Control(u32::from(c))),
                "{c:?}"
            );
        }
    }

    #[test]
    fn non_utf8_is_refused() {
        assert_eq!(check(b"\x9b2J"), Err(InputRefusal::NotUtf8));
        assert_eq!(check(b"\xff"), Err(InputRefusal::NotUtf8));
    }

    #[test]
    fn oversize_input_is_refused() {
        let max = "x".repeat(MAX_LEAD_INPUT_BYTES);
        assert_eq!(check(max.as_bytes()), Ok(()));
        let over = "x".repeat(MAX_LEAD_INPUT_BYTES + 1);
        assert_eq!(
            check(over.as_bytes()),
            Err(InputRefusal::TooLarge {
                len: MAX_LEAD_INPUT_BYTES + 1
            })
        );
    }

    #[test]
    fn refusals_never_echo_the_input() {
        let secret = "hunter2\x1b[A";
        let msg = check(secret.as_bytes()).unwrap_err().to_string();
        assert!(!msg.contains("hunter2"), "{msg}");
        let msg = check(b"hunter2\x03").unwrap_err().to_string();
        assert!(!msg.contains("hunter2"), "{msg}");
    }
}
