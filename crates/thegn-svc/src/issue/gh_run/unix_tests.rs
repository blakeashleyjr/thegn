//! Runner tests that drive real `sh` children (Unix process groups, signals).

use super::*;

fn sh(script: &str) -> Command {
    let mut c = Command::new("sh");
    c.args(["-c", script]);
    c
}

fn lim(ms: u64) -> Limits {
    Limits {
        timeout: Duration::from_millis(ms),
        max_stdout: 1024,
        max_stderr: 64,
    }
}

/// True when `pid` is gone or a zombie awaiting a subreaper.
fn dead(pid: &str) -> bool {
    let out = Command::new("ps")
        .args(["-o", "stat=", "-p", pid])
        .output()
        .unwrap();
    let s = String::from_utf8_lossy(&out.stdout);
    let s = s.trim();
    s.is_empty() || s.starts_with('Z')
}

fn wait_dead(pid: &str) -> bool {
    for _ in 0..100 {
        if dead(pid) {
            return true;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    false
}

#[tokio::test]
async fn ok_captures_stdout() {
    assert_eq!(run(sh("printf hi"), lim(5000)).await.unwrap(), "hi");
}

#[tokio::test]
async fn hung_process_times_out_and_group_dies() {
    let d = tempfile::tempdir().unwrap();
    let pidfile = d.path().join("bg.pid");
    let script = format!("sleep 30 & echo $! > {}; sleep 30", pidfile.display());
    let t = Instant::now();
    let r = run(sh(&script), lim(300)).await;
    assert!(matches!(r, Err(RunError::Timeout)), "{r:?}");
    assert!(t.elapsed() < Duration::from_secs(5));
    let pid = std::fs::read_to_string(&pidfile).unwrap();
    assert!(wait_dead(pid.trim()), "background sleep {pid} survived");
}

#[tokio::test]
async fn oversized_stdout_is_truncated_error() {
    let r = run(sh("yes x | head -c 100000; sleep 30"), lim(10_000)).await;
    assert!(matches!(r, Err(RunError::Truncated)), "{r:?}");
}

#[tokio::test]
async fn nonzero_exit_carries_capped_stderr() {
    let r = run(sh("yes e | head -c 5000 >&2; exit 3"), lim(5000)).await;
    match r {
        Err(RunError::Exit { code, stderr }) => {
            assert_eq!(code, Some(3));
            assert!(stderr.len() <= 64);
        }
        other => panic!("{other:?}"),
    }
}

#[tokio::test]
async fn stderr_flood_past_cap_does_not_kill_the_child() {
    // The child keeps writing long after the 64-byte cap and must still
    // finish cleanly (no EPIPE/SIGPIPE from a closed read end).
    let r = run(sh("head -c 500000 /dev/zero >&2; echo done"), lim(10_000)).await;
    assert_eq!(r.unwrap(), "done\n");
}

#[tokio::test]
async fn escaped_descendant_holding_the_pipe_cannot_pin_the_runner() {
    if Command::new("setsid").arg("--version").output().is_err() {
        return; // no setsid here (e.g. macOS)
    }
    let d = tempfile::tempdir().unwrap();
    let pidfile = d.path().join("esc.pid");
    // The pause lets setsid leave the group before the leader exits.
    let script = format!(
        "setsid sleep 30 & echo $! > {}; sleep 0.3; echo hi",
        pidfile.display()
    );
    let t = Instant::now();
    let r = run(sh(&script), lim(10_000)).await;
    let pid = std::fs::read_to_string(&pidfile).unwrap_or_default();
    // Clean up the escapee before asserting.
    if let Ok(p) = pid.trim().parse::<i32>() {
        // SAFETY: signalling the synthetic sleep this test started.
        unsafe {
            libc::kill(p, libc::SIGKILL);
        }
    }
    assert!(matches!(r, Err(RunError::Timeout)), "{r:?}");
    assert!(t.elapsed() < Duration::from_secs(5), "{:?}", t.elapsed());
}

#[tokio::test]
async fn missing_program_is_not_installed() {
    let r = run(Command::new("definitely-not-a-real-gh-binary"), lim(1000)).await;
    assert!(matches!(r, Err(RunError::NotInstalled)), "{r:?}");
}

#[test]
fn cancel_flag_kills_run() {
    let cancel = Arc::new(AtomicBool::new(false));
    let c2 = cancel.clone();
    std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(100));
        c2.store(true, Ordering::Relaxed);
    });
    let t = Instant::now();
    let r = run_blocking(sh("sleep 30"), lim(20_000), &cancel);
    assert!(matches!(r, Err(RunError::Cancelled)), "{r:?}");
    assert!(t.elapsed() < Duration::from_secs(5));
}

#[tokio::test]
async fn concurrency_is_bounded() {
    let dir = tempfile::tempdir().unwrap();
    let log = dir.path().join("log");
    let script = format!("echo s >> {p}; sleep 0.3; echo e >> {p}", p = log.display());
    let mut hs = vec![];
    for _ in 0..(MAX_CONCURRENT * 2) {
        hs.push(tokio::spawn(run(sh(&script), lim(10_000))));
    }
    for h in hs {
        h.await.unwrap().unwrap();
    }
    let (mut live, mut peak) = (0i32, 0i32);
    for l in std::fs::read_to_string(&log).unwrap().lines() {
        live += if l == "s" { 1 } else { -1 };
        peak = peak.max(live);
    }
    assert!(peak <= MAX_CONCURRENT as i32, "peak {peak}");
}
