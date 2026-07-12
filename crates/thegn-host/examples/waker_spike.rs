//! Waker spike: proves the compositor's 0%-idle event model on this
//! platform's termwiz backend — the loop blocks in `poll_input(None)` (no
//! timeout, no tick) and an **off-thread** `TerminalWaker` pulse must wake it.
//! This is the load-bearing assumption of the whole event loop; run this on a
//! new platform (esp. Windows Terminal) before trusting the compositor there.
//!
//! ```sh
//! cargo run -p thegn-host --example waker_spike
//! ```
//!
//! PASS looks like: a `tick N (waker)` line printing once per second with the
//! process at ~0% CPU between lines, and every keypress echoing immediately.
//! FAIL looks like: no tick lines (waker lost — the loop never wakes without
//! input) or pegged CPU (the backend degraded to polling). Press `q` to exit.

use std::time::Duration;

use termwiz::caps::Capabilities;
use termwiz::input::{InputEvent, KeyCode, KeyEvent};
use termwiz::terminal::{Terminal, new_terminal};

fn main() -> anyhow::Result<()> {
    let caps = Capabilities::new_from_env()?;
    let mut term = new_terminal(caps)?;
    term.set_raw_mode()?;
    let waker = term.waker();

    // Off-thread producer: exactly what PTY readers / hydration / watchers do.
    let pulse = std::thread::spawn(move || {
        for _ in 0..30 {
            std::thread::sleep(Duration::from_secs(1));
            if waker.wake().is_err() {
                return;
            }
        }
    });

    let mut ticks = 0u32;
    print!("waker spike: expect one tick/second at ~0%% CPU; press q to quit\r\n");
    loop {
        // THE invariant under test: block forever until input OR waker.
        match term.poll_input(None)? {
            Some(InputEvent::Wake) => {
                ticks += 1;
                print!("tick {ticks} (waker)\r\n");
                if ticks >= 30 {
                    break;
                }
            }
            Some(InputEvent::Key(KeyEvent {
                key: KeyCode::Char('q') | KeyCode::Char('\u{3}'), // q / Ctrl+C
                ..
            })) => break,
            Some(ev) => print!("input: {ev:?}\r\n"),
            None => break, // EOF/terminal gone
        }
    }
    term.set_cooked_mode()?;
    drop(term);
    let _ = pulse.join();
    println!("done: {ticks} waker ticks observed");
    Ok(())
}
