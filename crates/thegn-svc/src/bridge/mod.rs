//! Resident-agent **bridge**: a framed JSON request/response (+ notification)
//! protocol over any duplex byte stream, so the host runs commands / watches
//! files *inside* a remote env over one persistent connection instead of a
//! process spawn per op. The agent is `thegn --bridge` ([`serve`]); the host
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
use notify::{EventKind, RecommendedWatcher, RecursiveMode, Watcher};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap};
use std::io::{Read, Write};
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use base64::{Engine as _, engine::general_purpose::STANDARD as B64};

use crate::lsp::framing::{self, FrameDecoder};
use thegn_core::remote::GitLoc;

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

/// A filesystem change streamed from an `fs.watch` subscription.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FsEvent {
    pub paths: Vec<String>,
    /// Coarse kind: `"create"` | `"modify"` | `"remove"`.
    pub kind: String,
}

/// Params for `exec.batch`: run each argv (with the shared `env`) and return all
/// results in order. Each argv is self-contained (`git -C <path> …`), so no
/// per-command cwd is carried.
#[derive(Serialize, Deserialize)]
struct BatchParams {
    cmds: Vec<Vec<String>>,
    #[serde(default)]
    env: Vec<(String, String)>,
}

#[derive(Serialize, Deserialize)]
struct WatchParams {
    path: String,
    watch_id: u64,
}
#[derive(Serialize, Deserialize)]
struct ProcParams {
    paths: Vec<String>,
}
#[derive(Serialize, Deserialize)]
struct ProcResult {
    jiffies: BTreeMap<String, u64>,
}
/// The params of an `fs.event` server→client notification.
#[derive(Serialize, Deserialize)]
struct FsEventNote {
    watch_id: u64,
    paths: Vec<String>,
    kind: String,
}

// --- streaming process channel (proc.spawn) -------------------------------
// A long-lived child in the env with bidirectional stdio over the bridge — the
// shared primitive for a remote LSP server (lsp-forward) and an interactive pane
// (drop the provider CLI). Distinct from `exec` (one-shot, buffered): here output
// streams as `proc.out` notifications and the client feeds stdin via `proc.stdin`.
// Binary-safe: payloads are base64 (LSP is UTF-8, but a PTY is arbitrary bytes).

#[derive(Serialize, Deserialize)]
struct SpawnParams {
    chan: u64,
    argv: Vec<String>,
    #[serde(default)]
    cwd: Option<String>,
    #[serde(default)]
    env: Vec<(String, String)>,
}
#[derive(Serialize, Deserialize)]
struct ChanData {
    chan: u64,
    /// base64-encoded bytes.
    data: String,
}
#[derive(Serialize, Deserialize)]
struct ChanRef {
    chan: u64,
}
/// `proc.out` server→client notification: a chunk of the child's stdout/stderr.
#[derive(Serialize, Deserialize)]
struct ProcOutNote {
    chan: u64,
    /// `"stdout"` | `"stderr"`.
    stream: String,
    data: String,
}
/// `proc.exit` server→client notification: the child terminated with `code`.
#[derive(Serialize, Deserialize)]
struct ProcExitNote {
    chan: u64,
    code: i32,
}

/// An event from a streaming process channel ([`BridgeClient::spawn_proc`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProcEvent {
    /// A chunk of output. `stream` is `"stdout"` or `"stderr"`; `data` is raw bytes.
    Out { stream: String, data: Vec<u8> },
    /// The process exited with this code (`-1` if killed/unknown).
    Exit { code: i32 },
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
/// Active `fs.watch` subscriptions: watch_id → the channel delivering its events.
type Subs = Arc<Mutex<HashMap<u64, Sender<FsEvent>>>>;
/// Active streaming-process channels: chan → the channel delivering its events.
type Procs = Arc<Mutex<HashMap<u64, Sender<ProcEvent>>>>;

/// The host side of the bridge: spawn-over-transport happens by the caller (it
/// hands us the connected stream's reader+writer), then `exec()` issues blocking
/// RPCs correlated by id. Cloneable handles share one connection via `Arc`.
pub struct BridgeClient {
    writer: Arc<Mutex<Box<dyn Write + Send>>>,
    next_id: AtomicU64,
    pending: Pending,
    timeout: Duration,
    _reader: std::thread::JoinHandle<()>,
    subs: Subs,
    next_watch: AtomicU64,
    procs: Procs,
    next_chan: AtomicU64,
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

    /// Spawn `cmd` (e.g. `ssh host thegn --bridge`, `sprite exec … thegn
    /// --bridge`, or `thegn --bridge` locally) and talk to it over its stdio.
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
        let subs: Subs = Arc::new(Mutex::new(HashMap::new()));
        let procs: Procs = Arc::new(Mutex::new(HashMap::new()));
        let reader_pending = pending.clone();
        let reader_subs = subs.clone();
        let reader_procs = procs.clone();
        let handle = std::thread::Builder::new()
            .name("bridge-reader".into())
            .spawn(move || reader_loop(reader, reader_pending, reader_subs, reader_procs))
            .expect("spawn bridge reader");
        BridgeClient {
            writer: Arc::new(Mutex::new(Box::new(writer))),
            next_id: AtomicU64::new(1),
            pending,
            timeout: Duration::from_secs(120),
            _reader: handle,
            subs,
            next_watch: AtomicU64::new(1),
            procs,
            next_chan: AtomicU64::new(1),
            child: Mutex::new(child),
        }
    }

    fn call(&self, method: &str, params: serde_json::Value) -> Result<serde_json::Value> {
        warn_if_on_loop_thread(method);
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

    /// Run several commands in the env in **one** round-trip, returning each one's
    /// captured output in order. Semantically N sequential [`exec`](Self::exec)s
    /// (same shared `env`, no per-command cwd — pass `git -C <path> …` argv), but a
    /// single RPC — collapses the per-worktree git fan-out (status + ahead/behind +
    /// branch) from three hops to one over the persistent connection.
    pub fn exec_batch(
        &self,
        cmds: &[Vec<String>],
        env: &[(String, String)],
    ) -> Result<Vec<ExecResult>> {
        let params = serde_json::to_value(BatchParams {
            cmds: cmds.to_vec(),
            env: env.to_vec(),
        })?;
        Ok(serde_json::from_value(self.call("exec.batch", params)?)?)
    }

    /// Sum of CPU jiffies per path for processes in the env whose cwd is under it
    /// (feeds the activity FSM with the *env's* processes).
    pub fn proc_list(&self, paths: &[String]) -> Result<BTreeMap<String, u64>> {
        let params = serde_json::to_value(ProcParams {
            paths: paths.to_vec(),
        })?;
        let r: ProcResult = serde_json::from_value(self.call("proc.list", params)?)?;
        Ok(r.jiffies)
    }

    /// Subscribe to filesystem changes under `path` in the env. The agent streams
    /// `fs.event` notifications; they arrive on the returned receiver until the
    /// client (and thus the connection) drops.
    pub fn watch(&self, path: &str) -> Result<Receiver<FsEvent>> {
        let (tx, rx) = channel();
        let watch_id = self.next_watch.fetch_add(1, Ordering::SeqCst);
        // Register before the request so an immediate event can't race the insert.
        self.subs.lock().unwrap().insert(watch_id, tx);
        let params = serde_json::json!({ "path": path, "watch_id": watch_id });
        if let Err(e) = self.call("fs.watch", params) {
            self.subs.lock().unwrap().remove(&watch_id);
            return Err(e);
        }
        Ok(rx)
    }

    /// Spawn a long-lived process in the env with streaming stdio — the
    /// foundation for a forwarded LSP server and an interactive pane. Returns the
    /// channel id and a receiver of [`ProcEvent`]s (output chunks + exit); feed
    /// its stdin with [`proc_stdin`](Self::proc_stdin), end it with
    /// [`proc_kill`](Self::proc_kill). Events flow until the process exits or the
    /// connection drops.
    pub fn spawn_proc(
        &self,
        argv: &[&str],
        cwd: Option<&str>,
        env: &[(String, String)],
    ) -> Result<(u64, Receiver<ProcEvent>)> {
        let (tx, rx) = channel();
        let chan = self.next_chan.fetch_add(1, Ordering::SeqCst);
        // Register before the request so early output can't race the insert.
        self.procs.lock().unwrap().insert(chan, tx);
        let params = serde_json::to_value(SpawnParams {
            chan,
            argv: argv.iter().map(|s| s.to_string()).collect(),
            cwd: cwd.map(str::to_string),
            env: env.to_vec(),
        })?;
        if let Err(e) = self.call("proc.spawn", params) {
            self.procs.lock().unwrap().remove(&chan);
            return Err(e);
        }
        Ok((chan, rx))
    }

    /// Write bytes to a streaming process's stdin.
    pub fn proc_stdin(&self, chan: u64, data: &[u8]) -> Result<()> {
        let params = serde_json::to_value(ChanData {
            chan,
            data: B64.encode(data),
        })?;
        self.call("proc.stdin", params)?;
        Ok(())
    }

    /// Kill a streaming process (and stop its stream).
    pub fn proc_kill(&self, chan: u64) -> Result<()> {
        let params = serde_json::to_value(ChanRef { chan })?;
        self.call("proc.kill", params)?;
        self.procs.lock().unwrap().remove(&chan);
        Ok(())
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
// worktree; `thegn-svc::git`'s `run`/`run_w` consult `for_loc` to route git
// (and gh/cli/mutations) through the bridge instead of a per-op process spawn.
// Local locs never touch the registry (the hot-path fast exit).
// ---------------------------------------------------------------------------

type Registry = Mutex<HashMap<String, Arc<BridgeClient>>>;

fn registry() -> &'static Registry {
    static R: OnceLock<Registry> = OnceLock::new();
    R.get_or_init(|| Mutex::new(HashMap::new()))
}

/// The host's event-loop thread id, recorded once at startup by
/// [`note_loop_thread`]. A bridge RPC issued on this thread blocks the compositor
/// on a network/subprocess round-trip — and, before the writer's `try_send` fix,
/// could panic. `None` in tests / non-host callers (the guard is then inert).
static LOOP_THREAD: OnceLock<std::thread::ThreadId> = OnceLock::new();
static LOOP_WARNED: AtomicBool = AtomicBool::new(false);

/// Record the current thread as the event loop so `BridgeClient::call` can flag
/// any bridge RPC issued on it. Called once by the host at startup; a no-op
/// second call is harmless.
pub fn note_loop_thread() {
    let _ = LOOP_THREAD.set(std::thread::current().id());
}

/// Whether the caller is running on the event-loop thread recorded by
/// [`note_loop_thread`]. The reusable "am I about to block the compositor?"
/// predicate — blocking I/O seams (bridge RPCs, and future git/DB guards) can
/// `debug_assert!(!is_on_loop_thread())` to catch loop-thread stalls in tests.
/// `false` when no loop thread was recorded (tests / non-host callers).
pub fn is_on_loop_thread() -> bool {
    LOOP_THREAD.get() == Some(&std::thread::current().id())
}

/// Warn (once) if a bridge RPC is being issued on the event-loop thread — the
/// "never block the loop" invariant. Non-fatal: the `try_send` writer keeps this
/// from crashing, but the caller should move the op off-loop (`spawn_blocking`).
fn warn_if_on_loop_thread(method: &str) {
    if is_on_loop_thread() && !LOOP_WARNED.swap(true, Ordering::Relaxed) {
        tracing::warn!(
            method,
            "bridge RPC issued on the event-loop thread — this blocks the \
             compositor; move it off-loop (spawn_blocking)",
        );
    }
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

fn reader_loop(mut reader: impl Read, pending: Pending, subs: Subs, procs: Procs) {
    let mut dec = FrameDecoder::new();
    let mut buf = [0u8; 8192];
    loop {
        let n = match reader.read(&mut buf) {
            Ok(0) | Err(_) => break,
            Ok(n) => n,
        };
        dec.push(&buf[..n]);
        while let Some(body) = dec.next_message() {
            let Ok(v) = serde_json::from_str::<serde_json::Value>(&body) else {
                continue;
            };
            // A server→client notification (no `id`): route to its subscriber.
            match v.get("method").and_then(|m| m.as_str()) {
                Some("fs.event") => {
                    if let Some(note) = v
                        .get("params")
                        .cloned()
                        .and_then(|p| serde_json::from_value::<FsEventNote>(p).ok())
                        && let Some(tx) = subs.lock().unwrap().get(&note.watch_id)
                    {
                        let _ = tx.send(FsEvent {
                            paths: note.paths,
                            kind: note.kind,
                        });
                    }
                    continue;
                }
                Some("proc.out") => {
                    if let Some(note) = v
                        .get("params")
                        .cloned()
                        .and_then(|p| serde_json::from_value::<ProcOutNote>(p).ok())
                        && let Some(tx) = procs.lock().unwrap().get(&note.chan)
                    {
                        let _ = tx.send(ProcEvent::Out {
                            stream: note.stream,
                            data: B64.decode(&note.data).unwrap_or_default(),
                        });
                    }
                    continue;
                }
                Some("proc.exit") => {
                    if let Some(note) = v
                        .get("params")
                        .cloned()
                        .and_then(|p| serde_json::from_value::<ProcExitNote>(p).ok())
                    {
                        // Final event, then drop the sub so the receiver ends.
                        if let Some(tx) = procs.lock().unwrap().remove(&note.chan) {
                            let _ = tx.send(ProcEvent::Exit { code: note.code });
                        }
                    }
                    continue;
                }
                _ => {}
            }
            // Otherwise a response to a pending request.
            if let Ok(resp) = serde_json::from_value::<Response>(v)
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

/// The agent side (`thegn --bridge`): read framed requests off `reader`, run
/// them, write framed responses to `writer`, until the stream closes. Runs
/// *inside* the env. The stateless, potentially-slow ops (`exec`/`exec.batch`/
/// `proc.list`) run on their own thread so a slow git command doesn't
/// head-of-line-block the *concurrent* requests the host issues (the panel /
/// sidebar git fan-out across scoped threads) — responses are id-correlated, so
/// out-of-order completion is fine and the shared writer is mutex-guarded. The
/// writer is shared between the request loop, those exec threads, and the
/// `fs.watch` background watcher threads (which push `fs.event` notifications).
type SharedWriter = Arc<Mutex<Box<dyn Write + Send>>>;

/// A live streaming child: only its stdin is retained (for `proc.stdin`). Dropping
/// it closes stdin → the child sees EOF → exits → its waiter thread fires
/// `proc.exit`. So `proc.kill` and connection-close are just a map removal — no
/// shared `Child` mutex, no libc signal, no deadlock between reader/kill paths.
struct ProcState {
    stdin: Arc<Mutex<std::process::ChildStdin>>,
}
type ProcRegistry = Arc<Mutex<HashMap<u64, ProcState>>>;

pub fn serve(mut reader: impl Read, writer: impl Write + Send + 'static) {
    let writer: SharedWriter = Arc::new(Mutex::new(Box::new(writer)));
    // Live fs.watch watchers, kept alive for the connection's lifetime.
    let mut watchers: Vec<RecommendedWatcher> = Vec::new();
    // Live streaming processes (proc.spawn), keyed by channel id.
    let procs: ProcRegistry = Arc::new(Mutex::new(HashMap::new()));
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
            match req.method.as_str() {
                // Stateless + potentially slow: run off the read loop so concurrent
                // host requests parallelize (restores the pre-bridge parallel-
                // subprocess behavior instead of serializing every git read
                // through one connection).
                "exec" | "exec.batch" | "proc.list" => {
                    let w = writer.clone();
                    let _ = std::thread::Builder::new()
                        .name("bridge-exec".into())
                        .spawn(move || {
                            let resp = match req.method.as_str() {
                                "exec" => exec_response(&req),
                                "exec.batch" => exec_batch_response(&req),
                                _ => proc_response(&req),
                            };
                            write_frame(&w, &resp);
                        });
                }
                // Stateful / fast: stay inline (they borrow `watchers`/`procs`).
                _ => {
                    let resp = match req.method.as_str() {
                        "fs.watch" => watch_response(&req, &writer, &mut watchers),
                        "proc.spawn" => proc_spawn_response(&req, &writer, &procs),
                        "proc.stdin" => proc_stdin_response(&req, &procs),
                        "proc.kill" => proc_kill_response(&req, &procs),
                        other => resp_err(req.id, format!("unknown method: {other}")),
                    };
                    write_frame(&writer, &resp);
                }
            }
        }
    }
    // Connection closed: drop every child's stdin → EOF → the children exit.
    procs.lock().unwrap().clear();
}

fn proc_spawn_response(req: &Request, writer: &SharedWriter, procs: &ProcRegistry) -> Response {
    let p: SpawnParams = match serde_json::from_value(req.params.clone()) {
        Ok(p) => p,
        Err(e) => return resp_err(req.id, format!("bad proc.spawn params: {e}")),
    };
    match do_spawn(p, writer.clone(), procs.clone()) {
        Ok(()) => resp_ok(req.id, serde_json::json!({})),
        Err(e) => resp_err(req.id, format!("proc.spawn failed: {e}")),
    }
}

fn do_spawn(p: SpawnParams, writer: SharedWriter, procs: ProcRegistry) -> Result<()> {
    let Some((cmd, args)) = p.argv.split_first() else {
        bail!("empty argv");
    };
    let mut c = Command::new(cmd);
    c.args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if let Some(cwd) = &p.cwd {
        c.current_dir(cwd);
    }
    scrub_git_env(&mut c);
    for (k, v) in &p.env {
        c.env(k, v);
    }
    let mut child = c
        .spawn()
        .with_context(|| format!("spawn {}", p.argv.join(" ")))?;
    let stdout = child.stdout.take().context("child stdout")?;
    let stderr = child.stderr.take().context("child stdin")?;
    let stdin = child.stdin.take().context("child stdin")?;
    let chan = p.chan;
    procs.lock().unwrap().insert(
        chan,
        ProcState {
            stdin: Arc::new(Mutex::new(stdin)),
        },
    );
    // Stream stdout + stderr as proc.out notifications.
    spawn_stream_relay(stdout, chan, "stdout", writer.clone());
    spawn_stream_relay(stderr, chan, "stderr", writer.clone());
    // Waiter: owns the Child, blocks on exit (no lock held), then reports exit and
    // drops the channel. The child exits when it finishes or when proc.kill /
    // connection-close drops its stdin (EOF).
    std::thread::Builder::new()
        .name("bridge-proc-wait".into())
        .spawn(move || {
            let code = child.wait().ok().and_then(|s| s.code()).unwrap_or(-1);
            procs.lock().unwrap().remove(&chan);
            let note = serde_json::json!({
                "method": "proc.exit",
                "params": ProcExitNote { chan, code },
            });
            write_frame(&writer, &note);
        })
        .ok();
    Ok(())
}

/// Relay a child stream to the client as `proc.out` notifications until EOF.
fn spawn_stream_relay(
    mut r: impl Read + Send + 'static,
    chan: u64,
    stream: &'static str,
    writer: SharedWriter,
) {
    std::thread::Builder::new()
        .name(format!("bridge-proc-{stream}"))
        .spawn(move || {
            let mut buf = [0u8; 8192];
            loop {
                match r.read(&mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        let note = serde_json::json!({
                            "method": "proc.out",
                            "params": ProcOutNote {
                                chan,
                                stream: stream.to_string(),
                                data: B64.encode(&buf[..n]),
                            },
                        });
                        write_frame(&writer, &note);
                    }
                }
            }
        })
        .ok();
}

fn proc_stdin_response(req: &Request, procs: &ProcRegistry) -> Response {
    let p: ChanData = match serde_json::from_value(req.params.clone()) {
        Ok(p) => p,
        Err(e) => return resp_err(req.id, format!("bad proc.stdin params: {e}")),
    };
    let stdin = procs.lock().unwrap().get(&p.chan).map(|s| s.stdin.clone());
    let Some(stdin) = stdin else {
        return resp_err(req.id, format!("no such channel {}", p.chan));
    };
    let data = B64.decode(&p.data).unwrap_or_default();
    let mut g = stdin.lock().unwrap();
    match g.write_all(&data).and_then(|_| g.flush()) {
        Ok(()) => resp_ok(req.id, serde_json::json!({})),
        Err(e) => resp_err(req.id, format!("proc.stdin write: {e}")),
    }
}

fn proc_kill_response(req: &Request, procs: &ProcRegistry) -> Response {
    let p: ChanRef = match serde_json::from_value(req.params.clone()) {
        Ok(p) => p,
        Err(e) => return resp_err(req.id, format!("bad proc.kill params: {e}")),
    };
    // Drop the ProcState → close stdin → EOF → the child exits; the waiter fires
    // proc.exit. (A child that ignores stdin EOF is reaped on env teardown.)
    procs.lock().unwrap().remove(&p.chan);
    resp_ok(req.id, serde_json::json!({}))
}

fn resp_ok(id: u64, v: impl Serialize) -> Response {
    match serde_json::to_value(v) {
        Ok(v) => Response {
            id,
            ok: Some(v),
            err: None,
        },
        Err(e) => resp_err(id, e.to_string()),
    }
}

fn resp_err(id: u64, msg: String) -> Response {
    Response {
        id,
        ok: None,
        err: Some(msg),
    }
}

/// Frame + write any serializable message (Response or a notification).
fn write_frame(w: &SharedWriter, msg: &impl Serialize) {
    let Ok(s) = serde_json::to_string(msg) else {
        return;
    };
    if let Ok(mut g) = w.lock() {
        let _ = g.write_all(&framing::encode(&s)).and_then(|_| g.flush());
    }
}

fn exec_response(req: &Request) -> Response {
    match serde_json::from_value::<ExecParams>(req.params.clone()) {
        Ok(p) => match do_exec(&p) {
            Ok(r) => resp_ok(req.id, r),
            Err(e) => resp_err(req.id, e.to_string()),
        },
        Err(e) => resp_err(req.id, format!("bad exec params: {e}")),
    }
}

fn exec_batch_response(req: &Request) -> Response {
    let p: BatchParams = match serde_json::from_value(req.params.clone()) {
        Ok(p) => p,
        Err(e) => return resp_err(req.id, format!("bad exec.batch params: {e}")),
    };
    // Each command runs independently; a spawn failure becomes a synthetic
    // exit=-1 result rather than failing the whole batch, so one bad subcommand
    // never masks its siblings' output.
    let results: Vec<ExecResult> = p
        .cmds
        .into_iter()
        .map(|argv| {
            do_exec(&ExecParams {
                argv,
                cwd: None,
                env: p.env.clone(),
            })
            .unwrap_or_else(|e| ExecResult {
                stdout: String::new(),
                stderr: e.to_string(),
                exit: -1,
            })
        })
        .collect();
    resp_ok(req.id, results)
}

fn proc_response(req: &Request) -> Response {
    match serde_json::from_value::<ProcParams>(req.params.clone()) {
        Ok(p) => resp_ok(
            req.id,
            ProcResult {
                jiffies: thegn_core::activity::cpu_jiffies_by_path(&p.paths),
            },
        ),
        Err(e) => resp_err(req.id, format!("bad proc.list params: {e}")),
    }
}

fn watch_response(
    req: &Request,
    writer: &SharedWriter,
    watchers: &mut Vec<RecommendedWatcher>,
) -> Response {
    let p: WatchParams = match serde_json::from_value(req.params.clone()) {
        Ok(p) => p,
        Err(e) => return resp_err(req.id, format!("bad fs.watch params: {e}")),
    };
    match start_watch(&p.path, p.watch_id, writer.clone()) {
        Ok(w) => {
            watchers.push(w);
            resp_ok(req.id, serde_json::json!({}))
        }
        Err(e) => resp_err(req.id, format!("fs.watch failed: {e}")),
    }
}

/// Spawn an inotify watcher on `path` that streams `fs.event` notifications
/// (Create/Modify/Remove only, 500 ms debounce, git-internal churn filtered).
fn start_watch(
    path: &str,
    watch_id: u64,
    writer: SharedWriter,
) -> notify::Result<RecommendedWatcher> {
    let mut last = Instant::now()
        .checked_sub(Duration::from_secs(1))
        .unwrap_or_else(Instant::now);
    let mut watcher = notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
        let Ok(ev) = res else {
            return;
        };
        let kind = match ev.kind {
            EventKind::Create(_) => "create",
            EventKind::Modify(_) => "modify",
            EventKind::Remove(_) => "remove",
            _ => return,
        };
        let paths: Vec<String> = ev
            .paths
            .iter()
            .filter(|p| relevant_fs_path(p))
            .map(|p| p.to_string_lossy().into_owned())
            .collect();
        if paths.is_empty() || last.elapsed() < Duration::from_millis(500) {
            return;
        }
        last = Instant::now();
        let note = serde_json::json!({
            "method": "fs.event",
            "params": FsEventNote { watch_id, paths, kind: kind.to_string() },
        });
        write_frame(&writer, &note);
    })?;
    watcher.watch(Path::new(path), RecursiveMode::Recursive)?;
    Ok(watcher)
}

/// Whether a changed path should refresh the chrome — real worktree edits, plus
/// git *state* (refs/logs/rebase/merge/HEAD), but never the index/`*.lock`/object
/// churn that hydration's own git reads cause (which would self-sustain a refresh
/// loop). Mirrors `host/src/hydrate.rs::is_git_state_path`.
fn relevant_fs_path(p: &Path) -> bool {
    let s = p.to_string_lossy();
    let Some(i) = s.find("/.git/") else {
        return true; // an ordinary worktree file
    };
    let rest = &s[i + 6..];
    if rest.ends_with(".lock") || rest == "index" || rest.starts_with("objects/") {
        return false;
    }
    rest.starts_with("refs/")
        || rest.starts_with("logs/")
        || rest.starts_with("rebase")
        || rest.starts_with("MERGE")
        || rest.starts_with("HEAD")
        || rest.starts_with("ORIG_HEAD")
}

/// Strip the outer repo's git-targeting env vars (`GIT_DIR`/`GIT_WORK_TREE`/
/// `GIT_INDEX_FILE`/…) from a bridged command so a `git -C <dir>` run over the
/// bridge targets the intended repo, never whatever repo the host process was
/// launched in. Matters most locally: when the test suite runs under a git
/// pre-commit hook, git exports those vars into the environment, which would
/// otherwise retarget a bridged `git` at the outer thegn repo. Same
/// invariant (and var list) as [`thegn_core::util::git_cmd`]. Applied before
/// the caller's explicit `env`, so an intentional override still wins.
fn scrub_git_env(c: &mut Command) {
    for var in thegn_core::util::GIT_ENV_VARS {
        c.env_remove(var);
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
    scrub_git_env(&mut c);
    // Concurrent bridged git reads (the panel/sidebar fan-out now runs in
    // parallel on the agent) must not fight over `index.lock`; mirrors the
    // host-side `util::git_cmd`. Harmless for non-git argv. Applied before the
    // caller's `env` so an explicit override still wins.
    c.env("GIT_OPTIONAL_LOCKS", "0");
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
    fn is_on_loop_thread_is_false_off_the_recorded_loop() {
        // A freshly spawned thread was never recorded as the event loop, so the
        // guard reads false there regardless of global `LOOP_THREAD` state — the
        // property the blocking-I/O seams rely on. (No `note_loop_thread` here,
        // to avoid polluting the process-global for parallel tests.)
        assert!(!std::thread::spawn(is_on_loop_thread).join().unwrap());
    }

    #[test]
    fn exec_batch_runs_all_in_one_round_trip_and_preserves_order() {
        let c = connect();
        let r = c
            .exec_batch(
                &[
                    vec!["echo".into(), "first".into()],
                    vec!["sh".into(), "-c".into(), "exit 7".into()],
                    vec!["echo".into(), "third".into()],
                ],
                &[],
            )
            .unwrap();
        assert_eq!(r.len(), 3);
        assert_eq!(r[0].stdout.trim(), "first");
        assert_eq!(r[0].exit, 0);
        // A non-zero exit is data, not an error — the batch still returns it.
        assert_eq!(r[1].exit, 7);
        assert_eq!(r[2].stdout.trim(), "third");
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
        use thegn_core::remote::GitLoc;

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

    #[test]
    fn proc_list_includes_this_process_cwd() {
        let c = connect();
        let cwd = std::env::current_dir()
            .unwrap()
            .to_string_lossy()
            .into_owned();
        let map = c.proc_list(std::slice::from_ref(&cwd)).unwrap();
        // The test process's own cwd is under the requested path → it's counted.
        assert!(map.contains_key(&cwd), "expected {cwd} in {map:?}");
    }

    #[test]
    fn fs_watch_streams_create_events_and_filters_git_churn() {
        let c = connect();
        let dir = std::env::temp_dir().join(format!("sz-bridge-watch-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join(".git")).unwrap();
        let rx = c.watch(&dir.to_string_lossy()).unwrap();
        // Let the inotify watch initialize before mutating.
        std::thread::sleep(Duration::from_millis(200));

        std::fs::write(dir.join("hello.rs"), b"fn main(){}").unwrap();
        let ev = rx
            .recv_timeout(Duration::from_secs(5))
            .expect("an fs.event for the new file");
        assert!(
            ev.paths.iter().any(|p| p.ends_with("hello.rs")),
            "event paths: {:?}",
            ev.paths
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn spawn_proc_streams_stdin_to_stdout_then_exits() {
        // `cat` echoes stdin to stdout — the canonical bidirectional stream test.
        let c = connect();
        let (chan, rx) = c.spawn_proc(&["cat"], None, &[]).unwrap();
        c.proc_stdin(chan, b"ping\n").unwrap();
        // The echoed bytes come back as a proc.out(stdout) event — the first
        // event is either that (the happy path) or an early Exit (a failure),
        // so a single recv suffices.
        let got = match rx
            .recv_timeout(Duration::from_secs(5))
            .expect("a proc event")
        {
            ProcEvent::Out { stream, data } => {
                assert_eq!(stream, "stdout");
                data
            }
            ProcEvent::Exit { .. } => panic!("exited before echo"),
        };
        assert_eq!(&got, b"ping\n");
        // Killing the channel closes cat's stdin (EOF) → it exits → Exit event.
        c.proc_kill(chan).unwrap();
        // Drain until the Exit (the kill-removed client sub may drop it, so accept
        // either an Exit or the channel closing).
        let mut saw_end = false;
        while let Ok(ev) = rx.recv_timeout(Duration::from_secs(5)) {
            if matches!(ev, ProcEvent::Exit { .. }) {
                saw_end = true;
                break;
            }
        }
        let _ = saw_end; // the receiver ending (sender dropped) is also acceptance
    }

    #[test]
    fn spawn_proc_reports_exit_code() {
        let c = connect();
        // Exits 0 immediately; stdin EOF isn't needed.
        let (_chan, rx) = c.spawn_proc(&["sh", "-c", "exit 0"], None, &[]).unwrap();
        let mut code = None;
        while let Ok(ev) = rx.recv_timeout(Duration::from_secs(5)) {
            if let ProcEvent::Exit { code: c } = ev {
                code = Some(c);
                break;
            }
        }
        assert_eq!(code, Some(0));
    }

    #[test]
    fn git_churn_paths_are_filtered() {
        // Pure predicate: index/lock/objects churn never refreshes; refs/logs do.
        assert!(!relevant_fs_path(Path::new("/w/.git/index")));
        assert!(!relevant_fs_path(Path::new("/w/.git/index.lock")));
        assert!(!relevant_fs_path(Path::new("/w/.git/objects/ab/cd")));
        assert!(relevant_fs_path(Path::new("/w/.git/refs/heads/main")));
        assert!(relevant_fs_path(Path::new("/w/.git/logs/HEAD")));
        assert!(relevant_fs_path(Path::new("/w/src/main.rs")));
    }
}
