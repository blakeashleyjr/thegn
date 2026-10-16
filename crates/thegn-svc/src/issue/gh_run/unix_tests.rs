//! Runner tests that drive real `sh` children (Unix process groups, signals).

use super::*;
use std::io::BufRead;
use std::os::unix::process::ExitStatusExt;

fn proc_run(
    end: thegn_proc::End,
    leader: thegn_proc::Leader,
    stdout: thegn_proc::Capture,
    stderr: thegn_proc::Capture,
    group: thegn_proc::GroupState,
) -> thegn_proc::Run {
    thegn_proc::Run {
        end,
        group,
        leader,
        leader_exited_first: true,
        stdout,
        stderr,
        containment: thegn_proc::Containment::ProcessGroup,
    }
}

fn capture(bytes: &[u8], eof: bool, truncated: bool) -> thegn_proc::Capture {
    thegn_proc::Capture {
        bytes: bytes.to_vec(),
        eof,
        truncated,
        ..Default::default()
    }
}

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

#[test]
fn map_run_covers_terminal_outcomes_and_precedence() {
    use thegn_proc::{End, GroupState, Leader, Which};

    assert!(matches!(
        map_run(proc_run(
            End::Spawn(std::io::Error::new(std::io::ErrorKind::NotFound, "missing")),
            Leader::NotSpawned,
            capture(b"", false, false),
            capture(b"", false, false),
            GroupState::Unknown,
        )),
        Err(RunError::NotInstalled)
    ));
    assert!(matches!(
        map_run(proc_run(
            End::Spawn(std::io::Error::other("spawn")),
            Leader::NotSpawned,
            capture(b"", false, false),
            capture(b"", false, false),
            GroupState::Unknown,
        )),
        Err(RunError::Io(message)) if message == "spawn"
    ));
    assert!(matches!(
        map_run(proc_run(
            End::Supervise(std::io::Error::other("supervise")),
            Leader::NotSpawned,
            capture(b"", false, false),
            capture(b"", false, false),
            GroupState::Unknown,
        )),
        Err(RunError::Io(message)) if message == "supervise"
    ));
    assert!(matches!(
        map_run(proc_run(
            End::Cancelled,
            Leader::NotSpawned,
            capture(b"", false, false),
            capture(b"", false, false),
            GroupState::Unknown,
        )),
        Err(RunError::Cancelled)
    ));
    assert!(matches!(
        map_run(proc_run(
            End::OutputLimit(Which::Stdout),
            Leader::NotSpawned,
            capture(b"", false, false),
            capture(b"", false, false),
            GroupState::Unknown,
        )),
        Err(RunError::Truncated)
    ));
    assert!(matches!(
        map_run(proc_run(
            End::TimedOut,
            Leader::NotSpawned,
            capture(b"", false, false),
            capture(b"", false, false),
            GroupState::Unknown,
        )),
        Err(RunError::Timeout)
    ));
    assert!(matches!(
        map_run(proc_run(
            End::LeaderExited,
            Leader::ReapFailed(std::io::Error::other("reap")),
            capture(b"", false, false),
            capture(b"", false, false),
            GroupState::Unknown,
        )),
        Err(RunError::Io(message)) if message == "reap"
    ));
    for leader in [Leader::Detached, Leader::NotSpawned] {
        assert!(matches!(
            map_run(proc_run(
                End::LeaderExited,
                leader,
                capture(b"", true, false),
                capture(b"", true, false),
                GroupState::Unknown,
            )),
            Err(RunError::Io(message)) if message == "could not reap child"
        ));
    }
    assert!(matches!(
        map_run(proc_run(
            End::LeaderExited,
            Leader::Reaped(std::process::ExitStatus::from_raw(0)),
            capture(b"ok", false, false),
            capture(b"", true, false),
            GroupState::Drained,
        )),
        Err(RunError::Timeout)
    ));
    assert!(matches!(
        map_run(proc_run(
            End::LeaderExited,
            Leader::Reaped(std::process::ExitStatus::from_raw(0)),
            capture(b"ok", true, false),
            capture(b"", false, false),
            GroupState::Drained,
        )),
        Err(RunError::Timeout)
    ));
    assert!(matches!(
        map_run(proc_run(
            End::LeaderExited,
            Leader::Reaped(std::process::ExitStatus::from_raw(0)),
            capture(b"over", true, true),
            capture(b"", true, false),
            GroupState::Drained,
        )),
        Err(RunError::Truncated)
    ));
    assert!(matches!(
        map_run(proc_run(
            End::LeaderExited,
            Leader::Reaped(std::process::ExitStatus::from_raw(0)),
            capture(b"ok", true, false),
            capture(b"", true, false),
            GroupState::Drained,
        )),
        Ok(output) if output == "ok"
    ));
    assert!(matches!(
        map_run(proc_run(
            End::LeaderExited,
            Leader::Reaped(std::process::ExitStatus::from_raw(3 << 8)),
            capture(b"", true, false),
            capture(b"failure", true, false),
            GroupState::Drained,
        )),
        Err(RunError::Exit { code: Some(3), stderr }) if stderr == "failure"
    ));
}

#[test]
fn nonzero_exit_keeps_the_newest_stderr() {
    let mut limits = lim(5_000);
    limits.max_stderr = 64;
    let result = run_blocking(
        sh("yes e | head -c 5000 >&2; printf LASTLINE >&2; exit 3"),
        limits,
        &AtomicBool::new(false),
    );
    match result {
        Err(RunError::Exit { code, stderr }) => {
            assert_eq!(code, Some(3));
            assert!(stderr.len() <= 64);
            assert!(stderr.ends_with("LASTLINE"), "{stderr:?}");
        }
        other => panic!("{other:?}"),
    }
}

#[test]
fn cancel_before_spawn_spawns_nothing() {
    let directory = tempfile::tempdir().unwrap();
    let marker = directory.path().join("spawned");
    let script = format!("touch {}; printf unexpected", marker.display());
    let cancel = AtomicBool::new(true);
    assert!(matches!(
        run_blocking(sh(&script), lim(5_000), &cancel),
        Err(RunError::Cancelled)
    ));
    assert!(!marker.exists());
}

/// True when `pid` is gone or a zombie awaiting a subreaper.
#[expect(
    clippy::disallowed_methods,
    reason = "ps is only used to verify test child cleanup"
)]
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
#[expect(
    clippy::disallowed_methods,
    reason = "setsid availability is probed in this Unix-only test"
)]
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

#[test]
fn reap_error_does_not_replace_an_existing_run_failure() {
    let mut failure = Some(RunError::Timeout);
    set_io_failure_if_missing(&mut failure, std::io::Error::other("wait failed"));
    assert!(matches!(failure, Some(RunError::Timeout)));

    let mut failure = None;
    set_io_failure_if_missing(&mut failure, std::io::Error::other("wait failed"));
    assert!(matches!(failure, Some(RunError::Io(message)) if message == "wait failed"));
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

#[test]
fn streaming_stdout_uses_consumer_retention_past_stdout_limit() {
    let cancel = AtomicBool::new(false);
    let (last, status) = run_blocking_streaming(
        sh("yes x | head -c 3000000; echo tail"),
        lim(10_000),
        &cancel,
        |reader| {
            let mut input = std::io::BufReader::new(reader);
            let mut line = Vec::new();
            let mut last = String::new();
            loop {
                line.clear();
                match input.read_until(b'\n', &mut line) {
                    Ok(0) | Err(_) => break,
                    Ok(_) => last = String::from_utf8_lossy(&line).into_owned(),
                }
            }
            last
        },
    )
    .unwrap();
    assert!(status.success());
    assert_eq!(last, "tail\n");
}

#[test]
fn streaming_hung_child_times_out() {
    let cancel = AtomicBool::new(false);
    let start = Instant::now();
    let result = run_blocking_streaming(sh("sleep 30"), lim(300), &cancel, |reader| {
        std::io::read_to_string(reader).unwrap_or_default()
    });
    assert!(matches!(result, Err(RunError::Timeout)), "{result:?}");
    assert!(start.elapsed() < Duration::from_secs(5));
}

#[test]
fn streaming_kills_background_pipe_holder_on_timeout() {
    let directory = tempfile::tempdir().unwrap();
    let pidfile = directory.path().join("stream-bg.pid");
    let script = format!("sleep 30 & echo $! > {}; sleep 30", pidfile.display());
    let cancel = AtomicBool::new(false);
    let start = Instant::now();
    let result = run_blocking_streaming(sh(&script), lim(300), &cancel, |reader| {
        std::io::read_to_string(reader).unwrap_or_default()
    });
    assert!(matches!(result, Err(RunError::Timeout)), "{result:?}");
    assert!(start.elapsed() < Duration::from_secs(5));
    let pid = std::fs::read_to_string(pidfile).unwrap();
    assert!(wait_dead(pid.trim()), "background sleep {pid} survived");
}

#[test]
fn streaming_consumer_can_stop_early_without_sigpipe() {
    let cancel = AtomicBool::new(false);
    let (first, status) = run_blocking_streaming(
        sh("head -c 500000 /dev/zero"),
        lim(10_000),
        &cancel,
        |reader| {
            let mut first = [0; 10];
            reader.read_exact(&mut first).unwrap();
            first
        },
    )
    .unwrap();
    assert!(status.success());
    assert_eq!(first, [0; 10]);
}

#[test]
fn streaming_nonzero_exit_carries_capped_stderr() {
    let cancel = AtomicBool::new(false);
    let result = run_blocking_streaming(
        sh("yes e | head -c 5000 >&2; exit 7"),
        lim(5000),
        &cancel,
        |reader| std::io::read_to_string(reader).unwrap_or_default(),
    );
    match result {
        Err(RunError::Exit { code, stderr }) => {
            assert_eq!(code, Some(7));
            assert_eq!(stderr.len(), 64);
            assert!(stderr.starts_with('e'));
        }
        other => panic!("{other:?}"),
    }
}

#[test]
fn streaming_cancel_flag_kills_run() {
    let cancel = Arc::new(AtomicBool::new(false));
    let cancel_signal = cancel.clone();
    std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(100));
        cancel_signal.store(true, Ordering::Relaxed);
    });
    let start = Instant::now();
    let result = run_blocking_streaming(sh("sleep 30"), lim(20_000), &cancel, |reader| {
        std::io::read_to_string(reader).unwrap_or_default()
    });
    assert!(matches!(result, Err(RunError::Cancelled)), "{result:?}");
    assert!(start.elapsed() < Duration::from_secs(5));
}
