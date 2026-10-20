use std::fs::{self, File};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use thegn_core::config::{SandboxBackend, SandboxProfile};
use thegn_core::placement::Placement;
use thegn_core::remote::GitLoc;
use thegn_core::sandbox::{self, Backend};
use thegn_core::testenv::{IsolatedState, state_env_all};

const PROCESS_TIMEOUT: Duration = Duration::from_secs(10);
const OUTPUT_LIMIT: u64 = 64 * 1024;

struct Captured {
    status: ExitStatus,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
}

struct RunningDaemon {
    child: Child,
}

impl Drop for RunningDaemon {
    fn drop(&mut self) {
        let timeout = Duration::from_secs(2);
        if let Err(error) = self.child.kill()
            && error.kind() != std::io::ErrorKind::InvalidInput
        {
            let _write_result = writeln!(
                std::io::stderr().lock(),
                "security_e2e: could not kill daemon during cleanup: {error}"
            );
            return;
        }
        let deadline = Instant::now() + timeout;
        loop {
            match self.child.try_wait() {
                Ok(Some(_)) => return,
                Ok(None) if Instant::now() < deadline => thread::sleep(Duration::from_millis(20)),
                Ok(None) => {
                    let _write_result = writeln!(
                        std::io::stderr().lock(),
                        "security_e2e: daemon did not exit after kill within {timeout:?}"
                    );
                    return;
                }
                Err(error) => {
                    let _write_result = writeln!(
                        std::io::stderr().lock(),
                        "security_e2e: could not poll daemon during cleanup: {error}"
                    );
                    return;
                }
            }
        }
    }
}

fn spawn_captured(command: &mut Command, stdout: &Path, stderr: &Path) -> Child {
    let stdout_file = File::create(stdout).expect("create bounded stdout capture");
    let stderr_file = File::create(stderr).expect("create bounded stderr capture");
    command
        .stdout(Stdio::from(stdout_file))
        .stderr(Stdio::from(stderr_file))
        .spawn()
        .expect("spawn bounded child process")
}

fn spawn_quiet(command: &mut Command) -> Child {
    command
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn isolated daemon")
}

fn read_capped(path: &Path) -> Vec<u8> {
    let file = File::open(path).expect("open captured child output");
    let mut bytes = Vec::new();
    file.take(OUTPUT_LIMIT)
        .read_to_end(&mut bytes)
        .expect("read capped child output");
    bytes
}

fn enforce_capture_cap(path: &Path) {
    if let Ok(metadata) = fs::metadata(path)
        && metadata.len() > OUTPUT_LIMIT
    {
        let file = File::options()
            .write(true)
            .open(path)
            .expect("open child capture for truncation");
        file.set_len(OUTPUT_LIMIT)
            .expect("cap child output capture");
    }
}

fn stop_child_bounded(child: &mut Child, timeout: Duration) {
    if let Err(error) = child.kill()
        && error.kind() != std::io::ErrorKind::InvalidInput
    {
        panic!("could not kill child during bounded cleanup: {error}");
    }
    let deadline = Instant::now() + timeout;
    loop {
        if child.try_wait().expect("poll child cleanup").is_some() {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "child did not exit after kill within {timeout:?}"
        );
        thread::sleep(Duration::from_millis(20));
    }
}

fn wait_bounded(mut child: Child, stdout: &Path, stderr: &Path, timeout: Duration) -> Captured {
    let deadline = Instant::now() + timeout;
    loop {
        enforce_capture_cap(stdout);
        enforce_capture_cap(stderr);
        match child.try_wait().expect("poll bounded child process") {
            Some(status) => {
                return Captured {
                    status,
                    stdout: read_capped(stdout),
                    stderr: read_capped(stderr),
                };
            }
            None if Instant::now() < deadline => thread::sleep(Duration::from_millis(20)),
            None => {
                stop_child_bounded(&mut child, Duration::from_secs(2));
                panic!("child exceeded its {timeout:?} total deadline");
            }
        }
    }
}

fn run_bounded(
    command: &mut Command,
    capture_dir: &Path,
    label: &str,
    timeout: Duration,
) -> Captured {
    let stdout = capture_dir.join(format!("{label}.stdout"));
    let stderr = capture_dir.join(format!("{label}.stderr"));
    let child = spawn_captured(command, &stdout, &stderr);
    wait_bounded(child, &stdout, &stderr, timeout)
}

fn print_skip() {
    writeln!(
        std::io::stderr().lock(),
        "security_e2e: skipped; set THEGN_REQUIRE_SECURITY_TESTS=1 or run `just security-e2e`"
    )
    .expect("write security-e2e skip message");
}

fn required_prerequisites() -> bool {
    if std::env::var("THEGN_REQUIRE_SECURITY_TESTS").as_deref() != Ok("1") {
        print_skip();
        return false;
    }
    assert!(cfg!(target_os = "linux"), "security-e2e requires Linux");
    let Some(path) = std::env::var_os("PATH") else {
        panic!("security-e2e requires PATH to locate bwrap");
    };
    let Some(bwrap) = std::env::split_paths(&path)
        .map(|dir| dir.join("bwrap"))
        .find(|candidate| candidate.is_file())
    else {
        panic!("security-e2e requires bwrap on PATH");
    };
    let probe_dir = tempfile::tempdir().expect("private bwrap probe directory");
    let probe = run_bounded(
        Command::new(bwrap).args(["--unshare-user", "--ro-bind", "/", "/", "true"]),
        probe_dir.path(),
        "userns-probe",
        PROCESS_TIMEOUT,
    );
    assert!(
        probe.status.success(),
        "security-e2e requires working bwrap user namespaces; stderr: {}",
        String::from_utf8_lossy(&probe.stderr)
    );
    true
}

struct Fixture {
    state: IsolatedState,
    runtime: tempfile::TempDir,
    worktree: tempfile::TempDir,
    captures: tempfile::TempDir,
    _marker_dir: tempfile::TempDir,
    marker: PathBuf,
    socket: PathBuf,
    _daemon: RunningDaemon,
}

impl Fixture {
    fn new() -> Self {
        let state = IsolatedState::new();
        let runtime = tempfile::tempdir_in(state.root()).expect("private runtime directory");
        let worktree = tempfile::tempdir_in(state.root()).expect("private worktree directory");
        let captures = tempfile::tempdir_in(state.root()).expect("private capture directory");
        let marker_dir = tempfile::tempdir().expect("private host-only marker directory");
        let marker = marker_dir.path().join("marker");

        let init = run_bounded(
            thegn_core::util::git_cmd(worktree.path())
                .args(["-c", "commit.gpgsign=false", "init", "--quiet"])
                .envs(state_env_all(state.root()))
                .env("GIT_CONFIG_GLOBAL", "/dev/null"),
            captures.path(),
            "git-init",
            PROCESS_TIMEOUT,
        );
        assert!(
            init.status.success(),
            "git init failed: {}",
            String::from_utf8_lossy(&init.stderr)
        );

        let mut daemon_cmd = Command::new(env!("CARGO_BIN_EXE_thegn"));
        daemon_cmd
            .arg("daemon")
            .envs(state_env_all(state.root()))
            .env("XDG_RUNTIME_DIR", runtime.path())
            .env_remove("THEGN_LOG")
            .env_remove("THEGN_DATABASE_MIGRATION_EXECUTABLE")
            .env_remove("THEGN_DATABASE_MIGRATION_AUTHORITY");
        let child = spawn_quiet(&mut daemon_cmd);
        let daemon = RunningDaemon { child };
        let socket = wait_for_socket(runtime.path(), &daemon);

        Self {
            state,
            runtime,
            worktree,
            captures,
            _marker_dir: marker_dir,
            marker,
            socket,
            _daemon: daemon,
        }
    }

    fn child_env(&self, command: &mut Command) {
        command
            .envs(state_env_all(self.state.root()))
            .env("XDG_RUNTIME_DIR", self.runtime.path())
            .env_remove("THEGN_LOG")
            .env_remove("THEGN_DATABASE_MIGRATION_EXECUTABLE")
            .env_remove("THEGN_DATABASE_MIGRATION_AUTHORITY");
    }

    fn assert_no_sessions(&self, captured: &Captured) {
        let sessions: Vec<serde_json::Value> =
            serde_json::from_slice(&captured.stdout).expect("session list emits a JSON array");
        assert!(
            sessions.is_empty(),
            "daemon listed unexpected sessions: {sessions:?}"
        );
    }

    fn hardened_argv(&self, inner: &str) -> Vec<String> {
        let sandbox_config = thegn_core::config::SandboxConfig {
            enabled: true,
            backend: SandboxBackend::Bwrap,
            backend_chain: vec!["bwrap".into()],
            profile: SandboxProfile::Hardened,
            ..Default::default()
        };
        let location = GitLoc::from_db(
            self.worktree
                .path()
                .to_str()
                .expect("worktree path is UTF-8"),
            None,
        );
        let spec = sandbox::resolve_placed_exact(
            &sandbox_config,
            &location,
            "security-e2e",
            SandboxProfile::Hardened,
            Placement::Local,
        )
        .expect("production resolver builds bwrap hardened pane spec");
        assert_eq!(spec.backend, Backend::Bwrap);
        sandbox::enter_argv(&spec, inner).expect("production enter_argv builds hardened pane")
    }

    fn copy_binary(&self) {
        fs::copy(
            env!("CARGO_BIN_EXE_thegn"),
            self.worktree.path().join("thegn"),
        )
        .expect("copy thegn executable into writable pane worktree");
    }

    fn run_pane(&self, label: &str, inner: &str) -> Captured {
        self.copy_binary();
        let argv = self.hardened_argv(inner);
        let (program, args) = argv.split_first().expect("enter_argv returns a program");
        let mut command = Command::new(program);
        command.args(args).current_dir(self.worktree.path());
        self.child_env(&mut command);
        run_bounded(&mut command, self.captures.path(), label, PROCESS_TIMEOUT)
    }

    fn probe_config(&self) -> (PathBuf, PathBuf) {
        let config_home = self.worktree.path().join("probe-config");
        let config_dir = config_home.join("thegn");
        fs::create_dir_all(&config_dir).expect("create in-worktree config directory");
        let config_path = config_dir.join("config.toml");
        let socket = self.socket.to_string_lossy();
        fs::write(&config_path, format!("[daemon]\nsocket = {socket:?}\n"))
            .expect("write probe config");
        (config_home, config_path)
    }

    fn host_sessions_with_config(&self, label: &str, config_home: &Path) -> Captured {
        let mut command = Command::new(env!("CARGO_BIN_EXE_thegn"));
        command.args(["session", "list", "--json"]);
        self.child_env(&mut command);
        command.env("XDG_CONFIG_HOME", config_home);
        let result = run_bounded(&mut command, self.captures.path(), label, PROCESS_TIMEOUT);
        assert!(
            result.status.success(),
            "host session list with probe config failed: stdout: {} stderr: {}",
            String::from_utf8_lossy(&result.stdout),
            String::from_utf8_lossy(&result.stderr)
        );
        result
    }
}

fn wait_for_socket(runtime: &Path, daemon: &RunningDaemon) -> PathBuf {
    let deadline = Instant::now() + PROCESS_TIMEOUT;
    loop {
        if let Some(socket) = find_socket(runtime) {
            return socket;
        }
        if let Some(status) = daemon.child.try_wait().expect("poll isolated daemon") {
            panic!("isolated daemon exited before creating its socket ({status})");
        }
        assert!(
            Instant::now() < deadline,
            "daemon did not create its socket within 10 seconds"
        );
        thread::sleep(Duration::from_millis(20));
    }
}

fn find_socket(root: &Path) -> Option<PathBuf> {
    for entry in fs::read_dir(root).ok()? {
        let path = entry.ok()?.path();
        if path.file_name().is_some_and(|name| name == "daemon.sock") {
            return Some(path);
        }
        if path.is_dir()
            && let Some(socket) = find_socket(&path)
        {
            return Some(socket);
        }
    }
    None
}

#[test]
fn security_e2e_hardened_pane_cannot_stop_daemon() {
    if !required_prerequisites() {
        return;
    }
    let fixture = Fixture::new();
    let (config_home, _) = fixture.probe_config();
    let inner = format!(
        "XDG_CONFIG_HOME={} ./thegn daemon stop",
        shell_quote(config_home.to_str().expect("config home path is UTF-8"))
    );
    let host = fixture.host_sessions_with_config("host-stop-control", &config_home);
    fixture.assert_no_sessions(&host);
    let version = fixture.run_pane("version-control", "./thegn --version");
    assert!(
        version.status.success(),
        "in-pane thegn --version failed: stdout: {} stderr: {}",
        String::from_utf8_lossy(&version.stdout),
        String::from_utf8_lossy(&version.stderr)
    );
    let probe = fixture.run_pane("stop-probe", &inner);
    assert!(
        !probe.status.success(),
        "hardened pane reached the daemon control socket; stdout: {} stderr: {}",
        String::from_utf8_lossy(&probe.stdout),
        String::from_utf8_lossy(&probe.stderr)
    );
    let probe_output = format!(
        "{}{}",
        String::from_utf8_lossy(&probe.stdout),
        String::from_utf8_lossy(&probe.stderr)
    );
    assert!(
        probe_output.contains("no thegn pane daemon is running"),
        "probe failed for an unrelated reason: {probe_output}"
    );
    let host = fixture.host_sessions_with_config("sessions-after-stop", &config_home);
    fixture.assert_no_sessions(&host);
}

#[test]
fn security_e2e_hardened_pane_cannot_open_host_session() {
    if !required_prerequisites() {
        return;
    }
    let fixture = Fixture::new();
    let (config_home, config_path) = fixture.probe_config();
    let socket = fixture.socket.to_string_lossy();
    let command = format!(
        "touch {}",
        shell_quote(fixture.marker.to_str().expect("marker path is UTF-8"))
    );
    fs::write(
        &config_path,
        format!(
            "[daemon]\nsocket = {socket:?}\n\n[[agents]]\nname = \"probe\"\ncommand = {command:?}\n"
        ),
    )
    .expect("write attack config");
    let host = fixture.host_sessions_with_config("host-open-control", &config_home);
    fixture.assert_no_sessions(&host);
    let version = fixture.run_pane("open-version-control", "./thegn --version");
    assert!(
        version.status.success(),
        "in-pane thegn --version failed: stdout: {} stderr: {}",
        String::from_utf8_lossy(&version.stdout),
        String::from_utf8_lossy(&version.stderr)
    );
    let inner = format!(
        "XDG_CONFIG_HOME={} ./thegn session open --agent probe --worktree . --prompt x",
        shell_quote(config_home.to_str().expect("config home path is UTF-8"))
    );
    let probe = fixture.run_pane("open-probe", &inner);
    assert!(
        !probe.status.success(),
        "hardened pane opened a host session; stdout: {} stderr: {}",
        String::from_utf8_lossy(&probe.stdout),
        String::from_utf8_lossy(&probe.stderr)
    );
    let probe_output = format!(
        "{}{}",
        String::from_utf8_lossy(&probe.stdout),
        String::from_utf8_lossy(&probe.stderr)
    );
    assert!(
        probe_output.contains("no thegn pane daemon is running"),
        "probe failed for an unrelated reason: {probe_output}"
    );
    let deadline = Instant::now() + Duration::from_secs(2);
    while fixture.marker.exists() && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(20));
    }
    assert!(!fixture.marker.exists(), "host-only marker was created");
    let host = fixture.host_sessions_with_config("sessions-after-open", &config_home);
    fixture.assert_no_sessions(&host);
}

fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}
