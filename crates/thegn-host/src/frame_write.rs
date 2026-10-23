//! Termwiz debug-renderer frame encoding and terminal-write resilience.
//!
//! A transient write hiccup to the controlling terminal — `EIO`/`EINTR`/`EAGAIN`,
//! e.g. a subprocess briefly disturbing the terminal's foreground process group
//! during worktree creation — must NOT tear the compositor down. The render path
//! used to `?`-propagate any flush error out of the event loop; that drops the
//! `BufferedTerminal`, and termwiz's `UnixTerminal::drop` does `flush().unwrap()`
//! — a second EIO there panics, leaving the screen frozen with the tty echoing.
//! Frames are rendered to bytes here and sent through the writer FIFO, so a
//! terminal write cannot block the event loop and stays ordered with OOB writes
//! (THE-344).

use anyhow::Context as _;
use std::collections::VecDeque;
use termwiz::input::InputEvent;
use termwiz::render::RenderTty;
use termwiz::render::terminfo::TerminfoRenderer;
use termwiz::surface::Change;
use termwiz::terminal::Terminal;
use termwiz::terminal::buffered::BufferedTerminal;

/// Consecutive transient write failures tolerated before the loop gives up and
/// tears down (a truly gone terminal, not a transient blip).
pub(crate) const RETRY_MAX: u32 = 30;

/// Loop-held termwiz renderer. It only encodes bytes; frames must ride the
/// writer FIFO so terminal I/O stays off the event loop and ordered with OOB
/// writes (THE-344).
pub(crate) struct TermwizFrames {
    caps: termwiz::caps::Capabilities,
    renderer: TerminfoRenderer,
}

impl TermwizFrames {
    pub(crate) fn with_caps(caps: termwiz::caps::Capabilities) -> Self {
        let renderer = TerminfoRenderer::new(caps.clone());
        Self { caps, renderer }
    }

    pub(crate) fn from_env() -> anyhow::Result<Self> {
        let caps = termwiz::caps::Capabilities::new_from_env().context("term capabilities")?;
        Ok(Self::with_caps(caps))
    }

    pub(crate) fn reset(&mut self) {
        self.renderer = TerminfoRenderer::new(self.caps.clone());
    }

    pub(crate) fn render(
        &mut self,
        wire: &[Change],
        cols: usize,
        rows: usize,
    ) -> anyhow::Result<Vec<u8>> {
        let mut out = FrameBytes {
            buf: Vec::new(),
            cols,
            rows,
        };
        self.renderer
            .render_to(wire, &mut out)
            .context("termwiz render")?;
        Ok(out.buf)
    }
}

struct FrameBytes {
    buf: Vec<u8>,
    cols: usize,
    rows: usize,
}

impl std::io::Write for FrameBytes {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.buf.extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl RenderTty for FrameBytes {
    fn get_size_in_cells(&mut self) -> termwiz::Result<(usize, usize)> {
        Ok((self.cols, self.rows))
    }
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

/// Release the termwiz terminal after the explicit bounded teardown path.
///
/// When the raw restore owns teardown, termwiz cleanup and `UnixTerminal::drop`
/// would write and flush synchronously without a bound. The process is exiting,
/// so forget the buffer and let process exit release its tty descriptors and
/// SIGWINCH registration. If no raw restore was captured, preserve termwiz's
/// cleanup behaviour as the only available restoration path.
pub(crate) fn release_terminal<T: Terminal>(mut buf: BufferedTerminal<T>, raw_restore_owned: bool) {
    if raw_restore_owned {
        std::mem::forget(buf);
        return;
    }

    if let Err(error) = buf.terminal().exit_alternate_screen() {
        tracing::debug!(target: "thegn::shutdown", %error, "termwiz alternate-screen cleanup failed");
    }
    if let Err(error) = buf.terminal().set_cooked_mode() {
        tracing::debug!(target: "thegn::shutdown", %error, "termwiz cooked-mode cleanup failed");
    }
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
    marker: &mut crate::preview_retry::PendingReadyMarker,
    submit: impl FnMut(Vec<u8>) -> Result<(), Vec<u8>>,
) {
    if pty_input_pending(buf, pending_input) {
        marker.cancel();
    } else {
        // Through the writer FIFO, so the marker lands AFTER the frame whose
        // stability it certifies (never mid-frame).
        marker.offer(submit);
    }
}

pub(crate) fn pty_input_pending<T: Terminal>(
    buf: &mut BufferedTerminal<T>,
    pending_input: &mut VecDeque<InputEvent>,
) -> bool {
    let in_queue = pending_input.iter().any(|e| {
        matches!(
            e,
            InputEvent::Key(_) | InputEvent::Mouse(_) | InputEvent::Paste(_)
        )
    });
    if in_queue {
        return true;
    }
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
                    return true;
                }
            }
            _ => return false,
        }
    }
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
        let error = std::io::Error::from_raw_os_error(libc::EIO);
        assert!(is_transient_io(&error));
    }

    #[test]
    fn classifies_interrupted_and_wouldblock_as_transient() {
        for kind in [
            std::io::ErrorKind::Interrupted,
            std::io::ErrorKind::WouldBlock,
        ] {
            let error = std::io::Error::from(kind);
            assert!(is_transient_io(&error), "{kind:?} should be transient");
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
        let broken = std::io::Error::from(std::io::ErrorKind::BrokenPipe);
        assert!(!is_transient_io(&broken));
    }

    #[test]
    fn termwiz_frame_renders_text_to_bytes_without_io() {
        let caps =
            termwiz::caps::Capabilities::new_with_hints(termwiz::caps::ProbeHints::default())
                .unwrap();
        let mut frames = TermwizFrames::with_caps(caps);
        let bytes = frames
            .render(&[Change::Text("hello".into())], 80, 24)
            .unwrap();
        assert!(bytes.windows(5).any(|window| window == b"hello"));
    }

    #[test]
    fn termwiz_frame_clear_screen_uses_the_given_size() {
        let caps =
            termwiz::caps::Capabilities::new_with_hints(termwiz::caps::ProbeHints::default())
                .unwrap();
        let mut frames = TermwizFrames::with_caps(caps);
        let bytes = frames
            .render(
                &[
                    Change::ClearScreen(Default::default()),
                    Change::Text("x".into()),
                ],
                10,
                3,
            )
            .unwrap();
        assert!(
            !bytes.is_empty(),
            "clear uses the adapter size, not a tty query"
        );
    }

    #[test]
    fn termwiz_frame_reset_restarts_attribute_state() {
        let caps =
            termwiz::caps::Capabilities::new_with_hints(termwiz::caps::ProbeHints::default())
                .unwrap();
        let mut frames = TermwizFrames::with_caps(caps);
        frames
            .render(
                &[
                    Change::Attribute(termwiz::cell::AttributeChange::Intensity(
                        termwiz::cell::Intensity::Bold,
                    )),
                    Change::Text("a".into()),
                ],
                80,
                24,
            )
            .unwrap();
        frames.reset();
        let bytes = frames.render(&[Change::Text("b".into())], 80, 24).unwrap();
        assert!(bytes.contains(&b'b'));
    }
}
