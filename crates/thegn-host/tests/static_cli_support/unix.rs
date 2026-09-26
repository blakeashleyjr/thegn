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
    // source-proof observations, not execution of an API call or automation. No
    // FIFO writer is ever opened.
    //
    // The static commands above succeeded against this same FIFO because they
    // never consult the configuration source. Every command below does consult
    // it — which is the property under test — but they do not agree on how:
    // configuration admission refuses a non-regular source outright, while a
    // command that still opens the source directly blocks on the unread FIFO.
    // Admission is not yet uniform across the CLI, so assert what each one
    // actually does rather than blurring the two. THE-691 makes them uniform;
    // when it lands, move the rest into `assert_source_refused` alongside this.
    let admitted: &[&str] = &["api", "call", "worktrees.list"];
    fixture.assert_source_refused(admitted, &config);
    assert_eq!(fixture.snapshot(), before, "{admitted:?} mutated the root");
    for args in [
        &["config", "get", "drawer.height"][..],
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
