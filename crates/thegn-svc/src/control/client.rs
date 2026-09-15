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

use thegn_core::control_wire::{EventDecoder, EventFrame, FeedFilter, PROTO_VERSION};
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
}

fn encoded_issue_path(id: &str, suffix: &str) -> Result<String> {
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
        Self { addr }
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
        let (status, value) = match &self.addr {
            ControlAddr::Unix(sock) => {
                let ep = crate::ipc::IpcEndpoint::for_socket_path(sock);
                let stream = crate::ipc::connect(&ep)
                    .await
                    .with_context(|| format!("connect control endpoint {}", ep.display()))?;
                send_request(stream, method, path, self.token(), body).await?
            }
            ControlAddr::Tcp { addr, .. } => {
                let stream = tokio::net::TcpStream::connect(addr)
                    .await
                    .with_context(|| format!("connect control addr {addr}"))?;
                send_request(stream, method, path, self.token(), body).await?
            }
            ControlAddr::HttpOrigin { origin, token } => {
                send_origin_request(origin, token, method, path, body).await?
            }
        };
        if (200..300).contains(&status) {
            Ok(value)
        } else {
            Err(anyhow::Error::new(request_error(status, &value)))
        }
    }

    /// Generic request for the catalog-driven client (`thegn api call`):
    /// verb → route resolution happens in `routes::api_call_for`; this just
    /// performs it. Method is `GET`/`POST`/`DELETE`.
    pub async fn call_raw(&self, method: &str, path: &str, body: Option<Value>) -> Result<Value> {
        self.request(method, path, body).await
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
            v.get("worktrees").cloned().unwrap_or(Value::Array(vec![])),
        )?)
    }

    pub async fn open(&self, spec: &OpenSpec) -> Result<SessionInfo> {
        let v = self
            .request("POST", "/v1/sessions", Some(serde_json::to_value(spec)?))
            .await?;
        Ok(serde_json::from_value(v)?)
    }

    pub async fn fork(&self, spec: &ForkSpec) -> Result<SessionInfo> {
        let v = self
            .request(
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

    /// Block until `session` reaches `condition` (a JSON `WaitCondition`), or
    /// `timeout_ms` elapses. Returns the `WaitOutcome` JSON (`matched`,
    /// `condition`, `exit_code`).
    pub async fn wait(
        &self,
        session: &str,
        condition: Value,
        timeout_ms: Option<i64>,
    ) -> Result<Value> {
        self.request(
            "POST",
            &format!("/v1/sessions/{session}/wait"),
            Some(json!({ "condition": condition, "timeout_ms": timeout_ms })),
        )
        .await
    }

    /// Split `session`: open a sibling pane running `argv` (empty = a shell) in
    /// direction `dir` (`right`/`down`). Returns the new [`SessionInfo`].
    pub async fn split(&self, session: &str, dir: &str, argv: &[String]) -> Result<SessionInfo> {
        let v = self
            .request(
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
        Ok(serde_json::from_value(
            v.get("prs").cloned().unwrap_or(Value::Array(vec![])),
        )?)
    }

    /// `GET /v1/ci/runs` — cache-first CI run history for a worktree.
    pub async fn ci_runs(&self, worktree: &str, limit: Option<usize>) -> Result<CiRunsReply> {
        let mut path = format!("/v1/ci/runs?worktree={}", query_escape(worktree));
        if let Some(limit) = limit {
            path.push_str(&format!("&limit={limit}"));
        }
        let value = self.request("GET", &path, None).await?;
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
        let value = self.request("GET", &path, None).await?;
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
            value
                .get("rules")
                .cloned()
                .unwrap_or(Value::Array(Vec::new())),
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
            .request(
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
            .request(
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
            .request("POST", "/v1/worktrees", Some(serde_json::to_value(req)?))
            .await?;
        Ok(serde_json::from_value(v)?)
    }

    /// `GET /v1/issues` — tracker issues, filtered by status/limit.
    pub async fn issues_list(
        &self,
        statuses: &[thegn_core::issue::IssueStatus],
        limit: usize,
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
        if !params.is_empty() {
            path.push('?');
            path.push_str(&params.join("&"));
        }
        let v = self.request("GET", &path, None).await?;
        Ok(serde_json::from_value(
            v.get("issues").cloned().unwrap_or(Value::Array(vec![])),
        )?)
    }

    /// `GET /v1/issues/{id}` — one issue with detail/comments.
    pub async fn issue_get(&self, id: &str) -> Result<thegn_core::issue::IssueDetail> {
        let path = encoded_issue_path(id, "")?;
        let v = self.request("GET", &path, None).await?;
        Ok(serde_json::from_value(v)?)
    }

    /// `POST /v1/issues/{id}` — patch an issue; returns the updated issue.
    pub async fn issue_update(
        &self,
        id: &str,
        patch: &thegn_core::issue::IssuePatch,
    ) -> Result<thegn_core::issue::Issue> {
        let path = encoded_issue_path(id, "")?;
        let v = self
            .request("POST", &path, Some(serde_json::to_value(patch)?))
            .await?;
        Ok(serde_json::from_value(v)?)
    }

    /// `POST /v1/issues/{id}/comment` — add a comment.
    pub async fn issue_comment(&self, id: &str, body: &str) -> Result<()> {
        let path = encoded_issue_path(id, "/comment")?;
        self.request("POST", &path, Some(json!({ "body": body })))
            .await
            .map(|_| ())
    }

    /// `GET /v1/dispatches` — the durable dispatch roster.
    pub async fn dispatches_list(&self) -> Result<Vec<thegn_core::issue::AgentDispatch>> {
        let v = self.request("GET", "/v1/dispatches", None).await?;
        Ok(serde_json::from_value(
            v.get("dispatches").cloned().unwrap_or(Value::Array(vec![])),
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
        let (frame_tx, frame_rx) = tokio_mpsc::channel::<EventFrame>(256);
        let (ctrl_tx, ctrl_rx) = tokio_mpsc::channel::<AttachControl>(1);
        match &self.addr {
            ControlAddr::Unix(sock) => {
                let ep = crate::ipc::IpcEndpoint::for_socket_path(sock);
                let stream = crate::ipc::connect(&ep)
                    .await
                    .with_context(|| format!("connect control endpoint {}", ep.display()))?;
                let (ws, _) = tokio_tungstenite::client_async(req, stream)
                    .await
                    .context("events websocket handshake")?;
                start_attach(ws, frame_tx, ctrl_rx).await?;
            }
            ControlAddr::Tcp { addr, .. } => {
                let stream = tokio::net::TcpStream::connect(addr)
                    .await
                    .with_context(|| format!("connect control addr {addr}"))?;
                let (ws, _) = tokio_tungstenite::client_async(req, stream)
                    .await
                    .context("events websocket handshake")?;
                start_attach(ws, frame_tx, ctrl_rx).await?;
            }
            ControlAddr::HttpOrigin { .. } => {
                let (ws, _) = tokio_tungstenite::connect_async(req)
                    .await
                    .context("events websocket handshake")?;
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

        let (frame_tx, frame_rx) = tokio_mpsc::channel::<EventFrame>(256);
        let (ctrl_tx, ctrl_rx) = tokio_mpsc::channel::<AttachControl>(64);
        match &self.addr {
            ControlAddr::Unix(sock) => {
                let ep = crate::ipc::IpcEndpoint::for_socket_path(sock);
                let stream = crate::ipc::connect(&ep)
                    .await
                    .with_context(|| format!("connect control endpoint {}", ep.display()))?;
                let (ws, _) = tokio_tungstenite::client_async(req, stream)
                    .await
                    .context("attach websocket handshake")?;
                start_attach(ws, frame_tx, ctrl_rx).await?;
            }
            ControlAddr::Tcp { addr, .. } => {
                let stream = tokio::net::TcpStream::connect(addr)
                    .await
                    .with_context(|| format!("connect control addr {addr}"))?;
                let (ws, _) = tokio_tungstenite::client_async(req, stream)
                    .await
                    .context("attach websocket handshake")?;
                start_attach(ws, frame_tx, ctrl_rx).await?;
            }
            ControlAddr::HttpOrigin { .. } => {
                let (ws, _) = tokio_tungstenite::connect_async(req)
                    .await
                    .context("attach websocket handshake")?;
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

type Ws<S> = tokio_tungstenite::WebSocketStream<S>;

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
    let mut decoder = EventDecoder::new();
    let deadline = tokio::time::Instant::now() + HELLO_TIMEOUT;
    let first = loop {
        let msg = tokio::time::timeout_at(deadline, ws.next())
            .await
            .map_err(|_| anyhow!("pane daemon sent no greeting within {HELLO_TIMEOUT:?}"))?;
        match msg {
            Some(Ok(Message::Binary(bytes))) => {
                decoder.push(&bytes);
                let ready = decoder.drain().map_err(|e| {
                    anyhow!(
                        "undecodable greeting from the pane daemon ({e}) — likely a \
                         protocol-incompatible daemon; restart it (`thegn daemon`) or \
                         quit stale daemons"
                    )
                })?;
                if !ready.is_empty() {
                    break ready;
                }
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
    if let Some(EventFrame::Hello(h)) = first.first()
        && h.proto != PROTO_VERSION
    {
        return Err(anyhow!(
            "pane daemon ({}) speaks control protocol v{}, this thegn speaks v{PROTO_VERSION} — \
             restart the daemon (`thegn daemon`) or quit stale daemons",
            h.server,
            h.proto,
        ));
    }
    for f in first {
        // The channel is fresh (cap 256); the greeting burst always fits.
        let _ = frames.send(f).await; // best-effort: fresh channel always fits (see above)
    }
    tokio::spawn(pump_attach_inner(ws, decoder, frames, ctrl));
    Ok(())
}

async fn pump_attach_inner<S>(
    mut ws: Ws<S>,
    mut decoder: EventDecoder,
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
    let raw_host = url.host_str().context("control HTTP origin has no host")?;
    let host = if raw_host.contains(':') {
        format!("[{raw_host}]")
    } else {
        raw_host.to_string()
    };
    Ok(match url.port() {
        Some(port) => format!("{host}:{port}"),
        None => host,
    })
}

fn websocket_url(origin: &str, path: &str) -> Result<String> {
    let mut url = origin_request_url(origin, path)?;
    let scheme = if url.scheme() == "https" { "wss" } else { "ws" };
    url.set_scheme(scheme)
        .map_err(|()| anyhow!("could not derive WebSocket control origin"))?;
    Ok(url.to_string())
}

/// Send one request to the client-facing HTTP(S) origin. reqwest supplies the
/// normal WebPKI verification path for `https`; TLS termination remains outside
/// thegn's plaintext loopback backend.
async fn send_origin_request(
    origin: &str,
    token: &str,
    method: &str,
    path: &str,
    body: Option<Value>,
) -> Result<(u16, Value)> {
    let method = reqwest::Method::from_bytes(method.as_bytes())
        .with_context(|| format!("invalid control HTTP method {method:?}"))?;
    let url = origin_request_url(origin, path)?;
    let client = reqwest::Client::new();
    let mut request = client.request(method, url).bearer_auth(token);
    if let Some(body) = body {
        request = request.json(&body);
    }
    let response = request.send().await.context("control HTTP request")?;
    let status = response.status().as_u16();
    let bytes = response
        .bytes()
        .await
        .context("control HTTP response body")?;
    let value = if bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&bytes).unwrap_or(Value::Null)
    };
    Ok((status, value))
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
        let _ = conn.await; // best-effort: conn error surfaces via the request path
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

#[cfg(test)]
mod tests {
    use super::*;
    use thegn_core::db::Db;

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
}
