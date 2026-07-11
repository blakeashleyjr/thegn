//! The control-API client — what `thegn` CLI verbs and the compositor's
//! daemon-backed panes speak.
//!
//! Talks the HTTP surface ([`super::http`]) over a unix socket (local; peer
//! credentials are the auth) or TCP (serve mode; bearer token required). One
//! hyper connection per request — CLI verbs are one-shot and the daemon is
//! local, so a pool would buy nothing. The warm-attach stream rides a
//! WebSocket (`tokio-tungstenite` over the same stream types).

use anyhow::{Context, Result, anyhow};
use base64::Engine as _;
use futures_util::{SinkExt, StreamExt};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use std::path::PathBuf;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::sync::mpsc as tokio_mpsc;

use thegn_core::control_wire::{EventDecoder, EventFrame};
use thegn_core::store::{ControlStore, DaemonRow};

use super::{OpenSpec, SessionInfo};

/// Heartbeats older than this mark a daemon row stale for discovery.
pub const DAEMON_HEARTBEAT_TTL_MS: i64 = 60_000;

/// Where the daemon is and how to authenticate to it.
#[derive(Debug, Clone)]
pub enum ControlAddr {
    /// Local unix socket (implicit same-uid auth).
    Unix(PathBuf),
    /// Remote serve-mode listener; every request carries the bearer token.
    Tcp { addr: String, token: String },
}

/// Discover a live local daemon for `scope` (the canonical state dir) from the
/// registry: freshest heartbeat wins. Returns its unix-socket address; `None`
/// means "no daemon running" (callers degrade gracefully).
pub fn discover(store: &dyn ControlStore, scope: &str, now_ms: i64) -> Option<ControlAddr> {
    let mut live = store
        .live_daemons(scope, now_ms, DAEMON_HEARTBEAT_TTL_MS)
        .ok()?;
    live.sort_by_key(|d: &DaemonRow| d.heartbeat_at);
    live.pop()
        .map(|d| ControlAddr::Unix(PathBuf::from(d.endpoint)))
}

pub struct ControlClient {
    addr: ControlAddr,
}

/// Control messages for an attached session stream.
pub enum AttachControl {
    Input(Vec<u8>),
    Resize { rows: u16, cols: u16 },
    Close,
}

/// A live warm-attach: decoded frames in (snapshot first), control out.
pub struct AttachStream {
    pub frames: tokio_mpsc::Receiver<EventFrame>,
    pub control: tokio_mpsc::Sender<AttachControl>,
}

impl ControlClient {
    pub fn new(addr: ControlAddr) -> Self {
        Self { addr }
    }

    pub fn addr(&self) -> &ControlAddr {
        &self.addr
    }

    fn token(&self) -> Option<&str> {
        match &self.addr {
            ControlAddr::Unix(_) => None,
            ControlAddr::Tcp { token, .. } => Some(token),
        }
    }

    /// One HTTP request → parsed JSON body. Non-2xx returns the error message
    /// from the server's `{"error": …}` envelope.
    async fn request(&self, method: &str, path: &str, body: Option<Value>) -> Result<Value> {
        let (status, value) = match &self.addr {
            ControlAddr::Unix(sock) => {
                let stream = tokio::net::UnixStream::connect(sock)
                    .await
                    .with_context(|| format!("connect control socket {}", sock.display()))?;
                send_request(stream, method, path, self.token(), body).await?
            }
            ControlAddr::Tcp { addr, .. } => {
                let stream = tokio::net::TcpStream::connect(addr)
                    .await
                    .with_context(|| format!("connect control addr {addr}"))?;
                send_request(stream, method, path, self.token(), body).await?
            }
        };
        if (200..300).contains(&status) {
            Ok(value)
        } else {
            let msg = value
                .get("error")
                .and_then(Value::as_str)
                .unwrap_or("control request failed");
            Err(anyhow!("{msg} (http {status})"))
        }
    }

    pub async fn health(&self) -> Result<()> {
        self.request("GET", "/health", None).await.map(|_| ())
    }

    pub async fn me(&self) -> Result<Value> {
        self.request("GET", "/v1/me", None).await
    }

    pub async fn sessions(&self) -> Result<Vec<SessionInfo>> {
        let v = self.request("GET", "/v1/sessions", None).await?;
        Ok(serde_json::from_value(
            v.get("sessions").cloned().unwrap_or(Value::Array(vec![])),
        )?)
    }

    pub async fn open(&self, spec: &OpenSpec) -> Result<SessionInfo> {
        let v = self
            .request("POST", "/v1/sessions", Some(serde_json::to_value(spec)?))
            .await?;
        Ok(serde_json::from_value(v)?)
    }

    /// One-shot snapshot: `(seq, rows, cols, ansi_bytes)`.
    pub async fn snapshot(&self, session: &str) -> Result<(u64, u16, u16, Vec<u8>)> {
        let v = self
            .request("GET", &format!("/v1/sessions/{session}/snapshot"), None)
            .await?;
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(v.get("ansi_b64").and_then(Value::as_str).unwrap_or(""))
            .context("snapshot base64")?;
        Ok((
            v.get("seq").and_then(Value::as_u64).unwrap_or(0),
            v.get("rows").and_then(Value::as_u64).unwrap_or(0) as u16,
            v.get("cols").and_then(Value::as_u64).unwrap_or(0) as u16,
            bytes,
        ))
    }

    pub async fn send_input(&self, session: &str, bytes: &[u8], enter: bool) -> Result<()> {
        let b64 = base64::engine::general_purpose::STANDARD.encode(bytes);
        self.request(
            "POST",
            &format!("/v1/sessions/{session}/input"),
            Some(json!({ "b64": b64, "enter": enter })),
        )
        .await
        .map(|_| ())
    }

    pub async fn resize(&self, session: &str, rows: u16, cols: u16) -> Result<()> {
        self.request(
            "POST",
            &format!("/v1/sessions/{session}/resize"),
            Some(json!({ "rows": rows, "cols": cols })),
        )
        .await
        .map(|_| ())
    }

    pub async fn detach(&self, session: &str, client_id: &str) -> Result<()> {
        self.request(
            "POST",
            &format!("/v1/sessions/{session}/detach"),
            Some(json!({ "client_id": client_id })),
        )
        .await
        .map(|_| ())
    }

    pub async fn kill(&self, session: &str) -> Result<()> {
        self.request("DELETE", &format!("/v1/sessions/{session}"), None)
            .await
            .map(|_| ())
    }

    pub async fn leases(&self) -> Result<Value> {
        self.request("GET", "/v1/leases", None).await
    }

    pub async fn open_worktree(&self, repo: &str, branch: Option<&str>) -> Result<()> {
        self.request(
            "POST",
            "/v1/worktrees/open",
            Some(json!({ "repo": repo, "branch": branch })),
        )
        .await
        .map(|_| ())
    }

    /// The reserved drive-browser verb (v1 answers 501 Unimplemented).
    pub async fn send_browse(&self, session: Option<&str>, url: &str) -> Result<()> {
        self.request(
            "POST",
            "/v1/browser",
            Some(json!({
                "session": session,
                "action": { "navigate": { "url": url } },
            })),
        )
        .await
        .map(|_| ())
    }

    pub async fn pair(&self, code: &str, label: &str) -> Result<Value> {
        self.request(
            "POST",
            "/v1/pair",
            Some(json!({ "code": code, "label": label })),
        )
        .await
    }

    /// Warm-attach over WebSocket. The first frames on `frames` are `Hello`
    /// then the `PaneSnapshot`; live deltas follow.
    pub async fn attach(
        &self,
        session: &str,
        client_id: &str,
        rows: u16,
        cols: u16,
        observer: bool,
    ) -> Result<AttachStream> {
        let path = format!(
            "/v1/sessions/{session}/attach?client_id={client_id}&rows={rows}&cols={cols}&observer={observer}"
        );
        let (host, token) = match &self.addr {
            ControlAddr::Unix(_) => ("localhost".to_string(), None),
            ControlAddr::Tcp { addr, token } => (addr.clone(), Some(token.clone())),
        };
        let mut req = tokio_tungstenite::tungstenite::http::Request::builder()
            .method("GET")
            .uri(format!("ws://{host}{path}"))
            .header("Host", &host)
            .header("Connection", "Upgrade")
            .header("Upgrade", "websocket")
            .header("Sec-WebSocket-Version", "13")
            .header(
                "Sec-WebSocket-Key",
                tokio_tungstenite::tungstenite::handshake::client::generate_key(),
            );
        if let Some(t) = token {
            req = req.header("Authorization", format!("Bearer {t}"));
        }
        let req = req.body(()).context("build attach request")?;

        let ws = match &self.addr {
            ControlAddr::Unix(sock) => {
                let stream = tokio::net::UnixStream::connect(sock)
                    .await
                    .with_context(|| format!("connect control socket {}", sock.display()))?;
                let (ws, _) = tokio_tungstenite::client_async(req, stream)
                    .await
                    .context("attach websocket handshake")?;
                WsEither::Unix(ws)
            }
            ControlAddr::Tcp { addr, .. } => {
                let stream = tokio::net::TcpStream::connect(addr)
                    .await
                    .with_context(|| format!("connect control addr {addr}"))?;
                let (ws, _) = tokio_tungstenite::client_async(req, stream)
                    .await
                    .context("attach websocket handshake")?;
                WsEither::Tcp(ws)
            }
        };

        let (frame_tx, frame_rx) = tokio_mpsc::channel::<EventFrame>(256);
        let (ctrl_tx, ctrl_rx) = tokio_mpsc::channel::<AttachControl>(64);
        tokio::spawn(pump_attach_ws(ws, frame_tx, ctrl_rx));
        Ok(AttachStream {
            frames: frame_rx,
            control: ctrl_tx,
        })
    }
}

type Ws<S> = tokio_tungstenite::WebSocketStream<S>;

/// The two attach transports, unified for the pump.
enum WsEither {
    Unix(Ws<tokio::net::UnixStream>),
    Tcp(Ws<tokio::net::TcpStream>),
}

async fn pump_attach_ws(
    ws: WsEither,
    frames: tokio_mpsc::Sender<EventFrame>,
    ctrl: tokio_mpsc::Receiver<AttachControl>,
) {
    match ws {
        WsEither::Unix(ws) => pump_attach_inner(ws, frames, ctrl).await,
        WsEither::Tcp(ws) => pump_attach_inner(ws, frames, ctrl).await,
    }
}

async fn pump_attach_inner<S>(
    mut ws: Ws<S>,
    frames: tokio_mpsc::Sender<EventFrame>,
    mut ctrl: tokio_mpsc::Receiver<AttachControl>,
) where
    S: AsyncRead + AsyncWrite + Unpin,
{
    use tokio_tungstenite::tungstenite::Message;
    let mut decoder = EventDecoder::new();
    loop {
        tokio::select! {
            msg = ws.next() => match msg {
                Some(Ok(Message::Binary(bytes))) => {
                    decoder.push(&bytes);
                    loop {
                        match decoder.next_frame() {
                            Ok(Some(frame)) => {
                                if frames.send(frame).await.is_err() {
                                    return; // consumer gone
                                }
                            }
                            Ok(None) => break,
                            Err(e) => {
                                tracing::warn!(target: "thegn::control", "attach stream decode error: {e}");
                                return;
                            }
                        }
                    }
                }
                Some(Ok(Message::Close(_))) | None => return,
                Some(Ok(_)) => {} // text/ping/pong
                Some(Err(e)) => {
                    tracing::debug!(target: "thegn::control", "attach websocket error: {e}");
                    return;
                }
            },
            c = ctrl.recv() => match c {
                Some(AttachControl::Input(bytes)) => {
                    if ws.send(Message::Binary(bytes)).await.is_err() {
                        return;
                    }
                }
                Some(AttachControl::Resize { rows, cols }) => {
                    let text = json!({ "type": "resize", "rows": rows, "cols": cols });
                    if ws.send(Message::Text(text.to_string())).await.is_err() {
                        return;
                    }
                }
                Some(AttachControl::Close) | None => {
                    let _ = ws.send(Message::Close(None)).await;
                    return;
                }
            },
        }
    }
}

/// Send one HTTP/1.1 request over `stream` and collect the JSON body.
async fn send_request<S>(
    stream: S,
    method: &str,
    path: &str,
    token: Option<&str>,
    body: Option<Value>,
) -> Result<(u16, Value)>
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    let io = hyper_util::rt::TokioIo::new(stream);
    let (mut sender, conn) = hyper::client::conn::http1::handshake(io)
        .await
        .context("control http handshake")?;
    // The connection task ends when the request completes (no pool).
    tokio::spawn(async move {
        let _ = conn.await;
    });

    let mut req = hyper::Request::builder()
        .method(method)
        .uri(path)
        .header(hyper::header::HOST, "thegn-daemon");
    if let Some(t) = token {
        req = req.header(hyper::header::AUTHORIZATION, format!("Bearer {t}"));
    }
    let req = match body {
        Some(v) => req
            .header(hyper::header::CONTENT_TYPE, "application/json")
            .body(http_body_util::Full::new(hyper::body::Bytes::from(
                serde_json::to_vec(&v)?,
            )))?,
        None => req.body(http_body_util::Full::new(hyper::body::Bytes::new()))?,
    };

    let res = sender.send_request(req).await.context("control request")?;
    let status = res.status().as_u16();
    let bytes = res
        .into_body()
        .collect()
        .await
        .context("control response body")?
        .to_bytes();
    let value = if bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&bytes).unwrap_or(Value::Null)
    };
    Ok((status, value))
}
