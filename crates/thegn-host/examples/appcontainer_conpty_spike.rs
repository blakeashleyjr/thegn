//! Spike: does an AppContainer process work inside a **ConPTY**?
//!
//! `appcontainer_spike` proved thegn can spawn into an AppContainer at all, and
//! that the filesystem boundary is real. It did so from an ordinary console
//! process, which leaves the assumption the whole `Backend::WinAppContainer`
//! design rests on untested:
//!
//! > thegn spawns a **trampoline** into the ConPTY the normal way (portable-pty
//! > owns `PROC_THREAD_ATTRIBUTE_PSEUDOCONSOLE` and will not share its attribute
//! > list), and the trampoline re-launches the real shell with
//! > `PROC_THREAD_ATTRIBUTE_SECURITY_CAPABILITIES`, inheriting the console it
//! > was given.
//!
//! That makes the pane's shell a **grandchild** of thegn, and the console it
//! must use is a pseudoconsole owned by a process outside the container. The
//! specific way this fails, if it fails: a console is reached through
//! `\Device\ConDrv`, and an AppContainer token is denied most of the object
//! namespace by default. If ConDrv is one of those, the grandchild starts and
//! then cannot read or write its terminal — which no amount of ACL granting on
//! the worktree would fix, and which would sink the trampoline design.
//!
//! # What this measures
//!
//! Three spawns down an identical ConPTY, so the results are comparable:
//!
//! 1. **direct** — the command straight into the ConPTY (portable-pty's normal
//!    path). The control: proves the harness itself works.
//! 2. **trampoline, no container** — the same command re-launched by a
//!    trampoline via `CreateProcessW`, inheriting the console. Isolates
//!    "grandchild through a trampoline" from "AppContainer".
//! 3. **trampoline, IN an AppContainer** — the real thing.
//!
//! If 1 and 2 pass and 3 does not, the trampoline is fine and the AppContainer
//! token is the problem. If 2 fails too, console inheritance through
//! `CreateProcessW` is the problem and the container is a red herring.
//!
//! # Run it
//!
//! ```text
//! cargo run -p thegn-host --example appcontainer_conpty_spike
//! ```
//!
//! It reuses the `thegn-spike` AppContainer profile if `appcontainer_spike`
//! already created one, and creates it otherwise. Nothing else is modified.

// A spike is a standalone report: its stdout IS the deliverable.
#![allow(clippy::disallowed_macros, clippy::disallowed_methods)]

fn main() {
    #[cfg(not(windows))]
    {
        eprintln!("appcontainer_conpty_spike: Windows-only (ConPTY + AppContainer are Win32)");
    }
    #[cfg(windows)]
    win::run();
}

#[cfg(windows)]
mod win {
    use std::ffi::OsStr;
    use std::io::{Read, Write};
    use std::os::windows::ffi::OsStrExt;
    use std::sync::{Arc, Mutex};
    use std::time::{Duration, Instant};

    use windows_sys::Win32::Foundation::{CloseHandle, HANDLE, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::Security::Isolation::{
        CreateAppContainerProfile, DeriveAppContainerSidFromAppContainerName,
    };
    use windows_sys::Win32::Security::{PSID, SECURITY_CAPABILITIES};
    use windows_sys::Win32::System::Threading::{
        CreateProcessW, DeleteProcThreadAttributeList, EXTENDED_STARTUPINFO_PRESENT,
        GetExitCodeProcess, INFINITE, InitializeProcThreadAttributeList, PROCESS_INFORMATION,
        STARTUPINFOEXW, UpdateProcThreadAttribute, WaitForSingleObject,
    };

    /// `PROC_THREAD_ATTRIBUTE_SECURITY_CAPABILITIES` — not exported by
    /// windows-sys, so spelled out (same value the sibling spike uses).
    const PROC_THREAD_ATTRIBUTE_SECURITY_CAPABILITIES: u32 = 131081;

    const PROFILE: &str = "thegn-spike";
    /// Printed by the innermost command. Seeing this on the PTY master is the
    /// entire pass condition: it means the grandchild could WRITE the console.
    const MARKER: &str = "CONPTY-OK-7f3a";
    const BUDGET: Duration = Duration::from_secs(30);

    pub fn run() {
        let args: Vec<String> = std::env::args().collect();
        if let Some(pos) = args.iter().position(|a| a == "--trampoline") {
            trampoline(&args[pos + 1..]);
            return;
        }

        println!("== AppContainer x ConPTY spike ==\n");
        let exe = std::env::current_exe().expect("current_exe");
        let exe = exe.to_string_lossy().into_owned();

        // The two signals are measured by SEPARATE runs, deliberately.
        //
        // An earlier version ran one script that both echoed the marker and read
        // a guard file. In the contained case that script lived on a path the
        // container could not traverse, so `cmd` died with "Access is denied"
        // before echoing anything — and the spike read that as "the container
        // cannot use the console". It is a filesystem denial wearing a console
        // denial's clothes, and it inverts the conclusion.
        //
        // So: the marker run touches NO file (`cmd.exe` lives in System32, which
        // does carry an ALL APPLICATION PACKAGES ACE), and the guard run is the
        // only one allowed to fail on access.
        let guard = repo_guard();
        let marker = |pre: &[&str]| {
            let mut argv: Vec<&str> = pre.to_vec();
            argv.extend(["cmd.exe", "/c", "echo", MARKER]);
            through_conpty(&argv)
        };
        let reads = |pre: &[&str]| {
            let mut argv: Vec<&str> = pre.to_vec();
            argv.extend(["cmd.exe", "/c", "type", &guard]);
            through_conpty(&argv).output.contains("[workspace]")
        };

        let mut direct = marker(&[]);
        direct.read_ok = reads(&[]);
        report("1. direct into the ConPTY", &direct);

        let mut tramp = marker(&[&exe, "--trampoline"]);
        tramp.read_ok = reads(&[&exe, "--trampoline"]);
        report("2. trampoline, no container", &tramp);

        let mut contained = marker(&[&exe, "--trampoline", "--contained"]);
        contained.read_ok = reads(&[&exe, "--trampoline", "--contained"]);
        report("3. trampoline, IN an AppContainer", &contained);

        println!("== containment check ==");
        println!(
            "  case 1 read the guard file: {}\n  case 2 read the guard file: {}\n  \
             case 3 read the guard file: {}",
            yes_no(direct.read_ok),
            yes_no(tramp.read_ok),
            yes_no(contained.read_ok)
        );
        // The other half of "interactive": can the contained grandchild READ the
        // console? `set /p` consumes one line of stdin; `/v:on` gives delayed
        // expansion so the value is visible in the same command line.
        let typed = through_conpty_stdin(
            &[
                &exe,
                "--trampoline",
                "--contained",
                "cmd.exe",
                "/v:on",
                "/c",
                "set /p L= && echo GOT-!L!",
            ],
            Some("PING\r\n"),
        );
        let input_ok = strip_ansi(&typed.output)
            .lines()
            .any(|l| l.trim() == "GOT-PING");
        println!(
            "  case 3 read a keystroke from the ConPTY: {}",
            if input_ok { "GOT-PING" } else { "no echo back" }
        );

        let contained_for_real = direct.read_ok && !contained.read_ok;
        if contained_for_real {
            println!("  => the token boundary IS being applied in case 3.\n");
        } else {
            println!(
                "  => INCONCLUSIVE: case 3 is not observably contained, so a passing\n     \
                 marker below proves only that the trampoline works.\n"
            );
        }

        println!("== verdict ==");
        if direct.ok && tramp.ok && contained.ok && !contained_for_real {
            println!(
                "  TRAMPOLINE WORKS, CONTAINMENT UNPROVEN — console I/O survives the\n  \
                 grandchild hop, but this run could not show the AppContainer token taking\n  \
                 effect. Resolve that before trusting the result."
            );
            return;
        }
        match (direct.ok, tramp.ok, contained.ok) {
            (false, _, _) => println!(
                "  INCONCLUSIVE — the control failed, so the harness is wrong, not Windows."
            ),
            (true, false, _) => println!(
                "  TRAMPOLINE IS THE PROBLEM — console inheritance through CreateProcessW\n  \
                 does not carry the ConPTY. AppContainer is not implicated; a pane would\n  \
                 need a different mechanism (thegn owning the ConPTY creation itself)."
            ),
            (true, true, false) => println!(
                "  APPCONTAINER IS THE PROBLEM — the trampoline works, but the contained\n  \
                 grandchild cannot use the pseudoconsole. Most likely \\Device\\ConDrv is\n  \
                 outside the container token's reach. Backend::WinAppContainer cannot host\n  \
                 an interactive pane on this design; re-plan before writing it."
            ),
            (true, true, true) if !input_ok => println!(
                "  OUTPUT ONLY — the contained grandchild can write the ConPTY but did not
  \n                 read a keystroke back. An interactive pane needs both; investigate before
  \n                 building on this."
            ),
            (true, true, true) => println!(
                "  TRAMPOLINE DESIGN HOLDS — an AppContainer grandchild reads and writes a\n  \
                 ConPTY owned by thegn. The rest of Phase 3 (profile lifecycle, ACL grants,\n  \
                 capability SIDs, isolation class) is ordinary implementation."
            ),
        }
    }

    /// Trampoline mode: already inside the ConPTY, re-launch the rest of argv.
    fn trampoline(rest: &[String]) {
        let contained = rest.iter().any(|a| a == "--contained");
        let cmd = rest
            .iter()
            .filter(|a| *a != "--contained")
            .cloned()
            .collect::<Vec<_>>()
            .join(" ");
        let sid = if contained {
            match container_sid() {
                Ok(s) => Some(s),
                Err(e) => {
                    println!("TRAMPOLINE-SID-FAIL {e}");
                    std::process::exit(2);
                }
            }
        } else {
            None
        };
        match spawn_inheriting_console(sid, &cmd) {
            Ok(code) => std::process::exit(code as i32),
            Err(e) => {
                // Printed on the trampoline's own (console) stdout so it reaches
                // the master: that is how "the grandchild never started" is told
                // apart from "it started and was mute".
                println!("TRAMPOLINE-SPAWN-FAIL {e}");
                std::process::exit(3);
            }
        }
    }

    struct Run {
        ok: bool,
        read_ok: bool,
        exit: Option<u32>,
        output: String,
    }

    fn yes_no(b: bool) -> &'static str {
        if b { "READ-OK" } else { "denied/absent" }
    }

    /// A text file an ordinary process can read and a contained one cannot:
    /// the repo manifest, under `Documents`, which carries no ALL APPLICATION
    /// PACKAGES ACE. Nothing is granted anywhere: this spike modifies no ACLs.
    fn repo_guard() -> String {
        std::env::current_dir()
            .map(|d| d.join("Cargo.toml").to_string_lossy().into_owned())
            .unwrap_or_default()
    }

    fn report(label: &str, r: &Run) {
        println!(
            "{label}: {} (exit {:?})",
            if r.ok { "MARKER SEEN" } else { "NO MARKER" },
            r.exit
        );
        for line in r
            .output
            .lines()
            .map(str::trim)
            .filter(|l| !l.is_empty())
            .take(6)
        {
            println!("      | {line}");
        }
        println!();
    }

    /// Spawn `argv` into a real ConPTY and collect what comes back.
    ///
    /// Mirrors `tests/pty_launch.rs`, which is the shape that actually works on
    /// Windows: poll `try_wait` rather than blocking (ConPTY signals no EOF on
    /// master close, so a blocking read never returns and a blocking wait can
    /// deadlock against it), and **answer the startup DSR query** — ConPTY opens
    /// every session with `ESC[6n` and stalls the child until something replies.
    /// An unanswered query is indistinguishable from the failure being measured.
    fn through_conpty(argv: &[&str]) -> Run {
        through_conpty_stdin(argv, None)
    }

    /// As [`through_conpty`], but type `stdin` at the child once the ConPTY has
    /// answered its startup query. This proves the INPUT half of an interactive
    /// pane: output alone leaves read access to `\Device\ConDrv` untested, and
    /// a pane that can print but not receive keystrokes is not a pane.
    fn through_conpty_stdin(argv: &[&str], stdin: Option<&str>) -> Run {
        let pair = match portable_pty::native_pty_system().openpty(portable_pty::PtySize {
            rows: 24,
            cols: 120,
            pixel_width: 0,
            pixel_height: 0,
        }) {
            Ok(p) => p,
            Err(e) => return fail(format!("openpty failed: {e}")),
        };

        let mut cb = portable_pty::CommandBuilder::new(argv[0]);
        for a in &argv[1..] {
            cb.arg(a);
        }

        let mut child = match pair.slave.spawn_command(cb) {
            Ok(c) => c,
            Err(e) => return fail(format!("spawn failed: {e}")),
        };
        drop(pair.slave);

        let out = Arc::new(Mutex::new(Vec::<u8>::new()));
        let mut reader = match pair.master.try_clone_reader() {
            Ok(r) => r,
            Err(e) => return fail(format!("try_clone_reader failed: {e}")),
        };
        let mut writer = match pair.master.take_writer() {
            Ok(w) => w,
            Err(e) => return fail(format!("take_writer failed: {e}")),
        };
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

        let mut answered = false;
        let deadline = Instant::now() + BUDGET;
        let exit = loop {
            if !answered
                && let Ok(b) = out.lock()
                && b.windows(4).any(|w| w == b"\x1b[6n")
            {
                let _ = writer.write_all(b"\x1b[1;1R");
                if let Some(text) = stdin {
                    let _ = writer.write_all(text.as_bytes());
                }
                let _ = writer.flush();
                answered = true;
            }
            match child.try_wait() {
                Ok(Some(status)) => break Some(status.exit_code()),
                Ok(None) if Instant::now() >= deadline => {
                    let _ = child.kill();
                    let _ = child.wait();
                    break None;
                }
                Ok(None) => std::thread::sleep(Duration::from_millis(20)),
                Err(_) => break None,
            }
        };
        // Let the reader catch the tail: on Windows the exit can beat the last
        // read. The master is deliberately NOT dropped first — closing the
        // pseudoconsole while the reader is blocked in `read` is the hang.
        std::thread::sleep(Duration::from_millis(250));

        let output = out
            .lock()
            .map(|b| String::from_utf8_lossy(&b).into_owned())
            .unwrap_or_default();
        // ConPTY interleaves escape sequences with the text on the same line
        // (`ESC[?9001h ESC[?1004h ESC[6n ESC[m CONPTY-OK-…`), so a raw
        // line-equality test finds nothing even when the marker is right there.
        // Strip the escapes first, then require a line that is JUST the marker —
        // still strict enough that a marker echoed inside a command line would
        // not count.
        let plain = strip_ansi(&output);
        let ok = plain.lines().any(|l| l.trim() == MARKER);
        let read_ok = plain.lines().any(|l| l.trim() == "READ-OK");
        Run {
            ok,
            read_ok,
            exit,
            output,
        }
    }

    /// Remove CSI (`ESC[…final`) and OSC (`ESC]…BEL`/`ESC\`) sequences.
    fn strip_ansi(s: &str) -> String {
        let b: Vec<char> = s.chars().collect();
        let mut out = String::with_capacity(s.len());
        let mut i = 0;
        while i < b.len() {
            if b[i] != '\u{1b}' {
                out.push(b[i]);
                i += 1;
                continue;
            }
            i += 1;
            match b.get(i) {
                Some('[') => {
                    i += 1;
                    while i < b.len() && !matches!(b[i], '@'..='~') {
                        i += 1;
                    }
                    i += 1; // the final byte
                }
                Some(']') => {
                    i += 1;
                    while i < b.len() && b[i] != '\u{7}' {
                        // ST (`ESC \`) also terminates an OSC.
                        if b[i] == '\u{1b}' && b.get(i + 1) == Some(&'\\') {
                            i += 1;
                            break;
                        }
                        i += 1;
                    }
                    i += 1;
                }
                // A lone ESC or a two-byte sequence: drop the next char.
                _ => i += 1,
            }
        }
        out
    }

    fn fail(msg: String) -> Run {
        Run {
            ok: false,
            read_ok: false,
            exit: None,
            output: msg,
        }
    }

    fn wide(s: &str) -> Vec<u16> {
        OsStr::new(s).encode_wide().chain(Some(0)).collect()
    }

    /// The `thegn-spike` container's SID, creating the profile if needed.
    fn container_sid() -> Result<PSID, String> {
        let name = wide(PROFILE);
        let mut sid: PSID = std::ptr::null_mut();
        // SAFETY: `name` is NUL-terminated; `sid` receives an owned PSID.
        let hr = unsafe {
            CreateAppContainerProfile(
                name.as_ptr(),
                name.as_ptr(),
                name.as_ptr(),
                std::ptr::null_mut(),
                0,
                &mut sid,
            )
        };
        if hr >= 0 {
            return Ok(sid);
        }
        // Already exists (or we lack create rights): derive it instead.
        // SAFETY: same contract as above.
        let hr2 = unsafe { DeriveAppContainerSidFromAppContainerName(name.as_ptr(), &mut sid) };
        if hr2 >= 0 {
            Ok(sid)
        } else {
            Err(format!(
                "could not create or derive the profile SID (0x{hr2:x})"
            ))
        }
    }

    /// Re-launch `command_line` inheriting THIS process's console.
    ///
    /// With `sid`, the child runs in that AppContainer. Without, it is an
    /// ordinary child — the control that separates "trampoline" from
    /// "container" in the results.
    fn spawn_inheriting_console(sid: Option<PSID>, command_line: &str) -> Result<u32, String> {
        let mut si: STARTUPINFOEXW = unsafe { std::mem::zeroed() };
        si.StartupInfo.cb = std::mem::size_of::<STARTUPINFOEXW>() as u32;

        let mut caps;
        let mut buf;
        let mut flags = 0u32;
        if let Some(sid) = sid {
            caps = SECURITY_CAPABILITIES {
                AppContainerSid: sid,
                Capabilities: std::ptr::null_mut(),
                CapabilityCount: 0,
                Reserved: 0,
            };
            let mut size: usize = 0;
            // SAFETY: documented to fail and report the required size.
            unsafe { InitializeProcThreadAttributeList(std::ptr::null_mut(), 1, 0, &mut size) };
            buf = vec![0u8; size];
            let attrs = buf.as_mut_ptr() as *mut _;
            // SAFETY: `attrs` points at `size` bytes allocated just above.
            if unsafe { InitializeProcThreadAttributeList(attrs, 1, 0, &mut size) } == 0 {
                return Err("InitializeProcThreadAttributeList failed".into());
            }
            // SAFETY: `caps` outlives the CreateProcessW call below.
            let ok = unsafe {
                UpdateProcThreadAttribute(
                    attrs,
                    0,
                    PROC_THREAD_ATTRIBUTE_SECURITY_CAPABILITIES as usize,
                    &mut caps as *mut _ as *mut _,
                    std::mem::size_of::<SECURITY_CAPABILITIES>(),
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                )
            };
            if ok == 0 {
                return Err("UpdateProcThreadAttribute(SECURITY_CAPABILITIES) failed".into());
            }
            si.lpAttributeList = attrs;
            flags = EXTENDED_STARTUPINFO_PRESENT;
        }

        let mut pi: PROCESS_INFORMATION = unsafe { std::mem::zeroed() };
        let mut cmd = wide(command_line);
        // `bInheritHandles = TRUE` is the load-bearing argument: it is how the
        // child gets the console this trampoline is attached to.
        // SAFETY: cmd is a NUL-terminated mutable buffer; si/pi are live locals.
        let spawned = unsafe {
            CreateProcessW(
                std::ptr::null(),
                cmd.as_mut_ptr(),
                std::ptr::null(),
                std::ptr::null(),
                1,
                flags,
                std::ptr::null(),
                std::ptr::null(),
                &si.StartupInfo,
                &mut pi,
            )
        };
        if !si.lpAttributeList.is_null() {
            // SAFETY: initialised above, no longer referenced.
            unsafe { DeleteProcThreadAttributeList(si.lpAttributeList) };
        }
        if spawned == 0 {
            return Err(format!(
                "CreateProcessW failed: {}",
                std::io::Error::last_os_error()
            ));
        }
        // SAFETY: handles come from a successful CreateProcessW.
        unsafe {
            WaitForSingleObject(pi.hProcess, INFINITE);
            let mut code: u32 = 0;
            GetExitCodeProcess(pi.hProcess, &mut code);
            close(pi.hThread);
            close(pi.hProcess);
            Ok(code)
        }
    }

    unsafe fn close(h: HANDLE) {
        if !h.is_null() && h != INVALID_HANDLE_VALUE {
            unsafe { CloseHandle(h) };
        }
    }
}
