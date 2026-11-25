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

/// The longest NDJSON line the caller may buffer (the caller enforces it).
pub const MAX_LOG_LINE_BYTES: usize = 1024 * 1024;

fn bad(reason: &'static str) -> ApiParseError {
    ApiParseError(reason)
}

/// The create body for `req` in `scope`. Never emits `source`, `credentials`,
/// `env` or `ports`; always `persistent: true`; always carries a
/// `networkPolicy` (an empty egress list is `deny-all`).
pub fn create_body(scope: &Scope, req: &CreatePlan) -> serde_json::Value {
    let body = CreateRequest {
        project_id: scope.project_id.clone(),
        name: req.name.clone(),
        timeout: req.timeout_ms,
        resources: Resources { vcpus: req.vcpus },
        image: req.image.clone(),
        persistent: true,
        region: req.region.clone(),
        tags: req.tags.clone(),
        network_policy: Some(network_policy_body(&req.egress)),
    };
    serde_json::to_value(body).unwrap_or(serde_json::Value::Null)
}

/// Case- and whitespace-insensitive: the live casing is UNVERIFIED (THE-2234),
/// and an unknown status disables extension and confuses the reaper.
fn status_of(s: &str) -> SessionStatus {
    match s.trim().to_ascii_lowercase().as_str() {
        "pending" => SessionStatus::Pending,
        "running" => SessionStatus::Running,
        "stopping" => SessionStatus::Stopping,
        "stopped" => SessionStatus::Stopped,
        "failed" => SessionStatus::Failed,
        "aborted" => SessionStatus::Aborted,
        "snapshotting" => SessionStatus::Snapshotting,
        _ => SessionStatus::Unknown,
    }
}

fn str_of(v: &serde_json::Value, key: &str) -> Option<String> {
    v.get(key)
        .and_then(|x| x.as_str())
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

/// Parse one sandbox object (bare, or wrapped as `{"sandbox": …}`). Tolerant
/// on optional fields; a missing `name` is an error.
pub fn parse_sandbox(v: &serde_json::Value) -> Result<SandboxView, ApiParseError> {
    let o = v.get("sandbox").filter(|s| s.is_object()).unwrap_or(v);
    if !o.is_object() {
        return Err(bad("vercel: sandbox is not an object"));
    }
    let name = str_of(o, "name").ok_or_else(|| bad("vercel: sandbox has no name"))?;
    let session = o
        .get("session")
        .or_else(|| v.get("session"))
        .filter(|s| s.is_object());
    let status_text = session
        .and_then(|s| str_of(s, "status"))
        .or_else(|| str_of(o, "status"));
    let session_id = str_of(o, "currentSessionId")
        .or_else(|| str_of(o, "sessionId"))
        .or_else(|| session.and_then(|s| str_of(s, "id")));
    let vcpus = o
        .get("resources")
        .and_then(|r| r.get("vcpus"))
        .or_else(|| o.get("vcpus"))
        .and_then(|x| x.as_u64())
        .and_then(|n| u32::try_from(n).ok());
    let tags = o
        .get("tags")
        .and_then(|t| t.as_object())
        .map(|m| {
            m.iter()
                .filter_map(|(k, v)| v.as_str().map(|v| (k.clone(), v.to_string())))
                .collect()
        })
        .unwrap_or_default();
    Ok(SandboxView {
        name,
        status: status_text
            .as_deref()
            .map(status_of)
            .unwrap_or(SessionStatus::Unknown),
        session_id,
        vcpus,
        region: str_of(o, "region"),
        created_at_ms: o.get("createdAt").and_then(|x| x.as_i64()),
        tags,
        cwd: str_of(o, "cwd"),
    })
}

/// Parse one list page: the sandboxes and the next cursor. An element without
/// a name fails the page (identity is strict).
pub fn parse_list_page(
    v: &serde_json::Value,
) -> Result<(Vec<SandboxView>, Option<String>), ApiParseError> {
    let arr = v
        .get("sandboxes")
        .and_then(|x| x.as_array())
        .ok_or_else(|| bad("vercel: list page has no sandboxes array"))?;
    let items = arr
        .iter()
        .map(parse_sandbox)
        .collect::<Result<Vec<_>, _>>()?;
    let next = v
        .get("pagination")
        .and_then(|p| p.get("next"))
        .and_then(|n| match n {
            serde_json::Value::String(s) if !s.is_empty() => Some(s.clone()),
            serde_json::Value::Number(n) => Some(n.to_string()),
            _ => None,
        });
    Ok((items, next))
}

/// Parse one NDJSON log line (`data` is UTF-8 text, not base64).
pub fn parse_log_line(line: &[u8]) -> Result<LogLine, ApiParseError> {
    if line.len() > MAX_LOG_LINE_BYTES {
        return Err(bad("vercel: log line too long"));
    }
    let v: serde_json::Value =
        serde_json::from_slice(line).map_err(|_| bad("vercel: malformed log line"))?;
    if !v.is_object() {
        return Err(bad("vercel: malformed log line"));
    }
    match v.get("stream").and_then(|s| s.as_str()) {
        Some("stdout") => Ok(LogLine::Stdout(data_bytes(&v)?)),
        Some("stderr") => Ok(LogLine::Stderr(data_bytes(&v)?)),
        Some("error") => {
            let d = v.get("data").unwrap_or(&serde_json::Value::Null);
            Ok(LogLine::Error {
                code: str_of(d, "code").unwrap_or_default(),
                message: str_of(d, "message").unwrap_or_default(),
            })
        }
        Some(_) => Err(bad("vercel: unknown log stream")),
        None => {
            let c = v.get("command").unwrap_or(&v);
            let id = str_of(c, "id").ok_or_else(|| bad("vercel: unrecognised log line"))?;
            let exit_code = c
                .get("exitCode")
                .and_then(|x| x.as_i64())
                .and_then(|n| i32::try_from(n).ok());
            Ok(LogLine::Command { id, exit_code })
        }
    }
}

fn data_bytes(v: &serde_json::Value) -> Result<Vec<u8>, ApiParseError> {
    v.get("data")
        .and_then(|d| d.as_str())
        .map(|s| s.as_bytes().to_vec())
        .ok_or_else(|| bad("vercel: log line has no text data"))
}

fn has_nul(s: &str) -> bool {
    s.contains('\0')
}

fn valid_env_key(k: &str) -> bool {
    let b = k.as_bytes();
    !b.is_empty()
        && (b[0].is_ascii_uppercase() || b[0] == b'_')
        && b.iter()
            .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || *c == b'_')
}

/// Validate argv/cwd/env and return the shared JSON fragments.
fn command_parts(
    argv: &[String],
    cwd: Option<&str>,
    env: &[(String, String)],
) -> Result<serde_json::Map<String, serde_json::Value>, ApiParseError> {
    let (cmd, args) = argv
        .split_first()
        .ok_or_else(|| bad("vercel: empty argv"))?;
    if argv.iter().any(|a| has_nul(a)) || cwd.is_some_and(has_nul) {
        return Err(bad("vercel: NUL in command"));
    }
    let mut m = serde_json::Map::new();
    m.insert("command".into(), cmd.clone().into());
    m.insert("args".into(), serde_json::json!(args));
    if let Some(cwd) = cwd.filter(|c| !c.is_empty()) {
        m.insert("cwd".into(), cwd.into());
    }
    if !env.is_empty() {
        let mut e = serde_json::Map::new();
        for (k, v) in env {
            if !valid_env_key(k) {
                return Err(bad("vercel: invalid env key"));
            }
            if has_nul(v) {
                return Err(bad("vercel: NUL in command"));
            }
            e.insert(k.clone(), v.clone().into());
        }
        m.insert("env".into(), e.into());
    }
    Ok(m)
}

/// The run-command body.
pub fn command_body(
    argv: &[String],
    cwd: Option<&str>,
    env: &[(String, String)],
    wait: bool,
) -> Result<serde_json::Value, ApiParseError> {
    let mut m = command_parts(argv, cwd, env)?;
    m.insert("wait".into(), wait.into());
    if wait {
        m.insert("logs".into(), true.into());
    }
    Ok(m.into())
}

/// Mark a REST command body for stdin attachment. Non-object values are
/// returned unchanged.
pub fn with_attach_stdin(mut body: serde_json::Value) -> serde_json::Value {
    if let serde_json::Value::Object(ref mut object) = body {
        object.insert("attachStdin".into(), true.into());
    }
    body
}

/// The `/interactive` start frame.
pub fn interactive_start_frame(
    argv: &[String],
    cwd: Option<&str>,
    env: &[(String, String)],
    cols: u16,
    rows: u16,
) -> Result<String, ApiParseError> {
    let mut m = command_parts(argv, cwd, env)?;
    if !env.is_empty() {
        let mut sorted_env = env.iter().collect::<Vec<_>>();
        sorted_env.sort_by(|(left, _), (right, _)| left.cmp(right));
        m.insert(
            "env".into(),
            sorted_env
                .into_iter()
                .map(|(key, value)| format!("{key}={value}"))
                .collect::<Vec<_>>()
                .into(),
        );
    }
    m.insert("type".into(), "start".into());
    m.insert("cols".into(), cols.into());
    m.insert("rows".into(), rows.into());
    Ok(serde_json::Value::Object(m).to_string())
}

/// The `/interactive` resize frame.
pub fn resize_frame(cols: u16, rows: u16) -> String {
    serde_json::json!({"type": "resize", "cols": cols, "rows": rows}).to_string()
}

/// Classify a `/interactive` text frame: a valid exit frame, else output.
pub fn parse_ws_control(text: &str) -> WsControl {
    if let Ok(v) = serde_json::from_str::<serde_json::Value>(text)
        && v.get("type").and_then(|t| t.as_str()) == Some("exit")
    {
        let code = v
            .get("code")
            .and_then(|c| c.as_i64())
            .and_then(|n| i32::try_from(n).ok());
        return WsControl::Exit(code);
    }
    WsControl::Output(text.to_string())
}

/// The network policy body. An empty allow list fails closed as `deny-all`.
pub fn network_policy_body(domains: &[String]) -> serde_json::Value {
    if domains.is_empty() {
        serde_json::json!({"mode": "deny-all"})
    } else if domains.len() == 1 && domains[0] == "*" {
        serde_json::json!({"mode": "allow-all"})
    } else {
        serde_json::json!({"mode": "custom", "allowedDomains": domains})
    }
}

fn checked_base(base: &str) -> Result<&str, ApiParseError> {
    let base = base.trim_end_matches('/');
    let rest = base
        .strip_prefix("https://")
        .ok_or_else(|| bad("vercel: api base must be https"))?;
    if rest.is_empty()
        || rest
            .bytes()
            .any(|b| b.is_ascii_whitespace() || b.is_ascii_control() || b == b'?' || b == b'#')
    {
        return Err(bad("vercel: invalid api base"));
    }
    Ok(base)
}

fn query(scope: &Scope) -> Result<String, ApiParseError> {
    use crate::vercel::admit::{valid_project_id, valid_team_id};
    if !valid_team_id(&scope.team_id) || !valid_project_id(&scope.project_id) {
        return Err(bad("vercel: invalid scope"));
    }
    Ok(format!(
        "teamId={}&projectId={}",
        scope.team_id, scope.project_id
    ))
}

/// `{base}/v2/sandboxes/{name}` with the scope query.
pub fn sandbox_url(base: &str, scope: &Scope, name: &str) -> Result<String, ApiParseError> {
    let base = checked_base(base)?;
    if !crate::vercel::admit::valid_sandbox_name(name) {
        return Err(bad("vercel: invalid sandbox name"));
    }
    Ok(format!("{base}/v2/sandboxes/{name}?{}", query(scope)?))
}

/// `{base}/v2/sandboxes/sessions/{session_id}{tail}` with the scope query.
pub fn session_url(
    base: &str,
    scope: &Scope,
    session_id: &str,
    tail: &str,
) -> Result<String, ApiParseError> {
    let base = checked_base(base)?;
    if session_id.is_empty()
        || session_id.len() > 128
        || !session_id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
    {
        return Err(bad("vercel: invalid session id"));
    }
    let tail_ok = tail.is_empty()
        || (tail.starts_with('/')
            && !tail.contains("..")
            && !tail.bytes().any(|b| {
                b == b'?'
                    || b == b'#'
                    || b == b'\\'
                    || b.is_ascii_whitespace()
                    || b.is_ascii_control()
            }));
    if !tail_ok {
        return Err(bad("vercel: invalid url tail"));
    }
    Ok(format!(
        "{base}/v2/sandboxes/sessions/{session_id}{tail}?{}",
        query(scope)?
    ))
}

/// Built-in interactive-host suffixes (all Vercel-owned; never `vercel.app`).
pub const DEFAULT_INTERACTIVE_HOST_SUFFIXES: &[&str] = &["vercel.run", "vercel.sh", "vercel.com"];

/// Lowercase `name` with at most one trailing dot removed, or `None` when it
/// is empty or carries anything but `[A-Za-z0-9.-]` (so a port, userinfo,
/// brackets, whitespace or a percent-escape never reaches the comparison).
fn dns_name_lower(name: &str) -> Option<String> {
    let name = name.strip_suffix('.').unwrap_or(name);
    (!name.is_empty()
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'.' || b == b'-'))
    .then(|| name.to_ascii_lowercase())
}

/// Whether `host` may carry the interactive WebSocket. The single
/// implementation of the ticket host pin (the svc client calls it after its
/// own scheme/userinfo/IP admission of the ticket URL).
///
/// `host` must be a DNS name (IP literals, brackets, userinfo, ports and empty
/// names are refused) that equals one of `suffixes` or ends in `"." + suffix`,
/// case-insensitively, tolerating one trailing dot on either side. Empty
/// `suffixes` means [`DEFAULT_INTERACTIVE_HOST_SUFFIXES`]. `vercel.app` and
/// names under it are refused whatever `suffixes` says (anyone can deploy
/// there).
pub fn interactive_host_allowed(host: &str, suffixes: &[String]) -> bool {
    let Some(h) = dns_name_lower(host) else {
        return false;
    };
    if h.parse::<std::net::IpAddr>().is_ok() || crate::vercel::admit::under_untrusted_suffix(&h) {
        return false;
    }
    let matches = |s: &str| {
        dns_name_lower(s).is_some_and(|s| {
            !crate::vercel::admit::under_untrusted_suffix(&s)
                && (h == s
                    || h.strip_suffix(s.as_str())
                        .is_some_and(|rest| rest.ends_with('.')))
        })
    };
    if suffixes.is_empty() {
        DEFAULT_INTERACTIVE_HOST_SUFFIXES.iter().any(|s| matches(s))
    } else {
        suffixes.iter().any(|s| matches(s))
    }
}

/// Parse the current session's usage from a `GET /v2/sandboxes/{name}`
/// response (`{"sandbox": …, "session": …}`, or a bare sandbox carrying its
/// own `session`). `Ok(None)` when there is no `session` object. The
/// timestamps are read from the session and fall back to the sandbox;
/// `vcpus`/`memory` fall back further to the sandbox's `resources`; every
/// field is optional. CPU is session-only: it comes from
/// `session.activeCpuDurationMs` and never from the sandbox, whose total is
/// cumulative across sessions, so a missing session CPU figure reads as
/// unavailable (`None`, which [`crate::vercel::usage::session_accrual`] prices
/// at zero). A non-object response is an error.
pub fn parse_session_usage(
    v: &serde_json::Value,
) -> Result<Option<crate::vercel::usage::SessionUsage>, ApiParseError> {
    let sandbox = v.get("sandbox").filter(|s| s.is_object()).unwrap_or(v);
    if !sandbox.is_object() {
        return Err(bad("vercel: usage response has no sandbox object"));
    }
    let Some(session) = sandbox
        .get("session")
        .or_else(|| v.get("session"))
        .filter(|s| s.is_object())
    else {
        return Ok(None);
    };
    let field = |key: &str| {
        session
            .get(key)
            .filter(|x| !x.is_null())
            .or_else(|| sandbox.get(key).filter(|x| !x.is_null()))
    };
    let resource = |key: &str| {
        field(key).or_else(|| {
            sandbox
                .get("resources")
                .and_then(|r| r.get(key))
                .filter(|x| !x.is_null())
        })
    };
    let active_cpu_ms = session.get("activeCpuDurationMs").and_then(|x| x.as_u64());
    Ok(Some(crate::vercel::usage::SessionUsage {
        active_cpu_ms,
        memory_mb: resource("memory").and_then(|x| x.as_u64()),
        vcpus: resource("vcpus")
            .and_then(|x| x.as_u64())
            .and_then(|n| u32::try_from(n).ok()),
        started_at_ms: field("startedAt").and_then(|x| x.as_i64()),
        stopped_at_ms: field("stoppedAt").and_then(|x| x.as_i64()),
    }))
}

/// Snapshot ids: `[A-Za-z0-9_-]{1,128}` (they become URL path segments).
fn valid_snapshot_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 128
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}

/// Cap on snapshots read from one response.
pub const MAX_SNAPSHOTS: usize = 1_000;

/// Parse a snapshots page: `snapshots[]` (capped at [`MAX_SNAPSHOTS`]) and
/// `pagination.next`. Elements without a valid id are skipped.
pub fn parse_snapshots(
    v: &serde_json::Value,
) -> Result<(Vec<crate::vercel::usage::SnapshotUsage>, Option<String>), ApiParseError> {
    let arr = v
        .get("snapshots")
        .and_then(|x| x.as_array())
        .ok_or_else(|| bad("vercel: snapshots page has no snapshots array"))?;
    let items = arr
        .iter()
        .filter_map(|o| {
            Some(crate::vercel::usage::SnapshotUsage {
                id: str_of(o, "id").filter(|id| valid_snapshot_id(id))?,
                size_bytes: o.get("sizeBytes").and_then(|x| x.as_u64()).unwrap_or(0),
                expires_at_ms: o.get("expiresAt").and_then(|x| x.as_i64()),
                status: str_of(o, "status").unwrap_or_default(),
            })
        })
        .take(MAX_SNAPSHOTS)
        .collect();
    let next = v
        .get("pagination")
        .and_then(|p| p.get("next"))
        .and_then(|n| match n {
            serde_json::Value::String(s) if !s.is_empty() => Some(s.clone()),
            serde_json::Value::Number(n) => Some(n.to_string()),
            _ => None,
        });
    Ok((items, next))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn scope() -> Scope {
        Scope {
            team_id: "team_a1".into(),
            project_id: "prj_b2".into(),
        }
    }

    fn plan() -> CreatePlan {
        CreatePlan {
            name: "tg-x".into(),
            vcpus: 2,
            image: None,
            region: None,
            timeout_ms: 1_800_000,
            tags: BTreeMap::from([("thegn-owner".into(), "x".into())]),
            egress: vec![],
        }
    }

    fn s(v: &[&str]) -> Vec<String> {
        v.iter().map(|x| x.to_string()).collect()
    }

    #[test]
    fn create_body_golden_minimal() {
        assert_eq!(
            create_body(&scope(), &plan()),
            json!({
                "projectId": "prj_b2", "name": "tg-x", "timeout": 1800000,
                "resources": {"vcpus": 2}, "persistent": true,
                "tags": {"thegn-owner": "x"},
                "networkPolicy": {"mode": "deny-all"}
            })
        );
    }

    #[test]
    fn create_body_golden_full_and_never_secrets() {
        let mut p = plan();
        p.image = Some("node24".into());
        p.region = Some("iad1".into());
        p.egress = s(&["github.com"]);
        let v = create_body(&scope(), &p);
        assert_eq!(
            v,
            json!({
                "projectId": "prj_b2", "name": "tg-x", "timeout": 1800000,
                "resources": {"vcpus": 2}, "persistent": true,
                "image": "node24", "region": "iad1",
                "tags": {"thegn-owner": "x"},
                "networkPolicy": {"mode": "custom", "allowedDomains": ["github.com"]}
            })
        );
        for k in ["source", "credentials", "env", "ports"] {
            assert!(v.get(k).is_none(), "{k}");
        }
    }

    #[test]
    fn network_policy_golden() {
        assert_eq!(network_policy_body(&[]), json!({"mode": "deny-all"}));
        assert_eq!(
            network_policy_body(&s(&["*"])),
            json!({"mode": "allow-all"})
        );
        assert_eq!(
            network_policy_body(&s(&["a.com", "b.com"])),
            json!({"mode": "custom", "allowedDomains": ["a.com", "b.com"]})
        );
    }

    #[test]
    fn command_body_golden() {
        let v = command_body(
            &s(&["git", "status"]),
            Some("/work"),
            &[("FOO_1".into(), "bar".into())],
            true,
        )
        .unwrap();
        assert_eq!(
            v,
            json!({"command": "git", "args": ["status"], "cwd": "/work",
                   "env": {"FOO_1": "bar"}, "wait": true, "logs": true})
        );
        let v = command_body(&s(&["ls"]), None, &[], false).unwrap();
        assert_eq!(v, json!({"command": "ls", "args": [], "wait": false}));
        assert!(v.get("logs").is_none());
        let v = command_body(&s(&["ls"]), Some(""), &[], false).unwrap();
        assert!(v.get("cwd").is_none());
    }

    #[test]
    fn command_body_attach_stdin() {
        assert_eq!(
            with_attach_stdin(json!({"command": "cat"})),
            json!({"command": "cat", "attachStdin": true})
        );
        let detached = with_attach_stdin(command_body(&s(&["cat"]), None, &[], false).unwrap());
        assert_eq!(detached.get("attachStdin"), Some(&json!(true)));
        assert!(detached.get("logs").is_none());
        assert_eq!(
            with_attach_stdin(json!("not an object")),
            json!("not an object")
        );
    }

    #[test]
    fn command_body_refusals() {
        assert!(command_body(&[], None, &[], true).is_err());
        for k in ["foo", "1A", "A-B", "", "A B"] {
            assert!(
                command_body(&s(&["x"]), None, &[(k.into(), "v".into())], true).is_err(),
                "{k}"
            );
        }
        assert!(command_body(&s(&["a\0"]), None, &[], true).is_err());
        assert!(command_body(&s(&["a"]), Some("/w\0"), &[], true).is_err());
        assert!(command_body(&s(&["a"]), None, &[("A".into(), "v\0".into())], true).is_err());
        assert!(command_body(&s(&["a"]), None, &[("_A9".into(), "v".into())], true).is_ok());
    }

    #[test]
    fn interactive_frames_golden() {
        let f = interactive_start_frame(&s(&["bash", "-l"]), Some("/w"), &[], 80, 24).unwrap();
        let v: serde_json::Value = serde_json::from_str(&f).unwrap();
        assert_eq!(
            v,
            json!({"type": "start", "command": "bash", "args": ["-l"], "cwd": "/w",
                   "cols": 80, "rows": 24})
        );
        let f = interactive_start_frame(
            &s(&["env"]),
            None,
            &[
                ("Z_LAST".into(), "z".into()),
                ("A_FIRST".into(), "a".into()),
            ],
            80,
            24,
        )
        .unwrap();
        let v: serde_json::Value = serde_json::from_str(&f).unwrap();
        assert_eq!(v.get("env"), Some(&json!(["A_FIRST=a", "Z_LAST=z"])));
        let rest = command_body(
            &s(&["env"]),
            None,
            &[
                ("Z_LAST".into(), "z".into()),
                ("A_FIRST".into(), "a".into()),
            ],
            false,
        )
        .unwrap();
        assert_eq!(
            rest.get("env"),
            Some(&json!({"Z_LAST": "z", "A_FIRST": "a"}))
        );
        assert!(interactive_start_frame(&[], None, &[], 1, 1).is_err());
        let r: serde_json::Value = serde_json::from_str(&resize_frame(100, 30)).unwrap();
        assert_eq!(r, json!({"type": "resize", "cols": 100, "rows": 30}));
    }

    #[test]
    fn ndjson_fixtures() {
        assert_eq!(
            parse_log_line(br#"{"stream":"stdout","data":"hi\n"}"#).unwrap(),
            LogLine::Stdout(b"hi\n".to_vec())
        );
        assert_eq!(
            parse_log_line(r#"{"stream":"stderr","data":"é"}"#.as_bytes()).unwrap(),
            LogLine::Stderr("é".as_bytes().to_vec())
        );
        assert_eq!(
            parse_log_line(br#"{"stream":"error","data":{"code":"x","message":"boom"}}"#).unwrap(),
            LogLine::Error {
                code: "x".into(),
                message: "boom".into()
            }
        );
        assert_eq!(
            parse_log_line(br#"{"stream":"error","data":null}"#).unwrap(),
            LogLine::Error {
                code: String::new(),
                message: String::new()
            }
        );
        assert_eq!(
            parse_log_line(br#"{"command":{"id":"cmd_1","exitCode":3}}"#).unwrap(),
            LogLine::Command {
                id: "cmd_1".into(),
                exit_code: Some(3)
            }
        );
        assert_eq!(
            parse_log_line(br#"{"id":"cmd_2"}"#).unwrap(),
            LogLine::Command {
                id: "cmd_2".into(),
                exit_code: None
            }
        );
        // base64 is not decoded.
        assert_eq!(
            parse_log_line(br#"{"stream":"stdout","data":"aGk="}"#).unwrap(),
            LogLine::Stdout(b"aGk=".to_vec())
        );
    }

    #[test]
    fn ndjson_rejects_bad_lines() {
        let truncated = br#"{"stream":"stdout","da"#;
        assert!(parse_log_line(truncated).is_err());
        assert!(parse_log_line(b"\xff\xfe garbage").is_err());
        assert!(parse_log_line(b"[1,2]").is_err());
        assert!(parse_log_line(br#"{"stream":"weird","data":"x"}"#).is_err());
        assert!(parse_log_line(br#"{"stream":"stdout"}"#).is_err());
        assert!(parse_log_line(br#"{"nothing":1}"#).is_err());
        let huge = vec![b' '; MAX_LOG_LINE_BYTES + 1];
        assert!(parse_log_line(&huge).is_err());
    }

    #[test]
    fn ws_control() {
        assert_eq!(
            parse_ws_control(r#"{"type":"exit","code":7}"#),
            WsControl::Exit(Some(7))
        );
        assert_eq!(
            parse_ws_control(r#"{"type":"exit","code":"x"}"#),
            WsControl::Exit(None)
        );
        assert_eq!(
            parse_ws_control(r#"{"type":"exit"}"#),
            WsControl::Exit(None)
        );
        for t in [r#"{"type":"data"}"#, "plain text", "{bad", "42"] {
            assert_eq!(parse_ws_control(t), WsControl::Output(t.to_string()));
        }
    }

    #[test]
    fn sandbox_parsing() {
        let v = json!({
            "name": "tg-x", "status": "running", "currentSessionId": "sess_1",
            "resources": {"vcpus": 4}, "region": "iad1", "createdAt": 1700000000000i64,
            "tags": {"a": "b", "n": 1}, "cwd": "/home/u"
        });
        let sb = parse_sandbox(&v).unwrap();
        assert_eq!(sb.name, "tg-x");
        assert_eq!(sb.status, SessionStatus::Running);
        assert_eq!(sb.session_id.as_deref(), Some("sess_1"));
        assert_eq!(sb.vcpus, Some(4));
        assert_eq!(sb.region.as_deref(), Some("iad1"));
        assert_eq!(sb.created_at_ms, Some(1_700_000_000_000));
        assert_eq!(sb.tags.len(), 1);
        assert_eq!(sb.cwd.as_deref(), Some("/home/u"));
        // wrapped, with the session object carrying status/id
        let w = json!({"sandbox": {"name": "n", "vcpus": 2},
                       "session": {"id": "s2", "status": "stopped"}});
        let sb = parse_sandbox(&w).unwrap();
        assert_eq!(sb.status, SessionStatus::Stopped);
        assert_eq!(sb.session_id.as_deref(), Some("s2"));
        assert_eq!(sb.vcpus, Some(2));
        // minimal: tolerant
        let sb = parse_sandbox(&json!({"name": "m"})).unwrap();
        assert_eq!(sb.status, SessionStatus::Unknown);
        assert!(sb.tags.is_empty() && sb.session_id.is_none());
    }

    #[test]
    fn session_status_takes_precedence_over_sandbox_status() {
        let pending = parse_sandbox(&json!({
            "sandbox": {"name": "n", "status": "running"},
            "session": {"status": "pending"}
        }))
        .unwrap();
        assert_eq!(pending.status, SessionStatus::Pending);
        let failed = parse_sandbox(&json!({
            "sandbox": {"name": "n", "status": "running"},
            "session": {"status": "failed"}
        }))
        .unwrap();
        assert_eq!(failed.status, SessionStatus::Failed);
        let stopped = parse_sandbox(&json!({"name": "n", "status": "stopped"})).unwrap();
        assert_eq!(stopped.status, SessionStatus::Stopped);
        let capitalized = parse_sandbox(&json!({
            "sandbox": {"name": "n", "status": "RUNNING"},
            "session": {"status": "PeNdInG"}
        }))
        .unwrap();
        assert_eq!(capitalized.status, SessionStatus::Pending);
    }

    #[test]
    fn sandbox_identity_is_strict() {
        assert!(parse_sandbox(&json!({"status": "running"})).is_err());
        assert!(parse_sandbox(&json!({"name": ""})).is_err());
        assert!(parse_sandbox(&json!({"name": 5})).is_err());
        assert!(parse_sandbox(&json!("str")).is_err());
    }

    #[test]
    fn every_status_maps() {
        for (t, st) in [
            ("pending", SessionStatus::Pending),
            ("running", SessionStatus::Running),
            ("Running", SessionStatus::Running),
            ("stopping", SessionStatus::Stopping),
            ("stopped", SessionStatus::Stopped),
            (" STOPPED ", SessionStatus::Stopped),
            ("failed", SessionStatus::Failed),
            ("aborted", SessionStatus::Aborted),
            ("snapshotting", SessionStatus::Snapshotting),
            ("Snapshotting", SessionStatus::Snapshotting),
            ("???", SessionStatus::Unknown),
        ] {
            let sb = parse_sandbox(&json!({"name": "n", "status": t})).unwrap();
            assert_eq!(sb.status, st, "{t}");
        }
    }

    #[test]
    fn status_unknown_stays_unknown() {
        let sb = parse_sandbox(&json!({"name": "n", "status": "runningish"})).unwrap();
        assert_eq!(sb.status, SessionStatus::Unknown);
    }

    #[test]
    fn list_pages() {
        let v = json!({"sandboxes": [{"name": "a"}, {"name": "b"}],
                       "pagination": {"next": "cur1"}});
        let (items, next) = parse_list_page(&v).unwrap();
        assert_eq!(items.len(), 2);
        assert_eq!(next.as_deref(), Some("cur1"));
        let v = json!({"sandboxes": [], "pagination": {"next": 1700}});
        assert_eq!(parse_list_page(&v).unwrap().1.as_deref(), Some("1700"));
        let v = json!({"sandboxes": [], "pagination": {"next": null}});
        assert_eq!(parse_list_page(&v).unwrap().1, None);
        assert_eq!(parse_list_page(&json!({"sandboxes": []})).unwrap().1, None);
        assert!(parse_list_page(&json!({})).is_err());
        assert!(parse_list_page(&json!({"sandboxes": [{"x": 1}]})).is_err());
    }

    #[test]
    fn urls() {
        let b = "https://api.vercel.com/";
        assert_eq!(
            sandbox_url(b, &scope(), "tg-x").unwrap(),
            "https://api.vercel.com/v2/sandboxes/tg-x?teamId=team_a1&projectId=prj_b2"
        );
        assert_eq!(
            session_url(b, &scope(), "sess_1", "/cmd").unwrap(),
            "https://api.vercel.com/v2/sandboxes/sessions/sess_1/cmd?teamId=team_a1&projectId=prj_b2"
        );
        assert!(session_url(b, &scope(), "sess_1", "").is_ok());
        assert!(sandbox_url("http://x", &scope(), "a").is_err());
        assert!(sandbox_url("https://", &scope(), "a").is_err());
        assert!(sandbox_url("https://a b", &scope(), "a").is_err());
        assert!(sandbox_url("https://a?x", &scope(), "a").is_err());
        assert!(sandbox_url(b, &scope(), "../x").is_err());
        let bad_scope = Scope {
            team_id: "t&x".into(),
            project_id: "prj_b2".into(),
        };
        assert!(sandbox_url(b, &bad_scope, "a").is_err());
        assert!(session_url(b, &scope(), "", "").is_err());
        assert!(session_url(b, &scope(), "a/b", "").is_err());
        assert!(session_url(b, &scope(), &"a".repeat(128), "").is_ok());
        assert!(session_url(b, &scope(), &"a".repeat(129), "").is_err());
        for t in ["cmd", "/a/../b", "/a?x", "/a#x", "/a b"] {
            assert!(session_url(b, &scope(), "s", t).is_err(), "{t}");
        }
    }

    #[test]
    fn errors_and_defaults() {
        assert_eq!(ApiParseError("x").to_string(), "x");
        assert_eq!(DEFAULT_API_BASE, "https://api.vercel.com");
    }

    #[test]
    fn interactive_host_matching() {
        let none: Vec<String> = vec![];
        assert!(interactive_host_allowed("a.vercel.run", &none));
        assert!(interactive_host_allowed("VERCEL.RUN", &none));
        assert!(interactive_host_allowed("x.vercel.com.", &none));
        assert!(!interactive_host_allowed("vercel.run.evil.com", &none));
        assert!(!interactive_host_allowed("evilvercel.run", &none));
        assert!(!interactive_host_allowed("a.vercel.app", &none));
        assert!(!interactive_host_allowed("1.2.3.4", &none));
        assert!(!interactive_host_allowed("::1", &none));
        assert!(!interactive_host_allowed("[::1]", &none));
        assert!(!interactive_host_allowed("", &none));
        let custom = ["Example.org".to_string(), String::new()];
        assert!(interactive_host_allowed("a.example.org", &custom));
        assert!(!interactive_host_allowed("a.vercel.run", &custom));
    }

    /// Cases carried over from the svc client's former copy of the matcher,
    /// plus the shapes a URL host must never take.
    #[test]
    fn interactive_host_refusals_and_suffix_forms() {
        let none: Vec<String> = Vec::new();
        for ok in [
            "x.vercel.run",
            "vercel.run",
            "a.b.vercel.sh",
            "X.Vercel.Com.",
        ] {
            assert!(interactive_host_allowed(ok, &none), "{ok}");
        }
        for bad in [
            "evil.example",
            "vercel.app",
            "x.vercel.app",
            "X.VERCEL.APP.",
            "notvercel.run",
            "vercel.run.evil.example",
            "a.vercel.run..",
            "user@a.vercel.run",
            "a.vercel.run:443",
            " a.vercel.run",
            "a%2evercel.run",
            "127.0.0.1",
            "127.0.0.1.",
        ] {
            assert!(!interactive_host_allowed(bad, &none), "{bad}");
        }
        let custom = ["vx.example".to_string()];
        assert!(interactive_host_allowed("a.vx.example", &custom));
        assert!(interactive_host_allowed("vx.example", &custom));
        assert!(!interactive_host_allowed("x.vercel.run", &custom));
        assert!(!interactive_host_allowed("a.vx.example", &none));
        // A configured `vercel.app` (or a name under it) never admits a host.
        for s in ["vercel.app", "team.vercel.app"] {
            let custom = [s.to_string()];
            assert!(!interactive_host_allowed("x.vercel.app", &custom), "{s}");
            assert!(
                !interactive_host_allowed("a.team.vercel.app", &custom),
                "{s}"
            );
        }
        let dotted = ["vx.example.".to_string()];
        assert!(interactive_host_allowed("a.vx.example", &dotted));
    }

    #[test]
    fn session_usage_reads_session_then_sandbox_fields() {
        let v = json!({
            "sandbox": {
                "name": "tg-a",
                "resources": {"vcpus": 2, "memory": 4096},
                "totalActiveCpuDurationMs": 9000
            },
            "session": {"activeCpuDurationMs": 1500, "startedAt": 10, "stoppedAt": null}
        });
        let u = parse_session_usage(&v).unwrap().expect("a session");
        assert_eq!(u.active_cpu_ms, Some(1500));
        assert_eq!(u.vcpus, Some(2));
        assert_eq!(u.memory_mb, Some(4096));
        assert_eq!(u.started_at_ms, Some(10));
        assert_eq!(u.stopped_at_ms, None);
        let nested = json!({"sandbox": {"vcpus": 4, "session": {"startedAt": 1}}});
        let u = parse_session_usage(&nested).unwrap().expect("a session");
        assert_eq!((u.vcpus, u.started_at_ms), (Some(4), Some(1)));
        let fallback = json!({
            "sandbox": {"name": "tg-a", "totalActiveCpuDurationMs": 9000},
            "session": {"activeCpuDurationMs": null}
        });
        let u = parse_session_usage(&fallback).unwrap().expect("a session");
        assert_eq!(u.active_cpu_ms, None);
        let none = json!({"sandbox": {"name": "tg-a"}, "session": {}});
        assert_eq!(
            parse_session_usage(&none).unwrap().unwrap().active_cpu_ms,
            None
        );
        assert_eq!(
            parse_session_usage(&json!({"sandbox": {"name": "tg-a"}})).unwrap(),
            None
        );
        assert!(parse_session_usage(&json!(null)).is_err());
        assert!(parse_session_usage(&json!([1])).is_err());
    }

    #[test]
    fn snapshots_skip_invalid_ids() {
        let (s, next) = parse_snapshots(&json!({
            "snapshots": [
                {"id": "snap_1", "sizeBytes": 1024},
                {"id": "bad/id", "sizeBytes": 1},
                {"id": "a?b"},
                {"id": "x".repeat(129)},
                {"id": "snap_2"}],
            "pagination": {"next": 17}}))
        .unwrap();
        let ids: Vec<&str> = s.iter().map(|x| x.id.as_str()).collect();
        assert_eq!(ids, ["snap_1", "snap_2"]);
        assert_eq!(next.as_deref(), Some("17"));
    }

    #[test]
    fn session_usage_parse() {
        assert_eq!(parse_session_usage(&json!({})).unwrap(), None);
        let u = parse_session_usage(&json!({"session": {
            "activeCpuDurationMs": 5, "memory": 2048, "vcpus": 2,
            "startedAt": 10, "stoppedAt": 20}}))
        .unwrap()
        .unwrap();
        assert_eq!(u.active_cpu_ms, Some(5));
        assert_eq!(u.memory_mb, Some(2048));
        assert_eq!(u.vcpus, Some(2));
        assert_eq!((u.started_at_ms, u.stopped_at_ms), (Some(10), Some(20)));
        let e = parse_session_usage(&json!({"session": {}}))
            .unwrap()
            .unwrap();
        assert_eq!(e, crate::vercel::usage::SessionUsage::default());
    }

    #[test]
    fn snapshots_parse_and_cap() {
        let (s, next) = parse_snapshots(&json!({
            "snapshots": [
                {"id": "a", "sizeBytes": 7, "status": "created", "expiresAt": 9},
                {"sizeBytes": 1},
                {"id": "b"}],
            "pagination": {"next": "c1"}}))
        .unwrap();
        assert_eq!(s.len(), 2);
        assert_eq!((s[0].size_bytes, s[0].status.as_str()), (7, "created"));
        assert_eq!(s[1].status, "");
        assert_eq!(next.as_deref(), Some("c1"));
        assert!(parse_snapshots(&json!({})).is_err());
        let many: Vec<_> = (0..1200).map(|i| json!({"id": format!("s{i}")})).collect();
        let (s, next) =
            parse_snapshots(&json!({"snapshots": many, "pagination": {"next": 5}})).unwrap();
        assert_eq!(s.len(), MAX_SNAPSHOTS);
        assert_eq!(next.as_deref(), Some("5"));
        let (_, n) =
            parse_snapshots(&json!({"snapshots": [], "pagination": {"next": ""}})).unwrap();
        assert_eq!(n, None);
    }
}
