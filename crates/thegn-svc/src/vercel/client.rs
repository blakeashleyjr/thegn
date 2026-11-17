//! The Vercel Sandbox HTTP client: the only holder of the API token. Reuses
//! `provider_http_client()`, `CONTROL_GATE`, `validate_api_base` and the
//! body-limited readers.
//!
//! Custody rules (enforced here, asserted by `tests/vercel_mock.rs`):
//! - the token is sent only as the `Authorization: Bearer` header, only to the
//!   validated `api_base` authority, and never in a path, query or body;
//! - error strings carry an operation label, an HTTP status and at most
//!   [`ERROR_SNIPPET_CHARS`] sanitized characters of the response body (with
//!   any echo of the token scrubbed), never a URL;
//! - redirects are refused (the shared client has `Policy::none`, and a 3xx is
//!   an error here rather than being followed by hand).
//!
//! URL shapes mirror `thegn_core::vercel::api::{sandbox_url, session_url}`
//! (`/v2/sandboxes/{name}` and `/v2/sandboxes/sessions/{id}{tail}`, scoped by
//! `teamId`/`projectId`). They are built here on the parsed, validated base
//! with escaped path segments, because the core builders admit `https` only and
//! the loopback mock needs the `http`-for-loopback rule `validate_api_base`
//! already enforces.

use std::collections::VecDeque;
use std::fmt;
use std::time::Duration;

use anyhow::{Result, anyhow, bail};
use base64::Engine as _;
use thegn_core::vercel::admit::{valid_project_id, valid_sandbox_name, valid_team_id};
use thegn_core::vercel::api::{
    LogLine, SandboxView, Scope, parse_list_page, parse_log_line, parse_sandbox,
};

use super::VercelToken;
use crate::provider::{
    CONTROL_BODY_LIMIT, CONTROL_GATE, EXEC_EXIT_STATUS_UNKNOWN, authority_key,
    provider_http_client, redacted_transport_error, response_json, transient_err, transient_status,
    validate_api_base,
};

const PROVIDER: &str = "vercel";
const USER_AGENT: &str = concat!("thegn/", env!("CARGO_PKG_VERSION"));

/// Attempts for the retried calls (create/list/get/extend), transient only.
const ATTEMPTS: u32 = 3;
const BACKOFF: Duration = Duration::from_millis(500);

/// Local copy of the provider file-transfer deadline (`provider.rs` keeps its
/// own private). Also bounds `run_wait`, whose command may outlive the 30 s
/// control deadline.
const FILE_TRANSFER_TIMEOUT: Duration = Duration::from_secs(600);

/// A `logs` follow lives as long as the stream-mode session it feeds; the
/// thegn lifetime ceiling (24 h) bounds it.
const LOG_FOLLOW_TIMEOUT: Duration = Duration::from_secs(24 * 60 * 60);

/// Longest accepted NDJSON line (matches core's `MAX_LOG_LINE_BYTES`).
pub const LOG_LINE_CAP: usize = 1024 * 1024;

/// Most response-body characters an error string may carry.
pub const ERROR_SNIPPET_CHARS: usize = 200;

/// Bytes of an error response read for the snippet.
const ERROR_BODY_READ: usize = 8 * 1024;

/// Buffered log lines between the reader task and its consumer.
const LOG_CHANNEL_DEPTH: usize = 64;

/// The outcome of a session-timeout extension.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExtendOutcome {
    Extended,
    /// A 4xx on extend: the plan ceiling; stop extending and warn once.
    AtCeiling,
}

/// The `/interactive` WebSocket ticket. `token` is a secret; `Debug` redacts it.
/// The API token is never sent to `url`: callers present `token` alone.
pub struct InteractiveTicket {
    pub url: url::Url,
    pub token: zeroize::Zeroizing<String>,
}

impl fmt::Debug for InteractiveTicket {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("InteractiveTicket")
            .field("host", &self.url.host_str().unwrap_or(""))
            .field("token", &"<redacted>")
            .finish()
    }
}

/// A followed command log. Derefs to the line receiver; the reader task ends on
/// body EOF (dropping the sender, so `recv` yields `None`) and is aborted when
/// this guard drops.
pub struct LogStream {
    rx: tokio::sync::mpsc::Receiver<LogLine>,
    task: tokio::task::JoinHandle<()>,
}

impl LogStream {
    /// Whether the reader task has finished (EOF, error or consumer gone).
    pub fn reader_finished(&self) -> bool {
        self.task.is_finished()
    }
}

impl std::ops::Deref for LogStream {
    type Target = tokio::sync::mpsc::Receiver<LogLine>;
    fn deref(&self) -> &Self::Target {
        &self.rx
    }
}

impl std::ops::DerefMut for LogStream {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.rx
    }
}

impl Drop for LogStream {
    fn drop(&mut self) {
        self.task.abort();
    }
}

/// How one call is gated, retried and judged.
#[derive(Clone, Copy)]
struct Policy {
    op: &'static str,
    /// Priority permit (delete/stop: cleanup must not starve behind reads).
    priority: bool,
    /// Retry transient statuses / transport errors up to [`ATTEMPTS`].
    retry: bool,
}

const fn policy(op: &'static str) -> Policy {
    Policy {
        op,
        priority: false,
        retry: false,
    }
}

const fn retried(op: &'static str) -> Policy {
    Policy {
        op,
        priority: false,
        retry: true,
    }
}

const fn priority(op: &'static str) -> Policy {
    Policy {
        op,
        priority: true,
        retry: false,
    }
}

pub struct VercelClient {
    base: url::Url,
    authority: String,
    token: VercelToken,
    scope: Scope,
    http: reqwest::Client,
}

impl fmt::Debug for VercelClient {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("VercelClient")
            .field("authority", &self.authority)
            .field("token", &self.token)
            .field("scope", &self.scope)
            .finish()
    }
}

impl VercelClient {
    /// `base` is admitted by `validate_api_base` (https, or http on loopback;
    /// no userinfo, query or fragment).
    pub fn new(base: &str, token: VercelToken, scope: Scope) -> Result<Self> {
        let validated = validate_api_base(base)?;
        let authority = authority_key(&validated)?;
        let base = url::Url::parse(&validated).map_err(|_| anyhow!("invalid provider api_base"))?;
        anyhow::ensure!(
            !base.cannot_be_a_base(),
            "provider api_base cannot carry a path"
        );
        Ok(VercelClient {
            base,
            authority,
            token,
            scope,
            http: provider_http_client(),
        })
    }

    // --- control plane ----------------------------------------------------

    pub async fn create(&self, body: &serde_json::Value) -> Result<SandboxView> {
        let p = retried("create sandbox");
        let url = self.url(&["v3", "sandboxes"], &[], false)?;
        let body = self.json_body(body, p.op)?;
        let resp = self
            .send(p, || Ok(self.http.post(url.clone()).json(&body)))
            .await?;
        let resp = self.ok(resp, p.op, false).await?;
        let v = response_json(resp, PROVIDER, p.op).await?;
        parse_sandbox(&v).map_err(|e| anyhow!("{PROVIDER}: {}: {e}", p.op))
    }

    /// `None` when the sandbox does not exist. `resume` asks the API to resume
    /// a stopped sandbox (buys compute): only `Wake::Allow` callers pass true.
    pub async fn get(&self, name: &str, resume: bool) -> Result<Option<SandboxView>> {
        let p = retried("get sandbox");
        let extra: &[(&str, &str)] = if resume { &[("resume", "true")] } else { &[] };
        let url = self.url(&["v2", "sandboxes", checked_name(name)?], extra, true)?;
        let resp = self.send(p, || Ok(self.http.get(url.clone()))).await?;
        let Some(resp) = self.ok_or_404(resp, p.op).await? else {
            return Ok(None);
        };
        let v = response_json(resp, PROVIDER, p.op).await?;
        parse_sandbox(&v)
            .map(Some)
            .map_err(|e| anyhow!("{PROVIDER}: {}: {e}", p.op))
    }

    pub async fn list_page(
        &self,
        cursor: Option<&str>,
        name_prefix: &str,
    ) -> Result<(Vec<SandboxView>, Option<String>)> {
        let p = retried("list sandboxes");
        let mut extra: Vec<(&str, &str)> = vec![("project", self.scope.project_id.as_str())];
        if !name_prefix.is_empty() {
            extra.push(("namePrefix", name_prefix));
        }
        if let Some(cursor) = cursor.filter(|c| !c.is_empty()) {
            extra.push(("cursor", cursor));
        }
        let url = self.url(&["v2", "sandboxes"], &extra, false)?;
        let resp = self.send(p, || Ok(self.http.get(url.clone()))).await?;
        let resp = self.ok(resp, p.op, false).await?;
        let v = response_json(resp, PROVIDER, p.op).await?;
        parse_list_page(&v).map_err(|e| anyhow!("{PROVIDER}: {}: {e}", p.op))
    }

    /// Delete the sandbox by name. 404 = Ok (already gone).
    pub async fn delete(&self, name: &str) -> Result<()> {
        let p = priority("delete sandbox");
        let url = self.url(&["v2", "sandboxes", checked_name(name)?], &[], true)?;
        let resp = self.send(p, || Ok(self.http.delete(url.clone()))).await?;
        self.ok(resp, p.op, true).await.map(drop)
    }

    /// Stop a session (compute to $0; the filesystem persists). 404 = Ok.
    pub async fn stop(&self, session: &str) -> Result<()> {
        let p = priority("stop session");
        let url = self.session_url(session, &["stop"])?;
        let resp = self.send(p, || Ok(self.http.post(url.clone()))).await?;
        self.ok(resp, p.op, true).await.map(drop)
    }

    /// Extend the session timeout by `ms`. A non-transient 4xx means the plan
    /// ceiling ([`ExtendOutcome::AtCeiling`]).
    pub async fn extend(&self, session: &str, ms: u64) -> Result<ExtendOutcome> {
        let p = retried("extend session");
        let url = self.session_url(session, &["extend-timeout"])?;
        let body = serde_json::json!({ "duration": ms });
        let resp = self
            .send(p, || Ok(self.http.post(url.clone()).json(&body)))
            .await?;
        let status = resp.status();
        if status.is_client_error() && !transient_status(status) {
            drain_quietly(resp).await;
            return Ok(ExtendOutcome::AtCeiling);
        }
        self.ok(resp, p.op, false).await?;
        Ok(ExtendOutcome::Extended)
    }

    pub async fn set_network_policy(&self, name: &str, body: &serde_json::Value) -> Result<()> {
        let p = policy("set network policy");
        let url = self.url(&["v2", "sandboxes", checked_name(name)?], &[], true)?;
        let body = self.json_body(&serde_json::json!({ "networkPolicy": body }), p.op)?;
        let resp = self
            .send(p, || Ok(self.http.patch(url.clone()).json(&body)))
            .await?;
        self.ok(resp, p.op, false).await.map(drop)
    }

    // --- commands -----------------------------------------------------------

    /// Run a command and wait (`wait: true` is forced), streaming the NDJSON
    /// body. Returns `(exit, last out_cap stdout bytes)`; stderr is not kept. A
    /// missing final exit gives [`EXEC_EXIT_STATUS_UNKNOWN`]; an `error` line or
    /// a line over [`LOG_LINE_CAP`] is an error.
    pub async fn run_wait(
        &self,
        session: &str,
        body: &serde_json::Value,
        out_cap: usize,
    ) -> Result<(i32, Vec<u8>)> {
        let p = policy("run command");
        let url = self.session_url(session, &["cmd"])?;
        let mut body = self.json_body(body, p.op)?;
        match body.as_object_mut() {
            Some(map) => {
                map.insert("wait".into(), serde_json::Value::Bool(true));
            }
            None => bail!("{PROVIDER}: {}: body must be a JSON object", p.op),
        }
        let resp = self
            .send(p, || {
                Ok(self
                    .http
                    .post(url.clone())
                    .timeout(FILE_TRANSFER_TIMEOUT)
                    .json(&body))
            })
            .await?;
        let mut resp = self.ok(resp, p.op, false).await?;

        let mut lines = LineSplitter::new(LOG_LINE_CAP);
        let mut out = TailBuffer::new(out_cap);
        let mut code: Option<i32> = None;
        loop {
            let chunk = resp
                .chunk()
                .await
                .map_err(|e| redacted_transport_error(PROVIDER, p.op, &e))?;
            let done = chunk.is_none();
            let complete = match chunk {
                Some(chunk) => lines.push(&chunk),
                None => lines.finish(),
            }
            .map_err(|e| anyhow!("{PROVIDER}: {}: {e}", p.op))?;
            for line in complete {
                match parse_log_line(&line) {
                    Ok(LogLine::Stdout(bytes)) => out.push(&bytes),
                    Ok(LogLine::Stderr(_)) => {}
                    Ok(LogLine::Command { exit_code, .. }) => {
                        if exit_code.is_some() {
                            code = exit_code;
                        }
                    }
                    Ok(LogLine::Error { code, message }) => bail!(
                        "{PROVIDER}: {}: the command stream reported an error: {}",
                        p.op,
                        self.snippet(&format!("{code}: {message}"))
                    ),
                    // An unrecognised line (a future stream kind) is skipped,
                    // never echoed.
                    Err(_) => {}
                }
            }
            if done {
                break;
            }
        }
        Ok((code.unwrap_or(EXEC_EXIT_STATUS_UNKNOWN), out.into_vec()))
    }

    /// Start a detached command (`wait: false` is forced). Returns its id.
    pub async fn run_detached(&self, session: &str, body: &serde_json::Value) -> Result<String> {
        let p = policy("start command");
        let url = self.session_url(session, &["cmd"])?;
        let mut body = self.json_body(body, p.op)?;
        match body.as_object_mut() {
            Some(map) => {
                map.insert("wait".into(), serde_json::Value::Bool(false));
            }
            None => bail!("{PROVIDER}: {}: body must be a JSON object", p.op),
        }
        let resp = self
            .send(p, || Ok(self.http.post(url.clone()).json(&body)))
            .await?;
        let resp = self.ok(resp, p.op, false).await?;
        let v = response_json(resp, PROVIDER, p.op).await?;
        let id = v
            .get("command")
            .and_then(|c| c.get("id"))
            .or_else(|| v.get("id"))
            .and_then(|id| id.as_str())
            .filter(|id| valid_id(id))
            .ok_or_else(|| anyhow!("{PROVIDER}: {}: response has no valid command id", p.op))?;
        Ok(id.to_string())
    }

    /// Follow a command's NDJSON log. A reader task feeds the returned stream
    /// and ends on body EOF; dropping the stream aborts it. A transport failure
    /// or an over-long line arrives as a final [`LogLine::Error`].
    pub async fn logs(&self, session: &str, cmd: &str) -> Result<LogStream> {
        let p = policy("follow command log");
        let url = self.session_url(session, &["cmd", checked_id(cmd, "command")?, "logs"])?;
        let resp = self
            .send(p, || {
                Ok(self.http.get(url.clone()).timeout(LOG_FOLLOW_TIMEOUT))
            })
            .await?;
        let resp = self.ok(resp, p.op, false).await?;
        let (tx, rx) = tokio::sync::mpsc::channel(LOG_CHANNEL_DEPTH);
        let task = tokio::spawn(read_log_stream(resp, tx));
        Ok(LogStream { rx, task })
    }

    /// Write `data` to a command's stdin; `close` ends it after the write.
    pub async fn stdin(&self, session: &str, cmd: &str, data: &[u8], close: bool) -> Result<()> {
        let p = policy("write command stdin");
        let url = self.session_url(session, &["cmd", checked_id(cmd, "command")?, "stdin"])?;
        let body = serde_json::json!({
            "data": base64::engine::general_purpose::STANDARD.encode(data),
            "close": close,
        });
        let resp = self
            .send(p, || Ok(self.http.post(url.clone()).json(&body)))
            .await?;
        self.ok(resp, p.op, false).await.map(drop)
    }

    pub async fn kill(&self, session: &str, cmd: &str, signal: i32) -> Result<()> {
        let p = priority("kill command");
        let url = self.session_url(session, &["cmd", checked_id(cmd, "command")?, "kill"])?;
        let body = serde_json::json!({ "signal": signal });
        let resp = self
            .send(p, || Ok(self.http.post(url.clone()).json(&body)))
            .await?;
        self.ok(resp, p.op, true).await.map(drop)
    }

    // --- files --------------------------------------------------------------

    /// Upload a gzipped tar, extracted under `cwd` (absolute, no NUL/control).
    pub async fn fs_write_targz(&self, session: &str, cwd: &str, targz: Vec<u8>) -> Result<()> {
        let p = policy("write files");
        anyhow::ensure!(
            valid_abs_path(cwd),
            "{PROVIDER}: {}: cwd must be an absolute path without NUL or control characters",
            p.op
        );
        let url = self.session_url(session, &["fs", "write"])?;
        let mut targz = Some(targz);
        let resp = self
            .send(p, || {
                let body = targz
                    .take()
                    .ok_or_else(|| anyhow!("{PROVIDER}: {}: body already sent", p.op))?;
                Ok(self
                    .http
                    .post(url.clone())
                    .timeout(FILE_TRANSFER_TIMEOUT)
                    .header(reqwest::header::CONTENT_TYPE, "application/gzip")
                    .header("x-cwd", cwd)
                    .body(body))
            })
            .await?;
        self.ok(resp, p.op, false).await.map(drop)
    }

    /// Read one file. `None` on 404; over `cap` bytes is an error (never a
    /// truncation), enforced while streaming.
    pub async fn fs_read(&self, session: &str, path: &str, cap: usize) -> Result<Option<Vec<u8>>> {
        let p = policy("read file");
        anyhow::ensure!(
            valid_abs_path(path),
            "{PROVIDER}: {}: path must be absolute without NUL or control characters",
            p.op
        );
        let url = self.session_url(session, &["fs", "read"])?;
        let body = serde_json::json!({ "path": path });
        let resp = self
            .send(p, || {
                Ok(self
                    .http
                    .post(url.clone())
                    .timeout(FILE_TRANSFER_TIMEOUT)
                    .json(&body))
            })
            .await?;
        let Some(mut resp) = self.ok_or_404(resp, p.op).await? else {
            return Ok(None);
        };
        if let Some(declared) = resp.content_length()
            && declared > cap as u64
        {
            bail!("{PROVIDER}: {}: file exceeds the {cap}-byte cap", p.op);
        }
        let mut data = Vec::new();
        while let Some(chunk) = resp
            .chunk()
            .await
            .map_err(|e| redacted_transport_error(PROVIDER, p.op, &e))?
        {
            anyhow::ensure!(
                chunk.len() <= cap.saturating_sub(data.len()),
                "{PROVIDER}: {}: file exceeds the {cap}-byte cap",
                p.op
            );
            data.extend_from_slice(&chunk);
        }
        Ok(Some(data))
    }

    pub async fn fs_mkdir(&self, session: &str, path: &str) -> Result<()> {
        let p = policy("make directory");
        anyhow::ensure!(
            valid_abs_path(path),
            "{PROVIDER}: {}: path must be absolute without NUL or control characters",
            p.op
        );
        let url = self.session_url(session, &["fs", "mkdir"])?;
        let body = serde_json::json!({ "path": path });
        let resp = self
            .send(p, || Ok(self.http.post(url.clone()).json(&body)))
            .await?;
        self.ok(resp, p.op, false).await.map(drop)
    }

    // --- interactive ----------------------------------------------------------

    /// Mint an `/interactive` WebSocket ticket. The returned URL is `wss` on a
    /// DNS host (no IP literal, no `localhost`, no userinfo); its `token` is the
    /// ticket's own secret, never the API token.
    pub async fn interactive(&self, session: &str) -> Result<InteractiveTicket> {
        let p = policy("open interactive session");
        let url = self.session_url(session, &["interactive"])?;
        let resp = self.send(p, || Ok(self.http.post(url.clone()))).await?;
        let resp = self.ok(resp, p.op, false).await?;
        let v = response_json(resp, PROVIDER, p.op).await?;
        let raw_url = v
            .get("url")
            .and_then(|u| u.as_str())
            .ok_or_else(|| anyhow!("{PROVIDER}: {}: response has no url", p.op))?;
        let token = v
            .get("token")
            .and_then(|t| t.as_str())
            .filter(|t| !t.is_empty())
            .ok_or_else(|| anyhow!("{PROVIDER}: {}: response has no token", p.op))?;
        let api = self.token.expose();
        anyhow::ensure!(
            api.is_empty() || (!raw_url.contains(api) && token != api),
            "{PROVIDER}: {}: refused a ticket that carries the API token",
            p.op
        );
        let url = admit_ticket_url(raw_url).map_err(|e| anyhow!("{PROVIDER}: {}: {e}", p.op))?;
        Ok(InteractiveTicket {
            url,
            token: zeroize::Zeroizing::new(token.to_string()),
        })
    }

    // --- snapshots ------------------------------------------------------------

    /// Snapshot ids recorded for sandbox `name`.
    pub async fn snapshots_for(&self, name: &str) -> Result<Vec<String>> {
        let p = retried("list snapshots");
        let name = checked_name(name)?;
        let url = self.url(
            &["v2", "sandboxes", "snapshots"],
            &[("project", self.scope.project_id.as_str()), ("name", name)],
            false,
        )?;
        let resp = self.send(p, || Ok(self.http.get(url.clone()))).await?;
        let resp = self.ok(resp, p.op, false).await?;
        let v = response_json(resp, PROVIDER, p.op).await?;
        let items = v
            .get("snapshots")
            .and_then(|s| s.as_array())
            .ok_or_else(|| anyhow!("{PROVIDER}: {}: response has no snapshots array", p.op))?;
        Ok(items
            .iter()
            .filter_map(|s| s.get("id").and_then(|id| id.as_str()))
            .filter(|id| valid_id(id))
            .map(str::to_string)
            .collect())
    }

    /// Delete one snapshot. 404 = Ok.
    pub async fn delete_snapshot(&self, id: &str) -> Result<()> {
        let p = priority("delete snapshot");
        let url = self.url(
            &["v2", "sandboxes", "snapshots", checked_id(id, "snapshot")?],
            &[],
            false,
        )?;
        let resp = self.send(p, || Ok(self.http.delete(url.clone()))).await?;
        self.ok(resp, p.op, true).await.map(drop)
    }

    // --- plumbing -------------------------------------------------------------

    /// `{base}/{segments…}?teamId=…[&projectId=…]{&extra…}`, segments escaped.
    fn url(
        &self,
        segments: &[&str],
        extra: &[(&str, &str)],
        with_project: bool,
    ) -> Result<url::Url> {
        anyhow::ensure!(
            valid_team_id(&self.scope.team_id) && valid_project_id(&self.scope.project_id),
            "{PROVIDER}: invalid team or project id"
        );
        let mut url = self.base.clone();
        {
            let mut path = url
                .path_segments_mut()
                .map_err(|_| anyhow!("invalid provider api_base"))?;
            path.pop_if_empty();
            path.extend(segments);
        }
        {
            let mut q = url.query_pairs_mut();
            q.append_pair("teamId", &self.scope.team_id);
            if with_project {
                q.append_pair("projectId", &self.scope.project_id);
            }
            for (k, v) in extra {
                q.append_pair(k, v);
            }
        }
        Ok(url)
    }

    fn session_url(&self, session: &str, tail: &[&str]) -> Result<url::Url> {
        let mut segments = vec![
            "v2",
            "sandboxes",
            "sessions",
            checked_id(session, "session")?,
        ];
        segments.extend_from_slice(tail);
        self.url(&segments, &[], true)
    }

    /// Clone a caller's JSON body, refusing one that carries the token.
    fn json_body(&self, body: &serde_json::Value, op: &str) -> Result<serde_json::Value> {
        let api = self.token.expose();
        if !api.is_empty() && body.to_string().contains(api) {
            bail!("{PROVIDER}: {op}: refused a request body that carries the API token");
        }
        Ok(body.clone())
    }

    /// Gate, authorize, send; retry transient failures under `p.retry`. Returns
    /// the final response whatever its status (the caller judges it).
    async fn send<F>(&self, p: Policy, mut build: F) -> Result<reqwest::Response>
    where
        F: FnMut() -> Result<reqwest::RequestBuilder>,
    {
        let attempts = if p.retry { ATTEMPTS } else { 1 };
        let mut attempt = 0;
        loop {
            attempt += 1;
            let request = build()?
                .bearer_auth(self.token.expose())
                .header(reqwest::header::USER_AGENT, USER_AGENT);
            let permit = if p.priority {
                CONTROL_GATE.acquire_priority(&self.authority).await?
            } else {
                CONTROL_GATE.acquire(&self.authority).await?
            };
            let result = request.send().await;
            drop(permit);
            match result {
                Ok(resp) if attempt < attempts && transient_status(resp.status()) => {
                    drain_quietly(resp).await;
                }
                Ok(resp) => return Ok(resp),
                Err(e) if attempt < attempts && transient_err(&e) => {}
                Err(e) => return Err(redacted_transport_error(PROVIDER, p.op, &e)),
            }
            tokio::time::sleep(BACKOFF * attempt).await;
        }
    }

    /// A 2xx passes; `ok_404` also passes a 404 (already gone). Anything else
    /// (a 3xx redirect included) is an error with a sanitized body snippet.
    async fn ok(
        &self,
        resp: reqwest::Response,
        op: &str,
        ok_404: bool,
    ) -> Result<reqwest::Response> {
        let status = resp.status();
        if status.is_success() || (ok_404 && status == reqwest::StatusCode::NOT_FOUND) {
            return Ok(resp);
        }
        Err(self.status_error(resp, op).await)
    }

    async fn ok_or_404(
        &self,
        resp: reqwest::Response,
        op: &str,
    ) -> Result<Option<reqwest::Response>> {
        if resp.status() == reqwest::StatusCode::NOT_FOUND {
            drain_quietly(resp).await;
            return Ok(None);
        }
        self.ok(resp, op, false).await.map(Some)
    }

    async fn status_error(&self, mut resp: reqwest::Response, op: &str) -> anyhow::Error {
        let status = resp.status();
        if status.is_redirection() {
            return anyhow!(
                "{PROVIDER}: {op}: redirect refused (HTTP {})",
                status.as_u16()
            );
        }
        let mut body = Vec::new();
        while body.len() < ERROR_BODY_READ {
            match resp.chunk().await {
                Ok(Some(chunk)) => {
                    let room = ERROR_BODY_READ - body.len();
                    body.extend_from_slice(&chunk[..chunk.len().min(room)]);
                }
                Ok(None) | Err(_) => break,
            }
        }
        let text = self.snippet(&String::from_utf8_lossy(&body));
        if text.is_empty() {
            anyhow!("{PROVIDER}: {op} returned HTTP {}", status.as_u16())
        } else {
            anyhow!("{PROVIDER}: {op} returned HTTP {}: {text}", status.as_u16())
        }
    }

    fn snippet(&self, text: &str) -> String {
        sanitize_snippet(text, self.token.expose())
    }
}

/// Feed `tx` from a followed NDJSON body until EOF, an error, or the consumer
/// going away.
async fn read_log_stream(mut resp: reqwest::Response, tx: tokio::sync::mpsc::Sender<LogLine>) {
    let mut lines = LineSplitter::new(LOG_LINE_CAP);
    loop {
        let (complete, done) = match resp.chunk().await {
            Ok(Some(chunk)) => (lines.push(&chunk), false),
            Ok(None) => (lines.finish(), true),
            Err(_) => (Err("log stream transport failed"), true),
        };
        let complete = match complete {
            Ok(complete) => complete,
            Err(reason) => {
                // best-effort: the consumer may already be gone; the task ends either way.
                let _sent = tx
                    .send(LogLine::Error {
                        code: "thegn_stream".into(),
                        message: reason.into(),
                    })
                    .await;
                return;
            }
        };
        for line in complete {
            if let Ok(parsed) = parse_log_line(&line)
                && tx.send(parsed).await.is_err()
            {
                return;
            }
        }
        if done {
            return;
        }
    }
}

/// Drain (a bounded amount of) a response we are discarding so the connection
/// can be reused; failures are irrelevant.
async fn drain_quietly(mut resp: reqwest::Response) {
    let mut seen = 0usize;
    while seen < CONTROL_BODY_LIMIT {
        match resp.chunk().await {
            Ok(Some(chunk)) => seen += chunk.len(),
            Ok(None) | Err(_) => return,
        }
    }
}

fn checked_name(name: &str) -> Result<&str> {
    anyhow::ensure!(valid_sandbox_name(name), "{PROVIDER}: invalid sandbox name");
    Ok(name)
}

/// Session/command/snapshot ids: `[A-Za-z0-9_-]{1,128}`.
fn valid_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 128
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}

fn checked_id<'a>(id: &'a str, what: &str) -> Result<&'a str> {
    anyhow::ensure!(valid_id(id), "{PROVIDER}: invalid {what} id");
    Ok(id)
}

fn valid_abs_path(path: &str) -> bool {
    path.starts_with('/') && !path.chars().any(|c| c == '\0' || c.is_control())
}

/// At most [`ERROR_SNIPPET_CHARS`] characters of `text`, control characters
/// flattened to spaces, every occurrence of `token` scrubbed first.
pub fn sanitize_snippet(text: &str, token: &str) -> String {
    let scrubbed = if token.is_empty() {
        text.to_string()
    } else {
        text.replace(token, "<redacted>")
    };
    let mut out: String = scrubbed
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .take(ERROR_SNIPPET_CHARS)
        .collect();
    let cut = out.trim_end().len();
    out.truncate(cut);
    out
}

/// Admit an `/interactive` URL: it must parse, be `wss` (`https` is rewritten
/// to `wss`), name a DNS host that is not `localhost`, and carry no userinfo.
/// The reason is fixed text, never the URL.
pub fn admit_ticket_url(raw: &str) -> Result<url::Url, &'static str> {
    let mut url = url::Url::parse(raw).map_err(|_| "interactive url does not parse")?;
    match url.scheme() {
        "wss" => {}
        "https" => url
            .set_scheme("wss")
            .map_err(|_| "interactive url scheme cannot be rewritten")?,
        _ => return Err("interactive url must use wss"),
    }
    if !url.username().is_empty() || url.password().is_some() {
        return Err("interactive url must not carry userinfo");
    }
    match url.host() {
        Some(url::Host::Domain(host)) => {
            let host = host.trim_end_matches('.').to_ascii_lowercase();
            if host == "localhost" || host.ends_with(".localhost") {
                return Err("interactive url must not be localhost");
            }
        }
        Some(url::Host::Ipv4(_) | url::Host::Ipv6(_)) => {
            return Err("interactive url must not be an IP literal");
        }
        None => return Err("interactive url has no host"),
    }
    url.set_fragment(None);
    Ok(url)
}

/// Splits a byte stream into NDJSON lines, refusing any line over `cap`.
struct LineSplitter {
    buf: Vec<u8>,
    cap: usize,
}

impl LineSplitter {
    fn new(cap: usize) -> Self {
        LineSplitter {
            buf: Vec::new(),
            cap,
        }
    }

    /// The lines completed by `chunk` (blank lines dropped, `\r` trimmed).
    fn push(&mut self, chunk: &[u8]) -> Result<Vec<Vec<u8>>, &'static str> {
        let mut complete = Vec::new();
        let mut rest = chunk;
        while let Some(pos) = rest.iter().position(|b| *b == b'\n') {
            let (head, tail) = rest.split_at(pos);
            if self.buf.len().saturating_add(head.len()) > self.cap {
                return Err("log line too long");
            }
            self.buf.extend_from_slice(head);
            let line = std::mem::take(&mut self.buf);
            if let Some(line) = trimmed(line) {
                complete.push(line);
            }
            rest = &tail[1..];
        }
        if self.buf.len().saturating_add(rest.len()) > self.cap {
            return Err("log line too long");
        }
        self.buf.extend_from_slice(rest);
        Ok(complete)
    }

    /// The unterminated final line, if any.
    fn finish(&mut self) -> Result<Vec<Vec<u8>>, &'static str> {
        Ok(trimmed(std::mem::take(&mut self.buf)).into_iter().collect())
    }
}

fn trimmed(mut line: Vec<u8>) -> Option<Vec<u8>> {
    if line.last() == Some(&b'\r') {
        line.pop();
    }
    (!line.iter().all(u8::is_ascii_whitespace)).then_some(line)
}

/// Keeps the last `cap` bytes pushed (the `collect_exec_output` semantics).
struct TailBuffer {
    buf: VecDeque<u8>,
    cap: usize,
}

impl TailBuffer {
    fn new(cap: usize) -> Self {
        TailBuffer {
            buf: VecDeque::new(),
            cap,
        }
    }

    fn push(&mut self, bytes: &[u8]) {
        if self.cap == 0 {
            return;
        }
        if bytes.len() >= self.cap {
            self.buf.clear();
            self.buf.extend(&bytes[bytes.len() - self.cap..]);
            return;
        }
        let excess = (self.buf.len() + bytes.len()).saturating_sub(self.cap);
        self.buf.drain(..excess);
        self.buf.extend(bytes);
    }

    fn into_vec(self) -> Vec<u8> {
        self.buf.into_iter().collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ticket_url_admission() {
        let ok = admit_ticket_url("wss://sbx.vercel.run/interactive?t=1").expect("wss");
        assert_eq!(ok.scheme(), "wss");
        let rewritten = admit_ticket_url("https://sbx.vercel.run/i").expect("https");
        assert_eq!(rewritten.scheme(), "wss");
        for bad in [
            "ws://sbx.vercel.run/i",
            "http://sbx.vercel.run/i",
            "wss://127.0.0.1/i",
            "ws://127.0.0.1:9/i",
            "wss://[::1]/i",
            "wss://localhost/i",
            "wss://LOCALHOST./i",
            "wss://a.localhost/i",
            "wss://user:pw@sbx.vercel.run/i",
            "wss://user@sbx.vercel.run/i",
            "not a url",
            "file:///etc/passwd",
        ] {
            assert!(admit_ticket_url(bad).is_err(), "{bad} must be refused");
        }
    }

    #[test]
    fn ticket_debug_is_redacted() {
        let ticket = InteractiveTicket {
            url: url::Url::parse("wss://sbx.vercel.run/i?k=1").unwrap(),
            token: zeroize::Zeroizing::new("ticket-secret".into()),
        };
        let shown = format!("{ticket:?}");
        assert!(!shown.contains("ticket-secret"), "{shown}");
        assert!(!shown.contains("k=1"), "{shown}");
    }

    #[test]
    fn snippet_scrubs_token_flattens_controls_and_caps() {
        let token = "vcp_sentinel";
        let text = format!("bad\r\ntoken {token} echoed {}", "x".repeat(500));
        let out = sanitize_snippet(&text, token);
        assert!(!out.contains(token));
        assert!(out.contains("<redacted>"));
        assert!(!out.contains('\n') && !out.contains('\r'));
        assert!(out.chars().count() <= ERROR_SNIPPET_CHARS);
    }

    #[test]
    fn splitter_handles_split_lines_crlf_and_blank_lines() {
        let mut s = LineSplitter::new(64);
        assert_eq!(s.push(b"{\"a\"").unwrap(), Vec::<Vec<u8>>::new());
        let got = s.push(b":1}\r\n\n{\"b\":2}\n{\"c\"").unwrap();
        assert_eq!(got, vec![b"{\"a\":1}".to_vec(), b"{\"b\":2}".to_vec()]);
        assert_eq!(s.finish().unwrap(), vec![b"{\"c\"".to_vec()]);
    }

    #[test]
    fn splitter_refuses_an_over_long_line_even_across_chunks() {
        let mut s = LineSplitter::new(8);
        assert!(s.push(b"12345").is_ok());
        assert!(s.push(b"6789").is_err());
        let mut s = LineSplitter::new(8);
        assert!(s.push(b"123456789\n").is_err());
        let mut s = LineSplitter::new(8);
        assert!(s.push(b"12345678\n").is_ok());
    }

    #[test]
    fn tail_buffer_keeps_the_last_cap_bytes() {
        let mut t = TailBuffer::new(4);
        t.push(b"ab");
        t.push(b"cde");
        assert_eq!(t.into_vec(), b"bcde");
        let mut t = TailBuffer::new(3);
        t.push(b"a");
        t.push(b"wxyz");
        assert_eq!(t.into_vec(), b"xyz");
        let mut t = TailBuffer::new(0);
        t.push(b"abc");
        assert!(t.into_vec().is_empty());
    }

    #[test]
    fn path_and_id_validation() {
        assert!(valid_abs_path("/vercel/sandbox"));
        assert!(!valid_abs_path("relative"));
        assert!(!valid_abs_path("/a\0b"));
        assert!(!valid_abs_path("/a\nb"));
        assert!(valid_id("sbx_ABC-1"));
        assert!(!valid_id(""));
        assert!(!valid_id("a/b"));
        assert!(!valid_id("a?b"));
        assert!(!valid_id(&"a".repeat(129)));
    }
}
