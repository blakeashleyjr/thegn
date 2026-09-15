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
fn static_commands_do_not_open_fifo_configuration_but_configured_siblings_do() {
    let mut fixture = Fixture::new();
    let config = fixture.path("blocking.toml");
    fifo(&config);
    let before = fixture.snapshot();
    for args in STATIC {
        let output = fixture.run(args, Some(&config), false);
        check_output(args, &output, &config, "thegn");
        assert_eq!(fixture.snapshot(), before);
    }
    // A successful regular-file counterpart is covered separately. These are
    // loader-bound observations plus source proof, not execution of an API call
    // or automation. No FIFO writer is ever opened.
    for args in [
        &["config", "get", "drawer.height"][..],
        &["api", "call", "worktrees.list"][..],
        &["automations", "test", "missing", "--event", "{}"][..],
    ] {
        fixture.assert_loader_wait(args, &config);
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
