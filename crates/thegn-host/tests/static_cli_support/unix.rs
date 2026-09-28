use super::*;
use std::ffi::CString;
use std::os::fd::{FromRawFd, OwnedFd};
use std::os::unix::ffi::OsStrExt;
use std::process::Stdio;

fn fifo(path: &Path) {
    let path = CString::new(path.as_os_str().as_bytes()).unwrap();
    // SAFETY: NUL-terminated private TempDir path, valid mode, no borrowed fd.
    assert_eq!(unsafe { libc::mkfifo(path.as_ptr(), 0o600) }, 0);
}

#[test]
fn static_commands_skip_config_but_configured_commands_refuse_fifo_sources() {
    let mut fixture = Fixture::new();
    let config = fixture.path("blocking.toml");
    fifo(&config);
    let before = fixture.snapshot();
    for args in STATIC {
        let output = fixture.run(args, Some(&config), false);
        check_output(args, &output, &config, "thegn");
        assert_eq!(fixture.snapshot(), before);
    }
    // No FIFO writer is opened. All configured commands refuse the same source
    // kind with a concrete reason, while static commands above never inspect it.
    let configured: &[&[&str]] = &[
        &["api", "call", "worktrees.list"],
        &["config", "get", "drawer.height"],
        &["automations", "test", "missing", "--event", "{}"],
    ];
    for args in configured {
        fixture.assert_source_refused(args, &config);
    }
    assert_eq!(
        fixture.snapshot(),
        before,
        "configured commands mutated root"
    );
    fixture.close();
}

#[test]
fn config_get_still_inspects_malformed_regular_configuration() {
    let mut fixture = Fixture::new();
    let config = fixture.path("malformed.toml");
    std::fs::write(&config, b"[drawer\nheight = ").unwrap();

    let output = fixture.run(&["config", "get", "drawer.height"], Some(&config), false);
    assert!(
        output.status.success(),
        "malformed regular config should remain inspectable: {}",
        output.stderr
    );
    assert!(
        output.stderr.contains("parse error"),
        "malformed TOML should still be reported: {}",
        output.stderr
    );
    fixture.close();
}

#[test]
fn tolerant_inspection_refuses_oversized_regular_configuration() {
    let mut fixture = Fixture::new();
    let config = fixture.path("oversized.toml");
    let limit = thegn_core::config_budget::MAX_SOURCE_BYTES;
    std::fs::write(&config, vec![b' '; limit + 1]).unwrap();

    for args in [
        &["config", "get", "drawer.height"][..],
        &["automations", "test", "missing", "--event", "{}"][..],
    ] {
        let output = fixture.run(args, Some(&config), false);
        assert!(
            !output.status.success(),
            "accepted oversized source: {args:?}"
        );
        assert!(
            output
                .stderr
                .contains("exceeds its 4194304-byte tolerant read limit"),
            "expected the bounded-source refusal for {args:?}: {}",
            output.stderr
        );
    }
    fixture.close();
}

#[test]
fn tolerant_inspection_refuses_nonregular_profile_configuration() {
    let mut fixture = Fixture::new();
    let base = fixture.path("base.toml");
    std::fs::write(&base, "").unwrap();
    let profile_dir = fixture
        .path("config")
        .join("thegn/profiles/fixture-profile");
    std::fs::create_dir_all(&profile_dir).unwrap();
    let profile = profile_dir.join("config.toml");
    fifo(&profile);

    for args in [
        &["config", "get", "drawer.height"][..],
        &["automations", "test", "missing", "--event", "{}"][..],
    ] {
        let output = fixture.run(args, Some(&base), true);
        assert!(!output.status.success(), "accepted profile FIFO: {args:?}");
        assert!(
            output
                .stderr
                .contains("config source is a FIFO, not a regular file"),
            "expected a profile source-kind refusal for {args:?}: {}",
            output.stderr
        );
    }
    fixture.close();
}

fn closed_reader() -> Stdio {
    let mut fds = [-1; 2];
    // SAFETY: writable two-element fd output array. Success transfers two new
    // owned descriptors; each is converted exactly once and the reader closes.
    assert_eq!(unsafe { libc::pipe(fds.as_mut_ptr()) }, 0);
    let reader = unsafe { OwnedFd::from_raw_fd(fds[0]) };
    let writer = unsafe { OwnedFd::from_raw_fd(fds[1]) };
    drop(reader);
    Stdio::from(writer)
}

#[test]
fn early_closed_stdout_keeps_schema_catalog_and_completion_exit_success() {
    let mut fixture = Fixture::new();
    let config = fixture.path("blocking.toml");
    fifo(&config);
    for args in [
        &["config", "schema"][..],
        &["api", "list", "--json"][..],
        &["api", "schema"][..],
        &["completions", "bash"][..],
        &["completions", "bash", "--static"][..],
    ] {
        let output = fixture.run_binary(
            &fixture.binary(),
            args,
            Some(&config),
            false,
            Some(closed_reader()),
        );
        success(&output);
        assert!(output.stdout.is_empty());
    }
    fixture.close();
}
