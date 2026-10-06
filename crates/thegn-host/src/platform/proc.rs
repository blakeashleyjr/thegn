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

/// The direct child of `pid` that belongs to the terminal's FOREGROUND process
/// group, if any (a shell's foreground job).
///
/// The foreground group is asked of the terminal, not guessed from pid order:
/// `tpgid` (field 8 of `/proc/<pid>/stat`) is `tcgetpgrp` of `pid`'s
/// controlling tty. Background jobs, `&` helpers and daemons live in other
/// process groups and are never returned; an idle prompt (the shell itself is
/// the foreground group) has no such child and yields `None`. Fails closed:
/// no controlling tty or an unreadable stat is `None`, never a guess.
///
/// Cost: one stat read for `pid`, the O(children) children-file read, and one
/// stat read per child — the same order as the pid-order heuristic it replaced.
#[cfg(target_os = "linux")]
pub(crate) fn foreground_child(pid: u32) -> Option<u32> {
    let (_, tpgid) = stat_pgrps(pid)?;
    let kids = children_of(pid).unwrap_or_else(|| children_scan(pid));
    let with_pgrp: Vec<(u32, u32)> = kids
        .into_iter()
        .filter(|&c| c != pid)
        .filter_map(|c| Some((c, stat_pgrps(c)?.0)))
        .collect();
    select_foreground(tpgid, &with_pgrp)
}

/// The fallback `/proc/*/stat` walk: every direct child of `pid`. O(all
/// processes on the box); kept for kernels without CONFIG_PROC_CHILDREN.
#[cfg(target_os = "linux")]
fn children_scan(pid: u32) -> Vec<u32> {
    let mut out = Vec::new();
    let Ok(rd) = std::fs::read_dir("/proc") else {
        return out;
    };
    for ent in rd.flatten() {
        let Some(child) = ent.file_name().to_str().and_then(|s| s.parse::<u32>().ok()) else {
            continue;
        };
        if child != pid && stat_ppid(child) == Some(pid) {
            out.push(child);
        }
    }
    out
}

/// `(pgrp, tpgid)` from `/proc/<pid>/stat`.
#[cfg(target_os = "linux")]
fn stat_pgrps(pid: u32) -> Option<(u32, i64)> {
    parse_stat_pgrps(&std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?)
}

/// Parse `(pgrp, tpgid)` out of a `/proc/<pid>/stat` line. Fields after the
/// final `)` (comm may contain anything): state ppid pgrp session tty_nr tpgid.
#[cfg(any(target_os = "linux", test))]
fn parse_stat_pgrps(stat: &str) -> Option<(u32, i64)> {
    let rest = &stat[stat.rfind(')')? + 1..];
    let mut it = rest.split_whitespace();
    let pgrp = it.nth(2)?.parse().ok()?;
    let tpgid = it.nth(2)?.parse().ok()?;
    Some((pgrp, tpgid))
}

/// The foreground-selection rule, pure so every platform shares it. `kids` are
/// `(pid, pgrp)` of a process's direct children; `tpgid` is the terminal's
/// foreground group. Only children IN that group qualify. Within it the group
/// leader (`pid == tpgid`, a pipeline's first stage) wins, else the lowest pid
/// (oldest member) so the answer is deterministic. `tpgid <= 0` (no controlling
/// terminal) selects nothing.
#[cfg(any(target_os = "linux", target_os = "macos", test))]
fn select_foreground(tpgid: i64, kids: &[(u32, u32)]) -> Option<u32> {
    if tpgid <= 0 {
        return None;
    }
    let mut group = kids.iter().filter(|&&(_, g)| i64::from(g) == tpgid);
    let first = group.next()?;
    let mut best = first.0;
    for &(pid, _) in std::iter::once(first).chain(group) {
        if i64::from(pid) == tpgid {
            return Some(pid);
        }
        best = best.min(pid);
    }
    Some(best)
}

/// Direct children of `pid` from `/proc/<pid>/task/<tid>/children` — a
/// space-separated pid list maintained by the kernel (CONFIG_PROC_CHILDREN).
/// Children are recorded per *task* (thread): a child forked from a worker
/// thread lists under that thread's tid, not the main one's — so union every
/// task's file. O(threads + children), still far below the O(all processes)
/// stat walk. `None` when no task's file was readable (a kernel without the
/// option, or the pid died), so the caller can fall back. An EMPTY list from
/// readable files is a real answer (no children), not a miss.
#[cfg(target_os = "linux")]
fn children_of(pid: u32) -> Option<Vec<u32>> {
    let mut out = Vec::new();
    let mut any_read = false;
    for ent in std::fs::read_dir(format!("/proc/{pid}/task"))
        .ok()?
        .flatten()
    {
        let tid = ent.file_name();
        let Some(tid) = tid.to_str() else { continue };
        if let Ok(raw) = std::fs::read_to_string(format!("/proc/{pid}/task/{tid}/children")) {
            any_read = true;
            out.extend(raw.split_whitespace().filter_map(|s| s.parse::<u32>().ok()));
        }
    }
    any_read.then_some(out)
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

/// The direct child of `pid` in the terminal's foreground process group, via
/// `proc_pidinfo(PROC_PIDTBSDINFO)` (`e_tpgid` of `pid`, `pbi_pgid` per child)
/// and `proc_listchildpids`. Same selection rule as the Linux arm.
#[cfg(target_os = "macos")]
pub(crate) fn foreground_child(pid: u32) -> Option<u32> {
    let tpgid = i64::from(bsd_pgrps(pid)?.1);
    let with_pgrp: Vec<(u32, u32)> = child_pids(pid)?
        .into_iter()
        .filter_map(|c| Some((c, bsd_pgrps(c)?.0)))
        .collect();
    select_foreground(tpgid, &with_pgrp)
}

/// `(pgrp, tpgid)` for `pid` from `proc_bsdinfo`.
#[cfg(target_os = "macos")]
fn bsd_pgrps(pid: u32) -> Option<(u32, u32)> {
    // SAFETY: zeroed POD out-param; the kernel is told its exact size.
    let mut info: libc::proc_bsdinfo = unsafe { std::mem::zeroed() };
    let want = size_of::<libc::proc_bsdinfo>() as libc::c_int;
    let got = unsafe {
        libc::proc_pidinfo(
            pid as libc::c_int,
            libc::PROC_PIDTBSDINFO,
            0,
            std::ptr::from_mut(&mut info).cast(),
            want,
        )
    };
    (got == want).then_some((info.pbi_pgid, info.e_tpgid))
}

/// Direct children of `pid`, via `proc_listchildpids`.
#[cfg(target_os = "macos")]
fn child_pids(pid: u32) -> Option<Vec<u32>> {
    // Ask for the size first, then read. The count can change between the two
    // calls, so the buffer gets headroom and a short read is fine.
    // SAFETY: a null buffer with size 0 is the documented "how big?" query.
    let want = unsafe { libc::proc_listchildpids(pid as libc::pid_t, std::ptr::null_mut(), 0) };
    if want <= 0 {
        return None;
    }
    // The sizing query's units do not match the real call's (measured: it
    // answered 534 for a process with ONE child), so treat it purely as an
    // upper bound and add headroom rather than converting it.
    let cap = (want as usize).saturating_add(16);
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
    // `n` is a COUNT OF PIDS, not a byte length — unlike `proc_listpids`, whose
    // return you do divide by the element size. Dividing here silently broke the
    // common case: a shell with 1-3 children gave `n/4 == 0`, so every pane
    // reported "no foreground job" and the relaunch hint never captured
    // anything on macOS. Verified against the real syscall: 3 children ⇒ 3.
    let got = (n as usize).min(cap);
    Some(
        pids[..got]
            .iter()
            .filter(|&&c| c > 0 && c as u32 != pid)
            .map(|&c| c as u32)
            .collect(),
    )
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
/// resolved binary, whereas `argv[0]` is what the caller actually invoked, which
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
pub(crate) fn foreground_child(_pid: u32) -> Option<u32> {
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

    /// Kills **and reaps** a fixture child on every exit path, panics included.
    ///
    /// The tests below spawn real `sleep`s to probe. Killing them only on the
    /// success path leaks a process out of every failing run — the same
    /// test-hygiene bug that, with a CPU-burning fixture, left strays holding
    /// cores for hours. `Drop` runs while unwinding, so the child dies with the
    /// test either way; `drop(g)` where a test needs it gone mid-body.
    #[cfg(unix)]
    struct KillOnDrop(std::process::Child);

    #[cfg(unix)]
    impl Drop for KillOnDrop {
        // test code: reaping the fixture child, never on the event loop.
        #[expect(clippy::disallowed_methods)]
        fn drop(&mut self) {
            // best-effort: the child may already have exited or been reaped.
            let _ = self.0.kill();
            // best-effort: teardown: the child may already have exited or been reaped
            let _ = self.0.wait(); // reap, so no zombie outlives the test binary
        }
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
    #[cfg(target_os = "linux")]
    fn children_file_and_stat_walk_agree_on_a_live_child() {
        // Spawn a real child, then the O(1) children-file path and the
        // O(processes) stat-walk fallback must both find it — the fast path is
        // only correct if it never diverges from what the walk would say.
        let child = KillOnDrop(
            std::process::Command::new("sleep")
                .arg("30")
                .stdout(std::process::Stdio::null())
                .spawn()
                .expect("spawn sleep"),
        );
        let me = std::process::id();
        let kids = children_of(me).expect("/proc/<pid>/task/<pid>/children readable");
        assert!(
            kids.contains(&child.0.id()),
            "children file lists the spawned child"
        );
        let mut fast = kids;
        let mut walk = children_scan(me);
        fast.sort_unstable();
        walk.sort_unstable();
        // The walk may see children other tests spawned meanwhile; the fast
        // path must agree on ours.
        assert!(
            walk.contains(&child.0.id()),
            "stat-walk fallback lists the spawned child"
        );
        assert!(fast.contains(&child.0.id()));
    }

    /// The syscall seam answers for a CHILD, not just for self.
    ///
    /// This is the shape that matters: a pane's shell is a child of the
    /// compositor, so pane restore (its cwd) and the relaunch hint (its argv)
    /// both read another process. `cwd_of`/`cmdline` were only ever tested
    /// against self — the one pid every OS lets you introspect — and
    /// `foreground_child` was tested under `cfg(target_os = "linux")` only, leaving
    /// the macOS libproc arm with no coverage at all.
    ///
    /// One test for both arms, so `/proc` and libproc are held to the same
    /// contract rather than each being trusted separately.
    #[test]
    #[cfg(unix)]
    fn child_cwd_and_argv_are_readable_from_the_parent() {
        let dir = std::env::temp_dir().join(format!("tg-proc-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir); // best-effort: cleanup: the target may already be gone; a failed removal never fails the caller
        std::fs::create_dir_all(&dir).unwrap();
        // macOS hands back /private/var… for /var…; compare resolved paths.
        let dir = std::fs::canonicalize(&dir).unwrap();

        // `sleep` directly, NOT `sh -c "sleep 30"`: the shell exec-replaces
        // itself, so argv would legitimately read ["sleep", "30"] and the test
        // would be asserting the wrong thing about its own fixture.
        let child = KillOnDrop(
            std::process::Command::new("sleep")
                .arg("30")
                .current_dir(&dir)
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .spawn()
                .expect("spawn fixture child"),
        );
        // Give the child a moment to exec before introspecting it.
        std::thread::sleep(std::time::Duration::from_millis(200));
        let pid = child.0.id();

        let got_cwd = cwd_of(pid).and_then(|p| std::fs::canonicalize(p).ok());
        let got_argv = cmdline(pid);

        drop(child); // kills + reaps
        let _ = std::fs::remove_dir_all(&dir); // best-effort: cleanup: the target may already be gone; a failed removal never fails the caller

        assert_eq!(
            got_cwd.as_deref(),
            Some(dir.as_path()),
            "a pane's cwd is read from the CHILD — this is what pane restore \
             brings back after a relaunch"
        );
        let argv = got_argv.expect("cmdline(child) must resolve — it is the relaunch hint");
        assert!(
            argv.first().is_some_and(|a| a.contains("sleep")),
            "argv[0] should name the running program, got {argv:?}"
        );
    }

    /// Every syscall wrapper answers `None` for a pid that is definitively gone,
    /// rather than crashing, blocking, or handing back a half-filled buffer.
    ///
    /// This is the safety contract the whole module rests on ("best-effort,
    /// `None` on any failure"), and it is the case these wrappers hit constantly
    /// in production: a pane's child exits between the persist loop listing it
    /// and reading it. The macOS arms are raw `proc_pidinfo`/`proc_listchildpids`/
    /// `sysctl` calls into caller-owned buffers, so a mis-read return value shows
    /// up here as garbage rather than a clean miss.
    ///
    /// The pid is dead *deterministically* — spawned, killed and reaped by this
    /// test — rather than a "probably unused" number that could belong to a live
    /// process on a busy machine.
    #[test]
    #[cfg(unix)]
    // test code: reaping the fixture child, never on the event loop.
    #[expect(clippy::disallowed_methods)]
    fn dead_pids_yield_none_from_every_probe() {
        let mut child = std::process::Command::new("sleep")
            .arg("30")
            .stdout(std::process::Stdio::null())
            .spawn()
            .expect("spawn fixture child");
        let dead = child.id();
        let _ = child.kill(); // best-effort: teardown: the child may already have exited or been reaped
        // best-effort: teardown: the child may already have exited or been reaped
        let _ = child.wait(); // reaped: the pid is now truly gone, not a zombie

        assert_eq!(cwd_of(dead), None, "cwd_of(dead pid)");
        assert_eq!(cmdline(dead), None, "cmdline(dead pid)");
        assert_eq!(foreground_child(dead), None, "foreground_child(dead pid)");
    }

    /// A live process with no children reports no foreground job.
    ///
    /// The complement of the "finds the child" test, and the case the
    /// count-vs-bytes bug used to fake: it returned `None` for everything, so a
    /// test that only checked the empty case would have passed against a
    /// completely broken implementation.
    #[test]
    #[cfg(unix)]
    fn a_childless_process_has_no_foreground_child() {
        let child = KillOnDrop(
            std::process::Command::new("sleep")
                .arg("30")
                .stdout(std::process::Stdio::null())
                .spawn()
                .expect("spawn fixture child"),
        );
        std::thread::sleep(std::time::Duration::from_millis(150));
        let got = foreground_child(child.0.id());
        drop(child); // kills + reaps
        assert_eq!(got, None, "`sleep` spawns nothing, so it has no children");
    }

    /// The selection rule is terminal-semantic, never pid order: only a child in
    /// the foreground group qualifies, the group leader wins, else the oldest
    /// member; no controlling terminal selects nothing.
    #[test]
    fn select_foreground_uses_the_group_not_pid_order() {
        // fg group 100 (a pipeline: leader 100, member 105), newer bg job 200.
        let kids = [(100, 100), (105, 100), (200, 200), (300, 300)];
        assert_eq!(select_foreground(100, &kids), Some(100));
        // Leader gone: lowest member, not highest.
        assert_eq!(
            select_foreground(100, &[(107, 100), (105, 100), (300, 300)]),
            Some(105)
        );
        // A newer background job is never chosen over an older foreground one.
        assert_eq!(select_foreground(50, &[(50, 50), (900, 900)]), Some(50));
        // Idle prompt: the shell itself is foreground, no child is in the group.
        assert_eq!(select_foreground(10, &[(200, 200), (300, 300)]), None);
        // No controlling terminal / unknown.
        assert_eq!(select_foreground(-1, &[(200, 200)]), None);
        assert_eq!(select_foreground(0, &[(200, 0)]), None);
        assert_eq!(select_foreground(5, &[]), None);
    }

    #[test]
    fn stat_pgrps_parse_survives_hostile_comm() {
        let line = "42 ((a) b c) S 1 77 77 34816 77 4194304 0 0";
        assert_eq!(parse_stat_pgrps(line), Some((77, 77)));
        assert_eq!(parse_stat_pgrps("1 (x) S 1 5 5 0 -1 0"), Some((5, -1)));
        assert_eq!(parse_stat_pgrps("garbage"), None);
        assert_eq!(parse_stat_pgrps("1 (x) S 1 5"), None);
    }

    /// Real PTY: a bash with job control runs an OLD foreground job beside
    /// NEWER background jobs. The probe must return the foreground one — the
    /// highest-pid heuristic returns a background `sleep`.
    #[test]
    #[cfg(target_os = "linux")]
    #[expect(
        clippy::disallowed_methods,
        reason = "test-only probe for a bash binary; not on any runtime path"
    )]
    fn foreground_child_ignores_newer_background_jobs_on_a_real_pty() {
        use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
        use std::os::unix::process::CommandExt;
        if std::process::Command::new("bash")
            .arg("-c")
            .arg("true")
            .status()
            .is_err()
        {
            return; // no bash in this environment
        }
        let (mut m, mut sl) = (0, 0);
        // SAFETY: out-params are valid; null name/termios/winsize are allowed.
        let rc = unsafe {
            libc::openpty(
                &mut m,
                &mut sl,
                std::ptr::null_mut(),
                std::ptr::null(),
                std::ptr::null(),
            )
        };
        assert_eq!(rc, 0, "openpty");
        // SAFETY: fresh fds we own.
        let (master, slave) = unsafe { (OwnedFd::from_raw_fd(m), OwnedFd::from_raw_fd(sl)) };
        let mk = || slave.try_clone().expect("dup slave");
        let mut cmd = std::process::Command::new("bash");
        // fg job is STARTED first (lowest pid) then foregrounded; two newer
        // background jobs follow it.
        cmd.arg("-c")
            .arg("set -m; sleep 41 & sleep 42 & sleep 43 & fg %1")
            .stdin(mk())
            .stdout(mk())
            .stderr(mk());
        // SAFETY: only async-signal-safe libc calls between fork and exec.
        unsafe {
            cmd.pre_exec(|| {
                if libc::setsid() < 0 {
                    return Err(std::io::Error::last_os_error());
                }
                if libc::ioctl(0, libc::TIOCSCTTY, 0) < 0 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let sh = KillOnDrop(cmd.spawn().expect("spawn bash on a pty"));
        let sh_pid = sh.0.id();
        let _keep = (master.as_raw_fd(), &slave);

        // Wait (bounded) for all three jobs to exist.
        let mut got = None;
        for _ in 0..50 {
            std::thread::sleep(std::time::Duration::from_millis(100));
            if children_of(sh_pid).is_some_and(|k| k.len() >= 3) {
                got = foreground_child(sh_pid);
                if got.is_some() {
                    break;
                }
            }
        }
        let kids = children_of(sh_pid).unwrap_or_default();
        let oldest = kids.iter().copied().min();
        let newest = kids.iter().copied().max();
        drop(sh); // kills + reaps the shell
        let got = got.expect("a foreground child must be found");
        assert_eq!(Some(got), oldest, "the foreground job, not pid order");
        assert_ne!(Some(got), newest, "never the newest background job");
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
