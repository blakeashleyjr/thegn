//! Vercel Sandbox REST shapes: request bodies, response parsing, the NDJSON
//! log line and the `/interactive` WebSocket frames. Pure; no I/O.

use std::collections::BTreeMap;
use std::fmt;

use serde::Serialize;

use crate::vercel::policy::CreatePlan;

/// The documented control-plane base.
pub const DEFAULT_API_BASE: &str = "https://api.vercel.com";

/// The team/project a sandbox lives in. Not secrets.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Scope {
    pub team_id: String,
    pub project_id: String,
}

/// `resources` in a create body.
#[derive(Debug, Clone, Serialize)]
pub struct Resources {
    pub vcpus: u32,
}

/// The `POST /v2/sandboxes` body.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CreateRequest {
    pub project_id: String,
    pub name: String,
    pub timeout: u64,
    pub resources: Resources,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub image: Option<String>,
    pub persistent: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub region: Option<String>,
    pub tags: BTreeMap<String, String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub network_policy: Option<serde_json::Value>,
}

/// One sandbox as the API reports it, typed where thegn needs it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SandboxView {
    pub name: String,
    pub status: SessionStatus,
    pub session_id: Option<String>,
    pub vcpus: Option<u32>,
    pub region: Option<String>,
    pub created_at_ms: Option<i64>,
    pub tags: BTreeMap<String, String>,
    pub cwd: Option<String>,
}

/// A session's status.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionStatus {
    Pending,
    Running,
    Stopping,
    Stopped,
    Failed,
    Aborted,
    Snapshotting,
    Unknown,
}

/// One NDJSON line of a command's log stream.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LogLine {
    Command { id: String, exit_code: Option<i32> },
    Stdout(Vec<u8>),
    Stderr(Vec<u8>),
    Error { code: String, message: String },
}

/// A text control frame from the `/interactive` WebSocket.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WsControl {
    Exit(Option<i32>),
    Output(String),
}

/// Why a body/response/URL was refused (fixed reason, never a value).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApiParseError(pub &'static str);

impl fmt::Display for ApiParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.0)
    }
}

impl std::error::Error for ApiParseError {}

const NOT_BUILT: ApiParseError = ApiParseError("vercel: not implemented in this lane");

/// The create body for `req` in `scope`. Spine stub, filled by lane 2.
pub fn create_body(_scope: &Scope, _req: &CreatePlan) -> serde_json::Value {
    serde_json::Value::Null
}

/// Parse one sandbox object. Spine stub, filled by lane 2.
pub fn parse_sandbox(_v: &serde_json::Value) -> Result<SandboxView, ApiParseError> {
    Err(NOT_BUILT)
}

/// Parse one list page: the sandboxes and the next cursor. Spine stub, filled
/// by lane 2.
pub fn parse_list_page(
    _v: &serde_json::Value,
) -> Result<(Vec<SandboxView>, Option<String>), ApiParseError> {
    Err(NOT_BUILT)
}

/// Parse one NDJSON log line. Spine stub, filled by lane 2.
pub fn parse_log_line(_line: &[u8]) -> Result<LogLine, ApiParseError> {
    Err(NOT_BUILT)
}

/// The run-command body. Spine stub, filled by lane 2.
pub fn command_body(
    _argv: &[String],
    _cwd: Option<&str>,
    _env: &[(String, String)],
    _wait: bool,
) -> Result<serde_json::Value, ApiParseError> {
    Err(NOT_BUILT)
}

/// The `/interactive` start frame. Spine stub, filled by lane 2.
pub fn interactive_start_frame(
    _argv: &[String],
    _cwd: Option<&str>,
    _env: &[(String, String)],
    _cols: u16,
    _rows: u16,
) -> Result<String, ApiParseError> {
    Err(NOT_BUILT)
}

/// The `/interactive` resize frame. Spine stub, filled by lane 2.
pub fn resize_frame(_cols: u16, _rows: u16) -> String {
    String::new()
}

/// Classify a `/interactive` text frame. Spine stub, filled by lane 2.
pub fn parse_ws_control(text: &str) -> WsControl {
    WsControl::Output(text.to_string())
}

/// The user-defined network policy body (`allow` list; empty ⇒ `allow-all`).
/// Spine stub, filled by lane 2.
pub fn network_policy_body(_domains: &[String]) -> serde_json::Value {
    serde_json::Value::Null
}

/// `{base}/…/sandboxes/{name}` with the scope query. Spine stub, filled by
/// lane 2.
pub fn sandbox_url(_base: &str, _scope: &Scope, _name: &str) -> Result<String, ApiParseError> {
    Err(NOT_BUILT)
}

/// `{base}/…/sessions/{session_id}{tail}` with the scope query. Spine stub,
/// filled by lane 2.
pub fn session_url(
    _base: &str,
    _scope: &Scope,
    _session_id: &str,
    _tail: &str,
) -> Result<String, ApiParseError> {
    Err(NOT_BUILT)
}
