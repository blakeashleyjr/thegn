//! POSIX subprocess fixtures. Unsupported platforms do not register these tests;
//! supported platforms fail explicitly if a required fixture tool is absent.
#[cfg(unix)]
mod unix {
    use super::super::*;
    use std::path::PathBuf;
    use std::time::{Duration, Instant};

    fn fixture_tool(name: &str) -> PathBuf {
        let discovered = PathBuf::from(thegn_core::util::which_path(name).unwrap_or_else(|| {
            panic!("required private fixture tool missing from initial PATH: {name}")
        }));
        let absolute = if discovered.is_absolute() {
            discovered
        } else {
            std::env::current_dir()
                .expect("private fixture discovery cwd must be available")
                .join(discovered)
        };
        let resolved = std::fs::canonicalize(&absolute).unwrap_or_else(|error| {
            panic!("required fixture tool identity unavailable: {name}: {error}")
        });
        assert!(
            absolute.is_absolute() && resolved.is_file(),
            "fixture tool must resolve to an absolute regular file: {name}"
        );
        // Preserve argv[0]'s lexical basename: Nix sleep may resolve to the
        // multicall coreutils binary, whose behavior depends on that basename.
        absolute
    }

    fn private_shell(dir: &std::path::Path, script: &str) -> std::process::Command {
        let mut command = std::process::Command::new(fixture_tool("sh"));
        command
            .env_clear()
            .current_dir(dir)
            .args(["-c", script, "thegn-capability-fixture"]);
        command
    }

    #[test]
    fn capability_full_output_preserves_nonzero_status_and_both_streams() {
        static COUNTER: AtomicUsize = AtomicUsize::new(0);
        let dir = tempfile::tempdir().unwrap();
        let output = capture_output_with(
            private_shell(
                dir.path(),
                "printf 'version stdout'; printf 'version stderr' >&2; exit 23",
            ),
            None,
            Duration::from_secs(1),
            16 * 1024,
            &COUNTER,
            CaptureLane::Capability,
        )
        .unwrap();
        assert_eq!(output.status.code(), Some(23));
        assert_eq!(output.stdout, b"version stdout");
        assert_eq!(output.stderr, b"version stderr");
        assert_eq!(COUNTER.load(Ordering::Acquire), 0);
    }

    #[test]
    fn capability_each_noisy_stream_returns_with_bounded_capture() {
        static COUNTER: AtomicUsize = AtomicUsize::new(0);
        let dir = tempfile::tempdir().unwrap();
        for script in ["printf '123456789'", "printf '123456789' >&2"] {
            let error = capture_output_with(
                private_shell(dir.path(), script),
                None,
                Duration::from_secs(1),
                8,
                &COUNTER,
                CaptureLane::Capability,
            )
            .unwrap_err();
            assert_eq!(
                error.to_string(),
                "capability probe probe output exceeded the safety bound"
            );
            assert_eq!(COUNTER.load(Ordering::Acquire), 0);
        }
    }

    #[test]
    fn git_output_over_the_cap_errors_instead_of_truncating() {
        static COUNTER: AtomicUsize = AtomicUsize::new(0);
        let dir = tempfile::tempdir().unwrap();
        // 3 MiB against the 2 MiB Git cap: the producer outruns the bound.
        let mut command = private_shell(dir.path(), "exec \"$1\" -c 3145728 /dev/zero");
        command.arg(fixture_tool("head"));
        let error = capture_with(
            command,
            None,
            "over-cap fixture",
            Duration::from_secs(10),
            &COUNTER,
        )
        .unwrap_err();
        assert_eq!(
            error.to_string(),
            "Git probe output exceeded the safety bound"
        );
        assert_eq!(COUNTER.load(Ordering::Acquire), 0);
    }

    #[test]
    fn git_stdin_payload_round_trips_through_cat() {
        static COUNTER: AtomicUsize = AtomicUsize::new(0);
        let dir = tempfile::tempdir().unwrap();
        let mut command = private_shell(dir.path(), "exec \"$1\"");
        command.arg(fixture_tool("cat"));
        let payload = "refs/heads/main\nprivate input\n".to_owned();
        let stdout = capture_with(
            command,
            Some(payload.clone()),
            "stdin fixture",
            Duration::from_secs(5),
            &COUNTER,
        )
        .unwrap();
        assert_eq!(stdout, payload.as_bytes());
        assert_eq!(COUNTER.load(Ordering::Acquire), 0);
    }

    #[test]
    fn git_timeout_returns_within_the_documented_bound() {
        static COUNTER: AtomicUsize = AtomicUsize::new(0);
        let dir = tempfile::tempdir().unwrap();
        // Not exec'd: the shell leader and its sleep child both sit in the group.
        let mut command = private_shell(dir.path(), "\"$1\" 30");
        command.arg(fixture_tool("sleep"));
        let timeout = Duration::from_millis(200);
        let started = Instant::now();
        let error = capture_with(command, None, "timeout fixture", timeout, &COUNTER).unwrap_err();
        let elapsed = started.elapsed();
        assert_eq!(error.to_string(), "Git operation exceeded cleanup deadline");
        assert!(elapsed < timeout + Duration::from_secs(2), "{elapsed:?}");
        assert_eq!(COUNTER.load(Ordering::Acquire), 0);
    }

    #[test]
    fn capability_hung_helper_returns_with_bounded_capture() {
        static COUNTER: AtomicUsize = AtomicUsize::new(0);
        let dir = tempfile::tempdir().unwrap();
        let mut command = private_shell(dir.path(), "exec \"$1\" 5");
        command.arg(fixture_tool("sleep"));
        let started = Instant::now();
        let error = capture_output_with(
            command,
            None,
            Duration::from_millis(50),
            16,
            &COUNTER,
            CaptureLane::Capability,
        )
        .unwrap_err();
        assert!(error.to_string().contains("deadline"), "{error}");
        assert!(started.elapsed() < Duration::from_secs(2));
        assert_eq!(COUNTER.load(Ordering::Acquire), 0);
    }

    #[test]
    fn grandchild_holding_the_pipe_does_not_hang_past_the_bound() {
        static CAPABILITY: AtomicUsize = AtomicUsize::new(0);
        static GIT: AtomicUsize = AtomicUsize::new(0);
        let dir = tempfile::tempdir().unwrap();
        // The backgrounded sleep inherits stdout/stderr and outlives the leader;
        // the group is terminated as soon as the leader exits.
        let mut command = private_shell(dir.path(), "\"$1\" 30 & printf 'leader done'; exit 0");
        command.arg(fixture_tool("sleep"));
        let timeout = Duration::from_secs(1);
        let started = Instant::now();
        let output = capture_output_with(
            command,
            None,
            timeout,
            16,
            &CAPABILITY,
            CaptureLane::Capability,
        )
        .unwrap();
        let elapsed = started.elapsed();
        assert!(elapsed < timeout + Duration::from_secs(2), "{elapsed:?}");
        assert!(output.status.success());
        assert_eq!(output.stdout, b"leader done");
        assert_eq!(CAPABILITY.load(Ordering::Acquire), 0);
        // The Git lane is unaffected and still admits work.
        let git = capture_with(
            private_shell(dir.path(), "printf 'git lane available'"),
            None,
            "lane fixture",
            Duration::from_secs(1),
            &GIT,
        )
        .unwrap();
        assert_eq!(git, b"git lane available");
        assert_eq!(GIT.load(Ordering::Acquire), 0);
    }
}
