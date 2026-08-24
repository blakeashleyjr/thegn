//! Steady-state idle CPU for thegn on **Windows**, with wake attribution.
//!
//! `test/perf/cpu-sample.sh` is the Linux harness and says so in its first
//! lines: it reads `/proc/PID/task/*/stat`, and skips on anything else. So the
//! one number `KNOWN_ISSUES.md` cites as a reason Windows is unsupported — idle
//! CPU ~0.09 cores against Linux's ~0.056 on the same fixture — had no repeatable
//! way to be measured, let alone attributed.
//!
//! This is that harness. It launches the release binary in a real ConPTY with a
//! fully isolated environment and an N-worktree fixture repo, lets it settle,
//! samples the process's kernel+user time over a fixed window via
//! `GetProcessTimes`, and then reads the in-process `thegn::perf` rollup back out
//! of the log so the cores figure comes with a *cause* rather than just a value.
//!
//! # Run it
//!
//! ```text
//! cargo build --release -p thegn-host
//! cargo run -p thegn-host --example idle_cpu_windows
//! ```
//!
//! Advisory, never a gate: CPU sampling is machine-dependent, which is exactly
//! why the Linux one is excluded from `just ci` too. Everything it creates lives
//! under one temp directory and is removed on the way out.

#![allow(clippy::disallowed_macros, clippy::disallowed_methods)]

fn main() {
    #[cfg(not(windows))]
    {
        eprintln!(
            "idle_cpu_windows: Windows-only — use `just bench-idle` \
             (test/perf/cpu-sample.sh) elsewhere"
        );
    }
    #[cfg(windows)]
    win::run();
}

#[cfg(windows)]
mod win {
    use std::io::{Read, Write};
    use std::path::{Path, PathBuf};
    use std::sync::{Arc, Mutex};
    use std::time::{Duration, Instant};

    use portable_pty::{CommandBuilder, PtySize};
    use windows_sys::Win32::Foundation::{CloseHandle, FILETIME};
    use windows_sys::Win32::System::Threading::{
        GetProcessTimes, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
    };

    /// Worktrees in the fixture. `THEGN_IDLE_WORKTREES` overrides it, which is how
    /// you tell a per-worktree cost (hydration) from a fixed background poller:
    /// run at 1 and at 14 and see whether the number moves.
    fn worktrees() -> usize {
        std::env::var("THEGN_IDLE_WORKTREES")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(14)
    }
    /// Settle before sampling. `THEGN_IDLE_SETTLE_MS` overrides it — the default
    /// matches the Linux harness, but a freshly-started instance is still
    /// catching up, so a longer settle is how you tell startup cost from steady
    /// state before publishing either as a number.
    fn settle() -> Duration {
        Duration::from_millis(
            std::env::var("THEGN_IDLE_SETTLE_MS")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(2500),
        )
    }
    const WINDOW: Duration = Duration::from_millis(8000);

    pub fn run() {
        let root = std::env::temp_dir().join("thegn-idle-cpu");
        let _ = std::fs::remove_dir_all(&root);
        let repo = match fixture(&root) {
            Ok(r) => r,
            Err(e) => {
                eprintln!("fixture failed: {e}");
                return;
            }
        };
        let bin = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../target/release/thegn.exe")
            .canonicalize()
            .unwrap_or_else(|_| PathBuf::from("target/release/thegn.exe"));
        if !bin.exists() {
            eprintln!("build it first: cargo build --release -p thegn-host");
            return;
        }

        println!("== thegn idle CPU (Windows) ==");
        println!("  binary    {}", bin.display());
        println!(
            "  fixture   {} worktrees at {}",
            worktrees(),
            repo.display()
        );
        println!("  settle    {:?}, window {:?}\n", settle(), WINDOW);

        match measure(&bin, &root, &repo) {
            Ok(m) => report(&m, &root),
            Err(e) => eprintln!("measure failed: {e}"),
        }
        // `THEGN_IDLE_KEEP=1` leaves the fixture and log behind: without it a
        // surprising number cannot be dug into after the fact.
        if std::env::var("THEGN_IDLE_KEEP").ok().as_deref() == Some("1") {
            println!("\n  kept: {}", root.display());
        } else {
            let _ = std::fs::remove_dir_all(&root);
        }
    }

    struct Measurement {
        cores: f64,
        elapsed: Duration,
        /// Per-thread cores over the window, busiest first.
        threads: Vec<(String, f64)>,
    }

    /// A repo with `worktrees()` linked worktrees — the same shape the Linux
    /// harness builds, so the two numbers are comparable.
    fn fixture(root: &Path) -> std::io::Result<PathBuf> {
        let repo = root.join("repo");
        std::fs::create_dir_all(&repo)?;
        let git = |args: &[&str]| {
            std::process::Command::new("git")
                .arg("-C")
                .arg(&repo)
                .args(args)
                .output()
        };
        git(&["init", "-q", "-b", "main", "."])?;
        git(&["config", "user.email", "perf@example.com"])?;
        git(&["config", "user.name", "perf"])?;
        std::fs::write(repo.join("README.md"), "perf fixture\n")?;
        git(&["add", "-A"])?;
        git(&["commit", "-qm", "base"])?;
        for i in 0..worktrees() {
            let wt = root.join(format!("wt{i}"));
            git(&[
                "worktree",
                "add",
                "-q",
                &wt.to_string_lossy(),
                "-b",
                &format!("feat-{i}"),
            ])?;
        }
        Ok(repo)
    }

    /// Kernel+user time of `pid`, as a Duration.
    fn cpu_time(pid: u32) -> Option<Duration> {
        // SAFETY: a query-only handle; closed below.
        let h = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
        if h.is_null() {
            return None;
        }
        let mut c: FILETIME = unsafe { std::mem::zeroed() };
        let mut e: FILETIME = unsafe { std::mem::zeroed() };
        let mut k: FILETIME = unsafe { std::mem::zeroed() };
        let mut u: FILETIME = unsafe { std::mem::zeroed() };
        // SAFETY: `h` is a live process handle; the four out-params are locals.
        let ok = unsafe { GetProcessTimes(h, &mut c, &mut e, &mut k, &mut u) };
        // SAFETY: `h` came from a successful OpenProcess.
        unsafe { CloseHandle(h) };
        if ok == 0 {
            return None;
        }
        let as_ns = |f: FILETIME| {
            // FILETIME is 100-nanosecond units split across two 32-bit halves.
            (((f.dwHighDateTime as u64) << 32) | f.dwLowDateTime as u64) * 100
        };
        Some(Duration::from_nanos(as_ns(k) + as_ns(u)))
    }

    fn measure(bin: &Path, root: &Path, repo: &Path) -> anyhow::Result<Measurement> {
        let pair = portable_pty::native_pty_system().openpty(PtySize {
            rows: 50,
            cols: 200,
            pixel_width: 0,
            pixel_height: 0,
        })?;

        let mut cmd = CommandBuilder::new(bin);
        cmd.cwd(repo);
        // The in-process profiler: free when off, and the only thing that can say
        // WHY the loop woke.
        cmd.env("THEGN_PERF", "1");
        // The default rollup cadence is 10s, which a short window can miss
        // entirely — and a missing rollup reads as "no attribution" rather than
        // as a harness that simply did not run long enough.
        cmd.env("THEGN_PERF_INTERVAL_MS", "2000");
        cmd.env("THEGN_LOG", "info");
        cmd.env("TERM", "xterm-256color");
        // A modern-terminal marker, or the compositor refuses conhost and exits.
        cmd.env("WT_SESSION", "idle-cpu-harness");
        // Full isolation: both names must move, because `home()` reads USERPROFILE
        // on Windows while the XDG vars are honoured when set explicitly.
        for (k, v) in [
            ("HOME", "home"),
            ("USERPROFILE", "home"),
            ("XDG_CONFIG_HOME", "config"),
            ("XDG_STATE_HOME", "state"),
            ("XDG_RUNTIME_DIR", "state"),
        ] {
            let p = root.join(v);
            std::fs::create_dir_all(&p)?;
            cmd.env(k, p);
        }

        let mut child = pair.slave.spawn_command(cmd)?;
        drop(pair.slave);
        let pid = child
            .process_id()
            .ok_or_else(|| anyhow::anyhow!("no pid"))?;

        let out = Arc::new(Mutex::new(Vec::<u8>::new()));
        let mut reader = pair.master.try_clone_reader()?;
        let mut writer = pair.master.take_writer()?;
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

        // ConPTY stalls the child until its startup DSR is answered, and thegn's
        // own probe asks for one too. An unanswered query looks exactly like a
        // hang — and would read here as an implausibly low CPU figure.
        let deadline = Instant::now() + settle();
        let mut answered = false;
        while Instant::now() < deadline {
            if !answered
                && let Ok(b) = out.lock()
                && b.windows(4).any(|w| w == b"\x1b[6n")
            {
                let _ = writer.write_all(b"\x1b[1;1R");
                let _ = writer.flush();
                answered = true;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        if !answered {
            eprintln!("  note: no DSR seen during settle — the reading may be a stalled process");
        }

        let t0 = cpu_time(pid).ok_or_else(|| anyhow::anyhow!("GetProcessTimes failed"))?;
        let th0 = thread_times(pid);
        let w0 = Instant::now();
        std::thread::sleep(WINDOW);
        let t1 = cpu_time(pid).ok_or_else(|| anyhow::anyhow!("process gone mid-window"))?;
        let th1 = thread_times(pid);
        let elapsed = w0.elapsed();

        let _ = child.kill();
        let _ = child.wait();

        let mut threads: Vec<(String, f64)> = th1
            .iter()
            .map(|(tid, name, t)| {
                let before = th0
                    .iter()
                    .find(|(i, _, _)| i == tid)
                    .map(|(_, _, d)| *d)
                    .unwrap_or_default();
                (
                    format!("{name} (tid {tid})"),
                    t.saturating_sub(before).as_secs_f64() / elapsed.as_secs_f64(),
                )
            })
            .filter(|(_, c)| *c > 0.0005)
            .collect();
        threads.sort_by(|a, b| b.1.total_cmp(&a.1));

        Ok(Measurement {
            cores: (t1.saturating_sub(t0)).as_secs_f64() / elapsed.as_secs_f64(),
            elapsed,
            threads,
        })
    }

    fn report(m: &Measurement, root: &Path) {
        println!("  idle CPU  {:.4} cores over {:?}", m.cores, m.elapsed);
        // Linux reads ~0.056 on the same 14-worktree fixture; the shipped guard
        // in cpu-sample.sh is 0.12. Both are stated so the number has a scale.
        println!("            (linux ~0.056 on this fixture; harness ceiling 0.12)");

        if !m.threads.is_empty() {
            println!("\n  per-thread cores (busiest first):");
            let named: f64 = m.threads.iter().map(|(_, c)| c).sum();
            for (name, cores) in m.threads.iter().take(12) {
                println!("    {cores:>7.4}  {name}");
            }
            println!("    {named:>7.4}  = accounted for");
        }

        let log = root.join("state/thegn/logs/thegn.log");
        let Ok(text) = std::fs::read_to_string(&log) else {
            println!("\n  no log at {} — cannot attribute", log.display());
            return;
        };
        let rollups: Vec<&str> = text
            .lines()
            .filter(|l| l.contains("thegn::perf"))
            .rev()
            .take(2)
            .collect();
        if rollups.is_empty() {
            println!("\n  no thegn::perf rollup in the log (THEGN_PERF unset?)");
            return;
        }
        println!("\n  wake attribution (last rollups):");
        for r in rollups.iter().rev() {
            for field in [
                "wakes_per_s",
                "renders_per_s",
                "render_skips_per_s",
                "idle_ratio",
                "render_busy_ratio",
                "hot_source",
                "hot_items_per_s",
            ] {
                if let Some(v) = field_of(r, field) {
                    print!("{field}={v}  ");
                }
            }
            println!();
        }
    }

    /// Pull `key=value` out of a tracing line without pulling in a parser.
    ///
    /// The file sink writes ANSI attributes around every field name
    /// (`ESC[3m name ESC[0m ESC[2m = ESC[0m value`), so a naive `find("key=")`
    /// matches nothing at all — which reads as "no attribution available" rather
    /// than as a parsing bug.
    fn field_of(line: &str, key: &str) -> Option<String> {
        let plain = strip_ansi(line);
        let at = plain.find(&format!("{key}="))? + key.len() + 1;
        let rest = &plain[at..];
        let end = rest.find(' ').unwrap_or(rest.len());
        Some(rest[..end].trim_matches('"').to_string())
    }

    fn strip_ansi(s: &str) -> String {
        let b: Vec<char> = s.chars().collect();
        let mut out = String::with_capacity(s.len());
        let mut i = 0;
        while i < b.len() {
            if b[i] == '\u{1b}' && b.get(i + 1) == Some(&'[') {
                i += 2;
                while i < b.len() && !matches!(b[i], '@'..='~') {
                    i += 1;
                }
                i += 1;
                continue;
            }
            out.push(b[i]);
            i += 1;
        }
        out
    }

    /// Per-thread kernel+user time for `pid`, keyed by `(tid, name)`.
    ///
    /// This is the half the cores figure is useless without. The loop reports
    /// itself 98%+ idle while the process burns a fifth of a core, which means
    /// the cost is on some OTHER thread — and only a per-thread breakdown can say
    /// which. It is precisely what `cpu-sample.sh` reads out of
    /// `/proc/PID/task/*/stat`, and the reason that harness is Linux-only.
    fn thread_times(pid: u32) -> Vec<(u32, String, Duration)> {
        use windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE;
        use windows_sys::Win32::System::Diagnostics::ToolHelp::{
            CreateToolhelp32Snapshot, TH32CS_SNAPTHREAD, THREADENTRY32, Thread32First, Thread32Next,
        };
        use windows_sys::Win32::System::Threading::{
            GetThreadDescription, GetThreadTimes, OpenThread, THREAD_QUERY_LIMITED_INFORMATION,
        };

        let mut out = Vec::new();
        // SAFETY: a thread snapshot handle, closed below.
        let snap = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPTHREAD, 0) };
        if snap == INVALID_HANDLE_VALUE {
            return out;
        }
        let mut te: THREADENTRY32 = unsafe { std::mem::zeroed() };
        te.dwSize = std::mem::size_of::<THREADENTRY32>() as u32;
        // SAFETY: `te` is sized as the API requires.
        let mut ok = unsafe { Thread32First(snap, &mut te) };
        while ok != 0 {
            if te.th32OwnerProcessID == pid {
                // SAFETY: query-only handle for a tid from the snapshot.
                let h = unsafe { OpenThread(THREAD_QUERY_LIMITED_INFORMATION, 0, te.th32ThreadID) };
                if !h.is_null() {
                    let mut c: FILETIME = unsafe { std::mem::zeroed() };
                    let mut e: FILETIME = unsafe { std::mem::zeroed() };
                    let mut k: FILETIME = unsafe { std::mem::zeroed() };
                    let mut u: FILETIME = unsafe { std::mem::zeroed() };
                    // SAFETY: `h` is live; out-params are locals.
                    if unsafe { GetThreadTimes(h, &mut c, &mut e, &mut k, &mut u) } != 0 {
                        let ns = |f: FILETIME| {
                            (((f.dwHighDateTime as u64) << 32) | f.dwLowDateTime as u64) * 100
                        };
                        // Rust's std sets a thread's OS description from
                        // `Builder::name`, so most of these come back readable.
                        let mut desc: *mut u16 = std::ptr::null_mut();
                        let name = {
                            // SAFETY: `desc` receives a LocalAlloc'd wide string.
                            let hr = unsafe { GetThreadDescription(h, &mut desc) };
                            if hr >= 0 && !desc.is_null() {
                                let mut len = 0usize;
                                // SAFETY: NUL-terminated.
                                while unsafe { *desc.add(len) } != 0 {
                                    len += 1;
                                }
                                // SAFETY: `len` just measured.
                                let s = String::from_utf16_lossy(unsafe {
                                    std::slice::from_raw_parts(desc, len)
                                });
                                // SAFETY: from LocalAlloc inside the call.
                                unsafe {
                                    windows_sys::Win32::Foundation::LocalFree(desc as *mut _)
                                };
                                s
                            } else {
                                String::new()
                            }
                        };
                        let name = if name.is_empty() {
                            "(unnamed)".to_string()
                        } else {
                            name
                        };
                        out.push((te.th32ThreadID, name, Duration::from_nanos(ns(k) + ns(u))));
                    }
                    // SAFETY: `h` came from a successful OpenThread.
                    unsafe { CloseHandle(h) };
                }
            }
            te.dwSize = std::mem::size_of::<THREADENTRY32>() as u32;
            // SAFETY: same contract as Thread32First.
            ok = unsafe { Thread32Next(snap, &mut te) };
        }
        // SAFETY: from CreateToolhelp32Snapshot.
        unsafe { CloseHandle(snap) };
        out
    }
}
