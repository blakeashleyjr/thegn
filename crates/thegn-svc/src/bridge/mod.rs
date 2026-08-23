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
    /// RPC deadline for the default/write path (fetch/push/pull run long).
    timeout: Duration,
    /// Shorter deadline for interactive read-only ops (glyph fan-out, status).
    read_timeout: Duration,
    /// Set once by the reader loop when the stream closes (see `reader_loop`
    /// teardown). Read/written **only under the `pending` lock** so a `call`
    /// registering a waiter and the reader draining-on-close are serialized: a
    /// call either inserts before the drain (woken fast by it) or observes
    /// `closed` afterwards and errors immediately — never orphaned until the
    /// deadline. (Prior bug: a call that registered *after* the reader had
    /// already torn down blocked the full RPC timeout.)
    closed: Arc<AtomicBool>,
    _reader: std::thread::JoinHandle<()>,
    subs: Subs,
    next_watch: AtomicU64,
    procs: Procs,
    next_chan: AtomicU64,
    /// The spawned agent process, owned so it's killed when the client drops
    /// (subprocess transports). `None` for a caller-provided stream (tests).
    child: Mutex<Option<Child>>,
}

/// Resolve a bridge RPC deadline from `var` (seconds), falling back to
/// `default_secs`. A missing/blank/unparseable value uses the default; `0` is
/// treated as the default too (a bridge RPC with no deadline could hang forever).
fn env_timeout(var: &str, default_secs: u64) -> Duration {
    let secs = std::env::var(var)
        .ok()
        .and_then(|s| s.trim().parse::<u64>().ok())
        .filter(|&s| s > 0)
        .unwrap_or(default_secs);
    Duration::from_secs(secs)
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
        let closed: Arc<AtomicBool> = Arc::new(AtomicBool::new(false));
        let reader_pending = pending.clone();
        let reader_subs = subs.clone();
        let reader_procs = procs.clone();
        let reader_closed = closed.clone();
        let handle = std::thread::Builder::new()
            .name("bridge-reader".into())
            .spawn(move || {
                reader_loop(
                    reader,
                    reader_pending,
                    reader_subs,
                    reader_procs,
                    reader_closed,
                )
            })
            .expect("spawn bridge reader");
        BridgeClient {
            writer: Arc::new(Mutex::new(Box::new(writer))),
            next_id: AtomicU64::new(1),
            pending,
            timeout: env_timeout("THEGN_BRIDGE_TIMEOUT_SECS", 120),
            read_timeout: env_timeout("THEGN_BRIDGE_READ_TIMEOUT_SECS", 20),
            closed,
            _reader: handle,
            subs,
            next_watch: AtomicU64::new(1),
            procs,
            next_chan: AtomicU64::new(1),
            child: Mutex::new(child),
        }
    }

    fn call(&self, method: &str, params: serde_json::Value) -> Result<serde_json::Value> {
        self.call_within(method, params, self.timeout)
    }

    /// [`call`](Self::call) with an explicit RPC deadline. Interactive read-only
    /// ops (the sidebar glyph fan-out, `git status`/`rev-list` reads) pass the
    /// shorter `read_timeout` so a stalled remote can't freeze a panel poll for
    /// two minutes; network writes (fetch/push/pull via `run_w`) keep the long
    /// default because a large fetch legitimately runs for a while.
    fn call_within(
        &self,
        method: &str,
        params: serde_json::Value,
        timeout: Duration,
    ) -> Result<serde_json::Value> {
        warn_if_on_loop_thread(method);
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        let (tx, rx) = channel();
        {
            // Register the waiter under the same lock the reader's close-teardown
            // holds, and bail if the stream already closed — otherwise a call
            // that lands after teardown would wait out the full RPC deadline for
            // a response that can never come.
            let mut p = self.pending.lock().unwrap();
            if self.closed.load(Ordering::SeqCst) {
                bail!("bridge connection closed");
            }
            p.insert(id, tx);
        }
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
        match rx.recv_timeout(timeout) {
            Ok(Ok(v)) => Ok(v),
            Ok(Err(e)) => Err(anyhow!("bridge: {e}")),
            Err(_) => {
                self.pending.lock().unwrap().remove(&id);
                Err(anyhow!("bridge: timed out waiting for {method}"))
            }
        }
    }

    /// Run a command in the env and return its captured output. Uses the long
    /// default RPC timeout — the write path (`run_w`: fetch/push/pull) rides this,
    /// and those can legitimately run for a while.
    pub fn exec(
        &self,
        argv: &[&str],
        cwd: Option<&str>,
        env: &[(String, String)],
    ) -> Result<ExecResult> {
        self.exec_within(argv, cwd, env, self.timeout)
    }

    /// [`exec`](Self::exec) bounded by the shorter interactive `read_timeout`, for
    /// read-only git ops on the panel-poll path where a stalled remote must not
    /// wedge the UI for the full write deadline.
    pub fn exec_read(
        &self,
        argv: &[&str],
        cwd: Option<&str>,
        env: &[(String, String)],
    ) -> Result<ExecResult> {
        self.exec_within(argv, cwd, env, self.read_timeout)
    }

    fn exec_within(
        &self,
        argv: &[&str],
        cwd: Option<&str>,
        env: &[(String, String)],
        timeout: Duration,
    ) -> Result<ExecResult> {
        let params = serde_json::to_value(ExecParams {
            argv: argv.iter().map(|s| s.to_string()).collect(),
            cwd: cwd.map(str::to_string),
            env: env.to_vec(),
        })?;
        Ok(serde_json::from_value(
            self.call_within("exec", params, timeout)?,
        )?)
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
        // Read-only glyph fan-out — bound by the shorter interactive deadline.
        Ok(serde_json::from_value(self.call_within(
            "exec.batch",
            params,
            self.read_timeout,
        )?)?)
    }

    /// Sum of CPU jiffies per path for processes in the env whose cwd is under it
    /// (feeds the activity FSM with the *env's* processes). Runs on the recurring
    /// hydration/activity poll — a read-only interactive op — so it's bound by the
    /// shorter `read_timeout`: a stalled remote must freeze the sidebar refresh for
    /// the read deadline, not the full 120s write deadline.
    pub fn proc_list(&self, paths: &[String]) -> Result<BTreeMap<String, u64>> {
        let params = serde_json::to_value(ProcParams {
            paths: paths.to_vec(),
        })?;
        let r: ProcResult =
            serde_json::from_value(self.call_within("proc.list", params, self.read_timeout)?)?;
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

/// Remove a worktree's bridge from the process-global registry. This drops **only
/// the registry's** `Arc`; the agent is killed by `BridgeClient::drop` **only if
/// this was the last `Arc`**. Callers that hold a second `Arc` (the host's
/// `BridgeSupervisor` keeps one in its `conns` map) must drop that too — route
/// teardown through the supervisor's disconnect path, not this alone — or the
/// agent keeps running and `is_connected` still reports the stale client alive.
pub fn drop_key(key: &str) {
    registry().lock().unwrap().remove(key);
}

/// The live bridge for a loc, if one is registered. Returns `None` (without
/// locking) for local locs — keeps the common case off the registry mutex.
pub fn for_loc(loc: &GitLoc) -> Option<Arc<BridgeClient>> {
    let key = bridge_key(loc)?;
    registry().lock().unwrap().get(&key).cloned()
}

fn reader_loop(
    mut reader: impl Read,
    pending: Pending,
    subs: Subs,
    procs: Procs,
    closed: Arc<AtomicBool>,
) {
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
                    {
                        // A malformed payload is a protocol violation; drop the
                        // frame (never deliver a silently-empty chunk) but keep
                        // the stream alive.
                        match B64.decode(&note.data) {
                            Ok(data) => {
                                if let Some(tx) = procs.lock().unwrap().get(&note.chan) {
                                    let _ = tx.send(ProcEvent::Out {
                                        stream: note.stream,
                                        data,
                                    });
                                }
                            }
                            Err(e) => tracing::warn!(
                                chan = note.chan,
                                "proc.out: dropping frame with invalid base64: {e}"
                            ),
                        }
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
    // Stream closed — tear down every consumer so none hangs waiting for events
    // that can never arrive (the connection can die without the client dropping).
    // Pending RPC waiters get an error; proc subscribers get a final synthetic
    // Exit; fs.watch subscribers observe the drop (Sender gone → recv errs).
    // Mark `closed` *inside* the same critical section as the drain so a
    // concurrent `call_within` is serialized: it either inserted before us (and
    // is drained here) or observes `closed` after us and errors immediately —
    // no waiter is left to time out.
    {
        let mut p = pending.lock().unwrap();
        closed.store(true, Ordering::SeqCst);
        for (_, tx) in p.drain() {
            let _ = tx.send(Err("bridge connection closed".into()));
        }
    }
    for (_, tx) in procs.lock().unwrap().drain() {
        let _ = tx.send(ProcEvent::Exit { code: -1 });
    }
    // Dropping the Senders disconnects each fs.watch receiver's `recv()`.
    subs.lock().unwrap().clear();
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

/// A live streaming child. Its stdin is owned by a dedicated per-channel writer
/// thread; `proc.stdin` just hands bytes to that thread over a **bounded** channel
/// so the blocking pipe write never runs on the serve read loop (a child that
/// stops draining stdin must not wedge the whole agent — every other request,
/// including the `proc.kill` that would free the pipe, would sit unread otherwise).
/// Dropping the `ProcState` drops the sender → the writer thread's `recv` errs →
/// it drops stdin → the child sees EOF → exits → its waiter thread fires
/// `proc.exit`. So `proc.kill` and connection-close remain just a map removal —
/// no shared `Child` mutex, no libc signal, no deadlock between reader/kill paths.
struct ProcState {
    /// Bounded so a wedged child's backlog can't grow without limit; a full queue
    /// makes `proc.stdin` fail fast rather than block the read loop.
    stdin_tx: std::sync::mpsc::SyncSender<Vec<u8>>,
}
/// Backlog depth for a channel's stdin writer thread before `proc.stdin` errors.
const STDIN_QUEUE_DEPTH: usize = 64;
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
    let stderr = child.stderr.take().context("child stderr")?;
    let stdin = child.stdin.take().context("child stdin")?;
    let chan = p.chan;
    // Stream stdout + stderr as proc.out notifications. A relay thread that
    // fails to start would leave a channel that silently drops output, so reap
    // the child and fail the spawn instead of registering a dead channel.
    let relays = spawn_stream_relay(stdout, chan, "stdout", writer.clone())
        .and_then(|()| spawn_stream_relay(stderr, chan, "stderr", writer.clone()));
    if let Err(e) = relays {
        let _ = child.kill();
        let _ = child.wait();
        return Err(e).context("spawn bridge proc relay thread");
    }
    // A dedicated writer thread owns stdin: proc.stdin bytes arrive over a bounded
    // channel, so the blocking pipe write happens here, never on the serve read
    // loop. The thread ends when the sender drops (ProcState removed by proc.kill /
    // connection-close) or the pipe errors — either way stdin drops → EOF.
    let (stdin_tx, stdin_rx) = std::sync::mpsc::sync_channel::<Vec<u8>>(STDIN_QUEUE_DEPTH);
    let writer_thread = std::thread::Builder::new()
        .name("bridge-proc-stdin".into())
        .spawn(move || {
            let mut stdin = stdin;
            while let Ok(chunk) = stdin_rx.recv() {
                if stdin
                    .write_all(&chunk)
                    .and_then(|()| stdin.flush())
                    .is_err()
                {
                    break;
                }
            }
            // Drop stdin → child sees EOF.
        });
    if let Err(e) = writer_thread {
        let _ = child.kill();
        let _ = child.wait();
        return Err(e).context("spawn bridge-proc-stdin thread");
    }
    procs.lock().unwrap().insert(chan, ProcState { stdin_tx });
    // Waiter: owns the Child, blocks on exit (no lock held), then reports exit and
    // drops the channel. The child exits when it finishes or when proc.kill /
    // connection-close drops its stdin (EOF).
    let procs2 = procs.clone();
    let waiter = std::thread::Builder::new()
        .name("bridge-proc-wait".into())
        .spawn(move || {
            let code = child.wait().ok().and_then(|s| s.code()).unwrap_or(-1);
            procs2.lock().unwrap().remove(&chan);
            let note = serde_json::json!({
                "method": "proc.exit",
                "params": ProcExitNote { chan, code },
            });
            write_frame(&writer, &note);
        });
    if let Err(e) = waiter {
        // Without a waiter the client would never see proc.exit. The failed
        // spawn dropped its closure (and the Child with it, un-reaped);
        // deregistering drops stdin → EOF → the child exits on its own.
        procs.lock().unwrap().remove(&chan);
        return Err(e).context("spawn bridge-proc-wait thread");
    }
    Ok(())
}

/// Relay a child stream to the client as `proc.out` notifications until EOF.
fn spawn_stream_relay(
    mut r: impl Read + Send + 'static,
    chan: u64,
    stream: &'static str,
    writer: SharedWriter,
) -> std::io::Result<()> {
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
        .map(|_| ())
}

fn proc_stdin_response(req: &Request, procs: &ProcRegistry) -> Response {
    let p: ChanData = match serde_json::from_value(req.params.clone()) {
        Ok(p) => p,
        Err(e) => return resp_err(req.id, format!("bad proc.stdin params: {e}")),
    };
    let stdin_tx = procs
        .lock()
        .unwrap()
        .get(&p.chan)
        .map(|s| s.stdin_tx.clone());
    let Some(stdin_tx) = stdin_tx else {
        return resp_err(req.id, format!("no such channel {}", p.chan));
    };
    let data = match B64.decode(&p.data) {
        Ok(d) => d,
        Err(e) => return resp_err(req.id, format!("proc.stdin: invalid base64: {e}")),
    };
    // Hand off to the channel's writer thread without blocking the read loop. A
    // full queue means the child has stalled on reading its stdin — fail fast
    // (the client sees the error and can back off / kill) rather than wedging the
    // agent behind a blocked pipe write.
    match stdin_tx.try_send(data) {
        Ok(()) => resp_ok(req.id, serde_json::json!({})),
        Err(std::sync::mpsc::TrySendError::Full(_)) => {
            resp_err(req.id, "proc.stdin: channel stdin backlog full".into())
        }
        Err(std::sync::mpsc::TrySendError::Disconnected(_)) => {
            resp_err(req.id, format!("proc.stdin: channel {} closed", p.chan))
        }
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

/// Spawn a recursive fs-watcher on `path` (the `notify` crate: inotify on
/// Linux, FSEvents on macOS) that streams `fs.event` notifications
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
    // Bounded like the host's local `output_bounded`: a wedged git in the env (an
    // index.lock held by a crashed process, a hung NFS/SSH mount) must not pin a
    // `bridge-exec` thread + live process forever. The client abandons its RPC on
    // timeout without cancelling us, so cap it here — deadline defaults above the
    // client's read timeout so a legitimately-slow-but-live command still finishes.
    let deadline = env_timeout("THEGN_BRIDGE_EXEC_DEADLINE_SECS", 90);
    output_bounded(c, deadline).with_context(|| format!("exec {}", p.argv.join(" ")))
}

/// Run `c` to completion, draining stdout/stderr on threads (so a full pipe never
/// deadlocks) and killing the child if it outlives `deadline`. Mirrors the host's
/// `git::output_bounded`. A killed child yields `exit = -1`.
fn output_bounded(mut c: Command, deadline: Duration) -> Result<ExecResult> {
    c.stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = c.spawn().context("spawn")?;
    let stdout = child.stdout.take().context("child stdout")?;
    let stderr = child.stderr.take().context("child stderr")?;
    // Drain both pipes concurrently — a child that fills one while we block on the
    // other would otherwise deadlock, and killing on deadline needs the readers to
    // not be holding the process open.
    //
    // Each drain publishes as it reads instead of returning at EOF, because EOF
    // is not ours to wait for: the pipe closes only when the LAST writer lets
    // go, and a grandchild can outlive the process we killed. `sh -c "sleep 30"`
    // is exactly that on Windows — MSYS `sh` forks a `sleep.exe` that inherits
    // stdout, so joining the reader after killing `sh` sat here for the full 30
    // seconds and turned this bounded call into an unbounded one. (Unix never
    // showed it: `sh` execs into a single command rather than forking, so the
    // process we kill IS the one holding the pipe.)
    let out_buf = Arc::new(Mutex::new(Vec::new()));
    let err_buf = Arc::new(Mutex::new(Vec::new()));
    let drains: Vec<Arc<AtomicBool>> = [
        (Box::new(stdout) as Box<dyn Read + Send>, out_buf.clone()),
        (Box::new(stderr) as Box<dyn Read + Send>, err_buf.clone()),
    ]
    .into_iter()
    .map(|(mut pipe, buf)| {
        let done = Arc::new(AtomicBool::new(false));
        let flag = done.clone();
        std::thread::spawn(move || {
            let mut chunk = [0u8; 8 * 1024];
            while let Ok(n) = pipe.read(&mut chunk) {
                if n == 0 {
                    break;
                }
                if let Ok(mut b) = buf.lock() {
                    b.extend_from_slice(&chunk[..n]);
                }
            }
            flag.store(true, Ordering::Relaxed);
        });
        done
    })
    .collect();
    let start = Instant::now();
    let status = loop {
        match child.try_wait().context("wait")? {
            Some(s) => break Some(s),
            None if start.elapsed() >= deadline => {
                let _ = child.kill();
                let _ = child.wait();
                break None;
            }
            None => std::thread::sleep(Duration::from_millis(20)),
        }
    };
    // The child is done; let the drains catch up, but on a leash. Whatever they
    // have collected by then is the result, and a reader still blocked on a
    // pipe an orphan is holding is simply abandoned — the same "hand it off and
    // return at the deadline" call `sandbox::output_with_timeout` makes about
    // reaping a wedged probe.
    const DRAIN_GRACE: Duration = Duration::from_secs(2);
    let flush_by = Instant::now() + DRAIN_GRACE;
    while drains.iter().any(|d| !d.load(Ordering::Relaxed)) && Instant::now() < flush_by {
        std::thread::sleep(Duration::from_millis(5));
    }
    let take = |b: &Arc<Mutex<Vec<u8>>>| {
        b.lock()
            .map(|g| String::from_utf8_lossy(&g).into_owned())
            .unwrap_or_default()
    };
    Ok(ExecResult {
        stdout: take(&out_buf),
        stderr: take(&err_buf),
        exit: status.and_then(|s| s.code()).unwrap_or(-1),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{TcpListener, TcpStream};

    /// Resolve a POSIX utility the fixtures below spawn (`sh`, `cat`, `echo`).
    ///
    /// None of these are Windows executables — `echo` is a cmd builtin, and
    /// there is no system `sh`/`cat` at all — so a bare name fails to spawn
    /// there. `posix_util` finds the MSYS userland Git for Windows ships; on
    /// unix it is a plain PATH lookup.
    fn p(name: &str) -> String {
        thegn_core::util::posix_util(name)
            .unwrap_or_else(|| panic!("POSIX `{name}` not found (Git for Windows ships one)"))
    }

    /// How long to wait for an event the fixture produces immediately.
    ///
    /// Headroom, not the thing under test — but "immediately" is relative: on
    /// Windows each of these spawns an MSYS binary through fork emulation with
    /// a security agent inspecting the process creation, and under a saturated
    /// suite that overran 5s and failed on the fixture rather than on anything
    /// asserted.
    const EV_BUDGET: Duration = if cfg!(windows) {
        Duration::from_secs(45)
    } else {
        Duration::from_secs(5)
    };

    #[test]
    fn env_timeout_uses_default_when_unset_or_invalid() {
        // A name no test sets — exercises the missing/blank fallback.
        assert_eq!(
            env_timeout("THEGN_NOPE_UNSET_TIMEOUT", 42),
            Duration::from_secs(42)
        );
    }

    #[test]
    fn read_timeout_is_shorter_than_the_write_default() {
        // The interactive read deadline must not exceed the write/default one, or
        // a stalled read would still block for the full write timeout.
        let c = connect();
        assert!(
            c.read_timeout <= c.timeout,
            "read_timeout {:?} should be <= write timeout {:?}",
            c.read_timeout,
            c.timeout
        );
    }

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
        let r = c
            .exec(&[p("echo").as_str(), "hello-bridge"], None, &[])
            .unwrap();
        assert_eq!(r.exit, 0);
        assert_eq!(r.stdout.trim(), "hello-bridge");
        // Non-zero exit is reported (not an RPC error).
        let r2 = c
            .exec(&[p("sh").as_str(), "-c", "exit 3"], None, &[])
            .unwrap();
        assert_eq!(r2.exit, 3);
        // Many sequential calls reuse the one connection.
        for i in 0..5 {
            let r = c
                .exec(&[p("sh").as_str(), "-c", &format!("echo {i}")], None, &[])
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
                    vec![p("echo"), "first".into()],
                    vec![p("sh"), "-c".into(), "exit 7".into()],
                    vec![p("echo"), "third".into()],
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
        // Let the fs-watch initialize before mutating.
        std::thread::sleep(Duration::from_millis(200));

        std::fs::write(dir.join("hello.rs"), b"fn main(){}").unwrap();
        let ev = rx
            .recv_timeout(EV_BUDGET)
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
        let (chan, rx) = c.spawn_proc(&[p("cat").as_str()], None, &[]).unwrap();
        c.proc_stdin(chan, b"ping\n").unwrap();
        // The echoed bytes come back as a proc.out(stdout) event — the first
        // event is either that (the happy path) or an early Exit (a failure),
        // so a single recv suffices.
        let got = match rx.recv_timeout(EV_BUDGET).expect("a proc event") {
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
        while let Ok(ev) = rx.recv_timeout(EV_BUDGET) {
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
        let (_chan, rx) = c
            .spawn_proc(&[p("sh").as_str(), "-c", "exit 0"], None, &[])
            .unwrap();
        let mut code = None;
        while let Ok(ev) = rx.recv_timeout(EV_BUDGET) {
            if let ProcEvent::Exit { code: c } = ev {
                code = Some(c);
                break;
            }
        }
        assert_eq!(code, Some(0));
    }

    #[test]
    fn proc_stdin_rejects_invalid_base64() {
        let c = connect();
        let (chan, rx) = c.spawn_proc(&[p("cat").as_str()], None, &[]).unwrap();
        // A corrupt payload must be rejected, not silently written as empty.
        let e = c
            .call(
                "proc.stdin",
                serde_json::json!({ "chan": chan, "data": "%%%" }),
            )
            .unwrap_err();
        assert!(e.to_string().contains("invalid base64"), "err: {e}");
        // The channel survives the rejected write: valid stdin still round-trips.
        c.proc_stdin(chan, b"still-alive\n").unwrap();
        match rx.recv_timeout(EV_BUDGET).expect("a proc event") {
            ProcEvent::Out { data, .. } => assert_eq!(&data, b"still-alive\n"),
            ProcEvent::Exit { .. } => panic!("exited before echo"),
        }
        c.proc_kill(chan).unwrap();
    }

    #[test]
    fn proc_out_with_invalid_base64_is_dropped_not_emptied() {
        // Hand-roll the server side so a corrupt proc.out frame can be injected:
        // the client must drop it (never deliver a silently-empty chunk) and keep
        // the stream alive for the next valid frame.
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let (go_tx, go_rx) = channel::<()>();
        let server = std::thread::spawn(move || {
            let (mut sock, _) = listener.accept().unwrap();
            // Wait until the client has registered chan 1, so neither frame
            // races the subscription.
            go_rx.recv().unwrap();
            for data in ["%%%not-base64%%%".to_string(), B64.encode(b"ok")] {
                let note = serde_json::json!({
                    "method": "proc.out",
                    "params": { "chan": 1, "stream": "stdout", "data": data }
                });
                sock.write_all(&framing::encode(&note.to_string())).unwrap();
            }
            sock.flush().unwrap();
            sock // keep the connection open until the client has read both frames
        });
        let sock = TcpStream::connect(addr).unwrap();
        let c = BridgeClient::new(sock.try_clone().unwrap(), sock);
        let (tx, rx) = channel();
        c.procs.lock().unwrap().insert(1, tx);
        go_tx.send(()).unwrap();
        // Only the valid frame arrives; the corrupt one was dropped.
        match rx.recv_timeout(EV_BUDGET).expect("a proc event") {
            ProcEvent::Out { data, .. } => assert_eq!(&data, b"ok"),
            other => panic!("unexpected event: {other:?}"),
        }
        drop(server.join());
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

    /// Drive the client's reader with a hand-rolled server: run `body(sock)` to
    /// push frames once the client has registered chan 1, and return a
    /// `ProcEvent` receiver for that channel.
    fn scripted_server(
        body: impl FnOnce(&mut TcpStream) + Send + 'static,
    ) -> (BridgeClient, Receiver<ProcEvent>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let (go_tx, go_rx) = channel::<()>();
        std::thread::spawn(move || {
            let (mut sock, _) = listener.accept().unwrap();
            go_rx.recv().unwrap();
            body(&mut sock);
            sock.flush().unwrap();
            // Hold the connection open until the client has consumed the frames.
            std::thread::sleep(Duration::from_millis(500));
        });
        let sock = TcpStream::connect(addr).unwrap();
        let c = BridgeClient::new(sock.try_clone().unwrap(), sock);
        let (tx, rx) = channel();
        c.procs.lock().unwrap().insert(1, tx);
        go_tx.send(()).unwrap();
        (c, rx)
    }

    fn proc_out_frame(chan: u64, data: &[u8]) -> Vec<u8> {
        let note = serde_json::json!({
            "method": "proc.out",
            "params": { "chan": chan, "stream": "stdout", "data": B64.encode(data) }
        });
        framing::encode(&note.to_string())
    }

    #[test]
    fn malformed_frame_is_skipped_and_loop_survives() {
        // A non-JSON frame must not kill the reader; a following valid frame is
        // still delivered.
        let (_c, rx) = scripted_server(|sock| {
            sock.write_all(&framing::encode("this is not json {"))
                .unwrap();
            sock.write_all(&proc_out_frame(1, b"after-garbage"))
                .unwrap();
        });
        match rx.recv_timeout(EV_BUDGET).expect("a proc event") {
            ProcEvent::Out { data, .. } => assert_eq!(&data, b"after-garbage"),
            other => panic!("unexpected event: {other:?}"),
        }
    }

    #[test]
    fn unknown_response_id_is_ignored() {
        // A response correlating to no pending request is dropped silently; the
        // reader keeps running and delivers the next notification.
        let (_c, rx) = scripted_server(|sock| {
            let resp = serde_json::json!({ "id": 99999, "ok": {} });
            sock.write_all(&framing::encode(&resp.to_string())).unwrap();
            sock.write_all(&proc_out_frame(1, b"still-here")).unwrap();
        });
        match rx.recv_timeout(EV_BUDGET).expect("a proc event") {
            ProcEvent::Out { data, .. } => assert_eq!(&data, b"still-here"),
            other => panic!("unexpected event: {other:?}"),
        }
    }

    #[test]
    fn connection_close_delivers_exit_to_proc_subscribers() {
        // A live proc.spawn subscriber must observe a terminal Exit when the
        // connection dies (reader_loop close), not hang on recv forever — the
        // production `bridge-fswatch`/proc forwarder threads rely on this.
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        std::thread::spawn(move || {
            let (sock, _) = listener.accept().unwrap();
            // Hold briefly so the client registers its sub, then hang up.
            std::thread::sleep(Duration::from_millis(100));
            drop(sock);
        });
        let sock = TcpStream::connect(addr).unwrap();
        let c = BridgeClient::new(sock.try_clone().unwrap(), sock);
        // Register a proc subscriber directly (no real proc.spawn round-trip; the
        // server here never answers).
        let (tx, rx) = channel();
        c.procs.lock().unwrap().insert(7, tx);
        let start = Instant::now();
        match rx.recv_timeout(Duration::from_secs(10)) {
            Ok(ProcEvent::Exit { code }) => assert_eq!(code, -1),
            other => panic!("expected a synthetic Exit on close, got {other:?}"),
        }
        assert!(start.elapsed() < Duration::from_secs(5));
    }

    #[test]
    fn connection_close_disconnects_fs_watch_subscribers() {
        // An fs.watch receiver must disconnect (recv errs) when the connection
        // dies, rather than blocking the forwarder thread for the process lifetime.
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        std::thread::spawn(move || {
            let (sock, _) = listener.accept().unwrap();
            std::thread::sleep(Duration::from_millis(100));
            drop(sock);
        });
        let sock = TcpStream::connect(addr).unwrap();
        let c = BridgeClient::new(sock.try_clone().unwrap(), sock);
        let (tx, rx) = channel::<FsEvent>();
        c.subs.lock().unwrap().insert(3, tx);
        // recv must err (sender dropped by reader_loop close), not time out.
        match rx.recv_timeout(Duration::from_secs(10)) {
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {}
            other => panic!("expected the fs.watch receiver to disconnect, got {other:?}"),
        }
    }

    #[test]
    fn proc_stdin_does_not_block_read_loop_when_child_ignores_stdin() {
        // A child that never reads its stdin must not wedge the serve loop: once
        // the OS pipe buffer fills, further proc.stdin either queue-fails fast or
        // succeed, and crucially a subsequent proc.kill on the SAME connection is
        // still processed (the read loop was never blocked in write_all).
        let c = connect();
        // `sleep` never reads stdin; feed it far more than a pipe buffer (~64KB).
        let (chan, _rx) = c
            .spawn_proc(&[p("sh").as_str(), "-c", "sleep 30"], None, &[])
            .unwrap();
        let chunk = vec![b'x'; 16 * 1024];
        // Push enough to overflow both the pipe and the bounded queue; some sends
        // may error (backlog full) — that's the fast-fail, not a hang.
        for _ in 0..256 {
            let _ = c.proc_stdin(chan, &chunk);
        }
        // The read loop is still alive: proc.kill returns promptly instead of
        // sitting behind a blocked pipe write.
        let start = Instant::now();
        c.proc_kill(chan).unwrap();
        assert!(
            start.elapsed() < Duration::from_secs(5),
            "proc.kill was serviced (read loop not wedged): {:?}",
            start.elapsed()
        );
    }

    #[test]
    fn output_bounded_kills_a_wedged_command_at_the_deadline() {
        // A command that outlives the deadline is killed and reported as exit -1,
        // rather than pinning the exec thread + process forever (a wedged git in
        // the env). Uses a short local deadline — no process-global env needed.
        let mut c = Command::new(p("sh"));
        c.args(["-c", "sleep 30"]);
        let start = Instant::now();
        let r = output_bounded(c, Duration::from_millis(300)).unwrap();
        assert_eq!(r.exit, -1, "killed child reports -1");
        // The claim is "killed at the deadline, not after the full 30s sleep",
        // so the bound only has to sit well below 30s. On Windows a bare
        // CreateProcess is ~40ms (vs ~1-3ms for fork+exec), MSYS `sh` adds its
        // fork emulation on top, and a saturated suite stretches both — 5s is
        // not enough headroom there and the test failed every retry, which is a
        // slow machine, not a broken watchdog.
        let bound = if cfg!(windows) {
            Duration::from_secs(20)
        } else {
            Duration::from_secs(5)
        };
        assert!(
            start.elapsed() < bound,
            "killed at the deadline, not after the full sleep: {:?}",
            start.elapsed()
        );
    }

    #[test]
    fn output_bounded_returns_output_for_a_fast_command() {
        // The happy path still captures stdout/exit for a command that finishes
        // well within the deadline.
        let mut c = Command::new(p("sh"));
        c.args(["-c", "printf hi; exit 4"]);
        let r = output_bounded(c, Duration::from_secs(10)).unwrap();
        assert_eq!(r.stdout, "hi");
        assert_eq!(r.exit, 4);
    }

    #[test]
    fn connection_close_fails_pending_calls_fast() {
        // A server that accepts then hangs up must wake any in-flight call with an
        // error immediately, not leave it blocked until the 120s RPC deadline.
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        std::thread::spawn(move || {
            let (sock, _) = listener.accept().unwrap();
            // Drop the connection without answering.
            drop(sock);
        });
        let sock = TcpStream::connect(addr).unwrap();
        let c = BridgeClient::new(sock.try_clone().unwrap(), sock);
        let start = Instant::now();
        let r = c.exec(&["echo", "hi"], None, &[]);
        assert!(r.is_err(), "closed transport ⇒ the call errors");
        assert!(
            start.elapsed() < Duration::from_secs(10),
            "call woke fast, not at the 120s deadline: {:?}",
            start.elapsed()
        );
    }
}
