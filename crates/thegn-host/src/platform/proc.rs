//! Live per-process introspection for pane bookkeeping: a pane's working
//! directory, its shell's foreground child job, and that child's argv.
//!
//! These back two session features — "respawn panes where they were"
//! (`snapshot::capture_pane_cwds`) and "offer to relaunch what was running"
//! (`snapshot::capture_pane_cmds`) — both of which run a loop over live panes at
//! persist time. That cost shape is why this is a syscall seam rather than a
//! `sysinfo` call: `sysinfo` would refresh the whole process table once per
//! pane, turning an O(panes) pass into O(panes x processes). Every function
//! here is O(1) or O(children) in the pid it is asked about.
//!
//! Platform coverage:
//! * **Linux** reads `/proc` directly (this is the original implementation,
//!   moved here unchanged).
//! * **macOS** uses the `libproc` / `sysctl` calls that `/proc` stands in for.
//!   No new dependency: `libc` already declares all of them for apple targets.
//! * **Everything else** returns `None`, which is what the `/proc` reads
//!   silently degraded to before this module existed.
//!
//! Every function is best-effort and returns `None` on any failure: a pid that
//! exited mid-call, a process owned by another user, a permission refusal. The
//! callers all treat absence as "no information", never as an error.

use std::path::PathBuf;

// ── Linux ───────────────────────────────────────────────────────────────────

/// The process's current working directory.
#[cfg(target_os = "linux")]
pub(crate) fn cwd_of(pid: u32) -> Option<PathBuf> {
    std::fs::read_link(format!("/proc/{pid}/cwd")).ok()
}

/// The most-recently-started direct child of `pid` (a shell's foreground job),
/// if any. Walks `/proc/*/stat` for the `ppid` field; ties break to the highest
/// pid (newest).
#[cfg(target_os = "linux")]
pub(crate) fn newest_child(pid: u32) -> Option<u32> {
    let mut best: Option<u32> = None;
    for ent in std::fs::read_dir("/proc").ok()?.flatten() {
        let Some(child) = ent.file_name().to_str().and_then(|s| s.parse::<u32>().ok()) else {
            continue;
        };
        if child != pid && stat_ppid(child) == Some(pid) {
            best = Some(best.map_or(child, |b| b.max(child)));
        }
    }
    best
}

/// The parent pid from `/proc/<pid>/stat`. The `comm` field (field 2) can itself
/// contain spaces and parens, so the numeric fields are parsed after the final
/// `)`: state is the first token, ppid the second.
#[cfg(target_os = "linux")]
fn stat_ppid(pid: u32) -> Option<u32> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let rest = &stat[stat.rfind(')')? + 1..];
    rest.split_whitespace().nth(1)?.parse().ok()
}

/// Parse `/proc/<pid>/cmdline` (NUL-separated argv) into a `Vec`, dropping empty
/// trailing entries. `None` when unreadable or empty (e.g. a kernel thread).
#[cfg(target_os = "linux")]
pub(crate) fn cmdline(pid: u32) -> Option<Vec<String>> {
    let raw = std::fs::read(format!("/proc/{pid}/cmdline")).ok()?;
    let argv: Vec<String> = raw
        .split(|&b| b == 0)
        .filter(|s| !s.is_empty())
        .map(|s| String::from_utf8_lossy(s).into_owned())
        .collect();
    (!argv.is_empty()).then_some(argv)
}

// ── macOS ───────────────────────────────────────────────────────────────────

/// The process's current working directory, via
/// `proc_pidinfo(PROC_PIDVNODEPATHINFO)` — the `/proc/<pid>/cwd` equivalent.
///
/// The kernel only hands another process's vnode info to the same user (or
/// root). That is exactly the set thegn cares about: every pane it tracks is its
/// own child.
#[cfg(target_os = "macos")]
pub(crate) fn cwd_of(pid: u32) -> Option<PathBuf> {
    use std::os::unix::ffi::OsStrExt as _;

    // SAFETY: `info` is a correctly-sized, fully-owned `proc_vnodepathinfo`, and
    // we hand the kernel its exact size. `proc_pidinfo` either fills it and
    // returns the byte count, or fails — it never writes past `buffersize`.
    let mut info = std::mem::MaybeUninit::<libc::proc_vnodepathinfo>::zeroed();
    let size = size_of::<libc::proc_vnodepathinfo>() as libc::c_int;
    let n = unsafe {
        libc::proc_pidinfo(
            pid as libc::c_int,
            libc::PROC_PIDVNODEPATHINFO,
            0,
            info.as_mut_ptr().cast(),
            size,
        )
    };
    if n < size {
        // A short read means the struct wasn't populated (dead pid, or denied).
        return None;
    }
    // SAFETY: the call above returned a full-size write, so `info` is init.
    let info = unsafe { info.assume_init() };
    // `vip_path` is declared as [[c_char; 32]; 32] rather than [c_char; 1024]
    // (a libc workaround for old rustc), so flatten it back to bytes.
    let raw: &[u8] = unsafe {
        std::slice::from_raw_parts(
            info.pvi_cdir.vip_path.as_ptr().cast::<u8>(),
            size_of_val(&info.pvi_cdir.vip_path),
        )
    };
    let end = raw.iter().position(|&b| b == 0).unwrap_or(raw.len());
    if end == 0 {
        return None;
    }
    Some(PathBuf::from(std::ffi::OsStr::from_bytes(&raw[..end])))
}

/// The most-recently-started direct child of `pid`, via `proc_listchildpids`.
///
/// Selection matches the Linux arm exactly — highest pid wins — so the two
/// platforms pick the same job in the same situations. (macOS could sort on
/// `proc_bsdinfo::pbi_start_tvsec` instead, which would be strictly more correct
/// across pid wraparound, but a gratuitous behaviour split between platforms is
/// worse than a rare mis-pick that both platforms share.)
#[cfg(target_os = "macos")]
pub(crate) fn newest_child(pid: u32) -> Option<u32> {
    // Ask for the size first, then read. The count can change between the two
    // calls, so the buffer gets headroom and a short read is fine.
    // SAFETY: a null buffer with size 0 is the documented "how big?" query.
    let want = unsafe { libc::proc_listchildpids(pid as libc::pid_t, std::ptr::null_mut(), 0) };
    if want <= 0 {
        return None;
    }
    let cap = (want as usize / size_of::<libc::pid_t>()).saturating_add(16);
    let mut pids: Vec<libc::pid_t> = vec![0; cap];
    // SAFETY: `pids` owns `cap` elements and we pass its exact byte length, so
    // the kernel cannot write out of bounds.
    let n = unsafe {
        libc::proc_listchildpids(
            pid as libc::pid_t,
            pids.as_mut_ptr().cast(),
            (cap * size_of::<libc::pid_t>()) as libc::c_int,
        )
    };
    if n <= 0 {
        return None;
    }
    let got = (n as usize / size_of::<libc::pid_t>()).min(cap);
    pids[..got]
        .iter()
        .filter(|&&c| c > 0 && c as u32 != pid)
        .map(|&c| c as u32)
        .max()
}

/// The process's argv, via `sysctl(KERN_PROCARGS2)` — the
/// `/proc/<pid>/cmdline` equivalent.
#[cfg(target_os = "macos")]
pub(crate) fn cmdline(pid: u32) -> Option<Vec<String>> {
    // KERN_ARGMAX is the kernel's own cap on this blob, so one read always
    // suffices — no grow-and-retry loop.
    let mut argmax: libc::c_int = 0;
    let mut len = size_of::<libc::c_int>();
    let mut mib = [libc::CTL_KERN, libc::KERN_ARGMAX];
    // SAFETY: `mib` has the declared 2 entries and `argmax`/`len` are a matching
    // out-param pair; no new value is written (null/0).
    let rc = unsafe {
        libc::sysctl(
            mib.as_mut_ptr(),
            mib.len() as libc::c_uint,
            std::ptr::from_mut(&mut argmax).cast(),
            &mut len,
            std::ptr::null_mut(),
            0,
        )
    };
    if rc != 0 || argmax <= 0 {
        return None;
    }

    let mut buf: Vec<u8> = vec![0; argmax as usize];
    let mut out_len = buf.len();
    let mut mib = [libc::CTL_KERN, libc::KERN_PROCARGS2, pid as libc::c_int];
    // SAFETY: `buf` owns `out_len` bytes and the kernel is told exactly that;
    // on success it updates `out_len` to what it actually wrote.
    let rc = unsafe {
        libc::sysctl(
            mib.as_mut_ptr(),
            mib.len() as libc::c_uint,
            buf.as_mut_ptr().cast(),
            &mut out_len,
            std::ptr::null_mut(),
            0,
        )
    };
    if rc != 0 {
        // EINVAL for a dead pid, EPERM for another user's process.
        return None;
    }
    buf.truncate(out_len);
    parse_procargs2(&buf)
}

/// Decode a `KERN_PROCARGS2` blob into argv.
///
/// Layout: a `u32` argc, then the NUL-terminated executable path, then padding
/// NULs, then exactly `argc` NUL-terminated argument strings (the environment
/// follows, and is ignored). The exec path is deliberately skipped — it is the
/// resolved binary, whereas argv[0] is what the caller actually invoked, which
/// is what the Linux `/proc/<pid>/cmdline` arm yields and what the pane
/// relaunch hint wants.
///
/// Kept out of the `cfg(target_os = "macos")` gate for `test` so the parsing —
/// the only part with real edge cases — is exercised on the Linux dev/CI box.
#[cfg(any(target_os = "macos", test))]
fn parse_procargs2(buf: &[u8]) -> Option<Vec<String>> {
    let (argc_bytes, rest) = buf.split_at_checked(size_of::<u32>())?;
    let argc = u32::from_ne_bytes(argc_bytes.try_into().ok()?) as usize;
    if argc == 0 {
        return None;
    }
    // Skip the exec path and the alignment NULs padding it out.
    let after_path = rest.iter().position(|&b| b == 0)? + 1;
    let rest = &rest[after_path..];
    let start = rest.iter().position(|&b| b != 0)?;
    let rest = &rest[start..];

    let argv: Vec<String> = rest
        .split(|&b| b == 0)
        .take(argc)
        .map(|s| String::from_utf8_lossy(s).into_owned())
        .collect();
    // A truncated blob can yield fewer entries than argc promised; take what is
    // there rather than dropping the whole answer.
    let argv: Vec<String> = argv.into_iter().filter(|s| !s.is_empty()).collect();
    (!argv.is_empty()).then_some(argv)
}

// ── Everything else (Windows, other unices) ─────────────────────────────────
//
// The pre-existing behaviour: these reads targeted `/proc` unconditionally and
// simply failed off Linux. Keeping explicit `None` arms makes that a decision
// rather than an accident.

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
pub(crate) fn cwd_of(_pid: u32) -> Option<PathBuf> {
    None
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
pub(crate) fn newest_child(_pid: u32) -> Option<u32> {
    None
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
pub(crate) fn cmdline(_pid: u32) -> Option<Vec<String>> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a KERN_PROCARGS2-shaped blob: argc, exec path, padding, argv, env.
    fn blob(argc: u32, exec_path: &str, pad: usize, argv: &[&str], env: &[&str]) -> Vec<u8> {
        let mut v = argc.to_ne_bytes().to_vec();
        v.extend_from_slice(exec_path.as_bytes());
        v.push(0);
        v.extend(std::iter::repeat_n(0u8, pad));
        for a in argv {
            v.extend_from_slice(a.as_bytes());
            v.push(0);
        }
        for e in env {
            v.extend_from_slice(e.as_bytes());
            v.push(0);
        }
        v
    }

    #[test]
    fn procargs2_yields_argv_not_the_exec_path() {
        // The resolved binary differs from argv[0]; argv[0] is what we want.
        let b = blob(
            3,
            "/usr/local/bin/cargo",
            7,
            &["cargo", "nextest", "run"],
            &["PATH=/usr/bin", "HOME=/Users/x"],
        );
        assert_eq!(
            parse_procargs2(&b),
            Some(vec![
                "cargo".to_string(),
                "nextest".to_string(),
                "run".to_string()
            ])
        );
    }

    #[test]
    fn procargs2_stops_at_argc_and_never_leaks_the_environment() {
        let b = blob(1, "/bin/sh", 1, &["sh"], &["SECRET=hunter2"]);
        let argv = parse_procargs2(&b).expect("argv");
        assert_eq!(argv, vec!["sh".to_string()]);
        assert!(!argv.iter().any(|a| a.contains("SECRET")));
    }

    #[test]
    fn procargs2_survives_no_padding_between_path_and_argv() {
        // Alignment padding is not guaranteed; zero NULs after the path is legal.
        let b = blob(2, "/bin/ls", 0, &["ls", "-la"], &[]);
        assert_eq!(
            parse_procargs2(&b),
            Some(vec!["ls".to_string(), "-la".to_string()])
        );
    }

    #[test]
    fn procargs2_rejects_garbage_instead_of_panicking() {
        // Too short for the argc header, argc == 0, and a header-only blob:
        // every one is `None`, and none may index out of bounds.
        assert_eq!(parse_procargs2(&[]), None);
        assert_eq!(parse_procargs2(&[1, 2]), None);
        assert_eq!(parse_procargs2(&0u32.to_ne_bytes()), None);
        assert_eq!(parse_procargs2(&9u32.to_ne_bytes()), None);
        // argc promises more than the blob delivers — take what is there.
        let b = blob(5, "/bin/sh", 2, &["sh", "-c"], &[]);
        assert_eq!(
            parse_procargs2(&b),
            Some(vec!["sh".to_string(), "-c".to_string()])
        );
    }

    #[test]
    fn cmdline_and_cwd_agree_with_this_very_process() {
        // Whatever the platform arm, it must answer correctly for self — the one
        // pid every platform is allowed to introspect.
        let me = std::process::id();
        let cwd = cwd_of(me);
        let argv = cmdline(me);
        if cfg!(any(target_os = "linux", target_os = "macos")) {
            assert_eq!(
                cwd.as_deref(),
                std::env::current_dir().ok().as_deref(),
                "cwd_of(self) must match std::env::current_dir"
            );
            let argv = argv.expect("cmdline(self) must resolve");
            assert!(!argv.is_empty());
        } else {
            assert!(cwd.is_none() && argv.is_none());
        }
    }
}
