//! Reverse host→sandbox tunnel: the async transport that pumps the pure mux
//! protocol ([`thegn_core::revtunnel`]) over a single bidirectional byte
//! stream (in production: the provider WSS exec / resident-bridge channel).
//!
//! Two symmetric endpoints share the stream:
//! - [`run_sandbox`] runs INSIDE the sandbox: it `accept`s loopback connections
//!   (e.g. an agent dialing `127.0.0.1:8383` for `tgproxy`), opens a muxed
//!   logical connection per accept, and pumps bytes both ways.
//! - [`run_host`] runs on the HOST: for each `Open` it [`Dialer`]s the real host
//!   target (the local `tgproxy`, a host `localhost` DB/API, a host-bound MCP
//!   server) and pumps bytes back.
//!
//! Both are generic over the stream, and the host side over the [`Dialer`], so the
//! whole thing is exercised end-to-end **with in-memory mocks** (a
//! `tokio::io::duplex` pair as the channel + an echo `Dialer`) — no real network,
//! no sprite. Mirrors the `[forward]` proxy pattern, but reversed.

use std::collections::HashMap;
use std::io;
use std::sync::Arc;
use std::sync::LazyLock;
use std::sync::Mutex as StdMutex;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use thegn_core::revtunnel::{
    Frame, FrameDecoder, MAX_RESYNC_SKIP, SYNC_MAGIC, SyncOutcome, encode, encode_data_chunked,
};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::sync::mpsc::{Sender, channel};
use tokio::sync::{Mutex, Notify, Semaphore};
use tokio::task::{JoinHandle, JoinSet};

/// Bounded depth for the shared outbound frame sink and each per-connection
/// inbound channel. This is the flow-control knob: with unbounded channels a
/// fast local service (e.g. the host nix-cache serving a multi-GB NAR at
/// loopback speed) outruns the WSS exec-stream drain and the difference piles
/// up as queued frames, ballooning compositor RSS. A bounded channel makes the
/// producing read `send(...).await` block once the queue is full, so
/// backpressure propagates back to the local TCP read. Frames are at most
/// `MAX_FRAME_PAYLOAD` (1 MiB) each, so peak in-flight is bounded by
/// `CHANNEL_DEPTH * 1 MiB` per connection direction rather than the whole
/// transfer size.
const CHANNEL_DEPTH: usize = 32;

/// Safe per-tunnel ceiling for concurrent logical connections.
const MAX_TUNNEL_CONNS: usize = 256;

/// Process-wide host ceiling, shared by every reverse-tunnel run in this process.
const MAX_GLOBAL_TUNNEL_CONNS: usize = 1024;

/// Bound host endpoint connection establishment so an unresponsive dial cannot
/// pin a route, a connection permit, or the shared tunnel read loop indefinitely.
const DIAL_TIMEOUT: Duration = Duration::from_secs(10);
const WRITE_STALL_TIMEOUT: Duration = Duration::from_secs(60);
const IDLE_TIMEOUT: Duration = Duration::from_secs(2 * 60 * 60);
const LIFETIME_TIMEOUT: Duration = Duration::from_secs(24 * 60 * 60);
const ACCEPT_BACKOFF_BASE: Duration = Duration::from_millis(10);
const ACCEPT_BACKOFF_MAX: Duration = Duration::from_secs(1);
const REJECTION_LOG_INTERVAL: Duration = Duration::from_secs(60);

static GLOBAL_HOST_PERMITS: LazyLock<Arc<Semaphore>> =
    LazyLock::new(|| Arc::new(Semaphore::new(MAX_GLOBAL_TUNNEL_CONNS)));
static REJECTIONS_TOTAL: AtomicU64 = AtomicU64::new(0);
static REJECTION_COUNTS: RejectionCounters = RejectionCounters::new();
static LAST_REJECTION_WARNING: LazyLock<StdMutex<HashMap<&'static str, Instant>>> =
    LazyLock::new(|| StdMutex::new(HashMap::new()));

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct RejectionCounts {
    pub connection_limit: u64,
    pub global_connection_limit: u64,
    pub duplicate_id: u64,
    pub dial_timeout: u64,
    pub accept_transient: u64,
    pub write_stall: u64,
    pub idle: u64,
    pub lifetime: u64,
}

struct RejectionCounters {
    connection_limit: AtomicU64,
    global_connection_limit: AtomicU64,
    duplicate_id: AtomicU64,
    dial_timeout: AtomicU64,
    accept_transient: AtomicU64,
    write_stall: AtomicU64,
    idle: AtomicU64,
    lifetime: AtomicU64,
}

impl RejectionCounters {
    const fn new() -> Self {
        Self {
            connection_limit: AtomicU64::new(0),
            global_connection_limit: AtomicU64::new(0),
            duplicate_id: AtomicU64::new(0),
            dial_timeout: AtomicU64::new(0),
            accept_transient: AtomicU64::new(0),
            write_stall: AtomicU64::new(0),
            idle: AtomicU64::new(0),
            lifetime: AtomicU64::new(0),
        }
    }

    fn increment(&self, reason: RejectionReason) -> u64 {
        let counter = match reason {
            RejectionReason::ConnectionLimit => &self.connection_limit,
            RejectionReason::GlobalConnectionLimit => &self.global_connection_limit,
            RejectionReason::DuplicateId => &self.duplicate_id,
            RejectionReason::DialTimeout => &self.dial_timeout,
            RejectionReason::AcceptTransient => &self.accept_transient,
            RejectionReason::WriteStall => &self.write_stall,
            RejectionReason::Idle => &self.idle,
            RejectionReason::Lifetime => &self.lifetime,
        };
        counter.fetch_add(1, Ordering::Relaxed) + 1
    }

    fn snapshot(&self) -> RejectionCounts {
        RejectionCounts {
            connection_limit: self.connection_limit.load(Ordering::Relaxed),
            global_connection_limit: self.global_connection_limit.load(Ordering::Relaxed),
            duplicate_id: self.duplicate_id.load(Ordering::Relaxed),
            dial_timeout: self.dial_timeout.load(Ordering::Relaxed),
            accept_transient: self.accept_transient.load(Ordering::Relaxed),
            write_stall: self.write_stall.load(Ordering::Relaxed),
            idle: self.idle.load(Ordering::Relaxed),
            lifetime: self.lifetime.load(Ordering::Relaxed),
        }
    }
}

#[derive(Clone, Copy, Debug)]
enum RejectionReason {
    ConnectionLimit,
    GlobalConnectionLimit,
    DuplicateId,
    DialTimeout,
    AcceptTransient,
    WriteStall,
    Idle,
    Lifetime,
}

impl RejectionReason {
    const fn as_str(self) -> &'static str {
        match self {
            Self::ConnectionLimit => "connection_limit",
            Self::GlobalConnectionLimit => "global_connection_limit",
            Self::DuplicateId => "duplicate_id",
            Self::DialTimeout => "dial_timeout",
            Self::AcceptTransient => "accept_transient",
            Self::WriteStall => "write_stall",
            Self::Idle => "idle",
            Self::Lifetime => "lifetime",
        }
    }
}

type ConnectionTasks = Arc<Mutex<JoinSet<()>>>;

fn report_rejection(rejections: &AtomicUsize, endpoint: &'static str, reason: RejectionReason) {
    REJECTIONS_TOTAL.fetch_add(1, Ordering::Relaxed);
    let count = rejections.fetch_add(1, Ordering::Relaxed) + 1;
    let total = REJECTION_COUNTS.increment(reason);
    let now = Instant::now();
    let should_warn = LAST_REJECTION_WARNING
        .lock()
        .map(|mut last| rejection_warning_due(&mut last, reason, now))
        .unwrap_or(false);
    if should_warn {
        tracing::warn!(
            target: "thegn::revtunnel",
            endpoint,
            reason = reason.as_str(),
            tunnel_rejections = count,
            process_rejections = total,
            "reverse-tunnel connection rejected"
        );
    }
}

fn rejection_warning_due(
    last: &mut HashMap<&'static str, Instant>,
    reason: RejectionReason,
    now: Instant,
) -> bool {
    if last
        .get(reason.as_str())
        .is_some_and(|at| now.duration_since(*at) < REJECTION_LOG_INTERVAL)
    {
        false
    } else {
        last.insert(reason.as_str(), now);
        true
    }
}

fn report_global_rejection(endpoint: &'static str, reason: RejectionReason) {
    static EMPTY: AtomicUsize = AtomicUsize::new(0);
    report_rejection(&EMPTY, endpoint, reason);
}

/// Number of categorized reverse-tunnel rejections since process start.
pub fn rejection_total() -> u64 {
    REJECTIONS_TOTAL.load(Ordering::Relaxed)
}

pub fn rejection_counts() -> RejectionCounts {
    REJECTION_COUNTS.snapshot()
}

/// Clamp a configured connection count to Tokio's maximum semaphore capacity.
pub fn clamp_permits(n: usize) -> usize {
    n.min(Semaphore::MAX_PERMITS)
}

async fn reap_finished(tasks: &ConnectionTasks) {
    let mut tasks = tasks.lock().await;
    while tasks.try_join_next().is_some() {}
}

async fn abort_tasks(tasks: &ConnectionTasks) {
    let mut tasks = tasks.lock().await;
    tasks.abort_all();
    while tasks.join_next().await.is_some() {}
}

async fn stop_sink(task: JoinHandle<io::Result<()>>) -> io::Result<()> {
    // Once the read side has hit EOF, queued frames (including final Close
    // frames) are intentionally discarded. Waiting for the sink to drain here
    // could hang forever if the peer stopped reading at tunnel end.
    task.abort();
    match task.await {
        Ok(Err(error)) if peer_disconnect_error(&error) => Ok(()),
        Ok(result) => result,
        Err(error) if error.is_cancelled() => Ok(()),
        Err(error) => Err(io::Error::other(format!(
            "reverse-tunnel sink task: {error}"
        ))),
    }
}

async fn supervise_sink<F>(
    read_loop: F,
    sink_task: &mut JoinHandle<io::Result<()>>,
) -> (io::Result<()>, bool)
where
    F: std::future::Future<Output = io::Result<()>>,
{
    tokio::pin!(read_loop);
    tokio::select! {
        result = &mut read_loop => (result, false),
        sink_result = sink_task => {
            // The peer may stop reading before the read half has drained
            // buffered frames and observed EOF. On transport-close errors,
            // let that read finish so a peer disconnect remains a clean end.
            if matches!(sink_result, Ok(Err(ref error)) if peer_disconnect_error(error)) {
                (read_loop.await, true)
            } else {
                (
                    Err(io::Error::new(io::ErrorKind::BrokenPipe, "reverse-tunnel shared sink stopped")),
                    true,
                )
            }
        }
    }
}

fn peer_disconnect_error(error: &io::Error) -> bool {
    matches!(
        error.kind(),
        io::ErrorKind::BrokenPipe
            | io::ErrorKind::ConnectionAborted
            | io::ErrorKind::ConnectionReset
            | io::ErrorKind::UnexpectedEof
    )
}

/// Any bidirectional async stream usable as a tunneled connection (a real
/// `TcpStream`, or an in-memory `DuplexStream` in tests).
pub trait IoStream: AsyncRead + AsyncWrite + Unpin + Send {}
impl<T: AsyncRead + AsyncWrite + Unpin + Send> IoStream for T {}

trait TunnelListener: Send + 'static {
    fn accept(&mut self)
    -> impl std::future::Future<Output = io::Result<Box<dyn IoStream>>> + Send;
}

impl TunnelListener for TcpListener {
    async fn accept(&mut self) -> io::Result<Box<dyn IoStream>> {
        let (stream, _) = TcpListener::accept(self).await?;
        Ok(Box::new(stream))
    }
}

fn transient_accept_error(error: &io::Error) -> bool {
    matches!(
        error.kind(),
        io::ErrorKind::Interrupted
            | io::ErrorKind::WouldBlock
            | io::ErrorKind::ConnectionAborted
            | io::ErrorKind::OutOfMemory
    ) || crate::plugin::platform::is_transient_accept_resource_error(error.raw_os_error())
}

fn next_accept_backoff(current: Duration) -> Duration {
    current.saturating_mul(2).min(ACCEPT_BACKOFF_MAX)
}

/// Adapt a provider [`ExecSession`](crate::provider::ExecSession) (a non-tty exec
/// over the WSS API — stdout frames in, stdin control out) into a standard
/// bidirectional `DuplexStream` so `run_host` can pump the tunnel over it.
/// The returned stream's writes become `Stdin` to the sandbox process; the
/// sandbox's stdout becomes reads. Two forwarding tasks own the session.
pub fn exec_stream(session: crate::provider::ExecSession) -> tokio::io::DuplexStream {
    use crate::provider::{ExecControl, ExecFrame};
    let crate::provider::ExecSession {
        mut frames,
        control,
        ..
    } = session;
    let (near, far) = tokio::io::duplex(64 * 1024);
    let (mut far_rd, mut far_wr) = tokio::io::split(far);

    // caller writes → sandbox stdin
    tokio::spawn(async move {
        let mut buf = vec![0u8; 16 * 1024];
        loop {
            match far_rd.read(&mut buf).await {
                Ok(0) | Err(_) => {
                    let _ = control.send(ExecControl::Close).await; // best-effort: control end may already be gone
                    break;
                }
                Ok(n) => {
                    if control
                        .send(ExecControl::Stdin(buf[..n].to_vec()))
                        .await
                        .is_err()
                    {
                        break;
                    }
                }
            }
        }
    });
    // sandbox stdout → caller reads
    tokio::spawn(async move {
        while let Some(frame) = frames.recv().await {
            match frame {
                ExecFrame::Stdout(d) => {
                    if far_wr.write_all(&d).await.is_err() {
                        break;
                    }
                }
                ExecFrame::Exit(_) => break,
            }
        }
        let _ = far_wr.shutdown().await; // best-effort: peer may already be gone
    });
    near
}

/// Opens the real host-side target for each tunneled connection. Injectable so
/// tests substitute an in-memory mock for a TCP dial.
pub trait Dialer: Clone + Send + Sync + 'static {
    fn dial(&self) -> impl std::future::Future<Output = io::Result<Box<dyn IoStream>>> + Send;
}

/// Dials a fixed host `addr` (the production host endpoint, e.g. `127.0.0.1:8383`
/// for `tgproxy`).
#[derive(Clone)]
pub struct TcpDialer {
    pub addr: String,
}

impl Dialer for TcpDialer {
    async fn dial(&self) -> io::Result<Box<dyn IoStream>> {
        let s = tokio::net::TcpStream::connect(&self.addr).await?;
        Ok(Box::new(s))
    }
}

fn decode_err(e: thegn_core::revtunnel::DecodeError) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, format!("tunnel decode: {e:?}"))
}

/// Spawn the writer task: serialize all outbound frames (from any connection
/// task) onto the shared stream write half. Returns the frame sink. The sink is
/// **bounded** ([`CHANNEL_DEPTH`]) so a producer outracing the stream write
/// blocks on `send().await` instead of queueing unboundedly (see the constant).
fn spawn_sink<W: AsyncWrite + Unpin + Send + 'static>(
    wr: W,
) -> (Sender<Vec<u8>>, JoinHandle<io::Result<()>>) {
    spawn_sink_with_timeout(wr, WRITE_STALL_TIMEOUT)
}

fn spawn_sink_with_timeout<W: AsyncWrite + Unpin + Send + 'static>(
    mut wr: W,
    write_timeout: Duration,
) -> (Sender<Vec<u8>>, JoinHandle<io::Result<()>>) {
    let (tx, mut rx) = channel::<Vec<u8>>(CHANNEL_DEPTH);
    let task = tokio::spawn(async move {
        while let Some(bytes) = rx.recv().await {
            match tokio::time::timeout(write_timeout, async {
                wr.write_all(&bytes).await?;
                wr.flush().await
            })
            .await
            {
                Ok(Ok(())) => {}
                Ok(Err(error)) => {
                    tracing::warn!(target: "thegn::revtunnel", %error, "reverse-tunnel shared sink failed");
                    return Err(error);
                }
                Err(_) => {
                    report_global_rejection("tunnel", RejectionReason::WriteStall);
                    return Err(io::Error::new(
                        io::ErrorKind::TimedOut,
                        "reverse-tunnel shared sink write stalled",
                    ));
                }
            }
        }
        Ok(())
    });
    (tx, task)
}

/// Pump one logical connection's local stream ⇄ the mux:
/// - local reads → `Data(id)` frames on the sink, then `Close(id)` at EOF;
/// - `inbound` payloads (peer `Data(id)`) → local writes.
///
/// Deadline closes send `Close(id)`, a half-close; the peer's other direction
/// can remain until its own idle or lifetime deadline because this protocol has
/// no full-close frame.
async fn pump_conn(
    id: u32,
    local: Box<dyn IoStream>,
    inbound: tokio::sync::mpsc::Receiver<Vec<u8>>,
    sink: Sender<Vec<u8>>,
) {
    pump_conn_with_deadlines(
        id,
        local,
        inbound,
        sink,
        ConnectionDeadlines {
            write_stall: WRITE_STALL_TIMEOUT,
            idle: IDLE_TIMEOUT,
            lifetime: LIFETIME_TIMEOUT,
        },
    )
    .await;
}

#[derive(Clone, Copy)]
struct ConnectionDeadlines {
    write_stall: Duration,
    idle: Duration,
    lifetime: Duration,
}

async fn pump_conn_with_deadlines(
    id: u32,
    local: Box<dyn IoStream>,
    mut inbound: tokio::sync::mpsc::Receiver<Vec<u8>>,
    sink: Sender<Vec<u8>>,
    deadlines: ConnectionDeadlines,
) {
    let (mut rd, mut wr) = tokio::io::split(local);
    let started = tokio::time::Instant::now();
    let lifetime_deadline = started + deadlines.lifetime;
    let activity_ms = Arc::new(AtomicU64::new(0));
    let activity_notify = Arc::new(Notify::new());
    let stall_notify = Arc::new(Notify::new());
    let close_sent = Arc::new(AtomicBool::new(false));

    let local_activity = activity_ms.clone();
    let local_activity_notify = activity_notify.clone();
    let local_stall_notify = stall_notify.clone();
    let local_close_sent = close_sent.clone();
    let local_sink = sink.clone();
    let local_to_peer = async move {
        let mut buf = vec![0u8; 16 * 1024];
        loop {
            let (frame, is_close) = match rd.read(&mut buf).await {
                Ok(0) | Err(_) => (encode(&Frame::Close(id)), true),
                Ok(n) => (encode_data_chunked(id, &buf[..n]), false),
            };
            match tokio::time::timeout(deadlines.write_stall, local_sink.send(frame)).await {
                Err(_) => {
                    local_stall_notify.notify_one();
                    return;
                }
                Ok(Err(_)) => return,
                Ok(Ok(())) if is_close => {
                    local_close_sent.store(true, Ordering::Release);
                    return;
                }
                Ok(Ok(())) => {
                    local_activity.fetch_max(
                        started.elapsed().as_millis().min(u64::MAX as u128) as u64,
                        Ordering::Release,
                    );
                    local_activity_notify.notify_one();
                    tokio::task::yield_now().await;
                }
            }
        }
    };

    let inbound_activity = activity_ms.clone();
    let inbound_activity_notify = activity_notify.clone();
    let inbound_stall_notify = stall_notify.clone();
    let inbound_to_local = async {
        while let Some(data) = inbound.recv().await {
            match tokio::time::timeout(deadlines.write_stall, wr.write_all(&data)).await {
                Err(_) => {
                    inbound_stall_notify.notify_one();
                    return;
                }
                Ok(Err(_)) => return,
                Ok(Ok(())) => {
                    inbound_activity.fetch_max(
                        started.elapsed().as_millis().min(u64::MAX as u128) as u64,
                        Ordering::Release,
                    );
                    inbound_activity_notify.notify_one();
                    tokio::task::yield_now().await;
                }
            }
        }
        if let Err(error) = wr.shutdown().await {
            tracing::debug!(target: "thegn::revtunnel", %error, "local tunnel half-close failed");
        }
    };

    // These independent futures preserve full-duplex draining while either
    // bounded sink send or local write applies backpressure.
    let reason = {
        let directions = async { tokio::join!(local_to_peer, inbound_to_local) };
        tokio::pin!(directions);
        loop {
            let last_activity = Duration::from_millis(activity_ms.load(Ordering::Acquire));
            let idle_deadline = started + last_activity + deadlines.idle;
            tokio::select! {
                _ = &mut directions => break None,
                _ = tokio::time::sleep_until(lifetime_deadline) => break Some(RejectionReason::Lifetime),
                _ = tokio::time::sleep_until(idle_deadline) => break Some(RejectionReason::Idle),
                _ = activity_notify.notified() => continue,
                _ = stall_notify.notified() => break Some(RejectionReason::WriteStall),
            }
        }
    };
    if let Some(reason) = reason {
        report_global_rejection("connection", reason);
    }
    if !close_sent.load(Ordering::Acquire) {
        match tokio::time::timeout(deadlines.write_stall, sink.send(encode(&Frame::Close(id))))
            .await
        {
            Err(_) => report_global_rejection("connection", RejectionReason::WriteStall),
            Ok(Err(error)) => {
                tracing::debug!(target: "thegn::revtunnel", %error, "connection close frame dropped")
            }
            Ok(Ok(())) => {}
        }
    }
    if let Err(error) = wr.shutdown().await {
        tracing::debug!(target: "thegn::revtunnel", %error, "local tunnel stream shutdown failed");
    }
}

/// Shared per-connection routing table: id → inbound payload sender.
struct Route {
    inbound: Sender<Vec<u8>>,
    // Keep cleanup identity separate: a sender clone here would keep the
    // inbound receiver alive after `Close` removes the route.
    identity: Arc<()>,
}

type Conns = Arc<Mutex<HashMap<u32, Route>>>;

/// Render a bounded byte preview for logs: printable ASCII verbatim, everything
/// else as `.`. So a skipped preamble/garbage run shows up readably.
fn preview_str(bytes: &[u8]) -> String {
    bytes
        .iter()
        .map(|&b| {
            if b.is_ascii_graphic() || b == b' ' {
                b as char
            } else {
                '.'
            }
        })
        .collect()
}

/// Drive the shared read loop: decode frames off `rd` and route them. `on_open`
/// is called for each `Open(id)` (the host dials; the sandbox never receives
/// `Open`). `Data`/`Close` are routed to the registered connection.
///
/// `sync` (when `Some`) is a startup marker the loop skips to before decoding —
/// the host passes [`SYNC_MAGIC`] so a one-time preamble on the exec's stdout
/// (banner/MOTD/shell echo) can't desync the framing. Beyond that, a mid-stream
/// [`DecodeError`](thegn_core::revtunnel::DecodeError) triggers a bounded resync (drop garbage until the framing
/// re-aligns) instead of tearing the tunnel down.
async fn read_loop<R, F, Fut>(
    mut rd: R,
    conns: Conns,
    mut on_open: F,
    sync: Option<&'static [u8]>,
) -> io::Result<()>
where
    R: AsyncRead + Unpin,
    F: FnMut(u32) -> Fut,
    Fut: std::future::Future<Output = ()>,
{
    let mut dec = FrameDecoder::new();
    let mut buf = vec![0u8; 16 * 1024];
    let mut synced = sync.is_none();
    loop {
        let n = rd.read(&mut buf).await?;
        if n == 0 {
            return Ok(());
        }
        dec.push(&buf[..n]);
        // Skip a one-time startup preamble before the first frame.
        if !synced {
            match dec.sync_to(sync.unwrap(), MAX_RESYNC_SKIP) {
                SyncOutcome::Synced { skipped, preview } => {
                    if skipped > 0 {
                        tracing::warn!(
                            target: "thegn::revtunnel",
                            skipped,
                            preview = %preview_str(&preview),
                            "skipped reverse-tunnel startup preamble before sync marker"
                        );
                    }
                    synced = true;
                }
                SyncOutcome::NeedMore => continue,
                SyncOutcome::Overflow => {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "reverse tunnel: no sync marker within bound (stream contaminated)",
                    ));
                }
            }
        }
        loop {
            match dec.next_frame() {
                Ok(Some(frame)) => match frame {
                    Frame::Open(id) => on_open(id).await,
                    Frame::Data(id, d) => {
                        // Clone the sender out and drop the guard before awaiting
                        // the (now bounded) send — never hold the routing lock
                        // across an await. A full inbound channel backpressures
                        // this read loop, which stalls the shared stream read and
                        // so throttles the peer's send rate for this connection.
                        let tx = conns
                            .lock()
                            .await
                            .get(&id)
                            .map(|route| route.inbound.clone());
                        if let Some(tx) = tx {
                            let _ = tx.send(d).await; // best-effort: connection may be gone
                        }
                    }
                    Frame::Close(id) => {
                        // Dropping the inbound sender ends the connection's writer.
                        conns.lock().await.remove(&id);
                    }
                },
                Ok(None) => break, // partial frame — need more bytes
                Err(e) => match dec.resync(MAX_RESYNC_SKIP) {
                    Some((dropped, preview)) => {
                        tracing::warn!(
                            target: "thegn::revtunnel",
                            dropped,
                            error = ?e,
                            preview = %preview_str(&preview),
                            "resynced reverse-tunnel stream after a decode error"
                        );
                        continue;
                    }
                    None => return Err(decode_err(e)),
                },
            }
        }
    }
}

/// HOST endpoint: for each `Open(id)` from the sandbox, dial the real target and
/// pump it. Runs until the stream closes.
pub async fn run_host<S, D>(stream: S, dialer: D) -> io::Result<()>
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
    D: Dialer,
{
    run_host_with_limit(stream, dialer, MAX_TUNNEL_CONNS).await
}

async fn run_host_with_limit<S, D>(stream: S, dialer: D, max_connections: usize) -> io::Result<()>
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
    D: Dialer,
{
    run_host_with_admission(
        stream,
        dialer,
        max_connections,
        GLOBAL_HOST_PERMITS.clone(),
        DIAL_TIMEOUT,
    )
    .await
}

/// Run the host endpoint with configured per-tunnel and shared process-wide caps.
pub async fn run_host_with_caps<S, D>(
    stream: S,
    dialer: D,
    max_connections: usize,
    global_permits: Arc<Semaphore>,
) -> io::Result<()>
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
    D: Dialer,
{
    run_host_with_admission(
        stream,
        dialer,
        max_connections,
        global_permits,
        DIAL_TIMEOUT,
    )
    .await
}

async fn run_host_with_admission<S, D>(
    stream: S,
    dialer: D,
    max_connections: usize,
    global_permits: Arc<Semaphore>,
    dial_timeout: Duration,
) -> io::Result<()>
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
    D: Dialer,
{
    let (rd, wr) = tokio::io::split(stream);
    let (sink, sink_task) = spawn_sink(wr);
    let conns: Conns = Arc::new(Mutex::new(HashMap::new()));
    let permits = Arc::new(Semaphore::new(clamp_permits(max_connections)));
    let rejections = Arc::new(AtomicUsize::new(0));
    let tasks: ConnectionTasks = Arc::new(Mutex::new(JoinSet::new()));
    let conns2 = conns.clone();
    let permits2 = permits.clone();
    let rejections2 = rejections.clone();
    let tasks2 = tasks.clone();
    let mut sink_task = sink_task;
    let read = read_loop(
        rd,
        conns,
        move |id| {
            let dialer = dialer.clone();
            let sink = sink.clone();
            let conns = conns2.clone();
            let permits = permits2.clone();
            let global_permits = global_permits.clone();
            let rejections = rejections2.clone();
            let tasks = tasks2.clone();
            async move {
                reap_finished(&tasks).await;
                let global_permit = match global_permits.try_acquire_owned() {
                    Ok(permit) => permit,
                    Err(_) => {
                        report_rejection(
                            &rejections,
                            "host",
                            RejectionReason::GlobalConnectionLimit,
                        );
                        if let Err(error) = sink.send(encode(&Frame::Close(id))).await {
                            tracing::debug!(target: "thegn::revtunnel", %error, "close reply dropped");
                        }
                        return;
                    }
                };
                let permit = match permits.try_acquire_owned() {
                    Ok(permit) => permit,
                    Err(_) => {
                        report_rejection(&rejections, "host", RejectionReason::ConnectionLimit);
                        drop(global_permit);
                        if let Err(error) = sink.send(encode(&Frame::Close(id))).await {
                            tracing::debug!(target: "thegn::revtunnel", %error, "close reply dropped");
                        }
                        return;
                    }
                };
                let (in_tx, in_rx) = channel::<Vec<u8>>(CHANNEL_DEPTH);
                let mut routes = conns.lock().await;
                if routes.contains_key(&id) {
                    report_rejection(&rejections, "host", RejectionReason::DuplicateId);
                    drop(routes);
                    drop(permit);
                    drop(global_permit);
                    if let Err(error) = sink.send(encode(&Frame::Close(id))).await {
                        tracing::debug!(target: "thegn::revtunnel", %error, "close reply dropped");
                    }
                    return;
                }
                let identity = Arc::new(());
                let mine = identity.clone();
                drop(routes.insert(
                    id,
                    Route {
                        inbound: in_tx,
                        identity,
                    },
                ));
                drop(routes);
                tasks.lock().await.spawn(async move {
                    let dial_result = tokio::time::timeout(dial_timeout, dialer.dial()).await;
                    let dial_failed = match dial_result {
                        Ok(Ok(local)) => {
                            pump_conn(id, local, in_rx, sink.clone()).await;
                            false
                        }
                        Ok(Err(_)) => true,
                        Err(_) => {
                            report_rejection(&rejections, "host", RejectionReason::DialTimeout);
                            true
                        }
                    };
                    let mut routes = conns.lock().await;
                    if routes
                        .get(&id)
                        .is_some_and(|route| Arc::ptr_eq(&route.identity, &mine))
                    {
                        drop(routes.remove(&id));
                    }
                    drop(routes);
                    drop(permit);
                    drop(global_permit);
                    if dial_failed && let Err(error) = sink.send(encode(&Frame::Close(id))).await {
                        tracing::debug!(target: "thegn::revtunnel", %error, "close reply dropped");
                    }
                });
            }
        },
        Some(SYNC_MAGIC),
    );
    let (result, sink_failed) = supervise_sink(read, &mut sink_task).await;
    abort_tasks(&tasks).await;
    if sink_failed {
        result
    } else {
        result.and(stop_sink(sink_task).await)
    }
}

/// SANDBOX endpoint: accept loopback connections on `listener`, open a muxed
/// connection per accept, and pump. `listener` is pre-bound so the caller knows
/// the address. Runs until the stream closes.
pub async fn run_sandbox<S>(stream: S, listener: TcpListener) -> io::Result<()>
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    run_sandbox_with_limit(stream, listener, MAX_TUNNEL_CONNS).await
}

async fn run_sandbox_with_limit<S>(
    stream: S,
    mut listener: impl TunnelListener,
    max_connections: usize,
) -> io::Result<()>
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    let (rd, wr) = tokio::io::split(stream);
    let (sink, sink_task) = spawn_sink(wr);
    // Emit the sync marker as the FIRST bytes on stdout, before any frame, so the
    // host can skip whatever one-time preamble the exec transport prepended (a
    // runtime banner/MOTD/shell echo) and lock onto the framing. See `SYNC_MAGIC`.
    let _ = sink.send(SYNC_MAGIC.to_vec()).await; // best-effort: remote may have vanished before the sync bytes
    let conns: Conns = Arc::new(Mutex::new(HashMap::new()));
    let next_id = Arc::new(AtomicU32::new(1));
    let permits = Arc::new(Semaphore::new(max_connections));
    let rejections = Arc::new(AtomicUsize::new(0));
    let tasks: ConnectionTasks = Arc::new(Mutex::new(JoinSet::new()));

    // Accept loop: each new local connection becomes a muxed Open.
    let acc_conns = conns.clone();
    let acc_sink = sink.clone();
    let acc_ids = next_id.clone();
    let acc_permits = permits.clone();
    let acc_rejections = rejections.clone();
    let acc_tasks = tasks.clone();
    tasks.lock().await.spawn(async move {
        let mut accept_backoff = ACCEPT_BACKOFF_BASE;
        loop {
            let sock = match listener.accept().await {
                Ok(sock) => {
                    accept_backoff = ACCEPT_BACKOFF_BASE;
                    sock
                }
                Err(error) if transient_accept_error(&error) => {
                    report_global_rejection("sandbox", RejectionReason::AcceptTransient);
                    tokio::time::sleep(accept_backoff).await;
                    accept_backoff = next_accept_backoff(accept_backoff);
                    continue;
                }
                Err(error) => {
                    tracing::warn!(target: "thegn::revtunnel", %error, "reverse-tunnel accept loop stopped");
                    break;
                }
            };
            reap_finished(&acc_tasks).await;
            let permit = match acc_permits.clone().try_acquire_owned() {
                Ok(permit) => permit,
                Err(_) => {
                    report_rejection(&acc_rejections, "sandbox", RejectionReason::ConnectionLimit);
                    drop(sock);
                    continue;
                }
            };
            let id = acc_ids.fetch_add(1, Ordering::Relaxed);
            let (in_tx, in_rx) = channel::<Vec<u8>>(CHANNEL_DEPTH);
            let identity = Arc::new(());
            let mine = identity.clone();
            acc_conns.lock().await.insert(
                id,
                Route {
                    inbound: in_tx,
                    identity,
                },
            );
            if acc_sink.send(encode(&Frame::Open(id))).await.is_err() {
                drop(acc_conns.lock().await.remove(&id));
                break;
            }
            let sink = acc_sink.clone();
            let conns = acc_conns.clone();
            acc_tasks.lock().await.spawn(async move {
                let _permit = permit;
                pump_conn(id, sock, in_rx, sink).await;
                let mut routes = conns.lock().await;
                if routes
                    .get(&id)
                    .is_some_and(|route| Arc::ptr_eq(&route.identity, &mine))
                {
                    drop(routes.remove(&id));
                }
            });
        }
    });

    // The sandbox never receives Open; Data/Close route to accepted conns. The
    // host→sandbox direction is thegn's own clean frame stream (no preamble),
    // so no sync marker is expected here.
    let mut sink_task = sink_task;
    let (res, sink_failed) =
        supervise_sink(read_loop(rd, conns, |_id| async {}, None), &mut sink_task).await;
    abort_tasks(&tasks).await;
    if sink_failed {
        res
    } else {
        res.and(stop_sink(sink_task).await)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    static WRITE_STALL_TEST_LOCK: std::sync::LazyLock<tokio::sync::Mutex<()>> =
        std::sync::LazyLock::new(|| tokio::sync::Mutex::new(()));
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    struct DuplexListener {
        accepted: tokio::sync::mpsc::UnboundedReceiver<tokio::io::DuplexStream>,
    }

    struct ScriptedListener {
        accepted: tokio::sync::mpsc::UnboundedReceiver<io::Result<tokio::io::DuplexStream>>,
    }

    impl TunnelListener for ScriptedListener {
        async fn accept(&mut self) -> io::Result<Box<dyn IoStream>> {
            self.accepted
                .recv()
                .await
                .unwrap_or_else(|| {
                    Err(io::Error::new(
                        io::ErrorKind::UnexpectedEof,
                        "listener closed",
                    ))
                })
                .map(|stream| Box::new(stream) as Box<dyn IoStream>)
        }
    }

    impl TunnelListener for DuplexListener {
        async fn accept(&mut self) -> io::Result<Box<dyn IoStream>> {
            self.accepted
                .recv()
                .await
                .map(|stream| Box::new(stream) as Box<dyn IoStream>)
                .ok_or_else(|| io::Error::new(io::ErrorKind::UnexpectedEof, "listener closed"))
        }
    }

    #[derive(Clone)]
    struct HoldingDialer {
        calls: Arc<AtomicUsize>,
        far_ends: tokio::sync::mpsc::UnboundedSender<tokio::io::DuplexStream>,
    }

    impl Dialer for HoldingDialer {
        async fn dial(&self) -> io::Result<Box<dyn IoStream>> {
            self.calls.fetch_add(1, Ordering::Relaxed);
            let (near, far) = tokio::io::duplex(1024);
            self.far_ends
                .send(far)
                .map_err(|_| io::Error::new(io::ErrorKind::BrokenPipe, "test receiver closed"))?;
            Ok(Box::new(near))
        }
    }

    #[derive(Clone)]
    struct FailingDialer {
        calls: Arc<AtomicUsize>,
    }

    impl Dialer for FailingDialer {
        async fn dial(&self) -> io::Result<Box<dyn IoStream>> {
            self.calls.fetch_add(1, Ordering::Relaxed);
            Err(io::Error::new(
                io::ErrorKind::ConnectionRefused,
                "test dial failure",
            ))
        }
    }

    #[derive(Clone)]
    struct PendingDialer {
        calls: Arc<AtomicUsize>,
    }

    impl Dialer for PendingDialer {
        async fn dial(&self) -> io::Result<Box<dyn IoStream>> {
            self.calls.fetch_add(1, Ordering::Relaxed);
            std::future::pending().await
        }
    }

    async fn next_frame<R: AsyncRead + Unpin>(reader: &mut R, decoder: &mut FrameDecoder) -> Frame {
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            let mut bytes = [0u8; 1024];
            loop {
                if let Some(frame) = decoder.next_frame().unwrap() {
                    return frame;
                }
                let n = reader.read(&mut bytes).await.unwrap();
                assert_ne!(n, 0, "host stream closed before expected frame");
                decoder.push(&bytes[..n]);
            }
        })
        .await
        .expect("timed out waiting for reverse-tunnel frame")
    }

    async fn send_opens<W: AsyncWrite + Unpin>(writer: &mut W, ids: &[u32]) {
        let mut bytes = SYNC_MAGIC.to_vec();
        for id in ids {
            bytes.extend(encode(&Frame::Open(*id)));
        }
        tokio::time::timeout(std::time::Duration::from_secs(2), writer.write_all(&bytes))
            .await
            .expect("timed out writing reverse-tunnel opens")
            .unwrap();
    }

    async fn send_open<W: AsyncWrite + Unpin>(writer: &mut W, id: u32) {
        tokio::time::timeout(
            std::time::Duration::from_secs(2),
            writer.write_all(&encode(&Frame::Open(id))),
        )
        .await
        .expect("timed out writing reverse-tunnel open")
        .unwrap();
    }

    #[tokio::test]
    async fn host_caps_concurrent_opens() {
        let (host, mut peer) = tokio::io::duplex(4096);
        let calls = Arc::new(AtomicUsize::new(0));
        let (far_tx, mut far_rx) = tokio::sync::mpsc::unbounded_channel();
        let task = tokio::spawn(run_host_with_limit(
            host,
            HoldingDialer {
                calls: calls.clone(),
                far_ends: far_tx,
            },
            2,
        ));
        send_opens(&mut peer, &[1, 2, 3, 4]).await;
        let mut decoder = FrameDecoder::new();
        let rejected = [
            next_frame(&mut peer, &mut decoder).await,
            next_frame(&mut peer, &mut decoder).await,
        ];
        assert_eq!(rejected, [Frame::Close(3), Frame::Close(4)]);
        assert_eq!(calls.load(Ordering::Relaxed), 2);
        let _first = tokio::time::timeout(std::time::Duration::from_secs(2), far_rx.recv())
            .await
            .unwrap()
            .unwrap();
        let _second = tokio::time::timeout(std::time::Duration::from_secs(2), far_rx.recv())
            .await
            .unwrap()
            .unwrap();
        drop(peer);
        tokio::time::timeout(std::time::Duration::from_secs(2), task)
            .await
            .expect("host did not stop after peer disconnect")
            .unwrap()
            .unwrap();
    }

    #[tokio::test]
    async fn host_permit_released_after_connection_closes() {
        let (host, mut peer) = tokio::io::duplex(4096);
        let calls = Arc::new(AtomicUsize::new(0));
        let (far_tx, mut far_rx) = tokio::sync::mpsc::unbounded_channel();
        let task = tokio::spawn(run_host_with_limit(
            host,
            HoldingDialer {
                calls: calls.clone(),
                far_ends: far_tx,
            },
            1,
        ));
        send_opens(&mut peer, &[1]).await;
        let first_far = tokio::time::timeout(std::time::Duration::from_secs(2), far_rx.recv())
            .await
            .unwrap()
            .unwrap();
        drop(first_far);
        tokio::time::timeout(
            std::time::Duration::from_secs(2),
            peer.write_all(&encode(&Frame::Close(1))),
        )
        .await
        .expect("timed out closing first logical connection")
        .unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            for _ in 0..20 {
                send_open(&mut peer, 2).await;
                if calls.load(Ordering::Relaxed) == 2 {
                    return;
                }
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
            panic!("connection permit was not released");
        })
        .await
        .expect("timed out waiting for released connection permit");
        let _second_far = tokio::time::timeout(std::time::Duration::from_secs(2), far_rx.recv())
            .await
            .expect("second dial did not complete")
            .expect("second open was not dialed");
        assert_eq!(calls.load(Ordering::Relaxed), 2);
        drop(peer);
        tokio::time::timeout(std::time::Duration::from_secs(2), task)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
    }

    #[tokio::test]
    async fn stale_host_pump_does_not_remove_reopened_route() {
        let (host, mut peer) = tokio::io::duplex(4096);
        let (far_tx, mut far_rx) = tokio::sync::mpsc::unbounded_channel();
        let task = tokio::spawn(run_host_with_limit(
            host,
            HoldingDialer {
                calls: Arc::new(AtomicUsize::new(0)),
                far_ends: far_tx,
            },
            2,
        ));
        send_opens(&mut peer, &[1]).await;
        let first_far = tokio::time::timeout(std::time::Duration::from_secs(2), far_rx.recv())
            .await
            .unwrap()
            .unwrap();

        tokio::time::timeout(
            std::time::Duration::from_secs(2),
            peer.write_all(&encode(&Frame::Close(1))),
        )
        .await
        .expect("timed out closing the first route")
        .unwrap();
        send_open(&mut peer, 1).await;
        let mut second_far = tokio::time::timeout(std::time::Duration::from_secs(2), far_rx.recv())
            .await
            .expect("reopened route was not dialed")
            .expect("dialer receiver closed");

        // The old pump is still alive until its local read end closes. Let its
        // completion and route cleanup happen before exercising the new route.
        drop(first_far);
        let mut decoder = FrameDecoder::new();
        assert_eq!(next_frame(&mut peer, &mut decoder).await, Frame::Close(1));
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        tokio::time::timeout(
            std::time::Duration::from_secs(2),
            peer.write_all(&encode(&Frame::Data(1, b"still routed".to_vec()))),
        )
        .await
        .expect("timed out sending data to reopened route")
        .unwrap();
        let mut received = [0u8; 12];
        tokio::time::timeout(
            std::time::Duration::from_secs(2),
            second_far.read_exact(&mut received),
        )
        .await
        .expect("old pump removed the reopened route")
        .unwrap();
        assert_eq!(&received, b"still routed");

        drop(peer);
        tokio::time::timeout(std::time::Duration::from_secs(2), task)
            .await
            .expect("host did not stop after peer disconnect")
            .unwrap()
            .unwrap();
    }

    #[tokio::test]
    async fn host_dial_failure_does_not_leak_entry_or_permit() {
        let (host, mut peer) = tokio::io::duplex(4096);
        let calls = Arc::new(AtomicUsize::new(0));
        let task = tokio::spawn(run_host_with_limit(
            host,
            FailingDialer {
                calls: calls.clone(),
            },
            1,
        ));
        send_opens(&mut peer, &[7]).await;
        let mut decoder = FrameDecoder::new();
        assert_eq!(next_frame(&mut peer, &mut decoder).await, Frame::Close(7));
        for _ in 0..10 {
            send_open(&mut peer, 7).await;
            assert_eq!(next_frame(&mut peer, &mut decoder).await, Frame::Close(7));
            if calls.load(Ordering::Relaxed) == 2 {
                break;
            }
        }
        assert_eq!(calls.load(Ordering::Relaxed), 2);
        drop(peer);
        tokio::time::timeout(std::time::Duration::from_secs(2), task)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn host_dial_timeout_closes_route_and_releases_permit() {
        let (host, mut peer) = tokio::io::duplex(16 * 1024);
        let calls = Arc::new(AtomicUsize::new(0));
        let task = tokio::spawn(run_host_with_admission(
            host,
            PendingDialer {
                calls: calls.clone(),
            },
            1,
            Arc::new(Semaphore::new(1)),
            DIAL_TIMEOUT,
        ));
        send_opens(&mut peer, &[1]).await;
        tokio::task::yield_now().await;
        tokio::time::advance(DIAL_TIMEOUT + Duration::from_secs(1)).await;
        let mut decoder = FrameDecoder::new();
        assert_eq!(next_frame(&mut peer, &mut decoder).await, Frame::Close(1));
        send_open(&mut peer, 2).await;
        tokio::time::timeout(Duration::from_secs(2), async {
            while calls.load(Ordering::Relaxed) < 2 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("timed-out dial did not release the permit and read loop");
        drop(peer);
        task.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn global_cap_spans_tunnels() {
        let shared_permits = Arc::new(Semaphore::new(1));
        let permit_observer = shared_permits.clone();
        let (host_a, mut peer_a) = tokio::io::duplex(4096);
        let calls_a = Arc::new(AtomicUsize::new(0));
        let (far_a_tx, mut far_a_rx) = tokio::sync::mpsc::unbounded_channel();
        let task_a = tokio::spawn(run_host_with_admission(
            host_a,
            HoldingDialer {
                calls: calls_a.clone(),
                far_ends: far_a_tx,
            },
            1,
            shared_permits.clone(),
            std::time::Duration::from_secs(2),
        ));

        let (host_b, mut peer_b) = tokio::io::duplex(4096);
        let calls_b = Arc::new(AtomicUsize::new(0));
        let (far_b_tx, mut far_b_rx) = tokio::sync::mpsc::unbounded_channel();
        let task_b = tokio::spawn(run_host_with_admission(
            host_b,
            HoldingDialer {
                calls: calls_b.clone(),
                far_ends: far_b_tx,
            },
            1,
            shared_permits,
            std::time::Duration::from_secs(2),
        ));

        send_opens(&mut peer_a, &[1]).await;
        let far_a = tokio::time::timeout(std::time::Duration::from_secs(2), far_a_rx.recv())
            .await
            .unwrap()
            .unwrap();
        send_opens(&mut peer_b, &[9]).await;
        let mut decoder_b = FrameDecoder::new();
        assert_eq!(
            next_frame(&mut peer_b, &mut decoder_b).await,
            Frame::Close(9)
        );
        assert_eq!(calls_b.load(Ordering::Relaxed), 0);

        drop(far_a);
        tokio::time::timeout(
            std::time::Duration::from_secs(2),
            peer_a.write_all(&encode(&Frame::Close(1))),
        )
        .await
        .expect("timed out closing tunnel A connection")
        .unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            while permit_observer.available_permits() != 1 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("tunnel A did not release the process-wide permit");
        send_open(&mut peer_b, 10).await;
        let _far_b = tokio::time::timeout(std::time::Duration::from_secs(2), far_b_rx.recv())
            .await
            .expect("released global permit did not admit tunnel B")
            .expect("tunnel B dialer receiver closed");
        assert_eq!(calls_a.load(Ordering::Relaxed), 1);
        assert_eq!(calls_b.load(Ordering::Relaxed), 1);

        drop(peer_a);
        drop(peer_b);
        for task in [task_a, task_b] {
            tokio::time::timeout(std::time::Duration::from_secs(2), task)
                .await
                .expect("host tunnel did not stop after peer disconnect")
                .unwrap()
                .unwrap();
        }
    }

    #[test]
    fn permit_counts_clamp_to_tokio_limit() {
        assert_eq!(clamp_permits(usize::MAX), Semaphore::MAX_PERMITS);
    }

    #[tokio::test]
    async fn configured_host_cap_rejects_excess_open_and_recovers() {
        let (host, mut peer) = tokio::io::duplex(16 * 1024);
        let calls = Arc::new(AtomicUsize::new(0));
        let (far_tx, mut far_rx) = tokio::sync::mpsc::unbounded_channel();
        let task = tokio::spawn(run_host_with_caps(
            host,
            HoldingDialer {
                calls: calls.clone(),
                far_ends: far_tx,
            },
            2,
            Arc::new(Semaphore::new(2)),
        ));

        send_opens(&mut peer, &[1, 2, 3]).await;
        let far_one = tokio::time::timeout(std::time::Duration::from_secs(2), far_rx.recv())
            .await
            .unwrap()
            .unwrap();
        let far_two = tokio::time::timeout(std::time::Duration::from_secs(2), far_rx.recv())
            .await
            .unwrap()
            .unwrap();
        let mut decoder = FrameDecoder::new();
        assert_eq!(next_frame(&mut peer, &mut decoder).await, Frame::Close(3));
        assert_eq!(calls.load(Ordering::Relaxed), 2);

        drop(far_one);
        drop(far_two);
        drop(peer);
        tokio::time::timeout(std::time::Duration::from_secs(2), task)
            .await
            .expect("host did not stop after peer disconnect")
            .unwrap()
            .unwrap();
    }

    #[tokio::test]
    async fn host_open_storm_stays_bounded_and_recovers_all_permits() {
        const PER_TUNNEL: usize = 8;
        const GLOBAL: usize = 16;
        const STORM: u32 = 1_000;

        let rejections_before = rejection_total();
        let global = Arc::new(Semaphore::new(GLOBAL));
        let (host, mut peer) = tokio::io::duplex(64 * 1024);
        let calls = Arc::new(AtomicUsize::new(0));
        let (far_tx, mut far_rx) = tokio::sync::mpsc::unbounded_channel();
        let task = tokio::spawn(run_host_with_caps(
            host,
            HoldingDialer {
                calls: calls.clone(),
                far_ends: far_tx,
            },
            PER_TUNNEL,
            global.clone(),
        ));

        let ids: Vec<u32> = (1..=STORM).collect();
        send_opens(&mut peer, &ids).await;
        let mut decoder = FrameDecoder::new();
        for id in (PER_TUNNEL as u32 + 1)..=STORM {
            assert_eq!(next_frame(&mut peer, &mut decoder).await, Frame::Close(id));
        }
        assert!(rejection_total() >= rejections_before + (STORM as u64 - PER_TUNNEL as u64));
        let mut far_ends = Vec::with_capacity(PER_TUNNEL);
        for _ in 0..PER_TUNNEL {
            far_ends.push(
                tokio::time::timeout(std::time::Duration::from_secs(2), far_rx.recv())
                    .await
                    .expect("admitted host connection did not dial")
                    .expect("holding dialer receiver closed"),
            );
        }
        assert_eq!(calls.load(Ordering::Relaxed), PER_TUNNEL);
        assert_eq!(global.available_permits(), GLOBAL - PER_TUNNEL);

        let closes: Vec<u8> = (1..=PER_TUNNEL as u32)
            .flat_map(|id| encode(&Frame::Close(id)))
            .collect();
        tokio::time::timeout(std::time::Duration::from_secs(2), peer.write_all(&closes))
            .await
            .expect("timed out disconnecting admitted host peers")
            .unwrap();
        drop(far_ends);
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            while global.available_permits() != GLOBAL {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("host storm leaked a global permit after peers disconnected");

        send_open(&mut peer, STORM + 1).await;
        let _recovered = tokio::time::timeout(std::time::Duration::from_secs(2), far_rx.recv())
            .await
            .expect("fresh host Open did not recover after the storm")
            .expect("holding dialer receiver closed after recovery");
        assert_eq!(calls.load(Ordering::Relaxed), PER_TUNNEL + 1);
        drop(peer);
        tokio::time::timeout(std::time::Duration::from_secs(2), task)
            .await
            .expect("host did not stop after peer disconnect")
            .unwrap()
            .unwrap();
        assert_eq!(global.available_permits(), GLOBAL);
    }

    #[tokio::test]
    async fn host_peer_disconnect_after_open_storm_aborts_all_admitted_tasks() {
        const CAP: usize = 8;
        const GLOBAL: usize = 16;

        let global = Arc::new(Semaphore::new(GLOBAL));
        let (host, mut peer) = tokio::io::duplex(64 * 1024);
        let calls = Arc::new(AtomicUsize::new(0));
        let (far_tx, mut far_rx) = tokio::sync::mpsc::unbounded_channel();
        let task = tokio::spawn(run_host_with_caps(
            host,
            HoldingDialer {
                calls: calls.clone(),
                far_ends: far_tx,
            },
            CAP,
            global.clone(),
        ));

        let ids: Vec<u32> = (1..=1_000).collect();
        send_opens(&mut peer, &ids).await;
        let first_far = tokio::time::timeout(std::time::Duration::from_secs(2), far_rx.recv())
            .await
            .expect("storm did not admit a host connection before disconnect")
            .expect("holding dialer receiver closed");
        drop(peer);
        tokio::time::timeout(std::time::Duration::from_secs(2), task)
            .await
            .expect("host did not abort its connection tasks after peer disconnect")
            .unwrap()
            .unwrap();
        let mut byte = [0u8; 1];
        let read = tokio::time::timeout(std::time::Duration::from_secs(2), async {
            let mut first_far = first_far;
            first_far.read(&mut byte).await
        })
        .await
        .expect("admitted connection task survived host tunnel cancellation")
        .unwrap();
        assert_eq!(read, 0, "aborted host pump should drop its stream");
        assert_eq!(global.available_permits(), GLOBAL);
        assert!(calls.load(Ordering::Relaxed) <= CAP);
    }

    #[tokio::test]
    async fn two_host_open_storms_share_the_global_cap() {
        const PER_TUNNEL: usize = 64;
        const GLOBAL: usize = 16;
        const STORM: u32 = 1_000;

        let global = Arc::new(Semaphore::new(GLOBAL));
        let (host_a, mut peer_a) = tokio::io::duplex(64 * 1024);
        let (host_b, mut peer_b) = tokio::io::duplex(64 * 1024);
        let calls_a = Arc::new(AtomicUsize::new(0));
        let calls_b = Arc::new(AtomicUsize::new(0));
        let (far_a_tx, mut far_a_rx) = tokio::sync::mpsc::unbounded_channel();
        let (far_b_tx, mut far_b_rx) = tokio::sync::mpsc::unbounded_channel();
        let task_a = tokio::spawn(run_host_with_caps(
            host_a,
            HoldingDialer {
                calls: calls_a.clone(),
                far_ends: far_a_tx,
            },
            PER_TUNNEL,
            global.clone(),
        ));
        let task_b = tokio::spawn(run_host_with_caps(
            host_b,
            HoldingDialer {
                calls: calls_b.clone(),
                far_ends: far_b_tx,
            },
            PER_TUNNEL,
            global.clone(),
        ));

        let ids: Vec<u32> = (1..=STORM).collect();
        send_opens(&mut peer_a, &ids).await;
        send_opens(&mut peer_b, &ids).await;
        let mut decoder_a = FrameDecoder::new();
        let mut decoder_b = FrameDecoder::new();
        let mut rejected_a = Vec::new();
        let mut rejected_b = Vec::new();
        for _ in 0..(STORM as usize * 2 - GLOBAL) {
            tokio::select! {
                frame = next_frame(&mut peer_a, &mut decoder_a) => {
                    let Frame::Close(id) = frame else { panic!("tunnel A sent a non-Close storm response"); };
                    rejected_a.push(id);
                }
                frame = next_frame(&mut peer_b, &mut decoder_b) => {
                    let Frame::Close(id) = frame else { panic!("tunnel B sent a non-Close storm response"); };
                    rejected_b.push(id);
                }
            }
        }
        let admitted_a = STORM as usize - rejected_a.len();
        let admitted_b = STORM as usize - rejected_b.len();
        assert_eq!(admitted_a + admitted_b, GLOBAL);
        assert_eq!(
            rejected_a,
            ((admitted_a as u32 + 1)..=STORM).collect::<Vec<_>>()
        );
        assert_eq!(
            rejected_b,
            ((admitted_b as u32 + 1)..=STORM).collect::<Vec<_>>()
        );
        assert_eq!(global.available_permits(), 0);

        let mut far_a = Vec::new();
        let mut far_b = Vec::new();
        for _ in 0..admitted_a {
            far_a.push(
                tokio::time::timeout(std::time::Duration::from_secs(2), far_a_rx.recv())
                    .await
                    .expect("tunnel A dial did not complete")
                    .expect("tunnel A dialer closed"),
            );
        }
        for _ in 0..admitted_b {
            far_b.push(
                tokio::time::timeout(std::time::Duration::from_secs(2), far_b_rx.recv())
                    .await
                    .expect("tunnel B dial did not complete")
                    .expect("tunnel B dialer closed"),
            );
        }
        assert_eq!(calls_a.load(Ordering::Relaxed), admitted_a);
        assert_eq!(calls_b.load(Ordering::Relaxed), admitted_b);
        let closes: Vec<u8> = ids
            .iter()
            .flat_map(|id| encode(&Frame::Close(*id)))
            .collect();
        tokio::time::timeout(std::time::Duration::from_secs(2), peer_a.write_all(&closes))
            .await
            .expect("timed out disconnecting tunnel A storm connections")
            .unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(2), peer_b.write_all(&closes))
            .await
            .expect("timed out disconnecting tunnel B storm connections")
            .unwrap();
        drop(far_a);
        drop(far_b);
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            while global.available_permits() != GLOBAL {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("shared host storm leaked global permits");

        send_open(&mut peer_a, STORM + 1).await;
        let _recovered = tokio::time::timeout(std::time::Duration::from_secs(2), far_a_rx.recv())
            .await
            .expect("tunnel A did not recover after the shared storm")
            .expect("tunnel A dialer closed after recovery");
        drop(peer_a);
        drop(peer_b);
        for tunnel in [task_a, task_b] {
            tokio::time::timeout(std::time::Duration::from_secs(2), tunnel)
                .await
                .expect("host tunnel did not stop after disconnect")
                .unwrap()
                .unwrap();
        }
        assert_eq!(global.available_permits(), GLOBAL);
    }

    #[tokio::test]
    async fn rejection_total_counts_connection_limit_rejections() {
        let before = rejection_total();
        let (host, mut peer) = tokio::io::duplex(4096);
        let (far_tx, mut far_rx) = tokio::sync::mpsc::unbounded_channel();
        let task = tokio::spawn(run_host_with_admission(
            host,
            HoldingDialer {
                calls: Arc::new(AtomicUsize::new(0)),
                far_ends: far_tx,
            },
            1,
            Arc::new(Semaphore::new(1)),
            std::time::Duration::from_secs(2),
        ));
        send_opens(&mut peer, &[1, 2]).await;
        let mut decoder = FrameDecoder::new();
        assert_eq!(next_frame(&mut peer, &mut decoder).await, Frame::Close(2));
        let _far = tokio::time::timeout(std::time::Duration::from_secs(2), far_rx.recv())
            .await
            .unwrap()
            .unwrap();
        assert!(rejection_total() > before);
        drop(peer);
        tokio::time::timeout(std::time::Duration::from_secs(2), task)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
    }

    #[tokio::test]
    async fn sandbox_drops_accepted_socket_at_connection_cap() {
        let (sandbox, mut peer) = tokio::io::duplex(4096);
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let task = tokio::spawn(run_sandbox_with_limit(sandbox, listener, 1));
        let mut sync = vec![0u8; SYNC_MAGIC.len()];
        tokio::time::timeout(
            std::time::Duration::from_secs(2),
            peer.read_exact(&mut sync),
        )
        .await
        .expect("sandbox did not send the sync marker")
        .unwrap();
        assert_eq!(sync, SYNC_MAGIC);

        let first = tokio::time::timeout(
            std::time::Duration::from_secs(2),
            tokio::net::TcpStream::connect(addr),
        )
        .await
        .expect("first sandbox client connect timed out")
        .unwrap();
        let mut decoder = FrameDecoder::new();
        assert_eq!(next_frame(&mut peer, &mut decoder).await, Frame::Open(1));
        let mut excess = tokio::time::timeout(
            std::time::Duration::from_secs(2),
            tokio::net::TcpStream::connect(addr),
        )
        .await
        .expect("excess sandbox client connect timed out")
        .unwrap();
        let mut byte = [0u8; 1];
        let read = tokio::time::timeout(std::time::Duration::from_secs(2), excess.read(&mut byte))
            .await
            .expect("sandbox kept the excess accepted socket")
            .unwrap();
        assert_eq!(read, 0, "excess accepted socket should be dropped");

        drop(first);
        drop(excess);
        drop(peer);
        tokio::time::timeout(std::time::Duration::from_secs(2), task)
            .await
            .expect("sandbox did not stop after tunnel disconnect")
            .unwrap()
            .unwrap();
    }

    #[tokio::test]
    async fn sandbox_accept_storm_keeps_only_capped_pumps_and_recovers() {
        const CAP: usize = 8;
        const STORM: usize = 1_000;

        let (sandbox, mut peer) = tokio::io::duplex(64 * 1024);
        let (accept_tx, accept_rx) = tokio::sync::mpsc::unbounded_channel();
        let listener = DuplexListener {
            accepted: accept_rx,
        };
        let task = tokio::spawn(run_sandbox_with_limit(sandbox, listener, CAP));
        let mut sync = vec![0u8; SYNC_MAGIC.len()];
        tokio::time::timeout(
            std::time::Duration::from_secs(2),
            peer.read_exact(&mut sync),
        )
        .await
        .expect("sandbox did not send the sync marker")
        .unwrap();
        assert_eq!(sync, SYNC_MAGIC);

        let mut clients = Vec::with_capacity(STORM);
        for _ in 0..STORM {
            let (sandbox_end, client_end) = tokio::io::duplex(1024);
            accept_tx.send(sandbox_end).unwrap();
            clients.push(client_end);
        }
        let mut decoder = FrameDecoder::new();
        for id in 1..=CAP as u32 {
            assert_eq!(next_frame(&mut peer, &mut decoder).await, Frame::Open(id));
        }
        for (index, client) in clients.iter_mut().enumerate().skip(CAP) {
            let mut byte = [0u8; 1];
            let read =
                tokio::time::timeout(std::time::Duration::from_secs(2), client.read(&mut byte))
                    .await
                    .unwrap_or_else(|_| panic!("sandbox retained excess accepted stream {index}"))
                    .unwrap();
            assert_eq!(read, 0, "excess sandbox stream should be dropped");
        }

        let closes: Vec<u8> = (1..=CAP as u32)
            .flat_map(|id| encode(&Frame::Close(id)))
            .collect();
        tokio::time::timeout(std::time::Duration::from_secs(2), peer.write_all(&closes))
            .await
            .expect("timed out closing sandbox storm connections")
            .unwrap();
        drop(clients);
        let mut closed_ids = Vec::with_capacity(CAP);
        for _ in 0..CAP {
            let Frame::Close(id) = next_frame(&mut peer, &mut decoder).await else {
                panic!("sandbox sent a non-Close frame while closing storm connections");
            };
            closed_ids.push(id);
        }
        closed_ids.sort_unstable();
        assert_eq!(closed_ids, (1..=CAP as u32).collect::<Vec<_>>());

        let (sandbox_end, mut recovered_client) = tokio::io::duplex(1024);
        accept_tx.send(sandbox_end).unwrap();
        assert_eq!(
            next_frame(&mut peer, &mut decoder).await,
            Frame::Open(CAP as u32 + 1)
        );
        tokio::time::timeout(
            std::time::Duration::from_secs(2),
            recovered_client.write_all(b"recovered"),
        )
        .await
        .expect("timed out writing through recovered sandbox stream")
        .unwrap();
        assert_eq!(
            next_frame(&mut peer, &mut decoder).await,
            Frame::Data(CAP as u32 + 1, b"recovered".to_vec())
        );

        drop(peer);
        drop(accept_tx);
        tokio::time::timeout(std::time::Duration::from_secs(2), task)
            .await
            .expect("sandbox did not stop after tunnel disconnect")
            .unwrap()
            .unwrap();
    }

    #[tokio::test]
    async fn tunnel_end_aborts_connection_tasks() {
        let (host, mut peer) = tokio::io::duplex(4096);
        let (far_tx, mut far_rx) = tokio::sync::mpsc::unbounded_channel();
        let task = tokio::spawn(run_host_with_limit(
            host,
            HoldingDialer {
                calls: Arc::new(AtomicUsize::new(0)),
                far_ends: far_tx,
            },
            1,
        ));
        send_opens(&mut peer, &[1]).await;
        let mut far = tokio::time::timeout(std::time::Duration::from_secs(2), far_rx.recv())
            .await
            .unwrap()
            .unwrap();
        drop(peer);
        tokio::time::timeout(std::time::Duration::from_secs(2), task)
            .await
            .expect("host did not finish after tunnel end")
            .unwrap()
            .unwrap();
        let mut byte = [0u8; 1];
        let read = tokio::time::timeout(std::time::Duration::from_secs(2), far.read(&mut byte))
            .await
            .expect("dialed connection survived tunnel cancellation")
            .unwrap();
        assert_eq!(read, 0, "dialed stream should be dropped with the tunnel");
    }

    /// A `Dialer` whose target echoes everything written to it (stands in for a
    /// real host service like `tgproxy`).
    #[derive(Clone)]
    struct EchoDialer;
    impl Dialer for EchoDialer {
        async fn dial(&self) -> io::Result<Box<dyn IoStream>> {
            let (near, far) = tokio::io::duplex(64 * 1024);
            // Echo task on the far end: copy its input back to its output.
            tokio::spawn(async move {
                let (mut r, mut w) = tokio::io::split(far);
                let _ = tokio::io::copy(&mut r, &mut w).await; // best-effort: copy error just ends the pump
            });
            Ok(Box::new(near))
        }
    }

    /// The `ExecSession` adapter: writes become `Stdin` control, and `Stdout`
    /// frames become reads. Mock session built from raw channels.
    #[tokio::test]
    async fn exec_stream_bridges_session_io() {
        use crate::provider::{ExecControl, ExecFrame, ExecSession};
        let (ftx, frx) = tokio::sync::mpsc::channel::<ExecFrame>(16);
        let (ctx, mut crx) = tokio::sync::mpsc::channel::<ExecControl>(16);
        let (_sid_tx, sid_rx) = tokio::sync::watch::channel::<Option<String>>(None);
        let session = ExecSession {
            frames: frx,
            control: ctx,
            session_id: sid_rx,
        };
        let mut stream = exec_stream(session);

        stream.write_all(b"to-sandbox").await.unwrap();
        match crx.recv().await.unwrap() {
            ExecControl::Stdin(d) => assert_eq!(d, b"to-sandbox"),
            _ => panic!("expected Stdin control"),
        }

        ftx.send(ExecFrame::Stdout(b"from-sandbox".to_vec()))
            .await
            .unwrap();
        let mut buf = [0u8; 12];
        stream.read_exact(&mut buf).await.unwrap();
        assert_eq!(&buf, b"from-sandbox");
    }

    /// Full end-to-end mock: a client connects to the sandbox's loopback port; the
    /// bytes traverse sandbox→mux→host→echo-target and back, all over an in-memory
    /// duplex "exec stream". No real network beyond loopback, no sprite.
    #[tokio::test]
    async fn reverse_tunnel_round_trips_over_mock_stream() {
        let (host_side, sandbox_side) = tokio::io::duplex(64 * 1024);
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        tokio::spawn(run_host(host_side, EchoDialer));
        tokio::spawn(run_sandbox(sandbox_side, listener));

        let mut client = tokio::net::TcpStream::connect(addr).await.unwrap();
        client.write_all(b"hello-proxy").await.unwrap();
        let mut buf = [0u8; 11];
        client.read_exact(&mut buf).await.unwrap();
        assert_eq!(&buf, b"hello-proxy", "bytes echoed back through the tunnel");
    }

    /// Two concurrent client connections stay independent over the single muxed
    /// stream (distinct ids, no cross-talk).
    #[tokio::test]
    async fn reverse_tunnel_multiplexes_concurrent_connections() {
        let (host_side, sandbox_side) = tokio::io::duplex(64 * 1024);
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(run_host(host_side, EchoDialer));
        tokio::spawn(run_sandbox(sandbox_side, listener));

        let mut a = tokio::net::TcpStream::connect(addr).await.unwrap();
        let mut b = tokio::net::TcpStream::connect(addr).await.unwrap();
        a.write_all(b"aaaa").await.unwrap();
        b.write_all(b"bbbb").await.unwrap();
        let mut ba = [0u8; 4];
        let mut bb = [0u8; 4];
        a.read_exact(&mut ba).await.unwrap();
        b.read_exact(&mut bb).await.unwrap();
        assert_eq!(&ba, b"aaaa");
        assert_eq!(&bb, b"bbbb");
    }

    /// A larger payload (multiple mux chunks) reassembles correctly.
    #[tokio::test]
    async fn reverse_tunnel_handles_large_payload() {
        let (host_side, sandbox_side) = tokio::io::duplex(64 * 1024);
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(run_host(host_side, EchoDialer));
        tokio::spawn(run_sandbox(sandbox_side, listener));

        let payload = vec![0x5Au8; 200 * 1024];
        let client = tokio::net::TcpStream::connect(addr).await.unwrap();
        let (mut rd, mut wr) = client.into_split();
        let p2 = payload.clone();
        let writer = tokio::spawn(async move {
            wr.write_all(&p2).await.unwrap();
            wr.shutdown().await.unwrap();
        });
        let mut got = Vec::new();
        rd.read_to_end(&mut got).await.unwrap();
        writer.await.unwrap();
        assert_eq!(got, payload, "large payload round-trips intact");
    }

    /// The host tolerates a startup preamble the exec transport prepends to the
    /// sandbox's stdout (a banner/MOTD/shell echo): it skips to the sync marker
    /// `run_sandbox` emits, then decodes frames normally. Regression for the
    /// `PayloadTooLarge` desync that killed the proxy tunnel.
    #[tokio::test]
    async fn host_skips_startup_preamble_before_frames() {
        // Three channels wire host ⇄ sandbox with a splice on the sandbox→host
        // leg that injects a banner ahead of run_sandbox's real (SYNC_MAGIC-led)
        // output — exactly the transport preamble that desynced the live tunnel.
        let (h2s_host, h2s_sbx) = tokio::io::duplex(64 * 1024); // host→sandbox control
        let (sbx_out_a, sbx_out_b) = tokio::io::duplex(64 * 1024); // run_sandbox's writes
        let (s2h_a, s2h_b) = tokio::io::duplex(64 * 1024); // spliced → run_host reads

        let (h2s_sbx_rd, _h2s_sbx_wr) = tokio::io::split(h2s_sbx);
        let (_sbx_out_a_rd, sbx_out_a_wr) = tokio::io::split(sbx_out_a);
        let (mut sbx_out_b_rd, _sbx_out_b_wr) = tokio::io::split(sbx_out_b);
        let (s2h_b_rd, _s2h_b_wr) = tokio::io::split(s2h_b);
        let (_s2h_a_rd, mut s2h_a_wr) = tokio::io::split(s2h_a);
        let (_h2s_host_rd, h2s_host_wr) = tokio::io::split(h2s_host);

        // Splice: banner first, then relay run_sandbox's output verbatim.
        tokio::spawn(async move {
            s2h_a_wr
                .write_all(b"sprite-vm ready\r\n\x1b[0m$ ")
                .await
                .unwrap();
            let _ = tokio::io::copy(&mut sbx_out_b_rd, &mut s2h_a_wr).await; // best-effort: test pipe pump; copy error ends the loop
        });

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        // run_host reads the spliced (banner + frames) stream; writes control back.
        tokio::spawn(run_host(tokio::io::join(s2h_b_rd, h2s_host_wr), EchoDialer));
        // run_sandbox reads host control; writes frames (SYNC_MAGIC-led) to splice.
        tokio::spawn(run_sandbox(
            tokio::io::join(h2s_sbx_rd, sbx_out_a_wr),
            listener,
        ));

        let mut client = tokio::net::TcpStream::connect(addr).await.unwrap();
        client.write_all(b"through-preamble").await.unwrap();
        let mut buf = [0u8; 16];
        client.read_exact(&mut buf).await.unwrap();
        assert_eq!(
            &buf, b"through-preamble",
            "tunnel works despite a startup banner on the sandbox stdout"
        );
    }

    /// Regression: the outbound frame sink is BOUNDED, so a producer outracing a
    /// stalled stream write can't queue frames without limit (the memory-balloon
    /// this audit targets). We wire `spawn_sink` to a writer whose reader is
    /// never drained; after `CHANNEL_DEPTH` frames the writer task is parked on
    /// `write_all` holding one frame and the channel fills, so further
    /// `try_send`s fail with `Full`. An unbounded sink would accept every one.
    #[tokio::test]
    async fn sink_is_bounded_under_a_stalled_writer() {
        use tokio::sync::mpsc::error::TrySendError;

        // A duplex whose far end we never read → writes block once its ~64 KiB
        // buffer fills, stalling the sink's writer task.
        let (near, _far) = tokio::io::duplex(64 * 1024);
        let (sink, sink_task) = spawn_sink(near);

        // Frames large enough that a handful overflow the duplex write buffer,
        // guaranteeing the writer task blocks rather than draining forever.
        let frame = vec![0u8; 32 * 1024];
        // Drain the initial slack (duplex buffer + channel depth + the one frame
        // the writer pops before parking). Once saturated, try_send must reject.
        let mut rejected = false;
        for _ in 0..(CHANNEL_DEPTH + 16) {
            match sink.try_send(frame.clone()) {
                Ok(()) => {
                    // Let the writer task run so it can pop/park deterministically.
                    tokio::task::yield_now().await;
                }
                Err(TrySendError::Full(_)) => {
                    rejected = true;
                    break;
                }
                Err(TrySendError::Closed(_)) => panic!("sink closed unexpectedly"),
            }
        }
        assert!(
            rejected,
            "bounded sink must reject sends once full under a stalled writer \
             (an unbounded sink would accept every frame and balloon memory)"
        );
        stop_sink(sink_task).await.unwrap();
    }

    /// A large multi-frame payload still round-trips end-to-end now that every
    /// hop uses bounded `send().await` — proving backpressure throttles rather
    /// than deadlocks the tunnel (the sink and per-conn inbound channels drain
    /// on independent tasks, so there is no cycle).
    #[tokio::test]
    async fn bounded_tunnel_round_trips_backpressured_payload() {
        let (host_side, sandbox_side) = tokio::io::duplex(16 * 1024);
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(run_host(host_side, EchoDialer));
        tokio::spawn(run_sandbox(sandbox_side, listener));

        // Much larger than any single channel/buffer, forcing sustained
        // backpressure through the bounded sink + inbound channels.
        let payload = vec![0x5Au8; 4 * 1024 * 1024];
        let client = tokio::net::TcpStream::connect(addr).await.unwrap();
        let (mut rd, mut wr) = client.into_split();
        let p2 = payload.clone();
        let writer = tokio::spawn(async move {
            wr.write_all(&p2).await.unwrap();
            wr.shutdown().await.unwrap();
        });
        let mut got = Vec::new();
        rd.read_to_end(&mut got).await.unwrap();
        writer.await.unwrap();
        assert_eq!(
            got, payload,
            "backpressured payload round-trips intact over bounded channels"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn sandbox_retries_emfile_and_pumps_later_accepts() {
        let (sandbox, mut peer) = tokio::io::duplex(4096);
        let (accept_tx, accept_rx) = tokio::sync::mpsc::unbounded_channel();
        let listener = ScriptedListener {
            accepted: accept_rx,
        };
        let task = tokio::spawn(run_sandbox_with_limit(sandbox, listener, 1));
        let mut sync = vec![0u8; SYNC_MAGIC.len()];
        peer.read_exact(&mut sync).await.unwrap();
        assert_eq!(sync, SYNC_MAGIC);

        let before = rejection_counts().accept_transient;
        accept_tx
            .send(Err(
                crate::plugin::platform::accept_resource_error_for_test(),
            ))
            .unwrap();
        let (sandbox_conn, mut client) = tokio::io::duplex(128);
        accept_tx.send(Ok(sandbox_conn)).unwrap();
        drop(accept_tx);
        tokio::task::yield_now().await;
        tokio::time::advance(ACCEPT_BACKOFF_BASE + Duration::from_secs(1)).await;

        let mut decoder = FrameDecoder::new();
        let open =
            tokio::time::timeout(Duration::from_secs(3), next_frame(&mut peer, &mut decoder))
                .await
                .expect("timed out waiting for accepted connection Open");
        assert_eq!(open, Frame::Open(1));
        peer.write_all(&encode_data_chunked(1, b"accepted-after-emfile"))
            .await
            .unwrap();
        let mut received = vec![0; b"accepted-after-emfile".len()];
        client.read_exact(&mut received).await.unwrap();
        assert_eq!(received, b"accepted-after-emfile");
        assert_eq!(rejection_counts().accept_transient, before + 1);

        drop(peer);
        task.await.unwrap().unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn idle_deadline_closes_only_the_stalled_connection() {
        let (local, _client) = tokio::io::duplex(128);
        let (mut peer, sink_stream) = tokio::io::duplex(4096);
        let (sink, sink_task) = spawn_sink(sink_stream);
        let survivor_sink = sink.clone();
        let (in_tx, in_rx) = channel(CHANNEL_DEPTH);
        let permits = Arc::new(Semaphore::new(1));
        let permit = permits.clone().acquire_owned().await.unwrap();
        let before = rejection_counts().idle;
        let task = tokio::spawn(async move {
            let _permit = permit;
            pump_conn_with_deadlines(
                91,
                Box::new(local),
                in_rx,
                sink,
                ConnectionDeadlines {
                    write_stall: WRITE_STALL_TIMEOUT,
                    idle: IDLE_TIMEOUT,
                    lifetime: LIFETIME_TIMEOUT,
                },
            )
            .await;
        });
        tokio::task::yield_now().await;
        tokio::time::advance(IDLE_TIMEOUT + Duration::from_secs(1)).await;
        assert_eq!(
            next_frame(&mut peer, &mut FrameDecoder::new()).await,
            Frame::Close(91)
        );
        task.await.unwrap();
        assert_eq!(permits.available_permits(), 1);
        assert_eq!(rejection_counts().idle, before + 1);
        survivor_sink.send(encode(&Frame::Open(191))).await.unwrap();
        assert_eq!(
            next_frame(&mut peer, &mut FrameDecoder::new()).await,
            Frame::Open(191)
        );
        stop_sink(sink_task).await.unwrap();
        drop(in_tx);
    }

    #[tokio::test(start_paused = true)]
    async fn lifetime_deadline_closes_only_the_stalled_connection() {
        let (local, mut client) = tokio::io::duplex(128);
        let (mut peer, sink_stream) = tokio::io::duplex(4096);
        let (sink, sink_task) = spawn_sink(sink_stream);
        let survivor_sink = sink.clone();
        let (_in_tx, in_rx) = channel(CHANNEL_DEPTH);
        let permits = Arc::new(Semaphore::new(1));
        let permit = permits.clone().acquire_owned().await.unwrap();
        let before = rejection_counts().lifetime;
        let task = tokio::spawn(async move {
            let _permit = permit;
            pump_conn_with_deadlines(
                92,
                Box::new(local),
                in_rx,
                sink,
                ConnectionDeadlines {
                    write_stall: WRITE_STALL_TIMEOUT,
                    // Advancing past the lifetime in one step would also expire a
                    // shorter idle deadline, and select! would pick either branch.
                    idle: LIFETIME_TIMEOUT * 2,
                    lifetime: LIFETIME_TIMEOUT,
                },
            )
            .await;
        });
        tokio::task::yield_now().await;
        tokio::time::advance(LIFETIME_TIMEOUT + Duration::from_secs(1)).await;
        assert_eq!(
            next_frame(&mut peer, &mut FrameDecoder::new()).await,
            Frame::Close(92)
        );
        task.await.unwrap();
        let mut local_byte = [0u8; 1];
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(1), client.read(&mut local_byte))
                .await
                .expect("lifetime deadline did not close the local write half")
                .unwrap(),
            0
        );
        assert_eq!(permits.available_permits(), 1);
        assert_eq!(rejection_counts().lifetime, before + 1);
        survivor_sink.send(encode(&Frame::Open(192))).await.unwrap();
        assert_eq!(
            next_frame(&mut peer, &mut FrameDecoder::new()).await,
            Frame::Open(192)
        );
        stop_sink(sink_task).await.unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn activity_before_idle_deadline_keeps_connection_until_lifetime() {
        let (local, mut client) = tokio::io::duplex(128);
        let (mut peer, sink_stream) = tokio::io::duplex(4096);
        let (sink, sink_task) = spawn_sink(sink_stream);
        let (in_tx, in_rx) = channel(CHANNEL_DEPTH);
        let task = tokio::spawn(pump_conn_with_deadlines(
            94,
            Box::new(local),
            in_rx,
            sink,
            ConnectionDeadlines {
                write_stall: WRITE_STALL_TIMEOUT,
                idle: IDLE_TIMEOUT,
                lifetime: LIFETIME_TIMEOUT,
            },
        ));
        tokio::task::yield_now().await;

        for byte in 0..12_u8 {
            tokio::time::advance(IDLE_TIMEOUT - Duration::from_secs(1)).await;
            in_tx.send(vec![byte]).await.unwrap();
            let mut received = [0];
            client.read_exact(&mut received).await.unwrap();
            assert_eq!(received, [byte]);
        }
        tokio::time::advance(Duration::from_secs(13)).await;
        assert_eq!(
            next_frame(&mut peer, &mut FrameDecoder::new()).await,
            Frame::Close(94)
        );
        task.await.unwrap();
        stop_sink(sink_task).await.unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn local_write_stall_closes_the_connection_and_releases_its_permit() {
        let _guard = WRITE_STALL_TEST_LOCK.lock().await;
        let (local, _client) = tokio::io::duplex(64);
        let (mut peer, sink_stream) = tokio::io::duplex(4096);
        let (sink, sink_task) = spawn_sink(sink_stream);
        let survivor_sink = sink.clone();
        let (in_tx, in_rx) = channel(CHANNEL_DEPTH);
        in_tx.send(vec![0xA5; 1024]).await.unwrap();
        let permits = Arc::new(Semaphore::new(1));
        let permit = permits.clone().acquire_owned().await.unwrap();
        let before = rejection_counts().write_stall;
        let task = tokio::spawn(async move {
            let _permit = permit;
            pump_conn_with_deadlines(
                93,
                Box::new(local),
                in_rx,
                sink,
                ConnectionDeadlines {
                    write_stall: WRITE_STALL_TIMEOUT,
                    idle: IDLE_TIMEOUT,
                    lifetime: LIFETIME_TIMEOUT,
                },
            )
            .await;
        });
        tokio::task::yield_now().await;
        tokio::time::advance(WRITE_STALL_TIMEOUT + Duration::from_secs(1)).await;
        assert_eq!(
            next_frame(&mut peer, &mut FrameDecoder::new()).await,
            Frame::Close(93)
        );
        task.await.unwrap();
        assert_eq!(permits.available_permits(), 1);
        assert!(rejection_counts().write_stall > before);
        survivor_sink.send(encode(&Frame::Open(193))).await.unwrap();
        assert_eq!(
            next_frame(&mut peer, &mut FrameDecoder::new()).await,
            Frame::Open(193)
        );
        stop_sink(sink_task).await.unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn ordinary_local_disconnect_does_not_count_as_write_stall() {
        let _guard = WRITE_STALL_TEST_LOCK.lock().await;
        let (local, client) = tokio::io::duplex(128);
        let (sink, mut sink_rx) = channel(4);
        let (in_tx, in_rx) = channel(4);
        in_tx.send(vec![0xA5]).await.unwrap();
        let before = rejection_counts().write_stall;
        let task = tokio::spawn(pump_conn_with_deadlines(
            95,
            Box::new(local),
            in_rx,
            sink,
            ConnectionDeadlines {
                write_stall: WRITE_STALL_TIMEOUT,
                idle: IDLE_TIMEOUT,
                lifetime: LIFETIME_TIMEOUT,
            },
        ));
        drop(client);
        drop(in_tx);

        task.await.unwrap();
        assert!(sink_rx.recv().await.is_some());
        assert_eq!(rejection_counts().write_stall, before);
    }

    #[tokio::test(start_paused = true)]
    async fn shared_sink_write_stall_ends_its_tunnel_stream() {
        let _guard = WRITE_STALL_TEST_LOCK.lock().await;
        let (near, mut far) = tokio::io::duplex(64);
        let (sink, sink_task) = spawn_sink_with_timeout(near, WRITE_STALL_TIMEOUT);
        let before = rejection_counts().write_stall;
        sink.send(vec![0xAB; 1024]).await.unwrap();
        let mut sink_task = sink_task;
        let supervisor = tokio::spawn(async move {
            supervise_sink(std::future::pending::<io::Result<()>>(), &mut sink_task).await
        });
        tokio::task::yield_now().await;
        tokio::time::advance(WRITE_STALL_TIMEOUT + Duration::from_secs(1)).await;
        let (result, sink_failed) = supervisor.await.unwrap();
        assert!(sink_failed);
        assert!(matches!(result, Err(error) if error.kind() == io::ErrorKind::BrokenPipe));
        assert!(rejection_counts().write_stall > before);
        let mut buffered = Vec::new();
        tokio::time::timeout(Duration::from_secs(1), far.read_to_end(&mut buffered))
            .await
            .unwrap()
            .unwrap();
        assert!(buffered.len() <= 64);
    }

    #[test]
    fn rejection_counters_track_each_reason() {
        let before = rejection_counts();
        let local = AtomicUsize::new(0);
        report_rejection(&local, "test", RejectionReason::DuplicateId);
        report_rejection(&local, "test", RejectionReason::DialTimeout);
        let after = rejection_counts();
        assert_eq!(after.duplicate_id, before.duplicate_id + 1);
        assert_eq!(after.dial_timeout, before.dial_timeout + 1);
    }

    #[test]
    fn rejection_warning_is_limited_to_one_per_reason_per_minute() {
        let start = Instant::now();
        let mut last = HashMap::new();
        assert!(rejection_warning_due(
            &mut last,
            RejectionReason::Idle,
            start
        ));
        assert!(!rejection_warning_due(
            &mut last,
            RejectionReason::Idle,
            start + Duration::from_secs(59)
        ));
        assert!(rejection_warning_due(
            &mut last,
            RejectionReason::Idle,
            start + REJECTION_LOG_INTERVAL
        ));
        assert!(rejection_warning_due(
            &mut last,
            RejectionReason::Lifetime,
            start + Duration::from_secs(1)
        ));
    }

    #[test]
    fn accept_errors_retry_with_capped_exponential_backoff() {
        assert!(transient_accept_error(
            &crate::plugin::platform::accept_resource_error_for_test()
        ));
        assert!(transient_accept_error(&io::Error::new(
            io::ErrorKind::ConnectionAborted,
            "accept aborted"
        )));
        assert!(transient_accept_error(&io::Error::new(
            io::ErrorKind::OutOfMemory,
            "accept out of memory"
        )));
        assert!(transient_accept_error(&io::Error::new(
            io::ErrorKind::Interrupted,
            "accept interrupted"
        )));
        assert!(transient_accept_error(&io::Error::new(
            io::ErrorKind::WouldBlock,
            "accept would block"
        )));
        assert!(!transient_accept_error(&io::Error::new(
            io::ErrorKind::PermissionDenied,
            "fatal listener failure"
        )));
        assert_eq!(
            next_accept_backoff(ACCEPT_BACKOFF_BASE),
            Duration::from_millis(20)
        );
        assert_eq!(next_accept_backoff(ACCEPT_BACKOFF_MAX), ACCEPT_BACKOFF_MAX);
    }
}
