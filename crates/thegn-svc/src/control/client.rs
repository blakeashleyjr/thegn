//! The control-API client — what `thegn` CLI verbs and the compositor's
//! daemon-backed panes speak.
//!
//! Talks the HTTP surface ([`super::http`]) over a unix socket (local; peer
//! credentials are the auth), TCP (serve mode; bearer token required), or a
//! client-facing HTTP(S) origin. Unix/TCP unary calls remain one hyper
//! connection per request because the daemon is local and those callers are
//! one-shot CLI verbs. HTTP-origin calls reuse one reqwest pool. The
//! warm-attach stream rides a WebSocket (`tokio-tungstenite` over the same
//! stream types).

use anyhow::{Context, Result, anyhow};
use base64::Engine as _;
use futures_util::{SinkExt, StreamExt};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use std::path::PathBuf;
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::sync::mpsc as tokio_mpsc;

use thegn_core::control_wire::{
    EventDecoder, EventFrame, FeedFilter, MAX_WIRE_FRAME, PROTO_VERSION,
};
use thegn_core::store::{ControlStore, DaemonRow};

use super::ControlErrorCode;
use super::{
    CiLogsReply, CiRunsReply, EditorOpenRequest, ForkSpec, OpenSpec, RecordStatus, SessionInfo,
};

/// Heartbeats older than this mark a daemon row stale for discovery.
pub const DAEMON_HEARTBEAT_TTL_MS: i64 = 60_000;

/// Where the daemon is and how to authenticate to it.
#[derive(Clone)]
pub enum ControlAddr {
    /// Local unix socket (implicit auth only after the daemon verifies the
    /// accepted stream's effective uid).
    Unix(PathBuf),
    /// Remote serve-mode listener; every request carries the bearer token.
    Tcp { addr: String, token: String },
    /// Client-facing HTTP(S) origin exposed by a TLS terminator or encrypted
    /// tunnel. Unlike [`Self::Tcp`], this is an origin (`scheme://host:port`),
    /// not a socket address. HTTP requests use reqwest so `https` receives
    /// ordinary WebPKI certificate verification; WebSockets derive `ws`/`wss`
    /// from the same origin.
    HttpOrigin { origin: String, token: String },
}

impl std::fmt::Debug for ControlAddr {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unix(path) => f.debug_tuple("Unix").field(path).finish(),
            Self::Tcp { addr, .. } => f
                .debug_struct("Tcp")
                .field("addr", addr)
                .field("token", &"[REDACTED]")
                .finish(),
            Self::HttpOrigin { origin, .. } => f
                .debug_struct("HttpOrigin")
                .field("origin", origin)
                .field("token", &"[REDACTED]")
                .finish(),
        }
    }
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

#[derive(Clone)]
pub struct ControlClient {
    addr: ControlAddr,
    /// Present only for [`ControlAddr::HttpOrigin`]. `reqwest::Client` is
    /// already internally shared, so cloning `ControlClient` also shares the
    /// connection pool without another wrapper or a global cache. The result
    /// is stored so a construction failure remains fail-closed when the
    /// request is made; it must never degrade to reqwest's policy-free
    /// default client.
    http_client: Option<std::result::Result<reqwest::Client, ControlTransportError>>,
    limits: ControlLimits,
}

/// A policy-configured control transport could not be initialized.
///
/// This deliberately carries no builder detail: the request-facing error is
/// stable and contains no configuration or credential material. The detailed
/// reqwest error is logged at construction time for diagnostics.
#[derive(Debug, Clone, Copy)]
pub struct ControlTransportError;

impl std::fmt::Display for ControlTransportError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("could not initialize policy-configured control HTTP client")
    }
}

impl std::error::Error for ControlTransportError {}

pub(super) fn encoded_issue_path(id: &str, suffix: &str) -> Result<String> {
    crate::issue::validate_control_issue_id(id).map_err(|e| anyhow!(e.to_string()))?;
    let encoded = crate::issue::identity::encode_control_segment(id).map_err(|e| anyhow!(e))?;
    Ok(format!("/v1/issues/{encoded}{suffix}"))
}

/// An HTTP response rejected by the control API.
///
/// Keep the status alongside the server's message so callers that have a
/// narrow, protocol-defined recovery (for example, a session disappearing
/// after selection) do not have to classify an error by matching display text.
#[derive(Debug)]
pub struct ControlRequestError {
    status: u16,
    message: String,
    code: Option<ControlErrorCode>,
}

impl ControlRequestError {
    pub fn new(status: u16, message: impl Into<String>) -> Self {
        Self {
            status,
            message: message.into(),
            code: None,
        }
    }

    fn with_code(status: u16, message: impl Into<String>, code: Option<ControlErrorCode>) -> Self {
        Self {
            status,
            message: message.into(),
            code,
        }
    }

    pub fn status(&self) -> u16 {
        self.status
    }

    /// The server's stable error code, when supplied by a current server.
    /// `None` means the response came from an older server (or used an
    /// unrecognized future code) and remains readable via status/message.
    pub fn code(&self) -> Option<ControlErrorCode> {
        self.code
    }
}

impl std::fmt::Display for ControlRequestError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} (http {})", self.message, self.status)
    }
}

impl std::error::Error for ControlRequestError {}

/// A successful control response that violates the JSON protocol. Keep these
/// messages fixed: proxy bodies and content-type values are untrusted and must
/// never become part of an error shown or logged by callers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ControlProtocolError {
    EmptySuccessBody,
    MissingJsonContentType,
    InvalidJson,
}

impl std::fmt::Display for ControlProtocolError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let message = match self {
            Self::EmptySuccessBody => "control protocol success response was empty",
            Self::MissingJsonContentType => {
                "control protocol success response was not declared as JSON"
            }
            Self::InvalidJson => "control protocol success response contained invalid JSON",
        };
        f.write_str(message)
    }
}

impl std::error::Error for ControlProtocolError {}

fn request_error(status: u16, value: &Value) -> ControlRequestError {
    let message = value
        .get("error")
        .and_then(Value::as_str)
        .unwrap_or("control request failed");
    let code = value
        .get("code")
        .and_then(|value| serde_json::from_value(value.clone()).ok());
    ControlRequestError::with_code(status, message, code)
}

fn required_field<'a>(value: &'a Value, field: &str) -> Result<&'a Value> {
    value
        .get(field)
        .ok_or_else(|| anyhow!("control response is missing required `{field}` field"))
}

fn required_array<'a>(value: &'a Value, field: &str) -> Result<&'a Value> {
    let value = required_field(value, field)?;
    anyhow::ensure!(
        value.is_array(),
        "control response field `{field}` was not an array"
    );
    Ok(value)
}

fn is_json_content_type(value: &str) -> bool {
    value
        .split(';')
        .next()
        .is_some_and(|kind| kind.trim().eq_ignore_ascii_case("application/json"))
}

/// Parse a response body for the two HTTP control transports. Every current
/// JSON control route declares a JSON success body; there is no documented
/// empty-body success route to allow here. Non-success bodies remain best
/// effort because they feed the existing structured server-error fallback.
fn parse_response_body(status: u16, content_type: Option<&str>, bytes: &[u8]) -> Result<Value> {
    if !(200..300).contains(&status) {
        return Ok(if bytes.is_empty() {
            Value::Null
        } else {
            serde_json::from_slice(bytes).unwrap_or(Value::Null)
        });
    }
    if bytes.is_empty() {
        return Err(ControlProtocolError::EmptySuccessBody.into());
    }
    if !content_type.is_some_and(is_json_content_type) {
        return Err(ControlProtocolError::MissingJsonContentType.into());
    }
    serde_json::from_slice(bytes).map_err(|_| ControlProtocolError::InvalidJson.into())
}

/// Default time allowed to establish a control connection (socket connect,
/// and for streams the WebSocket upgrade).
pub const DEFAULT_CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
/// Default total deadline for an ordinary finite control request.
pub const DEFAULT_REQUEST_TIMEOUT: Duration = Duration::from_secs(60);
/// Total deadline for requests that legitimately run longer (session launch,
/// worktree creation, tool runs, CI logs, previews).
pub const DEFAULT_LONG_REQUEST_TIMEOUT: Duration = Duration::from_secs(300);
/// Extra time added to a caller-supplied `wait` timeout so the daemon's own
/// timed-out answer arrives before the client gives up.
pub const WAIT_GRACE: Duration = Duration::from_secs(30);
/// Largest JSON response body the client will buffer.
pub const DEFAULT_MAX_RESPONSE_BYTES: usize = 8 * 1024 * 1024;

/// The bounds applied to every control request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ControlLimits {
    pub connect_timeout: Duration,
    pub request_timeout: Duration,
    pub long_request_timeout: Duration,
    pub max_response_bytes: usize,
}

impl Default for ControlLimits {
    fn default() -> Self {
        Self {
            connect_timeout: DEFAULT_CONNECT_TIMEOUT,
            request_timeout: DEFAULT_REQUEST_TIMEOUT,
            long_request_timeout: DEFAULT_LONG_REQUEST_TIMEOUT,
            max_response_bytes: DEFAULT_MAX_RESPONSE_BYTES,
        }
    }
}

/// Which deadline a request runs under.
#[derive(Debug, Clone, Copy)]
enum Deadline {
    Default,
    Long,
    After(Duration),
    /// Explicitly unbounded long-poll (a `wait` with no caller timeout).
    /// Connect and body-size bounds still apply.
    Unbounded,
}

/// A transport-level bound was hit. Fixed messages: nothing from the peer is
/// ever included.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ControlBoundError {
    ConnectTimeout,
    RequestTimeout,
    ResponseTooLarge,
}

impl std::fmt::Display for ControlBoundError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::ConnectTimeout => "control connection timed out",
            Self::RequestTimeout => "control request timed out",
            Self::ResponseTooLarge => "control response exceeded the size limit",
        })
    }
}

impl std::error::Error for ControlBoundError {}

fn wait_deadline(timeout_ms: Option<i64>) -> Deadline {
    match timeout_ms {
        Some(ms) if ms >= 0 => Deadline::After(Duration::from_millis(ms as u64) + WAIT_GRACE),
        _ => Deadline::Unbounded,
    }
}

/// Aborts the spawned connection driver when dropped, so a timeout or size
/// failure never leaves detached connection work behind.
struct AbortOnDrop(tokio::task::JoinHandle<()>);

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// Run `fut` under `limit`, mapping expiry to `err`.
async fn bounded<T>(
    limit: Duration,
    err: ControlBoundError,
    fut: impl std::future::Future<Output = Result<T>>,
) -> Result<T> {
    match tokio::time::timeout(limit, fut).await {
        Ok(out) => out,
        Err(_) => Err(anyhow::Error::new(err)),
    }
}

/// Collect a hyper body, rejecting a declared or streamed size over `max`.
async fn collect_capped<B>(mut body: B, max: usize) -> Result<Vec<u8>>
where
    B: hyper::body::Body<Data = hyper::body::Bytes> + Unpin,
    B::Error: std::error::Error + Send + Sync + 'static,
{
    if body.size_hint().lower() > max as u64 {
        return Err(ControlBoundError::ResponseTooLarge.into());
    }
    let mut out = Vec::new();
    while let Some(frame) = body.frame().await {
        let frame = frame.context("control response body")?;
        if let Ok(data) = frame.into_data() {
            if out.len().saturating_add(data.len()) > max {
                return Err(ControlBoundError::ResponseTooLarge.into());
            }
            out.extend_from_slice(&data);
        }
    }
    Ok(out)
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

/// The JSON envelope shared by SSE, plugin consumers, and CLI-facing client
/// code. The frame kind comes from [`EventFrame::kind`], so formatters cannot
/// drift from filter validation or SSE metadata.
pub fn frame_json(frame: &EventFrame) -> Value {
    let mut value = match frame {
        EventFrame::Hello(h) => json!({
            "proto": h.proto, "server": h.server, "scopes": h.scopes,
        }),
        EventFrame::PaneSnapshot {
            session,
            seq,
            cols,
            rows,
            bytes,
        } => json!({
            "session": session, "seq": seq, "cols": cols, "rows": rows,
            "ansi_b64": base64::engine::general_purpose::STANDARD.encode(bytes),
        }),
        EventFrame::PaneDelta {
            session,
            seq,
            bytes,
        } => json!({
            "session": session, "seq": seq,
            "b64": base64::engine::general_purpose::STANDARD.encode(bytes),
        }),
        EventFrame::Activity { json: j } => json!({
            "event": serde_json::from_str::<Value>(j)
                .unwrap_or_else(|_| Value::String(j.clone())),
        }),
        EventFrame::Lease {
            session,
            kind,
            expires_at,
        } => json!({
            "session": session, "event": kind, "expires_at": expires_at,
        }),
        EventFrame::Pairing {
            pairing_id,
            label,
            scope,
            state,
        } => json!({
            "pairing_id": pairing_id, "label": label,
            "scopes": scope, "state": state,
        }),
        EventFrame::Sessions => json!({}),
        EventFrame::SessionExit { session, code } => json!({
            "session": session, "code": code,
        }),
        EventFrame::Lagged { missed } => json!({ "missed": missed }),
    };
    if let Value::Object(fields) = &mut value {
        fields.insert("kind".into(), Value::String(frame.kind().into()));
    }
    value
}

fn parse_session_roster(v: Value) -> Result<Vec<SessionInfo>> {
    let sessions: Vec<SessionInfo> = serde_json::from_value(
        v.get("sessions")
            .context("session roster is missing sessions")?
            .clone(),
    )
    .context("invalid session roster")?;
    anyhow::ensure!(
        sessions.iter().all(|session| !session.id.is_empty()),
        "session roster contains an empty identifier"
    );
    Ok(sessions)
}

impl ControlClient {
    pub fn new(addr: ControlAddr) -> Self {
        let http_client = matches!(&addr, ControlAddr::HttpOrigin { .. }).then(build_http_client);
        Self {
            addr,
            http_client,
            limits: ControlLimits::default(),
        }
    }

    /// Override the request bounds (tests, or callers with a tighter budget).
    pub fn with_limits(mut self, limits: ControlLimits) -> Self {
        self.limits = limits;
        self
    }

    pub fn addr(&self) -> &ControlAddr {
        &self.addr
    }

    fn token(&self) -> Option<&str> {
        match &self.addr {
            ControlAddr::Unix(_) => None,
            ControlAddr::Tcp { token, .. } | ControlAddr::HttpOrigin { token, .. } => Some(token),
        }
    }

    /// One HTTP request → parsed JSON body. Non-2xx returns the error message
    /// from the server's `{"error": …}` envelope.
    async fn request(&self, method: &str, path: &str, body: Option<Value>) -> Result<Value> {
        self.request_with(method, path, body, Deadline::Default)
            .await
    }

    async fn request_long(&self, method: &str, path: &str, body: Option<Value>) -> Result<Value> {
        self.request_with(method, path, body, Deadline::Long).await
    }

    async fn request_with(
        &self,
        method: &str,
        path: &str,
        body: Option<Value>,
        deadline: Deadline,
    ) -> Result<Value> {
        let total = match deadline {
            Deadline::Default => Some(self.limits.request_timeout),
            Deadline::Long => Some(self.limits.long_request_timeout),
            Deadline::After(d) => Some(d),
            Deadline::Unbounded => None,
        };
        let exchange = self.exchange(method, path, body);
        let (status, value) = match total {
            Some(total) => bounded(total, ControlBoundError::RequestTimeout, exchange).await?,
            None => exchange.await?,
        };
        if (200..300).contains(&status) {
            Ok(value)
        } else if (300..400).contains(&status) {
            // Redirects are never part of the control endpoint contract. Keep
            // the response body and Location header out of the error: both
            // can be attacker-controlled and state-changing requests must not
            // be replayed at a new destination.
            Err(anyhow::Error::new(ControlRequestError::new(
                status,
                "control endpoint redirect refused",
            )))
        } else {
            Err(anyhow::Error::new(request_error(status, &value)))
        }
    }

    /// Connect, send one request and read the capped response.
    async fn exchange(
        &self,
        method: &str,
        path: &str,
        body: Option<Value>,
    ) -> Result<(u16, Value)> {
        let connect = self.limits.connect_timeout;
        let max = self.limits.max_response_bytes;
        match &self.addr {
            ControlAddr::Unix(sock) => {
                let ep = crate::ipc::IpcEndpoint::for_socket_path(sock);
                let stream = bounded(connect, ControlBoundError::ConnectTimeout, async {
                    crate::ipc::connect(&ep)
                        .await
                        .with_context(|| format!("connect control endpoint {}", ep.display()))
                })
                .await?;
                send_request(stream, method, path, self.token(), body, connect, max).await
            }
            ControlAddr::Tcp { addr, .. } => {
                let stream = bounded(connect, ControlBoundError::ConnectTimeout, async {
                    tokio::net::TcpStream::connect(addr)
                        .await
                        .with_context(|| format!("connect control addr {addr}"))
                })
                .await?;
                send_request(stream, method, path, self.token(), body, connect, max).await
            }
            ControlAddr::HttpOrigin { origin, token } => {
                let client = self
                    .http_client
                    .as_ref()
                    .ok_or_else(|| anyhow!("HTTP-origin client has no configured transport"))?;
                let client = client
                    .as_ref()
                    .map_err(|error| anyhow::Error::new(*error))?;
                send_origin_request(client, origin, token, method, path, body, max).await
            }
        }
    }

    /// Generic request for the catalog-driven client (`thegn api call`):
    /// verb → route resolution happens in `routes::api_call_for`; this just
    /// performs it. Method is `GET`/`POST`/`DELETE`.
    pub async fn call_raw(&self, method: &str, path: &str, body: Option<Value>) -> Result<Value> {
        // A `/wait` route is a long-poll; derive its budget from the body.
        let deadline = if path.ends_with("/wait") {
            wait_deadline(body.as_ref().and_then(|b| b.get("timeout_ms")?.as_i64()))
        } else {
            Deadline::Long
        };
        self.request_with(method, path, body, deadline).await
    }

    pub async fn health(&self) -> Result<()> {
        self.request("GET", "/health", None).await.map(|_| ())
    }

    pub async fn me(&self) -> Result<Value> {
        self.request("GET", "/v1/me", None).await
    }

    pub async fn sessions(&self) -> Result<Vec<SessionInfo>> {
        let v = self.request("GET", "/v1/sessions", None).await?;
        parse_session_roster(v)
    }

    /// `GET /v1/worktrees` — the worktrees registered with the instance.
    pub async fn worktrees(&self) -> Result<Vec<super::WorktreeInfo>> {
        let v = self.request("GET", "/v1/worktrees", None).await?;
        Ok(serde_json::from_value(
            required_array(&v, "worktrees")?.clone(),
        )?)
    }

    pub async fn open(&self, spec: &OpenSpec) -> Result<SessionInfo> {
        let v = self
            .request_long("POST", "/v1/sessions", Some(serde_json::to_value(spec)?))
            .await?;
        Ok(serde_json::from_value(v)?)
    }

    pub async fn fork(&self, spec: &ForkSpec) -> Result<SessionInfo> {
        let v = self
            .request_long(
                "POST",
                "/v1/sessions/fork",
                Some(serde_json::to_value(spec)?),
            )
            .await?;
        Ok(serde_json::from_value(v)?)
    }

    /// One-shot snapshot: `(seq, rows, cols, ansi_bytes)`.
    pub async fn snapshot(&self, session: &str) -> Result<(u64, u16, u16, Vec<u8>)> {
        let v = self
            .request("GET", &format!("/v1/sessions/{session}/snapshot"), None)
            .await?;
        let returned_session = required_field(&v, "session")?
            .as_str()
            .filter(|value| !value.is_empty())
            .ok_or_else(|| anyhow!("snapshot response has no session identity"))?;
        anyhow::ensure!(
            returned_session == session,
            "snapshot response session identity did not match the request"
        );
        let seq = required_field(&v, "seq")?
            .as_u64()
            .ok_or_else(|| anyhow!("snapshot response has no valid sequence"))?;
        let rows = u16::try_from(
            required_field(&v, "rows")?
                .as_u64()
                .ok_or_else(|| anyhow!("snapshot response has no valid row count"))?,
        )
        .map_err(|_| anyhow!("snapshot response row count exceeded u16"))?;
        let cols = u16::try_from(
            required_field(&v, "cols")?
                .as_u64()
                .ok_or_else(|| anyhow!("snapshot response has no valid column count"))?,
        )
        .map_err(|_| anyhow!("snapshot response column count exceeded u16"))?;
        let ansi_b64 = required_field(&v, "ansi_b64")?
            .as_str()
            .ok_or_else(|| anyhow!("snapshot response has no ANSI payload"))?;
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(ansi_b64)
            .context("snapshot base64")?;
        Ok((seq, rows, cols, bytes))
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

    /// Block until `session` reaches `condition` (a JSON `WaitCondition`), or
    /// `timeout_ms` elapses. Returns the `WaitOutcome` JSON (`matched`,
    /// `condition`, `exit_code`).
    pub async fn wait(
        &self,
        session: &str,
        condition: Value,
        timeout_ms: Option<i64>,
    ) -> Result<Value> {
        // The daemon treats an absent/negative timeout as "wait forever", so
        // that case is the one explicitly unbounded request; otherwise the
        // client allows the caller's timeout plus a grace period.
        self.request_with(
            "POST",
            &format!("/v1/sessions/{session}/wait"),
            Some(json!({ "condition": condition, "timeout_ms": timeout_ms })),
            wait_deadline(timeout_ms),
        )
        .await
    }

    /// Split `session`: open a sibling pane running `argv` (empty = a shell) in
    /// direction `dir` (`right`/`down`). Returns the new [`SessionInfo`].
    pub async fn split(&self, session: &str, dir: &str, argv: &[String]) -> Result<SessionInfo> {
        let v = self
            .request_long(
                "POST",
                &format!("/v1/sessions/{session}/split"),
                Some(json!({ "dir": dir, "argv": argv })),
            )
            .await?;
        Ok(serde_json::from_value(v)?)
    }

    /// Start/stop/query a daemon-side asciicast recording of `session`. `op` is
    /// `"start"`, `"stop"` or `"status"`. Returns the [`RecordStatus`] (path +
    /// byte count; never the recorded contents).
    pub async fn record(&self, session: &str, op: &str) -> Result<RecordStatus> {
        let v = self
            .request(
                "POST",
                &format!("/v1/sessions/{session}/record"),
                Some(json!({ "op": op })),
            )
            .await?;
        Ok(serde_json::from_value(v)?)
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

    /// Enqueue a worktree's branch on the (remote) host's merge queue — the
    /// `route_to_host` remote_mode path. `worktree` is the **host-canonical**
    /// path the host resolves (the sprite's `$THEGN_WORKTREE`), not the sprite's
    /// local mount. Returns the server's `{ "queued": … }` envelope.
    pub async fn merge_add(&self, worktree: &str) -> Result<Value> {
        self.request(
            "POST",
            "/v1/merge/add",
            Some(json!({ "worktree": worktree })),
        )
        .await
    }

    /// `GET /v1/pr/status` — cached PR status, one row per worktree with a
    /// `pr_cache` entry.
    pub async fn pr_status(&self) -> Result<Vec<super::PrStatusRow>> {
        let v = self.request("GET", "/v1/pr/status", None).await?;
        Ok(serde_json::from_value(required_array(&v, "prs")?.clone())?)
    }

    /// `GET /v1/ci/runs` — cache-first CI run history for a worktree.
    pub async fn ci_runs(&self, worktree: &str, limit: Option<usize>) -> Result<CiRunsReply> {
        let mut path = format!("/v1/ci/runs?worktree={}", query_escape(worktree));
        if let Some(limit) = limit {
            path.push_str(&format!("&limit={limit}"));
        }
        let value = self.request_long("GET", &path, None).await?;
        Ok(serde_json::from_value(value)?)
    }

    /// `GET /v1/ci/logs` — one bounded, redacted job-log projection.
    pub async fn ci_logs(
        &self,
        worktree: &str,
        run_id: &str,
        job_id: &str,
        tail_lines: Option<usize>,
    ) -> Result<CiLogsReply> {
        let mut path = format!(
            "/v1/ci/logs?worktree={}&run={}&job={}",
            query_escape(worktree),
            query_escape(run_id),
            query_escape(job_id),
        );
        if let Some(tail_lines) = tail_lines {
            path.push_str(&format!("&tail_lines={tail_lines}"));
        }
        let value = self.request_long("GET", &path, None).await?;
        Ok(serde_json::from_value(value)?)
    }

    /// `POST /v1/notify` — push a notification into the tray. Returns the
    /// stored notification's row id.
    pub async fn notify_push(&self, note: &super::PushedNote) -> Result<i64> {
        let v = self
            .request("POST", "/v1/notify", Some(serde_json::to_value(note)?))
            .await?;
        v.get("id")
            .and_then(Value::as_i64)
            .ok_or_else(|| anyhow!("malformed notify reply: {v}"))
    }

    pub async fn automations_list(&self) -> Result<Vec<super::AutomationRuleInfo>> {
        let value = self.request("GET", "/v1/automations", None).await?;
        Ok(serde_json::from_value(
            required_array(&value, "rules")?.clone(),
        )?)
    }

    pub async fn automations_test(
        &self,
        request: &super::AutomationTestRequest,
    ) -> Result<super::AutomationTestReply> {
        let value = self
            .request(
                "POST",
                "/v1/automations/test",
                Some(serde_json::to_value(request)?),
            )
            .await?;
        Ok(serde_json::from_value(value)?)
    }

    pub async fn tools_run(&self, request: &super::ToolRunRequest) -> Result<SessionInfo> {
        let value = self
            .request_long(
                "POST",
                "/v1/tools/run",
                Some(serde_json::to_value(request)?),
            )
            .await?;
        Ok(serde_json::from_value(value)?)
    }

    /// `GET /v1/mcp_proxy/status` — the mcp-proxy hub's per-upstream state.
    pub async fn mcp_proxy_status(&self) -> Result<super::McpProxyStatus> {
        let v = self.request("GET", "/v1/mcp_proxy/status", None).await?;
        Ok(serde_json::from_value(v)?)
    }

    /// `POST /v1/mcp_proxy/reload` — re-read config and reconcile the hub.
    pub async fn mcp_proxy_reload(&self) -> Result<super::McpProxyReloadReport> {
        let v = self.request("POST", "/v1/mcp_proxy/reload", None).await?;
        Ok(serde_json::from_value(v)?)
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

    /// Queue a safe editor handoff through `POST /v1/editor/open`.
    pub async fn open_editor(&self, request: &EditorOpenRequest) -> Result<()> {
        request.target()?;
        let value = self
            .request(
                "POST",
                "/v1/editor/open",
                Some(serde_json::to_value(request)?),
            )
            .await?;
        if value.get("queued").and_then(Value::as_bool) == Some(true) {
            Ok(())
        } else {
            Err(anyhow!("malformed editor-open reply: {value}"))
        }
    }

    /// `POST /v1/preview/fetch` — one bounded, credential-free preview GET.
    pub async fn preview_fetch(
        &self,
        req: &super::PreviewFetchRequest,
    ) -> Result<super::PreviewFetchReply> {
        let v = self
            .request_long(
                "POST",
                "/v1/preview/fetch",
                Some(serde_json::to_value(req)?),
            )
            .await?;
        Ok(serde_json::from_value(v)?)
    }

    // --- agent orchestration (THE-57) ---------------------------------------

    /// `POST /v1/worktrees` — create a worktree, optionally from an issue.
    pub async fn worktree_create(
        &self,
        req: &super::WorktreeCreateReq,
    ) -> Result<super::WorktreeInfo> {
        let v = self
            .request_long("POST", "/v1/worktrees", Some(serde_json::to_value(req)?))
            .await?;
        Ok(serde_json::from_value(v)?)
    }

    /// `POST /v1/worktrees/folder` — assign or clear a repo-local folder.
    pub async fn folder_assign(&self, req: &super::FolderAssignReq) -> Result<()> {
        self.request(
            "POST",
            "/v1/worktrees/folder",
            Some(serde_json::to_value(req)?),
        )
        .await?;
        Ok(())
    }

    /// `GET /v1/issues` — tracker issues, filtered by status/limit.
    pub async fn issues_list(
        &self,
        statuses: &[thegn_core::issue::IssueStatus],
        limit: usize,
        repo: Option<&str>,
    ) -> Result<Vec<thegn_core::issue::Issue>> {
        let mut path = String::from("/v1/issues");
        let mut params: Vec<String> = Vec::new();
        if !statuses.is_empty() {
            let csv = statuses
                .iter()
                .map(|s| s.as_str())
                .collect::<Vec<_>>()
                .join(",");
            params.push(format!("status={csv}"));
        }
        if limit > 0 {
            params.push(format!("limit={limit}"));
        }
        if let Some(repo) = repo {
            params.push(format!("repo={}", percent_encode(repo)));
        }
        if !params.is_empty() {
            path.push('?');
            path.push_str(&params.join("&"));
        }
        let v = self.request("GET", &path, None).await?;
        Ok(serde_json::from_value(
            required_array(&v, "issues")?.clone(),
        )?)
    }

    /// `GET /v1/issues/{id}` — one issue with detail/comments.
    pub async fn issue_get(
        &self,
        id: &str,
        repo: Option<&str>,
    ) -> Result<thegn_core::issue::IssueDetail> {
        let path = with_repo_query(encoded_issue_path(id, "")?, repo);
        let v = self.request("GET", &path, None).await?;
        Ok(serde_json::from_value(v)?)
    }

    /// `POST /v1/issues/{id}` — patch an issue; returns the updated issue.
    pub async fn issue_update(
        &self,
        id: &str,
        patch: &thegn_core::issue::IssuePatch,
        repo: Option<&str>,
    ) -> Result<thegn_core::issue::Issue> {
        let path = with_repo_query(encoded_issue_path(id, "")?, repo);
        let v = self
            .request("POST", &path, Some(serde_json::to_value(patch)?))
            .await?;
        Ok(serde_json::from_value(v)?)
    }

    /// `POST /v1/issues/{id}/comment` — add a comment.
    pub async fn issue_comment(&self, id: &str, body: &str, repo: Option<&str>) -> Result<()> {
        let path = with_repo_query(encoded_issue_path(id, "/comment")?, repo);
        self.request("POST", &path, Some(json!({ "body": body })))
            .await
            .map(|_| ())
    }

    /// `GET /v1/dispatches` — the durable dispatch roster.
    pub async fn dispatches_list(&self) -> Result<Vec<thegn_core::issue::AgentDispatch>> {
        let v = self.request("GET", "/v1/dispatches", None).await?;
        Ok(serde_json::from_value(
            required_array(&v, "dispatches")?.clone(),
        )?)
    }

    /// `POST /v1/dispatches` — record a new dispatch.
    pub async fn dispatch_put(
        &self,
        req: &super::DispatchPutReq,
    ) -> Result<thegn_core::issue::AgentDispatch> {
        let v = self
            .request("POST", "/v1/dispatches", Some(serde_json::to_value(req)?))
            .await?;
        Ok(serde_json::from_value(v)?)
    }

    /// `POST /v1/dispatches/{id}/status` — advance a dispatch's status.
    pub async fn dispatch_set_status(
        &self,
        id: i64,
        status: thegn_core::issue::AgentDispatchStatus,
    ) -> Result<()> {
        self.request(
            "POST",
            &format!("/v1/dispatches/{id}/status"),
            Some(json!({ "status": status.as_str() })),
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

    /// Subscribe to the broadcast event feed (`GET /v1/events` over WebSocket):
    /// activity, lease, pairing, session-list and exit frames (never pane
    /// bytes — those ride attach streams). Read scope. The returned stream's
    /// `frames` yield decoded [`EventFrame`]s (the daemon greets with `Hello`
    /// first); the `control` half is unused (the feed takes no client input) but
    /// keeping the [`AttachStream`] alive keeps the pump running. Dropping it
    /// ends the subscription.
    pub async fn subscribe_events(&self) -> Result<AttachStream> {
        self.subscribe_events_opts(&FeedFilter::default()).await
    }

    /// Subscribe with per-connection narrowing and optional lag signaling.
    /// The server-side filter is still constrained to the read-scoped feed;
    /// this method only adds query parameters and keeps the 256-frame buffer.
    pub async fn subscribe_events_opts(&self, filter: &FeedFilter) -> Result<AttachStream> {
        let path = events_path(filter);
        let (host, token, uri) = match &self.addr {
            ControlAddr::Unix(_) => (
                "localhost".to_string(),
                None,
                format!("ws://localhost{path}"),
            ),
            ControlAddr::Tcp { addr, token } => (
                addr.clone(),
                Some(token.clone()),
                format!("ws://{addr}{path}"),
            ),
            ControlAddr::HttpOrigin { origin, token } => (
                origin_authority(origin)?,
                Some(token.clone()),
                websocket_url(origin, &path)?,
            ),
        };
        let mut req = tokio_tungstenite::tungstenite::http::Request::builder()
            .method("GET")
            .uri(uri)
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
        let req = req.body(()).context("build events request")?;
        let connect = self.limits.connect_timeout;
        let (frame_tx, frame_rx) = tokio_mpsc::channel::<EventFrame>(256);
        let (ctrl_tx, ctrl_rx) = tokio_mpsc::channel::<AttachControl>(1);
        match &self.addr {
            ControlAddr::Unix(sock) => {
                let ep = crate::ipc::IpcEndpoint::for_socket_path(sock);
                let ws = bounded(connect, ControlBoundError::ConnectTimeout, async {
                    let stream = crate::ipc::connect(&ep)
                        .await
                        .with_context(|| format!("connect control endpoint {}", ep.display()))?;
                    let (ws, _) = tokio_tungstenite::client_async_with_config(
                        req,
                        stream,
                        Some(control_ws_config()),
                    )
                    .await
                    .context("events websocket handshake")?;
                    Ok(ws)
                })
                .await?;
                start_attach(ws, frame_tx, ctrl_rx).await?;
            }
            ControlAddr::Tcp { addr, .. } => {
                let ws = bounded(connect, ControlBoundError::ConnectTimeout, async {
                    let stream = tokio::net::TcpStream::connect(addr)
                        .await
                        .with_context(|| format!("connect control addr {addr}"))?;
                    let (ws, _) = tokio_tungstenite::client_async_with_config(
                        req,
                        stream,
                        Some(control_ws_config()),
                    )
                    .await
                    .context("events websocket handshake")?;
                    Ok(ws)
                })
                .await?;
                start_attach(ws, frame_tx, ctrl_rx).await?;
            }
            ControlAddr::HttpOrigin { .. } => {
                let ws = bounded(connect, ControlBoundError::ConnectTimeout, async {
                    let (ws, _) = tokio_tungstenite::connect_async_with_config(
                        req,
                        Some(control_ws_config()),
                        false,
                    )
                    .await
                    .context("events websocket handshake")?;
                    Ok(ws)
                })
                .await?;
                start_attach(ws, frame_tx, ctrl_rx).await?;
            }
        }
        Ok(AttachStream {
            frames: frame_rx,
            control: ctrl_tx,
        })
    }

    /// Owned-options convenience form for callers that construct a filter at
    /// the call site.
    pub async fn subscribe_events_with(&self, filter: FeedFilter) -> Result<AttachStream> {
        self.subscribe_events_opts(&filter).await
    }

    /// Warm-attach over WebSocket. The first frames on `frames` are `Hello`
    /// then the `PaneSnapshot`; live deltas follow. The snapshot carries the
    /// scrollback history tail (a fresh client emulator wants the context);
    /// reconnect paths use [`Self::attach_opts`] with `include_history =
    /// false`.
    pub async fn attach(
        &self,
        session: &str,
        client_id: &str,
        rows: u16,
        cols: u16,
        observer: bool,
    ) -> Result<AttachStream> {
        self.attach_opts(session, client_id, rows, cols, observer, true)
            .await
    }

    /// [`Self::attach`] with explicit control over the snapshot's scrollback
    /// context: a reconnect re-feeds an emulator that already holds the
    /// history, so it passes `include_history = false` and the daemon omits
    /// the tail (repaint only — no duplicated scrollback).
    pub async fn attach_opts(
        &self,
        session: &str,
        client_id: &str,
        rows: u16,
        cols: u16,
        observer: bool,
        include_history: bool,
    ) -> Result<AttachStream> {
        let path = format!(
            "/v1/sessions/{session}/attach?client_id={client_id}&rows={rows}&cols={cols}&observer={observer}&history={include_history}"
        );
        let (host, token, uri) = match &self.addr {
            ControlAddr::Unix(_) => (
                "localhost".to_string(),
                None,
                format!("ws://localhost{path}"),
            ),
            ControlAddr::Tcp { addr, token } => (
                addr.clone(),
                Some(token.clone()),
                format!("ws://{addr}{path}"),
            ),
            ControlAddr::HttpOrigin { origin, token } => (
                origin_authority(origin)?,
                Some(token.clone()),
                websocket_url(origin, &path)?,
            ),
        };
        let mut req = tokio_tungstenite::tungstenite::http::Request::builder()
            .method("GET")
            .uri(uri)
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

        let connect = self.limits.connect_timeout;
        let (frame_tx, frame_rx) = tokio_mpsc::channel::<EventFrame>(256);
        let (ctrl_tx, ctrl_rx) = tokio_mpsc::channel::<AttachControl>(64);
        match &self.addr {
            ControlAddr::Unix(sock) => {
                let ep = crate::ipc::IpcEndpoint::for_socket_path(sock);
                let ws = bounded(connect, ControlBoundError::ConnectTimeout, async {
                    let stream = crate::ipc::connect(&ep)
                        .await
                        .with_context(|| format!("connect control endpoint {}", ep.display()))?;
                    let (ws, _) = tokio_tungstenite::client_async_with_config(
                        req,
                        stream,
                        Some(control_ws_config()),
                    )
                    .await
                    .context("attach websocket handshake")?;
                    Ok(ws)
                })
                .await?;
                start_attach(ws, frame_tx, ctrl_rx).await?;
            }
            ControlAddr::Tcp { addr, .. } => {
                let ws = bounded(connect, ControlBoundError::ConnectTimeout, async {
                    let stream = tokio::net::TcpStream::connect(addr)
                        .await
                        .with_context(|| format!("connect control addr {addr}"))?;
                    let (ws, _) = tokio_tungstenite::client_async_with_config(
                        req,
                        stream,
                        Some(control_ws_config()),
                    )
                    .await
                    .context("attach websocket handshake")?;
                    Ok(ws)
                })
                .await?;
                start_attach(ws, frame_tx, ctrl_rx).await?;
            }
            ControlAddr::HttpOrigin { .. } => {
                let ws = bounded(connect, ControlBoundError::ConnectTimeout, async {
                    let (ws, _) = tokio_tungstenite::connect_async_with_config(
                        req,
                        Some(control_ws_config()),
                        false,
                    )
                    .await
                    .context("attach websocket handshake")?;
                    Ok(ws)
                })
                .await?;
                start_attach(ws, frame_tx, ctrl_rx).await?;
            }
        }
        Ok(AttachStream {
            frames: frame_rx,
            control: ctrl_tx,
        })
    }
}

/// Encode a query component without pulling URL policy into the core crate.
/// Paths and ids normally contain only safe ASCII, but worktree names can
/// contain spaces and the control endpoint must remain unambiguous there.
fn query_escape(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'/' | b'~') {
            out.push(byte as char);
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}

fn events_path(filter: &FeedFilter) -> String {
    let mut params = Vec::new();
    if let Some(kinds) = &filter.kinds {
        params.push(format!("kinds={}", percent_encode(&kinds.join(","))));
    }
    if let Some(session) = &filter.session {
        params.push(format!("session={}", percent_encode(session)));
    }
    if filter.signal_lag {
        params.push("signal_lag=1".into());
    }
    if params.is_empty() {
        "/v1/events".into()
    } else {
        format!("/v1/events?{}", params.join("&"))
    }
}

fn percent_encode(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~') {
            out.push(byte as char);
        } else {
            use std::fmt::Write as _;
            let _ = write!(out, "%{byte:02X}");
        }
    }
    out
}

fn with_repo_query(mut path: String, repo: Option<&str>) -> String {
    if let Some(repo) = repo {
        path.push('?');
        path.push_str("repo=");
        path.push_str(&percent_encode(repo));
    }
    path
}

type Ws<S> = tokio_tungstenite::WebSocketStream<S>;

/// Keep transport allocations within the largest legal encoded control frame.
/// tungstenite 0.29 measures these limits in WebSocket payload bytes (excluding
/// the WebSocket framing header), which includes the five-byte control header.
fn control_ws_config() -> tokio_tungstenite::tungstenite::protocol::WebSocketConfig {
    tokio_tungstenite::tungstenite::protocol::WebSocketConfig::default()
        .max_frame_size(Some(MAX_WIRE_FRAME))
        .max_message_size(Some(MAX_WIRE_FRAME))
}

/// Longest we wait for the daemon's greeting after the WS handshake before
/// declaring the connect wedged. The `Hello` is sent immediately after the
/// server-side attach succeeds, so a healthy connect never comes near this.
const HELLO_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

/// Read the daemon's greeting, enforce protocol compatibility, forward the
/// initial frame(s), then hand the socket to the long-lived pump.
///
/// This is the version-skew guard: the daemon greets every attach with
/// [`EventFrame::Hello`] carrying its `PROTO_VERSION`, and an incompatible
/// daemon (an old binary surviving an upgrade, or vice versa) is refused HERE
/// with an actionable error instead of misdecoding frames mid-session. The
/// same-version path pays no extra round trip — the greeting bytes are
/// already in flight behind the handshake.
async fn start_attach<S>(
    mut ws: Ws<S>,
    frames: tokio_mpsc::Sender<EventFrame>,
    ctrl: tokio_mpsc::Receiver<AttachControl>,
) -> Result<()>
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    use tokio_tungstenite::tungstenite::Message;
    let deadline = tokio::time::Instant::now() + HELLO_TIMEOUT;
    let hello = loop {
        let msg = tokio::time::timeout_at(deadline, ws.next())
            .await
            .map_err(|_| anyhow!("pane daemon sent no greeting within {HELLO_TIMEOUT:?}"))?;
        match msg {
            Some(Ok(Message::Binary(bytes))) => {
                let frame = EventDecoder::decode_message(&bytes).map_err(|e| {
                    anyhow!(
                        "undecodable greeting from the pane daemon ({e}) — likely a \
                         protocol-incompatible daemon; restart it (`thegn daemon`) or \
                         quit stale daemons"
                    )
                })?;
                let EventFrame::Hello(hello) = frame else {
                    return Err(anyhow!(
                        "expected Hello as the first control frame from the pane daemon"
                    ));
                };
                break hello;
            }
            // The server's attach-failure envelope (a JSON text frame).
            Some(Ok(Message::Text(text))) => {
                let msg = serde_json::from_str::<Value>(&text)
                    .ok()
                    .and_then(|v| v.get("error").and_then(Value::as_str).map(str::to_string))
                    .unwrap_or_else(|| text.to_string());
                return Err(anyhow!("attach refused: {msg}"));
            }
            Some(Ok(_)) => continue, // ping/pong
            Some(Err(e)) => return Err(anyhow!("attach websocket error: {e}")),
            None => return Err(anyhow!("attach stream closed before the daemon's greeting")),
        }
    };
    if hello.proto != PROTO_VERSION {
        return Err(anyhow!(
            "pane daemon ({}) speaks control protocol v{}, this thegn speaks v{PROTO_VERSION} — \
             restart the daemon (`thegn daemon`) or quit stale daemons",
            hello.server,
            hello.proto,
        ));
    }
    // Publish the receiver and start draining before forwarding the validated
    // greeting. This ordering cannot deadlock on a prefilled bootstrap queue.
    tokio::spawn(pump_attach_inner(ws, frames.clone(), ctrl));
    let _ = frames.send(EventFrame::Hello(hello)).await;
    Ok(())
}

async fn pump_attach_inner<S>(
    mut ws: Ws<S>,
    frames: tokio_mpsc::Sender<EventFrame>,
    mut ctrl: tokio_mpsc::Receiver<AttachControl>,
) where
    S: AsyncRead + AsyncWrite + Unpin,
{
    use tokio_tungstenite::tungstenite::Message;
    loop {
        tokio::select! {
            msg = ws.next() => match msg {
                Some(Ok(Message::Binary(bytes))) => {
                    match EventDecoder::decode_message(&bytes) {
                        Ok(frame) => {
                            if frames.send(frame).await.is_err() {
                                return; // consumer gone
                            }
                        }
                        Err(e) => {
                            tracing::warn!(target: "thegn::control", "attach stream decode error: {e}");
                            return;
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
                    if ws.send(Message::Binary(bytes.into())).await.is_err() {
                        return;
                    }
                }
                Some(AttachControl::Resize { rows, cols }) => {
                    let text = json!({ "type": "resize", "rows": rows, "cols": cols });
                    if ws.send(Message::Text(text.to_string().into())).await.is_err() {
                        return;
                    }
                }
                Some(AttachControl::Close) | None => {
                    let _ = ws.send(Message::Close(None)).await; // best-effort: peer may be gone
                    return;
                }
            },
        }
    }
}

/// Parse the client-facing endpoint contract. A remote endpoint is an HTTP(S)
/// *origin*, never a raw socket address and never a URL with caller-controlled
/// path/query state. Keeping that distinction explicit prevents a value such as
/// `https://control.example:443` from being handed to `TcpStream::connect`.
fn parse_http_origin(origin: &str) -> Result<reqwest::Url> {
    let mut url = reqwest::Url::parse(origin)
        .with_context(|| format!("invalid control HTTP origin {origin:?}"))?;
    anyhow::ensure!(
        matches!(url.scheme(), "http" | "https"),
        "control HTTP origin must use http or https"
    );
    anyhow::ensure!(url.host_str().is_some(), "control HTTP origin has no host");
    anyhow::ensure!(
        url.username().is_empty() && url.password().is_none(),
        "control HTTP origin must not contain credentials"
    );
    anyhow::ensure!(
        url.path().is_empty() || url.path() == "/",
        "control HTTP origin must not contain a path"
    );
    anyhow::ensure!(
        url.query().is_none() && url.fragment().is_none(),
        "control HTTP origin must not contain a query or fragment"
    );
    // Normalize the one accepted path spelling so request paths join exactly.
    url.set_path("/");
    Ok(url)
}

fn origin_request_url(origin: &str, path: &str) -> Result<reqwest::Url> {
    let mut url = parse_http_origin(origin)?;
    let (request_path, query) = path
        .split_once('?')
        .map_or((path, None), |(path, query)| (path, Some(query)));
    anyhow::ensure!(
        request_path.starts_with('/'),
        "control request path must be absolute"
    );
    url.set_path(request_path);
    url.set_query(query);
    Ok(url)
}

fn origin_authority(origin: &str) -> Result<String> {
    let url = parse_http_origin(origin)?;
    let host = url.host().context("control HTTP origin has no host")?;
    Ok(match url.port() {
        Some(port) => format!("{host}:{port}"),
        None => host.to_string(),
    })
}

fn websocket_url(origin: &str, path: &str) -> Result<String> {
    let mut url = origin_request_url(origin, path)?;
    let scheme = if url.scheme() == "https" { "wss" } else { "ws" };
    url.set_scheme(scheme)
        .map_err(|()| anyhow!("could not derive WebSocket control origin"))?;
    Ok(url.to_string())
}

/// Build the one unary HTTP transport for an HTTP-origin client.
///
/// Keep every reqwest transport policy here. The current contract is exactly
/// redirects are disabled and connects are time-bounded. The total request
/// deadline and response-size cap are enforced by the caller
/// (`ControlClient::request_with` and `send_origin_request`).
///
/// `ControlClient::new` remains infallible for existing command paths, but a
/// builder failure is retained as a typed error and surfaced by the first
/// request. There is deliberately no fallback: `reqwest::Client::new()` would
/// restore the default redirect policy and violate the control transport
/// contract.
fn build_http_client() -> std::result::Result<reqwest::Client, ControlTransportError> {
    reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(DEFAULT_CONNECT_TIMEOUT)
        .build()
        .map_err(|error| {
            tracing::warn!(
                target: "thegn::control",
                %error,
                "could not build policy-configured control HTTP client"
            );
            ControlTransportError
        })
}

/// Send one request to the client-facing HTTP(S) origin. Redirects are
/// explicitly disabled: a 307/308 must never replay an authenticated command
/// body at a second origin. Reqwest supplies the normal WebPKI verification
/// path for `https`; TLS termination remains outside thegn's plaintext loopback
/// backend.
async fn send_origin_request(
    client: &reqwest::Client,
    origin: &str,
    token: &str,
    method: &str,
    path: &str,
    body: Option<Value>,
    max_body: usize,
) -> Result<(u16, Value)> {
    let method = reqwest::Method::from_bytes(method.as_bytes())
        .with_context(|| format!("invalid control HTTP method {method:?}"))?;
    let url = origin_request_url(origin, path)?;
    let mut request = client.request(method, url).bearer_auth(token);
    if let Some(body) = body {
        request = request.json(&body);
    }
    let response = request.send().await.map_err(|error| {
        if error.is_connect() && error.is_timeout() {
            anyhow::Error::new(ControlBoundError::ConnectTimeout)
        } else {
            anyhow::Error::new(error).context("control HTTP request")
        }
    })?;
    if response
        .content_length()
        .is_some_and(|len| len > max_body as u64)
    {
        return Err(ControlBoundError::ResponseTooLarge.into());
    }
    let status = response.status().as_u16();
    let content_type = response
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned);
    if (300..400).contains(&status) {
        // Do not parse a redirect body. The caller turns this into a fixed
        // error, and dropping the response is sufficient to release it.
        return Ok((status, Value::Null));
    }
    let mut response = response;
    let mut bytes: Vec<u8> = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .context("control HTTP response body")?
    {
        if bytes.len().saturating_add(chunk.len()) > max_body {
            return Err(ControlBoundError::ResponseTooLarge.into());
        }
        bytes.extend_from_slice(&chunk);
    }
    let value = parse_response_body(status, content_type.as_deref(), &bytes)?;
    Ok((status, value))
}

/// Send one HTTP/1.1 request over `stream` and collect the JSON body.
async fn send_request<S>(
    stream: S,
    method: &str,
    path: &str,
    token: Option<&str>,
    body: Option<Value>,
    handshake_timeout: Duration,
    max_body: usize,
) -> Result<(u16, Value)>
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    let io = hyper_util::rt::TokioIo::new(stream);
    let (mut sender, conn) = bounded(
        handshake_timeout,
        ControlBoundError::ConnectTimeout,
        async {
            hyper::client::conn::http1::handshake(io)
                .await
                .context("control http handshake")
        },
    )
    .await?;
    // The connection task ends when the request completes (no pool); the
    // guard aborts it on every early exit (timeout, size cap, error).
    let _conn_task = AbortOnDrop(tokio::spawn(async move {
        let _ = conn.await; // best-effort: conn error surfaces via the request path
    }));

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
    if (300..400).contains(&status) {
        // Hyper does not follow redirects, but keep the transport contract
        // explicit so this path cannot grow replay behavior later.
        return Ok((status, Value::Null));
    }
    let content_type = res
        .headers()
        .get(hyper::header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok());
    let content_type = content_type.map(str::to_owned);
    let bytes = collect_capped(res.into_body(), max_body).await?;
    let value = parse_response_body(status, content_type.as_deref(), &bytes)?;
    Ok((status, value))
}

#[cfg(test)]
mod tests {
    use super::*;
    use thegn_core::db::Db;

    #[test]
    fn issue_repo_context_is_encoded_as_a_distinct_query_parameter() {
        let path = encoded_issue_path("linear:TEAM-1", "").unwrap();
        assert_eq!(
            with_repo_query(path, Some("/repo with space")),
            "/v1/issues/linear%3ATEAM-1?repo=%2Frepo%20with%20space"
        );
        assert_eq!(with_repo_query("/v1/issues".into(), None), "/v1/issues");
    }

    async fn one_response_client(
        origin: bool,
        content_type: Option<&str>,
        body: &[u8],
        path: &str,
    ) -> Result<Value> {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let addr = listener.local_addr()?;
        let body = body.to_vec();
        let content_type = content_type.map(str::to_owned);
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            const MAX_HEADERS: usize = 64 * 1024;
            let mut request = Vec::with_capacity(4096);
            loop {
                let mut chunk = [0; 1024];
                let n = stream.read(&mut chunk).await.unwrap();
                assert!(n > 0, "client closed before complete request headers");
                request.extend_from_slice(&chunk[..n]);
                assert!(
                    request.len() <= MAX_HEADERS,
                    "request headers exceeded bound"
                );
                if request.windows(4).any(|window| window == b"\r\n\r\n") {
                    break;
                }
            }
            let content_type = content_type
                .as_deref()
                .map_or(String::new(), |value| format!("Content-Type: {value}\r\n"));
            let header = format!(
                "HTTP/1.1 200 OK\r\n{content_type}Content-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            );
            stream.write_all(header.as_bytes()).await.unwrap();
            stream.write_all(&body).await.unwrap();
        });
        let client = if origin {
            ControlClient::new(ControlAddr::HttpOrigin {
                origin: format!("http://{addr}"),
                token: "protocol-token".into(),
            })
        } else {
            ControlClient::new(ControlAddr::Tcp {
                addr: addr.to_string(),
                token: "protocol-token".into(),
            })
        };
        let result = match path {
            "/v1/worktrees" => client.worktrees().await.map(|_| Value::Bool(true)),
            "/v1/pr/status" => client.pr_status().await.map(|_| Value::Bool(true)),
            "/v1/automations" => client.automations_list().await.map(|_| Value::Bool(true)),
            "/v1/issues" => client
                .issues_list(&[], 0, None)
                .await
                .map(|_| Value::Bool(true)),
            "/v1/dispatches" => client.dispatches_list().await.map(|_| Value::Bool(true)),
            path if path.ends_with("/snapshot") => {
                client.snapshot("s1").await.map(|_| Value::Bool(true))
            }
            _ => client.call_raw("GET", path, None).await,
        };
        server.await.unwrap();
        result
    }

    #[tokio::test]
    async fn http_origin_reuses_one_connection_across_client_clones() {
        use std::sync::{
            Arc,
            atomic::{AtomicUsize, Ordering},
        };
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let accepted = Arc::new(AtomicUsize::new(0));
        let accepted_by_server = Arc::clone(&accepted);
        let server = tokio::spawn(async move {
            loop {
                let (stream, _) = listener.accept().await.unwrap();
                accepted_by_server.fetch_add(1, Ordering::Relaxed);
                tokio::spawn(async move {
                    let mut stream = BufReader::new(stream);
                    for request_no in 0..2 {
                        let mut line = Vec::new();
                        loop {
                            line.clear();
                            if stream.read_until(b'\n', &mut line).await.unwrap() == 0 {
                                return;
                            }
                            if line == b"\r\n" {
                                break;
                            }
                        }
                        let connection = if request_no == 0 {
                            "keep-alive"
                        } else {
                            "close"
                        };
                        let response = format!(
                            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 2\r\nConnection: {connection}\r\n\r\n{{}}"
                        );
                        stream
                            .get_mut()
                            .write_all(response.as_bytes())
                            .await
                            .unwrap();
                    }
                });
            }
        });

        let client = ControlClient::new(ControlAddr::HttpOrigin {
            origin: format!("http://{addr}"),
            token: "pool-token".into(),
        });
        client.clone().health().await.unwrap();
        client.health().await.unwrap();

        assert_eq!(accepted.load(Ordering::Relaxed), 1);
        server.abort();
    }

    #[test]
    fn only_http_origins_construct_a_reqwest_transport() {
        let unix = ControlClient::new(ControlAddr::Unix("/tmp/thegn-control.sock".into()));
        assert!(unix.http_client.is_none());

        let tcp = ControlClient::new(ControlAddr::Tcp {
            addr: "127.0.0.1:5380".into(),
            token: "tcp-token".into(),
        });
        assert!(tcp.http_client.is_none());

        let http = ControlClient::new(ControlAddr::HttpOrigin {
            origin: "http://127.0.0.1:5380".into(),
            token: "http-token".into(),
        });
        assert!(http.http_client.is_some());
    }

    #[tokio::test]
    async fn failed_http_transport_initialization_is_fail_closed_and_typed() {
        let client = ControlClient {
            addr: ControlAddr::HttpOrigin {
                origin: "http://127.0.0.1:1".into(),
                token: "must-not-be-sent".into(),
            },
            http_client: Some(Err(ControlTransportError)),
        };

        let error = client
            .health()
            .await
            .expect_err("a failed transport must not use a degraded client");
        assert!(
            error.downcast_ref::<ControlTransportError>().is_some(),
            "transport initialization failures must remain typed: {error:#}"
        );
    }

    #[tokio::test]
    async fn different_http_origin_clients_keep_in_flight_work_on_old_client() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        async fn serve_one(listener: tokio::net::TcpListener, delay: std::time::Duration) {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut request = Vec::new();
            loop {
                let mut chunk = [0; 1024];
                let n = stream.read(&mut chunk).await.unwrap();
                assert!(n > 0, "client closed before sending request");
                request.extend_from_slice(&chunk[..n]);
                assert!(request.len() <= 64 * 1024, "request headers exceeded bound");
                if request.windows(4).any(|window| window == b"\r\n\r\n") {
                    break;
                }
            }
            tokio::time::sleep(delay).await;
            stream
                .write_all(
                    b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{}",
                )
                .await
                .unwrap();
        }

        let old_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let old_addr = old_listener.local_addr().unwrap();
        let old_server = tokio::spawn(serve_one(
            old_listener,
            std::time::Duration::from_millis(20),
        ));
        let old_client = ControlClient::new(ControlAddr::HttpOrigin {
            origin: format!("http://{old_addr}"),
            token: "old-token".into(),
        });
        let old_request = tokio::spawn({
            let client = old_client.clone();
            async move { client.health().await }
        });
        drop(old_client);

        let replacement_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let replacement_addr = replacement_listener.local_addr().unwrap();
        let replacement_server =
            tokio::spawn(serve_one(replacement_listener, std::time::Duration::ZERO));
        let replacement = ControlClient::new(ControlAddr::HttpOrigin {
            origin: format!("http://{replacement_addr}"),
            token: "new-token".into(),
        });

        // This branch has no config-reload owner to exercise. The property
        // established here is limited to independently constructed clients
        // for different effective origins: replacing the caller's handle
        // does not cancel work already using the old client.
        assert_ne!(old_addr, replacement_addr);
        replacement.health().await.unwrap();
        old_request.await.unwrap().unwrap();
        old_server.await.unwrap();
        replacement_server.await.unwrap();
    }

    #[test]
    fn response_parser_keeps_success_protocol_errors_typed_and_fixed() {
        let valid = parse_response_body(200, Some("application/json; charset=utf-8"), br#"{}"#)
            .expect("valid JSON success response");
        assert_eq!(valid, Value::Object(Default::default()));

        for (content_type, body, expected) in [
            (
                Some("application/json"),
                b"".as_slice(),
                ControlProtocolError::EmptySuccessBody,
            ),
            (
                Some("text/html"),
                br#"{}"#.as_slice(),
                ControlProtocolError::MissingJsonContentType,
            ),
            (
                None,
                br#"{}"#.as_slice(),
                ControlProtocolError::MissingJsonContentType,
            ),
            (
                Some("application/json"),
                b"{truncated".as_slice(),
                ControlProtocolError::InvalidJson,
            ),
        ] {
            let error = parse_response_body(200, content_type, body).unwrap_err();
            assert_eq!(
                error.downcast_ref::<ControlProtocolError>(),
                Some(&expected)
            );
            let rendered = format!("{error:#}");
            assert!(!rendered.contains("truncated"));
            assert!(!rendered.contains("text/html"));
        }

        assert_eq!(
            parse_response_body(404, Some("text/html"), b"not-json").unwrap(),
            Value::Null
        );
        assert_eq!(
            parse_response_body(404, Some("application/json"), br#"{"error":"gone"}"#).unwrap(),
            serde_json::json!({"error": "gone"})
        );
        let redirect = ControlRequestError::new(307, "control endpoint redirect refused");
        assert_eq!(
            redirect.to_string(),
            "control endpoint redirect refused (http 307)"
        );
    }

    #[tokio::test]
    async fn successful_control_replies_require_json_and_do_not_fabricate_envelopes() {
        for (path, valid, missing) in [
            (
                "/v1/worktrees",
                br#"{"worktrees":[]}"#.as_slice(),
                br#"{}"#.as_slice(),
            ),
            (
                "/v1/pr/status",
                br#"{"prs":[]}"#.as_slice(),
                br#"{}"#.as_slice(),
            ),
            (
                "/v1/automations",
                br#"{"rules":[]}"#.as_slice(),
                br#"{}"#.as_slice(),
            ),
            (
                "/v1/issues",
                br#"{"issues":[]}"#.as_slice(),
                br#"{}"#.as_slice(),
            ),
            (
                "/v1/dispatches",
                br#"{"dispatches":[]}"#.as_slice(),
                br#"{}"#.as_slice(),
            ),
        ] {
            for origin in [false, true] {
                let valid_result = tokio::time::timeout(
                    std::time::Duration::from_secs(3),
                    one_response_client(origin, Some("application/json"), valid, path),
                )
                .await
                .expect("valid collection probe exceeded timeout");
                assert!(
                    valid_result.is_ok(),
                    "origin={origin}, path={path}, result={valid_result:?}"
                );

                let missing_result = tokio::time::timeout(
                    std::time::Duration::from_secs(3),
                    one_response_client(origin, Some("application/json"), missing, path),
                )
                .await
                .expect("missing collection envelope probe exceeded timeout");
                assert!(
                    missing_result.is_err(),
                    "origin={origin}, path={path}, result={missing_result:?}"
                );
            }
        }

        for origin in [false, true] {
            for (content_type, body, expected, protocol_error) in [
                (
                    Some("application/json"),
                    br#"{"worktrees": []}"#.as_slice(),
                    true,
                    None,
                ),
                (
                    Some("application/json"),
                    b"{truncated".as_slice(),
                    false,
                    Some(ControlProtocolError::InvalidJson),
                ),
                (
                    Some("text/html"),
                    br#"{"worktrees": []}"#.as_slice(),
                    false,
                    Some(ControlProtocolError::MissingJsonContentType),
                ),
                (Some("application/json"), br#"{}"#.as_slice(), false, None),
                (
                    None,
                    br#"{"worktrees": []}"#.as_slice(),
                    false,
                    Some(ControlProtocolError::MissingJsonContentType),
                ),
                (
                    Some("application/json"),
                    b"".as_slice(),
                    false,
                    Some(ControlProtocolError::EmptySuccessBody),
                ),
            ] {
                let result = tokio::time::timeout(
                    std::time::Duration::from_secs(3),
                    one_response_client(origin, content_type, body, "/v1/worktrees"),
                )
                .await
                .expect("protocol response probe exceeded timeout");
                assert_eq!(
                    result.is_ok(),
                    expected,
                    "origin={origin}, body={body:?}, result={result:?}"
                );
                if !expected {
                    let error = result.unwrap_err();
                    assert_eq!(
                        error.downcast_ref::<ControlProtocolError>().copied(),
                        protocol_error,
                        "origin={origin}, body={body:?}"
                    );
                    let error = format!("{error:#}");
                    assert!(!error.contains("truncated"));
                }
            }
        }
    }

    #[tokio::test]
    async fn snapshot_requires_identity_dimensions_and_payload_on_both_transports() {
        for origin in [false, true] {
            for (body, expected) in [
                (
                    br#"{"session":"s1","seq":7,"rows":24,"cols":80,"ansi_b64":""}"#.as_slice(),
                    true,
                ),
                (
                    br#"{"session":"other","seq":7,"rows":24,"cols":80,"ansi_b64":""}"#.as_slice(),
                    false,
                ),
                (
                    br#"{"session":"s1","seq":7,"rows":65536,"cols":80,"ansi_b64":""}"#.as_slice(),
                    false,
                ),
                (
                    br#"{"session":"s1","seq":7,"rows":24,"cols":80}"#.as_slice(),
                    false,
                ),
            ] {
                let result = tokio::time::timeout(
                    std::time::Duration::from_secs(3),
                    one_response_client(
                        origin,
                        Some("application/json"),
                        body,
                        "/v1/sessions/s1/snapshot",
                    ),
                )
                .await
                .expect("snapshot protocol probe exceeded timeout");
                assert_eq!(
                    result.is_ok(),
                    expected,
                    "origin={origin}, body={body:?}, result={result:?}"
                );
            }
        }
    }

    #[test]
    fn authoritative_roster_rejects_missing_malformed_or_empty_identities() {
        assert!(
            parse_session_roster(serde_json::json!({"sessions": []}))
                .unwrap()
                .is_empty()
        );
        let session = SessionInfo {
            id: "live".into(),
            ..Default::default()
        };
        assert_eq!(
            parse_session_roster(serde_json::json!({"sessions": [session]}))
                .unwrap()
                .len(),
            1
        );
        for invalid in [
            serde_json::json!({}),
            serde_json::json!({"sessions": null}),
            serde_json::json!({"sessions": [{}]}),
            serde_json::json!({"sessions": [SessionInfo::default()]}),
        ] {
            assert!(parse_session_roster(invalid).is_err());
        }
    }

    #[test]
    fn remote_address_debug_redacts_the_bearer() {
        let token = "tgc1_public_secret";
        let rendered = format!(
            "{:?}",
            ControlAddr::Tcp {
                addr: "control.example.test:443".into(),
                token: token.into(),
            }
        );
        assert!(rendered.contains("control.example.test:443"));
        assert!(rendered.contains("[REDACTED]"));
        assert!(!rendered.contains(token));

        let rendered = format!(
            "{:?}",
            ControlAddr::HttpOrigin {
                origin: "https://control.example.test:443".into(),
                token: token.into(),
            }
        );
        assert!(rendered.contains("https://control.example.test:443"));
        assert!(rendered.contains("[REDACTED]"));
        assert!(!rendered.contains(token));
    }

    #[test]
    fn http_origin_is_an_origin_not_a_socket_or_arbitrary_url() {
        assert!(parse_http_origin("https://control.example.test:443").is_ok());
        assert!(parse_http_origin("control.example.test:443").is_err());
        assert!(parse_http_origin("ssh://control.example.test:443").is_err());
        assert!(parse_http_origin("https://user@control.example.test:443").is_err());
        assert!(parse_http_origin("https://control.example.test:443/prefix").is_err());
        assert!(parse_http_origin("https://control.example.test:443?token=nope").is_err());
        assert_eq!(
            websocket_url("https://control.example.test:443", "/v1/events").unwrap(),
            "wss://control.example.test/v1/events"
        );
        assert_eq!(
            websocket_url("http://127.0.0.1:5380", "/v1/events?kinds=exit").unwrap(),
            "ws://127.0.0.1:5380/v1/events?kinds=exit"
        );
    }

    #[test]
    fn http_origin_authority_matches_unary_and_websocket_endpoints() {
        for (origin, authority, request_url, websocket) in [
            (
                "https://[::1]:8443",
                "[::1]:8443",
                "https://[::1]:8443/v1/events",
                "wss://[::1]:8443/v1/events",
            ),
            (
                "https://[::1]",
                "[::1]",
                "https://[::1]/v1/events",
                "wss://[::1]/v1/events",
            ),
            (
                "https://[::1]:443",
                "[::1]",
                "https://[::1]/v1/events",
                "wss://[::1]/v1/events",
            ),
            (
                "http://[::1]:8080",
                "[::1]:8080",
                "http://[::1]:8080/v1/events",
                "ws://[::1]:8080/v1/events",
            ),
            (
                "http://[::1]",
                "[::1]",
                "http://[::1]/v1/events",
                "ws://[::1]/v1/events",
            ),
            (
                "http://[::1]:80",
                "[::1]",
                "http://[::1]/v1/events",
                "ws://[::1]/v1/events",
            ),
            (
                "http://127.0.0.1",
                "127.0.0.1",
                "http://127.0.0.1/v1/events",
                "ws://127.0.0.1/v1/events",
            ),
            (
                "http://127.0.0.1:80",
                "127.0.0.1",
                "http://127.0.0.1/v1/events",
                "ws://127.0.0.1/v1/events",
            ),
            (
                "http://127.0.0.1:5380",
                "127.0.0.1:5380",
                "http://127.0.0.1:5380/v1/events",
                "ws://127.0.0.1:5380/v1/events",
            ),
            (
                "https://control.example.test",
                "control.example.test",
                "https://control.example.test/v1/events",
                "wss://control.example.test/v1/events",
            ),
            (
                "https://control.example.test:8443",
                "control.example.test:8443",
                "https://control.example.test:8443/v1/events",
                "wss://control.example.test:8443/v1/events",
            ),
        ] {
            assert_eq!(origin_authority(origin).unwrap(), authority, "{origin}");
            assert_eq!(
                origin_request_url(origin, "/v1/events")
                    .unwrap()
                    .to_string(),
                request_url,
                "{origin} unary endpoint"
            );
            assert_eq!(
                websocket_url(origin, "/v1/events").unwrap(),
                websocket,
                "{origin}"
            );
        }
    }

    #[test]
    fn http_origin_rejects_scoped_ipv6_zone_ids_with_url_2_5_8() {
        assert!(parse_http_origin("https://[fe80::1%25eth0]/").is_err());
    }

    #[tokio::test]
    async fn http_origin_merge_add_crosses_a_real_tcp_listener_with_bearer() {
        use axum::Json;
        use axum::extract::Request;
        use axum::routing::post;

        async fn echo(request: Request) -> Json<Value> {
            let authorization = request
                .headers()
                .get(axum::http::header::AUTHORIZATION)
                .and_then(|value| value.to_str().ok())
                .unwrap_or_default()
                .to_string();
            let bytes = axum::body::to_bytes(request.into_body(), 8 * 1024)
                .await
                .unwrap();
            let body: Value = serde_json::from_slice(&bytes).unwrap();
            Json(json!({
                "authorization": authorization,
                "worktree": body["worktree"],
            }))
        }

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            axum::serve(
                listener,
                axum::Router::new().route("/v1/merge/add", post(echo)),
            )
            .await
            .unwrap();
        });
        let client = ControlClient::new(ControlAddr::HttpOrigin {
            origin: format!("http://{addr}"),
            token: "route-token".into(),
        });
        let reply = client.merge_add("/registered/remote").await.unwrap();
        assert_eq!(reply["authorization"], "Bearer route-token");
        assert_eq!(reply["worktree"], "/registered/remote");
        server.abort();
    }

    /// A redirect is an endpoint-contract error for both client transports.
    /// The second listener is deliberately a different authority so this
    /// catches the sensitive-body replay that 307/308 would otherwise permit.
    #[tokio::test]
    async fn control_requests_never_follow_redirects_or_replay_credentials() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        async fn probe(status: u16, origin: bool) -> (bool, Vec<u8>, String) {
            async fn consume_request(stream: &mut tokio::net::TcpStream) {
                const MAX_REQUEST: usize = 64 * 1024;
                let mut request = Vec::with_capacity(4096);
                let header_end = loop {
                    let mut chunk = [0; 1024];
                    let n = stream.read(&mut chunk).await.unwrap();
                    assert!(n > 0, "redirect source closed before request headers");
                    request.extend_from_slice(&chunk[..n]);
                    assert!(
                        request.len() <= MAX_REQUEST,
                        "fixture request exceeded bound"
                    );
                    if let Some(end) = request.windows(4).position(|w| w == b"\r\n\r\n") {
                        break end + 4;
                    }
                };
                let content_length = String::from_utf8_lossy(&request[..header_end])
                    .lines()
                    .find_map(|line| {
                        line.strip_prefix("Content-Length:")
                            .or_else(|| line.strip_prefix("content-length:"))
                    })
                    .and_then(|value| value.trim().parse::<usize>().ok())
                    .unwrap_or(0);
                let needed = header_end.saturating_add(content_length);
                assert!(needed <= MAX_REQUEST, "fixture request body exceeded bound");
                while request.len() < needed {
                    let mut chunk = [0; 1024];
                    let n = stream.read(&mut chunk).await.unwrap();
                    assert!(n > 0, "redirect source closed before request body");
                    request.extend_from_slice(&chunk[..n]);
                    assert!(
                        request.len() <= MAX_REQUEST,
                        "fixture request exceeded bound"
                    );
                }
            }

            let target = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let target_addr = target.local_addr().unwrap();
            let source = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let source_addr = source.local_addr().unwrap();
            let (tx, rx) = tokio::sync::oneshot::channel();
            let reason = match status {
                301 => "Moved Permanently",
                302 => "Found",
                303 => "See Other",
                307 => "Temporary Redirect",
                308 => "Permanent Redirect",
                _ => unreachable!(),
            };
            tokio::spawn(async move {
                let (mut stream, _) = source.accept().await.unwrap();
                consume_request(&mut stream).await;
                let location = format!("http://{target_addr}/replay-target");
                let response = format!(
                    "HTTP/1.1 {status} {reason}\r\nLocation: {location}\r\nContent-Type: text/plain\r\nContent-Length: 20\r\nConnection: close\r\n\r\nredirect-body-secret"
                );
                stream.write_all(response.as_bytes()).await.unwrap();
                drop(stream);
                let replay =
                    tokio::time::timeout(std::time::Duration::from_millis(300), target.accept())
                        .await
                        .ok()
                        .and_then(Result::ok);
                let followed = replay.is_some();
                let mut bytes = Vec::new();
                if let Some((mut stream, _)) = replay {
                    let mut request = [0; 4096];
                    if let Ok(n) = stream.read(&mut request).await {
                        bytes.extend_from_slice(&request[..n]);
                    }
                    let _ = stream
                        .write_all(
                            b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                        )
                        .await;
                }
                let _ = tx.send((followed, bytes));
            });

            let client = if origin {
                ControlClient::new(ControlAddr::HttpOrigin {
                    origin: format!("http://{source_addr}"),
                    token: "redirect-token".into(),
                })
            } else {
                ControlClient::new(ControlAddr::Tcp {
                    addr: source_addr.to_string(),
                    token: "redirect-token".into(),
                })
            };
            let error = client
                .call_raw(
                    "POST",
                    "/v1/tools/run",
                    Some(json!({"sentinel": "redirect-body-secret"})),
                )
                .await
                .expect_err("3xx must be rejected");
            let error = format!("{error:#}");
            let (followed, bytes) = rx.await.unwrap();
            (followed, bytes, error)
        }

        for status in [301, 302, 303, 307, 308] {
            for origin in [false, true] {
                let (followed, bytes, error) =
                    tokio::time::timeout(std::time::Duration::from_secs(3), probe(status, origin))
                        .await
                        .expect("redirect probe exceeded timeout");
                assert!(!followed, "{status} followed on origin={origin}: {error}");
                assert!(bytes.is_empty(), "redirect target received a request");
                assert!(error.contains("control endpoint redirect refused"));
                assert!(error.contains(&format!("http {status}")));
                assert!(!error.contains("redirect-token"));
                assert!(!error.contains("replay-target"));
                assert!(!error.contains("redirect-body-secret"));
            }
        }
    }

    #[test]
    fn control_error_request_code_is_optional_for_old_servers() {
        let old = request_error(404, &serde_json::json!({ "error": "not found" }));
        assert_eq!(old.code(), None);

        let current = request_error(
            404,
            &serde_json::json!({ "error": "not found", "code": "not_found" }),
        );
        assert_eq!(current.code(), Some(ControlErrorCode::NotFound));

        let future = request_error(
            500,
            &serde_json::json!({ "error": "failure", "code": "future_code" }),
        );
        assert_eq!(future.code(), None);
    }

    #[test]
    fn fragmented_websocket_binary_frame_reassembles_to_one_control_frame() {
        use std::io::Cursor;
        use tokio_tungstenite::tungstenite::{Message, protocol::Role};

        let encoded = EventFrame::Sessions.encode();
        let split = 2;
        // Deterministic server-to-client WebSocket frames: non-final Binary,
        // then final Continuation. Tungstenite reassembles them as one message.
        let mut wire = vec![0x02, split as u8];
        wire.extend_from_slice(&encoded[..split]);
        wire.push(0x80);
        wire.push((encoded.len() - split) as u8);
        wire.extend_from_slice(&encoded[split..]);

        let mut ws = tokio_tungstenite::tungstenite::WebSocket::from_raw_socket(
            Cursor::new(wire),
            Role::Client,
            Some(control_ws_config()),
        );
        let Message::Binary(message) = ws.read().unwrap() else {
            panic!("expected reassembled binary websocket message");
        };
        assert_eq!(
            EventDecoder::decode_message(&message),
            Ok(EventFrame::Sessions)
        );
    }

    fn daemon_row(id: &str, scope: &str, endpoint: &str, heartbeat_at: i64) -> DaemonRow {
        DaemonRow {
            daemon_id: id.into(),
            pid: 1,
            scope: scope.into(),
            endpoint: endpoint.into(),
            tcp_addr: None,
            control_origin: None,
            hostname: "h".into(),
            version: "0".into(),
            started_at: 0,
            heartbeat_at,
        }
    }

    /// Discovery is scope-bound and freshness-bound: a stale-heartbeat row and
    /// a fresh row for ANOTHER scope must both lose to the fresh same-scope
    /// daemon (freshest heartbeat wins among candidates).
    #[test]
    fn discover_picks_the_fresh_same_scope_daemon() {
        let db = Db::open_memory().unwrap();
        let now = 1_000_000;
        db.put_daemon(&daemon_row(
            "stale-same",
            "/scope/a",
            "/run/stale.sock",
            now - DAEMON_HEARTBEAT_TTL_MS - 1,
        ))
        .unwrap();
        db.put_daemon(&daemon_row(
            "fresh-other",
            "/scope/b",
            "/run/other.sock",
            now - 1,
        ))
        .unwrap();
        db.put_daemon(&daemon_row(
            "fresh-same",
            "/scope/a",
            "/run/fresh.sock",
            now - 10,
        ))
        .unwrap();

        let got = discover(&db, "/scope/a", now).expect("a live same-scope daemon exists");
        match got {
            ControlAddr::Unix(p) => assert_eq!(p, PathBuf::from("/run/fresh.sock")),
            other => panic!("expected a unix addr, got {other:?}"),
        }
    }

    /// All same-scope heartbeats stale ⇒ `None` (callers degrade gracefully —
    /// no daemon is spawned as a side effect of discovery).
    #[test]
    fn discover_returns_none_when_every_heartbeat_is_stale() {
        let db = Db::open_memory().unwrap();
        let now = 1_000_000;
        db.put_daemon(&daemon_row(
            "stale-1",
            "/scope/a",
            "/run/1.sock",
            now - DAEMON_HEARTBEAT_TTL_MS - 1,
        ))
        .unwrap();
        db.put_daemon(&daemon_row("stale-2", "/scope/a", "/run/2.sock", 0))
            .unwrap();
        assert!(discover(&db, "/scope/a", now).is_none());
    }

    /// The version-skew guard: a daemon greeting with an incompatible
    /// `Hello.proto` must fail the attach with an actionable error instead of
    /// handing the caller a stream it will misdecode.
    #[tokio::test(flavor = "multi_thread")]
    async fn attach_refuses_an_incompatible_daemon_proto() {
        use tokio_tungstenite::tungstenite::Message;

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap().to_string();
        tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut ws = tokio_tungstenite::accept_async(stream).await.unwrap();
            let hello = EventFrame::Hello(thegn_core::control_wire::Hello {
                proto: PROTO_VERSION + 1,
                server: "oldhost thegn 0.0".into(),
                scopes: vec![],
            });
            let _ = ws.send(Message::Binary(hello.encode().into())).await; // best-effort: test fixture; client may be gone
            let _ = ws.next().await; // hold the socket open until the client reacts // best-effort: hold open; client may be gone
        });

        let client = ControlClient::new(ControlAddr::Tcp {
            addr,
            token: "t".into(),
        });
        let err = client
            .attach("s1", "c1", 24, 80, false)
            .await
            .err()
            .expect("a proto mismatch must refuse the connect");
        let msg = format!("{err:#}");
        assert!(
            msg.contains("restart the daemon"),
            "error must be actionable: {msg}"
        );
    }

    #[test]
    fn issue_path_encodes_complete_identity_once_for_server_decode() {
        let id = "plugin:demo:客户/任务#7";
        let path = encoded_issue_path(id, "").unwrap();
        assert!(path.starts_with("/v1/issues/plugin%3Ademo%3A"));
        assert!(path.contains("%2F") && path.contains("%23"));
        let encoded = path.strip_prefix("/v1/issues/").unwrap();
        let mut out = Vec::new();
        let bytes = encoded.as_bytes();
        let mut i = 0;
        while i < bytes.len() {
            if bytes[i] == b'%' {
                let hex = std::str::from_utf8(&bytes[i + 1..i + 3]).unwrap();
                out.push(u8::from_str_radix(hex, 16).unwrap());
                i += 3;
            } else {
                out.push(bytes[i]);
                i += 1;
            }
        }
        let decoded = String::from_utf8(out).unwrap();
        assert_eq!(decoded, id);
        assert!(crate::issue::validate_control_issue_id(&decoded).is_ok());
    }

    #[test]
    fn issue_path_rejects_malformed_identity_before_encoding() {
        assert!(encoded_issue_path("linear:bad key", "").is_err());
        assert!(encoded_issue_path(&format!("plugin:demo:{}", "x".repeat(500)), "").is_err());
    }

    // ---- THE-273: deadlines and response-size bounds ----

    fn tiny_limits() -> ControlLimits {
        ControlLimits {
            connect_timeout: Duration::from_millis(200),
            request_timeout: Duration::from_millis(300),
            long_request_timeout: Duration::from_millis(300),
            max_response_bytes: 1024,
        }
    }

    fn bound_of(err: &anyhow::Error) -> Option<ControlBoundError> {
        err.downcast_ref::<ControlBoundError>().copied()
    }

    /// Serve one connection: read the request head, then run `respond`.
    async fn raw_server<F, Fut>(respond: F) -> std::net::SocketAddr
    where
        F: FnOnce(tokio::net::TcpStream) -> Fut + Send + 'static,
        Fut: std::future::Future<Output = ()> + Send + 'static,
    {
        use tokio::io::AsyncReadExt;
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut buf = Vec::new();
            let mut chunk = [0u8; 512];
            while !buf.windows(4).any(|w| w == b"\r\n\r\n") {
                let n = stream.read(&mut chunk).await.unwrap_or(0);
                if n == 0 {
                    return;
                }
                buf.extend_from_slice(&chunk[..n]);
            }
            respond(stream).await;
        });
        addr
    }

    fn clients(addr: std::net::SocketAddr) -> [ControlClient; 2] {
        [
            ControlClient::new(ControlAddr::Tcp {
                addr: addr.to_string(),
                token: "t".into(),
            }),
            ControlClient::new(ControlAddr::HttpOrigin {
                origin: format!("http://{addr}"),
                token: "t".into(),
            }),
        ]
    }

    #[tokio::test]
    async fn stalled_peer_hits_request_deadline_on_both_http_transports() {
        for origin in [false, true] {
            let addr = raw_server(|stream| async move {
                let _stream = stream;
                tokio::time::sleep(Duration::from_secs(30)).await;
            })
            .await;
            let client = clients(addr)[usize::from(origin)]
                .clone()
                .with_limits(tiny_limits());
            let err = client.health().await.unwrap_err();
            assert_eq!(bound_of(&err), Some(ControlBoundError::RequestTimeout));
        }
    }

    #[tokio::test]
    async fn slow_body_hits_request_deadline() {
        use tokio::io::AsyncWriteExt;
        for origin in [false, true] {
            let addr = raw_server(|mut stream| async move {
                let head = "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 100\r\n\r\n{";
                stream.write_all(head.as_bytes()).await.unwrap();
                tokio::time::sleep(Duration::from_secs(30)).await;
            })
            .await;
            let client = clients(addr)[usize::from(origin)]
                .clone()
                .with_limits(tiny_limits());
            let err = client.health().await.unwrap_err();
            assert_eq!(bound_of(&err), Some(ControlBoundError::RequestTimeout));
        }
    }

    #[tokio::test]
    async fn declared_oversize_body_is_rejected_early() {
        use tokio::io::AsyncWriteExt;
        for origin in [false, true] {
            let addr = raw_server(|mut stream| async move {
                // Promise far more than the cap but send almost nothing and
                // stall: only an early Content-Length check can pass this.
                let head = "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 99999999\r\n\r\n{";
                stream.write_all(head.as_bytes()).await.unwrap();
                tokio::time::sleep(Duration::from_secs(30)).await;
            })
            .await;
            let mut limits = tiny_limits();
            limits.request_timeout = Duration::from_secs(20);
            let client = clients(addr)[usize::from(origin)]
                .clone()
                .with_limits(limits);
            let err = client.health().await.unwrap_err();
            assert_eq!(bound_of(&err), Some(ControlBoundError::ResponseTooLarge));
        }
    }

    #[tokio::test]
    async fn chunked_oversize_body_is_aborted_mid_stream() {
        use tokio::io::AsyncWriteExt;
        for origin in [false, true] {
            let addr = raw_server(|mut stream| async move {
                let head = "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nTransfer-Encoding: chunked\r\n\r\n";
                stream.write_all(head.as_bytes()).await.unwrap();
                let payload = "a".repeat(512);
                for _ in 0..64 {
                    let chunk = format!("{:x}\r\n{payload}\r\n", payload.len());
                    if stream.write_all(chunk.as_bytes()).await.is_err() {
                        return;
                    }
                }
                tokio::time::sleep(Duration::from_secs(30)).await;
            })
            .await;
            let mut limits = tiny_limits();
            limits.request_timeout = Duration::from_secs(20);
            let client = clients(addr)[usize::from(origin)]
                .clone()
                .with_limits(limits);
            let err = client.health().await.unwrap_err();
            assert_eq!(bound_of(&err), Some(ControlBoundError::ResponseTooLarge));
        }
    }

    #[tokio::test]
    async fn stalled_websocket_upgrade_hits_connect_deadline() {
        let addr = raw_server(|stream| async move {
            let _stream = stream;
            tokio::time::sleep(Duration::from_secs(30)).await;
        })
        .await;
        let client = ControlClient::new(ControlAddr::Tcp {
            addr: addr.to_string(),
            token: "t".into(),
        })
        .with_limits(tiny_limits());
        let Err(err) = client.subscribe_events().await else {
            panic!("stalled upgrade must fail");
        };
        assert_eq!(bound_of(&err), Some(ControlBoundError::ConnectTimeout));
    }

    #[test]
    fn wait_budget_is_unbounded_only_without_a_caller_timeout() {
        assert!(matches!(wait_deadline(None), Deadline::Unbounded));
        assert!(matches!(wait_deadline(Some(-1)), Deadline::Unbounded));
        match wait_deadline(Some(5_000)) {
            Deadline::After(d) => assert_eq!(d, Duration::from_secs(5) + WAIT_GRACE),
            other => panic!("unexpected {other:?}"),
        }
    }

    #[tokio::test]
    async fn collect_capped_rejects_stream_over_cap_and_accepts_under() {
        let ok = http_body_util::Full::new(hyper::body::Bytes::from(vec![b'x'; 10]));
        assert_eq!(collect_capped(ok, 10).await.unwrap().len(), 10);
        let big = http_body_util::Full::new(hyper::body::Bytes::from(vec![b'x'; 11]));
        let err = collect_capped(big, 10).await.unwrap_err();
        assert_eq!(bound_of(&err), Some(ControlBoundError::ResponseTooLarge));
    }
}
