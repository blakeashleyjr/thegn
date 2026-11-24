//! Pure Windows command-line quoting (`join`) and its parsing oracle (`split`).
//!
//! Both sides of the sandbox call this: core builds the helper's command line
//! and the helper (and any test harness) parses one. The rules are the
//! MSVCRT/`CommandLineToArgvW` rules for arguments after `argv[0]`:
//!
//! * arguments are separated by spaces and tabs;
//! * a `"` toggles "in quotes", and whitespace inside quotes is literal;
//! * `2n` backslashes before a `"` are `n` literal backslashes and the quote is
//!   a delimiter; `2n + 1` backslashes are `n` literal backslashes and the quote
//!   is literal;
//! * backslashes not followed by a `"` are literal.
//!
//! [`split`] is a test oracle, not evidence: WS06-P6-01 (Wine) and WS06-P1-06
//! (Windows CI) compare these fixtures against the real API.

use std::fmt;

/// The longest command line `CreateProcessW` accepts, in UTF-16 units, excluding
/// the terminating NUL.
pub const MAX_COMMAND_LINE_UTF16_UNITS: usize = 32_766;

/// A command line that cannot be represented.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CmdlineError {
    /// A NUL character appeared in an argument; it would truncate the line.
    NulInArgument,
    /// The joined line exceeds [`MAX_COMMAND_LINE_UTF16_UNITS`] UTF-16 units.
    TooLong,
}

impl fmt::Display for CmdlineError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NulInArgument => {
                write!(f, "Windows command-line arguments cannot contain NUL")
            }
            Self::TooLong => write!(
                f,
                "Windows command line exceeds {MAX_COMMAND_LINE_UTF16_UNITS} UTF-16 units"
            ),
        }
    }
}

impl std::error::Error for CmdlineError {}

/// Join `argv` into one Windows command line that [`split`] (and the CRT parser)
/// reads back as the same arguments.
///
/// An argument is quoted when it is empty or contains a space, a tab or a quote.
/// Quotes are included because an embedded quote is only literal inside a
/// quoted argument, which is where the doubled backslashes apply.
pub fn join(argv: &[String]) -> Result<String, CmdlineError> {
    let mut command_line = String::new();
    for (index, arg) in argv.iter().enumerate() {
        if arg.contains('\0') {
            return Err(CmdlineError::NulInArgument);
        }
        if index > 0 {
            command_line.push(' ');
        }
        if needs_quotes(arg) {
            push_quoted(&mut command_line, arg);
        } else {
            command_line.push_str(arg);
        }
    }

    if command_line.encode_utf16().count() > MAX_COMMAND_LINE_UTF16_UNITS {
        return Err(CmdlineError::TooLong);
    }
    Ok(command_line)
}

/// Parse `command_line` with the reference rules. Test oracle only.
pub fn split(command_line: &str) -> Vec<String> {
    let chars: Vec<char> = command_line.chars().collect();
    let mut args = Vec::new();
    let mut pos = 0;

    loop {
        while pos < chars.len() && is_separator(chars[pos]) {
            pos += 1;
        }
        if pos >= chars.len() {
            return args;
        }

        let mut arg = String::new();
        let mut quoted = false;
        loop {
            let mut backslashes = 0;
            while pos < chars.len() && chars[pos] == '\\' {
                backslashes += 1;
                pos += 1;
            }
            if pos >= chars.len() {
                // Trailing backslashes are literal: nothing follows to escape.
                arg.extend(std::iter::repeat_n('\\', backslashes));
                break;
            }
            match chars[pos] {
                '"' if backslashes.is_multiple_of(2) => {
                    // An even run is literal backslashes plus a quote delimiter.
                    // A doubled quote inside quotes is one literal quote.
                    arg.extend(std::iter::repeat_n('\\', backslashes / 2));
                    pos += 1;
                    if quoted && chars.get(pos) == Some(&'"') {
                        arg.push('"');
                        pos += 1;
                    } else {
                        quoted = !quoted;
                    }
                }
                '"' => {
                    // An odd run escapes the quote: half the backslashes are
                    // literal and the quote itself belongs to the argument.
                    arg.extend(std::iter::repeat_n('\\', backslashes / 2));
                    arg.push('"');
                    pos += 1;
                }
                ch if !quoted && is_separator(ch) => {
                    // Unquoted whitespace ends the argument; pending backslashes
                    // are literal, not escapes.
                    arg.extend(std::iter::repeat_n('\\', backslashes));
                    break;
                }
                ch => {
                    arg.extend(std::iter::repeat_n('\\', backslashes));
                    arg.push(ch);
                    pos += 1;
                }
            }
        }
        args.push(arg);
    }
}

fn needs_quotes(arg: &str) -> bool {
    arg.is_empty() || arg.contains(' ') || arg.contains('\t') || arg.contains('"')
}

fn push_quoted(out: &mut String, arg: &str) {
    out.push('"');
    let mut backslashes = 0;
    for ch in arg.chars() {
        match ch {
            '\\' => backslashes += 1,
            '"' => {
                out.extend(std::iter::repeat_n('\\', backslashes * 2 + 1));
                out.push('"');
                backslashes = 0;
            }
            _ => {
                out.extend(std::iter::repeat_n('\\', backslashes));
                out.push(ch);
                backslashes = 0;
            }
        }
    }
    // Trailing backslashes would otherwise escape the closing quote.
    out.extend(std::iter::repeat_n('\\', backslashes * 2));
    out.push('"');
}

fn is_separator(ch: char) -> bool {
    ch == ' ' || ch == '\t'
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Fragments the round-trip property draws from: plain text, both
    /// separators, the escape characters, non-ASCII, and the empty piece.
    const FRAGMENTS: [&str; 10] = ["a", "Z", "0", " ", "\t", "\\", "\"", "☃", "é", ""];

    fn next(state: &mut u64) -> u64 {
        *state = state
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        *state >> 33
    }

    #[test]
    fn join_split_roundtrip_property() {
        // Deterministic pseudo-random argv over the fragment alphabet. A
        // `proptest` dependency would be the only third-party crate this
        // serde-only crate pulled in, so the generator is in-crate.
        let mut state = 0x5eed_2138_u64;
        for _ in 0..2_000 {
            let argc = (next(&mut state) % 5) as usize;
            let mut argv = Vec::with_capacity(argc);
            for _ in 0..argc {
                let pieces = (next(&mut state) % 7) as usize;
                let mut arg = String::new();
                for _ in 0..pieces {
                    let pick = (next(&mut state) % FRAGMENTS.len() as u64) as usize;
                    arg.push_str(FRAGMENTS[pick]);
                }
                argv.push(arg);
            }
            let command_line = join(&argv).unwrap();
            assert_eq!(split(&command_line), argv, "command line {command_line:?}");
        }
    }

    #[test]
    fn join_quotes_only_what_the_parser_needs() {
        assert_eq!(join(&[]).unwrap(), "");
        assert_eq!(join(&["C:\\app.exe".to_owned()]).unwrap(), "C:\\app.exe");
        assert_eq!(join(&["".to_owned()]).unwrap(), "\"\"");
        assert_eq!(
            join(&["one".to_owned(), "two words".to_owned()]).unwrap(),
            "one \"two words\""
        );
        assert_eq!(join(&["tab\there".to_owned()]).unwrap(), "\"tab\there\"");
        // A quote is escaped inside the quotes; the run before it is doubled.
        assert_eq!(join(&["a\\\"b c".to_owned()]).unwrap(), "\"a\\\\\\\"b c\"");
        // Trailing backslashes inside a quoted argument are doubled.
        assert_eq!(
            join(&["ends with slash\\".to_owned()]).unwrap(),
            "\"ends with slash\\\\\""
        );
        assert_eq!(join(&["one \\".to_owned()]).unwrap(), "\"one \\\\\"");
    }

    #[test]
    fn split_matches_the_reference_parser() {
        assert_eq!(split(""), Vec::<String>::new());
        assert_eq!(split("  \t "), Vec::<String>::new());
        assert_eq!(split("one two"), ["one", "two"]);
        assert_eq!(split("\"\" \"two words\""), ["", "two words"]);
        assert_eq!(split("tab\there"), ["tab", "here"]);
        // Backslashes not followed by a quote are literal; whitespace still ends
        // the argument, so the pending backslash stays.
        assert_eq!(split("a\\ b"), ["a\\", "b"]);
        assert_eq!(
            split("C:\\Program Files\\app.exe"),
            ["C:\\Program", "Files\\app.exe"]
        );
        assert_eq!(
            split("\"C:\\Program Files\\app.exe\""),
            ["C:\\Program Files\\app.exe"]
        );
        // 2n+1 backslashes before a quote: n literal backslashes, literal quote.
        assert_eq!(split("\"a\\\\\\\"b\""), ["a\\\"b"]);
        // Doubled quote inside a quoted region is one literal quote.
        assert_eq!(split("\"a\"\"b\""), ["a\"b"]);
        // A trailing run of backslashes is literal.
        assert_eq!(split("\"ends\\\\\""), ["ends\\"]);
        assert_eq!(split("ends\\"), ["ends\\"]);
    }

    #[test]
    fn refuses_nul() {
        assert_eq!(join(&["a\0b".to_owned()]), Err(CmdlineError::NulInArgument));
        assert_eq!(
            join(&["ok".to_owned(), "\0".to_owned()]),
            Err(CmdlineError::NulInArgument)
        );
    }

    #[test]
    fn refuses_over_32766() {
        assert_eq!(
            join(&["x".repeat(MAX_COMMAND_LINE_UTF16_UNITS)])
                .unwrap()
                .len(),
            MAX_COMMAND_LINE_UTF16_UNITS
        );
        assert_eq!(
            join(&["x".repeat(MAX_COMMAND_LINE_UTF16_UNITS + 1)]),
            Err(CmdlineError::TooLong)
        );
        // The limit counts UTF-16 units, not bytes: 20_000 snowmen are 60_000
        // bytes but only 20_000 units, so they fit.
        assert!(
            join(&["☃".repeat(20_000)]).is_ok(),
            "a UTF-16-unit limit must not be a byte limit"
        );
        assert_eq!(
            join(&["☃".repeat(MAX_COMMAND_LINE_UTF16_UNITS + 1)]),
            Err(CmdlineError::TooLong)
        );
    }
}
