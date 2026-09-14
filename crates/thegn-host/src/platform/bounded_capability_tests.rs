//! POSIX subprocess fixtures. Unsupported platforms do not register these tests;
//! supported platforms fail explicitly if a required fixture tool is absent.
#[cfg(unix)]
mod unix {
    use super::super::*;

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

    fn private_shell(dir: &Path, script: &str) -> std::process::Command {
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
    fn capability_each_noisy_stream_returns_with_bounded_capture() {
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
    }

    #[test]
    fn capability_hung_helper_returns_with_bounded_capture() {
        static COUNTER: AtomicUsize = AtomicUsize::new(0);
        let dir = tempfile::tempdir().unwrap();
        let sleep = fixture_tool("sleep");
        let mut command = private_shell(dir.path(), "exec \"$1\" 5");
        command.arg(sleep);
        let started = std::time::Instant::now();
        let error = capture_output_with(
            command,
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
        let sleep = fixture_tool("sleep");
        let mut command = private_shell(
            dir.path(),
            r#"(i=0; while [ ! -f release ] && [ $i -lt 200 ]; do i=$((i + 1)); "$1" 0.01; done) & exit 0"#,
        );
        command.arg(sleep);
        let error = capture_output_with(
            command,
            None,
            std::time::Duration::from_millis(100),
            16,
            &CAPABILITY,
            CaptureLane::Capability,
            spawn_worker,
        )
        .unwrap_err();
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
}
