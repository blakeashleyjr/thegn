//! The compositor must reach its first diff-flushed frame inside a **real**
//! pty and exit cleanly. This is the termwiz/openpty path no CLI-level test can
//! touch: a panic in geometry, chrome layout or the first flush shows up here
//! and nowhere else.
//!
//! Ported from `test/pty-smoke.sh`, which drives `script(1)`. That exists on
//! unix only, and the script's missing-tool guard is a *skip*, so on Windows it
//! printed "skip PTY smoke: script(1) not found", exited 0, and the compositor
//! had no launch coverage on that platform at all. `portable_pty` gives the same
//! guarantee everywhere (ConPTY on Windows), and it rides `cargo nextest` with
//! the rest of the suite instead of needing a POSIX shell.

use std::io::{Read, Write};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use portable_pty::{CommandBuilder, PtySize};

/// How long a launch gets to reach its first frame and exit.
///
/// Windows is far slower here and legitimately so: a cold ConPTY plus a
/// process-scanning security agent puts the floor an order of magnitude above
/// a unix fork+exec. This bound only has to catch a *wedged* compositor.
const LAUNCH_BUDGET: Duration = if cfg!(windows) {
    Duration::from_secs(90)
} else {
    Duration::from_secs(20)
};

/// Launch the compositor at `cols`x`rows` in a fresh pty with an isolated
/// config/state/home, and return `(exited_ok, output)`.
fn launch(case: &str, cols: u16, rows: u16) -> (bool, String) {
    let root = std::env::temp_dir().join(format!(
        "thegn-pty-launch-{case}-{}-{:?}",
        std::process::id(),
        std::thread::current().id()
    ));
    let _ = std::fs::remove_dir_all(&root);
    for sub in ["home", "config", "state"] {
        std::fs::create_dir_all(root.join(sub)).expect("case dirs");
    }

    let pair = portable_pty::native_pty_system()
        .openpty(PtySize {
            rows,
            cols,
            pixel_width: 0,
            pixel_height: 0,
        })
        .expect("openpty");

    let mut cmd = CommandBuilder::new(env!("CARGO_BIN_EXE_thegn"));
    // Exit as soon as the first frame is flushed — the same hook `just bench`
    // uses to time launch→first-frame.
    cmd.env("THEGN_BENCH_FIRST_FRAME_EXIT", "1");
    cmd.env("TERM", "xterm-256color");
    // Isolate config/state/home. `XDG_*` is honoured on Windows too when set
    // explicitly (see `thegn_core::util::xdg_state_home`), but `home()` there
    // reads USERPROFILE rather than HOME, so both names have to move or the
    // launch writes into the developer's real profile.
    cmd.env("HOME", root.join("home"));
    cmd.env("USERPROFILE", root.join("home"));
    cmd.env("XDG_CONFIG_HOME", root.join("config"));
    cmd.env("XDG_STATE_HOME", root.join("state"));
    // A live daemon on the real socket would otherwise be reachable from here.
    cmd.env("XDG_RUNTIME_DIR", root.join("state"));

    let mut child = pair.slave.spawn_command(cmd).expect("spawn compositor");
    drop(pair.slave);

    let out = Arc::new(Mutex::new(Vec::<u8>::new()));
    let mut reader = pair.master.try_clone_reader().expect("clone reader");
    let mut writer = pair.master.take_writer().expect("take writer");
    {
        let out = out.clone();
        std::thread::spawn(move || {
            let mut chunk = [0u8; 8 * 1024];
            while let Ok(n) = reader.read(&mut chunk) {
                if n == 0 {
                    break;
                }
                if let Ok(mut b) = out.lock() {
                    b.extend_from_slice(&chunk[..n]);
                }
            }
        });
    }

    // Be a terminal, not just a pipe. ConPTY opens every session with a DSR
    // cursor query and stalls the child until something answers it, and the
    // compositor's own startup probe asks for one too; an unanswered query is
    // indistinguishable from a hang.
    let mut answered = false;
    let deadline = Instant::now() + LAUNCH_BUDGET;
    let exited_ok = loop {
        if !answered
            && let Ok(b) = out.lock()
            && b.windows(4).any(|w| w == b"\x1b[6n")
        {
            let _ = writer.write_all(b"\x1b[1;1R");
            let _ = writer.flush();
            answered = true;
        }
        match child.try_wait() {
            Ok(Some(status)) => break status.success(),
            Ok(None) if Instant::now() >= deadline => {
                let _ = child.kill();
                let _ = child.wait();
                break false;
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(20)),
            Err(_) => break false,
        }
    };
    // Let the reader catch the tail (a panic message is written on the way
    // out, and on Windows the exit can beat the last read).
    std::thread::sleep(Duration::from_millis(150));

    let text = out
        .lock()
        .map(|b| String::from_utf8_lossy(&b).into_owned())
        .unwrap_or_default();
    let _ = std::fs::remove_dir_all(&root);
    (exited_ok, text)
}

fn assert_clean_launch(case: &str, cols: u16, rows: u16) {
    let (exited_ok, output) = launch(case, cols, rows);
    let lower = output.to_ascii_lowercase();
    for marker in ["panicked at", "fatal runtime error"] {
        assert!(
            !lower.contains(marker),
            "compositor panicked at {cols}x{rows} ({case}); tail:\n{}",
            tail(&output)
        );
    }
    assert!(
        exited_ok,
        "compositor did not reach its first frame and exit cleanly at \
         {cols}x{rows} ({case}); tail:\n{}",
        tail(&output)
    );
    // Exiting 0 is not the same as drawing. A compositor that bailed before
    // composing anything would still satisfy the checks above while emitting
    // only the terminal handshake, so require actual glyphs on the screen —
    // without pinning any particular UI string, which would make this a
    // snapshot test by the back door.
    let legible = visible_text(&output)
        .chars()
        .filter(char::is_ascii_alphanumeric)
        .count();
    assert!(
        legible >= 40,
        "compositor drew no readable frame at {cols}x{rows} ({case}) — \
         {legible} visible characters; tail:\n{}",
        tail(&output)
    );
}

/// The last few KB, which is where a failure message lands.
fn tail(s: &str) -> String {
    let start = s.len().saturating_sub(4096);
    s[start..].to_string()
}

/// Drop ANSI escape sequences, leaving roughly what a viewer would read.
///
/// Deliberately crude: it only has to tell "a frame with words in it" from "a
/// terminal handshake and nothing else", which is the whole question here.
/// Multi-byte UTF-8 falls through as individual bytes, which is harmless —
/// callers count ASCII alphanumerics.
fn visible_text(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = String::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] != 0x1b {
            out.push(b[i] as char);
            i += 1;
            continue;
        }
        i += 1;
        match b.get(i) {
            // CSI: parameters, then a final byte in @..~
            Some(b'[') => {
                i += 1;
                while i < b.len() && !(0x40..=0x7e).contains(&b[i]) {
                    i += 1;
                }
                i += 1;
            }
            // OSC: runs to BEL or to the ST introducer
            Some(b']') => {
                i += 1;
                while i < b.len() && b[i] != 0x07 && b[i] != 0x1b {
                    i += 1;
                }
                i += if b.get(i) == Some(&0x1b) { 2 } else { 1 };
            }
            // Two-byte escape (or a stray ESC at the end).
            _ => i += 1,
        }
    }
    out
}

#[test]
fn compositor_reaches_first_frame_at_a_normal_geometry() {
    assert_clean_launch("normal", 100, 30);
}

/// A viewport small enough that the chrome cannot have everything it wants —
/// the geometry that historically panicked on a subtraction underflow.
#[test]
fn compositor_reaches_first_frame_in_a_cramped_viewport() {
    assert_clean_launch("short", 40, 8);
}
