//! Spike: can thegn run a pane inside a Windows **AppContainer**?
//!
//! Windows has no sandbox in thegn today. OCI now works there (see
//! `sandbox::container_path`), but it needs Podman/Docker Desktop and a WSL2
//! VM. AppContainer is the native equivalent of the `bwrap` tier: a real
//! security boundary — its own SID, deny-by-default access to the filesystem
//! and registry, capability-gated network — with no VM, no image, and **no path
//! translation**, since it is the same filesystem seen through a weaker token.
//!
//! # The question this spike answers
//!
//! thegn cannot simply ask portable-pty to spawn into an AppContainer: the
//! ConPTY spawn owns the `STARTUPINFOEX` attribute list (it must set
//! `PROC_THREAD_ATTRIBUTE_PSEUDOCONSOLE`) and does not expose it, so there is
//! nowhere to add `PROC_THREAD_ATTRIBUTE_SECURITY_CAPABILITIES`.
//!
//! The way around is a **trampoline**: thegn spawns a small helper into the
//! ConPTY the normal way, and the helper re-launches the real program with the
//! AppContainer attribute list, inheriting its own (already console-attached)
//! std handles. This file is that helper, reduced to the smallest thing that
//! can prove the idea — plus probes for the things most likely to sink it.
//!
//! # Run it
//!
//! ```text
//! cargo run -p thegn-host --example appcontainer_spike
//! ```
//!
//! It creates (or reuses) an AppContainer profile named `thegn-spike`, grants
//! one temp directory to that container's SID, launches a probe inside, and
//! reports what the probe could and could not do. Nothing outside that temp
//! directory is modified, and the profile is left behind deliberately so a
//! second run exercises the reuse path.

// A spike is a standalone report, not compositor code: its stdout IS the
// deliverable, and its blocking `icacls` calls are the experiment rather than
// something that could stall a render loop.
#![allow(clippy::disallowed_macros, clippy::disallowed_methods)]

fn main() {
    #[cfg(not(windows))]
    {
        eprintln!("appcontainer_spike: Windows-only (AppContainer is a Win32 concept)");
    }
    #[cfg(windows)]
    win::run();
}

#[cfg(windows)]
mod win {
    use std::ffi::OsStr;
    use std::os::windows::ffi::OsStrExt;

    use windows_sys::Win32::Foundation::{CloseHandle, HANDLE, INVALID_HANDLE_VALUE, LocalFree};
    use windows_sys::Win32::Security::Authorization::ConvertSidToStringSidW;
    use windows_sys::Win32::Security::Isolation::{
        CreateAppContainerProfile, DeriveAppContainerSidFromAppContainerName,
    };
    use windows_sys::Win32::Security::{PSID, SECURITY_CAPABILITIES};
    use windows_sys::Win32::System::Threading::{
        CreateProcessW, DeleteProcThreadAttributeList, EXTENDED_STARTUPINFO_PRESENT,
        GetExitCodeProcess, INFINITE, InitializeProcThreadAttributeList,
        PROC_THREAD_ATTRIBUTE_SECURITY_CAPABILITIES, PROCESS_INFORMATION, STARTUPINFOEXW,
        UpdateProcThreadAttribute, WaitForSingleObject,
    };

    /// The container's identity. Stable, so a second run reuses the profile
    /// rather than accumulating one per launch — the same thing thegn would do
    /// per worktree.
    const CONTAINER: &str = "thegn-spike";

    fn wide(s: &str) -> Vec<u16> {
        OsStr::new(s).encode_wide().chain(Some(0)).collect()
    }

    /// `HRESULT_FROM_WIN32(ERROR_ALREADY_EXISTS)` — the profile survives across
    /// runs, so "already there" is the expected second-run answer, not a fault.
    const E_ALREADY_EXISTS: i32 = -2147024713; // 0x800700B7

    /// Create the AppContainer profile if needed, and return its SID.
    ///
    /// The SID is what everything else keys off: it is the identity an ACL
    /// grants to, and the one the child process runs as.
    fn container_sid() -> Result<PSID, String> {
        let name = wide(CONTAINER);
        let display = wide("thegn sandbox spike");
        let mut sid: PSID = std::ptr::null_mut();
        // SAFETY: all pointers are to live locals; `sid` is an out-param the
        // API fills with a LocalAlloc'd SID (freed by the caller).
        let hr = unsafe {
            CreateAppContainerProfile(
                name.as_ptr(),
                display.as_ptr(),
                display.as_ptr(),
                std::ptr::null(),
                0,
                &mut sid,
            )
        };
        if hr == 0 {
            println!("  profile   created `{CONTAINER}`");
            return Ok(sid);
        }
        if hr != E_ALREADY_EXISTS {
            return Err(format!("CreateAppContainerProfile failed: 0x{hr:08x}"));
        }
        println!("  profile   reusing existing `{CONTAINER}`");
        // SAFETY: as above; derive fills the same kind of out-param.
        let hr = unsafe { DeriveAppContainerSidFromAppContainerName(name.as_ptr(), &mut sid) };
        if hr != 0 {
            return Err(format!("DeriveAppContainerSid failed: 0x{hr:08x}"));
        }
        Ok(sid)
    }

    fn sid_to_string(sid: PSID) -> String {
        let mut out: *mut u16 = std::ptr::null_mut();
        // SAFETY: `sid` is a valid SID; `out` receives a LocalAlloc'd string we
        // free below.
        unsafe {
            if ConvertSidToStringSidW(sid, &mut out) == 0 {
                return "<unprintable>".into();
            }
            let mut len = 0;
            while *out.add(len) != 0 {
                len += 1;
            }
            let s = String::from_utf16_lossy(std::slice::from_raw_parts(out, len));
            LocalFree(out as *mut _);
            s
        }
    }

    /// Spawn `command_line` inside the AppContainer, inheriting our own std
    /// handles — which is the whole trampoline idea: when thegn runs THIS
    /// process inside a ConPTY, those handles are the pseudoconsole, so the
    /// sandboxed child lands on the pane without ever touching
    /// `PROC_THREAD_ATTRIBUTE_PSEUDOCONSOLE` itself.
    fn spawn_in_container(sid: PSID, command_line: &str) -> Result<u32, String> {
        let mut caps = SECURITY_CAPABILITIES {
            AppContainerSid: sid,
            Capabilities: std::ptr::null_mut(),
            CapabilityCount: 0,
            Reserved: 0,
        };

        // Two-call idiom: ask for the size, allocate, initialise.
        let mut size: usize = 0;
        // SAFETY: the first call is documented to fail with the required size.
        unsafe { InitializeProcThreadAttributeList(std::ptr::null_mut(), 1, 0, &mut size) };
        let mut buf = vec![0u8; size];
        let attrs = buf.as_mut_ptr() as *mut _;
        // SAFETY: `attrs` points at `size` bytes we just allocated for it.
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

        let mut si: STARTUPINFOEXW = unsafe { std::mem::zeroed() };
        si.StartupInfo.cb = std::mem::size_of::<STARTUPINFOEXW>() as u32;
        si.lpAttributeList = attrs;
        let mut pi: PROCESS_INFORMATION = unsafe { std::mem::zeroed() };
        let mut cmd = wide(command_line);

        // `bInheritHandles = TRUE` is the load-bearing argument: it is how the
        // child gets our console.
        // SAFETY: cmd is a NUL-terminated mutable buffer as CreateProcessW
        // requires; si/pi are live locals.
        let spawned = unsafe {
            CreateProcessW(
                std::ptr::null(),
                cmd.as_mut_ptr(),
                std::ptr::null(),
                std::ptr::null(),
                1,
                EXTENDED_STARTUPINFO_PRESENT,
                std::ptr::null(),
                std::ptr::null(),
                &si.StartupInfo,
                &mut pi,
            )
        };
        // SAFETY: the list was initialised above and is no longer referenced.
        unsafe { DeleteProcThreadAttributeList(attrs) };
        if spawned == 0 {
            let e = std::io::Error::last_os_error();
            return Err(format!("CreateProcessW failed: {e}"));
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

    /// Grant the container SID full access to `dir` via `icacls`.
    ///
    /// The real implementation would call `SetNamedSecurityInfoW`; `icacls` is
    /// enough to learn whether the grant is what makes the difference, which is
    /// the only thing the spike needs to know. `*<SID>` is icacls' spelling for
    /// "this is a SID, not a name".
    fn grant(dir: &std::path::Path, sid_str: &str) -> bool {
        std::process::Command::new("icacls")
            .arg(dir)
            .arg("/grant")
            .arg(format!("*{sid_str}:(OI)(CI)F"))
            .arg("/T")
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
    }

    pub fn run() {
        println!("AppContainer trampoline spike");

        let sid = match container_sid() {
            Ok(s) => s,
            Err(e) => {
                println!("  FAIL      {e}");
                return;
            }
        };
        let sid_str = sid_to_string(sid);
        println!("  sid       {sid_str}");

        let root = std::env::temp_dir().join(format!("thegn-ac-spike-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&root);
        let inside = root.join("granted");
        let _ = std::fs::create_dir_all(&inside);
        std::fs::write(inside.join("seed.txt"), b"hello from the host\n").expect("seed");

        // A file the container is deliberately NOT granted — the control for
        // "is this actually a boundary, or just a normal process?".
        let outside = root.join("denied.txt");
        std::fs::write(&outside, b"secret\n").expect("seed denied");

        println!("  granted   {}", inside.display());
        println!("  denied    {}", outside.display());
        println!(
            "  icacls    {}",
            if grant(&inside, &sid_str) {
                "ok"
            } else {
                "FAILED"
            }
        );

        // A toolchain directory, granted read+execute to the container.
        //
        // This is the question the first run raised: `cmd.exe` worked because
        // System32 carries `ALL APPLICATION PACKAGES:(RX)`, but nothing a dev
        // pane actually needs does — `C:\Program Files\Git` has no APPLICATION
        // PACKAGES entry at all, so `git` was simply invisible. Point
        // `THEGN_AC_SPIKE_TOOL_DIR` at a tool directory and
        // `THEGN_AC_SPIKE_TOOL` at an executable in it to find out whether an
        // explicit grant is enough to make it runnable.
        let tool = std::env::var("THEGN_AC_SPIKE_TOOL").ok();
        let tool_probe = match (std::env::var("THEGN_AC_SPIKE_TOOL_DIR").ok(), &tool) {
            (Some(dir), Some(exe)) => {
                let d = std::path::PathBuf::from(&dir);
                // Read+execute only: a toolchain is something the sandbox runs,
                // never something it should be able to rewrite.
                let granted = std::process::Command::new("icacls")
                    .arg(&d)
                    .args(["/grant", &format!("*{sid_str}:(OI)(CI)RX"), "/T"])
                    .output()
                    .map(|o| o.status.success())
                    .unwrap_or(false);
                println!(
                    "  tool dir  {} (grant RX: {})",
                    d.display(),
                    if granted {
                        "ok"
                    } else {
                        "FAILED — admin needed?"
                    }
                );
                format!(
                    " & (\"{}\" --version 2>nul || echo [probe] tool: UNAVAILABLE)",
                    d.join(exe).display()
                )
            }
            _ => String::new(),
        };

        // Execution from a granted directory, isolated from any question about
        // a particular tool's install. A copy of `cmd.exe` (no unusual
        // dependencies, and its DLLs live in the already-readable System32)
        // placed in the directory the container demonstrably CAN read and
        // write. If this runs, granting is sufficient to make a binary
        // executable and the problem is the toolchain's own ACLs; if it does
        // not, AppContainer refuses to execute from user-writable locations at
        // all — which would be the end of this approach.
        let copied = inside.join("copied-cmd.exe");
        let have_copy = std::fs::copy(r"C:\Windows\System32\cmd.exe", &copied).is_ok();
        // Re-grant so the freshly copied file carries the ACE too.
        let _ = grant(&inside, &sid_str);
        println!(
            "  exec test {} ({})",
            copied.display(),
            if have_copy { "copied" } else { "COPY FAILED" }
        );

        // The probe runs INSIDE the container and reports what it could do.
        // `cmd.exe` is used rather than PowerShell: System32 grants
        // ALL APPLICATION PACKAGES read+execute, and cmd has fewer moving parts
        // than a profile-loading shell — one less thing to blame if this fails.
        let probe = format!(
            "echo [probe] running as: %USERNAME% & \
             (type \"{seed}\" >nul 2>&1 && echo [probe] READ granted-dir: ok || echo [probe] READ granted-dir: DENIED) & \
             (echo written-by-container> \"{write}\" 2>nul && echo [probe] WRITE granted-dir: ok || echo [probe] WRITE granted-dir: DENIED) & \
             (type \"{denied}\" >nul 2>&1 && echo [probe] READ denied-file: LEAKED || echo [probe] READ denied-file: denied) & \
             (git --version 2>nul || echo [probe] git-on-PATH: UNAVAILABLE){tool}",
            seed = inside.join("seed.txt").display(),
            write = inside.join("written.txt").display(),
            denied = outside.display(),
            tool = tool_probe,
        );
        // NB: do NOT add a nested-quoted command to this probe. An inner
        // `"C:\path\x.exe"` inside `cmd.exe /c "…"` breaks the outer quoting
        // and the `||` fires on a PARSE error, which reads exactly like an
        // access denial and sent this spike chasing one. The execute question
        // is answered by the direct spawn below instead, where a failure is
        // `CreateProcessW`'s and carries a real error code.
        let cmdline = format!("cmd.exe /c \"{probe}\"");

        println!("  --- probe output (inside the container) ---");
        match spawn_in_container(sid, &cmdline) {
            Ok(code) => println!("  --- probe exited {code} ---"),
            Err(e) => println!("  FAIL      spawn: {e}"),
        }

        // The probe reported EXEC as denied through a swallowed `||`, which
        // names no cause. Launch the copied binary AS the container's own
        // process instead: then the failure is `CreateProcessW`'s, and comes
        // back as a real Win32 error rather than a shell shrug.
        if have_copy {
            let direct = format!("\"{}\" /c exit 0", copied.display());
            match spawn_in_container(sid, &direct) {
                Ok(code) => println!("  direct    exec of the granted copy: ok (exit {code})"),
                Err(e) => println!("  direct    exec of the granted copy: {e}"),
            }
        }

        // Did the write actually land on the host side?
        let wrote = inside.join("written.txt");
        println!(
            "  verdict   container write visible on host: {}",
            if wrote.exists() { "YES" } else { "no" }
        );

        println!("  cleanup   {}", root.display());
        let _ = std::fs::remove_dir_all(&root);
    }
}
