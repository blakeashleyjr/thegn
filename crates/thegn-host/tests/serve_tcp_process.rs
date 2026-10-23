#![cfg(target_os = "linux")]

use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

struct ServerChild {
    child: Child,
    finished: bool,
}

impl ServerChild {
    fn terminate(&mut self) -> Option<ExitStatus> {
        if self.finished {
            return None;
        }
        let pid = self.child.id();
        // SAFETY: pid is the live child owned by this guard.
        let signal_result = unsafe { libc::kill(pid as libc::pid_t, libc::SIGTERM) };
        assert_eq!(signal_result, 0, "SIGTERM reaches the owned serve process");
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            match self.child.try_wait() {
                Ok(Some(status)) => {
                    self.finished = true;
                    return Some(status);
                }
                Ok(None) if Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(20));
                }
                Ok(None) | Err(_) => {
                    self.child.kill().unwrap();
                    self.child.wait().unwrap();
                    self.finished = true;
                    return None;
                }
            }
        }
    }
}

impl Drop for ServerChild {
    #[allow(
        clippy::disallowed_macros,
        reason = "best-effort test teardown reporting"
    )]
    fn drop(&mut self) {
        if !self.finished {
            let unwinding = std::thread::panicking();
            let pid = self.child.id();
            // SAFETY: pid is the live child owned by this guard.
            if unsafe { libc::kill(pid as libc::pid_t, libc::SIGTERM) } != 0 {
                let error = std::io::Error::last_os_error();
                if error.raw_os_error() != Some(libc::ESRCH) {
                    if unwinding {
                        eprintln!(
                            "best-effort: failed to signal owned serve child during test unwind: {error}"
                        );
                    } else {
                        eprintln!("best-effort: failed to signal owned serve child: {error}");
                    }
                }
            }
            let deadline = Instant::now() + Duration::from_secs(5);
            loop {
                match self.child.try_wait() {
                    Ok(Some(_)) => {
                        self.finished = true;
                        break;
                    }
                    Ok(None) if Instant::now() < deadline => {
                        std::thread::sleep(Duration::from_millis(20));
                    }
                    Ok(None) | Err(_) => {
                        if let Err(error) = self.child.kill()
                            && error.kind() != std::io::ErrorKind::InvalidInput
                        {
                            if unwinding {
                                eprintln!(
                                    "best-effort: failed to kill owned serve child during test unwind: {error}"
                                );
                            } else {
                                eprintln!("best-effort: failed to kill owned serve child: {error}");
                            }
                        }
                        if let Err(error) = self.child.wait() {
                            if unwinding {
                                eprintln!(
                                    "best-effort: failed to wait for owned serve child during test unwind: {error}"
                                );
                            } else {
                                eprintln!(
                                    "best-effort: failed to wait for owned serve child: {error}"
                                );
                            }
                        }
                        self.finished = true;
                        break;
                    }
                }
            }
        }
    }
}

struct Fixture {
    _root: tempfile::TempDir,
    runtime: tempfile::TempDir,
    path: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let root = tempfile::tempdir_in("/tmp").unwrap();
        let runtime = tempfile::tempdir_in("/tmp").unwrap();
        let path = root.path().to_path_buf();
        for name in ["cwd", "config", "state", "cache", "local"] {
            std::fs::create_dir(path.join(name)).unwrap();
        }
        Self {
            _root: root,
            runtime,
            path,
        }
    }

    fn command(&self) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_thegn"));
        command
            .env_clear()
            .current_dir(self.path.join("cwd"))
            .env("THEGN_DIR", self.path.join("app"))
            .env("THEGN_CHANNEL", "dev")
            .env("XDG_CONFIG_HOME", self.path.join("config"))
            .env("XDG_STATE_HOME", self.path.join("state"))
            .env("XDG_CACHE_HOME", self.path.join("cache"))
            .env("XDG_RUNTIME_DIR", self.runtime.path())
            .env("TMPDIR", &self.path)
            .env("APPDATA", self.path.join("config"))
            .env("LOCALAPPDATA", self.path.join("local"))
            .env("PATH", std::env::var_os("PATH").unwrap_or_default())
            .arg("serve")
            .arg("--bind")
            .arg("127.0.0.1:0")
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        command
    }

    fn socket_path(&self) -> PathBuf {
        fn find(dir: &Path) -> Option<PathBuf> {
            let entries = std::fs::read_dir(dir).ok()?;
            for entry in entries.flatten() {
                let path = entry.path();
                if path.file_name().is_some_and(|name| name == "daemon.sock") {
                    return Some(path);
                }
                if path.is_dir()
                    && let Some(found) = find(&path)
                {
                    return Some(found);
                }
            }
            None
        }
        find(self.runtime.path()).expect("serve creates its Unix socket under XDG_RUNTIME_DIR")
    }
}

#[allow(
    clippy::disallowed_methods,
    reason = "integration test runs and reaps the real binary"
)]
#[tokio::test(flavor = "multi_thread")]
async fn cli_serve_tcp_pairs_and_requires_a_token() {
    let fixture = Fixture::new();
    let mut child = ServerChild {
        child: fixture.command().spawn().unwrap(),
        finished: false,
    };
    let stdout = child.child.stdout.take().expect("serve stdout is piped");
    let (line_tx, line_rx) = mpsc::channel();
    let reader = std::thread::Builder::new()
        .name("thegn-t-srvout".into())
        .spawn(move || {
            for line in BufReader::new(stdout).lines() {
                match line {
                    Ok(line) => {
                        if line_tx.send(line).is_err() {
                            break;
                        }
                    }
                    Err(_) => break,
                }
            }
        })
        .unwrap();

    let deadline = Instant::now() + Duration::from_secs(20);
    let mut listening = None;
    let mut pairing = None;
    while Instant::now() < deadline && (listening.is_none() || pairing.is_none()) {
        let remaining = deadline.saturating_duration_since(Instant::now());
        match line_rx.recv_timeout(remaining.min(Duration::from_secs(1))) {
            Ok(line) if line.contains("listening on ") => {
                let address = line
                    .split_once("listening on ")
                    .and_then(|(_, rest)| rest.split_once(" (").map(|(addr, _)| addr))
                    .expect("listening line contains a bound address");
                listening = Some(address.parse::<std::net::SocketAddr>().unwrap());
            }
            Ok(line) if line.trim_start().starts_with("thegn://pair?") => {
                pairing = thegn_core::control::PairingUrl::parse(line.trim());
            }
            Ok(_) | Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        }
    }
    let addr = listening.expect("serve announces its bound TCP address within 20s");
    let pairing = pairing.expect("serve prints its startup pairing URL within 20s");
    assert_eq!(pairing.port, addr.port());
    let socket = fixture.socket_path();

    let empty = thegn_svc::control::client::ControlClient::new(
        thegn_svc::control::client::ControlAddr::Tcp {
            addr,
            token: String::new(),
        },
    );
    empty
        .health()
        .await
        .expect("health is intentionally unauthenticated");
    let unauthorized = empty
        .me()
        .await
        .expect_err("identity requires a bearer token");
    assert_eq!(
        unauthorized
            .downcast_ref::<thegn_svc::control::client::ControlRequestError>()
            .map(thegn_svc::control::client::ControlRequestError::status),
        Some(401),
        "the server refuses the missing bearer token with HTTP 401"
    );

    let pair = empty
        .pair(&pairing.code, "itest")
        .await
        .expect("redeem startup pairing");
    let token = pair
        .get("token")
        .and_then(serde_json::Value::as_str)
        .filter(|token| !token.is_empty())
        .expect("pairing returns a nonempty read token")
        .to_owned();
    assert_eq!(
        pair.get("approved").and_then(serde_json::Value::as_bool),
        Some(true)
    );
    assert_eq!(
        pair.get("scopes").and_then(serde_json::Value::as_str),
        Some("read")
    );
    assert!(
        empty.pair(&pairing.code, "itest-again").await.is_err(),
        "pairing code is single-use"
    );

    let reader_client = thegn_svc::control::client::ControlClient::new(
        thegn_svc::control::client::ControlAddr::Tcp { addr, token },
    );
    reader_client
        .me()
        .await
        .expect("redeemed token can read identity");
    reader_client
        .sessions()
        .await
        .expect("redeemed token can read sessions");
    let unknown = thegn_svc::control::client::ControlClient::new(
        thegn_svc::control::client::ControlAddr::Tcp {
            addr,
            token: "tgp1_invalid".into(),
        },
    );
    assert!(
        unknown.me().await.is_err(),
        "unknown bearer token is refused"
    );

    let status = child
        .terminate()
        .expect("normal SIGTERM exit within five seconds");
    assert!(status.success(), "serve exits cleanly on SIGTERM: {status}");
    reader.join().unwrap();
    assert!(
        !socket.exists(),
        "serve removes its Unix socket on shutdown"
    );
}
