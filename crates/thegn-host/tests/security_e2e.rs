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
const BUS_SENTINEL: &str = "unix:path=/nonexistent/tg-sec-bus-sentinel";

struct Captured {
    program: String,
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

fn wait_bounded(
    mut child: Child,
    program: String,
    stdout: &Path,
    stderr: &Path,
    timeout: Duration,
) -> Captured {
    let deadline = Instant::now() + timeout;
    loop {
        enforce_capture_cap(stdout);
        enforce_capture_cap(stderr);
        match child.try_wait().expect("poll bounded child process") {
            Some(status) => {
                return Captured {
                    program,
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
    let program = command.get_program().to_string_lossy().into_owned();
    let child = spawn_captured(command, &stdout, &stderr);
    wait_bounded(child, program, &stdout, &stderr, timeout)
}

fn required_prerequisites() -> bool {
    if !thegn_core::testenv::require_or_skip(
        "THEGN_REQUIRE_SECURITY_TESTS=1",
        std::env::var("THEGN_REQUIRE_SECURITY_TESTS").as_deref() == Ok("1"),
    ) {
        return false;
    }
    if !cfg!(target_os = "linux") {
        panic!("security-e2e requires Linux");
    }
    let Some(path) = std::env::var_os("PATH") else {
        panic!("security-e2e requires PATH to locate bwrap");
    };
    let Some(bwrap) = std::env::split_paths(&path)
        .map(|dir| dir.join("bwrap"))
        .find(|candidate| candidate.is_file())
    else {
        if !thegn_core::testenv::require_or_skip("bwrap on PATH", false) {
            return false;
        }
        unreachable!("required_or_skip must reject unavailable bwrap");
    };
    let probe_dir = tempfile::tempdir().expect("private bwrap probe directory");
    let probe = run_bounded(
        Command::new(bwrap).args(["--unshare-user", "--ro-bind", "/", "/", "true"]),
        probe_dir.path(),
        "userns-probe",
        PROCESS_TIMEOUT,
    );
    if !probe.status.success()
        && !thegn_core::testenv::require_or_skip("working bwrap user namespaces", false)
    {
        return false;
    }
    let config = thegn_core::config::SandboxConfig {
        enabled: true,
        backend: SandboxBackend::Bwrap,
        backend_chain: vec!["bwrap".into()],
        profile: SandboxProfile::Hardened,
        ..Default::default()
    };
    let location = GitLoc::from_db(
        probe_dir
            .path()
            .to_str()
            .expect("bwrap probe path is UTF-8"),
        None,
    );
    let resolves_bwrap = sandbox::resolve_placed_exact(
        &config,
        &location,
        "security-e2e-prerequisite",
        SandboxProfile::Hardened,
        Placement::Local,
    )
    .is_some_and(|spec| spec.backend == Backend::Bwrap);
    if !resolves_bwrap
        && !thegn_core::testenv::require_or_skip("bwrap hardened pane backend resolution", false)
    {
        return false;
    }
    true
}

struct Fixture {
    state: IsolatedState,
    /// Keep the daemon runtime outside isolated HOME: hardened panes bind HOME
    /// read-only, and Unix socket connects still work through that bind. In
    /// production the runtime is under `/run/user/<uid>`, which bwrap does not mount.
    runtime: Option<tempfile::TempDir>,
    worktree: tempfile::TempDir,
    captures: tempfile::TempDir,
    _marker_dir: tempfile::TempDir,
    marker: PathBuf,
    socket: PathBuf,
    daemon: RunningDaemon,
    _endpoint_cleanup: Option<EndpointCleanup>,
    endpoint_layout: &'static str,
}

struct EndpointCleanup(PathBuf);

impl Drop for EndpointCleanup {
    fn drop(&mut self) {
        let lock = self.0.with_extension("sock.lock");
        for path in [self.0.as_path(), lock.as_path()] {
            if let Err(error) = fs::remove_file(path)
                && error.kind() != std::io::ErrorKind::NotFound
            {
                let _write_result = writeln!(
                    std::io::stderr().lock(),
                    "security_e2e: could not remove fallback endpoint {}: {error}",
                    path.display()
                );
            }
        }
        let Some(euid) = thegn_svc::ipc::effective_uid() else {
            return;
        };
        let Some(parent) = self.0.parent() else {
            return;
        };
        let expected_prefix = format!("thegn-{euid}-");
        if parent.parent() == Some(Path::new("/tmp"))
            && parent
                .file_name()
                .is_some_and(|name| name.to_string_lossy().starts_with(&expected_prefix))
            && let Err(error) = fs::remove_dir(parent)
        {
            let _write_result = writeln!(
                std::io::stderr().lock(),
                "security_e2e: could not remove fallback endpoint directory {}: {error}",
                parent.display()
            );
        }
    }
}

/// The test launches panes with a private XDG_RUNTIME_DIR that has no user
/// bus, so the host CPU-cap scope wrapper cannot run there. That cap is a
/// resource ceiling, not containment; bwrap remains the boundary under test.
/// A nice/ionice soft-cap prefix needs no bus and is left intact.
fn pane_argv_for_isolated_runtime(argv: Vec<String>) -> Vec<String> {
    thegn_core::sandbox_cpucap::strip_pane_cap(&argv).to_vec()
}

#[test]
fn security_e2e_pane_argv_strips_only_the_cpu_cap() {
    let capped = [
        "systemd-run",
        "--user",
        "--scope",
        "--quiet",
        "--collect",
        "--slice=thegn.slice",
        "-p",
        "CPUQuota=800%",
        "--",
        "bwrap",
        "--unshare-user",
        "--",
        "sh",
        "-lc",
        "true",
    ]
    .map(str::to_owned)
    .to_vec();
    let peeled = pane_argv_for_isolated_runtime(capped);
    assert_eq!(
        peeled,
        ["bwrap", "--unshare-user", "--", "sh", "-lc", "true"]
            .map(str::to_owned)
            .to_vec()
    );
    assert_eq!(thegn_core::sandbox_truth::observed(&peeled), Backend::Bwrap);

    let soft_capped = ["nice", "-n", "10", "bwrap", "--", "sh"]
        .map(str::to_owned)
        .to_vec();
    assert_eq!(
        pane_argv_for_isolated_runtime(soft_capped.clone()),
        soft_capped
    );

    let bare = ["bwrap", "--", "sh"].map(str::to_owned).to_vec();
    assert_eq!(pane_argv_for_isolated_runtime(bare.clone()), bare);
    // Soft-cap observation is pinned in thegn-core `sandbox_truth_tests`.
}

impl Fixture {
    fn new() -> Self {
        Self::build(true)
    }

    fn new_without_runtime_dir() -> Self {
        Self::build(false)
    }

    fn build(with_runtime_dir: bool) -> Self {
        let state = IsolatedState::new();
        let runtime = with_runtime_dir.then(|| {
            tempfile::Builder::new()
                .prefix("tg-sec-")
                .tempdir_in("/tmp")
                .expect("private runtime directory outside isolated HOME")
        });
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
            .env_remove("THEGN_LOG")
            .env_remove("THEGN_DATABASE_MIGRATION_EXECUTABLE")
            .env_remove("THEGN_DATABASE_MIGRATION_AUTHORITY");
        if let Some(runtime) = runtime.as_ref() {
            daemon_cmd.env("XDG_RUNTIME_DIR", runtime.path());
        } else {
            daemon_cmd.env_remove("XDG_RUNTIME_DIR");
        }
        let child = spawn_quiet(&mut daemon_cmd);
        let mut daemon = RunningDaemon { child };
        let socket = if let Some(runtime) = runtime.as_ref() {
            wait_for_socket(runtime.path(), &mut daemon)
        } else {
            let euid = thegn_svc::ipc::effective_uid().expect("effective uid on Linux");
            let state_dir = state.root().join("state").join("thegn");
            let key = thegn_core::config_daemon::state_endpoint_key(&state_dir);
            let candidates = [
                PathBuf::from(format!("/run/user/{euid}/thegn-{key}.sock")),
                PathBuf::from(format!("/tmp/thegn-{euid}-{key}/daemon.sock")),
            ];
            wait_for_endpoint(&candidates, &mut daemon)
        };
        assert!(
            !socket.starts_with(state.root()),
            "daemon socket {} must stay outside isolated HOME/state because hardened panes bind HOME read-only and Unix socket connects still work through that bind",
            socket.display()
        );
        let endpoint_cleanup = (!with_runtime_dir).then(|| EndpointCleanup(socket.clone()));
        let endpoint_layout = if with_runtime_dir {
            "isolated XDG_RUNTIME_DIR"
        } else if socket.starts_with("/run/user") {
            "/run/user short-runtime fallback"
        } else {
            "private /tmp fallback"
        };

        Self {
            state,
            runtime,
            worktree,
            captures,
            _marker_dir: marker_dir,
            marker,
            socket,
            daemon,
            _endpoint_cleanup: endpoint_cleanup,
            endpoint_layout,
        }
    }

    fn assert_daemon_alive(&mut self, context: &str) {
        assert!(
            self.daemon
                .child
                .try_wait()
                .expect("poll isolated daemon")
                .is_none(),
            "isolated daemon exited {context}"
        );
        assert!(
            self.socket.exists(),
            "isolated daemon socket disappeared {context}: {}",
            self.socket.display()
        );
    }

    fn child_env(&self, command: &mut Command) {
        command.envs(state_env_all(self.state.root()));
        if let Some(runtime) = self.runtime.as_ref() {
            command.env("XDG_RUNTIME_DIR", runtime.path());
        } else {
            command.env_remove("XDG_RUNTIME_DIR");
        }
        command
            // Never pass the developer's real bus to a security probe. The
            // sentinel also reaches fixture daemon and CLI children; a daemon
            // session-spawn test must account for it because it is unreachable.
            .env("DBUS_SESSION_BUS_ADDRESS", BUS_SENTINEL)
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
        let argv = pane_argv_for_isolated_runtime(
            sandbox::enter_argv(&spec, inner).expect("production enter_argv builds hardened pane"),
        );
        assert_eq!(
            thegn_core::sandbox_truth::observed(&argv),
            Backend::Bwrap,
            "hardened pane argv must enter bwrap: {argv:?}"
        );
        argv
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

fn wait_for_endpoint(candidates: &[PathBuf], daemon: &mut RunningDaemon) -> PathBuf {
    let deadline = Instant::now() + PROCESS_TIMEOUT;
    loop {
        if let Some(endpoint) = candidates.iter().find(|candidate| candidate.exists()) {
            return endpoint.clone();
        }
        if let Some(status) = daemon.child.try_wait().expect("poll isolated daemon") {
            panic!("isolated daemon exited before creating its socket ({status})");
        }
        assert!(
            Instant::now() < deadline,
            "daemon did not create its fallback socket within 10 seconds"
        );
        thread::sleep(Duration::from_millis(20));
    }
}

fn wait_for_socket(runtime: &Path, daemon: &mut RunningDaemon) -> PathBuf {
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
    let mut fixture = Fixture::new();
    let (config_home, _) = fixture.probe_config();
    let config_home_env = shell_quote(config_home.to_str().expect("config home path is UTF-8"));
    let inner = format!("env XDG_CONFIG_HOME={config_home_env} ./thegn daemon stop");
    let host = fixture.host_sessions_with_config("host-stop-control", &config_home);
    fixture.assert_no_sessions(&host);
    let version = fixture.run_pane("version-control", "./thegn --version");
    assert!(
        version.status.success(),
        "in-pane thegn --version failed (program {}): stdout: {} stderr: {}",
        version.program,
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
        !probe_output.contains("stopped ")
            && !probe_output.contains("shutdown refused for ")
            && !probe_output.contains("still running after "),
        "hardened pane reached a daemon listener; these messages are emitted only after a listener is reached: {probe_output}"
    );
    assert!(
        probe_output.contains("no thegn pane daemon is running")
            || probe_output.contains("listener registry could not be read"),
        "expected the hardened pane's fail-closed daemon refusal (accepted: `no thegn pane daemon is running` when no daemon is reachable, or `listener registry could not be read` when protected state prevents registry access): {probe_output}"
    );
    fixture.assert_daemon_alive("after the stop probe");
    let host = fixture.host_sessions_with_config("sessions-after-stop", &config_home);
    fixture.assert_no_sessions(&host);
}

#[test]
fn security_e2e_hardened_pane_cannot_reach_a_daemon_started_without_xdg_runtime_dir() {
    // The fixture owns one foreground daemon child; each host/pane probe is a
    // separate bounded CLI child, and the fallback poll uses the shared 20 ms
    // interval plus PROCESS_TIMEOUT. The isolated state key makes its candidate
    // endpoints distinct from other fixtures; bwrap exposes neither candidate.
    if !required_prerequisites() {
        return;
    }
    let mut fixture = Fixture::new_without_runtime_dir();
    let (config_home, _) = fixture.probe_config();
    let layout = fixture.endpoint_layout;
    assert!(
        !fixture.socket.starts_with(fixture.state.root()),
        "{layout} endpoint {} must be outside isolated HOME/state",
        fixture.socket.display()
    );
    let host = fixture.host_sessions_with_config("no-runtime-host-control", &config_home);
    fixture.assert_no_sessions(&host);

    let version = fixture.run_pane("no-runtime-version-control", "./thegn --version");
    assert!(
        version.status.success(),
        "in-pane thegn --version failed for {layout} (program {}): stdout: {} stderr: {}",
        version.program,
        String::from_utf8_lossy(&version.stdout),
        String::from_utf8_lossy(&version.stderr)
    );

    let default_probe = fixture.run_pane("no-runtime-default-stop-probe", "./thegn daemon stop");
    assert_fail_closed_daemon_probe(&default_probe, layout, "default endpoint resolution");

    let config_home_env = shell_quote(config_home.to_str().expect("config home path is UTF-8"));
    let explicit_inner = format!("env XDG_CONFIG_HOME={config_home_env} ./thegn daemon stop");
    let explicit_probe = fixture.run_pane("no-runtime-explicit-stop-probe", &explicit_inner);
    assert_fail_closed_daemon_probe(&explicit_probe, layout, "explicit socket configuration");

    fixture.assert_daemon_alive(&format!("after {layout} pane probes"));
    let host = fixture.host_sessions_with_config("no-runtime-sessions-after-stop", &config_home);
    fixture.assert_no_sessions(&host);
}

fn assert_fail_closed_daemon_probe(probe: &Captured, layout: &str, route: &str) {
    assert!(
        !probe.status.success(),
        "hardened pane reached the daemon through {route} at {layout}; stdout: {} stderr: {}",
        String::from_utf8_lossy(&probe.stdout),
        String::from_utf8_lossy(&probe.stderr)
    );
    let probe_output = format!(
        "{}{}",
        String::from_utf8_lossy(&probe.stdout),
        String::from_utf8_lossy(&probe.stderr)
    );
    assert!(
        !probe_output.contains("stopped ")
            && !probe_output.contains("shutdown refused for ")
            && !probe_output.contains("still running after "),
        "hardened pane reached a listener through {route} at {layout}: {probe_output}"
    );
    assert!(
        probe_output.contains("no thegn pane daemon is running")
            || probe_output.contains("listener registry could not be read"),
        "expected the hardened pane's fail-closed daemon refusal through {route} at {layout}: {probe_output}"
    );
}

#[test]
fn security_e2e_hardened_pane_cannot_open_host_session() {
    if !required_prerequisites() {
        return;
    }
    let mut fixture = Fixture::new();
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
        "in-pane thegn --version failed (program {}): stdout: {} stderr: {}",
        version.program,
        String::from_utf8_lossy(&version.stdout),
        String::from_utf8_lossy(&version.stderr)
    );
    let config_home_env = shell_quote(config_home.to_str().expect("config home path is UTF-8"));
    let inner = format!(
        "env XDG_CONFIG_HOME={config_home_env} ./thegn session open --agent probe --worktree . --prompt x"
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
    fixture.assert_daemon_alive("after the open probe");
}

#[test]
fn security_e2e_hardened_pane_does_not_inherit_the_session_bus_address() {
    if !required_prerequisites() {
        return;
    }
    let fixture = Fixture::new();
    let probe = fixture.run_pane(
        "bus-env",
        "printenv DBUS_SESSION_BUS_ADDRESS || echo tg-no-bus-address",
    );
    let stdout = String::from_utf8_lossy(&probe.stdout);
    let stderr = String::from_utf8_lossy(&probe.stderr);
    assert!(
        probe.status.success(),
        "session bus environment probe failed: stdout: {stdout} stderr: {stderr}"
    );
    assert!(
        stdout.contains("tg-no-bus-address"),
        "hardened pane retained a session bus address: stdout: {stdout} stderr: {stderr}"
    );
    assert!(
        !stdout.contains(BUS_SENTINEL) && !stderr.contains(BUS_SENTINEL),
        "sentinel bus address appeared in pane output: stdout: {stdout} stderr: {stderr}"
    );
}

fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}
