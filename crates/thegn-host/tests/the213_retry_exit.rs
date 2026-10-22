//! THE-213: real CLI exit-code and JSON refusal contract.
//!
//! Every fixture owns a private XDG state/config tree and uses a bounded child
//! wait. The assertions inspect only fixed protocol fields and never dump CLI
//! diagnostics or session data.
#![cfg(unix)]

use std::fs;
use std::io::{Read, Write};
use std::os::unix::net::UnixListener;
use std::path::PathBuf;
use std::process::{Command, Output, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde_json::Value;
use thegn_core::db::Db;
use thegn_core::issue::{AgentDispatchStatus, NewDispatch};
use thegn_core::store::NotificationStore;

const CHILD_TIMEOUT: Duration = Duration::from_secs(10);
const CAPTURE_LIMIT: u64 = 8 * 1024 * 1024;

struct Fixture {
    root: Arc<tempfile::TempDir>,
    config: PathBuf,
    db: PathBuf,
}

impl Fixture {
    fn new(concurrency: u32) -> Self {
        let root = Arc::new(tempfile::tempdir().unwrap());
        for name in ["config", "state", "runtime", "home", "tmp", "worktrees"] {
            fs::create_dir(root.path().join(name)).unwrap();
        }
        let config = root.path().join("config/pipeline.toml");
        let socket = root.path().join("runtime/control.sock");
        fs::write(
            &config,
            format!(
                // Configuration admission imposes two semantic rules on a stage
                // prompt: it must reference {row}, so the worker knows which
                // roster row it files against, and it must tell the worker to run
                // `thegn dispatch report`, because the done-gate requires a report
                // and a row without one can only be closed with --force. This
                // test is about retryable exit codes, so the prompt only has to
                // be admissible.
                "[daemon]\nenabled = false\nsocket = \"{}\"\n\n[[pipeline.stages]]\nname = \"code\"\nagent = \"claude\"\nprompt = \"You are dispatch row {{row}}. work {{issue_number}}. When finished run `thegn dispatch report`.\"\nconcurrency = {concurrency}\n",
                socket.display()
            ),
        )
        .unwrap();
        let db = root.path().join("state/thegn/thegn.db");
        Db::open_at(&db).unwrap();
        Self { root, config, db }
    }

    #[expect(
        clippy::disallowed_methods,
        reason = "fixture initializes isolated Git roots outside the event loop"
    )]
    fn worktree(&self, name: &str) -> String {
        let path = self.root.path().join("worktrees").join(name);
        fs::create_dir(&path).unwrap();
        let init = thegn_core::util::git_cmd(&path)
            .args(["-c", "commit.gpgsign=false", "init", "--quiet"])
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .output()
            .unwrap();
        assert!(
            init.status.success(),
            "git init failed: {}",
            String::from_utf8_lossy(&init.stderr)
        );
        let commit = thegn_core::util::git_cmd(&path)
            .args([
                "-c",
                "commit.gpgsign=false",
                "-c",
                "user.name=Thegn Test",
                "-c",
                "user.email=test@example.invalid",
                "commit",
                "--allow-empty",
                "--quiet",
                "-m",
                "initial",
            ])
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .output()
            .unwrap();
        assert!(
            commit.status.success(),
            "empty git commit failed: {}",
            String::from_utf8_lossy(&commit.stderr)
        );
        path.to_string_lossy().into_owned()
    }

    fn command(&self, args: &[&str]) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_thegn"));
        command
            .env_clear()
            .current_dir(self.root.path())
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .env("HOME", self.root.path().join("home"))
            // The child resolves pipeline worktree roots with git, which is not
            // under /usr/bin on every host (NixOS): keep the test's PATH.
            .env("PATH", std::env::var_os("PATH").unwrap_or_default())
            .env("XDG_CONFIG_HOME", self.root.path().join("config"))
            .env("XDG_STATE_HOME", self.root.path().join("state"))
            .env("XDG_RUNTIME_DIR", self.root.path().join("runtime"))
            .env("TMPDIR", self.root.path())
            .env("THEGN_NO_MIGRATE", "1")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env_remove("THEGN_LOG")
            .env_remove("THEGN_DATABASE_MIGRATION_EXECUTABLE")
            .env_remove("THEGN_DATABASE_MIGRATION_AUTHORITY")
            .arg("--config")
            .arg(&self.config)
            .args(args);
        command
    }

    fn run(&self, args: &[&str]) -> Output {
        run_bounded(self.command(args))
    }

    fn put(&self, issue: &str, worktree: &str, status: AgentDispatchStatus) -> i64 {
        let db = Db::open_at(&self.db).unwrap();
        let id = db
            .put_agent_dispatch(NewDispatch {
                issue_id: issue,
                worktree_path: worktree,
                agent_name: "claude",
                stage: Some("code"),
                ..NewDispatch::new(issue, worktree, "claude")
            })
            .unwrap();
        db.update_dispatch_status(id, status).unwrap();
        id
    }
}

#[expect(
    clippy::disallowed_methods,
    reason = "bounded private CLI fixture waits outside the event loop"
)]
fn run_bounded(mut command: Command) -> Output {
    let capture = tempfile::tempdir().unwrap();
    let stdout_path = capture.path().join("stdout");
    let stderr_path = capture.path().join("stderr");
    command
        .stdout(Stdio::from(std::fs::File::create(&stdout_path).unwrap()))
        .stderr(Stdio::from(std::fs::File::create(&stderr_path).unwrap()));
    let mut child = command.spawn().unwrap();
    let deadline = Instant::now() + CHILD_TIMEOUT;
    loop {
        if child.try_wait().unwrap().is_some() {
            let status = child.wait().unwrap();
            let stdout = fs::read(&stdout_path).unwrap();
            let stderr = fs::read(&stderr_path).unwrap();
            assert!(
                stdout.len() as u64 <= CAPTURE_LIMIT,
                "stdout capture exceeded bound"
            );
            assert!(
                stderr.len() as u64 <= CAPTURE_LIMIT,
                "stderr capture exceeded bound"
            );
            return Output {
                status,
                stdout,
                stderr,
            };
        }
        if Instant::now() >= deadline {
            if let Err(error) = child.kill() {
                tracing::warn!(%error, "CLI timeout cleanup could not kill child");
            }
            if let Err(error) = child.wait() {
                tracing::warn!(%error, "CLI timeout cleanup could not reap child");
            }
            panic!("thegn CLI child exceeded bounded test timeout");
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn json_lines(output: &Output, expected: usize) -> Vec<Value> {
    let text = std::str::from_utf8(&output.stdout).unwrap();
    let lines = text.lines().collect::<Vec<_>>();
    assert_eq!(lines.len(), expected, "unexpected JSON line count");
    lines
        .into_iter()
        .map(|line| serde_json::from_str(line).expect("stdout line must be JSON"))
        .collect()
}

fn assert_code(output: &Output, expected: i32) {
    assert_eq!(
        output.status.code(),
        Some(expected),
        "unexpected CLI exit code"
    );
}

fn serve_health(listener: UnixListener) {
    listener.set_nonblocking(true).unwrap();
    let deadline = Instant::now() + CHILD_TIMEOUT;
    loop {
        match listener.accept() {
            Ok((mut stream, _)) => {
                stream
                    .set_read_timeout(Some(Duration::from_secs(2)))
                    .unwrap();
                let mut request = Vec::with_capacity(1024);
                let mut complete = false;
                for _ in 0..64 {
                    let mut chunk = [0u8; 1024];
                    match stream.read(&mut chunk) {
                        Ok(0) => break,
                        Ok(n) => {
                            request.extend_from_slice(&chunk[..n]);
                            if request.windows(4).any(|window| window == b"\r\n\r\n") {
                                complete = true;
                                break;
                            }
                        }
                        Err(error)
                            if matches!(
                                error.kind(),
                                std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock
                            ) =>
                        {
                            break;
                        }
                        Err(error) => panic!("health fixture read failed: {error}"),
                    }
                }
                assert!(complete, "health fixture request headers were incomplete");
                let request_line = std::str::from_utf8(&request)
                    .unwrap()
                    .lines()
                    .next()
                    .unwrap();
                let path = request_line.split_whitespace().nth(1).unwrap();
                match path {
                    "/health" => stream
                        .write_all(
                            b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 11\r\nConnection: close\r\n\r\n{\"ok\":true}",
                        )
                        .unwrap(),
                    "/daemon/scope" => {
                        // Older daemons did not expose the Unix-only scope route.
                        // The client accepts that response while preserving its
                        // strict rejection of malformed scope replies.
                        stream
                            .write_all(
                                b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                            )
                            .unwrap();
                        return;
                    }
                    unexpected => panic!("unexpected health fixture request: {unexpected}"),
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                if Instant::now() >= deadline {
                    panic!("health fixture was not contacted before timeout");
                }
                std::thread::sleep(Duration::from_millis(10));
            }
            Err(error) => panic!("health fixture accept failed: {error}"),
        }
    }
}

#[test]
#[cfg(unix)]
fn claim_duplicate_capacity_and_lease_contention_are_retryable_json() {
    let fixture = Fixture::new(1);
    let worktree_a = fixture.worktree("a");
    let worktree_b = fixture.worktree("b");
    assert_code(
        &fixture.run(&[
            "dispatch",
            "claim",
            "linear:THE-213-A",
            &worktree_a,
            "claude",
            "--stage",
            "code",
            "--json",
        ]),
        0,
    );
    let duplicate = fixture.run(&[
        "dispatch",
        "claim",
        "linear:THE-213-A",
        &worktree_a,
        "claude",
        "--stage",
        "code",
        "--json",
    ]);
    assert_code(&duplicate, 2);
    assert_eq!(json_lines(&duplicate, 1)[0]["granted"], false);

    let capacity = fixture.run(&[
        "dispatch",
        "claim",
        "linear:THE-213-B",
        &worktree_b,
        "claude",
        "--stage",
        "code",
        "--json",
    ]);
    assert_code(&capacity, 2);
    assert_eq!(json_lines(&capacity, 1)[0]["granted"], false);

    let lease_owner = fixture.run(&[
        "dispatch",
        "lease",
        "acquire",
        "--owner",
        "monitor-a",
        "--json",
    ]);
    assert_code(&lease_owner, 0);
    let lease_contender = fixture.run(&[
        "dispatch",
        "lease",
        "acquire",
        "--owner",
        "monitor-b",
        "--json",
    ]);
    assert_code(&lease_contender, 2);
    assert_eq!(json_lines(&lease_contender, 1)[0]["acquired"], false);
}

#[test]
#[cfg(unix)]
fn stage_and_resume_contention_are_retryable_json_without_opening_a_session() {
    let fixture = Fixture::new(1);
    let worktree_capacity = fixture.worktree("capacity");
    let worktree_new = fixture.worktree("new");
    fixture.put(
        "linear:THE-213-capacity",
        &worktree_capacity,
        AgentDispatchStatus::Queued,
    );
    let socket = fixture.root.path().join("runtime/control.sock");
    let listener = UnixListener::bind(&socket).unwrap();
    let server = std::thread::spawn(move || serve_health(listener));
    let stage = fixture.run(&[
        "session",
        "open",
        "--stage",
        "code",
        "--issue",
        "linear:THE-213-new",
        "--worktree",
        &worktree_new,
        "--json",
    ]);
    assert_code(&stage, 2);
    assert_eq!(json_lines(&stage, 1)[0]["granted"], false);
    server.join().unwrap();

    let resume = Fixture::new(1);
    let worktree_resume = resume.worktree("resume");
    let worktree_full = resume.worktree("full");
    let source = resume.put(
        "linear:THE-213-resume",
        &worktree_resume,
        AgentDispatchStatus::WaitingHuman,
    );
    resume.put(
        "linear:THE-213-full",
        &worktree_full,
        AgentDispatchStatus::Queued,
    );
    let socket = resume.root.path().join("runtime/control.sock");
    let listener = UnixListener::bind(&socket).unwrap();
    let server = std::thread::spawn(move || serve_health(listener));
    let resumed = resume.run(&[
        "session",
        "open",
        "--resume-work",
        &source.to_string(),
        "--json",
    ]);
    assert_code(&resumed, 2);
    assert_eq!(json_lines(&resumed, 1)[0]["granted"], false);
    server.join().unwrap();
}

#[test]
#[cfg(unix)]
fn successful_claim_is_zero_and_fatal_config_or_database_errors_are_one() {
    let success = Fixture::new(1);
    let worktree_success = success.worktree("success");
    let output = success.run(&[
        "dispatch",
        "claim",
        "linear:THE-213-success",
        &worktree_success,
        "claude",
        "--stage",
        "code",
        "--json",
    ]);
    assert_code(&output, 0);
    assert_eq!(json_lines(&output, 1)[0]["granted"], true);

    let unknown = Fixture::new(1);
    let worktree_unknown = unknown.worktree("unknown");
    let output = unknown.run(&[
        "dispatch",
        "claim",
        "linear:THE-213-unknown",
        &worktree_unknown,
        "claude",
        "--stage",
        "missing",
        "--json",
    ]);
    assert_code(&output, 1);
    assert!(output.stdout.is_empty(), "fatal refusal must not emit JSON");

    let malformed = Fixture::new(1);
    let worktree_config = malformed.worktree("config");
    fs::write(&malformed.config, "[pipeline\n").unwrap();
    let output = malformed.run(&[
        "dispatch",
        "claim",
        "linear:THE-213-config",
        &worktree_config,
        "claude",
        "--stage",
        "missing",
        "--json",
    ]);
    assert_code(&output, 1);
    assert!(
        output.stdout.is_empty(),
        "fatal config error must not emit JSON"
    );

    let database = Fixture::new(1);
    let worktree_db = database.worktree("db");
    fs::remove_file(&database.db).unwrap();
    fs::create_dir(&database.db).unwrap();
    let output = database.run(&[
        "dispatch",
        "claim",
        "linear:THE-213-db",
        &worktree_db,
        "claude",
        "--stage",
        "code",
        "--json",
    ]);
    assert_code(&output, 1);
    assert!(
        output.stdout.is_empty(),
        "fatal DB error must not emit JSON"
    );
}
