//! Frame output + terminal-write resilience.
//!
//! A transient write hiccup to the controlling terminal — `EIO`/`EINTR`/`EAGAIN`,
//! e.g. a subprocess briefly disturbing the terminal's foreground process group
//! during worktree creation — must NOT tear the compositor down. The render path
//! used to `?`-propagate any flush error out of the event loop; that drops the
//! `BufferedTerminal`, and termwiz's `UnixTerminal::drop` does `flush().unwrap()`
//! — a second EIO there panics, leaving the screen frozen with the tty echoing.
//! Instead the loop skips the frame, forces a full repaint, and retries; only a
//! persistent failure (retries exhausted) or a genuinely fatal error tears down.

use anyhow::Context as _;
use std::collections::VecDeque;
use termwiz::input::InputEvent;
use termwiz::surface::Change;
use termwiz::terminal::Terminal;
use termwiz::terminal::buffered::BufferedTerminal;

/// Consecutive transient write failures tolerated before the loop gives up and
/// tears down (a truly gone terminal, not a transient blip).
pub(crate) const RETRY_MAX: u32 = 30;

/// Outcome of writing one frame to the terminal.
pub(crate) enum FrameWrite {
    /// Delivered.
    Ok,
    /// A transient tty condition (EIO/EINTR/EAGAIN): retry after a full repaint.
    Transient,
    /// Unrecoverable — propagate and tear down.
    Fatal(anyhow::Error),
}

/// Enter the alternate screen NOW, not at the next termwiz flush.
///
/// termwiz's `enter_alternate_screen` only appends `?1049h` to its userspace
/// write buffer. The wire renderer writes frames straight to fd 1 and never
/// flushes termwiz, so without this flush the `?1049h` sat buffered until
/// teardown and the whole session drew on the PRIMARY screen — where alacritty
/// scrolls the viewport into history on every ED 2, banking a stale frame in
/// scrollback on each full repaint (resize, refocus, drawer toggle, Redraw).
pub(crate) fn enter_alt_screen<T: Terminal>(term: &mut T) -> anyhow::Result<()> {
    term.enter_alternate_screen()?;
    term.flush()?;
    Ok(())
}

/// Render + flush one frame's `wire` change list, then ring the latched bell.
/// Any write failure is classified as [`FrameWrite::Transient`] (retry) or
/// [`FrameWrite::Fatal`] (propagate).
pub(crate) fn emit_frame<T: Terminal>(
    use_termwiz_renderer: bool,
    buf: &mut BufferedTerminal<T>,
    wire_renderer: &mut crate::wire::WireRenderer,
    recorder: &mut Option<crate::recorder::Recorder>,
    wire: &[Change],
    ring_bell: bool,
) -> FrameWrite {
    let result = if use_termwiz_renderer {
        buf.terminal()
            .render(wire)
            .context("render")
            .and_then(|()| buf.terminal().flush().context("terminal flush"))
    } else {
        let mut bytes = String::new();
        wire_renderer.render(wire, &mut bytes);
        if let Some(rec) = recorder {
            let _ = rec.write_frame(&bytes); // best-effort: recorder write: recording is advisory; a lost frame degrades replay fidelity only
        }
        use std::io::Write as _;
        let mut out = std::io::stdout();
        out.write_all(bytes.as_bytes())
            .context("render")
            .and_then(|()| out.flush().context("terminal flush"))
    };
    if let Err(e) = result {
        return if is_transient_write_error(&e) {
            FrameWrite::Transient
        } else {
            FrameWrite::Fatal(e)
        };
    }
    // Terminal bell (item 429): a latched notification sound, written right after
    // the frame flush so it never interleaves with the diff writes. BEL neither
    // moves the cursor nor prints, so it is safe between frames. Best-effort.
    if ring_bell {
        use std::io::Write as _;
        let mut out = std::io::stdout();
        let _ = out.write_all(b"\x07"); // best-effort: stdout write: EPIPE on a closed |head pipe is normal
        let _ = out.flush(); // best-effort: flush: display-only
    }
    FrameWrite::Ok
}

/// After a frame is flushed, emit the muse sync `ready` marker — but only once
/// the event queue is fully drained (both the in-memory queue AND the terminal's
/// own read buffer). Emitting mid-batch (e.g. 51 `a` chars each causing a render)
/// would fire a premature "stable" in the muse sync state machine, capturing only
/// partial state. Only KEY/MOUSE/PASTE events defer the marker — Wake/Resize
/// events (background ticks, hydration) must never block it or startup spins
/// indefinitely waiting for an empty queue.
pub(crate) fn emit_muse_ready_marker<T: Terminal>(
    buf: &mut BufferedTerminal<T>,
    pending_input: &mut VecDeque<InputEvent>,
    writer: &crate::frame_writer::FrameWriter,
) {
    let has_pty_pending = {
        let in_queue = pending_input.iter().any(|e| {
            matches!(
                e,
                InputEvent::Key(_) | InputEvent::Mouse(_) | InputEvent::Paste(_)
            )
        });
        if in_queue {
            true
        } else {
            let mut found = false;
            #[allow(clippy::while_let_loop)]
            loop {
                match buf.terminal().poll_input(Some(std::time::Duration::ZERO)) {
                    Ok(Some(ev)) => {
                        let is_pty = matches!(
                            ev,
                            InputEvent::Key(_) | InputEvent::Mouse(_) | InputEvent::Paste(_)
                        );
                        pending_input.push_back(ev);
                        if is_pty {
                            found = true;
                            break;
                        }
                    }
                    _ => break,
                }
            }
            found
        }
    };
    if !has_pty_pending {
        // Through the writer FIFO, so the marker lands AFTER the frame whose
        // stability it certifies (never mid-frame).
        writer.submit_oob(b"\x1b]5379;muse:ready\x07".to_vec());
    }
}

/// True when `err` (or anything in its source chain) is a transient terminal
/// write condition worth retrying rather than tearing the compositor down.
fn is_transient_write_error(err: &anyhow::Error) -> bool {
    err.chain().any(|cause| {
        cause
            .downcast_ref::<std::io::Error>()
            .is_some_and(is_transient_io)
    })
}

pub(crate) fn is_transient_io(io: &std::io::Error) -> bool {
    use std::io::ErrorKind;
    if matches!(io.kind(), ErrorKind::Interrupted | ErrorKind::WouldBlock) {
        return true;
    }
    // Raw-errno tail: EIO from a briefly-disturbed tty is worth retrying on
    // unix. Windows has no tty errno equivalent; the ErrorKind arm above is
    // the whole classification there.
    #[cfg(unix)]
    {
        matches!(
            io.raw_os_error(),
            Some(libc::EIO | libc::EINTR | libc::EAGAIN)
        )
    }
    #[cfg(not(unix))]
    {
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)] // raw-errno classification is unix-only
    #[test]
    fn classifies_eio_as_transient() {
        let e = anyhow::Error::new(std::io::Error::from_raw_os_error(libc::EIO)).context("flush");
        assert!(is_transient_write_error(&e));
    }

    #[test]
    fn classifies_interrupted_and_wouldblock_as_transient() {
        for kind in [
            std::io::ErrorKind::Interrupted,
            std::io::ErrorKind::WouldBlock,
        ] {
            let e = anyhow::Error::new(std::io::Error::from(kind)).context("write");
            assert!(is_transient_write_error(&e), "{kind:?} should be transient");
        }
    }

    /// Records the `Terminal` calls `enter_alt_screen` makes, in order.
    #[derive(Default)]
    struct RecordingTerminal {
        calls: Vec<&'static str>,
        fail_enter: bool,
        fail_flush: bool,
    }

    impl Terminal for RecordingTerminal {
        fn set_raw_mode(&mut self) -> termwiz::Result<()> {
            unimplemented!()
        }
        fn set_cooked_mode(&mut self) -> termwiz::Result<()> {
            unimplemented!()
        }
        fn enter_alternate_screen(&mut self) -> termwiz::Result<()> {
            self.calls.push("enter");
            if self.fail_enter {
                return Err(std::io::Error::other("enter failed").into());
            }
            Ok(())
        }
        fn exit_alternate_screen(&mut self) -> termwiz::Result<()> {
            unimplemented!()
        }
        fn get_screen_size(&mut self) -> termwiz::Result<termwiz::terminal::ScreenSize> {
            unimplemented!()
        }
        fn set_screen_size(&mut self, _: termwiz::terminal::ScreenSize) -> termwiz::Result<()> {
            unimplemented!()
        }
        fn render(&mut self, _: &[Change]) -> termwiz::Result<()> {
            unimplemented!()
        }
        fn flush(&mut self) -> termwiz::Result<()> {
            self.calls.push("flush");
            if self.fail_flush {
                return Err(std::io::Error::other("flush failed").into());
            }
            Ok(())
        }
        fn poll_input(
            &mut self,
            _: Option<std::time::Duration>,
        ) -> termwiz::Result<Option<InputEvent>> {
            unimplemented!()
        }
        fn waker(&self) -> termwiz::terminal::TerminalWaker {
            unimplemented!()
        }
    }

    #[test]
    fn enter_alt_screen_flushes_after_entering() {
        // The `?1049h` must reach the tty before any frame: frames bypass
        // termwiz's buffer (direct fd-1 writes), so an unflushed enter would
        // leave the whole session on the primary screen.
        let mut term = RecordingTerminal::default();
        enter_alt_screen(&mut term).unwrap();
        assert_eq!(term.calls, ["enter", "flush"]);
    }

    #[test]
    fn enter_alt_screen_stops_when_enter_fails() {
        let mut term = RecordingTerminal {
            fail_enter: true,
            ..Default::default()
        };
        let error = enter_alt_screen(&mut term).unwrap_err();
        assert!(error.to_string().contains("enter failed"));
        assert_eq!(term.calls, ["enter"]);
    }

    #[test]
    fn enter_alt_screen_propagates_flush_failure() {
        let mut term = RecordingTerminal {
            fail_flush: true,
            ..Default::default()
        };
        let error = enter_alt_screen(&mut term).unwrap_err();
        assert!(error.to_string().contains("flush failed"));
        assert_eq!(term.calls, ["enter", "flush"]);
    }

    #[test]
    fn classifies_broken_pipe_and_non_io_as_fatal() {
        let broken =
            anyhow::Error::new(std::io::Error::from(std::io::ErrorKind::BrokenPipe)).context("x");
        assert!(!is_transient_write_error(&broken));
        assert!(!is_transient_write_error(&anyhow::anyhow!(
            "plain string error"
        )));
    }
}
