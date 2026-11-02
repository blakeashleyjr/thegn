use std::fs::File;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use thegn_core::testenv::state_env_all;

const CAPTURE_LIMIT: u64 = 64 * 1024;
const CHILD_COUNT: usize = 4;
const CHILD_DEADLINE: Duration = Duration::from_secs(20);

struct Scope {
    root: tempfile::TempDir,
}

impl Scope {
    fn new() -> Scope {
        Scope {
            root: tempfile::Builder::new()
                .prefix("tgdb")
                .tempdir_in("/tmp")
                .expect("create short isolated database race scope"),
        }
    }

    fn command(&self) -> Command {
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

struct OwnedChild {
    child: Child,
    stdout_path: PathBuf,
    stderr_path: PathBuf,
}

impl Drop for OwnedChild {
    #[expect(
        clippy::disallowed_methods,
        reason = "process test owns and reaps every state-database child"
    )]
    fn drop(&mut self) {
        if let Err(error) = self.child.kill()
            && error.kind() != std::io::ErrorKind::InvalidInput
        {
            let _write_result = writeln!(
                std::io::stderr().lock(),
                "state_db_first_launch_race_process: could not kill child during cleanup: {error}"
            );
        }
        if let Err(error) = self.child.wait() {
            let _write_result = writeln!(
                std::io::stderr().lock(),
                "state_db_first_launch_race_process: could not reap child during cleanup: {error}"
            );
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

fn spawn_disk(scope: &Scope, index: usize) -> OwnedChild {
    let stdout_path = scope.root.path().join(format!("child-{index}.stdout"));
    let stderr_path = scope.root.path().join(format!("child-{index}.stderr"));
    let stdout = File::create(&stdout_path).expect("create captured child stdout");
    let stderr = File::create(&stderr_path).expect("create captured child stderr");
    let mut command = scope.command();
    command.args(["wt", "disk", "--all", "--json"]);
    let child = command
        .stdout(Stdio::from(stdout))
        .stderr(Stdio::from(stderr))
        .spawn()
        .expect("spawn isolated state database command");
    OwnedChild {
        child,
        stdout_path,
        stderr_path,
    }
}

#[expect(
    clippy::disallowed_methods,
    reason = "process test waits for and reaps every CLI child"
)]
fn wait_for_child(child: &mut OwnedChild) -> ExitStatus {
    let deadline = Instant::now() + CHILD_DEADLINE;
    loop {
        match child.child.try_wait() {
            Ok(Some(status)) => return status,
            Ok(None) if Instant::now() < deadline => thread::sleep(Duration::from_millis(10)),
            Ok(None) => {
                if let Err(error) = child.child.kill() {
                    panic!(
                        "CLI child exceeded {CHILD_DEADLINE:?}; kill failed: {error}; {}",
                        child_report(child)
                    );
                }
                let _status = child.child.wait().expect("reap timed-out CLI child");
                panic!(
                    "CLI child exceeded {CHILD_DEADLINE:?}; {}",
                    child_report(child)
                );
            }
            Err(error) => {
                if let Err(kill_error) = child.child.kill() {
                    panic!("could not poll CLI child ({error}) or kill it ({kill_error})");
                }
                let _status = child
                    .child
                    .wait()
                    .expect("reap CLI child after poll failure");
                panic!("could not poll CLI child: {error}; {}", child_report(child));
            }
        }
    }
}

fn child_report(child: &OwnedChild) -> String {
    format!(
        "stdout:\n{}\nstderr:\n{}",
        read_capped(&child.stdout_path),
        read_capped(&child.stderr_path)
    )
}

#[test]
fn state_db_first_launch_race_every_launch_gets_a_usable_database() {
    if !thegn_core::testenv::require_or_skip("Unix state-db process race", cfg!(unix)) {
        return;
    }

    // Every round starts with no state database. Four independent processes
    // contend on the same bootstrap schema lease; the 20-second child deadline
    // is generous under load, and no winner identity or process order is assumed.
    const ROUNDS: usize = 3;
    for _round in 0..ROUNDS {
        let scope = Scope::new();
        let mut children = (0..CHILD_COUNT)
            .map(|index| spawn_disk(&scope, index))
            .collect::<Vec<_>>();

        let statuses = children.iter_mut().map(wait_for_child).collect::<Vec<_>>();
        for (index, (child, status)) in children.iter().zip(&statuses).enumerate() {
            let stdout = read_capped(&child.stdout_path);
            let stderr = read_capped(&child.stderr_path);
            assert!(
                status.success(),
                "first-launch child {index} exited {status}; stdout:\n{stdout}\nstderr:\n{stderr}"
            );
            assert!(
                !stderr.contains("old schema") && !stderr.contains("could not be opened"),
                "first-launch child {index} reported a bootstrap failure; status={status}; stdout:\n{stdout}\nstderr:\n{stderr}"
            );
        }

        let mut follow_up = spawn_disk(&scope, CHILD_COUNT);
        let status = wait_for_child(&mut follow_up);
        let stdout = read_capped(&follow_up.stdout_path);
        let stderr = read_capped(&follow_up.stderr_path);
        assert!(
            status.success(),
            "database must remain usable after the race; status={status}; stdout:\n{stdout}\nstderr:\n{stderr}"
        );
        assert!(
            !stderr.contains("old schema") && !stderr.contains("could not be opened"),
            "usable database follow-up reported an open failure; status={status}; stdout:\n{stdout}\nstderr:\n{stderr}"
        );
    }
}
