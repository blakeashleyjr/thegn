//! Reverse host→sandbox tunnel: the async transport that pumps the pure mux
//! protocol ([`superzej_core::revtunnel`]) over a single bidirectional byte
//! stream (in production: the provider WSS exec / resident-bridge channel).
//!
//! Two symmetric endpoints share the stream:
//! - [`run_sandbox`] runs INSIDE the sandbox: it `accept`s loopback connections
//!   (e.g. an agent dialing `127.0.0.1:8383` for `szproxy`), opens a muxed
//!   logical connection per accept, and pumps bytes both ways.
//! - [`run_host`] runs on the HOST: for each `Open` it [`Dialer`]s the real host
//!   target (the local `szproxy`, a host `localhost` DB/API, a host-bound MCP
//!   server) and pumps bytes back.
//!
//! Both are generic over the stream, and the host side over the [`Dialer`], so the
//! whole thing is exercised end-to-end **with in-memory mocks** (a
//! `tokio::io::duplex` pair as the channel + an echo `Dialer`) — no real network,
//! no sprite. Mirrors the `[forward]` proxy pattern, but reversed.

use std::collections::HashMap;
use std::io;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};

use superzej_core::revtunnel::{
    Frame, FrameDecoder, MAX_RESYNC_SKIP, SYNC_MAGIC, SyncOutcome, encode, encode_data_chunked,
};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::sync::Mutex;
use tokio::sync::mpsc::{UnboundedSender, unbounded_channel};

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
                    let _ = control.send(ExecControl::Close).await;
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
        let _ = far_wr.shutdown().await;
    });
    near
}

/// Opens the real host-side target for each tunneled connection. Injectable so
/// tests substitute an in-memory mock for a TCP dial.
pub trait Dialer: Clone + Send + Sync + 'static {
    fn dial(&self) -> impl std::future::Future<Output = io::Result<Box<dyn IoStream>>> + Send;
}

/// Dials a fixed host `addr` (the production host endpoint, e.g. `127.0.0.1:8383`
/// for `szproxy`).
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

fn decode_err(e: superzej_core::revtunnel::DecodeError) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, format!("tunnel decode: {e:?}"))
}

/// Spawn the writer task: serialize all outbound frames (from any connection
/// task) onto the shared stream write half. Returns the frame sink.
fn spawn_sink<W: AsyncWrite + Unpin + Send + 'static>(mut wr: W) -> UnboundedSender<Vec<u8>> {
    let (tx, mut rx) = unbounded_channel::<Vec<u8>>();
    tokio::spawn(async move {
        while let Some(bytes) = rx.recv().await {
            if wr.write_all(&bytes).await.is_err() {
                break;
            }
            let _ = wr.flush().await;
        }
    });
    tx
}

/// Pump one logical connection's local stream ⇄ the mux:
/// - local reads → `Data(id)` frames on the sink, then `Close(id)` at EOF;
/// - `inbound` payloads (peer `Data(id)`) → local writes.
async fn pump_conn(
    id: u32,
    local: Box<dyn IoStream>,
    mut inbound: tokio::sync::mpsc::UnboundedReceiver<Vec<u8>>,
    sink: UnboundedSender<Vec<u8>>,
) {
    let (mut rd, mut wr) = tokio::io::split(local);
    // peer → local. Runs INDEPENDENTLY of the read direction: a half-close on the
    // local read side must NOT tear down this return path (the peer may still be
    // sending — e.g. a server's response after the client half-closed). It ends
    // when the peer's `Close(id)` drops the inbound sender.
    tokio::spawn(async move {
        while let Some(d) = inbound.recv().await {
            if wr.write_all(&d).await.is_err() {
                break;
            }
        }
        let _ = wr.shutdown().await;
    });
    // local → peer (Data frames), Close(id) at EOF/err (a half-close, not a full
    // teardown — the writer above keeps delivering the reverse direction).
    let mut buf = vec![0u8; 16 * 1024];
    loop {
        match rd.read(&mut buf).await {
            Ok(0) | Err(_) => break,
            Ok(n) => {
                if sink.send(encode_data_chunked(id, &buf[..n])).is_err() {
                    break;
                }
            }
        }
    }
    let _ = sink.send(encode(&Frame::Close(id)));
}

/// Shared per-connection routing table: id → inbound payload sender.
type Conns = Arc<Mutex<HashMap<u32, UnboundedSender<Vec<u8>>>>>;

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
/// [`DecodeError`] triggers a bounded resync (drop garbage until the framing
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
                            target: "szhost::revtunnel",
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
                        if let Some(tx) = conns.lock().await.get(&id) {
                            let _ = tx.send(d);
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
                            target: "szhost::revtunnel",
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
    let (rd, wr) = tokio::io::split(stream);
    let sink = spawn_sink(wr);
    let conns: Conns = Arc::new(Mutex::new(HashMap::new()));
    let conns2 = conns.clone();
    read_loop(
        rd,
        conns,
        move |id| {
            let dialer = dialer.clone();
            let sink = sink.clone();
            let conns = conns2.clone();
            async move {
                let (in_tx, in_rx) = unbounded_channel::<Vec<u8>>();
                conns.lock().await.insert(id, in_tx);
                tokio::spawn(async move {
                    match dialer.dial().await {
                        Ok(local) => pump_conn(id, local, in_rx, sink).await,
                        Err(_) => {
                            let _ = sink.send(encode(&Frame::Close(id)));
                        }
                    }
                });
            }
        },
        Some(SYNC_MAGIC),
    )
    .await
}

/// SANDBOX endpoint: accept loopback connections on `listener`, open a muxed
/// connection per accept, and pump. `listener` is pre-bound so the caller knows
/// the address. Runs until the stream closes.
pub async fn run_sandbox<S>(stream: S, listener: TcpListener) -> io::Result<()>
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    let (rd, wr) = tokio::io::split(stream);
    let sink = spawn_sink(wr);
    // Emit the sync marker as the FIRST bytes on stdout, before any frame, so the
    // host can skip whatever one-time preamble the exec transport prepended (a
    // runtime banner/MOTD/shell echo) and lock onto the framing. See `SYNC_MAGIC`.
    let _ = sink.send(SYNC_MAGIC.to_vec());
    let conns: Conns = Arc::new(Mutex::new(HashMap::new()));
    let next_id = Arc::new(AtomicU32::new(1));

    // Accept loop: each new local connection becomes a muxed Open.
    let acc_conns = conns.clone();
    let acc_sink = sink.clone();
    let acc_ids = next_id.clone();
    let accept = tokio::spawn(async move {
        loop {
            let Ok((sock, _)) = listener.accept().await else {
                break;
            };
            let id = acc_ids.fetch_add(1, Ordering::Relaxed);
            let (in_tx, in_rx) = unbounded_channel::<Vec<u8>>();
            acc_conns.lock().await.insert(id, in_tx);
            if acc_sink.send(encode(&Frame::Open(id))).is_err() {
                break;
            }
            let sink = acc_sink.clone();
            tokio::spawn(async move {
                pump_conn(id, Box::new(sock), in_rx, sink).await;
            });
        }
    });

    // The sandbox never receives Open; Data/Close route to accepted conns. The
    // host→sandbox direction is superzej's own clean frame stream (no preamble),
    // so no sync marker is expected here.
    let res = read_loop(rd, conns, |_id| async {}, None).await;
    accept.abort();
    res
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    /// A `Dialer` whose target echoes everything written to it (stands in for a
    /// real host service like `szproxy`).
    #[derive(Clone)]
    struct EchoDialer;
    impl Dialer for EchoDialer {
        async fn dial(&self) -> io::Result<Box<dyn IoStream>> {
            let (near, far) = tokio::io::duplex(64 * 1024);
            // Echo task on the far end: copy its input back to its output.
            tokio::spawn(async move {
                let (mut r, mut w) = tokio::io::split(far);
                let _ = tokio::io::copy(&mut r, &mut w).await;
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
            let _ = tokio::io::copy(&mut sbx_out_b_rd, &mut s2h_a_wr).await;
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
}
