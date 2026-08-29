//! `thegn attach [session]` — the local thin client.
//!
//! This is herdr's "SSH in and just run one command" ergonomic: the pane
//! daemon owns the terminals, and `thegn attach` is a *client* of it over the
//! local unix socket (peer-cred auth — no bearer token, no TCP). With no
//! argument it lists what's live; with a session id it grabs that session
//! interactively — raw-mode keystrokes go to the PTY, the PTY's bytes paint
//! the screen, and detaching (Ctrl-\) or the session exiting leaves the work
//! running under the daemon's warm relay lease. Local-only by construction:
//! it never dials the TCP `serve` listener (`ControlAddr::Unix` only).
//!
//! Interactive input is decoded by termwiz and re-encoded with the same
//! `crate::input::key_bytes` the compositor uses to feed its own panes, so a
//! key reaches the daemon PTY identically whether typed here or in the TUI.

use anyhow::{Context, Result};
use std::io::Write as _;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use termwiz::caps::Capabilities;
use termwiz::input::{InputEvent, KeyCode, KeyEvent, Modifiers};
use termwiz::terminal::{Terminal, new_terminal};
use thegn_core::config::Config;
use thegn_core::control_wire::EventFrame;
use thegn_core::outln;
use thegn_svc::control::client::AttachControl;

/// What the blocking input thread forwards to the async pump.
enum FromTty {
    Input(Vec<u8>),
    Resize {
        rows: u16,
        cols: u16,
    },
    /// The detach chord (Ctrl-\): leave the session running, tear down the UI.
    Detach,
}

pub fn run(cfg: &Config, session: Option<String>) -> Result<()> {
    let rt = tokio::runtime::Runtime::new()?;
    rt.block_on(run_async(cfg, session))
}

async fn run_async(cfg: &Config, session: Option<String>) -> Result<()> {
    let client = crate::cmd::session::connect(cfg).await?;
    match session {
        None => {
            // "SSH in and just run `thegn attach`": show what's live so the
            // user can pick a session to attach to.
            let sessions = client.sessions().await?;
            if sessions.is_empty() {
                outln!("no live sessions — start work in the compositor, then `thegn attach <id>`");
            } else {
                for s in &sessions {
                    outln!("{}", crate::cmd::session::session_line(s));
                }
                outln!("\nattach with: thegn attach <id>");
            }
            Ok(())
        }
        Some(session) => interactive(client, &session).await,
    }
}

/// Grab one session interactively over the local socket. Raw TTY in, PTY bytes
/// out, until the session exits or the user detaches (Ctrl-\).
async fn interactive(
    client: thegn_svc::control::client::ControlClient,
    session: &str,
) -> Result<()> {
    // Terminal first (cooked → we only need the size); attach BEFORE touching
    // raw mode so an attach failure never leaves the tty altered.
    let caps = Capabilities::new_from_env().context("term capabilities")?;
    let mut term = new_terminal(caps).context("open terminal")?;
    let size = term.get_screen_size().context("screen size")?;
    let (rows, cols) = (size.rows as u16, size.cols as u16);

    let client_id = format!("cli-attach-{}", std::process::id());
    let mut stream = client
        .attach(session, &client_id, rows, cols, false)
        .await
        .with_context(|| format!("attach to session {session}"))?;

    // Now own the screen. The daemon sends a full PaneSnapshot as the first
    // frame, so we don't clear — its repaint is authoritative.
    term.set_raw_mode().context("raw mode")?;
    term.enter_alternate_screen().context("alt screen")?;

    // The input thread blocks on `poll_input(None)`; pulse this waker to make it
    // return so it can observe `done` and tear the terminal down cleanly.
    let waker = term.waker();
    let done = Arc::new(AtomicBool::new(false));
    let (tty_tx, mut tty_rx) = tokio::sync::mpsc::channel::<FromTty>(256);
    let input_thread = {
        let done = done.clone();
        std::thread::spawn(move || input_loop(term, tty_tx, done))
    };

    let mut out = std::io::stdout();
    let exit_note;
    loop {
        tokio::select! {
            frame = stream.frames.recv() => match frame {
                Some(EventFrame::PaneSnapshot { bytes, .. })
                | Some(EventFrame::PaneDelta { bytes, .. }) => {
                    out.write_all(&bytes)?;
                    out.flush()?;
                }
                Some(EventFrame::SessionExit { code, .. }) => {
                    exit_note = Some(format!(
                        "[session exited: {}]",
                        code.map_or("?".into(), |c| c.to_string())
                    ));
                    break;
                }
                // Daemon hung up (restart / kill). Stop; the session is gone.
                None => { exit_note = Some("[disconnected]".into()); break; }
                _ => {}
            },
            msg = tty_rx.recv() => match msg {
                Some(FromTty::Input(bytes)) => {
                    let _ = stream.control.send(AttachControl::Input(bytes)).await; // best-effort: send: the consumer may be gone; a closed channel is the consumer going away
                }
                Some(FromTty::Resize { rows, cols }) => {
                    let _ = stream.control.send(AttachControl::Resize { rows, cols }).await; // best-effort: send: the consumer may be gone; a closed channel is the consumer going away
                }
                Some(FromTty::Detach) => {
                    let _ = stream.control.send(AttachControl::Close).await; // best-effort: send: the consumer may be gone; a closed channel is the consumer going away
                    exit_note = Some("[detached — session still running]".into());
                    break;
                }
                // Input thread died: nothing left to drive it.
                None => { exit_note = None; break; }
            },
        }
    }

    // Tear the terminal down: signal the input thread and wake its blocking
    // poll so it exits the alt screen + restores cooked mode, then join it.
    done.store(true, Ordering::SeqCst);
    let _ = waker.wake(); // best-effort: waker pulse: an input nudge must never fail the calling path
    let _ = input_thread.join(); // best-effort: thread join: a panicked helper loses its output, not the caller

    if let Some(note) = exit_note {
        outln!("{note}");
    }
    Ok(())
}

/// Blocking input reader: owns the terminal, forwards decoded keystrokes /
/// resizes to the async pump, and restores the terminal on the way out. Ends
/// when `done` is set (pumped via a waker) or the terminal errors.
fn input_loop<T: Terminal>(
    mut term: T,
    tx: tokio::sync::mpsc::Sender<FromTty>,
    done: Arc<AtomicBool>,
) {
    loop {
        if done.load(Ordering::SeqCst) {
            break;
        }
        match term.poll_input(None) {
            Ok(Some(InputEvent::Key(k))) => {
                if is_detach_chord(&k) {
                    let _ = tx.blocking_send(FromTty::Detach); // best-effort: send: the consumer may be gone; a closed channel is the consumer going away
                    break;
                }
                if let Some(bytes) = crate::input::key_bytes(&k.key, k.modifiers)
                    && tx.blocking_send(FromTty::Input(bytes)).is_err()
                {
                    break;
                }
            }
            Ok(Some(InputEvent::Paste(text))) => {
                if tx.blocking_send(FromTty::Input(text.into_bytes())).is_err() {
                    break;
                }
            }
            Ok(Some(InputEvent::Resized { rows, cols })) => {
                // best-effort: send: the consumer may be gone; a closed channel is the consumer going away
                let _ = tx.blocking_send(FromTty::Resize {
                    rows: rows as u16,
                    cols: cols as u16,
                });
            }
            // Wake (the teardown pulse) / mouse (no passthrough yet): re-check
            // `done` at the top of the loop.
            Ok(Some(_)) | Ok(None) => {}
            Err(_) => break,
        }
    }
    // Best-effort restore: leave the alt screen; termwiz restores cooked mode
    // when the terminal drops.
    let _ = term.exit_alternate_screen();
    let _ = term.flush(); // best-effort: flush: display-only
}

/// The detach chord: Ctrl-\ (`0x1c`), spelled either as `Char('\\')`+CTRL or as
/// the raw control char depending on the terminal's keyboard reporting.
fn is_detach_chord(k: &KeyEvent) -> bool {
    matches!(&k.key, KeyCode::Char('\\')) && k.modifiers.contains(Modifiers::CTRL)
        || matches!(&k.key, KeyCode::Char('\u{1c}'))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(code: KeyCode, mods: Modifiers) -> KeyEvent {
        KeyEvent {
            key: code,
            modifiers: mods,
        }
    }

    #[test]
    fn detach_chord_both_spellings() {
        // Ctrl-\ as a modified char…
        assert!(is_detach_chord(&key(KeyCode::Char('\\'), Modifiers::CTRL)));
        // …and as the raw control byte 0x1c some terminals report.
        assert!(is_detach_chord(&key(
            KeyCode::Char('\u{1c}'),
            Modifiers::NONE
        )));
    }

    #[test]
    fn plain_backslash_is_not_detach() {
        assert!(!is_detach_chord(&key(KeyCode::Char('\\'), Modifiers::NONE)));
        assert!(!is_detach_chord(&key(KeyCode::Char('q'), Modifiers::CTRL)));
    }
}
