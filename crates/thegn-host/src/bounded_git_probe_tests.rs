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
                // Best-effort diagnostic: cleanup must not panic on stderr failure.
                std::io::Write::write_fmt(
                    &mut std::io::stderr(),
                    format_args!("private pipe-canary release failed: {error}\n"),
                )
                .unwrap_or(());
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

#[test]
fn capability_fixed_policy_refuses_invalid_limits_before_spawn() {
    let dir = tempfile::tempdir().unwrap();
    for (duration, bytes, expected) in [
        (std::time::Duration::ZERO, 16, "invalid capture deadline"),
        (
            std::time::Duration::from_secs(3),
            16,
            "capability capture exceeds its fixed safety policy",
        ),
        (
            std::time::Duration::from_secs(1),
            0,
            "invalid capture byte bound",
        ),
        (
            std::time::Duration::from_secs(1),
            16 * 1024 + 1,
            "capability capture exceeds its fixed safety policy",
        ),
    ] {
        // Nonexistent command distinguishes admission refusal from spawn error.
        let command = std::process::Command::new(dir.path().join("must-not-spawn"));
        let error = capture_capability(command, duration, bytes).unwrap_err();
        assert_eq!(error.to_string(), expected);
    }
}

#[test]
fn capability_budget_is_independent_and_reapers_are_distinct() {
    static GIT: AtomicUsize = AtomicUsize::new(0);
    static CAPABILITY: AtomicUsize = AtomicUsize::new(0);
    let capability = budget_for(&CAPABILITY, CaptureLane::Capability).unwrap();
    assert!(budget_for(&CAPABILITY, CaptureLane::Capability).is_err());
    let first = budget_for(&GIT, CaptureLane::Git).unwrap();
    let second = budget_for(&GIT, CaptureLane::Git).unwrap();
    assert!(budget_for(&GIT, CaptureLane::Git).is_err());
    assert!(!std::ptr::eq(
        reaper(CaptureLane::Git).unwrap(),
        reaper(CaptureLane::Capability).unwrap()
    ));
    drop((first, second, capability));
    assert_eq!(GIT.load(Ordering::Acquire), 0);
    assert_eq!(CAPABILITY.load(Ordering::Acquire), 0);
}

fn private_shell(dir: &Path, script: &str) -> std::process::Command {
    let mut command = std::process::Command::new("/bin/sh");
    command.env_clear().current_dir(dir).args(["-c", script]);
    command
}

#[test]
fn capability_full_output_preserves_nonzero_status_and_both_streams() {
    if !Path::new("/bin/sh").is_file() {
        return;
    } // Unix subprocess fixture
    static COUNTER: AtomicUsize = AtomicUsize::new(0);
    let dir = tempfile::tempdir().unwrap();
    let output = capture_output_with(
        private_shell(
            dir.path(),
            "printf 'version stdout'; printf 'version stderr' >&2; exit 23",
        ),
        None,
        std::time::Duration::from_secs(1),
        16 * 1024,
        &COUNTER,
        CaptureLane::Capability,
        spawn_worker,
    )
    .unwrap();
    assert_eq!(output.status.code(), Some(23));
    assert_eq!(output.stdout, b"version stdout");
    assert_eq!(output.stderr, b"version stderr");
    wait_for_capacity_release(&COUNTER);
}

#[test]
fn capability_noisy_and_hung_helpers_return_with_bounded_capture() {
    if !Path::new("/bin/sh").is_file() || !Path::new("/bin/sleep").is_file() {
        return;
    } // Unix subprocess fixture
    static COUNTER: AtomicUsize = AtomicUsize::new(0);
    let dir = tempfile::tempdir().unwrap();
    for script in ["printf '123456789'", "printf '123456789' >&2"] {
        let error = capture_output_with(
            private_shell(dir.path(), script),
            None,
            std::time::Duration::from_secs(1),
            8,
            &COUNTER,
            CaptureLane::Capability,
            spawn_worker,
        )
        .unwrap_err();
        assert!(error.to_string().contains("output exceeded"), "{error}");
        wait_for_capacity_release(&COUNTER);
    }
    let started = std::time::Instant::now();
    let error = capture_output_with(
        private_shell(dir.path(), "exec /bin/sleep 5"),
        None,
        std::time::Duration::from_millis(50),
        16,
        &COUNTER,
        CaptureLane::Capability,
        spawn_worker,
    )
    .unwrap_err();
    assert!(error.to_string().contains("deadline"), "{error}");
    assert!(started.elapsed() < std::time::Duration::from_secs(2));
    wait_for_capacity_release(&COUNTER);
}

#[test]
fn capability_inherited_pipe_keeps_only_its_lane_budget() {
    if !Path::new("/bin/sh").is_file() || !Path::new("/bin/sleep").is_file() {
        return;
    } // Unix subprocess fixture
    static CAPABILITY: AtomicUsize = AtomicUsize::new(0);
    static GIT: AtomicUsize = AtomicUsize::new(0);
    struct Release(PathBuf);
    impl Drop for Release {
        fn drop(&mut self) {
            // Best-effort panic-path release; the child also has a fixed bound.
            std::fs::write(&self.0, "release").unwrap_or(());
        }
    }
    let dir = tempfile::tempdir().unwrap();
    let release = Release(dir.path().join("release"));
    let error = capture_output_with(private_shell(dir.path(),
        "(i=0; while [ ! -f release ] && [ $i -lt 200 ]; do i=$((i + 1)); /bin/sleep 0.01; done) & exit 0"),
        None, std::time::Duration::from_millis(100), 16, &CAPABILITY, CaptureLane::Capability, spawn_worker).unwrap_err();
    assert!(
        error.to_string().contains("reader") && error.to_string().contains("timed out"),
        "{error}"
    );
    assert_eq!(CAPABILITY.load(Ordering::Acquire), 1);
    assert!(budget_for(&CAPABILITY, CaptureLane::Capability).is_err());
    let git = capture_with(
        private_shell(dir.path(), "printf 'git lane available'"),
        None,
        "lane fixture",
        std::time::Duration::from_secs(1),
        &GIT,
        spawn_worker,
    )
    .unwrap();
    assert_eq!(git, b"git lane available");
    std::fs::write(&release.0, "release").unwrap();
    drop(release);
    wait_for_capacity_release(&CAPABILITY);
    wait_for_capacity_release(&GIT);
}

#[test]
fn capability_blocked_reaper_does_not_block_git_reaping() {
    if !Path::new("/bin/sh").is_file() {
        return;
    } // Unix subprocess fixture
    static CAPABILITY: AtomicUsize = AtomicUsize::new(0);
    static GIT: AtomicUsize = AtomicUsize::new(0);
    reaper(CaptureLane::Capability).unwrap();
    reaper(CaptureLane::Git).unwrap();
    let dir = tempfile::tempdir().unwrap();
    // Builtin read blocks on our retained pipe; dropping stdin also cleans up
    // on assertion unwind. No ambient process, signal or provider is touched.
    let mut child = private_shell(dir.path(), "read line")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .unwrap();
    let stdin = child.stdin.take().unwrap();
    let lease = budget_for(&CAPABILITY, CaptureLane::Capability).unwrap();
    reap_later(child, lease, CaptureLane::Capability);
    assert_eq!(CAPABILITY.load(Ordering::Acquire), 1);
    let child = private_shell(dir.path(), "exit 0").spawn().unwrap();
    reap_later(
        child,
        budget_for(&GIT, CaptureLane::Git).unwrap(),
        CaptureLane::Git,
    );
    wait_for_capacity_release(&GIT);
    assert_eq!(
        CAPABILITY.load(Ordering::Acquire),
        1,
        "capability reaper is still waiting on our pipe"
    );
    drop(stdin);
    wait_for_capacity_release(&CAPABILITY);
}
