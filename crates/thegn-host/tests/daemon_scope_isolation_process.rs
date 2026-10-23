use std::fs::{self, File};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use thegn_core::config::DaemonConfig;
use thegn_core::testenv::state_env_all;

const CAPTURE_LIMIT: u64 = 64 * 1024;

struct Scope {
    root: tempfile::TempDir,
}

impl Scope {
    fn new(prefix: &str) -> Scope {
        Scope {
            root: tempfile::Builder::new()
                .prefix(prefix)
                .tempdir_in("/tmp")
                .expect("create short isolated daemon scope"),
        }
    }

    fn command(&self, runtime: &Path) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_thegn"));
        command
            .env_clear()
            .current_dir(self.root.path())
            .envs(state_env_all(self.root.path()))
            .env("XDG_RUNTIME_DIR", runtime)
            .env("TMPDIR", self.root.path())
            .env(
                "PATH",
                std::env::var_os("PATH").expect("PATH is required for process tests"),
            )
            .stdin(Stdio::null());
        command
    }
}

fn predicted_endpoint(scope: &Scope, runtime: &Path) -> PathBuf {
    let runtime = runtime.to_str().expect("runtime path is UTF-8");
    DaemonConfig::default().socket_path(
        Some(runtime),
        &scope.root.path().join("state").join("thegn"),
    )
}

struct OwnedDaemon {
    child: Child,
    socket: PathBuf,
}

impl Drop for OwnedDaemon {
    #[expect(
        clippy::disallowed_methods,
        reason = "process test owns and reaps the daemon child"
    )]
    fn drop(&mut self) {
        if let Err(error) = self.child.kill()
            && error.kind() != std::io::ErrorKind::InvalidInput
        {
            let _write_result = writeln!(
                std::io::stderr().lock(),
                "daemon_scope_isolation_process: could not kill daemon during cleanup: {error}"
            );
        }

        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            match self.child.try_wait() {
                Ok(Some(_)) => return,
                Ok(None) if Instant::now() < deadline => thread::sleep(Duration::from_millis(10)),
                Ok(None) => {
                    let _write_result = writeln!(
                        std::io::stderr().lock(),
                        "daemon_scope_isolation_process: daemon did not exit after kill; waiting to reap"
                    );
                    let _status = self
                        .child
                        .wait()
                        .expect("reap daemon after cleanup timeout");
                    return;
                }
                Err(error) => {
                    let _write_result = writeln!(
                        std::io::stderr().lock(),
                        "daemon_scope_isolation_process: could not poll daemon during cleanup: {error}"
                    );
                    let _status = self.child.wait().expect("reap daemon after poll error");
                    return;
                }
            }
        }
    }
}

fn read_capped(path: &Path) -> String {
    let file = File::open(path).expect("open captured child output");
    let mut bytes = Vec::new();
    file.take(CAPTURE_LIMIT)
        .read_to_end(&mut bytes)
        .expect("read capped child output");
    String::from_utf8_lossy(&bytes).into_owned()
}

#[expect(
    clippy::disallowed_methods,
    reason = "process test owns and reaps the daemon child"
)]
fn start_daemon(mut command: Command, socket: &Path) -> OwnedDaemon {
    let capture_dir = command
        .get_current_dir()
        .expect("daemon command has an isolated scope directory")
        .to_path_buf();
    let stdout_path = capture_dir.join("daemon.stdout");
    let stderr_path = capture_dir.join("daemon.stderr");
    let stdout = File::create(&stdout_path).expect("create daemon stdout capture");
    let stderr = File::create(&stderr_path).expect("create daemon stderr capture");
    let mut child = command
        .stdout(Stdio::from(stdout))
        .stderr(Stdio::from(stderr))
        .spawn()
        .expect("spawn isolated pane daemon");

    let deadline = Instant::now() + Duration::from_secs(10);
    while !socket.exists() && Instant::now() < deadline {
        match child.try_wait() {
            Ok(Some(status)) => {
                panic!(
                    "daemon exited before binding {} ({status}); stdout:\n{}\nstderr:\n{}",
                    socket.display(),
                    read_capped(&stdout_path),
                    read_capped(&stderr_path)
                );
            }
            Ok(None) => thread::sleep(Duration::from_millis(10)),
            Err(error) => {
                child.kill().expect("kill daemon after poll failure");
                let _status = child.wait().expect("reap daemon after poll failure");
                panic!(
                    "could not poll daemon while waiting for {}: {error}",
                    socket.display()
                );
            }
        }
    }
    if !socket.exists() {
        child
            .kill()
            .expect("kill daemon that did not bind its socket");
        let _status = child.wait().expect("reap daemon that did not bind");
        panic!(
            "daemon did not bind {} before deadline; stdout:\n{}\nstderr:\n{}",
            socket.display(),
            read_capped(&stdout_path),
            read_capped(&stderr_path)
        );
    }

    OwnedDaemon {
        child,
        socket: socket.to_path_buf(),
    }
}

#[expect(
    clippy::disallowed_methods,
    reason = "process test owns and reaps every CLI child"
)]
fn run_cli(mut command: Command, timeout: Duration) -> (ExitStatus, String, String) {
    let captures = tempfile::Builder::new()
        .prefix("tgsc")
        .tempdir_in("/tmp")
        .expect("create short CLI capture directory");
    let stdout_path = captures.path().join("stdout");
    let stderr_path = captures.path().join("stderr");
    let stdout = File::create(&stdout_path).expect("create CLI stdout capture");
    let stderr = File::create(&stderr_path).expect("create CLI stderr capture");
    let mut child = command
        .stdout(Stdio::from(stdout))
        .stderr(Stdio::from(stderr))
        .spawn()
        .expect("spawn isolated CLI command");
    let deadline = Instant::now() + timeout;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if Instant::now() < deadline => thread::sleep(Duration::from_millis(10)),
            Ok(None) => {
                child.kill().expect("kill CLI child after timeout");
                let _status = child.wait().expect("reap CLI child after timeout");
                panic!(
                    "CLI command exceeded {timeout:?}; stdout:\n{}\nstderr:\n{}",
                    read_capped(&stdout_path),
                    read_capped(&stderr_path)
                );
            }
            Err(error) => {
                child.kill().expect("kill CLI child after poll failure");
                let _status = child.wait().expect("reap CLI child after poll failure");
                panic!("could not poll CLI child: {error}");
            }
        }
    };
    (status, read_capped(&stdout_path), read_capped(&stderr_path))
}

fn assert_alive(daemon: &mut OwnedDaemon, why: &str) {
    assert!(
        matches!(daemon.child.try_wait(), Ok(None)) && daemon.socket.exists(),
        "{why}: daemon must still be running with socket {}",
        daemon.socket.display()
    );
}

fn assert_exits(daemon: &mut OwnedDaemon, why: &str) {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        match daemon.child.try_wait() {
            Ok(Some(_)) => {
                assert!(
                    !daemon.socket.exists(),
                    "{why}: exited daemon must remove socket {}",
                    daemon.socket.display()
                );
                return;
            }
            Ok(None) if Instant::now() < deadline => thread::sleep(Duration::from_millis(10)),
            Ok(None) => panic!("{why}: daemon did not exit before deadline"),
            Err(error) => panic!("{why}: could not poll daemon: {error}"),
        }
    }
}

fn shared_runtime() -> tempfile::TempDir {
    let runtime = tempfile::Builder::new()
        .prefix("tgsr")
        .tempdir_in("/tmp")
        .expect("create short shared runtime directory");
    fs::create_dir(runtime.path().join("thegn")).expect("create runtime daemon directory");
    runtime
}

fn chmod_private(path: &Path, runtime: &Path) {
    let mut command = Command::new("chmod");
    command.arg("700").arg(path).env("TMPDIR", runtime);
    let (status, stdout, stderr) = run_cli(command, Duration::from_secs(5));
    assert!(
        status.success(),
        "chmod 700 {} failed; stdout:\n{stdout}\nstderr:\n{stderr}",
        path.display()
    );
}

fn start_foreign_at_a_endpoint(scope_b: &Scope, runtime: &Path, endpoint_a: &Path) -> OwnedDaemon {
    let parent = endpoint_a.parent().expect("foreign socket has a parent");
    fs::create_dir_all(parent).expect("create foreign endpoint parent directories");
    chmod_private(&runtime.join("thegn"), runtime);
    chmod_private(parent, runtime);
    let mut command = scope_b.command(runtime);
    command.arg("daemon").arg("--socket").arg(endpoint_a);
    start_daemon(command, endpoint_a)
}

#[test]
// A and B are separately spawned direct children with independent state roots and
// endpoints under one runtime directory. The stop CLI is another child; it must
// signal A through its endpoint while B remains independently pollable and owned.
fn scope_isolation_daemon_stop_leaves_a_parallel_scope_running() {
    if !thegn_core::testenv::require_or_skip("Unix-socket pane daemons", cfg!(unix)) {
        return;
    }

    let runtime = shared_runtime();
    let scope_a = Scope::new("tgsa");
    let scope_b = Scope::new("tgsb");
    let socket_a = predicted_endpoint(&scope_a, runtime.path());
    let socket_b = predicted_endpoint(&scope_b, runtime.path());
    assert_ne!(
        socket_a, socket_b,
        "parallel state scopes need distinct endpoints"
    );

    let mut daemon_a = scope_a.command(runtime.path());
    daemon_a.arg("daemon");
    let mut daemon_a = start_daemon(daemon_a, &socket_a);
    let mut daemon_b = scope_b.command(runtime.path());
    daemon_b.arg("daemon");
    let mut daemon_b = start_daemon(daemon_b, &socket_b);

    let mut stop_a = scope_a.command(runtime.path());
    stop_a.args(["daemon", "stop"]);
    let (status, stdout, stderr) = run_cli(stop_a, Duration::from_secs(10));
    assert!(
        status.success() && stdout.contains("stopped "),
        "scope A daemon stop should stop its daemon; stdout:\n{stdout}\nstderr:\n{stderr}"
    );
    assert_exits(
        &mut daemon_a,
        &format!("scope A daemon stop; stdout:\n{stdout}\nstderr:\n{stderr}"),
    );
    assert_alive(
        &mut daemon_b,
        &format!("scope A daemon stop must preserve scope B; stdout:\n{stdout}\nstderr:\n{stderr}"),
    );

    let mut stop_b = scope_b.command(runtime.path());
    stop_b.args(["daemon", "stop"]);
    let (status, stdout, stderr) = run_cli(stop_b, Duration::from_secs(10));
    assert!(
        status.success() && stdout.contains("stopped "),
        "scope B daemon stop should stop its daemon; stdout:\n{stdout}\nstderr:\n{stderr}"
    );
    assert_exits(
        &mut daemon_b,
        &format!("scope B daemon stop; stdout:\n{stdout}\nstderr:\n{stderr}"),
    );
}

#[test]
// B is the sole daemon child and reports B's state while listening at A's predicted
// endpoint. A's stop CLI must classify the reported scope before sending shutdown;
// the test process retains and reaps B regardless of the CLI result.
fn scope_isolation_daemon_stop_skips_a_foreign_daemon_at_its_own_endpoint() {
    if !thegn_core::testenv::require_or_skip("Unix-socket pane daemons", cfg!(unix)) {
        return;
    }

    let runtime = shared_runtime();
    let scope_a = Scope::new("tgsa");
    let scope_b = Scope::new("tgsb");
    let socket_a = predicted_endpoint(&scope_a, runtime.path());
    let mut daemon_b = start_foreign_at_a_endpoint(&scope_b, runtime.path(), &socket_a);

    let mut stop_a = scope_a.command(runtime.path());
    stop_a.args(["daemon", "stop"]);
    let (status, stdout, stderr) = run_cli(stop_a, Duration::from_secs(10));
    assert!(
        status.success()
            && stdout.contains("belongs to a different state directory")
            && stdout.contains("no thegn pane daemon is running"),
        "scope A should visibly skip the foreign daemon and report no local daemon; stdout:\n{stdout}\nstderr:\n{stderr}"
    );
    assert_alive(
        &mut daemon_b,
        &format!("scope A must not stop foreign scope B; stdout:\n{stdout}\nstderr:\n{stderr}"),
    );

    let mut stop_b = scope_b.command(runtime.path());
    stop_b.args(["daemon", "stop"]);
    let (status, stdout, stderr) = run_cli(stop_b, Duration::from_secs(10));
    assert!(
        status.success() && stdout.contains("stopped "),
        "scope B daemon stop should stop its daemon; stdout:\n{stdout}\nstderr:\n{stderr}"
    );
    assert_exits(
        &mut daemon_b,
        &format!("scope B daemon stop; stdout:\n{stdout}\nstderr:\n{stderr}"),
    );
}

#[test]
// B remains a direct child throughout A's session-list CLI request. The shared
// runtime path makes the endpoint reachable, while the reported scope must make
// the session verb refuse before it can act on B.
fn scope_isolation_session_verbs_refuse_a_foreign_daemon() {
    if !thegn_core::testenv::require_or_skip("Unix-socket pane daemons", cfg!(unix)) {
        return;
    }

    let runtime = shared_runtime();
    let scope_a = Scope::new("tgsa");
    let scope_b = Scope::new("tgsb");
    let socket_a = predicted_endpoint(&scope_a, runtime.path());
    let mut daemon_b = start_foreign_at_a_endpoint(&scope_b, runtime.path(), &socket_a);

    let mut list_a = scope_a.command(runtime.path());
    list_a.args(["session", "list"]);
    let (status, stdout, stderr) = run_cli(list_a, Duration::from_secs(10));
    assert!(
        !status.success()
            && (stdout.contains("belongs to a different state directory")
                || stderr.contains("belongs to a different state directory")),
        "scope A session list must refuse the foreign daemon; stdout:\n{stdout}\nstderr:\n{stderr}"
    );
    assert_alive(
        &mut daemon_b,
        &format!(
            "scope A session list must preserve scope B; stdout:\n{stdout}\nstderr:\n{stderr}"
        ),
    );

    let mut stop_b = scope_b.command(runtime.path());
    stop_b.args(["daemon", "stop"]);
    let (status, stdout, stderr) = run_cli(stop_b, Duration::from_secs(10));
    assert!(
        status.success() && stdout.contains("stopped "),
        "scope B daemon stop should stop its daemon; stdout:\n{stdout}\nstderr:\n{stderr}"
    );
    assert_exits(
        &mut daemon_b,
        &format!("scope B daemon stop; stdout:\n{stdout}\nstderr:\n{stderr}"),
    );
}
