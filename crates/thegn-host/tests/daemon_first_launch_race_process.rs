use std::fs::{self, File};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use thegn_core::config::DaemonConfig;
use thegn_core::testenv::state_env_all;

const CAPTURE_LIMIT: u64 = 64 * 1024;
const CHILD_COUNT: usize = 4;
const RACE_DEADLINE: Duration = Duration::from_secs(15);

struct Scope {
    root: tempfile::TempDir,
}

impl Scope {
    fn new() -> Scope {
        Scope {
            root: tempfile::Builder::new()
                .prefix("tgfr")
                .tempdir_in("/tmp")
                .expect("create short isolated daemon scope"),
        }
    }

    fn command(&self, runtime: &Path) -> Command {
        let mut command = self.base_command();
        command.env("XDG_RUNTIME_DIR", runtime);
        command
    }

    fn command_without_runtime(&self) -> Command {
        self.base_command()
    }

    fn base_command(&self) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_thegn"));
        command
            .env_clear()
            .current_dir(self.root.path())
            .envs(state_env_all(self.root.path()))
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

fn fallback_endpoints(scope: &Scope) -> [PathBuf; 2] {
    let state_dir = scope.root.path().join("state").join("thegn");
    let key = thegn_core::config_daemon::state_endpoint_key(&state_dir);
    let euid = thegn_svc::ipc::effective_uid().expect("effective uid for Unix daemon test");
    [
        PathBuf::from(format!("/run/user/{euid}/thegn-{key}.sock")),
        PathBuf::from(format!("/tmp/thegn-{euid}-{key}/daemon.sock")),
    ]
}

struct OwnedDaemon {
    child: Child,
    stdout_path: PathBuf,
    stderr_path: PathBuf,
}

impl Drop for OwnedDaemon {
    #[expect(
        clippy::disallowed_methods,
        reason = "process test owns and reaps every daemon child"
    )]
    fn drop(&mut self) {
        if let Err(error) = self.child.kill()
            && error.kind() != std::io::ErrorKind::InvalidInput
        {
            let _write_result = writeln!(
                std::io::stderr().lock(),
                "daemon_first_launch_race_process: could not kill daemon during cleanup: {error}"
            );
        }
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            match self.child.try_wait() {
                Ok(Some(_)) => return,
                Ok(None) if Instant::now() < deadline => thread::sleep(Duration::from_millis(10)),
                Ok(None) => {
                    let _status = self
                        .child
                        .wait()
                        .expect("reap daemon after cleanup timeout");
                    return;
                }
                Err(error) => {
                    let _write_result = writeln!(
                        std::io::stderr().lock(),
                        "daemon_first_launch_race_process: could not poll daemon during cleanup: {error}"
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

fn spawn_daemon(mut command: Command, scope: &Scope, index: usize) -> OwnedDaemon {
    let stdout_path = scope.root.path().join(format!("daemon-{index}.stdout"));
    let stderr_path = scope.root.path().join(format!("daemon-{index}.stderr"));
    let stdout = File::create(&stdout_path).expect("create daemon stdout capture");
    let stderr = File::create(&stderr_path).expect("create daemon stderr capture");
    command
        .arg("daemon")
        .stdout(Stdio::from(stdout))
        .stderr(Stdio::from(stderr));
    let child = command.spawn().expect("spawn isolated pane daemon");
    OwnedDaemon {
        child,
        stdout_path,
        stderr_path,
    }
}

#[expect(
    clippy::disallowed_methods,
    reason = "process test owns and reaps the CLI child"
)]
fn run_cli(mut command: Command, timeout: Duration) -> (ExitStatus, String, String) {
    let captures = tempfile::Builder::new()
        .prefix("tgfr")
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

struct EndpointCleanup([PathBuf; 2]);

impl Drop for EndpointCleanup {
    fn drop(&mut self) {
        for endpoint in &self.0 {
            let lock = endpoint.with_extension("sock.lock");
            for path in [endpoint.as_path(), lock.as_path()] {
                if let Err(error) = fs::remove_file(path)
                    && error.kind() != std::io::ErrorKind::NotFound
                {
                    let _write_result = writeln!(
                        std::io::stderr().lock(),
                        "daemon_first_launch_race_process: could not remove fallback endpoint {}: {error}",
                        path.display()
                    );
                }
            }
            let Some(euid) = thegn_svc::ipc::effective_uid() else {
                continue;
            };
            let Some(parent) = endpoint.parent() else {
                continue;
            };
            let expected_prefix = format!("thegn-{euid}-");
            if parent.parent() == Some(Path::new("/tmp"))
                && parent
                    .file_name()
                    .is_some_and(|name| name.to_string_lossy().starts_with(&expected_prefix))
                && let Err(error) = fs::remove_dir(parent)
                && error.kind() != std::io::ErrorKind::NotFound
            {
                let _write_result = writeln!(
                    std::io::stderr().lock(),
                    "daemon_first_launch_race_process: could not remove fallback directory {}: {error}",
                    parent.display()
                );
            }
        }
    }
}

mod unix_tests {
    use super::*;

    fn runtime_directory() -> tempfile::TempDir {
        let runtime = tempfile::Builder::new()
            .prefix("tgfr")
            .tempdir_in("/tmp")
            .expect("create short shared runtime directory");
        chmod_private(runtime.path());
        let daemon_dir = runtime.path().join("thegn");
        fs::create_dir(&daemon_dir).expect("create runtime daemon directory");
        chmod_private(&daemon_dir);
        runtime
    }

    fn chmod_private(path: &Path) {
        let mut command = Command::new("chmod");
        command.arg("700").arg(path);
        let (status, stdout, stderr) = run_cli(command, Duration::from_secs(5));
        assert!(
            status.success(),
            "chmod 700 {} failed; stdout:\n{stdout}\nstderr:\n{stderr}",
            path.display()
        );
    }

    // Four direct child processes share one isolated state scope. Each starts
    // without waiting for another; one child owns the listener and its peers
    // either observe that listener or receive the allocator's bounded refusal.
    // Polling is the only synchronization: the 15-second deadline is generous
    // under load, and no child ordering or winner identity is assumed.
    #[test]
    fn daemon_first_launch_race_converges_with_a_runtime_dir() {
        if !thegn_core::testenv::require_or_skip("Unix-socket pane daemons", cfg!(unix)) {
            return;
        }
        let scope = Scope::new();
        let runtime = runtime_directory();
        let endpoint = predicted_endpoint(&scope, runtime.path());
        run_first_launch_race(&scope, Some(runtime.path()), vec![endpoint]);
    }

    // The same process race uses only the runtime paths selected by the daemon:
    // /run/user/<uid> or its private /tmp fallback. Cleanup owns both candidate
    // names so a failed assertion cannot leave a socket, lock, or fallback dir.
    #[test]
    fn daemon_first_launch_race_converges_without_a_runtime_dir() {
        if !thegn_core::testenv::require_or_skip("Unix-socket pane daemons", cfg!(unix)) {
            return;
        }
        let scope = Scope::new();
        let candidates = fallback_endpoints(&scope);
        let _cleanup = EndpointCleanup(candidates.clone());
        run_first_launch_race(&scope, None, candidates.into());
    }

    fn run_first_launch_race(scope: &Scope, runtime: Option<&Path>, candidates: Vec<PathBuf>) {
        let mut children = Vec::with_capacity(CHILD_COUNT);
        for index in 0..CHILD_COUNT {
            let command = match runtime {
                Some(runtime) => scope.command(runtime),
                None => scope.command_without_runtime(),
            };
            children.push(spawn_daemon(command, scope, index));
        }

        let deadline = Instant::now() + RACE_DEADLINE;
        let mut socket_inode = None;
        let (endpoint, statuses) = loop {
            let endpoint = candidates
                .iter()
                .find(|candidate| candidate.exists())
                .cloned();
            if let Some(endpoint) = endpoint.as_ref()
                && socket_inode.is_none()
            {
                let (is_socket, inode) = endpoint_identity(endpoint);
                assert!(
                    is_socket,
                    "first launch endpoint must be a Unix socket: {}",
                    endpoint.display()
                );
                socket_inode = Some(inode);
            }

            let mut statuses = Vec::with_capacity(children.len());
            let mut running = 0_usize;
            for daemon in &mut children {
                let status = daemon
                    .child
                    .try_wait()
                    .expect("poll first-launch daemon child");
                if status.is_none() {
                    running += 1;
                }
                statuses.push(status);
            }
            if let Some(endpoint) = endpoint
                && running == 1
            {
                break (endpoint, statuses);
            }
            assert!(
                Instant::now() < deadline,
                "first-launch race did not converge before {RACE_DEADLINE:?}; {}",
                child_report(&children, &statuses)
            );
            thread::sleep(Duration::from_millis(10));
        };

        if runtime.is_none() {
            assert_eq!(
                candidates
                    .iter()
                    .filter(|candidate| candidate.exists())
                    .count(),
                1,
                "exactly one fallback endpoint must exist; {}",
                child_report(&children, &statuses)
            );
        }
        let winner = statuses
            .iter()
            .position(Option::is_none)
            .expect("one daemon child must remain alive");
        for (index, status) in statuses.iter().enumerate() {
            if index == winner {
                assert!(status.is_none(), "winner must still be running");
                continue;
            }
            let status = status.as_ref().expect("race loser must have exited");
            let stderr = read_capped(&children[index].stderr_path);
            assert!(
                status.success() || is_typed_race_refusal(status, &stderr),
                "race loser must exit successfully or report a typed refusal; {}",
                child_report(&children, &statuses)
            );
        }

        let (is_socket, inode) = endpoint_identity(&endpoint);
        assert!(
            is_socket,
            "surviving endpoint must remain a Unix socket: {}",
            endpoint.display()
        );
        assert_eq!(
            Some(inode),
            socket_inode,
            "losers must not replace the endpoint inode; {}",
            child_report(&children, &statuses)
        );
        if let Some(runtime) = runtime {
            let sockets = socket_files(&runtime.join("thegn"));
            assert_eq!(
                sockets,
                vec![endpoint.clone()],
                "runtime tree must contain exactly the one authentic socket"
            );
        }

        let mut list = match runtime {
            Some(runtime) => scope.command(runtime),
            None => scope.command_without_runtime(),
        };
        list.args(["session", "list", "--json"]);
        let (status, stdout, stderr) = run_cli(list, Duration::from_secs(30));
        let parsed: serde_json::Value = serde_json::from_str(&stdout).unwrap_or_else(|error| {
            panic!("session list returned invalid JSON: {error}; stderr:\n{stderr}")
        });
        assert!(
            status.success() && parsed.is_array(),
            "CLI call through winning endpoint must return a JSON array; status={status}, stdout:\n{stdout}\nstderr:\n{stderr}"
        );

        let mut stop = match runtime {
            Some(runtime) => scope.command(runtime),
            None => scope.command_without_runtime(),
        };
        stop.args(["daemon", "stop"]);
        let (stop_status, stop_stdout, stop_stderr) = run_cli(stop, Duration::from_secs(30));
        assert!(
            stop_status.success(),
            "daemon stop command must succeed; stdout:\n{stop_stdout}\nstderr:\n{stop_stderr}"
        );
        let survivor = &mut children[winner];
        let exit_deadline = Instant::now() + Duration::from_secs(10);
        let survivor_status = loop {
            match survivor.child.try_wait() {
                Ok(Some(status)) => break status,
                Ok(None) if Instant::now() < exit_deadline => {
                    thread::sleep(Duration::from_millis(10));
                }
                Ok(None) => panic!("stopped daemon did not exit before deadline"),
                Err(error) => panic!("could not poll stopped daemon: {error}"),
            }
        };
        let survivor_stderr = read_capped(&survivor.stderr_path);
        assert!(
            survivor_status.success(),
            "stopped daemon must exit successfully; status={survivor_status}; stderr:\n{survivor_stderr}"
        );
        assert!(
            !endpoint.exists(),
            "stopped daemon must remove endpoint {}",
            endpoint.display()
        );
    }

    fn is_typed_race_refusal(status: &ExitStatus, stderr: &str) -> bool {
        !status.success()
            && ((stderr.contains("daemon allocation lock") && stderr.contains("held by another"))
                || (stderr.contains("existing control socket did not answer the probe")
                    && stderr.contains("refusing to replace it"))
                || stderr.contains("control socket lock is held by another process"))
    }

    #[expect(
        clippy::disallowed_methods,
        reason = "process test reads Unix socket type and inode with the system listing utility"
    )]
    fn endpoint_identity(endpoint: &Path) -> (bool, u64) {
        let inode_output = Command::new("ls")
            .arg("-di")
            .arg(endpoint)
            .output()
            .expect("run ls to read Unix endpoint inode");
        assert!(
            inode_output.status.success(),
            "ls could not read endpoint inode {}: {}",
            endpoint.display(),
            String::from_utf8_lossy(&inode_output.stderr)
        );
        let inode_text = String::from_utf8_lossy(&inode_output.stdout);
        let inode = inode_text
            .split_whitespace()
            .next()
            .expect("ls inode output has a leading number")
            .parse::<u64>()
            .expect("ls inode output begins with an inode number");

        let mode_output = Command::new("ls")
            .args(["-ld"])
            .arg(endpoint)
            .output()
            .expect("run ls to read Unix endpoint type");
        assert!(
            mode_output.status.success(),
            "ls could not read endpoint type {}: {}",
            endpoint.display(),
            String::from_utf8_lossy(&mode_output.stderr)
        );
        let mode_text = String::from_utf8_lossy(&mode_output.stdout);
        let mode = mode_text
            .split_whitespace()
            .next()
            .expect("ls mode output has a leading file mode");
        (mode.starts_with('s'), inode)
    }

    fn child_report(children: &[OwnedDaemon], statuses: &[Option<ExitStatus>]) -> String {
        children
            .iter()
            .zip(statuses)
            .enumerate()
            .map(|(index, (daemon, status))| {
                format!(
                    "child {index}: status={:?}\nstdout:\n{}\nstderr:\n{}",
                    status,
                    read_capped(&daemon.stdout_path),
                    read_capped(&daemon.stderr_path)
                )
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn socket_files(root: &Path) -> Vec<PathBuf> {
        let mut sockets = Vec::new();
        let mut pending = vec![root.to_path_buf()];
        while let Some(directory) = pending.pop() {
            for entry in fs::read_dir(&directory).expect("walk runtime socket tree") {
                let path = entry.expect("read runtime directory entry").path();
                let metadata = fs::symlink_metadata(&path).expect("read runtime entry metadata");
                if metadata.file_type().is_dir() {
                    pending.push(path);
                } else if path
                    .extension()
                    .is_some_and(|extension| extension == "sock")
                    && endpoint_identity(&path).0
                {
                    sockets.push(path);
                }
            }
        }
        sockets.sort();
        sockets
    }
}
