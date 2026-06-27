//! Resident-agent **bridge**: a framed JSON request/response (+ notification)
//! protocol over any duplex byte stream, so the host runs commands / watches
//! files *inside* a remote env over one persistent connection instead of a
//! process spawn per op. The agent is `szhost --bridge` ([`serve`]); the host
//! side is [`BridgeClient`]. This is the latency-killing + live-`fs.watch` core
//! of the thin-client ("feels local") model; it rides ssh / `sprite exec` /
//! local-pipe transports identically.
//!
//! Frames reuse the LSP Content-Length codec ([`crate::lsp::framing`]); the
//! client mirrors `LspClient` (atomic id + `HashMap<id,Sender>` correlation +
//! reader thread). The protocol is intentionally tiny: a generic `exec`
//! (the workhorse — git/gh/cli/tasks all ride it, host-side parsers unchanged),
//! plus `proc.list` and the streaming `fs.watch` (added next).

use anyhow::{Context, Result, anyhow, bail};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::io::{Read, Write};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{Sender, channel};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use crate::lsp::framing::{self, FrameDecoder};
use superzej_core::remote::GitLoc;

/// Parameters for the `exec` method: run `argv` (optionally in `cwd`, with extra
/// `env`) and return its captured output.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExecParams {
    pub argv: Vec<String>,
    #[serde(default)]
    pub cwd: Option<String>,
    #[serde(default)]
    pub env: Vec<(String, String)>,
}

/// The captured result of an `exec`. `stdout`/`stderr` are UTF-8 (lossy for any
/// non-UTF-8 bytes — git/text tooling output, incl. `-z` NUL separators which are
/// valid UTF-8, round-trips exactly).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExecResult {
    pub stdout: String,
    pub stderr: String,
    pub exit: i32,
}

#[derive(Debug, Serialize, Deserialize)]
struct Request {
    id: u64,
    method: String,
    params: serde_json::Value,
}

#[derive(Debug, Serialize, Deserialize)]
struct Response {
    id: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    ok: Option<serde_json::Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    err: Option<String>,
}

type Pending = Arc<Mutex<HashMap<u64, Sender<std::result::Result<serde_json::Value, String>>>>>;

/// The host side of the bridge: spawn-over-transport happens by the caller (it
/// hands us the connected stream's reader+writer), then `exec()` issues blocking
/// RPCs correlated by id. Cloneable handles share one connection via `Arc`.
pub struct BridgeClient {
    writer: Arc<Mutex<Box<dyn Write + Send>>>,
    next_id: AtomicU64,
    pending: Pending,
    timeout: Duration,
    _reader: std::thread::JoinHandle<()>,
    /// The spawned agent process, owned so it's killed when the client drops
    /// (subprocess transports). `None` for a caller-provided stream (tests).
    child: Mutex<Option<Child>>,
}

impl BridgeClient {
    /// Build a client over an already-connected duplex stream (the transport's
    /// reader and writer halves). For a subprocess transport these are the
    /// child's stdout and stdin; for tests, two ends of a socket/pipe.
    pub fn new(
        reader: impl Read + Send + 'static,
        writer: impl Write + Send + 'static,
    ) -> BridgeClient {
        Self::build(reader, writer, None)
    }

    /// Spawn `cmd` (e.g. `ssh host szhost --bridge`, `sprite exec … szhost
    /// --bridge`, or `szhost --bridge` locally) and talk to it over its stdio.
    /// The child is owned and killed on drop.
    pub fn spawn(mut cmd: Command) -> Result<BridgeClient> {
        cmd.stdin(Stdio::piped()).stdout(Stdio::piped());
        let mut child = cmd.spawn().context("spawn bridge agent")?;
        let stdout = child.stdout.take().context("bridge agent: no stdout")?;
        let stdin = child.stdin.take().context("bridge agent: no stdin")?;
        Ok(Self::build(stdout, stdin, Some(child)))
    }

    fn build(
        reader: impl Read + Send + 'static,
        writer: impl Write + Send + 'static,
        child: Option<Child>,
    ) -> BridgeClient {
        let pending: Pending = Arc::new(Mutex::new(HashMap::new()));
        let reader_pending = pending.clone();
        let handle = std::thread::Builder::new()
            .name("bridge-reader".into())
            .spawn(move || reader_loop(reader, reader_pending))
            .expect("spawn bridge reader");
        BridgeClient {
            writer: Arc::new(Mutex::new(Box::new(writer))),
            next_id: AtomicU64::new(1),
            pending,
            timeout: Duration::from_secs(120),
            _reader: handle,
            child: Mutex::new(child),
        }
    }

    fn call(&self, method: &str, params: serde_json::Value) -> Result<serde_json::Value> {
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        let (tx, rx) = channel();
        self.pending.lock().unwrap().insert(id, tx);
        let req = serde_json::to_string(&Request {
            id,
            method: method.to_string(),
            params,
        })?;
        {
            let mut w = self.writer.lock().unwrap();
            if let Err(e) = w.write_all(&framing::encode(&req)).and_then(|_| w.flush()) {
                self.pending.lock().unwrap().remove(&id);
                bail!("bridge write failed: {e}");
            }
        }
        match rx.recv_timeout(self.timeout) {
            Ok(Ok(v)) => Ok(v),
            Ok(Err(e)) => Err(anyhow!("bridge: {e}")),
            Err(_) => {
                self.pending.lock().unwrap().remove(&id);
                Err(anyhow!("bridge: timed out waiting for {method}"))
            }
        }
    }

    /// Run a command in the env and return its captured output.
    pub fn exec(
        &self,
        argv: &[&str],
        cwd: Option<&str>,
        env: &[(String, String)],
    ) -> Result<ExecResult> {
        let params = serde_json::to_value(ExecParams {
            argv: argv.iter().map(|s| s.to_string()).collect(),
            cwd: cwd.map(str::to_string),
            env: env.to_vec(),
        })?;
        Ok(serde_json::from_value(self.call("exec", params)?)?)
    }
}

impl Drop for BridgeClient {
    fn drop(&mut self) {
        if let Ok(mut guard) = self.child.lock()
            && let Some(mut c) = guard.take()
        {
            let _ = c.kill();
            let _ = c.wait();
        }
    }
}

// ---------------------------------------------------------------------------
// Process-global registry: the host registers a live `BridgeClient` per remote
// worktree; `superzej-svc::git`'s `run`/`run_w` consult `for_loc` to route git
// (and gh/cli/mutations) through the bridge instead of a per-op process spawn.
// Local locs never touch the registry (the hot-path fast exit).
// ---------------------------------------------------------------------------

type Registry = Mutex<HashMap<String, Arc<BridgeClient>>>;

fn registry() -> &'static Registry {
    static R: OnceLock<Registry> = OnceLock::new();
    R.get_or_init(|| Mutex::new(HashMap::new()))
}

/// The registry key for a loc, or `None` for a local worktree (no bridge).
/// Provider keys on the control prefix (unique per sandbox — it carries the
/// sprite name); ssh keys on host:port:path.
pub fn bridge_key(loc: &GitLoc) -> Option<String> {
    match loc {
        GitLoc::Local(_) => None,
        GitLoc::Provider { control_prefix, .. } => Some(control_prefix.join("\u{1f}")),
        GitLoc::Remote { ssh, path } => Some(format!("ssh:{}:{}:{}", ssh.host, ssh.port, path)),
    }
}

/// Register a live bridge for the loc identified by `key` (from [`bridge_key`]).
pub fn register(key: &str, client: Arc<BridgeClient>) {
    registry().lock().unwrap().insert(key.to_string(), client);
}

/// Drop a worktree's bridge (on close); the `BridgeClient` Drop kills the agent.
pub fn drop_key(key: &str) {
    registry().lock().unwrap().remove(key);
}

/// The live bridge for a loc, if one is registered. Returns `None` (without
/// locking) for local locs — keeps the common case off the registry mutex.
pub fn for_loc(loc: &GitLoc) -> Option<Arc<BridgeClient>> {
    let key = bridge_key(loc)?;
    registry().lock().unwrap().get(&key).cloned()
}

fn reader_loop(mut reader: impl Read, pending: Pending) {
    let mut dec = FrameDecoder::new();
    let mut buf = [0u8; 8192];
    loop {
        let n = match reader.read(&mut buf) {
            Ok(0) | Err(_) => break,
            Ok(n) => n,
        };
        dec.push(&buf[..n]);
        while let Some(body) = dec.next_message() {
            if let Ok(resp) = serde_json::from_str::<Response>(&body)
                && let Some(tx) = pending.lock().unwrap().remove(&resp.id)
            {
                let payload = match resp.err {
                    Some(e) => Err(e),
                    None => Ok(resp.ok.unwrap_or(serde_json::Value::Null)),
                };
                let _ = tx.send(payload);
            }
        }
    }
    // Stream closed — unblock any waiters so they don't hang to the deadline.
    for (_, tx) in pending.lock().unwrap().drain() {
        let _ = tx.send(Err("bridge connection closed".into()));
    }
}

/// The agent side (`szhost --bridge`): read framed requests off `reader`, run
/// them, write framed responses to `writer`, until the stream closes. Runs
/// *inside* the env. Blocking/synchronous (one request at a time is fine — the
/// host issues sequentially per connection; concurrency rides separate threads).
pub fn serve(mut reader: impl Read, mut writer: impl Write) {
    let mut dec = FrameDecoder::new();
    let mut buf = [0u8; 8192];
    loop {
        let n = match reader.read(&mut buf) {
            Ok(0) | Err(_) => break,
            Ok(n) => n,
        };
        dec.push(&buf[..n]);
        while let Some(body) = dec.next_message() {
            let Ok(req) = serde_json::from_str::<Request>(&body) else {
                continue;
            };
            let resp = handle(&req);
            let s = match serde_json::to_string(&resp) {
                Ok(s) => s,
                Err(_) => continue,
            };
            if writer
                .write_all(&framing::encode(&s))
                .and_then(|_| writer.flush())
                .is_err()
            {
                return;
            }
        }
    }
}

fn handle(req: &Request) -> Response {
    let ok = |v: serde_json::Value| Response {
        id: req.id,
        ok: Some(v),
        err: None,
    };
    let err = |e: String| Response {
        id: req.id,
        ok: None,
        err: Some(e),
    };
    match req.method.as_str() {
        "exec" => match serde_json::from_value::<ExecParams>(req.params.clone()) {
            Ok(p) => match do_exec(&p) {
                Ok(r) => match serde_json::to_value(r) {
                    Ok(v) => ok(v),
                    Err(e) => err(e.to_string()),
                },
                Err(e) => err(e.to_string()),
            },
            Err(e) => err(format!("bad exec params: {e}")),
        },
        other => err(format!("unknown method: {other}")),
    }
}

fn do_exec(p: &ExecParams) -> Result<ExecResult> {
    let Some((cmd, args)) = p.argv.split_first() else {
        bail!("empty argv");
    };
    let mut c = std::process::Command::new(cmd);
    c.args(args);
    if let Some(cwd) = &p.cwd {
        c.current_dir(cwd);
    }
    for (k, v) in &p.env {
        c.env(k, v);
    }
    let out = c
        .output()
        .with_context(|| format!("exec {}", p.argv.join(" ")))?;
    Ok(ExecResult {
        stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
        exit: out.status.code().unwrap_or(-1),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{TcpListener, TcpStream};

    /// Connect a client to a freshly-served agent over a loopback socket (a real
    /// duplex byte stream — the same shape ssh/sprite-exec stdio provides).
    fn connect() -> BridgeClient {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        std::thread::spawn(move || {
            if let Ok((sock, _)) = listener.accept() {
                serve(sock.try_clone().unwrap(), sock);
            }
        });
        let sock = TcpStream::connect(addr).unwrap();
        BridgeClient::new(sock.try_clone().unwrap(), sock)
    }

    #[test]
    fn exec_roundtrip_success_and_failure() {
        let c = connect();
        let r = c.exec(&["echo", "hello-bridge"], None, &[]).unwrap();
        assert_eq!(r.exit, 0);
        assert_eq!(r.stdout.trim(), "hello-bridge");
        // Non-zero exit is reported (not an RPC error).
        let r2 = c.exec(&["sh", "-c", "exit 3"], None, &[]).unwrap();
        assert_eq!(r2.exit, 3);
        // Many sequential calls reuse the one connection.
        for i in 0..5 {
            let r = c
                .exec(&["sh", "-c", &format!("echo {i}")], None, &[])
                .unwrap();
            assert_eq!(r.stdout.trim(), i.to_string());
        }
    }

    #[test]
    fn exec_git_status_parses_like_cli() {
        // Prove the git-over-bridge path: run git in a temp repo via exec, and the
        // existing CliGit porcelain parse shape works on the returned stdout.
        let dir = std::env::temp_dir().join(format!("sz-bridge-git-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let d = dir.to_string_lossy().into_owned();
        let c = connect();
        assert_eq!(
            c.exec(&["git", "init", "-q"], Some(&d), &[]).unwrap().exit,
            0
        );
        std::fs::write(dir.join("new.rs"), b"fn main(){}").unwrap();
        let r = c
            .exec(
                &["git", "-C", &d, "status", "--porcelain=v1", "-z"],
                None,
                &[],
            )
            .unwrap();
        assert_eq!(r.exit, 0);
        // Untracked file shows as "?? new.rs" in porcelain.
        assert!(r.stdout.contains("?? new.rs"), "porcelain: {:?}", r.stdout);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn unknown_method_is_an_error_not_a_hang() {
        let c = connect();
        let e = c.call("nope", serde_json::Value::Null).unwrap_err();
        assert!(e.to_string().contains("unknown method"));
    }

    /// End-to-end: a registered bridge serves `GixGit::status` for a `Provider`
    /// loc — registry lookup → `run()`-routing → bridge `exec` → CliGit parse.
    /// Proves the whole git-through-the-bridge wiring with no sprite.
    #[test]
    fn gix_status_routes_through_registered_bridge() {
        use crate::git::{GitBackend, GixGit};
        use superzej_core::remote::GitLoc;

        let dir = std::env::temp_dir().join(format!("sz-bridge-route-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let d = dir.to_string_lossy().into_owned();

        let client = Arc::new(connect());
        client.exec(&["git", "init", "-q"], Some(&d), &[]).unwrap();
        std::fs::write(dir.join("a.rs"), b"x").unwrap();

        // A provider loc whose key we register; path = the (here local) repo dir.
        let loc = GitLoc::provider(vec!["test-bridge".into(), d.clone()], d.clone());
        let key = bridge_key(&loc).unwrap();
        register(&key, client);

        // GixGit (remote → CliGit → run → bridge) returns the repo's real status.
        let st = GixGit::new().status(&loc).unwrap();
        assert!(
            st.iter().any(|f| f.path == "a.rs"),
            "expected a.rs in {st:?}"
        );

        drop_key(&key);
        assert!(for_loc(&loc).is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
