//! Direct-only Iroh transport for the existing scoped control HTTP API.
//!
//! This is deliberately independent from `iroh_reach` (the sandbox ExecReq
//! protocol). The accepted peer identity is transport-authenticated metadata,
//! but it is never implicit admin: HTTP handlers still require the stored
//! bearer token on every protected operation.

use anyhow::{Context, Result};
use std::io;
use std::pin::Pin;
use std::task::{Context as TaskContext, Poll};
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::sync::mpsc;
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

use super::http::{ControlState, router};
use crate::ipc::{IpcConnectInfo, PeerIdentity};

pub const CONTROL_ALPN: &[u8] = b"thegn/control/v1";
const MAX_DIRECT_CONNECTIONS: usize = 32;
const HANDSHAKE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

/// Build an endpoint which cannot use relays or address lookup.
pub async fn bind_direct(secret: [u8; 32]) -> Result<iroh::Endpoint> {
    use iroh::endpoint::RelayMode;
    use iroh::endpoint::presets::Minimal;

    iroh::Endpoint::builder(Minimal)
        .alpns(vec![CONTROL_ALPN.to_vec()])
        .secret_key(iroh::SecretKey::from_bytes(&secret))
        .relay_mode(RelayMode::Disabled)
        .clear_address_lookup()
        .bind()
        .await
        .context("bind direct control Iroh endpoint")
}

/// Return only concrete IP socket addresses, with no relay or lookup path.
pub fn direct_address(endpoint: &iroh::Endpoint) -> Result<iroh::EndpointAddr> {
    use iroh::TransportAddr;

    let addr = endpoint.addr();
    let direct: Vec<_> = addr
        .addrs
        .into_iter()
        .filter_map(|addr| match addr {
            TransportAddr::Ip(socket) if !socket.ip().is_unspecified() && socket.port() != 0 => {
                Some(TransportAddr::Ip(socket))
            }
            _ => None,
        })
        .collect();
    anyhow::ensure!(
        !direct.is_empty(),
        "Iroh did not expose a concrete direct IP address; check the host network interfaces and retry"
    );
    Ok(iroh::EndpointAddr::from_parts(addr.id, direct))
}

/// Start a control listener on an already-bound Iroh endpoint. State is
/// defensively forced to token-only even if the caller passed a local state.
pub async fn serve(endpoint: iroh::Endpoint, mut state: ControlState) -> Result<()> {
    state.local_admin = false;
    state.daemon_euid = None;
    let listener = IrohControlListener::new(endpoint);
    axum::serve(
        listener,
        router(state).into_make_service_with_connect_info::<IpcConnectInfo>(),
    )
    .await
    .context("serve direct Iroh control listener")
}

struct IrohControlListener {
    rx: mpsc::Receiver<(QuicControlIo, IpcConnectInfo)>,
    accept_task: tokio::task::JoinHandle<()>,
}

impl IrohControlListener {
    fn new(endpoint: iroh::Endpoint) -> Self {
        let (tx, rx) = mpsc::channel(32);
        let accept_task = tokio::spawn(async move {
            let permits = std::sync::Arc::new(Semaphore::new(MAX_DIRECT_CONNECTIONS));
            let mut pending = tokio::task::JoinSet::new();
            loop {
                tokio::select! {
                    incoming = endpoint.accept() => {
                        let Some(incoming) = incoming else { break };
                        let permit = match permits.clone().try_acquire_owned() {
                            Ok(permit) => permit,
                            Err(_) => {
                                tracing::debug!(target: "thegn::control", "Iroh control connection limit reached");
                                continue;
                            }
                        };
                        let tx = tx.clone();
                        pending.spawn(async move {
                            let connection = match tokio::time::timeout(HANDSHAKE_TIMEOUT, incoming).await {
                                Ok(Ok(connection)) => connection,
                                Ok(Err(error)) => {
                                    tracing::debug!(target: "thegn::control", %error, "Iroh control handshake failed");
                                    return;
                                }
                                Err(_) => {
                                    tracing::debug!(target: "thegn::control", "Iroh control handshake timed out");
                                    return;
                                }
                            };
                            let peer = connection.remote_id().to_string();
                            let streams = tokio::time::timeout(HANDSHAKE_TIMEOUT, connection.accept_bi()).await;
                            let (send, recv) = match streams {
                                Ok(Ok(streams)) => streams,
                                Ok(Err(error)) => {
                                    tracing::debug!(target: "thegn::control", %error, "Iroh control stream rejected");
                                    return;
                                }
                                Err(_) => {
                                    tracing::debug!(target: "thegn::control", "Iroh first control stream timed out");
                                    return;
                                }
                            };
                            let io = QuicControlIo { send, recv, _connection: connection, _permit: permit };
                            let info = IpcConnectInfo {
                                endpoint: format!("iroh:{peer}"),
                                peer: PeerIdentity::IrohEndpointId(peer),
                            };
                            let _ = tx.send((io, info)).await;
                        });
                    }
                    Some(_) = pending.join_next(), if !pending.is_empty() => {}
                }
            }
        });
        Self { rx, accept_task }
    }
}

impl Drop for IrohControlListener {
    fn drop(&mut self) {
        self.accept_task.abort();
    }
}

/// Direct QUIC bidirectional stream exposed as one Tokio byte stream. Keeping
/// the connection and semaphore permit in this object ties both resource
/// lifetime and cancellation to Axum's HTTP/WebSocket connection lifetime.
struct QuicControlIo {
    send: iroh::endpoint::SendStream,
    recv: iroh::endpoint::RecvStream,
    _connection: iroh::endpoint::Connection,
    _permit: OwnedSemaphorePermit,
}

impl Drop for QuicControlIo {
    fn drop(&mut self) {
        self._connection
            .close(0u32.into(), b"control stream closed");
    }
}

impl AsyncRead for QuicControlIo {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut TaskContext<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        Pin::new(&mut self.recv).poll_read(cx, buf)
    }
}

impl AsyncWrite for QuicControlIo {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut TaskContext<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.send).poll_write(cx, buf)
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut TaskContext<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.send).poll_flush(cx)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut TaskContext<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.send).poll_shutdown(cx)
    }
}

impl axum::serve::Listener for IrohControlListener {
    type Io = tokio::io::DuplexStream;
    type Addr = IpcConnectInfo;

    async fn accept(&mut self) -> (Self::Io, Self::Addr) {
        loop {
            if let Some(stream) = self.rx.recv().await {
                return stream;
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
    }

    fn local_addr(&self) -> io::Result<Self::Addr> {
        Ok(IpcConnectInfo {
            endpoint: "iroh:listener".into(),
            peer: PeerIdentity::Unavailable("listener address has no peer".into()),
        })
    }
}

impl axum::extract::connect_info::Connected<axum::serve::IncomingStream<'_, IrohControlListener>>
    for IpcConnectInfo
{
    fn connect_info(stream: axum::serve::IncomingStream<'_, IrohControlListener>) -> Self {
        stream.remote_addr().clone()
    }
}
