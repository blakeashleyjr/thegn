use super::*;
use std::path::{Path, PathBuf};

#[test]
fn unfinished_child_or_pipe_owns_capacity_until_all_holders_release() {
    static COUNTER: AtomicUsize = AtomicUsize::new(0);
    let child = git_budget(&COUNTER).unwrap();
    let stuck_reader = Arc::clone(&child);
    let second = git_budget(&COUNTER).unwrap();
    assert!(git_budget(&COUNTER).is_err());
    drop(child);
    assert!(
        git_budget(&COUNTER).is_err(),
        "reader still owns first slot"
    );
    drop(stuck_reader);
    let replacement = git_budget(&COUNTER).unwrap();
    drop(second);
    drop(replacement);
    assert_eq!(COUNTER.load(Ordering::Acquire), 0);
}

fn wait_for_capacity_release(counter: &AtomicUsize) {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
    while counter.load(Ordering::Acquire) != 0 && std::time::Instant::now() < deadline {
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    assert_eq!(
        counter.load(Ordering::Acquire),
        0,
        "owned reaper/readers did not release capacity"
    );
}

#[test]
fn failed_pipe_worker_creation_hands_owned_child_to_bounded_reaper() {
    static COUNTER: AtomicUsize = AtomicUsize::new(0);
    fn fail_second(name: &str, task: Box<dyn FnOnce() + Send>) -> std::io::Result<()> {
        if name.ends_with("stderr") {
            return Err(std::io::Error::other("injected thread creation refusal"));
        }
        spawn_worker(name, task)
    }
    let dir = tempfile::tempdir().unwrap();
    let config = dir.path().join("gitconfig");
    std::fs::write(&config, "").unwrap();
    // Git waits for EOF on the still-owned stdin pipe; no executable shim.
    // hash-object does not need a repository; exclude ambient Git configuration.
    let mut command = thegn_core::util::git_cmd(dir.path());
    command
        .env("GIT_CONFIG_GLOBAL", &config)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_COUNT", "0")
        .env_remove("GIT_CONFIG_PARAMETERS")
        .env_remove("GIT_TEMPLATE_DIR");
    command.args(["hash-object", "--stdin"]);
    let result = capture_with(
        command,
        Some("private input".into()),
        "worker canary",
        std::time::Duration::from_millis(100),
        &COUNTER,
        fail_second,
    );
    assert!(
        matches!(result, Err(ProbeError(reason)) if reason.contains("stderr worker unavailable"))
    );
    wait_for_capacity_release(&COUNTER);
}

#[test]
fn timeout_returns_without_waiting_for_child_reap() {
    if !Path::new("/bin/sh").is_file() {
        return;
    } // private Unix runtime canary
    static COUNTER: AtomicUsize = AtomicUsize::new(0);
    let dir = tempfile::tempdir().unwrap();
    let mut command = std::process::Command::new("/bin/sh");
    command.current_dir(dir.path()).args(["-c", "sleep 10"]);
    let result = capture_with(
        command,
        None,
        "timeout canary",
        std::time::Duration::from_millis(50),
        &COUNTER,
        spawn_worker,
    );
    assert!(matches!(result, Err(ProbeError(reason)) if reason.contains("deadline")));
    wait_for_capacity_release(&COUNTER);
}

#[test]
fn inherited_pipe_is_bounded_and_keeps_its_resource_budget() {
    if !Path::new("/bin/sh").is_file() {
        return;
    } // private Unix runtime canary
    static COUNTER: AtomicUsize = AtomicUsize::new(0);
    let dir = tempfile::tempdir().unwrap();
    struct Release(PathBuf);
    impl Drop for Release {
        fn drop(&mut self) {
            if let Err(error) = std::fs::write(&self.0, "release") {
                eprintln!("private pipe-canary release failed: {error}");
            }
        } // best-effort: private canary cleanup even on assertion failure
    }
    let release = Release(dir.path().join("release"));
    let mut command = std::process::Command::new("/bin/sh");
    command.current_dir(dir.path()).args(["-c", "(i=0; while [ ! -f release ] && [ $i -lt 100 ]; do i=$((i + 1)); sleep 0.05; done) & exit 0"]);
    let result = capture_with(
        command,
        None,
        "pipe canary",
        std::time::Duration::from_millis(200),
        &COUNTER,
        spawn_worker,
    );
    assert!(
        matches!(result, Err(ProbeError(reason)) if reason.contains("reader") && reason.contains("timed out"))
    );
    assert_eq!(COUNTER.load(Ordering::Acquire), 1);
    drop(release);
    wait_for_capacity_release(&COUNTER);
}
