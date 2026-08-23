//! ConPTY spike: does a child spawned through `portable-pty` actually run and
//! exit on Windows, and does the master read see EOF?
//!
//! Diagnostic for the Windows pane path — `pane_pty` relies on "drop the slave
//! and the master sees EOF", which is a unix guarantee ConPTY does not make.
//!
//! ```sh
//! cargo run -p thegn-host --example conpty_spike
//! ```
#![allow(clippy::disallowed_macros)]

use portable_pty::{CommandBuilder, PtySize};
use std::io::Read;
use std::time::{Duration, Instant};

fn try_one(label: &str, argv: &[&str]) {
    println!("\n=== {label} ===");
    println!("argv: {argv:?}");
    let sys = portable_pty::native_pty_system();
    let pair = match sys.openpty(PtySize {
        rows: 24,
        cols: 80,
        pixel_width: 0,
        pixel_height: 0,
    }) {
        Ok(p) => p,
        Err(e) => {
            println!("openpty FAILED: {e}");
            return;
        }
    };
    let mut cmd = CommandBuilder::new(argv[0]);
    for a in &argv[1..] {
        cmd.arg(a);
    }
    let mut child = match pair.slave.spawn_command(cmd) {
        Ok(c) => c,
        Err(e) => {
            println!("spawn FAILED: {e}");
            return;
        }
    };
    drop(pair.slave);
    let mut reader = pair.master.try_clone_reader().expect("clone reader");

    // Reader on its own thread so a blocking read cannot wedge the spike.
    let t0 = Instant::now();
    let reader_thread = std::thread::spawn(move || {
        let mut buf = vec![0u8; 8192];
        let mut out = Vec::new();
        loop {
            match reader.read(&mut buf) {
                Ok(0) => return (out, "EOF".to_string()),
                Ok(n) => out.extend_from_slice(&buf[..n]),
                Err(e) => return (out, format!("read error: {e}")),
            }
        }
    });

    // Independently wait for the child, bounded.
    let mut waited = None;
    while t0.elapsed() < Duration::from_secs(10) {
        match child.try_wait() {
            Ok(Some(status)) => {
                waited = Some(status.exit_code());
                break;
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(50)),
            Err(e) => {
                println!("try_wait error: {e}");
                break;
            }
        }
    }
    match waited {
        Some(code) => println!("child EXITED code={code} after {:?}", t0.elapsed()),
        None => {
            println!("child STILL RUNNING after {:?} -- killing", t0.elapsed());
            let _ = child.kill();
        }
    }

    // Did the reader ever see EOF once the child was gone?
    let deadline = Instant::now() + Duration::from_secs(3);
    while !reader_thread.is_finished() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(50));
    }
    if reader_thread.is_finished() {
        let (out, why) = reader_thread.join().expect("reader join");
        println!("reader ended: {why}");
        println!("output: {:?}", String::from_utf8_lossy(&out));
    } else {
        // Dropping the master is what finally unblocks it.
        println!("reader STILL BLOCKED 3s after child exit (no EOF from ConPTY)");
        drop(pair.master);
        match reader_thread.join() {
            Ok((out, why)) => {
                println!("after dropping master, reader ended: {why}");
                println!("output: {:?}", String::from_utf8_lossy(&out));
            }
            Err(_) => println!("reader thread panicked"),
        }
    }
}

fn main() {
    #[cfg(windows)]
    {
        try_one(
            "powershell -Command, bracket expression",
            &[
                "powershell.exe",
                "-NoProfile",
                "-NonInteractive",
                "-Command",
                "[Console]::Out.Write('hello-pty')",
            ],
        );
        try_one(
            "powershell -Command, simple Write-Host",
            &[
                "powershell.exe",
                "-NoProfile",
                "-NonInteractive",
                "-Command",
                "Write-Host hello",
            ],
        );
        try_one("cmd /c echo", &["cmd.exe", "/c", "echo hello"]);
    }
    #[cfg(not(windows))]
    {
        try_one("sh -c printf", &["/bin/sh", "-c", "printf 'hello-pty'"]);
    }
}
