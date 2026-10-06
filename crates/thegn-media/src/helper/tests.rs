//! Fake-helper lifecycle tests. They shell out to `sh`/`sleep`/`yes`, so they
//! are a no-op where those don't exist (Windows).

use std::path::Path;
use std::time::{Duration, Instant};

use tokio::process::Command;

use super::{HelperError, Limits, output};

fn sh(script: &str) -> Command {
    let mut c = Command::new("sh");
    c.arg("-c").arg(script);
    c
}

fn have_sh() -> bool {
    Path::new("/bin/sh").exists() && Path::new("/proc/self/stat").exists()
}

fn lim(ms: u64, cap: usize) -> Limits {
    Limits {
        deadline: Duration::from_millis(ms),
        max_bytes: cap,
    }
}

/// True once `pid` no longer exists or is only a zombie awaiting init's reap.
fn gone(pid: u32) -> bool {
    match std::fs::read_to_string(format!("/proc/{pid}/stat")) {
        Err(_) => true,
        Ok(s) => s
            .rsplit(')')
            .next()
            .is_some_and(|t| t.trim_start().starts_with('Z')),
    }
}

async fn wait_gone(file: &Path) -> bool {
    let pid: u32 = loop {
        if let Ok(s) = std::fs::read_to_string(file) {
            if let Ok(p) = s.trim().parse() {
                break p;
            }
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    };
    for _ in 0..200 {
        if gone(pid) {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    false
}

#[tokio::test]
async fn completes_and_captures() {
    if !have_sh() {
        return;
    }
    let c = output(sh("printf hi; printf err >&2"), Limits::OP)
        .await
        .unwrap();
    assert!(c.status.success());
    assert_eq!(c.stdout, b"hi");
    assert_eq!(c.stderr, b"err");
}

#[tokio::test]
async fn missing_binary_is_a_spawn_error() {
    let r = output(Command::new("thegn-no-such-helper-binary"), Limits::OP).await;
    assert!(matches!(r, Err(HelperError::Spawn(_))), "{r:?}");
}

#[tokio::test]
async fn never_exit_times_out_and_is_reaped() {
    if !have_sh() {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let f = dir.path().join("pid");
    let t = Instant::now();
    let r = output(
        sh(&format!("echo $$ > {}; exec sleep 60", f.display())),
        lim(300, 1024),
    )
    .await;
    assert!(matches!(r, Err(HelperError::Timeout(_))), "{r:?}");
    assert!(t.elapsed() < Duration::from_secs(5));
    assert!(wait_gone(&f).await, "helper survived its deadline");
}

#[tokio::test]
async fn infinite_stdout_is_capped() {
    if !have_sh() {
        return;
    }
    let r = output(sh("yes"), lim(10_000, 64 * 1024)).await;
    assert!(
        matches!(
            r,
            Err(HelperError::Oversize {
                stream: "stdout",
                ..
            })
        ),
        "{r:?}"
    );
}

#[tokio::test]
async fn infinite_stderr_is_capped() {
    if !have_sh() {
        return;
    }
    let r = output(sh("yes >&2"), lim(10_000, 64 * 1024)).await;
    assert!(
        matches!(
            r,
            Err(HelperError::Oversize {
                stream: "stderr",
                ..
            })
        ),
        "{r:?}"
    );
}

#[tokio::test]
async fn grandchild_dies_with_the_tree() {
    if !have_sh() {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let f = dir.path().join("gpid");
    let script = format!("sleep 60 & echo $! > {}; wait", f.display());
    let r = output(sh(&script), lim(300, 1024)).await;
    assert!(matches!(r, Err(HelperError::Timeout(_))), "{r:?}");
    assert!(wait_gone(&f).await, "grandchild survived tree kill");
}

#[tokio::test]
async fn cancelling_the_future_kills_the_tree() {
    if !have_sh() {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    // Repeated "reload": each iteration aborts a pending run mid-flight.
    for i in 0..4 {
        let f = dir.path().join(format!("pid{i}"));
        let g = dir.path().join(format!("gpid{i}"));
        let script = format!(
            "echo $$ > {}; sleep 60 & echo $! > {}; wait",
            f.display(),
            g.display()
        );
        let task = tokio::spawn(async move { output(sh(&script), Limits::OP).await });
        // Wait until the helper is demonstrably running, then abort.
        while !g.exists()
            || std::fs::read_to_string(&g)
                .unwrap_or_default()
                .trim()
                .is_empty()
        {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        task.abort();
        let _ = task.await; // best-effort: JoinError::Cancelled expected
        assert!(wait_gone(&f).await, "leader survived abort {i}");
        assert!(wait_gone(&g).await, "grandchild survived abort {i}");
    }
}
