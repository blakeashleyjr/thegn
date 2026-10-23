use std::path::Path;
use std::process::{Child, Command, ExitStatus, Stdio};
use std::time::{Duration, Instant};

use thegn_core::testenv::state_env_all;

const EXIT_TIMEOUT: Duration = Duration::from_secs(5);

#[expect(
    clippy::disallowed_methods,
    reason = "process regression test owns and reaps the daemon child"
)]
fn signal_at_socket_bind(mut command: Command, socket: &Path) -> (ExitStatus, bool) {
    let mut child: Child = command.spawn().expect("spawn isolated daemon");
    let bind_deadline = Instant::now() + EXIT_TIMEOUT;
    while !socket.exists() && Instant::now() < bind_deadline {
        std::thread::sleep(Duration::from_millis(2));
    }
    if !socket.exists() {
        child
            .kill()
            .expect("kill daemon that did not bind its socket");
        let _status = child.wait().expect("reap daemon that did not bind");
        panic!("daemon did not create its socket before the bind deadline");
    }

    let signal = Command::new("kill")
        .arg("-TERM")
        .arg(child.id().to_string())
        .status()
        .expect("run kill utility for owned daemon child");
    if !signal.success() {
        child
            .kill()
            .expect("kill daemon after SIGTERM delivery failed");
        let _status = child
            .wait()
            .expect("reap daemon after SIGTERM delivery failed");
        panic!("SIGTERM did not reach daemon after socket bind: {signal}");
    }

    let exit_deadline = Instant::now() + EXIT_TIMEOUT;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if Instant::now() < exit_deadline => {
                std::thread::sleep(Duration::from_millis(10));
            }
            Ok(None) | Err(_) => {
                child
                    .kill()
                    .expect("kill daemon after graceful-shutdown timeout");
                let _status = child
                    .wait()
                    .expect("reap daemon after graceful-shutdown timeout");
                panic!("daemon did not exit within {EXIT_TIMEOUT:?} after SIGTERM");
            }
        }
    };
    (status, !socket.exists())
}

#[test]
fn sigterm_immediately_after_daemon_socket_bind_unlinks_socket() {
    if !thegn_core::testenv::require_or_skip("Unix sockets and SIGTERM", cfg!(unix)) {
        return;
    }

    // Each iteration owns independent XDG roots, so daemon registry state from
    // an earlier child cannot make a later socket bind exit as AlreadyRunning.
    for iteration in 0..10 {
        let root = tempfile::Builder::new()
            .prefix("tgds")
            .tempdir_in("/tmp")
            .expect("create short isolated daemon root");
        let runtime = root.path().join("runtime");
        std::fs::create_dir(&runtime).expect("create isolated runtime directory");
        let socket = runtime.join("d.sock");
        let mut command = Command::new(env!("CARGO_BIN_EXE_thegn"));
        command
            .env_clear()
            .current_dir(root.path())
            .envs(state_env_all(root.path()))
            .env("XDG_RUNTIME_DIR", &runtime)
            .env("TMPDIR", root.path())
            .env("PATH", std::env::var_os("PATH").unwrap_or_default())
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .args(["daemon", "--socket"])
            .arg(&socket);

        let (status, socket_removed) = signal_at_socket_bind(command, &socket);
        assert!(
            status.success(),
            "daemon iteration {iteration} should exit successfully after SIGTERM"
        );
        assert!(
            socket_removed,
            "daemon iteration {iteration} must unlink its socket after SIGTERM"
        );
    }
}
