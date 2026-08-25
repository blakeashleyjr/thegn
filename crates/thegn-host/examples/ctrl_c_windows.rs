//! Does Ctrl-C reach a ConPTY child, and in what encoding?
//!
//! `pane::tests::ctrl_c_interrupts_the_pane_child` says the shipped path does
//! not work on Windows: thegn writes `0x03` (what the key encoder produces) and
//! the child runs on regardless. Unix needs nothing more — the tty line
//! discipline turns that byte into SIGINT — but Windows has no SIGINT, so the
//! byte has to be recognised by ConPTY and turned into a `CTRL_C_EVENT` against
//! the attached console group. This asks which encodings actually achieve that.
//!
//! Three candidates, because the handshake hints at the answer. ConPTY opens
//! every session with `ESC[?9001h`, which asks the terminal to send **win32
//! input mode** records — `ESC[<vk>;<sc>;<uc>;<kd>;<cs>;<rc>_` — rather than
//! plain VT bytes. If a bare `0x03` is not recognised while that mode is on,
//! the fix is to speak the mode ConPTY asked for.
//!
//!   1. raw `0x03`                    — what thegn sends today
//!   2. win32-input-mode key record   — what `ESC[?9001h` requests
//!   3. both                          — belt and braces
//!
//! Run: `cargo run -p thegn-host --example ctrl_c_windows`

#![allow(clippy::disallowed_macros, clippy::disallowed_methods)]

fn main() {
    #[cfg(not(windows))]
    eprintln!("ctrl_c_windows is Windows-only (unix needs no such translation)");
    #[cfg(windows)]
    win::run();
}

#[cfg(windows)]
mod win {
    use std::io::{Read, Write};
    use std::sync::{Arc, Mutex};

    use portable_pty::{CommandBuilder, PtySize};

    /// A win32-input-mode key record for Ctrl-C.
    ///
    /// `ESC [ Vk ; Sc ; Uc ; Kd ; Cs ; Rc _`, where Vk=0x43 ('C'), Sc=46 is the
    /// scancode, Uc=3 is the resulting unicode char (ETX), Kd=1 is key-down and
    /// Cs=0x0008 is LEFT_CTRL_PRESSED. The key-up record follows, because a
    /// console app that watches for the transition wants both halves.
    fn win32_ctrl_c() -> Vec<u8> {
        let mut v = Vec::new();
        v.extend_from_slice(b"\x1b[67;46;3;1;8;1_"); // down
        v.extend_from_slice(b"\x1b[67;46;3;0;8;1_"); // up
        v
    }

    fn alive(pid: u32) -> bool {
        use windows_sys::Win32::Foundation::{CloseHandle, STILL_ACTIVE};
        use windows_sys::Win32::System::Threading::{
            GetExitCodeProcess, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
        };
        unsafe {
            let h = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
            if h.is_null() {
                return false;
            }
            let mut code: u32 = 0;
            let ok = GetExitCodeProcess(h, &mut code) != 0;
            CloseHandle(h);
            ok && code == STILL_ACTIVE as u32
        }
    }

    /// Spawn `argv` in a ConPTY, answer the DSR, send `input`, and report
    /// whether the child died on its own within `wait`.
    fn trial(label: &str, argv: &[&str], input: &[u8]) {
        let pty = portable_pty::native_pty_system();
        let pair = pty
            .openpty(PtySize {
                rows: 24,
                cols: 80,
                pixel_width: 0,
                pixel_height: 0,
            })
            .expect("openpty");
        let mut cmd = CommandBuilder::new(argv[0]);
        cmd.args(&argv[1..]);
        let mut child = pair.slave.spawn_command(cmd).expect("spawn");
        let pid = child.process_id().expect("pid");
        drop(pair.slave);

        let writer = Arc::new(Mutex::new(pair.master.take_writer().expect("writer")));
        let mut reader = pair.master.try_clone_reader().expect("reader");

        // Answer the opening cursor query, or the child never starts.
        let w = Arc::clone(&writer);
        std::thread::spawn(move || {
            let mut buf = vec![0u8; 8192];
            let mut answered = false;
            while let Ok(n) = reader.read(&mut buf) {
                if n == 0 {
                    break;
                }
                if !answered && buf[..n].windows(4).any(|x| x == b"\x1b[6n") {
                    if let Ok(mut g) = w.lock() {
                        let _ = g.write_all(b"\x1b[1;1R");
                        let _ = g.flush();
                    }
                    answered = true;
                }
            }
        });

        std::thread::sleep(std::time::Duration::from_millis(800));
        if !alive(pid) {
            println!("  {label:<34} SKIPPED — child exited before the interrupt");
            return;
        }

        if let Ok(mut g) = writer.lock() {
            let _ = g.write_all(input);
            let _ = g.flush();
        }

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(6);
        while alive(pid) && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(25));
        }
        let survived = alive(pid);
        println!(
            "  {label:<34} {}",
            if survived {
                "child SURVIVED — not interrupted"
            } else {
                "child was INTERRUPTED"
            }
        );
        let _ = child.kill();
        drop(pair.master);
    }

    /// Control: does ANY input reach the child? If plain typing does not
    /// arrive, nothing can be concluded about Ctrl-C from this harness.
    fn input_control() {
        let pty = portable_pty::native_pty_system();
        let pair = pty
            .openpty(PtySize {
                rows: 24,
                cols: 80,
                pixel_width: 0,
                pixel_height: 0,
            })
            .expect("openpty");
        let mut cmd = CommandBuilder::new("powershell.exe");
        cmd.args([
            "-NoProfile",
            "-Command",
            "$x = Read-Host; Write-Output ('GOT:' + $x)",
        ]);
        let mut child = pair.slave.spawn_command(cmd).expect("spawn");
        drop(pair.slave);
        let writer = Arc::new(Mutex::new(pair.master.take_writer().expect("writer")));
        let mut reader = pair.master.try_clone_reader().expect("reader");
        let seen = Arc::new(Mutex::new(String::new()));
        let s2 = Arc::clone(&seen);
        let w = Arc::clone(&writer);
        std::thread::spawn(move || {
            let mut buf = vec![0u8; 8192];
            let mut answered = false;
            while let Ok(n) = reader.read(&mut buf) {
                if n == 0 {
                    break;
                }
                if !answered && buf[..n].windows(4).any(|x| x == b"[6n") {
                    if let Ok(mut g) = w.lock() {
                        let _ = g.write_all(b"[1;1R");
                        let _ = g.flush();
                    }
                    answered = true;
                }
                if let Ok(mut g) = s2.lock() {
                    g.push_str(&String::from_utf8_lossy(&buf[..n]));
                }
            }
        });
        std::thread::sleep(std::time::Duration::from_millis(1200));
        if let Ok(mut g) = writer.lock() {
            let _ = g.write_all(b"hi\r");
            let _ = g.flush();
        }
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(6);
        let mut got = false;
        while std::time::Instant::now() < deadline && !got {
            std::thread::sleep(std::time::Duration::from_millis(50));
            got = seen.lock().map(|g| g.contains("GOT:hi")).unwrap_or(false);
        }
        println!(
            "  {:<34} {}",
            "CONTROL: plain typing",
            if got {
                "input ARRIVED (echoed back)"
            } else {
                "input did NOT arrive"
            }
        );
        let _ = child.kill();
        drop(pair.master);
    }

    pub fn run() {
        let ps = [
            "powershell.exe",
            "-NoProfile",
            "-Command",
            "Start-Sleep -Seconds 30",
        ];
        let ping = ["cmd.exe", "/c", "ping -n 40 127.0.0.1 >NUL"];

        println!("Ctrl-C delivery into a ConPTY child\n");
        println!("== control ==");
        input_control();
        println!("\n== powershell Start-Sleep ==");
        trial("raw 0x03 (what thegn sends)", &ps, b"\x03");
        trial("win32-input-mode record", &ps, &win32_ctrl_c());
        let mut both = win32_ctrl_c();
        both.push(0x03);
        trial("both", &ps, &both);

        println!("\n== cmd ping ==");
        trial("raw 0x03 (what thegn sends)", &ping, b"\x03");
        trial("win32-input-mode record", &ping, &win32_ctrl_c());
        trial("both", &ping, &both);

        println!("\nAn encoding that interrupts where raw 0x03 does not is the fix");
        println!("for the shipped path; if none interrupt, the byte is not the problem.");
        let _ = std::io::stdout().flush();
        std::process::exit(0);
    }
}
