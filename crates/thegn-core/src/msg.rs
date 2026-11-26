//! Branded diagnostics. Once `crate::log::init` has installed the `tracing`
//! subscriber, `info`/`warn`/`error` route through it (level-filtered, mirrored
//! to the log file with the same `✦ thegn` look). Before that — and always
//! for `die` — they print straight to stderr so early config errors and fatals
//! are never lost.
//!
//! Most code should keep calling these; new code wanting per-module filtering or
//! `debug!`/`trace!` can use `tracing` macros directly.
#![allow(clippy::disallowed_macros)] // this module is the stderr fallback

use crate::theme;
use std::borrow::Cow;
use std::io::IsTerminal;
use std::sync::atomic::{AtomicBool, Ordering};

/// Set true while the native compositor owns the raw/alternate screen. When set,
/// `info`/`warn`/`error` route to `tracing` instead of `eprintln!` — a direct
/// stderr write would paint over the alt-screen frame (the same reason
/// `log_trace::Role::Host` drops its stderr layer). This matters even without
/// `THEGN_LOG`: no user-facing *sink* is installed and `ready()` is false, so
/// the plain fallback would corrupt the frame (e.g. off-loop provisioning
/// `msg::warn`s). The always-on WARN+ ring layer (`log_trace::install`) still
/// captures the routed WARN/ERROR into memory for crash reports even with no
/// sink, which is exactly the drop this guard now prevents. `die` still writes
/// to stderr — a fatal must be seen even if it scars the frame on exit.
static TUI_ACTIVE: AtomicBool = AtomicBool::new(false);

/// Toggle the alt-screen guard (see `TUI_ACTIVE`). The compositor sets it true
/// right after entering the alternate screen and false on teardown.
pub fn set_tui_active(active: bool) {
    TUI_ACTIVE.store(active, Ordering::Relaxed);
}

/// Whether the compositor owns the raw/alternate screen (see `TUI_ACTIVE`).
/// Callers that spawn subprocesses with inheritable stdio consult this to
/// capture their output instead of letting it paint over the frame.
pub fn tui_active() -> bool {
    TUI_ACTIVE.load(Ordering::Relaxed)
}

/// A "✦ thegn" prefix in the given hue (faint star + faint name), tty-gated.
fn tag(hue: &str) -> String {
    if std::io::stderr().is_terminal() {
        format!(
            "\x1b[38;2;{}m\u{2726}\x1b[0m \x1b[38;2;{hue}mthegn\x1b[0m",
            theme::MAGENTA
        )
    } else {
        "thegn:".into()
    }
}

/// The stderr render boundary for diagnostics (THE-507); multi-line messages keep their newlines.
pub fn terminal_safe(s: &str) -> Cow<'_, str> {
    let mut output = None::<String>;
    let mut copied_until = 0;

    for (byte_index, c) in s.char_indices() {
        if (c.is_control() && c != '\n' && c != '\t')
            || crate::pipeline_report::is_format_control(c)
        {
            let escaped = output.get_or_insert_with(|| String::with_capacity(s.len()));
            escaped.push_str(&s[copied_until..byte_index]);
            escaped.push('\u{fffd}');
            copied_until = byte_index + c.len_utf8();
        }
    }

    match output {
        Some(mut output) => {
            output.push_str(&s[copied_until..]);
            Cow::Owned(output)
        }
        None => Cow::Borrowed(s),
    }
}

pub fn info(s: &str) {
    let s = terminal_safe(s);
    if crate::log_trace::ready() || tui_active() {
        tracing::info!("{s}");
    } else {
        write_stderr_line(&format!("{} {s}", tag(theme::DIM)));
    }
}

pub fn warn(s: &str) {
    let s = terminal_safe(s);
    if crate::log_trace::ready() || tui_active() {
        tracing::warn!("{s}");
    } else {
        write_stderr_line(&format!("{} {s}", tag(theme::AMBER)));
    }
}

pub fn error(s: &str) {
    let s = terminal_safe(s);
    if crate::log_trace::ready() || tui_active() {
        tracing::error!("{s}");
    } else {
        write_stderr_line(&format!("{} {s}", tag(theme::RED)));
    }
}

/// Print an error and exit non-zero. Always goes straight to stderr (branded),
/// regardless of the subscriber or level filter — a fatal must be seen.
pub fn die(s: &str) -> ! {
    let s = terminal_safe(s);
    write_stderr_line(&format!("{} {s}", tag(theme::RED)));
    std::process::exit(1);
}

/// Write one line to stderr without panicking. `eprintln!` panics when the
/// write fails, which is what a vanished terminal (EIO, EPIPE) does on a
/// shutdown or error path; here the failure is dropped instead, so the caller
/// exits on its own terms rather than with a crash report.
pub fn write_stderr_line(line: &str) {
    // best-effort: the terminal may already be gone, so a failed write is dropped
    let _ = write_line_to(&mut std::io::stderr().lock(), line);
}

/// Write `line` and a newline to `writer`, returning the first failure.
pub fn write_line_to<W: std::io::Write + ?Sized>(
    writer: &mut W,
    line: &str,
) -> std::io::Result<()> {
    writer.write_all(line.as_bytes())?;
    writer.write_all(b"\n")
}

/// True when an I/O error means the controlling terminal or its pipe has gone
/// away (a hangup): `EIO` from a torn-down tty, or a broken pipe.
pub fn is_hangup(error: &std::io::Error) -> bool {
    if error.kind() == std::io::ErrorKind::BrokenPipe {
        return true;
    }
    matches!(error.raw_os_error(), Some(ERRNO_EIO | ERRNO_EPIPE))
}

// `libc` is only a macOS dependency of this crate, so spell out the errnos.
// EIO is 5 and EPIPE is 32 on Linux, macOS and the Windows CRT alike.
const ERRNO_EIO: i32 = 5;
const ERRNO_EPIPE: i32 = 32;

#[cfg(test)]
mod tests {
    use super::terminal_safe;
    use std::borrow::Cow;

    #[test]
    fn terminal_safe_replaces_escape_c1_and_format_controls() {
        let input = "before\u{1b}]52;c;SGVsbG8=\u{7} mid \u{1b}[2J c1 \u{9b}\u{9d} del \u{7f} bidi \u{202e}\u{2066} zero \u{200b} sep \u{2028} after";
        let expected = "before\u{fffd}]52;c;SGVsbG8=\u{fffd} mid \u{fffd}[2J c1 \u{fffd}\u{fffd} del \u{fffd} bidi \u{fffd}\u{fffd} zero \u{fffd} sep \u{fffd} after";
        let safe = terminal_safe(input);

        assert_eq!(safe, expected);
        for hostile in [
            '\u{1b}', '\u{7}', '\u{9b}', '\u{9d}', '\u{7f}', '\u{202e}', '\u{2066}', '\u{200b}',
            '\u{2028}',
        ] {
            assert!(
                !safe.contains(hostile),
                "hostile character {hostile:?} survived"
            );
        }
    }

    #[test]
    fn terminal_safe_keeps_newlines_tabs_and_borrows_clean_text() {
        for input in ["a\tb\nc", "plain ascii / ünïcödé"] {
            let safe = terminal_safe(input);
            assert_eq!(safe, input);
            assert!(matches!(safe, Cow::Borrowed(_)));
        }
    }

    #[test]
    fn terminal_safe_handles_every_c0_and_c1_control() {
        let mut failures = Vec::new();
        for codepoint in (0x00..=0x1f).chain([0x7f]).chain(0x80..=0x9f) {
            let character = char::from_u32(codepoint).expect("control codepoint is a scalar");
            let text = character.to_string();
            let safe = terminal_safe(&text);
            let expected = if character == '\n' || character == '\t' {
                character
            } else {
                '\u{fffd}'
            };
            if safe != expected.to_string() {
                failures.push(codepoint);
            }
        }

        assert!(failures.is_empty(), "controls not handled: {failures:?}");
    }

    /// A writer whose every write fails with `EIO`, like a torn-down tty.
    struct EioWriter;

    impl std::io::Write for EioWriter {
        fn write(&mut self, _buf: &[u8]) -> std::io::Result<usize> {
            Err(std::io::Error::from_raw_os_error(super::ERRNO_EIO))
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Err(std::io::Error::from_raw_os_error(super::ERRNO_EIO))
        }
    }

    #[test]
    fn write_line_to_eio_writer_returns_error_without_panicking() {
        let result = super::write_line_to(&mut EioWriter, "gone");
        let error = result.expect_err("EIO must surface as an error, not a panic");
        assert!(super::is_hangup(&error));
    }

    #[test]
    fn write_line_to_vec_appends_newline() {
        let mut out: Vec<u8> = Vec::new();
        super::write_line_to(&mut out, "hello").expect("vec write");
        assert_eq!(out, b"hello\n");
    }

    #[test]
    fn is_hangup_recognises_terminal_loss_only() {
        assert!(super::is_hangup(&std::io::Error::from_raw_os_error(
            super::ERRNO_EIO
        )));
        assert!(super::is_hangup(&std::io::Error::from_raw_os_error(
            super::ERRNO_EPIPE
        )));
        assert!(super::is_hangup(&std::io::Error::from(
            std::io::ErrorKind::BrokenPipe
        )));
        assert!(!super::is_hangup(&std::io::Error::from(
            std::io::ErrorKind::NotFound
        )));
    }
}
