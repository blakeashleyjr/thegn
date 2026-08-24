//! The AppContainer trampoline: re-launch a command under a container token,
//! inheriting the console this process was given.
//!
//! # Why a separate process at all
//!
//! A pane is a ConPTY, and portable-pty owns the `STARTUPINFOEX` attribute list
//! for that spawn — it must set `PROC_THREAD_ATTRIBUTE_PSEUDOCONSOLE` — and does
//! not expose it. There is nowhere to add
//! `PROC_THREAD_ATTRIBUTE_SECURITY_CAPABILITIES`, so the contained shell cannot
//! be the process portable-pty starts. It has to be its **child**: thegn spawns
//! this trampoline into the ConPTY the ordinary way, and the trampoline
//! `CreateProcessW`s the real shell with the container attribute, passing on the
//! console it already holds.
//!
//! That the console survives that hop is not an assumption —
//! `examples/appcontainer_conpty_spike.rs` measures it, and measures that the
//! token boundary is genuinely in force at the same time (the contained
//! grandchild is denied a file its uncontained sibling reads).
//!
//! # Lifetime
//!
//! The trampoline **waits** for the grandchild and exits with its status, rather
//! than exec-ing away. Windows has no `exec`, and the pane's lifetime is the
//! trampoline's process handle as far as the compositor is concerned: exiting
//! early would report the pane dead while the shell was still running.

#[cfg(not(windows))]
pub fn run(_profile: &str, _capabilities: &[String], _argv: &[String]) -> anyhow::Result<i32> {
    anyhow::bail!("appcontainer-exec is Windows-only (AppContainer is a Win32 concept)")
}

/// Create the profile and apply the pane's grants, returning what could not be
/// granted.
///
/// **Grant what we can, report the rest.** thegn never elevates to force a grant
/// through — an ACL change on `C:\Program Files\…` is the machine owner's call,
/// not a side effect of opening a pane. So a toolchain that stays unreachable
/// comes back as a warning carrying the exact `icacls` command, and the caller
/// decides: the worktree grant failing is fatal (a pane that cannot read its own
/// files is not a pane), a toolchain grant failing is not.
#[cfg(not(windows))]
pub fn prepare(_spec: &thegn_core::sandbox::SandboxSpec) -> anyhow::Result<Vec<String>> {
    anyhow::bail!("the appcontainer backend is Windows-only")
}

#[cfg(windows)]
pub fn prepare(spec: &thegn_core::sandbox::SandboxSpec) -> anyhow::Result<Vec<String>> {
    win::prepare(spec)
}

#[cfg(windows)]
pub fn run(profile: &str, capabilities: &[String], argv: &[String]) -> anyhow::Result<i32> {
    win::run(profile, capabilities, argv)
}

#[cfg(windows)]
mod win {
    use std::ffi::OsStr;
    use std::os::windows::ffi::OsStrExt;

    use anyhow::{Context, bail};
    use windows_sys::Win32::Foundation::{CloseHandle, HANDLE, INVALID_HANDLE_VALUE, LocalFree};
    use windows_sys::Win32::Security::Authorization::ConvertSidToStringSidW;
    use windows_sys::Win32::Security::Isolation::{
        CreateAppContainerProfile, DeriveAppContainerSidFromAppContainerName,
    };
    use windows_sys::Win32::Security::{DeriveCapabilitySidsFromName, PSID, SECURITY_CAPABILITIES};
    use windows_sys::Win32::System::Threading::{
        CreateProcessW, DeleteProcThreadAttributeList, EXTENDED_STARTUPINFO_PRESENT,
        GetExitCodeProcess, INFINITE, InitializeProcThreadAttributeList, PROCESS_INFORMATION,
        STARTUPINFOEXW, UpdateProcThreadAttribute, WaitForSingleObject,
    };

    /// `PROC_THREAD_ATTRIBUTE_SECURITY_CAPABILITIES`. windows-sys does not export
    /// it; the value is `ProcThreadAttributeSecurityCapabilities (9)` packed with
    /// `PROC_THREAD_ATTRIBUTE_INPUT`, i.e. `9 | 0x00020000`.
    const PROC_THREAD_ATTRIBUTE_SECURITY_CAPABILITIES: usize = 131081;

    fn wide(s: &str) -> Vec<u16> {
        OsStr::new(s).encode_wide().chain(Some(0)).collect()
    }

    /// Derive the profile's SID, creating the profile if it does not exist.
    ///
    /// Creation is idempotent in practice: a second run gets
    /// `HRESULT_FROM_WIN32(ERROR_ALREADY_EXISTS)` and falls through to the
    /// derive, which is also the path taken when the user lacks create rights but
    /// the profile is already there.
    fn container_sid(profile: &str) -> anyhow::Result<PSID> {
        let name = wide(profile);
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
        // SAFETY: same contract.
        let hr2 = unsafe { DeriveAppContainerSidFromAppContainerName(name.as_ptr(), &mut sid) };
        if hr2 >= 0 {
            Ok(sid)
        } else {
            bail!("could not create or derive AppContainer profile {profile:?} (0x{hr2:08x})")
        }
    }

    /// Resolve well-known capability names to their SIDs.
    ///
    /// Returns the SIDs plus the buffers Windows allocated for them, which must
    /// outlive the `CreateProcessW` call — hence handing both back rather than
    /// freeing inside.
    fn capability_sids(
        names: &[String],
    ) -> anyhow::Result<(Vec<SID_AND_ATTRIBUTES>, Vec<Freeable>)> {
        let mut attrs = Vec::new();
        let mut owned = Vec::new();
        for name in names {
            let w = wide(name);
            let mut group_sids: *mut PSID = std::ptr::null_mut();
            let mut group_count: u32 = 0;
            let mut sids: *mut PSID = std::ptr::null_mut();
            let mut count: u32 = 0;
            // SAFETY: `w` is NUL-terminated; the four out-params receive
            // LocalAlloc'd arrays we free via `Freeable`.
            let ok = unsafe {
                DeriveCapabilitySidsFromName(
                    w.as_ptr(),
                    &mut group_sids,
                    &mut group_count,
                    &mut sids,
                    &mut count,
                )
            };
            if ok == 0 || count == 0 {
                bail!("unknown or unusable capability {name:?}");
            }
            // SAFETY: `sids` points at `count` PSIDs from the call above.
            let first = unsafe { *sids };
            attrs.push(SID_AND_ATTRIBUTES {
                Sid: first,
                Attributes: SE_GROUP_ENABLED,
            });
            owned.push(Freeable(sids as *mut _));
            if !group_sids.is_null() {
                owned.push(Freeable(group_sids as *mut _));
            }
        }
        Ok((attrs, owned))
    }

    /// `SID_AND_ATTRIBUTES` / `SE_GROUP_ENABLED` spelled locally: windows-sys
    /// puts them behind a different feature set than the rest of this file uses.
    // Mirrors the Win32 struct field-for-field, so it keeps Win32 spelling: a
    // snake_case rename here would obscure what it maps onto.
    #[allow(non_snake_case)]
    #[repr(C)]
    struct SID_AND_ATTRIBUTES {
        Sid: PSID,
        Attributes: u32,
    }
    const SE_GROUP_ENABLED: u32 = 0x4;

    /// A `LocalAlloc`'d block freed on drop, so an early `?` cannot leak it.
    struct Freeable(*mut core::ffi::c_void);
    impl Drop for Freeable {
        fn drop(&mut self) {
            if !self.0.is_null() {
                // SAFETY: every construction site passes a LocalAlloc'd pointer.
                unsafe { LocalFree(self.0) };
            }
        }
    }

    /// Render a SID to its `S-1-…` string form.
    fn sid_to_string(sid: PSID) -> anyhow::Result<String> {
        let mut out: *mut u16 = std::ptr::null_mut();
        // SAFETY: `sid` is valid; `out` receives a LocalAlloc'd wide string.
        if unsafe { ConvertSidToStringSidW(sid, &mut out) } == 0 {
            bail!(
                "ConvertSidToStringSidW failed: {}",
                std::io::Error::last_os_error()
            );
        }
        let _free = Freeable(out as *mut _);
        let mut len = 0usize;
        // SAFETY: `out` is NUL-terminated.
        while unsafe { *out.add(len) } != 0 {
            len += 1;
        }
        // SAFETY: `len` is the length just measured.
        Ok(String::from_utf16_lossy(unsafe {
            std::slice::from_raw_parts(out, len)
        }))
    }

    // `icacls` is a short, bounded subprocess and this runs on the pane-spawn
    // path, which is already off the event loop (the same place `sandbox::ensure`
    // shells out to podman) - never on the render loop.
    #[expect(clippy::disallowed_methods)]
    pub fn prepare(spec: &thegn_core::sandbox::SandboxSpec) -> anyhow::Result<Vec<String>> {
        use thegn_core::sandbox_appcontainer as ac;

        let profile = ac::profile_name(&spec.worktree.to_string_lossy());
        let sid = sid_to_string(container_sid(&profile)?)?;

        // The tools a pane actually needs reachable. `git` is the one that bites:
        // `C:\Program Files\Git` carries no ALL APPLICATION PACKAGES ACE, while
        // System32 (and therefore cmd.exe) does — which is why a trivial probe
        // passes and a real pane then cannot run git.
        let tools: Vec<std::path::PathBuf> = ["git", "pwsh", "powershell"]
            .iter()
            .filter_map(|t| thegn_core::util::which_path(t))
            .map(std::path::PathBuf::from)
            .collect();

        let plan = ac::plan(&spec.worktree, spec.network, &tools);
        let mut warnings = Vec::new();
        for grant in &plan.grants {
            if !grant.path.exists() {
                continue;
            }
            let argv = ac::icacls_argv(grant, &sid);
            let ok = std::process::Command::new("icacls")
                .args(&argv)
                .output()
                .map(|o| o.status.success())
                .unwrap_or(false);
            if ok {
                continue;
            }
            if grant.write {
                // The worktree. Without it the pane cannot read or edit a single
                // file, so failing here must not produce a half-working sandbox —
                // the caller falls through to the next backend.
                bail!(
                    "could not grant the worktree to AppContainer {profile}: {}\n  run it \
                     yourself with:  {}",
                    grant.needed_for,
                    ac::manual_grant_hint(grant, &sid)
                );
            }
            warnings.push(format!(
                "appcontainer: {} is not reachable inside the sandbox ({}). Grant it with:  {}",
                grant.path.display(),
                grant.needed_for,
                ac::manual_grant_hint(grant, &sid)
            ));
        }
        Ok(warnings)
    }

    pub fn run(profile: &str, capabilities: &[String], argv: &[String]) -> anyhow::Result<i32> {
        let sid = container_sid(profile)?;
        let (mut cap_attrs, _owned) =
            capability_sids(capabilities).context("resolving network capabilities")?;

        let mut caps = SECURITY_CAPABILITIES {
            AppContainerSid: sid,
            Capabilities: if cap_attrs.is_empty() {
                std::ptr::null_mut()
            } else {
                cap_attrs.as_mut_ptr() as *mut _
            },
            CapabilityCount: cap_attrs.len() as u32,
            Reserved: 0,
        };

        // Two-call idiom: ask for the size, allocate, initialise.
        let mut size: usize = 0;
        // SAFETY: documented to fail and report the required size.
        unsafe { InitializeProcThreadAttributeList(std::ptr::null_mut(), 1, 0, &mut size) };
        let mut buf = vec![0u8; size];
        let attrs = buf.as_mut_ptr() as *mut _;
        // SAFETY: `attrs` points at `size` bytes allocated just above.
        if unsafe { InitializeProcThreadAttributeList(attrs, 1, 0, &mut size) } == 0 {
            bail!(
                "InitializeProcThreadAttributeList failed: {}",
                std::io::Error::last_os_error()
            );
        }
        // SAFETY: `caps` (and the SIDs it points at) outlive CreateProcessW below.
        let ok = unsafe {
            UpdateProcThreadAttribute(
                attrs,
                0,
                PROC_THREAD_ATTRIBUTE_SECURITY_CAPABILITIES,
                &mut caps as *mut _ as *mut _,
                std::mem::size_of::<SECURITY_CAPABILITIES>(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
            )
        };
        if ok == 0 {
            // SAFETY: initialised above.
            unsafe { DeleteProcThreadAttributeList(attrs) };
            bail!(
                "UpdateProcThreadAttribute(SECURITY_CAPABILITIES) failed: {}",
                std::io::Error::last_os_error()
            );
        }

        let mut si: STARTUPINFOEXW = unsafe { std::mem::zeroed() };
        si.StartupInfo.cb = std::mem::size_of::<STARTUPINFOEXW>() as u32;
        si.lpAttributeList = attrs;
        let mut pi: PROCESS_INFORMATION = unsafe { std::mem::zeroed() };
        let mut cmd = wide(&join_argv(argv));

        // `bInheritHandles = TRUE` is the load-bearing argument: it is how the
        // grandchild gets the ConPTY this trampoline is attached to.
        // SAFETY: `cmd` is a NUL-terminated mutable buffer as CreateProcessW
        // requires; `si`/`pi` are live locals.
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
        // SAFETY: initialised above and no longer referenced.
        unsafe { DeleteProcThreadAttributeList(attrs) };
        if spawned == 0 {
            let e = std::io::Error::last_os_error();
            bail!(
                "could not start {:?} in AppContainer {profile:?}: {e}",
                argv[0]
            );
        }

        // SAFETY: handles come from a successful CreateProcessW.
        let code = unsafe {
            WaitForSingleObject(pi.hProcess, INFINITE);
            let mut code: u32 = 0;
            GetExitCodeProcess(pi.hProcess, &mut code);
            close(pi.hThread);
            close(pi.hProcess);
            code
        };
        Ok(code as i32)
    }

    /// Join argv into a command line, quoting only what needs it.
    ///
    /// `CreateProcessW` takes one string, so the split argv has to be reassembled.
    /// An unquoted path with a space (`C:\Program Files\Git\cmd\git.exe`) would be
    /// read as two arguments — the failure that makes a pane silently run the
    /// wrong program.
    fn join_argv(argv: &[String]) -> String {
        argv.iter()
            .map(|a| {
                if a.is_empty() || a.contains([' ', '\t', '"']) {
                    format!("\"{}\"", a.replace('"', "\\\""))
                } else {
                    a.clone()
                }
            })
            .collect::<Vec<_>>()
            .join(" ")
    }

    unsafe fn close(h: HANDLE) {
        if !h.is_null() && h != INVALID_HANDLE_VALUE {
            unsafe { CloseHandle(h) };
        }
    }

    #[cfg(test)]
    mod tests {
        use super::join_argv;

        #[test]
        fn argv_with_spaces_is_quoted_back_together() {
            let s = join_argv(&[
                r"C:\Program Files\Git\cmd\git.exe".into(),
                "status".into(),
                "--short".into(),
            ]);
            assert_eq!(
                s, r#""C:\Program Files\Git\cmd\git.exe" status --short"#,
                "an unquoted space would split the program path into two arguments"
            );
        }

        #[test]
        fn embedded_quotes_survive() {
            assert_eq!(join_argv(&[r#"say "hi""#.into()]), r#""say \"hi\"""#);
        }

        #[test]
        fn an_empty_argument_is_preserved() {
            // Dropping it would shift every later argument by one.
            assert_eq!(join_argv(&["cmd".into(), String::new()]), r#"cmd """#);
        }
    }
}
