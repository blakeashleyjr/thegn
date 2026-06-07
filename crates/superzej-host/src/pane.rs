//! A single PTY-backed pane: a child process on a pseudo-terminal, its emulator
//! grid, and an input writer. The reader runs on a blocking thread that funnels
//! bytes into a channel (portable-pty masters are blocking file handles — one
//! reader per pane, never a `select!` over N masters in the event loop).

use anyhow::{Context, Result};
use portable_pty::{Child, CommandBuilder, MasterPty, PtySize};
use std::io::Write;
use std::sync::mpsc::{Receiver, Sender};

use crate::emulator::{PaneEmulator, Vt100Emulator};

/// What a pane's reader thread emits (tagged with the pane id so one shared
/// channel multiplexes every pane's output to the event loop).
pub enum PaneEvent {
    /// PTY output bytes for pane `id`.
    Output(u32, Vec<u8>),
    /// Pane `id`'s child exited (or the PTY closed); its reader thread is done.
    Exit(u32),
}

pub struct PtyPane {
    master: Box<dyn MasterPty + Send>,
    #[allow(dead_code)]
    child: Box<dyn Child + Send + Sync>,
    writer: Box<dyn Write + Send>,
    emulator: Box<dyn PaneEmulator>,
    rows: u16,
    cols: u16,
}

impl PtyPane {
    /// Spawn `argv` (already composed by `sandbox::enter_argv`) in `cwd` on a
    /// fresh PTY of `rows`x`cols`. Reader-thread events arrive on `tx`, tagged
    /// with `id` so a shared channel can carry every pane's output.
    pub fn spawn(
        id: u32,
        argv: &[String],
        cwd: Option<&std::path::Path>,
        rows: u16,
        cols: u16,
        tx: Sender<PaneEvent>,
    ) -> Result<Self> {
        let pty = portable_pty::native_pty_system();
        let pair = pty
            .openpty(PtySize {
                rows,
                cols,
                pixel_width: 0,
                pixel_height: 0,
            })
            .context("openpty")?;

        let mut cmd = CommandBuilder::new(&argv[0]);
        cmd.args(&argv[1..]);
        if let Some(dir) = cwd {
            cmd.cwd(dir);
        }
        cmd.env("TERM", "xterm-256color");
        let child = pair.slave.spawn_command(cmd).context("spawn child")?;
        // Drop the slave so the master sees EOF when the child exits.
        drop(pair.slave);

        let writer = pair.master.take_writer().context("take_writer")?;
        let mut reader = pair.master.try_clone_reader().context("clone_reader")?;

        std::thread::spawn(move || {
            let mut buf = [0u8; 8192];
            loop {
                match reader.read(&mut buf) {
                    Ok(0) => {
                        let _ = tx.send(PaneEvent::Exit(id));
                        break;
                    }
                    Ok(n) => {
                        if tx.send(PaneEvent::Output(id, buf[..n].to_vec())).is_err() {
                            break; // consumer gone
                        }
                    }
                    Err(_) => {
                        let _ = tx.send(PaneEvent::Exit(id));
                        break;
                    }
                }
            }
        });

        Ok(Self {
            master: pair.master,
            child,
            writer,
            emulator: Box::new(Vt100Emulator::new(rows, cols, 10_000)),
            rows,
            cols,
        })
    }

    /// Feed PTY output into the emulator grid (drain-without-render is just this
    /// without a subsequent compose).
    pub fn feed(&mut self, bytes: &[u8]) {
        self.emulator.advance(bytes);
    }

    /// Forward user input bytes to the child. Typing snaps the viewport back to
    /// the live tail (the usual terminal behavior when scrolled into history).
    pub fn write_input(&mut self, bytes: &[u8]) -> Result<()> {
        if self.emulator.scrollback() > 0 {
            self.emulator.scroll_reset();
        }
        self.writer.write_all(bytes).context("pty write")?;
        self.writer.flush().ok();
        Ok(())
    }

    /// Resize the PTY and the emulator together.
    pub fn resize(&mut self, rows: u16, cols: u16) -> Result<()> {
        self.master
            .resize(PtySize {
                rows,
                cols,
                pixel_width: 0,
                pixel_height: 0,
            })
            .context("pty resize")?;
        self.emulator.resize(rows, cols);
        self.rows = rows;
        self.cols = cols;
        Ok(())
    }

    #[allow(dead_code)]
    pub fn size(&self) -> (u16, u16) {
        (self.rows, self.cols)
    }

    pub fn emulator(&self) -> &dyn PaneEmulator {
        self.emulator.as_ref()
    }

    /// Scroll the pane's viewport into/out of scrollback history.
    pub fn scroll_up(&mut self, n: usize) {
        self.emulator.scroll_up(n);
    }
    pub fn scroll_down(&mut self, n: usize) {
        self.emulator.scroll_down(n);
    }
}

/// Block (with a deadline) draining `rx` into the pane until the child exits or
/// the deadline passes. Test/helper for headless round-trips; the interactive
/// loop drains the same channel via `tokio::select!`.
#[allow(dead_code)]
pub fn drain_until_exit(pane: &mut PtyPane, rx: &Receiver<PaneEvent>, deadline_ms: u64) -> bool {
    use std::time::{Duration, Instant};
    let start = Instant::now();
    loop {
        let remaining = deadline_ms.saturating_sub(start.elapsed().as_millis() as u64);
        if remaining == 0 {
            return false;
        }
        match rx.recv_timeout(Duration::from_millis(remaining)) {
            Ok(PaneEvent::Output(_, b)) => pane.feed(&b),
            Ok(PaneEvent::Exit(_)) => return true,
            Err(_) => return false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc::channel;

    fn sh(script: &str) -> Vec<String> {
        vec!["/bin/sh".into(), "-c".into(), script.into()]
    }

    #[test]
    fn pty_round_trip_lands_output_in_grid() {
        let (tx, rx) = channel();
        let mut pane = PtyPane::spawn(0, &sh("printf 'hello-pty'"), None, 24, 80, tx).unwrap();
        assert!(drain_until_exit(&mut pane, &rx, 5000), "child should exit");
        assert_eq!(pane.emulator().row_text(0), "hello-pty");
    }

    #[test]
    fn resize_propagates_to_child_via_winsize() {
        // `stty size` prints "rows cols" read from the PTY winsize.
        let (tx, rx) = channel();
        let mut pane = PtyPane::spawn(0, &sh("stty size"), None, 30, 100, tx).unwrap();
        assert!(drain_until_exit(&mut pane, &rx, 5000));
        assert_eq!(pane.emulator().row_text(0), "30 100");
    }

    #[test]
    fn backpressure_does_not_deadlock_on_a_flood() {
        // A chatty child must not block the reader; we drain a bounded window
        // and drop the pane (reader thread exits when the channel sender errors).
        let (tx, rx) = channel();
        let mut pane =
            PtyPane::spawn(0, &sh("yes superzej | head -c 200000"), None, 24, 80, tx).unwrap();
        let exited = drain_until_exit(&mut pane, &rx, 5000);
        assert!(
            exited,
            "flood should drain and the child should exit cleanly"
        );
        // The flood scrolled through the grid; some visible row holds the token.
        let emu = pane.emulator();
        let (rows, _) = emu.size();
        let seen = (0..rows).any(|r| emu.row_text(r).contains("superzej"));
        assert!(
            seen,
            "expected the repeated token somewhere in the visible grid"
        );
    }
}
