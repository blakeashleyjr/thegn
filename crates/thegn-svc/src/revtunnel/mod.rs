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
use std::sync::atomic::{AtomicU32, AtomicU64, AtomicUsize, Ordering};
use std::time::Duration;

use thegn_core::revtunnel::{
    Frame, FrameDecoder, MAX_RESYNC_SKIP, SYNC_MAGIC, SyncOutcome, encode, encode_data_chunked,
};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::sync::mpsc::{Sender, channel};
use tokio::sync::{Mutex, Semaphore};
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

static GLOBAL_HOST_PERMITS: LazyLock<Arc<Semaphore>> =
    LazyLock::new(|| Arc::new(Semaphore::new(MAX_GLOBAL_TUNNEL_CONNS)));
static REJECTIONS_TOTAL: AtomicU64 = AtomicU64::new(0);

type ConnectionTasks = Arc<Mutex<JoinSet<()>>>;

fn report_rejection(rejections: &AtomicUsize, endpoint: &'static str, reason: &'static str) {
    REJECTIONS_TOTAL.fetch_add(1, Ordering::Relaxed);
    let count = rejections.fetch_add(1, Ordering::Relaxed) + 1;
    if count == 1 {
        tracing::warn!(
            target: "thegn::revtunnel",
            endpoint,
            reason,
            rejections = count,
            "reverse-tunnel connection rejected"
        );
    }
}

/// Number of reverse-tunnel admission and dial-timeout rejections since process start.
pub fn rejection_total() -> u64 {
    REJECTIONS_TOTAL.load(Ordering::Relaxed)
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

async fn stop_sink(task: JoinHandle<()>) -> io::Result<()> {
    // Once the read side has hit EOF, queued frames (including final Close
    // frames) are intentionally discarded. Waiting for the sink to drain here
    // could hang forever if the peer stopped reading at tunnel end.
    task.abort();
    match task.await {
        Ok(()) => Ok(()),
        Err(error) if error.is_cancelled() => Ok(()),
        Err(error) => Err(io::Error::other(format!(
            "reverse-tunnel sink task: {error}"
        ))),
    }
}

/// Any bidirectional async stream usable as a tunneled connection (a real
/// `TcpStream`, or an in-memory `DuplexStream` in tests).
pub trait IoStream: AsyncRead + AsyncWrite + Unpin + Send {}
impl<T: AsyncRead + AsyncWrite + Unpin + Send> IoStream for T {}

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
    mut wr: W,
) -> (Sender<Vec<u8>>, JoinHandle<()>) {
    let (tx, mut rx) = channel::<Vec<u8>>(CHANNEL_DEPTH);
    let task = tokio::spawn(async move {
        while let Some(bytes) = rx.recv().await {
            if wr.write_all(&bytes).await.is_err() {
                break;
            }
            let _ = wr.flush().await; // best-effort: sink may be gone
        }
    });
    (tx, task)
}

/// Pump one logical connection's local stream ⇄ the mux:
/// - local reads → `Data(id)` frames on the sink, then `Close(id)` at EOF;
/// - `inbound` payloads (peer `Data(id)`) → local writes.
async fn pump_conn(
    id: u32,
    local: Box<dyn IoStream>,
    mut inbound: tokio::sync::mpsc::Receiver<Vec<u8>>,
    sink: Sender<Vec<u8>>,
) {
    let (mut rd, mut wr) = tokio::io::split(local);
    // peer → local. This future is owned by the pump, so aborting the pump also
    // drops the local writer and cannot leave a detached task behind.
    let inbound_to_local = async move {
        while let Some(d) = inbound.recv().await {
            if wr.write_all(&d).await.is_err() {
                break;
            }
        }
        let _ = wr.shutdown().await; // best-effort: peer may already be gone
    };
    // local → peer (Data frames), Close(id) at EOF/err (a half-close, not a full
    // teardown — the writer above keeps delivering the reverse direction).
    // `send(...).await` blocks when the bounded sink is full, applying
    // backpressure to this local read so a fast source can't balloon the queue.
    let mut buf = vec![0u8; 16 * 1024];
    let local_to_peer = async {
        loop {
            match rd.read(&mut buf).await {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    if sink.send(encode_data_chunked(id, &buf[..n])).await.is_err() {
                        break;
                    }
                }
            }
        }
        let _ = sink.send(encode(&Frame::Close(id))).await; // best-effort: remote may be gone
    };
    tokio::join!(local_to_peer, inbound_to_local);
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
    let permits = Arc::new(Semaphore::new(max_connections));
    let rejections = Arc::new(AtomicUsize::new(0));
    let tasks: ConnectionTasks = Arc::new(Mutex::new(JoinSet::new()));
    let conns2 = conns.clone();
    let permits2 = permits.clone();
    let rejections2 = rejections.clone();
    let tasks2 = tasks.clone();
    let result = read_loop(
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
                        report_rejection(&rejections, "host", "global_connection_limit");
                        if let Err(error) = sink.send(encode(&Frame::Close(id))).await {
                            tracing::debug!(target: "thegn::revtunnel", %error, "close reply dropped");
                        }
                        return;
                    }
                };
                let permit = match permits.try_acquire_owned() {
                    Ok(permit) => permit,
                    Err(_) => {
                        report_rejection(&rejections, "host", "connection_limit");
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
                    report_rejection(&rejections, "host", "duplicate_id");
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
                            report_rejection(&rejections, "host", "dial_timeout");
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
    )
    .await;
    abort_tasks(&tasks).await;
    result.and(stop_sink(sink_task).await)
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
    listener: TcpListener,
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
        loop {
            let Ok((sock, _)) = listener.accept().await else {
                break;
            };
            reap_finished(&acc_tasks).await;
            let permit = match acc_permits.clone().try_acquire_owned() {
                Ok(permit) => permit,
                Err(_) => {
                    report_rejection(&acc_rejections, "sandbox", "connection_limit");
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
                pump_conn(id, Box::new(sock), in_rx, sink).await;
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
    let res = read_loop(rd, conns, |_id| async {}, None).await;
    abort_tasks(&tasks).await;
    res.and(stop_sink(sink_task).await)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

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

    #[tokio::test]
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
            std::time::Duration::from_millis(50),
        ));
        send_opens(&mut peer, &[1]).await;
        let mut data = Vec::new();
        for _ in 0..(CHANNEL_DEPTH + 4) {
            data.extend(encode(&Frame::Data(1, vec![0; 8])));
        }
        tokio::time::timeout(std::time::Duration::from_secs(2), peer.write_all(&data))
            .await
            .expect("timed out filling the stalled route's inbound queue")
            .unwrap();

        let mut decoder = FrameDecoder::new();
        assert_eq!(next_frame(&mut peer, &mut decoder).await, Frame::Close(1));
        send_open(&mut peer, 2).await;
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            while calls.load(Ordering::Relaxed) < 2 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("timed-out dial did not release the permit and read loop");
        drop(peer);
        tokio::time::timeout(std::time::Duration::from_secs(2), task)
            .await
            .expect("host did not stop after peer disconnect")
            .unwrap()
            .unwrap();
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
}
