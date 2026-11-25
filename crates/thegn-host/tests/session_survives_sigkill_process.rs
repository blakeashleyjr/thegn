//! Kill-test for the "sessions persist" claim (THE-2658): the daemon owns the
//! PTYs, so SIGKILLing an attached client must leave the same session id, the
//! same PTY child pid and the marker on screen — and a relaunched client must
//! see all three.
//!
//! The compositor (the TUI host) needs a real terminal, so the killed client
//! here is a `thegn session attach` process: it is the same daemon-side
//! attachment the compositor holds, dropped by the same uncatchable signal.

use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use thegn_core::testenv::state_env_all;

const STEP_TIMEOUT: Duration = Duration::from_secs(30);
const MARKER: &str = "KILLTEST_MARKER_4F2A";

struct Scope {
    root: tempfile::TempDir,
    runtime: std::path::PathBuf,
}

impl Scope {
    fn new() -> Scope {
        let root = tempfile::Builder::new()
            .prefix("tgks")
            .tempdir_in("/tmp")
            .expect("create short isolated daemon root");
        let runtime = root.path().join("runtime");
        std::fs::create_dir(&runtime).expect("create isolated runtime directory");
        Scope { root, runtime }
    }

    fn command(&self) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_thegn"));
        command
            .env_clear()
            .current_dir(self.root.path())
            .envs(state_env_all(self.root.path()))
            .env("XDG_RUNTIME_DIR", &self.runtime)
            .env("TMPDIR", self.root.path())
            .env("PATH", std::env::var_os("PATH").unwrap_or_default())
            .stdin(Stdio::null());
        command
    }

    /// Run the CLI to completion and return `(success, stdout, stderr)`.
    #[expect(
        clippy::disallowed_methods,
        reason = "process test owns and reaps the CLI child"
    )]
    fn run(&self, args: &[&str]) -> (bool, String, String) {
        let output = self
            .command()
            .args(args)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .output()
            .expect("run isolated thegn CLI");
        (
            output.status.success(),
            String::from_utf8_lossy(&output.stdout).into_owned(),
            String::from_utf8_lossy(&output.stderr).into_owned(),
        )
    }

    /// Live sessions as `(id, pid)`.
    fn live_sessions(&self) -> Option<Vec<(String, Option<u64>)>> {
        let (ok, stdout, _stderr) = self.run(&["session", "list", "--json", "--live"]);
        if !ok {
            return None;
        }
        let parsed: serde_json::Value = serde_json::from_str(&stdout).ok()?;
        Some(
            parsed
                .as_array()?
                .iter()
                .filter_map(|row| {
                    Some((
                        row.get("id")?.as_str()?.to_string(),
                        row.get("pid").and_then(serde_json::Value::as_u64),
                    ))
                })
                .collect(),
        )
    }

    fn snapshot_text(&self, id: &str) -> String {
        let (_ok, stdout, _stderr) = self.run(&["session", "snapshot", "--text", id]);
        stdout
    }
}

struct OwnedChild(Child);

impl Drop for OwnedChild {
    #[expect(
        clippy::disallowed_methods,
        reason = "process test owns and reaps its daemon child"
    )]
    fn drop(&mut self) {
        let _kill = self.0.kill();
        let _status = self.0.wait();
    }
}

fn wait_until<T>(what: &str, mut probe: impl FnMut() -> Option<T>) -> T {
    let deadline = Instant::now() + STEP_TIMEOUT;
    loop {
        if let Some(value) = probe() {
            return value;
        }
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn spawn_attach(scope: &Scope, id: &str) -> Child {
    scope
        .command()
        .args(["session", "attach", id])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn attaching client")
}

fn attached_clients(scope: &Scope, id: &str) -> Option<u64> {
    let (ok, stdout, _stderr) = scope.run(&["session", "list", "--json", "--live"]);
    if !ok {
        return None;
    }
    let parsed: serde_json::Value = serde_json::from_str(&stdout).ok()?;
    parsed
        .as_array()?
        .iter()
        .find(|row| row.get("id").and_then(serde_json::Value::as_str) == Some(id))?
        .get("attached_clients")?
        .as_u64()
}

#[test]
fn sigkilled_client_leaves_session_pid_and_screen_intact_for_relaunch() {
    if !thegn_core::testenv::require_or_skip("Unix sockets and SIGKILL", cfg!(unix)) {
        return;
    }
    let scope = Scope::new();

    // 1. A daemon of our own, owning the PTYs.
    let mut daemon_cmd = scope.command();
    daemon_cmd
        .arg("daemon")
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    let daemon = OwnedChild(daemon_cmd.spawn().expect("spawn isolated pane daemon"));
    wait_until("daemon to answer session list", || scope.live_sessions());

    // 2. One session running a marker command that stays alive.
    let params = serde_json::json!({
        "argv": ["sh", "-c", format!("echo {MARKER}; exec sleep 600")],
        "cwd": scope.root.path().to_str().expect("utf-8 root"),
        "rows": 24,
        "cols": 80,
    })
    .to_string();
    let (ok, stdout, stderr) = scope.run(&["api", "call", "sessions.open", "--params", &params]);
    assert!(
        ok,
        "sessions.open failed; stdout:\n{stdout}\nstderr:\n{stderr}"
    );

    let (id, pid_before) = wait_until("the marker session to be listed with a pid", || {
        let sessions = scope.live_sessions()?;
        match sessions.as_slice() {
            [(id, Some(pid))] => Some((id.clone(), *pid)),
            _ => None,
        }
    });
    wait_until("the marker on the session screen", || {
        scope.snapshot_text(&id).contains(MARKER).then_some(())
    });

    // 3. Attach a client, then SIGKILL it (`Child::kill` is SIGKILL on Unix).
    let mut client = spawn_attach(&scope, &id);
    wait_until("the client to be counted as attached", || {
        (attached_clients(&scope, &id)? >= 1).then_some(())
    });
    client.kill().expect("SIGKILL the attached client");
    #[expect(
        clippy::disallowed_methods,
        reason = "reap the SIGKILLed attach client"
    )]
    let status = client.wait().expect("reap SIGKILLed client");
    assert!(!status.success(), "client must have died by signal");

    // 4. "Relaunch": brand-new client processes see the same session, pid and
    // screen. The daemon must have noticed the dead attachment.
    wait_until("the daemon to drop the killed attachment", || {
        (attached_clients(&scope, &id)? == 0).then_some(())
    });
    let after = scope
        .live_sessions()
        .expect("session list after the client kill");
    assert_eq!(
        after,
        vec![(id.clone(), Some(pid_before))],
        "same session id and same PTY child pid must survive the SIGKILL"
    );
    let screen = scope.snapshot_text(&id);
    assert!(
        screen.contains(MARKER),
        "snapshot after relaunch must still show the marker; got:\n{screen}"
    );
    assert!(
        pid_alive(pid_before),
        "PTY child {pid_before} must still be running"
    );

    // 5. Clean up: close the session and stop the daemon.
    let _closed = scope.run(&["session", "close", &id]);
    let _stopped = scope.run(&["daemon", "stop"]);
    drop(daemon);
}

#[expect(
    clippy::disallowed_methods,
    reason = "process test: short-lived `kill -0` probe"
)]
fn pid_alive(pid: u64) -> bool {
    Path::new(&format!("/proc/{pid}")).exists()
        || Command::new("kill")
            .args(["-0", &pid.to_string()])
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|status| status.success())
}
