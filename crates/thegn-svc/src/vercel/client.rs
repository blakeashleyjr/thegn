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
//! `teamId` and with `projectId` on sandbox-scoped calls). They are built here
//! on the parsed, validated base
//! with escaped path segments, because the core builders admit `https` only and
//! the loopback mock needs the `http`-for-loopback rule `validate_api_base`
//! already enforces.

use std::collections::{HashSet, VecDeque};
use std::fmt;
use std::time::Duration;

use anyhow::{Result, anyhow, bail};
use base64::Engine as _;
use thegn_core::vercel::admit::{valid_project_id, valid_sandbox_name, valid_team_id};
use thegn_core::vercel::api::{
    LogLine, SandboxView, Scope, interactive_host_allowed, parse_list_page, parse_log_line,
    parse_sandbox, parse_session_usage, parse_snapshots,
};
use thegn_core::vercel::token::{TokenKind, classify, oidc_refusal};
use thegn_core::vercel::usage::{SessionUsage, SnapshotUsage};

use super::VercelToken;
use crate::provider::{
    CONTROL_BODY_LIMIT, CONTROL_GATE, EXEC_EXIT_STATUS_UNKNOWN, FILE_TRANSFER_TIMEOUT, StderrTail,
    authority_key, provider_http_client, redacted_transport_error, response_json, transient_err,
    transient_status, validate_api_base,
};

const PROVIDER: &str = "vercel";
const USER_AGENT: &str = concat!("thegn/", env!("CARGO_PKG_VERSION"));

/// Attempts for the retried calls (create/list/get/extend), transient only.
const ATTEMPTS: u32 = 3;
const BACKOFF: Duration = Duration::from_millis(500);

/// Attempts for `stdin` (the SDK's rule: 5 retries, on 429 only).
const STDIN_ATTEMPTS: u32 = 6;

/// The longest `Retry-After` (seconds) a retried call waits out; a longer one
/// fails the call with "retry after Ns".
pub const MAX_RETRY_AFTER_S: u64 = 20;

/// Most snapshot-listing pages [`VercelClient::snapshot_usage`] and
/// [`VercelClient::snapshots_for`] read.
pub const SNAPSHOT_PAGE_CAP: usize = 20;

/// The refusal for a ticket on a host outside the pin (names no URL, query or
/// ticket).
pub const INTERACTIVE_HOST_REFUSAL: &str = "vercel: interactive url host is not an allowed Vercel \
     host (set env.<name>.provider.interactive_host_suffixes)";

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

/// The hard overall deadline of [`VercelClient::wait_command`] in stream mode
/// (the log stream has already ended, so the command is finishing).
pub const WAIT_COMMAND_DEADLINE: Duration = Duration::from_secs(60);

/// The per-request deadline of one `wait=true` poll.
const WAIT_POLL_TIMEOUT: Duration = Duration::from_secs(30);

/// The pause between polls that came back without an exit code.
const WAIT_POLL_INTERVAL: Duration = Duration::from_millis(500);

/// The most polls one wait makes, whatever the deadline.
const WAIT_MAX_POLLS: u32 = 120;

/// The outcome of a session-timeout extension.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExtendOutcome {
    Extended,
    /// A 400 or 422 on extend: the plan ceiling; stop extending and warn once.
    AtCeiling,
}

/// `extend` found the session gone (OpenAPI: extend-timeout 404 or 410).
#[derive(Debug)]
pub struct SessionGone {
    pub status: u16,
}

impl fmt::Display for SessionGone {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{PROVIDER}: extend session: session no longer exists (HTTP {})",
            self.status
        )
    }
}

impl std::error::Error for SessionGone {}

/// A session-scoped call refused because the session stopped or is stopping:
/// HTTP 410, or 422 with `error.code` `sandbox_stopping` / `sandbox_snapshotting`
/// (SDK `withResume`). The request was refused in the response head, so the
/// command was not started; the caller may resolve the session again.
#[derive(Debug)]
pub struct SessionStopped {
    pub op: &'static str,
    pub status: u16,
}

impl fmt::Display for SessionStopped {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{PROVIDER}: {}: the sandbox session has stopped (HTTP {})",
            self.op, self.status
        )
    }
}

impl std::error::Error for SessionStopped {}

/// A sandbox GET answered 410 with `error.code` `snapshot_not_found`: the
/// sandbox's saved state has expired and it cannot be resumed.
#[derive(Debug)]
pub struct SnapshotExpired {
    pub name: String,
}

impl fmt::Display for SnapshotExpired {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{PROVIDER}: sandbox {} cannot be resumed: Vercel reports its saved snapshot has expired; destroy it (thegn env down) and create a new one",
            self.name
        )
    }
}

impl std::error::Error for SnapshotExpired {}

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

/// A followed command log. The reader task ends on body EOF (dropping the
/// sender, so `recv` yields `None`) and is aborted when this guard drops.
pub struct LogStream {
    rx: tokio::sync::mpsc::Receiver<LogLine>,
    task: tokio::task::JoinHandle<()>,
}

impl LogStream {
    /// A stream over `rx` with a reader task that has already finished: the
    /// seam the stream driver's tests use instead of a live log request.
    #[cfg(test)]
    pub(crate) fn from_receiver(rx: tokio::sync::mpsc::Receiver<LogLine>) -> Self {
        LogStream {
            rx,
            task: tokio::spawn(async {}),
        }
    }

    #[cfg(test)]
    pub(crate) fn empty() -> Self {
        let (_tx, rx) = tokio::sync::mpsc::channel(1);
        Self::from_receiver(rx)
    }

    /// Whether the reader task has finished (EOF, error or consumer gone).
    pub fn reader_finished(&self) -> bool {
        self.task.is_finished()
    }

    /// The log lines this stream follows. The borrow keeps this guard alive, so
    /// the reader task is still aborted when the stream drops.
    pub fn receiver_mut(&mut self) -> &mut tokio::sync::mpsc::Receiver<LogLine> {
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
    /// Total attempts (1 = never retried).
    attempts: u32,
    /// Retry transient statuses other than 429 / transport errors.
    transient: bool,
    /// Retry a 429 (honouring `Retry-After` up to [`MAX_RETRY_AFTER_S`]).
    rate_limited: bool,
}

const fn policy(op: &'static str) -> Policy {
    Policy {
        op,
        priority: false,
        attempts: 1,
        transient: false,
        rate_limited: false,
    }
}

const fn retried(op: &'static str) -> Policy {
    Policy {
        op,
        priority: false,
        attempts: ATTEMPTS,
        transient: true,
        rate_limited: true,
    }
}

const fn priority(op: &'static str) -> Policy {
    Policy {
        op,
        priority: true,
        attempts: 1,
        transient: false,
        rate_limited: false,
    }
}

/// A parsed `Retry-After` header (delta-seconds only; an HTTP-date is
/// `Unparsable`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RetryAfter {
    Absent,
    Secs(u64),
    Unparsable,
}

fn retry_after(headers: &reqwest::header::HeaderMap) -> RetryAfter {
    let Some(value) = headers.get(reqwest::header::RETRY_AFTER) else {
        return RetryAfter::Absent;
    };
    parse_retry_after(value.to_str().ok())
}

/// Checked parse of a `Retry-After` value: ASCII digits only, no overflow.
fn parse_retry_after(value: Option<&str>) -> RetryAfter {
    let Some(text) = value.map(str::trim) else {
        return RetryAfter::Unparsable;
    };
    if text.is_empty() || !text.bytes().all(|b| b.is_ascii_digit()) {
        return RetryAfter::Unparsable;
    }
    text.parse::<u64>()
        .map_or(RetryAfter::Unparsable, RetryAfter::Secs)
}

/// The failure for a 429 that is not (or no longer) retried.
fn rate_limited_error(op: &str, after: RetryAfter) -> anyhow::Error {
    match after {
        RetryAfter::Secs(n) => {
            anyhow!("{PROVIDER}: {op}: rate limited by Vercel; retry after {n}s")
        }
        RetryAfter::Absent | RetryAfter::Unparsable => {
            anyhow!("{PROVIDER}: {op}: rate limited by Vercel; retry later")
        }
    }
}

fn unix_now_s() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX))
}

pub struct VercelClient {
    base: url::Url,
    authority: String,
    token: VercelToken,
    scope: Scope,
    http: reqwest::Client,
    /// The token's kind, classified once in [`VercelClient::new`]; an OIDC
    /// token near expiry refuses every request.
    token_kind: TokenKind,
    /// `[env.<name>.provider] interactive_host_suffixes` (empty ⇒ the
    /// built-in Vercel-owned defaults); the WebSocket ticket host pin.
    interactive_hosts: Vec<String>,
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
        let token_kind = classify(token.expose());
        Ok(VercelClient {
            base,
            authority,
            token,
            scope,
            http: provider_http_client(),
            token_kind,
            interactive_hosts: Vec::new(),
        })
    }

    /// Pin the interactive WebSocket ticket host to `suffixes` (empty ⇒ the
    /// built-in defaults).
    #[must_use]
    pub fn with_interactive_hosts(mut self, suffixes: Vec<String>) -> Self {
        self.interactive_hosts = suffixes;
        self
    }

    /// The current session's usage of sandbox `name` (a GET without
    /// `resume`; never buys compute). `None` when the sandbox does not exist.
    pub async fn usage(&self, name: &str) -> Result<Option<SessionUsage>> {
        let p = retried("read sandbox usage");
        let url = self.url(&["v2", "sandboxes", checked_name(name)?], &[], true)?;
        let resp = self.send(p, || Ok(self.http.get(url.clone()))).await?;
        let Some(resp) = self.ok_or_404(resp, p.op).await? else {
            return Ok(None);
        };
        let v = response_json(resp, PROVIDER, p.op).await?;
        parse_session_usage(&v).map_err(|e| anyhow!("{PROVIDER}: {}: {e}", p.op))
    }

    /// The snapshots of sandbox `name` with their sizes and status, paged
    /// through at most [`SNAPSHOT_PAGE_CAP`] listing pages (a repeated cursor
    /// also stops it).
    pub async fn snapshot_usage(&self, name: &str) -> Result<(Vec<SnapshotUsage>, bool)> {
        self.snapshot_pages(name).await
    }

    /// Every snapshot listing page for `name` (`limit=50`, the documented
    /// maximum), at most [`SNAPSHOT_PAGE_CAP`] pages. `complete` is false when
    /// the cap ends the walk with a next cursor left, or a cursor repeats.
    async fn snapshot_pages(&self, name: &str) -> Result<(Vec<SnapshotUsage>, bool)> {
        let p = retried("list snapshots");
        let name = checked_name(name)?;
        let mut out = Vec::new();
        let mut cursor: Option<String> = None;
        let mut seen = HashSet::new();
        // A row the parser refused (an id that is not a valid snapshot id) is
        // a snapshot this listing cannot name, so the listing is not complete.
        let mut dropped = false;
        for _page in 0..SNAPSHOT_PAGE_CAP {
            let mut extra: Vec<(&str, &str)> = vec![
                ("project", self.scope.project_id.as_str()),
                ("name", name),
                ("limit", "50"),
            ];
            if let Some(c) = cursor.as_deref() {
                extra.push(("cursor", c));
            }
            let url = self.url(&["v2", "sandboxes", "snapshots"], &extra, false)?;
            let resp = self.send(p, || Ok(self.http.get(url.clone()))).await?;
            let resp = self.ok(resp, p.op, false).await?;
            let v = response_json(resp, PROVIDER, p.op).await?;
            let (items, next) =
                parse_snapshots(&v).map_err(|e| anyhow!("{PROVIDER}: {}: {e}", p.op))?;
            let listed = v
                .get("snapshots")
                .and_then(|rows| rows.as_array())
                .map_or(0, Vec::len);
            if items.len() != listed {
                dropped = true;
            }
            out.extend(items);
            match next {
                None => return Ok((out, !dropped)),
                Some(next) if seen.insert(next.clone()) => cursor = Some(next),
                Some(_) => return Ok((out, false)),
            }
        }
        Ok((out, false))
    }

    // --- control plane ----------------------------------------------------

    pub async fn create(&self, body: &serde_json::Value) -> Result<SandboxView> {
        // Transient failures are retried; a 429 is not (a create is never
        // repeated after the API refused it for rate).
        let p = Policy {
            rate_limited: false,
            ..retried("create sandbox")
        };
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
        let name = checked_name(name)?;
        let extra: &[(&str, &str)] = if resume { &[("resume", "true")] } else { &[] };
        let url = self.url(&["v2", "sandboxes", name], extra, true)?;
        let resp = self.send(p, || Ok(self.http.get(url.clone()))).await?;
        if resp.status() == reqwest::StatusCode::GONE {
            let body = self.error_body(resp).await;
            if api_error_code(&body).as_deref() == Some("snapshot_not_found") {
                return Err(SnapshotExpired {
                    name: name.to_string(),
                }
                .into());
            }
            return Err(self.error_from(reqwest::StatusCode::GONE, &body, p.op));
        }
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
            extra.push(("sortBy", "name"));
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

    /// Delete the sandbox by name, asking the API to drop its orphaned
    /// snapshots too (`deleteOrphanSnapshots=true`). 404 = Ok (already gone).
    pub async fn delete(&self, name: &str) -> Result<()> {
        let p = priority("delete sandbox");
        let url = self.url(
            &["v2", "sandboxes", checked_name(name)?],
            &[("deleteOrphanSnapshots", "true")],
            true,
        )?;
        let resp = self.send(p, || Ok(self.http.delete(url.clone()))).await?;
        self.ok(resp, p.op, true).await.map(drop)
    }

    /// Stop a session (compute to $0; the filesystem persists). 404 = Ok.
    pub async fn stop(&self, session: &str) -> Result<()> {
        let p = priority("stop session");
        let url = self.session_url(session, &["stop"])?;
        // Vercel answers a body-less POST with 415 Unsupported Media Type, so
        // send an empty JSON object (live run, THE-2234).
        let body = serde_json::json!({});
        let resp = self
            .send(p, || Ok(self.http.post(url.clone()).json(&body)))
            .await?;
        // OpenAPI lists 410; the SDK treats 410 on a session as stopped (sandbox.ts isSandboxStoppedError)
        if resp.status() == reqwest::StatusCode::GONE {
            drain_quietly(resp).await;
            return Ok(());
        }
        self.ok(resp, p.op, true).await.map(drop)
    }

    /// Extend the session timeout by `ms`. Only documented plan-ceiling
    /// responses (400 and 422) map to [`ExtendOutcome::AtCeiling`].
    pub async fn extend(&self, session: &str, ms: u64) -> Result<ExtendOutcome> {
        let p = retried("extend session");
        let url = self.session_url(session, &["extend-timeout"])?;
        let body = serde_json::json!({ "duration": ms });
        let resp = self
            .send(p, || Ok(self.http.post(url.clone()).json(&body)))
            .await?;
        let status = resp.status();
        match status {
            reqwest::StatusCode::BAD_REQUEST | reqwest::StatusCode::UNPROCESSABLE_ENTITY => {
                drain_quietly(resp).await;
                return Ok(ExtendOutcome::AtCeiling);
            }
            reqwest::StatusCode::NOT_FOUND | reqwest::StatusCode::GONE => {
                drain_quietly(resp).await;
                return Err(SessionGone {
                    status: status.as_u16(),
                }
                .into());
            }
            _ => {}
        }
        self.ok(resp, p.op, false).await?;
        Ok(ExtendOutcome::Extended)
    }

    pub async fn set_network_policy(&self, name: &str, body: &serde_json::Value) -> Result<()> {
        let p = policy("set network policy");
        let name = checked_name(name)?;
        let url = self.url(&["v2", "sandboxes", name], &[], true)?;
        let body = self.json_body(&serde_json::json!({ "networkPolicy": body }), p.op)?;
        let resp = self
            .send(p, || Ok(self.http.patch(url.clone()).json(&body)))
            .await?;
        self.ok(resp, p.op, false).await?;

        let Some(view) = self.get(name, false).await? else {
            return Ok(());
        };
        if matches!(
            view.status,
            thegn_core::vercel::api::SessionStatus::Running
                | thegn_core::vercel::api::SessionStatus::Pending
        ) && let Some(session) = view.session_id.as_deref()
        {
            let policy = body.get("networkPolicy").ok_or_else(|| {
                anyhow!("{PROVIDER}: {}: request body has no networkPolicy", p.op)
            })?;
            self.set_session_network_policy(session, policy).await?;
        }
        Ok(())
    }

    /// Apply the policy to a running session. OpenAPI: POST
    /// `/v2/sandboxes/sessions/{sessionId}/network-policy`, with the policy
    /// object as the whole request body.
    pub async fn set_session_network_policy(
        &self,
        session: &str,
        body: &serde_json::Value,
    ) -> Result<()> {
        let p = policy("set session network policy");
        let url = self.session_url(session, &["network-policy"])?;
        let body = self.json_body(body, p.op)?;
        let resp = self
            .send(p, || Ok(self.http.post(url.clone()).json(&body)))
            .await?;
        self.ok(resp, p.op, false).await.map(drop)
    }

    // --- commands -----------------------------------------------------------

    /// Run a command and wait (`wait: true` is forced), streaming the NDJSON
    /// body. Returns `(exit, last out_cap stdout bytes, sanitized stderr tail)`.
    /// A missing final exit is an error; an `error` line or a line over
    /// [`LOG_LINE_CAP`] is an error.
    pub async fn run_wait_with_stderr(
        &self,
        session: &str,
        body: &serde_json::Value,
        out_cap: usize,
    ) -> Result<(i32, Vec<u8>, String)> {
        self.run_wait_with_stderr_within(session, body, out_cap, crate::provider::RUN_EXEC_DEADLINE)
            .await
    }

    /// Run a command and wait, bounded by `deadline`. The request timeout is
    /// slightly longer than the caller's deadline so the outer timeout owns
    /// the user-facing error and can distinguish an incomplete command stream.
    pub async fn run_wait_with_stderr_within(
        &self,
        session: &str,
        body: &serde_json::Value,
        out_cap: usize,
        deadline: Duration,
    ) -> Result<(i32, Vec<u8>, String)> {
        let p = policy("run command");
        let url = self.session_url(session, &["cmd"])?;
        let mut body = self.json_body(body, p.op)?;
        match body.as_object_mut() {
            Some(map) => {
                map.insert("wait".into(), serde_json::Value::Bool(true));
                map.insert("logs".into(), serde_json::Value::Bool(true));
            }
            None => bail!("{PROVIDER}: {}: body must be a JSON object", p.op),
        }
        tokio::time::timeout(deadline, async {
            let resp = self
                .send(p, || {
                    Ok(self
                        .http
                        .post(url.clone())
                        .timeout(deadline + Duration::from_secs(5))
                        .json(&body))
                })
                .await?;
            let mut resp = self.ok_session(resp, p.op).await?;

            let mut lines = LineSplitter::new(LOG_LINE_CAP);
            let mut out = TailBuffer::new(out_cap);
            let mut stderr = StderrTail::default();
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
                        Ok(LogLine::Stderr(bytes)) => stderr.push(&bytes),
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
            let code = code.ok_or_else(|| anyhow!("{PROVIDER}: run command ended without an exit status"))?;
            Ok((code, out.into_vec(), stderr.render()))
        })
        .await
        .map_err(|_| {
            anyhow!(
                "{PROVIDER}: run command exceeded its {}s deadline; the command may still be running in the sandbox",
                deadline.as_secs()
            )
        })?
    }

    /// Run a command and wait (`wait: true` is forced), streaming the NDJSON
    /// body. Returns `(exit, last out_cap stdout bytes)`; stderr is not kept.
    /// A missing final exit is an error; an `error` line or a line over
    /// [`LOG_LINE_CAP`] is an error.
    pub async fn run_wait(
        &self,
        session: &str,
        body: &serde_json::Value,
        out_cap: usize,
    ) -> Result<(i32, Vec<u8>)> {
        let (code, out, _) = self.run_wait_with_stderr(session, body, out_cap).await?;
        Ok((code, out))
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
        let resp = self.ok_session(resp, p.op).await?;
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

    /// Wait for a command's exit code: `GET …/cmd/{id}?wait=true` until the
    /// command reports `exitCode`. `Ok(None)` when `deadline` (a hard bound on
    /// the whole wait, retries and backoff included) or `WAIT_MAX_POLLS`
    /// runs out first. An exit code outside `i32` is
    /// [`EXEC_EXIT_STATUS_UNKNOWN`]; a response naming another command is an
    /// error.
    pub async fn wait_command(
        &self,
        session: &str,
        cmd: &str,
        deadline: Duration,
    ) -> Result<Option<i32>> {
        let p = retried("wait for command");
        let cmd = checked_id(cmd, "command")?;
        let url = self.url(
            &[
                "v2",
                "sandboxes",
                "sessions",
                checked_id(session, "session")?,
                "cmd",
                cmd,
            ],
            &[("wait", "true")],
            false,
        )?;
        let until = tokio::time::Instant::now() + deadline;
        let mut polls: u32 = 0;
        loop {
            let remaining = until.saturating_duration_since(tokio::time::Instant::now());
            if remaining.is_zero() || polls >= WAIT_MAX_POLLS {
                return Ok(None);
            }
            polls = polls.saturating_add(1);
            let per_request = remaining.min(WAIT_POLL_TIMEOUT);
            let polled = tokio::time::timeout(remaining, async {
                let resp = self
                    .send(p, || Ok(self.http.get(url.clone()).timeout(per_request)))
                    .await?;
                let resp = self.ok(resp, p.op, false).await?;
                response_json(resp, PROVIDER, p.op).await
            })
            .await;
            let v = match polled {
                Ok(result) => result?,
                Err(_elapsed) => return Ok(None),
            };
            let exit = command_exit(&v, cmd).map_err(|e| anyhow!("{PROVIDER}: {}: {e}", p.op))?;
            if exit.is_some() {
                return Ok(exit);
            }
            let left = until.saturating_duration_since(tokio::time::Instant::now());
            tokio::time::sleep(WAIT_POLL_INTERVAL.min(left)).await;
        }
    }

    /// Write `data` to a command's stdin; `close` ends it after the write. A
    /// 429 is retried up to 5 times (the SDK's rule); nothing else is.
    pub async fn stdin(&self, session: &str, cmd: &str, data: &[u8], close: bool) -> Result<()> {
        let p = Policy {
            attempts: STDIN_ATTEMPTS,
            rate_limited: true,
            ..policy("write command stdin")
        };
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
        // OpenAPI lists 410; the SDK treats 410 on a session as stopped (sandbox.ts isSandboxStoppedError)
        if resp.status() == reqwest::StatusCode::GONE {
            drain_quietly(resp).await;
            return Ok(());
        }
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
        self.ok_session(resp, p.op).await.map(drop)
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
        let resp = if resp.status() == reqwest::StatusCode::NOT_FOUND {
            drain_quietly(resp).await;
            return Ok(None);
        } else {
            self.ok_session(resp, p.op).await?
        };
        let mut resp = resp;
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
        self.ok_session(resp, p.op).await.map(drop)
    }

    // --- interactive ----------------------------------------------------------

    /// Mint an `/interactive` WebSocket ticket. The returned URL is `wss` on a
    /// DNS host (no IP literal, no `localhost`, no userinfo) under one of the
    /// pinned interactive host suffixes; its `token` is the ticket's own
    /// secret, never the API token.
    pub async fn interactive(&self, session: &str) -> Result<InteractiveTicket> {
        let p = policy("open interactive session");
        let url = self.session_url(session, &["interactive"])?;
        // the SDK sends `{}` (openInteractive)
        let resp = self
            .send(p, || {
                Ok(self.http.post(url.clone()).json(&serde_json::json!({})))
            })
            .await?;
        let resp = self.ok_session(resp, p.op).await?;
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
        let host = url.host_str().unwrap_or("");
        if !interactive_host_allowed(host, &self.interactive_hosts) {
            let shown: String = host.chars().take(253).collect();
            bail!("{INTERACTIVE_HOST_REFUSAL} (host {shown})");
        }
        Ok(InteractiveTicket {
            url,
            token: zeroize::Zeroizing::new(token.to_string()),
        })
    }

    // --- snapshots ------------------------------------------------------------

    /// Snapshot ids recorded for sandbox `name`, following pagination.
    pub async fn snapshots_for(&self, name: &str) -> Result<(Vec<String>, bool)> {
        let (snapshots, mut complete) = self.snapshot_pages(name).await?;
        let ids = snapshots
            .into_iter()
            .filter_map(|snapshot| {
                if valid_id(&snapshot.id) {
                    Some(snapshot.id)
                } else {
                    complete = false;
                    None
                }
            })
            .collect();
        Ok((ids, complete))
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
        self.url(&segments, &[], false)
    }

    /// Clone a caller's JSON body, refusing one that carries the token.
    fn json_body(&self, body: &serde_json::Value, op: &str) -> Result<serde_json::Value> {
        let api = self.token.expose();
        if !api.is_empty() && body.to_string().contains(api) {
            bail!("{PROVIDER}: {op}: refused a request body that carries the API token");
        }
        Ok(body.clone())
    }

    /// Refuse, gate, authorize, send; retry under `p`. Returns the final
    /// response whatever its status (the caller judges it), except a 429,
    /// which is either waited out (`p.rate_limited`, `Retry-After` absent, 0
    /// or at most [`MAX_RETRY_AFTER_S`]) or fails the call with "rate limited
    /// by Vercel; retry after Ns". An OIDC token near expiry fails before any
    /// request.
    async fn send<F>(&self, p: Policy, mut build: F) -> Result<reqwest::Response>
    where
        F: FnMut() -> Result<reqwest::RequestBuilder>,
    {
        let attempts = p.attempts.max(1);
        let mut attempt: u32 = 0;
        loop {
            attempt += 1;
            if let Some(msg) = oidc_refusal(self.token_kind, unix_now_s()) {
                bail!("{PROVIDER}: {}: {msg}", p.op);
            }
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
            let more = attempt < attempts;
            let backoff = BACKOFF * attempt;
            let wait = match result {
                Ok(resp) if resp.status() == reqwest::StatusCode::TOO_MANY_REQUESTS => {
                    let after = retry_after(resp.headers());
                    drain_quietly(resp).await;
                    match after {
                        _ if !(more && p.rate_limited) => {
                            return Err(rate_limited_error(p.op, after));
                        }
                        RetryAfter::Absent | RetryAfter::Secs(0) => backoff,
                        RetryAfter::Secs(n) if n <= MAX_RETRY_AFTER_S => Duration::from_secs(n),
                        RetryAfter::Secs(_) | RetryAfter::Unparsable => {
                            return Err(rate_limited_error(p.op, after));
                        }
                    }
                }
                Ok(resp) if more && p.transient && transient_status(resp.status()) => {
                    drain_quietly(resp).await;
                    backoff
                }
                Ok(resp) => return Ok(resp),
                Err(e) if more && p.transient && transient_err(&e) => backoff,
                Err(e) => return Err(redacted_transport_error(PROVIDER, p.op, &e)),
            };
            tokio::time::sleep(wait).await;
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

    /// Like [`Self::ok`], plus the stopped-session responses recognized by the
    /// SDK's `withResume` rule.
    async fn ok_session(
        &self,
        resp: reqwest::Response,
        op: &'static str,
    ) -> Result<reqwest::Response> {
        let status = resp.status();
        if status == reqwest::StatusCode::GONE {
            drain_quietly(resp).await;
            return Err(SessionStopped {
                op,
                status: status.as_u16(),
            }
            .into());
        }
        if status == reqwest::StatusCode::UNPROCESSABLE_ENTITY {
            let body = self.error_body(resp).await;
            if matches!(
                api_error_code(&body).as_deref(),
                Some("sandbox_stopping" | "sandbox_snapshotting")
            ) {
                return Err(SessionStopped {
                    op,
                    status: status.as_u16(),
                }
                .into());
            }
            return Err(self.error_from(status, &body, op));
        }
        self.ok(resp, op, false).await
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

    /// Read at most [`ERROR_BODY_READ`] bytes of an error response.
    async fn error_body(&self, mut resp: reqwest::Response) -> Vec<u8> {
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
        body
    }

    /// The sanitized error for `status` and an already-read `body` (same text as today).
    fn error_from(&self, status: reqwest::StatusCode, body: &[u8], op: &str) -> anyhow::Error {
        if status.is_redirection() {
            return anyhow!(
                "{PROVIDER}: {op}: redirect refused (HTTP {})",
                status.as_u16()
            );
        }
        let text = self.snippet(&String::from_utf8_lossy(body));
        if text.is_empty() {
            anyhow!("{PROVIDER}: {op} returned HTTP {}", status.as_u16())
        } else {
            anyhow!("{PROVIDER}: {op} returned HTTP {}: {text}", status.as_u16())
        }
    }

    async fn status_error(&self, resp: reqwest::Response, op: &str) -> anyhow::Error {
        let status = resp.status();
        if status.is_redirection() {
            return self.error_from(status, &[], op);
        }
        let body = self.error_body(resp).await;
        self.error_from(status, &body, op)
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

/// The exit code a get-command response reports (`{"command": {…}}` or bare):
/// `None` while `exitCode` is absent or null. A response naming a different
/// command is refused; a code outside `i32` is [`EXEC_EXIT_STATUS_UNKNOWN`].
fn command_exit(v: &serde_json::Value, cmd: &str) -> Result<Option<i32>, &'static str> {
    let c = v.get("command").filter(|c| c.is_object()).unwrap_or(v);
    if !c.is_object() {
        return Err("response has no command object");
    }
    if let Some(id) = c.get("id").and_then(|id| id.as_str())
        && id != cmd
    {
        return Err("response names a different command");
    }
    match c.get("exitCode") {
        None | Some(serde_json::Value::Null) => Ok(None),
        Some(code) => {
            let code = code.as_i64().ok_or("exitCode is not an integer")?;
            Ok(Some(
                i32::try_from(code).unwrap_or(EXEC_EXIT_STATUS_UNKNOWN),
            ))
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

/// A name the provider may address: refuses anything else before any request
/// (the one check; the provider module uses it too).
pub(super) fn checked_name(name: &str) -> Result<&str> {
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

/// `error.code` from a Vercel error body (`{"error":{"code":..}}`, the SDK's
/// `APIError.json` shape). `None` unless it is 1..=64 chars of [A-Za-z0-9_-].
fn api_error_code(body: &[u8]) -> Option<String> {
    let value = serde_json::from_slice::<serde_json::Value>(body).ok()?;
    let code = value.get("error")?.get("code")?.as_str()?;
    if (1..=64).contains(&code.len())
        && code
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
    {
        Some(code.to_string())
    } else {
        None
    }
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
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    async fn loopback_response(response: Vec<u8>, hold: Duration) -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind loopback server");
        let addr = listener.local_addr().expect("loopback address");
        tokio::spawn(async move {
            let Ok((mut stream, _)) = listener.accept().await else {
                return;
            };
            let mut request = [0u8; 4096];
            if stream.read(&mut request).await.is_err() {
                return;
            }
            if stream.write_all(&response).await.is_err() || stream.flush().await.is_err() {
                return;
            }
            tokio::time::sleep(hold).await;
        });
        format!("http://{addr}")
    }

    fn content_length_response(body: &str) -> Vec<u8> {
        format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/x-ndjson\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        )
        .into_bytes()
    }

    fn stalled_chunked_response(line: &str) -> Vec<u8> {
        format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/x-ndjson\r\nTransfer-Encoding: chunked\r\nConnection: keep-alive\r\n\r\n{:X}\r\n{line}\r\n",
            line.len()
        )
        .into_bytes()
    }

    #[tokio::test]
    async fn run_wait_with_stderr_within_times_out_stalled_stream() {
        let line = "{\"stream\":\"stdout\",\"data\":\"x\"}\n";
        let base = loopback_response(stalled_chunked_response(line), Duration::from_secs(2)).await;
        let client = VercelClient::new(
            &base,
            VercelToken::new("t".into()),
            Scope {
                team_id: "team_t".into(),
                project_id: "prj_t".into(),
            },
        )
        .expect("loopback client");

        let err = client
            .run_wait_with_stderr_within(
                "sess_1",
                &serde_json::json!({}),
                64,
                Duration::from_millis(300),
            )
            .await
            .unwrap_err();
        assert!(format!("{err:#}").contains("exceeded its"));
    }

    #[tokio::test]
    async fn missing_exit_is_an_error() {
        let body = "{\"stream\":\"stdout\",\"data\":\"x\"}\n";
        let base = loopback_response(content_length_response(body), Duration::ZERO).await;
        let client = VercelClient::new(
            &base,
            VercelToken::new("t".into()),
            Scope {
                team_id: "team_t".into(),
                project_id: "prj_t".into(),
            },
        )
        .expect("loopback client");

        let err = client
            .run_wait_with_stderr_within(
                "sess_1",
                &serde_json::json!({}),
                64,
                Duration::from_secs(1),
            )
            .await
            .unwrap_err();
        assert!(format!("{err:#}").contains("without an exit status"));
    }

    #[tokio::test]
    async fn exit_line_is_returned() {
        let body = concat!(
            "{\"stream\":\"stdout\",\"data\":\"out\"}\n",
            "{\"stream\":\"stderr\",\"data\":\"err\"}\n",
            "{\"command\":{\"id\":\"cmd_1\",\"exitCode\":3}}\n",
        );
        let base = loopback_response(content_length_response(body), Duration::ZERO).await;
        let client = VercelClient::new(
            &base,
            VercelToken::new("t".into()),
            Scope {
                team_id: "team_t".into(),
                project_id: "prj_t".into(),
            },
        )
        .expect("loopback client");

        let result = client
            .run_wait_with_stderr_within(
                "sess_1",
                &serde_json::json!({}),
                64,
                Duration::from_secs(1),
            )
            .await
            .expect("command result");
        assert_eq!(result, (3, b"out".to_vec(), "err".to_string()));
    }

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
    fn command_exit_reads_wrapped_bare_null_and_out_of_range_codes() {
        let v = |j: serde_json::Value| command_exit(&j, "cmd_1");
        assert_eq!(
            v(serde_json::json!({"command": {"id": "cmd_1", "exitCode": 2}})),
            Ok(Some(2))
        );
        assert_eq!(
            v(serde_json::json!({"id": "cmd_1", "exitCode": 0})),
            Ok(Some(0))
        );
        assert_eq!(
            v(serde_json::json!({"command": {"id": "cmd_1", "exitCode": null}})),
            Ok(None)
        );
        assert_eq!(v(serde_json::json!({"command": {"id": "cmd_1"}})), Ok(None));
        assert_eq!(
            v(serde_json::json!({"command": {"exitCode": 9_000_000_000_i64}})),
            Ok(Some(EXEC_EXIT_STATUS_UNKNOWN))
        );
        assert!(v(serde_json::json!({"command": {"id": "cmd_9", "exitCode": 0}})).is_err());
        assert!(v(serde_json::json!({"command": {"id": "cmd_1", "exitCode": "0"}})).is_err());
        assert!(v(serde_json::json!(null)).is_err());
    }

    #[test]
    fn retry_after_parse_is_checked() {
        assert_eq!(parse_retry_after(Some("1")), RetryAfter::Secs(1));
        assert_eq!(parse_retry_after(Some(" 20 ")), RetryAfter::Secs(20));
        assert_eq!(parse_retry_after(Some("0")), RetryAfter::Secs(0));
        for bad in [
            "",
            "-1",
            "+5",
            "1.5",
            "Wed, 21 Oct 2015 07:28:00 GMT",
            "99999999999999999999999",
        ] {
            assert_eq!(
                parse_retry_after(Some(bad)),
                RetryAfter::Unparsable,
                "{bad}"
            );
        }
        assert_eq!(parse_retry_after(None), RetryAfter::Unparsable);
        let none = reqwest::header::HeaderMap::new();
        assert_eq!(retry_after(&none), RetryAfter::Absent);
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
