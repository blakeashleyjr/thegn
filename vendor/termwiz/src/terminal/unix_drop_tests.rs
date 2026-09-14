use super::*;
use std::os::fd::FromRawFd;

fn pty() -> (std::fs::File, std::fs::File) {
    let (mut master, mut slave) = (-1, -1);
    // Both descriptors belong solely to this test, never to /dev/tty.
    let result = unsafe {
        libc::openpty(
            &mut master,
            &mut slave,
            std::ptr::null_mut(),
            std::ptr::null(),
            std::ptr::null(),
        )
    };
    assert_eq!(result, 0, "{}", std::io::Error::last_os_error());
    unsafe {
        (
            std::fs::File::from_raw_fd(master),
            std::fs::File::from_raw_fd(slave),
        )
    }
}

fn terminal(slave: &std::fs::File) -> UnixTerminal {
    let caps = Capabilities::new_with_hints(ProbeHints::default()).unwrap();
    let mut terminal = UnixTerminal::new_with(caps, slave, slave).unwrap();
    terminal.set_raw_mode().unwrap();
    terminal.enter_alternate_screen().unwrap();
    terminal.flush().unwrap();
    terminal
}

#[test]
fn healthy_drop_restores_saved_terminal_modes() {
    let (_master, slave) = pty();
    let saved = unsafe {
        let mut saved = std::mem::zeroed::<libc::termios>();
        assert_eq!(libc::tcgetattr(slave.as_raw_fd(), &mut saved), 0);
        saved
    };
    drop(terminal(&slave));
    let restored = unsafe {
        let mut restored = std::mem::zeroed::<libc::termios>();
        assert_eq!(libc::tcgetattr(slave.as_raw_fd(), &mut restored), 0);
        restored
    };
    assert_eq!(saved.c_lflag, restored.c_lflag);
    assert_eq!(saved.c_iflag, restored.c_iflag);
    assert_eq!(saved.c_oflag, restored.c_oflag);
    assert_eq!(saved.c_cflag, restored.c_cflag);
    assert_eq!(saved.c_cc, restored.c_cc);
}

#[test]
fn hung_up_pty_drop_is_non_panicking_after_explicit_cleanup() {
    let (master, slave) = pty();
    let mut terminal = terminal(&slave);
    drop(master);
    // Application teardown already attempts these operations. Buffered writes
    // can succeed here while the destructor's subsequent flush fails with EIO.
    terminal.exit_alternate_screen().ok();
    terminal.set_cooked_mode().ok();
    drop(terminal);
}

#[test]
fn hung_up_pty_drop_during_unwind_child() {
    if std::env::var_os("THEGN_TERMWIZ_UNWIND_CHILD").is_none() {
        return;
    }
    let primary = std::panic::catch_unwind(|| {
        let (master, slave) = pty();
        let _terminal = terminal(&slave);
        drop(master);
        panic!("primary test panic");
    });
    assert!(
        primary.is_err(),
        "the original panic must remain observable"
    );
}

#[test]
fn hung_up_pty_drop_during_unwind_does_not_abort() {
    #[expect(
        clippy::disallowed_methods,
        reason = "test-only parent waits for its exact reexecuted PTY unwind fixture, off the compositor"
    )]
    let output = std::process::Command::new(std::env::current_exe().unwrap())
        .arg("--exact")
        .arg(format!(
            "{}::hung_up_pty_drop_during_unwind_child",
            module_path!().split_once("::").unwrap().1
        ))
        .arg("--nocapture")
        .env("THEGN_TERMWIZ_UNWIND_CHILD", "1")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "destruction aborted while unwinding: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(String::from_utf8_lossy(&output.stdout).contains("1 passed"));
}
