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
const BUS_SENTINEL: &str = "unix:path=/nonexistent/tg-stage-worker-bus-sentinel";

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
        if let Err(error) = self.child.kill()
            && error.kind() != std::io::ErrorKind::InvalidInput
        {
            let _write_result = writeln!(
                std::io::stderr().lock(),
                "stage_worker_commit_e2e: could not kill daemon during cleanup: {error}"
            );
            return;
        }
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            match self.child.try_wait() {
                Ok(Some(_)) => return,
                Ok(None) if Instant::now() < deadline => thread::sleep(Duration::from_millis(20)),
                Ok(None) => {
                    let _write_result = writeln!(
                        std::io::stderr().lock(),
                        "stage_worker_commit_e2e: daemon did not exit after kill within 2s"
                    );
                    return;
                }
                Err(error) => {
                    let _write_result = writeln!(
                        std::io::stderr().lock(),
                        "stage_worker_commit_e2e: could not poll daemon during cleanup: {error}"
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
        return thegn_core::testenv::require_or_skip("Linux bwrap stage-worker e2e", false);
    }
    let Some(path) = std::env::var_os("PATH") else {
        panic!("stage-worker e2e requires PATH to locate bwrap");
    };
    let Some(bwrap) = std::env::split_paths(&path)
        .map(|dir| dir.join("bwrap"))
        .find(|candidate| candidate.is_file())
    else {
        return thegn_core::testenv::require_or_skip("bwrap on PATH", false);
    };
    let probe_dir = tempfile::tempdir().expect("private bwrap probe directory");
    let probe = run_bounded(
        Command::new(bwrap).args(["--unshare-user", "--ro-bind", "/", "/", "true"]),
        probe_dir.path(),
        "userns-probe",
        PROCESS_TIMEOUT,
    );
    if !probe.status.success() {
        return thegn_core::testenv::require_or_skip("working bwrap user namespaces", false);
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
        "stage-worker-commit-e2e-prerequisite",
        SandboxProfile::Hardened,
        Placement::Local,
    )
    .is_some_and(|spec| spec.backend == Backend::Bwrap);
    if !resolves_bwrap {
        return thegn_core::testenv::require_or_skip(
            "bwrap standard pane backend resolution",
            false,
        );
    }
    true
}

struct Fixture {
    state: IsolatedState,
    runtime: tempfile::TempDir,
    captures: tempfile::TempDir,
    outside: tempfile::TempDir,
    repo: PathBuf,
    worktree: PathBuf,
    base_head: String,
    git_config: Vec<u8>,
    daemon: RunningDaemon,
}

impl Fixture {
    fn new(deny_outside_write: bool) -> Self {
        let state = IsolatedState::new();
        let runtime = tempfile::Builder::new()
            .prefix("tg-stage-worker-")
            .tempdir_in("/tmp")
            .expect("private daemon runtime directory");
        let captures = tempfile::tempdir_in(state.root()).expect("private capture directory");
        let outside = tempfile::tempdir().expect("private outside directory");
        let repo = state.root().join("repo");
        let worktree = state.root().join("worker");
        fs::create_dir(&repo).expect("create disposable repository");

        let init = Self::git(&state, &repo, ["init", "--quiet"]);
        assert_git_success(&init, "git init");
        fs::write(repo.join("README"), b"base\n").expect("write base file");
        let initial = Self::git(
            &state,
            &repo,
            [
                "-c",
                "user.name=stage-worker-e2e",
                "-c",
                "user.email=stage-worker-e2e@example.invalid",
                "add",
                "README",
            ],
        );
        assert_git_success(&initial, "git add base file");
        let initial = Self::git(
            &state,
            &repo,
            [
                "-c",
                "user.name=stage-worker-e2e",
                "-c",
                "user.email=stage-worker-e2e@example.invalid",
                "-c",
                "commit.gpgsign=false",
                "commit",
                "-m",
                "base",
            ],
        );
        assert_git_success(&initial, "git commit base file");
        let base_head = Self::git_output(&state, &repo, ["rev-parse", "HEAD"]);
        let worktree_path = worktree.to_string_lossy().into_owned();
        let added = Self::git(
            &state,
            &repo,
            ["worktree", "add", "--quiet", "-b", "worker", &worktree_path],
        );
        assert_git_success(&added, "git worktree add");

        let outside_write = deny_outside_write
            .then_some(" && : > \"$OUTSIDE/denied\"")
            .unwrap_or_default();
        let script = format!(
            "printf '%s\\n' committed > stage-worker-marker && git add stage-worker-marker && git -c user.name=stage-worker-e2e -c user.email=stage-worker-e2e@example.invalid -c commit.gpgsign=false commit -m x{outside_write}"
        );
        let command = format!("sh -c {}", thegn_core::util::sh_quote(&script));
        let outside_path = outside.path().to_string_lossy();
        let config = format!(
            "[sandbox]\nenabled = true\nbackend = \"bwrap\"\nbackend_chain = [\"bwrap\"]\nprofile = \"standard\"\nnetwork = \"none\"\nenv_passthrough = []\n\n[[agents]]\nname = \"stub\"\ncommand = {command:?}\n\n[[pipeline.stages]]\nname = \"commit\"\nagent = \"stub\"\nprompt = \"commit the fixture\"\nconcurrency = 1\ntimeout_secs = 60\nenv = {{ OUTSIDE = {outside_path:?}, GIT_CONFIG_GLOBAL = \"/dev/null\", GIT_CONFIG_NOSYSTEM = \"1\", GIT_TERMINAL_PROMPT = \"0\" }}\n",
        );
        let config_dir = state.config_home().join("thegn");
        fs::create_dir_all(&config_dir).expect("create isolated config directory");
        fs::write(config_dir.join("config.toml"), config).expect("write isolated config");

        let mut daemon_command = Command::new(env!("CARGO_BIN_EXE_thegn"));
        daemon_command
            .arg("daemon")
            .envs(state_env_all(state.root()))
            .env("XDG_RUNTIME_DIR", runtime.path())
            .env("DBUS_SESSION_BUS_ADDRESS", BUS_SENTINEL)
            .env_remove("THEGN_LOG")
            .env_remove("THEGN_DATABASE_MIGRATION_EXECUTABLE")
            .env_remove("THEGN_DATABASE_MIGRATION_AUTHORITY")
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        let child = daemon_command.spawn().expect("spawn isolated pane daemon");
        let mut daemon = RunningDaemon { child };
        wait_for_socket(runtime.path(), &mut daemon);
        let git_config = fs::read(repo.join(".git/config")).expect("read git config fixture");

        Self {
            state,
            runtime,
            captures,
            outside,
            repo,
            worktree,
            base_head,
            git_config,
            daemon,
        }
    }

    fn git<'a, I>(state: &IsolatedState, cwd: &Path, args: I) -> Captured
    where
        I: IntoIterator<Item = &'a str>,
    {
        let captures = tempfile::tempdir_in(state.root()).expect("private git capture directory");
        let mut command = thegn_core::util::git_cmd(cwd);
        command
            .args(args)
            .envs(state_env_all(state.root()))
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_TERMINAL_PROMPT", "0");
        run_bounded(&mut command, captures.path(), "git", PROCESS_TIMEOUT)
    }

    fn git_output<'a, I>(state: &IsolatedState, cwd: &Path, args: I) -> String
    where
        I: IntoIterator<Item = &'a str>,
    {
        let result = Self::git(state, cwd, args);
        assert_git_success(&result, "git rev-parse");
        String::from_utf8(result.stdout)
            .expect("git output is UTF-8")
            .trim()
            .to_string()
    }

    fn cli(&self, args: &[&str]) -> Captured {
        let mut command = Command::new(env!("CARGO_BIN_EXE_thegn"));
        command
            .args(args)
            .envs(state_env_all(self.state.root()))
            .env("XDG_RUNTIME_DIR", self.runtime.path())
            .env("DBUS_SESSION_BUS_ADDRESS", BUS_SENTINEL)
            .env_remove("THEGN_LOG")
            .env_remove("THEGN_DATABASE_MIGRATION_EXECUTABLE")
            .env_remove("THEGN_DATABASE_MIGRATION_AUTHORITY");
        run_bounded(&mut command, self.captures.path(), "cli", PROCESS_TIMEOUT)
    }

    fn run_stage(&self) -> (Captured, Captured) {
        let worktree = self.worktree.to_str().expect("worktree path is UTF-8");
        let opened = self.cli(&[
            "session",
            "open",
            "--stage",
            "commit",
            "--agent",
            "stub",
            "--headless",
            "--prompt",
            "x",
            "--worktree",
            worktree,
            "--json",
        ]);
        assert!(
            opened.status.success(),
            "stage session open failed (program {}): stdout: {} stderr: {}",
            opened.program,
            String::from_utf8_lossy(&opened.stdout),
            String::from_utf8_lossy(&opened.stderr)
        );
        let info: serde_json::Value =
            serde_json::from_slice(&opened.stdout).unwrap_or_else(|error| {
                panic!(
                    "stage session open did not emit JSON: {error}; stdout: {}; stderr: {}",
                    String::from_utf8_lossy(&opened.stdout),
                    String::from_utf8_lossy(&opened.stderr)
                )
            });
        let session = info["id"].as_str().expect("session open JSON has an id");
        let waited = self.cli(&[
            "session",
            "wait",
            "--session",
            session,
            "--until",
            "exited",
            "--timeout",
            "10000",
            "--json",
        ]);
        assert!(
            waited.status.success(),
            "session wait failed (program {}): stdout: {} stderr: {}",
            waited.program,
            String::from_utf8_lossy(&waited.stdout),
            String::from_utf8_lossy(&waited.stderr)
        );
        (opened, waited)
    }
}

fn assert_git_success(result: &Captured, operation: &str) {
    assert!(
        result.status.success(),
        "{operation} failed (program {}): stdout: {} stderr: {}",
        result.program,
        String::from_utf8_lossy(&result.stdout),
        String::from_utf8_lossy(&result.stderr)
    );
}

fn wait_for_socket(runtime: &Path, daemon: &mut RunningDaemon) {
    let deadline = Instant::now() + PROCESS_TIMEOUT;
    loop {
        if find_socket(runtime).is_some() {
            return;
        }
        if let Some(status) = daemon.child.try_wait().expect("poll isolated daemon") {
            panic!("isolated daemon exited before creating its socket ({status})");
        }
        assert!(
            Instant::now() < deadline,
            "daemon did not create its socket within {PROCESS_TIMEOUT:?}"
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

fn wait_exit_code(waited: &Captured) -> i64 {
    let outcome: serde_json::Value =
        serde_json::from_slice(&waited.stdout).unwrap_or_else(|error| {
            panic!(
                "session wait did not emit JSON: {error}; stdout: {}; stderr: {}",
                String::from_utf8_lossy(&waited.stdout),
                String::from_utf8_lossy(&waited.stderr)
            )
        });
    outcome["exit_code"]
        .as_i64()
        .expect("session wait JSON has an exit code")
}

#[test]
fn stage_worker_commit_e2e_commits_in_the_isolated_worktree() {
    if !required_prerequisites() {
        return;
    }
    let fixture = Fixture::new(false);
    let (_opened, waited) = fixture.run_stage();
    assert_eq!(wait_exit_code(&waited), 0, "worker commit must succeed");
    let head = Fixture::git_output(&fixture.state, &fixture.worktree, ["rev-parse", "HEAD"]);
    assert_ne!(head, fixture.base_head, "worker commit advanced HEAD");
    assert!(
        fixture.worktree.join("stage-worker-marker").is_file(),
        "worker commit left its staged file in the worktree"
    );
}

#[test]
fn stage_worker_commit_e2e_denies_writes_outside_the_mounted_worktree() {
    if !required_prerequisites() {
        return;
    }
    let fixture = Fixture::new(true);
    let (_opened, waited) = fixture.run_stage();
    assert_ne!(
        wait_exit_code(&waited),
        0,
        "worker must fail when its script writes to the unmounted sibling"
    );
    assert!(
        !fixture.outside.path().join("denied").exists(),
        "worker wrote through the sandbox boundary: {}",
        fixture.outside.path().join("denied").display()
    );
    let head = Fixture::git_output(&fixture.state, &fixture.worktree, ["rev-parse", "HEAD"]);
    assert_ne!(
        head, fixture.base_head,
        "the commit completed before denial"
    );
    let config = fixture.repo.join(".git/config");
    assert_eq!(
        fs::read(&config).expect("read git config after worker"),
        fixture.git_config,
        "git config stayed unchanged (and therefore read-only when the launch pins it)"
    );
}
