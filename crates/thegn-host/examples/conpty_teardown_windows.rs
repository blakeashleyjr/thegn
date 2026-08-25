//! Why does closing a ConPTY pane not return, and does a compositor leak?
//!
//! Found while merging `origin/main` into the Windows branch:
//! `pane::tests::dropping_a_pane_reaps_a_sighup_ignoring_child` **passes** on
//! Windows — the child really is reaped, confirmed from the process tree — but
//! the test process never exits, and an orphaned test binary survived a killed
//! run and blocked the next build's linker.
//!
//! That went into KNOWN_ISSUES with an explicit gap: it had only been seen at
//! process *exit*, where one parked thread is enough to hang shutdown. A
//! compositor closes panes continuously — tab close, pool eviction — so the
//! question that actually matters is whether each close leaks or blocks. This
//! measures that.
//!
//! ## What thegn does today
//!
//! `pane_pty::spawn` gives the reader thread a **clone** of the master
//! (`try_clone_reader`) and the thread sits in `reader.read()` until EOF. That
//! module's own comment records that ConPTY withholds EOF: "the pseudoconsole
//! outlives the child — dropping the slave does not close it".
//!
//! `PtyHandle` then declares `master` before `writer`, and Rust drops fields in
//! declaration order — so a closing pane calls `ClosePseudoConsole` while both
//! the writer and a blocked cloned reader are still alive. Whether that is safe
//! is exactly what the orderings below probe.
//!
//! ## KNOWN LIMITATION — read before trusting the numbers
//!
//! This counts only **this process's** threads and handles. The pseudoconsole
//! host is a SEPARATE process (`OpenConsole.exe`) and is not counted at all, so
//! a clean 0/0 here does NOT establish that pane close leaks nothing. It
//! reported exactly that while a long session on this box accumulated **16
//! orphaned `OpenConsole.exe` processes**, every parent dead, each spinning
//! ~0.9 of a core — about 10 of 12 cores. The conclusion drawn from those zeros
//! ("teardown is clean") was wrong because it measured the wrong process.
//!
//! Extending this to count console hosts across an ordinary close is the open
//! work. Note the confound when doing so: every orphan seen so far followed a
//! **force-kill** of the client, and `TerminateProcess` bypasses
//! `ClosePseudoConsole`, so the arm that matters is a *normal* pane close.
//!
//! ## Reading the output
//!
//! Each trial runs on its own thread under a watchdog, so a teardown that never
//! returns is reported as `BLOCKED` instead of hanging the harness. `master
//! first` is the order thegn ships. If it blocks and another order does not,
//! that is a fix, not just a diagnosis.
//!
//! Run: `cargo run -p thegn-host --example conpty_teardown_windows [rounds]`

#![allow(clippy::disallowed_macros, clippy::disallowed_methods)]

fn main() {
    #[cfg(not(windows))]
    eprintln!("conpty_teardown_windows is Windows-only (ConPTY is a Win32 concept)");
    #[cfg(windows)]
    win::run();
}

#[cfg(windows)]
mod win {
    use std::io::Read;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

    use portable_pty::{CommandBuilder, PtySize};

    /// Live thread count for this process, via a Toolhelp snapshot.
    pub fn thread_count() -> u32 {
        use windows_sys::Win32::Foundation::{CloseHandle, INVALID_HANDLE_VALUE};
        use windows_sys::Win32::System::Diagnostics::ToolHelp::{
            CreateToolhelp32Snapshot, TH32CS_SNAPTHREAD, THREADENTRY32, Thread32First, Thread32Next,
        };
        let me = std::process::id();
        unsafe {
            let snap = CreateToolhelp32Snapshot(TH32CS_SNAPTHREAD, 0);
            if snap == INVALID_HANDLE_VALUE {
                return 0;
            }
            let mut e: THREADENTRY32 = std::mem::zeroed();
            e.dwSize = std::mem::size_of::<THREADENTRY32>() as u32;
            let mut n = 0u32;
            if Thread32First(snap, &mut e) != 0 {
                loop {
                    if e.th32OwnerProcessID == me {
                        n += 1;
                    }
                    if Thread32Next(snap, &mut e) == 0 {
                        break;
                    }
                }
            }
            CloseHandle(snap);
            n
        }
    }

    /// Open kernel handles held by this process.
    pub fn handle_count() -> u32 {
        use windows_sys::Win32::System::Threading::{GetCurrentProcess, GetProcessHandleCount};
        let mut n = 0u32;
        unsafe {
            if GetProcessHandleCount(GetCurrentProcess(), &mut n) == 0 {
                return 0;
            }
        }
        n
    }

    /// Teardown order under test.
    #[derive(Clone, Copy, PartialEq)]
    pub enum Order {
        /// What `PtyHandle`'s field order produces today.
        MasterThenWriter,
        /// Drop the writer, then the master.
        WriterThenMaster,
        /// Drop the writer AND the reader clone, then the master.
        ReaderWriterThenMaster,
    }

    impl Order {
        fn label(self) -> &'static str {
            match self {
                Order::MasterThenWriter => "master first (SHIPPED)",
                Order::WriterThenMaster => "writer, then master",
                Order::ReaderWriterThenMaster => "reader+writer, then master",
            }
        }
    }

    /// Build a pane the way `pane_pty::spawn` does, then close it in `order`.
    ///
    /// `kill` selects the arm: `true` terminates a live child (the case under
    /// suspicion), `false` lets it exit on its own first.
    fn one_pane(
        kill: bool,
        order: Order,
        reader_ended: &Arc<AtomicBool>,
        stage: &Arc<std::sync::Mutex<String>>,
    ) {
        let mark = |s: &str| {
            *stage.lock().unwrap_or_else(|e| e.into_inner()) = s.to_string();
        };
        mark("openpty");
        let pty = portable_pty::native_pty_system();
        let pair = pty
            .openpty(PtySize {
                rows: 24,
                cols: 80,
                pixel_width: 0,
                pixel_height: 0,
            })
            .expect("openpty");

        // `cmd /c` rather than a POSIX shell, so the measurement does not also
        // depend on Git for Windows being installed.
        let mut cmd = CommandBuilder::new("cmd.exe");
        if kill {
            cmd.args(["/c", "ping -n 60 127.0.0.1"]);
        } else {
            cmd.args(["/c", "echo hi"]);
        }

        mark("spawn_command");
        let mut child = pair.slave.spawn_command(cmd).expect("spawn child");
        // Same as pane_pty: drop the slave so a unix master would see EOF.
        drop(pair.slave);

        let counter = Arc::new(AtomicU64::new(0));
        let c = Arc::clone(&counter);

        let master = pair.master;
        mark("take_writer");
        // Shared with the reader thread, because the reader has to be able to
        // ANSWER the child — see the DSR reply below.
        let writer = Arc::new(std::sync::Mutex::new(Some(
            master.take_writer().expect("take_writer"),
        )));
        mark("try_clone_reader");
        let reader = master.try_clone_reader().expect("clone_reader");

        // The reader thread, as in pane_pty: blocks until EOF.
        //
        // It must also reply to the cursor-position report. ConPTY emits
        // `ESC[6n` at startup and **stalls the child until it is answered** —
        // thegn's vt100 emulator answers it as a matter of course, so a harness
        // that merely counts bytes is not modelling thegn, it is modelling a
        // terminal that never responds. Without this the child never runs to
        // completion, `child.wait()` blocks forever, and the harness reports a
        // platform failure that is really its own omission. (That misreading
        // has already cost this branch two wrong verdicts; the reply is cheap
        // insurance against a third.)
        let ended = Arc::clone(reader_ended);
        let reader_cell = Arc::new(std::sync::Mutex::new(Some(reader)));
        let rc = Arc::clone(&reader_cell);
        let w_reply = Arc::clone(&writer);
        std::thread::spawn(move || {
            use std::io::Write;
            let Some(mut r) = rc.lock().unwrap_or_else(|e| e.into_inner()).take() else {
                return;
            };
            let mut buf = vec![0u8; 64 * 1024];
            loop {
                match r.read(&mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        c.fetch_add(n as u64, Ordering::Relaxed);
                        if buf[..n].windows(4).any(|w| w == b"\x1b[6n")
                            && let Ok(mut g) = w_reply.lock()
                            && let Some(w) = g.as_mut()
                        {
                            let _ = w.write_all(b"\x1b[1;1R");
                            let _ = w.flush();
                        }
                    }
                }
            }
            ended.store(true, Ordering::Relaxed);
        });

        if kill {
            std::thread::sleep(std::time::Duration::from_millis(250));
            mark("child.kill");
            let _ = child.kill();
        } else {
            mark("child.wait");
            let _ = child.wait();
            std::thread::sleep(std::time::Duration::from_millis(150));
        }

        // The writer is shared with the reader thread now, so dropping our Arc
        // clone would close nothing — take the boxed writer out of the cell.
        let close_writer = || {
            let _ = writer.lock().unwrap_or_else(|e| e.into_inner()).take();
        };

        match order {
            Order::MasterThenWriter => {
                mark("drop(master)");
                drop(master);
                mark("drop(writer)");
                close_writer();
            }
            Order::WriterThenMaster => {
                mark("drop(writer)");
                close_writer();
                mark("drop(master)");
                drop(master);
            }
            Order::ReaderWriterThenMaster => {
                mark("drop(writer)");
                close_writer();
                // Drop the reader clone the thread never got, if it is still
                // here; the thread's own copy is what actually matters, so this
                // arm mainly shows whether an extra live clone is the blocker.
                mark("drop(reader clone)");
                let _ = reader_cell.lock().unwrap_or_else(|e| e.into_inner()).take();
                mark("drop(master)");
                drop(master);
            }
        }
        mark("closed");
    }

    /// Run one (arm, order) trial under a watchdog. Returns None if it blocked.
    fn trial(kill: bool, order: Order, rounds: usize) -> Option<(i64, i64, bool)> {
        let ended = Arc::new(AtomicBool::new(false));
        let e2 = Arc::clone(&ended);
        let stage = Arc::new(std::sync::Mutex::new(String::from("(not started)")));
        let st = Arc::clone(&stage);
        let round = Arc::new(std::sync::atomic::AtomicU64::new(0));
        let rn = Arc::clone(&round);
        let (tx, rx) = std::sync::mpsc::channel::<(i64, i64)>();

        std::thread::spawn(move || {
            // Warm up: the first pane in a process allocates state that is not
            // per-pane, and counting it would overstate any growth.
            one_pane(kill, order, &e2, &st);
            std::thread::sleep(std::time::Duration::from_millis(300));
            let t0 = thread_count() as i64;
            let h0 = handle_count() as i64;
            for i in 0..rounds {
                rn.store(i as u64 + 1, Ordering::Relaxed);
                one_pane(kill, order, &e2, &st);
            }
            std::thread::sleep(std::time::Duration::from_millis(500));
            let _ = tx.send((thread_count() as i64 - t0, handle_count() as i64 - h0));
        });

        // Generous: a per-pane close that needs seconds is itself the finding.
        let budget = std::time::Duration::from_secs(20 + 3 * rounds as u64);
        match rx.recv_timeout(budget) {
            Ok((dt, dh)) => Some((dt, dh, ended.load(Ordering::Relaxed))),
            Err(_) => {
                let s = stage.lock().unwrap_or_else(|e| e.into_inner()).clone();
                println!(
                    "BLOCKED at `{}` on round {} of {rounds}",
                    s,
                    round.load(Ordering::Relaxed)
                );
                None
            }
        }
    }

    pub fn run() {
        let rounds: usize = std::env::args()
            .nth(1)
            .and_then(|s| s.parse().ok())
            .unwrap_or(10);

        println!("ConPTY pane teardown — blocking and leak behaviour\n");
        println!(
            "baseline threads={} handles={}\n",
            thread_count(),
            handle_count()
        );

        for (arm, kill) in [("exit", false), ("terminate", true)] {
            println!("=== child {arm}s ===");
            for order in [
                Order::MasterThenWriter,
                Order::WriterThenMaster,
                Order::ReaderWriterThenMaster,
            ] {
                print!("  {:<28} ", order.label());
                use std::io::Write;
                let _ = std::io::stdout().flush();
                match trial(kill, order, rounds) {
                    Some((dt, dh, reader_ended)) => println!(
                        "ok   threads {dt:+} ({:.1}/pane)  handles {dh:+} ({:.1}/pane)  reader-ended={reader_ended}",
                        dt as f64 / rounds as f64,
                        dh as f64 / rounds as f64
                    ),
                    None => {}
                }
            }
            println!();
        }

        println!("`master first` is the order thegn ships (PtyHandle field order).");
        println!("BLOCKED there means closing a pane does not return; growth means");
        println!("each close leaks. An order that is ok while the shipped one is not");
        println!("is the fix.");

        use std::io::Write;
        let _ = std::io::stdout().flush();
        // Never wait on parked readers — a hang here would be the exit-path
        // symptom, which is a separate datum and must not eat the numbers.
        std::process::exit(0);
    }
}
