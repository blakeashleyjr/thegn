//! Managed-sandbox **providers** (the `provider` placement) — the async seam for
//! platforms that spawn dev environments via an HTTP API or SDK (Daytona,
//! Codespaces, Modal, …), as opposed to a static `exec_command` CLI.
//!
//! The host's `[env.<name>.provider]` config can drive a sandbox two ways:
//!  1. **CLI** — `up_command`/`exec_command` run a vendor CLI (no code here;
//!     handled by `thegn_core::placement::Placement::ensure`).
//!  2. **API** — `api_base` + `api_key_env` point at a REST provider; this module
//!     creates/destroys/lists sandboxes over HTTP and resolves an exec handle.
//!
//! Request/response shaping is split into **pure** functions (`*_url`,
//! `create_body`, `parse_*`) so they're unit-testable without a live endpoint;
//! the async trait methods are thin reqwest wrappers around them. The reference
//! impl is [`DaytonaProvider`] (Daytona's documented `POST /api/sandbox` + Bearer
//! auth); field names are defensive and easy to retarget to another platform.

use anyhow::{Context, Result, anyhow};
use futures_util::future::BoxFuture;
use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, LazyLock, Mutex};
use thegn_core::remote::SshTarget;

/// Build the shared reqwest client for a provider. The default `Client::new()`
/// has **no timeouts**, so a stalled TCP handshake or a hung TLS negotiation to
/// the provider's control plane blocks the calling request *forever* — which is
/// exactly what strands the "Sync agent logins" provisioning step on a bad
/// network. Control requests have a total deadline; individual file-transfer
/// requests override it with their separate, longer deadline. An invalid
/// client policy is fatal rather than falling back to an unbounded client.
/// Redirects are rejected so authenticated requests cannot be replayed to
/// another host.
pub(crate) fn provider_http_client() -> reqwest::Client {
    reqwest::Client::builder()
        .connect_timeout(std::time::Duration::from_secs(15))
        .timeout(CONTROL_TIMEOUT)
        // Reap idle keep-alive sockets so a half-open connection to a sprite that
        // was suspended/rotated doesn't get reused into a silent stall.
        .pool_idle_timeout(std::time::Duration::from_secs(90))
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .expect("provider HTTP client policy must be supported")
}

/// Maximum decoded control-plane JSON response. File contents use their
/// separate streaming API.
pub(crate) const CONTROL_BODY_LIMIT: usize = 2 * 1024 * 1024;
pub(crate) const CONTROL_GLOBAL_CONCURRENCY: usize = 16;
pub(crate) const CONTROL_PER_AUTHORITY_CONCURRENCY: usize = 4;
/// File contents use a separate, longer policy than metadata/control calls.
const FILE_TRANSFER_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(600);
const FILE_TRANSFER_BODY_LIMIT: usize = 256 * 1024 * 1024;
const TRANSFER_GLOBAL_CONCURRENCY: usize = 16;
const TRANSFER_PER_AUTHORITY_CONCURRENCY: usize = 4;

/// Read a control response in chunks so unknown-length responses cannot grow
/// an unbounded in-memory JSON value. reqwest exposes decoded chunks here, so
/// this cap applies after supported content decoding as well.
pub(crate) async fn response_json(
    response: reqwest::Response,
    provider: &str,
    operation: &str,
) -> Result<serde_json::Value> {
    let bytes = response_bytes(response, provider, operation, CONTROL_BODY_LIMIT).await?;
    if bytes.is_empty() {
        return Ok(serde_json::Value::Null);
    }
    serde_json::from_slice(&bytes)
        .with_context(|| format!("{provider}: decode {operation} response JSON"))
}

pub(crate) async fn response_text(
    response: reqwest::Response,
    provider: &str,
    operation: &str,
) -> Result<String> {
    String::from_utf8(response_bytes(response, provider, operation, CONTROL_BODY_LIMIT).await?)
        .with_context(|| format!("{provider}: decode {operation} response text"))
}

async fn response_bytes(
    mut response: reqwest::Response,
    provider: &str,
    operation: &str,
    limit: usize,
) -> Result<Vec<u8>> {
    let mut bytes = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|error| redacted_transport_error(provider, operation, &error))?
    {
        anyhow::ensure!(
            chunk.len() <= limit.saturating_sub(bytes.len()),
            "{provider}: {operation} response exceeded the {limit}-byte body limit"
        );
        bytes.extend_from_slice(&chunk);
    }
    Ok(bytes)
}

/// A stable, value-free description of a failed provider HTTP exchange.
///
/// `reqwest::Error` carries the request URL in its `Display`, and anyhow's
/// context chains keep that error as a `source()`, so `{:#}`/`{:?}` renderings
/// would echo an endpoint (and any query or path secret in it). The original
/// error is therefore dropped here and replaced by its category alone.
pub(crate) fn redacted_transport_error(
    provider: &str,
    method: &str,
    error: &reqwest::Error,
) -> anyhow::Error {
    let kind = if error.is_timeout() {
        "timed out"
    } else if error.is_connect() {
        "could not connect"
    } else if error.is_redirect() {
        "redirect refused"
    } else if error.is_body() || error.is_decode() {
        "response body failed"
    } else if error.is_builder() {
        "request could not be built"
    } else {
        "request failed"
    };
    anyhow::anyhow!("{provider}: {method} provider endpoint {kind}")
}

#[path = "provider_sessions.rs"]
mod sessions;

/// Per-request deadline for the small JSON **control-plane** calls (create,
/// list, destroy, fs list, network policy, checkpoint metadata). The shared
/// client applies this deadline by default; long file transfers override it
/// with their separate policy. Capping control calls turns an infinite stall
/// into a fast, retryable error.
pub(crate) const CONTROL_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

/// Bounds simultaneous provider control calls across provider instances.
pub(crate) struct ControlGate {
    global: Arc<tokio::sync::Semaphore>,
    per_authority: Mutex<HashMap<String, Arc<tokio::sync::Semaphore>>>,
    per_cap: usize,
}

pub(crate) struct ControlPermit {
    _authority: tokio::sync::OwnedSemaphorePermit,
    _global: tokio::sync::OwnedSemaphorePermit,
}

impl ControlGate {
    pub(crate) fn new(global: usize, per: usize) -> Self {
        Self {
            global: Arc::new(tokio::sync::Semaphore::new(global)),
            per_authority: Mutex::new(HashMap::new()),
            per_cap: per,
        }
    }

    fn authority_gate(&self, authority: &str) -> Arc<tokio::sync::Semaphore> {
        let mut gates = self
            .per_authority
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        Arc::clone(
            gates
                .entry(authority.to_string())
                .or_insert_with(|| Arc::new(tokio::sync::Semaphore::new(self.per_cap))),
        )
    }

    pub(crate) async fn acquire(&self, authority: &str) -> Result<ControlPermit> {
        let authority = self.authority_gate(authority);
        let authority_permit =
            tokio::time::timeout(CONTROL_TIMEOUT, Arc::clone(&authority).acquire_owned())
                .await
                .map_err(|_| anyhow!("provider concurrency cap reached"))?
                .map_err(|_| anyhow!("provider concurrency gate closed"))?;
        let global_permit =
            tokio::time::timeout(CONTROL_TIMEOUT, Arc::clone(&self.global).acquire_owned())
                .await
                .map_err(|_| anyhow!("provider concurrency cap reached"))?
                .map_err(|_| anyhow!("provider concurrency gate closed"))?;
        Ok(ControlPermit {
            _authority: authority_permit,
            _global: global_permit,
        })
    }

    #[cfg(test)]
    pub(crate) fn try_acquire(&self, authority: &str) -> Result<ControlPermit> {
        let authority = self.authority_gate(authority);
        let authority_permit = Arc::clone(&authority)
            .try_acquire_owned()
            .map_err(|_| anyhow!("provider concurrency cap reached"))?;
        let global_permit = Arc::clone(&self.global)
            .try_acquire_owned()
            .map_err(|_| anyhow!("provider concurrency cap reached"))?;
        Ok(ControlPermit {
            _authority: authority_permit,
            _global: global_permit,
        })
    }
}

pub(crate) static CONTROL_GATE: LazyLock<ControlGate> = LazyLock::new(|| {
    ControlGate::new(
        CONTROL_GLOBAL_CONCURRENCY,
        CONTROL_PER_AUTHORITY_CONCURRENCY,
    )
});

/// Bounds simultaneous long-running file transfers independently of control calls.
static TRANSFER_GATE: LazyLock<ControlGate> = LazyLock::new(|| {
    ControlGate::new(
        TRANSFER_GLOBAL_CONCURRENCY,
        TRANSFER_PER_AUTHORITY_CONCURRENCY,
    )
});

/// Parse and admit a configured provider authority without allowing credentials
/// or request modifiers to be smuggled into authenticated URLs.
pub(crate) fn validate_api_base(raw: &str) -> Result<String> {
    let url = reqwest::Url::parse(raw).map_err(|_| anyhow!("invalid provider api_base"))?;
    let host = url
        .host_str()
        .ok_or_else(|| anyhow!("provider api_base requires a host"))?;
    let authority = url
        .as_str()
        .split_once("://")
        .map(|(_, rest)| rest)
        .unwrap_or_default()
        .split('/')
        .next()
        .unwrap_or_default();
    anyhow::ensure!(
        url.username().is_empty() && url.password().is_none() && !authority.contains('@'),
        "api_base has embedded credentials"
    );
    anyhow::ensure!(
        url.query().is_none(),
        "provider api_base cannot have a query"
    );
    anyhow::ensure!(
        url.fragment().is_none(),
        "provider api_base cannot have a fragment"
    );
    let loopback = host.eq_ignore_ascii_case("localhost")
        || host
            .trim_start_matches('[')
            .trim_end_matches(']')
            .parse::<std::net::IpAddr>()
            .is_ok_and(|address| address.is_loopback());
    anyhow::ensure!(
        url.scheme() == "https" || (url.scheme() == "http" && loopback),
        "provider api_base must use https (http is allowed only for loopback)"
    );
    let mut base = url.to_string();
    while base.ends_with('/') {
        base.pop();
    }
    Ok(base)
}

pub(crate) fn authority_key(base: &str) -> Result<String> {
    let url = reqwest::Url::parse(base).map_err(|_| anyhow!("invalid provider api_base"))?;
    let host = url
        .host_str()
        .ok_or_else(|| anyhow!("provider api_base requires a host"))?;
    let port = url
        .port_or_known_default()
        .ok_or_else(|| anyhow!("provider api_base requires a port"))?;
    let host = if host.contains(':') && !host.starts_with('[') {
        format!("[{host}]")
    } else {
        host.to_string()
    };
    Ok(format!("{}://{host}:{port}", url.scheme()))
}

/// Control-plane retry budget for the idempotent create/list calls: a transient
/// 5xx/429/408 or a request timeout (our [`CONTROL_TIMEOUT`] firing) can clear on
/// a retry; other 4xx are terminal (see [`transient_status`]/[`transient_err`]).
const CONTROL_ATTEMPTS: u32 = 3;
const CONTROL_BACKOFF: std::time::Duration = std::time::Duration::from_millis(500);

/// How to exec into a provider sandbox once it exists.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExecKind {
    /// The provider exposes ssh; attach via the normal ssh/mosh transport.
    Ssh(SshTarget),
    /// Exec through a vendor CLI prefix, e.g. `["daytona", "ssh", "<id>", "--"]`.
    Command(Vec<String>),
}

/// A live (or discovered) provider sandbox.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SandboxHandle {
    pub id: String,
    pub exec: ExecKind,
}

/// A managed-sandbox provider. Object-safe async seam (mirrors
/// `thegn_core::seam`): methods return [`BoxFuture`]s (not native `async fn`)
/// so the trait stays `dyn`-compatible; concrete impls own their HTTP client.
pub trait RemoteProvider: Send + Sync {
    /// Create (and start) a new sandbox, returning a handle to exec into.
    fn create<'a>(&'a self) -> BoxFuture<'a, Result<SandboxHandle>>;
    /// Destroy a sandbox by id (idempotent — a missing sandbox is success).
    fn destroy<'a>(&'a self, id: &'a str) -> BoxFuture<'a, Result<()>>;
    /// List existing sandbox ids (for discovery / reuse).
    fn list<'a>(&'a self) -> BoxFuture<'a, Result<Vec<String>>>;
}

/// Daytona (`github.com/daytonaio/daytona`) REST provider. Targets the
/// documented `POST {api_base}/sandbox` (Bearer auth); `snapshot` selects the
/// image/template to create from.
pub struct DaytonaProvider {
    api_base: String,
    base_error: Option<String>,
    token: String,
    snapshot: String,
    client: reqwest::Client,
}

impl DaytonaProvider {
    /// `api_base` like `https://app.daytona.io/api`; `token` is the API key
    /// (already resolved from `api_key_env`); `snapshot` is the image/template.
    pub fn new(api_base: &str, token: &str, snapshot: &str) -> Self {
        let (api_base, base_error) = match validate_api_base(api_base) {
            Ok(base) => (base, None),
            Err(error) => (String::new(), Some(error.to_string())),
        };
        DaytonaProvider {
            api_base,
            base_error,
            token: token.to_string(),
            snapshot: snapshot.to_string(),
            client: provider_http_client(),
        }
    }

    // --- pure request/response shaping (unit-tested) ------------------------

    fn base(&self) -> Result<&str> {
        match self.base_error.as_deref() {
            Some(error) => Err(anyhow!("{error}")),
            None => Ok(&self.api_base),
        }
    }

    fn authority(&self) -> Result<String> {
        authority_key(self.base()?)
    }

    fn sandbox_url(&self) -> Result<String> {
        Ok(format!("{}/sandbox", self.base()?))
    }

    fn sandbox_id_url(&self, id: &str) -> Result<String> {
        Ok(format!("{}/sandbox/{id}", self.base()?))
    }

    /// The create body. `snapshot` empty ⇒ the provider's default image.
    fn create_body(&self) -> serde_json::Value {
        let mut body = serde_json::Map::new();
        if !self.snapshot.trim().is_empty() {
            body.insert("snapshot".into(), self.snapshot.clone().into());
        }
        serde_json::Value::Object(body)
    }

    /// Extract a sandbox id from a create/list element. Daytona returns the id
    /// at the top level; tolerate a `{ "sandbox": { "id" } }` envelope too.
    fn parse_id(v: &serde_json::Value) -> Option<String> {
        v.get("id")
            .or_else(|| v.get("sandbox").and_then(|s| s.get("id")))
            .and_then(|i| i.as_str())
            .map(str::to_string)
    }

    /// Parse a list response (an array, or `{ "sandboxes": [...] }`).
    fn parse_list(v: &serde_json::Value) -> Vec<String> {
        let arr = v
            .as_array()
            .cloned()
            .or_else(|| v.get("sandboxes").and_then(|s| s.as_array()).cloned())
            .unwrap_or_default();
        arr.iter().filter_map(Self::parse_id).collect()
    }

    /// The exec handle for a created sandbox id: the `daytona` CLI ssh bridge
    /// (handles per-sandbox auth without exposing keys here).
    fn exec_for(id: &str) -> ExecKind {
        ExecKind::Command(vec![
            "daytona".into(),
            "ssh".into(),
            id.to_string(),
            "--".into(),
        ])
    }
}

impl RemoteProvider for DaytonaProvider {
    fn create<'a>(&'a self) -> BoxFuture<'a, Result<SandboxHandle>> {
        Box::pin(async move {
            let _permit = CONTROL_GATE.acquire(&self.authority()?).await?;
            // Bounded per-request deadline: the shared client only caps *connection*
            // setup, so a control plane that accepts the connection then withholds
            // the response would hang provisioning forever (the "hung on startup"
            // failure CONTROL_TIMEOUT documents). Sprites already guards this; Daytona
            // was skipped.
            let resp = self
                .client
                .post(self.sandbox_url()?)
                .bearer_auth(&self.token)
                .timeout(CONTROL_TIMEOUT)
                .json(&self.create_body())
                .send()
                .await
                .context("daytona: POST /sandbox")?;
            let status = resp.status();
            let body = response_json(resp, "daytona", "POST /sandbox").await?;
            if !status.is_success() {
                return Err(anyhow!("daytona create failed ({status})"));
            }
            let id = Self::parse_id(&body)
                .ok_or_else(|| anyhow!("daytona create: response did not contain a sandbox id"))?;
            let exec = Self::exec_for(&id);
            Ok(SandboxHandle { id, exec })
        })
    }

    fn destroy<'a>(&'a self, id: &'a str) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            let _permit = CONTROL_GATE.acquire(&self.authority()?).await?;
            let resp = self
                .client
                .delete(self.sandbox_id_url(id)?)
                .bearer_auth(&self.token)
                .timeout(CONTROL_TIMEOUT)
                .send()
                .await
                .context("daytona: DELETE /sandbox/{id}")?;
            // 404 = already gone — treat as success (idempotent teardown).
            if resp.status().is_success() || resp.status() == reqwest::StatusCode::NOT_FOUND {
                Ok(())
            } else {
                Err(anyhow!("daytona destroy failed ({})", resp.status()))
            }
        })
    }

    fn list<'a>(&'a self) -> BoxFuture<'a, Result<Vec<String>>> {
        Box::pin(async move {
            let _permit = CONTROL_GATE.acquire(&self.authority()?).await?;
            let resp = self
                .client
                .get(self.sandbox_url()?)
                .bearer_auth(&self.token)
                .timeout(CONTROL_TIMEOUT)
                .send()
                .await
                .context("daytona: GET /sandbox")?;
            let body = response_json(resp, "daytona", "GET /sandbox").await?;
            Ok(Self::parse_list(&body))
        })
    }
}

/// Sprites (`sprites.dev`, Fly.io) REST provider. Firecracker microVMs with a
/// persistent fs, ~300ms live checkpoints, and L3 egress policies. Sprites are
/// **named** (caller-chosen), not server-assigned ids: create posts
/// `POST {api_base}/sprites {"name": …}`; teardown is `DELETE …/sprites/{name}`.
/// Exec uses the `sprite` CLI bridge (`sprite exec -s <name> --tty --`), which
/// carries per-sprite auth without exposing the token here. `api_base` defaults
/// to `https://api.sprites.dev/v1`; the token comes from `SPRITES_TOKEN`.
pub struct SpritesProvider {
    api_base: String,
    base_error: Option<String>,
    token: String,
    /// The sprite name to create/attach (caller-chosen; sprites are persistent).
    name: String,
    /// Exact value-free custody namespace for managed SSH teardown. Direct
    /// service consumers may omit it; in that case teardown is allowed only
    /// when the provider+instance row is unique across accounts.
    custody_accounts: Vec<String>,
    client: reqwest::Client,
}

/// Whether a non-OK control-plane HTTP status is worth retrying: transient
/// server/overload conditions (any 5xx, 429 Too Many Requests, 408 Request
/// Timeout) can clear on a retry; other 4xx are the caller's fault and won't
/// change on retry. Shared by destroy (a stuck teardown leaks a paid sandbox)
/// and the idempotent create/list calls. (2xx and 404 are handled as
/// success/already-gone before this is consulted.)
pub(crate) fn transient_status(status: reqwest::StatusCode) -> bool {
    status.is_server_error()
        || status == reqwest::StatusCode::TOO_MANY_REQUESTS
        || status == reqwest::StatusCode::REQUEST_TIMEOUT
}

/// Whether a reqwest **transport** error (no HTTP status — e.g. our
/// [`CONTROL_TIMEOUT`] fired, or the connection dropped) is worth retrying.
pub(crate) fn transient_err(e: &reqwest::Error) -> bool {
    e.is_timeout() || e.is_connect()
}

impl SpritesProvider {
    /// `api_base` like `https://api.sprites.dev/v1` (empty ⇒ that default);
    /// `token` is the resolved `SPRITES_TOKEN`; `name` is the sprite to manage.
    pub fn new(api_base: &str, token: &str, name: &str) -> Self {
        let base = api_base.trim().trim_end_matches('/');
        let configured_base = if base.is_empty() {
            "https://api.sprites.dev/v1"
        } else {
            base
        };
        let (api_base, base_error) = match validate_api_base(configured_base) {
            Ok(base) => (base, None),
            Err(error) => (String::new(), Some(error.to_string())),
        };
        SpritesProvider {
            api_base,
            base_error,
            token: token.to_string(),
            name: name.trim().to_string(),
            custody_accounts: Vec::new(),
            client: provider_http_client(),
        }
    }

    pub fn with_custody_accounts(mut self, accounts: impl IntoIterator<Item = String>) -> Self {
        self.custody_accounts = accounts.into_iter().collect();
        self
    }

    fn custody_account(&self, id: &str) -> Result<Option<String>> {
        if self.custody_accounts.is_empty() {
            return thegn_core::managed_ssh::read_unique_instance("sprites", id)
                .map(|record| record.map(|record| record.account));
        }
        let mut matches = Vec::new();
        for account in &self.custody_accounts {
            if thegn_core::managed_ssh::read("sprites", account, id)?.is_some() {
                matches.push(account.clone());
            }
        }
        anyhow::ensure!(
            matches.len() <= 1,
            "sprites: managed SSH custody for {id} is ambiguous across configured scope namespaces"
        );
        Ok(matches.pop())
    }

    // --- pure request/response shaping (unit-tested) ------------------------

    fn base(&self) -> Result<&str> {
        match self.base_error.as_deref() {
            Some(error) => Err(anyhow!("{error}")),
            None => Ok(&self.api_base),
        }
    }

    fn authority(&self) -> Result<String> {
        authority_key(self.base()?)
    }

    fn sprites_url(&self) -> Result<String> {
        Ok(format!("{}/sprites", self.base()?))
    }

    fn sprite_name_url(&self, name: &str) -> Result<String> {
        Ok(format!("{}/sprites/{name}", self.base()?))
    }

    /// The create body — sprites are named by the caller.
    fn create_body(name: &str) -> serde_json::Value {
        serde_json::json!({ "name": name })
    }

    /// Pull a sprite name from a create/list element (`{ "name": … }`, tolerating
    /// a `{ "sprite": { "name" } }` envelope).
    fn parse_name(v: &serde_json::Value) -> Option<String> {
        v.get("name")
            .or_else(|| v.get("sprite").and_then(|s| s.get("name")))
            .and_then(|n| n.as_str())
            .map(str::to_string)
    }

    /// Parse a list response (an array, or `{ "sprites": [...] }`).
    fn parse_list(v: &serde_json::Value) -> Vec<String> {
        let arr = v
            .as_array()
            .cloned()
            .or_else(|| v.get("sprites").and_then(|s| s.as_array()).cloned())
            .unwrap_or_default();
        arr.iter().filter_map(Self::parse_name).collect()
    }

    /// The exec handle for a sprite: the `sprite` CLI exec bridge (PTY-capable).
    fn exec_for(name: &str) -> ExecKind {
        ExecKind::Command(vec![
            "sprite".into(),
            "exec".into(),
            "-s".into(),
            name.to_string(),
            "--tty".into(),
            "--".into(),
        ])
    }
}

impl RemoteProvider for SpritesProvider {
    fn create<'a>(&'a self) -> BoxFuture<'a, Result<SandboxHandle>> {
        Box::pin(async move {
            let _permit = CONTROL_GATE.acquire(&self.authority()?).await?;
            if self.name.is_empty() {
                return Err(anyhow!(
                    "sprites: set `[env.<name>.provider] id = \"<sprite-name>\"` — sprites are named"
                ));
            }
            // Bounded per-request timeout + a small retry budget: a cold-booting
            // control plane can stall or transiently 5xx, and an untimed create is
            // exactly what hung provisioning "forever" on startup.
            let mut last: Option<String> = None;
            for attempt in 0..CONTROL_ATTEMPTS {
                match self
                    .client
                    .post(self.sprites_url()?)
                    .bearer_auth(&self.token)
                    .timeout(CONTROL_TIMEOUT)
                    .json(&Self::create_body(&self.name))
                    .send()
                    .await
                {
                    Ok(resp) => {
                        let status = resp.status();
                        if status.is_success() {
                            let body = response_json(resp, "sprites", "POST /sprites").await?;
                            // The server echoes the name; fall back to the requested one.
                            let name = Self::parse_name(&body).unwrap_or_else(|| self.name.clone());
                            let exec = Self::exec_for(&name);
                            return Ok(SandboxHandle { id: name, exec });
                        }
                        last = Some(status.to_string());
                        if !transient_status(status) {
                            break;
                        }
                    }
                    Err(e) => {
                        last = Some(e.to_string());
                        if !transient_err(&e) {
                            break;
                        }
                    }
                }
                if attempt + 1 < CONTROL_ATTEMPTS {
                    tokio::time::sleep(CONTROL_BACKOFF).await;
                }
            }
            Err(anyhow!(
                "sprites create failed ({})",
                last.unwrap_or_else(|| "no response".into())
            ))
        })
    }

    fn destroy<'a>(&'a self, id: &'a str) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            let _permit = CONTROL_GATE.acquire(&self.authority()?).await?;
            // Validate custody before the irreversible remote delete. A corrupt
            // row may be the only evidence of an authorized key and must never
            // be collapsed into "missing" by lifecycle cleanup.
            let custody_account = self.custody_account(id)?;
            // A deleted worktree must not keep billing, so a transient 5xx/429/408 on
            // teardown (the observed "sprites destroy failed (500)") is retried a few
            // times with a short backoff rather than immediately leaking a paid
            // sandbox. 404 (already gone) and 2xx are terminal-OK; other 4xx are the
            // caller's fault and won't change on retry, so we stop early.
            const ATTEMPTS: u32 = 3;
            let mut last_status = None;
            for attempt in 0..ATTEMPTS {
                let resp = self
                    .client
                    .delete(self.sprite_name_url(id)?)
                    .bearer_auth(&self.token)
                    .timeout(CONTROL_TIMEOUT)
                    .send()
                    .await
                    .context("sprites: DELETE /sprites/{name}")?;
                let status = resp.status();
                // 404 = already gone — idempotent teardown.
                if status.is_success() || status == reqwest::StatusCode::NOT_FOUND {
                    if let Some(account) = custody_account.as_deref() {
                        thegn_core::managed_ssh::record_revoked("sprites", account, id)
                            .context("sprites: retire managed SSH custody")?;
                    }
                    return Ok(());
                }
                last_status = Some(status);
                if !transient_status(status) {
                    break;
                }
                if attempt + 1 < ATTEMPTS {
                    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
                }
            }
            Err(anyhow!(
                "sprites destroy failed ({})",
                last_status
                    .map(|s| s.to_string())
                    .unwrap_or_else(|| "no response".into())
            ))
        })
    }

    fn list<'a>(&'a self) -> BoxFuture<'a, Result<Vec<String>>> {
        Box::pin(async move {
            let _permit = CONTROL_GATE.acquire(&self.authority()?).await?;
            // Bounded + retried like `create`: `ensure_exists` calls this first, so an
            // untimed list would strand provisioning on a stalled control plane.
            let mut last: Option<String> = None;
            for attempt in 0..CONTROL_ATTEMPTS {
                match self
                    .client
                    .get(self.sprites_url()?)
                    .bearer_auth(&self.token)
                    .timeout(CONTROL_TIMEOUT)
                    .send()
                    .await
                {
                    Ok(resp) => {
                        let status = resp.status();
                        if status.is_success() {
                            let body = response_json(resp, "sprites", "GET /sprites").await?;
                            return Ok(Self::parse_list(&body));
                        }
                        last = Some(status.to_string());
                        if !transient_status(status) {
                            break;
                        }
                    }
                    Err(e) => {
                        last = Some(e.to_string());
                        if !transient_err(&e) {
                            break;
                        }
                    }
                }
                if attempt + 1 < CONTROL_ATTEMPTS {
                    tokio::time::sleep(CONTROL_BACKOFF).await;
                }
            }
            Err(anyhow!(
                "sprites list failed ({})",
                last.unwrap_or_else(|| "no response".into())
            ))
        })
    }
}

// ===========================================================================
// Capability-segmented provider axes. A provider implements `RemoteProvider`
// (lifecycle, required) plus whichever optional sub-traits below it supports;
// the `Provider` enum is the capability facade and declares support via
// `caps()`. Adding a provider = one enum variant + its sub-trait impls + a
// `caps()` arm + the accessor arms (`remote`/`files_ops`/…) that route the
// facade's methods to the variant's trait impls as `&dyn` objects.
// ===========================================================================

/// A network egress rule: a domain pattern (`github.com`, `*.npmjs.org`, `*`) and
/// an action. Mirrors the Sprites `policy/network` rule shape.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct PolicyRule {
    pub domain: String,
    pub action: String, // "allow" | "deny"
}

/// Lower thegn's `network_allow`/`network_block` domain lists to a provider
/// network policy. **Pure** (unit-tested). Order encodes the precedence:
/// deny rules first (a block beats an allow), then allow rules, then a trailing
/// default-`deny *` **only when an allow-list is present** (an allow-list means
/// "deny everything else") — exactly the local DNS-filter semantics
/// ([`thegn_core::dns_filter`]). Empty + empty ⇒ no rules (allow all).
pub fn rules_from(allow: &[String], block: &[String]) -> Vec<PolicyRule> {
    let mut rules = Vec::new();
    for d in block {
        let d = d.trim();
        if !d.is_empty() {
            rules.push(PolicyRule {
                domain: d.to_string(),
                action: "deny".into(),
            });
        }
    }
    let mut any_allow = false;
    for d in allow {
        let d = d.trim();
        if !d.is_empty() {
            rules.push(PolicyRule {
                domain: d.to_string(),
                action: "allow".into(),
            });
            any_allow = true;
        }
    }
    if any_allow {
        rules.push(PolicyRule {
            domain: "*".into(),
            action: "deny".into(),
        });
    }
    rules
}

/// Egress-policy translation — realizes `EgressKind::Translate`
/// (`thegn_core::capabilities`): lower allow/block lists to the provider's own
/// network controls, since we can't run our DNS filter inside the provider's box.
pub trait ProviderEgress: Send + Sync {
    fn set_network_policy<'a>(
        &'a self,
        id: &'a str,
        allow: &'a [String],
        block: &'a [String],
    ) -> BoxFuture<'a, Result<()>>;
    fn get_network_policy<'a>(&'a self, id: &'a str) -> BoxFuture<'a, Result<Vec<PolicyRule>>>;
}

impl SpritesProvider {
    fn policy_network_url(&self, name: &str) -> Result<String> {
        Ok(format!("{}/sprites/{name}/policy/network", self.base()?))
    }
}

impl ProviderEgress for SpritesProvider {
    fn set_network_policy<'a>(
        &'a self,
        id: &'a str,
        allow: &'a [String],
        block: &'a [String],
    ) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            let _permit = CONTROL_GATE.acquire(&self.authority()?).await?;
            let body = serde_json::json!({ "rules": rules_from(allow, block) });
            let resp = self
                .client
                .post(self.policy_network_url(id)?)
                .bearer_auth(&self.token)
                .timeout(CONTROL_TIMEOUT)
                .json(&body)
                .send()
                .await
                .context("sprites: POST /policy/network")?;
            if resp.status().is_success() {
                Ok(())
            } else {
                let st = resp.status();
                Err(anyhow!("sprites set network policy failed ({st})"))
            }
        })
    }

    fn get_network_policy<'a>(&'a self, id: &'a str) -> BoxFuture<'a, Result<Vec<PolicyRule>>> {
        Box::pin(async move {
            let _permit = CONTROL_GATE.acquire(&self.authority()?).await?;
            let resp = self
                .client
                .get(self.policy_network_url(id)?)
                .bearer_auth(&self.token)
                .timeout(CONTROL_TIMEOUT)
                .send()
                .await
                .context("sprites: GET /policy/network")?;
            let status = resp.status();
            if !status.is_success() {
                return Err(anyhow!("sprites GET /policy/network failed ({status})"));
            }
            let body = response_json(resp, "sprites", "GET /policy/network").await?;
            Ok(body
                .get("rules")
                .and_then(|r| r.as_array())
                .map(|a| {
                    a.iter()
                        .filter_map(|v| {
                            Some(PolicyRule {
                                domain: v.get("domain")?.as_str()?.to_string(),
                                action: v.get("action")?.as_str()?.to_string(),
                            })
                        })
                        .collect()
                })
                .unwrap_or_default())
        })
    }
}

/// A checkpoint/snapshot of a sandbox.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CheckpointInfo {
    pub id: String,
    pub label: Option<String>,
}

/// Snapshot / restore — realizes `Capabilities::can_snapshot`. A live, transactional
/// capture of the sandbox's fs+memory (Sprites' headline ~300ms checkpoints).
pub trait ProviderCheckpoints: Send + Sync {
    /// Create a checkpoint, returning its id.
    fn checkpoint<'a>(
        &'a self,
        id: &'a str,
        label: Option<&'a str>,
    ) -> BoxFuture<'a, Result<String>>;
    fn list_checkpoints<'a>(&'a self, id: &'a str) -> BoxFuture<'a, Result<Vec<CheckpointInfo>>>;
    fn restore<'a>(&'a self, id: &'a str, checkpoint: &'a str) -> BoxFuture<'a, Result<()>>;
}

impl SpritesProvider {
    /// List endpoint (plural): `…/checkpoints`.
    fn checkpoints_url(&self, name: &str) -> Result<String> {
        Ok(format!("{}/sprites/{name}/checkpoints", self.base()?))
    }
    /// Create endpoint (SINGULAR): `…/checkpoint` — returns an NDJSON stream.
    fn checkpoint_create_url(&self, name: &str) -> Result<String> {
        Ok(format!("{}/sprites/{name}/checkpoint", self.base()?))
    }
    fn restore_url(&self, name: &str, cp: &str) -> Result<String> {
        Ok(format!(
            "{}/sprites/{name}/checkpoints/{cp}/restore",
            self.base()?
        ))
    }
    /// Parse a checkpoint list element (`{ "id", "comment"? }`).
    fn parse_checkpoint(v: &serde_json::Value) -> Option<CheckpointInfo> {
        let id = v.get("id").and_then(|i| i.as_str())?.to_string();
        let label = v
            .get("comment")
            .or_else(|| v.get("label"))
            .and_then(|c| c.as_str())
            .filter(|s| !s.is_empty())
            .map(str::to_string);
        Some(CheckpointInfo { id, label })
    }
    /// Extract the new checkpoint id from the create stream's NDJSON lines. The
    /// `complete` message reads `Checkpoint <id> created successfully`; an info
    /// line reads `  ID: <id>`. Try both.
    fn parse_checkpoint_stream(body: &str) -> Option<String> {
        let mut from_complete = None;
        for line in body.lines() {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else {
                continue;
            };
            let data = v.get("data").and_then(|d| d.as_str()).unwrap_or("");
            if let Some(rest) = data.trim().strip_prefix("ID:") {
                return Some(rest.trim().to_string());
            }
            if v.get("type").and_then(|t| t.as_str()) == Some("complete")
                && let Some(rest) = data.strip_prefix("Checkpoint ")
            {
                from_complete = rest.split_whitespace().next().map(str::to_string);
            }
        }
        from_complete
    }
}

impl ProviderCheckpoints for SpritesProvider {
    fn checkpoint<'a>(
        &'a self,
        id: &'a str,
        label: Option<&'a str>,
    ) -> BoxFuture<'a, Result<String>> {
        Box::pin(async move {
            let _permit = CONTROL_GATE.acquire(&self.authority()?).await?;
            let mut body = serde_json::Map::new();
            if let Some(l) = label.map(str::trim).filter(|l| !l.is_empty()) {
                body.insert("comment".into(), l.into());
            }
            let resp = self
                .client
                .post(self.checkpoint_create_url(id)?)
                .bearer_auth(&self.token)
                .timeout(CONTROL_TIMEOUT)
                .json(&serde_json::Value::Object(body))
                .send()
                .await
                .context("sprites: POST /checkpoint")?;
            let status = resp.status();
            if !status.is_success() {
                return Err(anyhow!("sprites checkpoint failed ({status})"));
            }
            let text = response_text(resp, "sprites", "POST /checkpoint").await?;
            Self::parse_checkpoint_stream(&text)
                .ok_or_else(|| anyhow!("sprites checkpoint response did not contain an id"))
        })
    }

    fn list_checkpoints<'a>(&'a self, id: &'a str) -> BoxFuture<'a, Result<Vec<CheckpointInfo>>> {
        Box::pin(async move {
            let _permit = CONTROL_GATE.acquire(&self.authority()?).await?;
            let resp = self
                .client
                .get(self.checkpoints_url(id)?)
                .bearer_auth(&self.token)
                .timeout(CONTROL_TIMEOUT)
                .send()
                .await
                .context("sprites: GET /checkpoints")?;
            let v = response_json(resp, "sprites", "GET /checkpoints").await?;
            let arr = v
                .as_array()
                .cloned()
                .or_else(|| v.get("checkpoints").and_then(|c| c.as_array()).cloned())
                .unwrap_or_default();
            Ok(arr.iter().filter_map(Self::parse_checkpoint).collect())
        })
    }

    fn restore<'a>(&'a self, id: &'a str, checkpoint: &'a str) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            let _permit = CONTROL_GATE.acquire(&self.authority()?).await?;
            let resp = self
                .client
                .post(self.restore_url(id, checkpoint)?)
                .bearer_auth(&self.token)
                .timeout(CONTROL_TIMEOUT)
                .send()
                .await
                .context("sprites: POST /checkpoints/{id}/restore")?;
            if resp.status().is_success() {
                Ok(())
            } else {
                Err(anyhow!("sprites restore failed ({})", resp.status()))
            }
        })
    }
}

/// One entry in a sandbox directory listing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileEntry {
    pub name: String,
    pub is_dir: bool,
    pub size: u64,
}

pub(crate) const DOWNLOAD_MAX_DEPTH: usize = 64;
pub(crate) const DOWNLOAD_MAX_FILES: usize = 10_000;
pub(crate) const DOWNLOAD_MAX_FILE_BYTES: u64 = 64 * 1024 * 1024;
pub(crate) const DOWNLOAD_MAX_TOTAL_BYTES: u64 = 512 * 1024 * 1024;

fn valid_remote_component(name: &str) -> bool {
    !name.is_empty()
        && name != "."
        && name != ".."
        && !name.chars().any(|ch| matches!(ch, '/' | '\\' | '\0'))
        && !name.chars().any(char::is_control)
        && !(name.len() >= 2
            && name.as_bytes()[0].is_ascii_alphabetic()
            && name.as_bytes()[1] == b':')
}

fn ensure_download_dir(path: &Path) -> Result<()> {
    match std::fs::symlink_metadata(path) {
        Ok(meta) if meta.file_type().is_symlink() || !meta.is_dir() => {
            anyhow::bail!(
                "download destination component is not a real directory: {}",
                path.display()
            )
        }
        Ok(_) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            std::fs::create_dir(path).with_context(|| format!("create {}", path.display()))?;
            let meta = std::fs::symlink_metadata(path)?;
            anyhow::ensure!(
                meta.is_dir() && !meta.file_type().is_symlink(),
                "download destination changed during directory creation: {}",
                path.display()
            );
            Ok(())
        }
        Err(error) => Err(error).with_context(|| format!("inspect {}", path.display())),
    }
}

fn publish_download_file(path: &Path, bytes: &[u8]) -> Result<()> {
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let parent = path
        .parent()
        .context("download destination has no parent")?;
    let metadata = std::fs::symlink_metadata(path);
    let existing_permissions = match metadata {
        Ok(meta) if meta.file_type().is_symlink() || !meta.is_file() => {
            anyhow::bail!(
                "download destination is not a regular file: {}",
                path.display()
            )
        }
        Ok(_) => {
            thegn_core::fsperm::open_regular_file_nofollow(path)?;
            Some(std::fs::symlink_metadata(path)?.permissions())
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => return Err(error).with_context(|| format!("inspect {}", path.display())),
    };
    let nonce = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let temp = parent.join(format!(".thegn-sync-{}-{nonce}.tmp", std::process::id()));
    let staged = (|| -> Result<()> {
        use std::io::Write;
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temp)
            .with_context(|| format!("stage {}", path.display()))?;
        if let Some(permissions) = existing_permissions {
            file.set_permissions(permissions)?;
        }
        file.write_all(bytes)?;
        Ok(())
    })();
    if let Err(error) = staged {
        let cleanup = std::fs::remove_file(&temp);
        if let Err(cleanup_error) = cleanup {
            return Err(error)
                .with_context(|| format!("staging failed and cleanup failed ({cleanup_error})"));
        }
        return Err(error);
    }
    if let Err(error) = thegn_core::fsperm::rename_replace(&temp, path) {
        let cleanup = std::fs::remove_file(&temp);
        if let Err(cleanup_error) = cleanup {
            return Err(error).with_context(|| {
                format!("publish failed and staging cleanup failed ({cleanup_error})")
            });
        }
        return Err(error).with_context(|| format!("publish {}", path.display()));
    }
    Ok(())
}

/// Join a remote base dir and a relative path into a clean remote path.
pub(crate) fn join_remote(base: &str, rel: &str) -> String {
    let base = base.trim_end_matches('/');
    let rel = rel.trim_start_matches('/');
    if rel.is_empty() {
        base.to_string()
    } else {
        format!("{base}/{rel}")
    }
}

/// Collect files under `root` as `(absolute, relative)` pairs for upload. Pure +
/// iterative (no recursion); skips `.git` (never sync the repo metadata into a
/// sandbox). Returns an error only if `root` can't be read.
fn collect_files(root: &Path) -> Result<Vec<(std::path::PathBuf, String)>> {
    use ignore::WalkBuilder;
    // A missing source is an error (don't silently sync nothing).
    anyhow::ensure!(
        root.exists(),
        "source dir does not exist: {}",
        root.display()
    );
    // Gitignore-aware: upload the worktree's tracked + untracked WORK but SKIP the
    // build/junk the repo ignores (target/, result/, .direnv/, node_modules/…) —
    // otherwise a `data = "sync"` projection pushes gigabytes of artifacts over the
    // per-file fs API. `.git` is always skipped (the sandbox has its own from the
    // clone). Dotfiles are KEPT (`hidden(false)`) since `.envrc`/`.config` are real
    // state; `require_git(false)` applies `.gitignore` even though a worktree's
    // `.git` is a file pointing into the canonical repo.
    let mut out = Vec::new();
    let walk = WalkBuilder::new(root)
        .hidden(false)
        .git_ignore(true)
        .git_exclude(true)
        .git_global(true)
        .require_git(false)
        .filter_entry(|e| e.file_name() != ".git")
        .build();
    for entry in walk.flatten() {
        if entry.file_type().is_some_and(|t| t.is_file())
            && let Ok(rel) = entry.path().strip_prefix(root)
        {
            out.push((
                entry.path().to_path_buf(),
                rel.to_string_lossy().replace('\\', "/"),
            ));
        }
    }
    Ok(out)
}

/// Remote filesystem access — backs a provider `sync` projection (push the local
/// worktree into the sandbox fs, pull changes back). `read`/`write`/`list_dir`/
/// `delete` are provider-specific; the recursive `upload_dir`/`download_dir` are
/// generic defaults over them, so any provider gets directory sync for free.
pub trait ProviderFiles: Send + Sync {
    fn read<'a>(&'a self, id: &'a str, path: &'a str) -> BoxFuture<'a, Result<Vec<u8>>>;
    fn write<'a>(&'a self, id: &'a str, path: &'a str, data: &'a [u8])
    -> BoxFuture<'a, Result<()>>;
    fn list_dir<'a>(&'a self, id: &'a str, path: &'a str) -> BoxFuture<'a, Result<Vec<FileEntry>>>;
    fn delete<'a>(&'a self, id: &'a str, path: &'a str) -> BoxFuture<'a, Result<()>>;

    /// Write `data` as an **executable** (mode 0755 where the provider's file API
    /// supports a mode). Default delegates to [`write`](Self::write) — the bytes
    /// land, the exec bit is best-effort; providers with a mode override this.
    fn write_exec<'a>(
        &'a self,
        id: &'a str,
        path: &'a str,
        data: &'a [u8],
    ) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move { self.write(id, path, data).await })
    }

    /// Recursively upload `local` into the sandbox at `remote` (skips `.git`).
    fn upload_dir<'a>(
        &'a self,
        id: &'a str,
        local: &'a Path,
        remote: &'a str,
    ) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            for (abs, rel) in collect_files(local)? {
                let data =
                    std::fs::read(&abs).with_context(|| format!("read {}", abs.display()))?;
                self.write(id, &join_remote(remote, &rel), &data).await?;
            }
            Ok(())
        })
    }

    /// Recursively download the sandbox `remote` dir into `local`.
    fn download_dir<'a>(
        &'a self,
        id: &'a str,
        remote: &'a str,
        local: &'a Path,
    ) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            anyhow::ensure!(
                local.is_dir(),
                "download destination is not a directory: {}",
                local.display()
            );
            anyhow::ensure!(
                !std::fs::symlink_metadata(local)?.file_type().is_symlink(),
                "download root is a symlink: {}",
                local.display()
            );
            let mut stack = vec![String::new()]; // rel dirs to visit
            let mut seen = std::collections::HashSet::new();
            let mut file_count = 0usize;
            let mut total_bytes = 0u64;
            while let Some(rel) = stack.pop() {
                let depth = if rel.is_empty() {
                    0
                } else {
                    rel.split('/').count()
                };
                anyhow::ensure!(
                    depth <= DOWNLOAD_MAX_DEPTH,
                    "provider listing exceeded maximum directory depth"
                );
                let listing = self.list_dir(id, &join_remote(remote, &rel)).await?;
                let mut siblings = std::collections::HashSet::new();
                for e in &listing {
                    if e.name == ".git" {
                        continue;
                    }
                    anyhow::ensure!(
                        valid_remote_component(&e.name),
                        "provider returned an unsafe path component"
                    );
                    anyhow::ensure!(
                        siblings.insert(e.name.clone()),
                        "provider returned a duplicate directory entry"
                    );
                    let child = if rel.is_empty() {
                        e.name.clone()
                    } else {
                        format!("{rel}/{}", e.name)
                    };
                    anyhow::ensure!(
                        seen.insert(child.clone()),
                        "provider returned a repeated path"
                    );
                    let child_depth = depth + 1;
                    anyhow::ensure!(
                        child_depth <= DOWNLOAD_MAX_DEPTH,
                        "provider listing exceeded maximum directory depth"
                    );
                }
                for e in listing {
                    if e.name == ".git" {
                        continue;
                    }
                    let child = if rel.is_empty() {
                        e.name.clone()
                    } else {
                        format!("{rel}/{}", e.name)
                    };
                    let mut dest = local.to_path_buf();
                    for component in child.split('/') {
                        dest.push(component);
                    }
                    if e.is_dir {
                        ensure_download_dir(&dest)?;
                        stack.push(child);
                    } else {
                        file_count += 1;
                        anyhow::ensure!(
                            file_count <= DOWNLOAD_MAX_FILES,
                            "provider listing exceeded maximum file count"
                        );
                        let data = self.read(id, &join_remote(remote, &child)).await?;
                        let actual_size = data.len() as u64;
                        anyhow::ensure!(
                            actual_size <= DOWNLOAD_MAX_FILE_BYTES,
                            "provider file exceeded per-file size limit"
                        );
                        total_bytes = total_bytes
                            .checked_add(actual_size)
                            .context("provider transfer byte count overflow")?;
                        anyhow::ensure!(
                            total_bytes <= DOWNLOAD_MAX_TOTAL_BYTES,
                            "provider transfer exceeded total byte limit"
                        );
                        if actual_size != e.size {
                            tracing::warn!(target: "thegn::provider", path = %child,
                                listed_size = e.size, actual_size, "provider file changed between listing and read; entry skipped");
                            continue;
                        }
                        publish_download_file(&dest, &data)?;
                    }
                }
            }
            Ok(())
        })
    }
}

impl SpritesProvider {
    /// The fs operation endpoint: `…/sprites/{name}/fs/{op}` (op = read|write|list
    /// |delete). The target path is a **query param** (`?path=…&workingDir=/`),
    /// not part of the URL — matching the live v1 API (and the `sprites-py` SDK).
    fn fs_op_url(&self, name: &str, op: &str) -> Result<String> {
        Ok(format!("{}/sprites/{name}/fs/{op}", self.base()?))
    }

    /// Poll a cheap liveness probe until the freshly-created sprite's guest
    /// filesystem API answers, or `budget` elapses. A brand-new sprite cold-boots
    /// (Firecracker microVM) for a few seconds; without this gate the first
    /// marker `read` / provisioning exec raced the boot and — before per-request
    /// timeouts — hung the whole open **forever**. `list_dir("/")` makes no
    /// assumption about the (undocumented) sprite-status JSON: it succeeds only
    /// once the in-guest fs service is up. Bounded by construction: each probe is
    /// itself capped by [`CONTROL_TIMEOUT`], and the loop stops at `budget`.
    async fn wait_ready(&self, id: &str, budget: std::time::Duration) -> Result<()> {
        const PROBE_BACKOFF: std::time::Duration = std::time::Duration::from_secs(2);
        let start = std::time::Instant::now();
        loop {
            let remaining = budget.saturating_sub(start.elapsed());
            if remaining.is_zero() {
                return Err(anyhow!(
                    "sprite {id} did not become ready within {}s",
                    budget.as_secs()
                ));
            }
            let err = match tokio::time::timeout(remaining, self.list_dir(id, "/")).await {
                Ok(Ok(_)) => return Ok(()),
                Ok(Err(error)) => error,
                Err(_) => {
                    return Err(anyhow!(
                        "sprite {id} did not become ready within {}s (probe timed out)",
                        budget.as_secs()
                    ));
                }
            };
            let remaining = budget.saturating_sub(start.elapsed());
            if remaining.is_zero() {
                return Err(anyhow!(
                    "sprite {id} did not become ready within {}s: {err}",
                    budget.as_secs()
                ));
            }
            tokio::time::sleep(PROBE_BACKOFF.min(remaining)).await;
        }
    }
    /// PUT `/fs/write` with an explicit unix `mode` (octal string). `write`
    /// uses `0644`; `write_exec` uses `0755` so a pushed binary is runnable.
    async fn write_with_mode(&self, id: &str, path: &str, data: &[u8], mode: &str) -> Result<()> {
        let _permit = TRANSFER_GATE.acquire(&self.authority()?).await?;
        anyhow::ensure!(
            data.len() <= FILE_TRANSFER_BODY_LIMIT,
            "sprites file write exceeded the {FILE_TRANSFER_BODY_LIMIT}-byte limit"
        );
        let resp = self
            .client
            .put(self.fs_op_url(id, "write")?)
            .bearer_auth(&self.token)
            .timeout(FILE_TRANSFER_TIMEOUT)
            .query(&[
                ("path", path),
                ("workingDir", "/"),
                ("mode", mode),
                ("mkdirParents", "true"),
            ])
            .header("Content-Type", "application/octet-stream")
            .body(data.to_vec())
            .send()
            .await
            .context("sprites: PUT /fs/write")?;
        if resp.status().is_success() {
            Ok(())
        } else {
            Err(anyhow!("sprites write {path} failed ({})", resp.status()))
        }
    }

    /// Parse a listing entry: `{name, size, isDir}` (the server uses camelCase
    /// `isDir`; tolerate `is_dir` too).
    fn parse_entry(v: &serde_json::Value) -> Option<FileEntry> {
        let name = v.get("name")?.as_str()?.to_string();
        let is_dir = v
            .get("isDir")
            .or_else(|| v.get("is_dir"))
            .and_then(|b| b.as_bool())
            .unwrap_or(false);
        let size = v.get("size").and_then(|s| s.as_u64()).unwrap_or(0);
        Some(FileEntry { name, is_dir, size })
    }
}

impl ProviderFiles for SpritesProvider {
    fn read<'a>(&'a self, id: &'a str, path: &'a str) -> BoxFuture<'a, Result<Vec<u8>>> {
        Box::pin(async move {
            let _permit = TRANSFER_GATE.acquire(&self.authority()?).await?;
            let resp = self
                .client
                .get(self.fs_op_url(id, "read")?)
                .bearer_auth(&self.token)
                .timeout(FILE_TRANSFER_TIMEOUT)
                .query(&[("path", path), ("workingDir", "/")])
                .send()
                .await
                .context("sprites: GET /fs/read")?;
            if !resp.status().is_success() {
                return Err(anyhow!("sprites read {path} failed ({})", resp.status()));
            }
            response_bytes(resp, "sprites", "GET /fs/read", FILE_TRANSFER_BODY_LIMIT).await
        })
    }

    fn write<'a>(
        &'a self,
        id: &'a str,
        path: &'a str,
        data: &'a [u8],
    ) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move { self.write_with_mode(id, path, data, "0644").await })
    }

    fn write_exec<'a>(
        &'a self,
        id: &'a str,
        path: &'a str,
        data: &'a [u8],
    ) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move { self.write_with_mode(id, path, data, "0755").await })
    }

    fn list_dir<'a>(&'a self, id: &'a str, path: &'a str) -> BoxFuture<'a, Result<Vec<FileEntry>>> {
        Box::pin(async move {
            let _permit = CONTROL_GATE.acquire(&self.authority()?).await?;
            let resp = self
                .client
                .get(self.fs_op_url(id, "list")?)
                .bearer_auth(&self.token)
                .timeout(CONTROL_TIMEOUT)
                .query(&[("path", path), ("workingDir", "/")])
                .send()
                .await
                .context("sprites: GET /fs/list")?;
            if !resp.status().is_success() {
                return Err(anyhow!("sprites list {path} failed ({})", resp.status()));
            }
            let v = response_json(resp, "sprites", "GET /fs/list").await?;
            let arr = v
                .get("entries")
                .and_then(|e| e.as_array())
                .cloned()
                .or_else(|| v.as_array().cloned())
                .unwrap_or_default();
            Ok(arr.iter().filter_map(Self::parse_entry).collect())
        })
    }

    fn delete<'a>(&'a self, id: &'a str, path: &'a str) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            let _permit = CONTROL_GATE.acquire(&self.authority()?).await?;
            let resp = self
                .client
                .delete(self.fs_op_url(id, "delete")?)
                .bearer_auth(&self.token)
                .timeout(CONTROL_TIMEOUT)
                .query(&[("path", path), ("workingDir", "/"), ("recursive", "true")])
                .send()
                .await
                .context("sprites: DELETE /fs/delete")?;
            if resp.status().is_success() || resp.status() == reqwest::StatusCode::NOT_FOUND {
                Ok(())
            } else {
                Err(anyhow!("sprites delete {path} failed ({})", resp.status()))
            }
        })
    }
}

/// What a provider supports beyond lifecycle — drives CLI/UI gating so the rest
/// of the system degrades gracefully (no snapshot command for a provider that
/// can't checkpoint, etc.). Flags flip on as each axis is implemented.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ProviderCaps {
    pub files: bool,
    pub checkpoints: bool,
    pub egress: bool,
    pub exec_api: bool,
    /// An idle sandbox self-suspends for free (billed only while awake). Mirrors
    /// `thegn_core::config::provider_scale_to_zero` by kind — the warm pool
    /// parks scale-to-zero spares indefinitely instead of aging them out.
    pub scale_to_zero: bool,
}

/// Whether the named provider has a native exec API (PTY-over-WebSocket), so an
/// env can prefer it over the vendor CLI *without* first constructing a
/// (token-requiring) [`Provider`]. Mirrors `Provider::caps().exec_api` by name.
pub fn exec_api_by_name(provider: &str) -> bool {
    thegn_core::config::EnvProviderKind::of(provider).exec_api()
}

fn sha256_hex(data: &[u8]) -> String {
    use sha2::Digest;
    format!("{:x}", sha2::Sha256::digest(data))
}

#[derive(Debug)]
enum ExecInstallError {
    StageWrite(anyhow::Error),
    StageRead(anyhow::Error),
    StageVerification(String),
    Publish(anyhow::Error),
    PublishedVerification(String),
}

impl std::fmt::Display for ExecInstallError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::StageWrite(error) => write!(f, "stage bridge executable: {error:#}"),
            Self::StageRead(error) => write!(
                f,
                "staged bridge executable failed verification ({error:#})"
            ),
            Self::StageVerification(reason) => {
                write!(f, "staged bridge executable failed verification ({reason})")
            }
            Self::Publish(error) => write!(f, "publish bridge executable: {error:#}"),
            Self::PublishedVerification(reason) => write!(
                f,
                "published bridge executable failed verification ({reason})"
            ),
        }
    }
}

impl std::error::Error for ExecInstallError {}

async fn install_executable_verified<P, Fut>(
    files: &dyn ProviderFiles,
    id: &str,
    path: &str,
    data: &[u8],
    publish: P,
) -> Result<bool>
where
    P: FnOnce(String, String) -> Fut,
    Fut: std::future::Future<Output = Result<()>>,
{
    let want = sha256_hex(data);
    if let Ok(remote) = files.read(id, path).await
        && remote.len() == data.len()
        && sha256_hex(&remote) == want
    {
        return Ok(false);
    }

    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |duration| duration.as_nanos());
    let stage = format!("{path}.tg-stage-{}-{nanos}", std::process::id());
    if let Err(error) = files.write_exec(id, &stage, data).await {
        if let Err(error) = files.delete(id, &stage).await {
            tracing::debug!(target: "thegn::provider", %error, "best-effort staged executable cleanup failed");
        }
        return Err(ExecInstallError::StageWrite(error).into());
    }

    match files.read(id, &stage).await {
        Ok(bytes) if bytes.len() == data.len() && sha256_hex(&bytes) == want => {}
        Ok(bytes) => {
            if let Err(error) = files.delete(id, &stage).await {
                tracing::debug!(target: "thegn::provider", %error, "best-effort staged executable cleanup failed");
            }
            return Err(ExecInstallError::StageVerification(format!(
                "expected {} bytes with SHA-256 {want}, got {} bytes with SHA-256 {}",
                data.len(),
                bytes.len(),
                sha256_hex(&bytes)
            ))
            .into());
        }
        Err(error) => {
            if let Err(error) = files.delete(id, &stage).await {
                tracing::debug!(target: "thegn::provider", %error, "best-effort staged executable cleanup failed");
            }
            return Err(ExecInstallError::StageRead(error).into());
        }
    };

    if let Err(error) = publish(stage.clone(), path.to_string()).await {
        if let Err(error) = files.delete(id, &stage).await {
            tracing::debug!(target: "thegn::provider", %error, "best-effort staged executable cleanup failed");
        }
        return Err(ExecInstallError::Publish(error).into());
    }

    match files.read(id, path).await {
        Ok(bytes) if bytes.len() == data.len() && sha256_hex(&bytes) == want => {}
        Ok(bytes) => {
            if let Err(error) = files.delete(id, &stage).await {
                tracing::debug!(target: "thegn::provider", %error, "best-effort staged executable cleanup failed");
            }
            return Err(ExecInstallError::PublishedVerification(format!(
                "expected {} bytes with SHA-256 {want}, got {} bytes with SHA-256 {}",
                data.len(),
                bytes.len(),
                sha256_hex(&bytes)
            ))
            .into());
        }
        Err(error) => {
            if let Err(error) = files.delete(id, &stage).await {
                tracing::debug!(target: "thegn::provider", %error, "best-effort staged executable cleanup failed");
            }
            return Err(ExecInstallError::PublishedVerification(format!(
                "read-back failed: {error:#}"
            ))
            .into());
        }
    };

    let fp_path = format!("{path}.fp");
    if let Err(error) = files.delete(id, &fp_path).await {
        tracing::debug!(target: "thegn::provider", %error, "best-effort legacy executable fingerprint cleanup failed");
    }
    Ok(true)
}

// ===========================================================================
// Native exec (the `exec_api` capability) — a generic, provider-agnostic PTY
// stream so an interactive pane attaches over the provider's API instead of a
// local PTY child running a vendor CLI. The transport is decoupled via channels
// (async-fn-in-trait isn't object-safe, so there's no `dyn ExecStream`): a
// provider's `open_exec`/`attach_exec` spawns a driver task that bridges its
// socket to these channels, and the host wires them into a pane.
// ===========================================================================

/// What to run for a native exec session (provider-agnostic).
#[derive(Debug, Clone)]
pub struct ExecSpec {
    /// The command + args to run in the sandbox (e.g. the login shell).
    pub argv: Vec<String>,
    /// Allocate a PTY (interactive panes always do).
    pub tty: bool,
    pub cols: u16,
    pub rows: u16,
    /// Extra environment as `KEY=VALUE` pairs.
    pub env: Vec<(String, String)>,
    /// Working directory inside the sandbox. Providers without a cwd param bake
    /// it into `argv` upstream; `None` ⇒ the sandbox default.
    pub cwd: Option<String>,
}

/// One server→client event from a native exec session. PTY mode merges stderr
/// into the pty stream, so only `Stdout`/`Exit` occur.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExecFrame {
    Stdout(Vec<u8>),
    Exit(i32),
}

/// Exit status used when a provider reports an absent or invalid exit code.
/// Matches the host daemon adapter's unknown-exit sentinel.
pub const EXEC_EXIT_STATUS_UNKNOWN: i32 = 254;

/// One client→server control message for a native exec session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExecControl {
    Stdin(Vec<u8>),
    Resize { cols: u16, rows: u16 },
    Close,
}

/// A live native exec session: a provider-agnostic handle built from channels.
/// The provider spawns a driver task that bridges the underlying transport (e.g.
/// a WebSocket) to these channels until the socket closes or `Close` is sent.
pub struct ExecSession {
    /// Server→client output and the terminal exit.
    pub frames: tokio::sync::mpsc::Receiver<ExecFrame>,
    /// Client→server stdin/resize/close.
    pub control: tokio::sync::mpsc::Sender<ExecControl>,
    /// The provider session id once the server announces it (for reattach). It
    /// arrives asynchronously on connect, so it's a watch the caller reads when
    /// persisting — `None` until the session-info frame lands.
    pub session_id: tokio::sync::watch::Receiver<Option<String>>,
}

/// A raw bidirectional byte stream to an in-sandbox `host:port`, tunneled over the
/// provider's TCP-over-WebSocket proxy. Unlike [`ExecSession`] there is no framing
/// — bytes are forwarded verbatim — so it can carry an arbitrary protocol (e.g. an
/// SSH connection to an in-sandbox `sshd`). The driver task lives on the ambient
/// runtime until either side closes.
pub struct ProxyStream {
    /// Server→client bytes (data from the in-sandbox service).
    pub rx: tokio::sync::mpsc::Receiver<Vec<u8>>,
    /// Client→server bytes (data to the in-sandbox service). Dropping it (or
    /// sending nothing) and draining `rx` to `None` ends the stream.
    pub tx: tokio::sync::mpsc::Sender<Vec<u8>>,
}

/// Rewrite an `http(s)` API base to its `ws(s)` scheme for the WebSocket handshake.
fn ws_scheme(api_base: &str) -> String {
    if let Some(rest) = api_base.strip_prefix("https://") {
        format!("wss://{rest}")
    } else if let Some(rest) = api_base.strip_prefix("http://") {
        format!("ws://{rest}")
    } else {
        api_base.to_string()
    }
}

const EXEC_CWD_NON_TTY_SCRIPT: &str = "cd -- \"$1\" || exit 125; shift; exec \"$@\"";
const EXEC_CWD_TTY_SCRIPT: &str = "cd -- \"$1\" 2>/dev/null; shift; exec \"$@\"";
const EXEC_WS_MAX_MESSAGE_SIZE: usize = 16 << 20;
const EXEC_WS_MAX_FRAME_SIZE: usize = 4 << 20;
const EXEC_WS_MAX_WRITE_BUFFER_SIZE: usize = 8 << 20;
const PROXY_CONNECT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(15);

#[derive(Debug)]
struct ProxyConnectTimeout {
    deadline: std::time::Duration,
}

impl std::fmt::Display for ProxyConnectTimeout {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "sprites: proxy connect timed out after {:?}",
            self.deadline
        )
    }
}

impl std::error::Error for ProxyConnectTimeout {}

/// The wrapper could not enter the requested directory; the command did not run.
pub const EXEC_CWD_FAILED_STATUS: i32 = 125;

fn exec_argv(spec: &ExecSpec) -> Result<Vec<String>> {
    let Some(cwd) = spec.cwd.as_deref().filter(|cwd| !cwd.trim().is_empty()) else {
        return Ok(spec.argv.clone());
    };
    anyhow::ensure!(
        !cwd.contains('\0'),
        "sprites: exec cwd must be an absolute path without NUL"
    );
    if spec.tty && !cwd.starts_with('/') {
        return Ok(spec.argv.clone());
    }
    anyhow::ensure!(
        cwd.starts_with('/'),
        "sprites: exec cwd must be an absolute path without NUL"
    );
    anyhow::ensure!(
        !spec.argv.is_empty(),
        "sprites: exec argv must not be empty when cwd is set"
    );

    let script = if spec.tty {
        EXEC_CWD_TTY_SCRIPT
    } else {
        EXEC_CWD_NON_TTY_SCRIPT
    };
    let mut argv = Vec::with_capacity(spec.argv.len() + 5);
    argv.extend([
        "/bin/sh".to_string(),
        "-c".to_string(),
        script.to_string(),
        "thegn-exec".to_string(),
        cwd.to_string(),
    ]);
    argv.extend(spec.argv.iter().cloned());
    Ok(argv)
}

fn exec_ws_config() -> tokio_tungstenite::tungstenite::protocol::WebSocketConfig {
    tokio_tungstenite::tungstenite::protocol::WebSocketConfig::default()
        .max_message_size(Some(EXEC_WS_MAX_MESSAGE_SIZE))
        .max_frame_size(Some(EXEC_WS_MAX_FRAME_SIZE))
        .max_write_buffer_size(EXEC_WS_MAX_WRITE_BUFFER_SIZE)
}

fn next_connect_wait(
    elapsed: std::time::Duration,
    budget: std::time::Duration,
    step: std::time::Duration,
) -> Option<std::time::Duration> {
    (elapsed < budget).then(|| step.min(budget - elapsed))
}

/// Build the Sprites open-exec WSS URL: `wss://…/sprites/{id}/exec?cmd=…&tty=…`.
fn exec_ws_url(api_base: &str, id: &str, spec: &ExecSpec) -> Result<String> {
    let base = ws_scheme(api_base);
    let mut url = reqwest::Url::parse(&format!("{base}/sprites/{id}/exec"))
        .context("sprites: bad exec url")?;
    {
        let mut q = url.query_pairs_mut();
        for a in exec_argv(spec)? {
            q.append_pair("cmd", &a);
        }
        q.append_pair("tty", if spec.tty { "true" } else { "false" });
        q.append_pair("stdin", "true");
        q.append_pair("cols", &spec.cols.to_string());
        q.append_pair("rows", &spec.rows.to_string());
        for (k, v) in &spec.env {
            q.append_pair("env", &format!("{k}={v}"));
        }
    }
    Ok(url.to_string())
}

/// Build the Sprites reattach WSS URL: `wss://…/sprites/{id}/exec/{session}?cols=…&rows=…`.
fn attach_ws_url(api_base: &str, id: &str, session: &str, cols: u16, rows: u16) -> Result<String> {
    let base = ws_scheme(api_base);
    let mut url = reqwest::Url::parse(&format!("{base}/sprites/{id}/exec/{session}"))
        .context("sprites: bad attach url")?;
    {
        let mut q = url.query_pairs_mut();
        q.append_pair("cols", &cols.to_string());
        q.append_pair("rows", &rows.to_string());
    }
    Ok(url.to_string())
}

/// Build the Sprites TCP-proxy WSS URL: `wss://…/sprites/{id}/proxy`.
fn proxy_ws_url(api_base: &str, id: &str) -> Result<String> {
    let base = ws_scheme(api_base);
    Ok(reqwest::Url::parse(&format!("{base}/sprites/{id}/proxy"))
        .context("sprites: bad proxy url")?
        .to_string())
}

/// The init frame the Sprites `/proxy` socket expects before it becomes a raw
/// relay: `{"host":"…","port":N}`.
fn proxy_init_json(host: &str, port: u16) -> String {
    format!(
        "{{\"host\":{},\"port\":{port}}}",
        serde_json::to_string(host).unwrap_or_else(|_| "\"127.0.0.1\"".into())
    )
}

/// The resize control frame the Sprites exec socket expects.
fn resize_json(cols: u16, rows: u16) -> String {
    format!("{{\"type\":\"resize\",\"cols\":{cols},\"rows\":{rows}}}")
}

// --- non-PTY (tty=false) binary stream framing -----------------------------
// In non-PTY mode each binary frame is prefixed with a 1-byte stream id:
//   0=stdin (client→server), 1=stdout, 2=stderr, 3=exit, 4=stdin-EOF.
// (PTY mode is raw, no prefix.) The bridge runs non-PTY so stdout stays a clean
// byte stream separate from the agent's stderr logs.

/// Prefix client stdin with the stdin stream id (0) for non-PTY mode.
fn encode_stream_stdin(data: &[u8]) -> Vec<u8> {
    let mut v = Vec::with_capacity(data.len() + 1);
    v.push(0u8);
    v.extend_from_slice(data);
    v
}

/// A decoded non-PTY server→client binary frame.
#[derive(Debug, PartialEq, Eq)]
enum StreamMsg {
    Stdout(Vec<u8>),
    Stderr(Vec<u8>),
    Exit(i32),
    /// Empty/unknown stream id — ignored.
    Ignore,
}

/// Split a non-PTY server→client binary frame on its 1-byte stream id.
fn decode_stream_frame(b: &[u8]) -> StreamMsg {
    let Some((&id, rest)) = b.split_first() else {
        return StreamMsg::Ignore;
    };
    match id {
        1 => StreamMsg::Stdout(rest.to_vec()),
        2 => StreamMsg::Stderr(rest.to_vec()),
        3 => StreamMsg::Exit(
            String::from_utf8_lossy(rest)
                .trim()
                .parse()
                .unwrap_or(EXEC_EXIT_STATUS_UNKNOWN),
        ),
        _ => StreamMsg::Ignore,
    }
}

/// A parsed Sprites exec text (control) frame.
#[derive(Debug, PartialEq, Eq)]
enum SpriteCtrl {
    /// The session-info frame announced its `session_id` (for reattach).
    Session(String),
    /// The command exited with this code.
    Exit(i32),
    /// A frame we don't act on (port notifications, unknown shapes).
    Ignore,
}

/// Classify a Sprites exec text frame: `{"type":"exit","exit_code":N}` ⇒ exit,
/// a frame carrying `session_id` ⇒ the session announcement, else ignore.
fn parse_sprite_ctrl(text: &str) -> SpriteCtrl {
    let Ok(v) = serde_json::from_str::<serde_json::Value>(text) else {
        return SpriteCtrl::Ignore;
    };
    if v.get("type").and_then(|t| t.as_str()) == Some("exit") {
        let code = v
            .get("exit_code")
            .and_then(|c| c.as_i64())
            .and_then(|code| i32::try_from(code).ok())
            .unwrap_or(EXEC_EXIT_STATUS_UNKNOWN);
        return SpriteCtrl::Exit(code);
    }
    if let Some(sid) = v.get("session_id").and_then(|s| s.as_str()) {
        return SpriteCtrl::Session(sid.to_string());
    }
    SpriteCtrl::Ignore
}

impl SpritesProvider {
    /// Open a native PTY exec session over the Sprites WSS exec API — no `sprite`
    /// CLI. The returned [`ExecSession`]'s driver task lives on the ambient tokio
    /// runtime until the socket closes or `ExecControl::Close` is sent.
    pub async fn open_exec(&self, id: &str, spec: &ExecSpec) -> Result<ExecSession> {
        let url = exec_ws_url(self.base()?, id, spec)?;
        self.start_session(url, spec.tty).await
    }

    /// Reattach to a persisted exec session (the server replays its scrollback).
    /// Reattach is always interactive (PTY), so the stream is raw.
    pub async fn attach_exec(
        &self,
        id: &str,
        session: &str,
        cols: u16,
        rows: u16,
    ) -> Result<ExecSession> {
        let url = attach_ws_url(self.base()?, id, session, cols, rows)?;
        self.connect_exec(url, true, false).await
    }

    /// Run a one-shot command in the sprite over the WSS exec API (NON-tty) and
    /// collect its combined output + exit code. This is the CLI-free / bridge-free
    /// channel the env provisioner uses to set the sandbox up (the non-tty framing
    /// surfaces stdout + the exit; redirect stderr with `2>&1` in the script to
    /// capture it). Returns `(exit_code, output)`.
    pub async fn run_exec(
        &self,
        id: &str,
        argv: &[String],
        cwd: Option<&str>,
        env: &[(String, String)],
    ) -> Result<(i32, String)> {
        let spec = ExecSpec {
            argv: argv.to_vec(),
            tty: false,
            cols: 0,
            rows: 0,
            env: env.to_vec(),
            cwd: cwd.map(str::to_string),
        };
        let mut sess = self.open_exec(id, &spec).await?;
        let mut out: Vec<u8> = Vec::new();
        let mut exit_code = None;
        while let Some(frame) = sess.frames.recv().await {
            match frame {
                ExecFrame::Stdout(b) => out.extend_from_slice(&b),
                ExecFrame::Exit(c) => {
                    exit_code = Some(c);
                    break;
                }
            }
        }
        if let Err(error) = sess.control.send(ExecControl::Close).await {
            tracing::debug!(target: "thegn::provider", %error, "best-effort exec close failed");
        }
        let Some(code) = exit_code else {
            return Err(anyhow!("exec stream ended without an exit status"));
        };
        Ok((code, String::from_utf8_lossy(&out).into_owned()))
    }

    /// Run the WSS handshake (bearer auth) and spawn the bridge task. `tty`
    /// selects the wire framing: raw (PTY) vs 1-byte stream-id prefixes (non-PTY).
    async fn start_session(&self, url: String, tty: bool) -> Result<ExecSession> {
        self.connect_exec(url, tty, true).await
    }

    // Fresh exec can wait for cold boot. Reattach is already retried by the
    // session relay and must not multiply its budget by an inner 90s loop.
    async fn connect_exec(&self, url: String, tty: bool, cold_open: bool) -> Result<ExecSession> {
        use tokio_tungstenite::tungstenite::client::IntoClientRequest;
        let auth: tokio_tungstenite::tungstenite::http::HeaderValue =
            format!("Bearer {}", self.token)
                .parse()
                .context("sprites: exec auth header")?;
        // A freshly-created sprite's exec endpoint isn't up during its first cold
        // boot, so a single `connect_async` hangs on a dead endpoint (~OS timeout)
        // and then fatally aborts the provision's `workspace` step. Retry with a
        // short per-attempt timeout + backoff so the connect lands the moment the
        // sprite warms, within a bounded budget (a genuinely unreachable sprite
        // still errors, just after the budget rather than on a lone long hang).
        const ATTEMPT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(8);
        const CONNECT_BUDGET: std::time::Duration = std::time::Duration::from_secs(90);
        const BACKOFF: std::time::Duration = std::time::Duration::from_secs(3);
        let start = std::time::Instant::now();
        let ws = loop {
            let mut req = url
                .clone()
                .into_client_request()
                .context("sprites: build exec ws request")?;
            req.headers_mut().insert("Authorization", auth.clone());
            let Some(attempt_timeout) =
                next_connect_wait(start.elapsed(), CONNECT_BUDGET, ATTEMPT_TIMEOUT)
            else {
                return Err(anyhow!(
                    "sprites: exec ws connect timed out after {}s",
                    start.elapsed().as_secs()
                ));
            };
            match tokio::time::timeout(
                attempt_timeout,
                tokio_tungstenite::connect_async_with_config(req, Some(exec_ws_config()), false),
            )
            .await
            {
                Ok(Ok((ws, _resp))) => break ws,
                res => {
                    let permanent = matches!(&res, Ok(Err(tokio_tungstenite::tungstenite::Error::Http(response)))
                        if response.status().is_client_error()
                            && !matches!(response.status().as_u16(), 408 | 429));
                    if !cold_open || permanent {
                        return match res {
                            Ok(Err(e)) => Err(e).context("sprites: exec ws connect"),
                            _ => Err(anyhow!(
                                "sprites: exec ws connect timed out after {}s",
                                start.elapsed().as_secs()
                            )),
                        };
                    }
                    tracing::debug!(
                        target: "thegn::sandbox",
                        elapsed_s = start.elapsed().as_secs(),
                        "sprite exec endpoint not ready (cold boot?); retrying connect"
                    );
                    let Some(backoff) = next_connect_wait(start.elapsed(), CONNECT_BUDGET, BACKOFF)
                    else {
                        return match res {
                            Ok(Err(e)) => Err(e).context("sprites: exec ws connect"),
                            _ => Err(anyhow!(
                                "sprites: exec ws connect timed out after {}s",
                                start.elapsed().as_secs()
                            )),
                        };
                    };
                    tokio::time::sleep(backoff).await;
                }
            }
        };

        let (frames_tx, frames_rx) = tokio::sync::mpsc::channel::<ExecFrame>(256);
        let (control_tx, control_rx) = tokio::sync::mpsc::channel::<ExecControl>(256);
        let (sid_tx, sid_rx) = tokio::sync::watch::channel::<Option<String>>(None);

        tokio::spawn(drive_exec(ws, tty, frames_tx, control_rx, sid_tx));

        Ok(ExecSession {
            frames: frames_rx,
            control: control_tx,
            session_id: sid_rx,
        })
    }

    /// Open a raw TCP relay to an in-sandbox `host:port` over the Sprites `/proxy`
    /// WSS endpoint (bearer auth + a `{host,port}` init frame, then a transparent
    /// byte relay). Used to tunnel an `ssh` connection to an in-sandbox `sshd`.
    pub async fn open_proxy(&self, id: &str, host: &str, port: u16) -> Result<ProxyStream> {
        use tokio_tungstenite::tungstenite::client::IntoClientRequest;
        let url = proxy_ws_url(self.base()?, id)?;
        let mut req = url
            .into_client_request()
            .context("sprites: build proxy ws request")?;
        let auth = format!("Bearer {}", self.token)
            .parse()
            .context("sprites: proxy auth header")?;
        req.headers_mut().insert("Authorization", auth);
        let ws = connect_proxy_ws(req, proxy_init_json(host, port), PROXY_CONNECT_TIMEOUT).await?;

        let (to_srv_tx, to_srv_rx) = tokio::sync::mpsc::channel::<Vec<u8>>(256);
        let (from_srv_tx, from_srv_rx) = tokio::sync::mpsc::channel::<Vec<u8>>(256);
        tokio::spawn(drive_proxy(ws, to_srv_rx, from_srv_tx));
        Ok(ProxyStream {
            rx: from_srv_rx,
            tx: to_srv_tx,
        })
    }
}

async fn connect_proxy_ws(
    req: tokio_tungstenite::tungstenite::http::Request<()>,
    init: String,
    deadline: std::time::Duration,
) -> Result<
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>,
> {
    use futures_util::SinkExt;
    use tokio_tungstenite::tungstenite::Message;

    let result = tokio::time::timeout(deadline, async move {
        let (mut ws, _resp) =
            tokio_tungstenite::connect_async_with_config(req, Some(exec_ws_config()), false)
                .await
                .context("sprites: proxy ws connect")?;
        // The init frame selects the target; after it, the socket is a raw relay.
        ws.send(Message::Text(init.into()))
            .await
            .context("sprites: proxy init")?;
        Ok::<_, anyhow::Error>(ws)
    })
    .await;
    match result {
        Ok(result) => result,
        Err(_) => Err(anyhow::Error::new(ProxyConnectTimeout { deadline })),
    }
}

/// The proxy relay task: pump raw bytes WebSocket ⇄ the [`ProxyStream`] channels.
/// No framing — binary frames are forwarded verbatim in both directions.
async fn drive_proxy<S>(
    ws: S,
    mut to_srv: tokio::sync::mpsc::Receiver<Vec<u8>>,
    from_srv: tokio::sync::mpsc::Sender<Vec<u8>>,
) where
    S: futures_util::Sink<tokio_tungstenite::tungstenite::Message>
        + futures_util::Stream<
            Item = std::result::Result<
                tokio_tungstenite::tungstenite::Message,
                tokio_tungstenite::tungstenite::Error,
            >,
        > + Unpin,
{
    use futures_util::{SinkExt, StreamExt};
    use tokio_tungstenite::tungstenite::Message;
    let (mut write, mut read) = ws.split();
    loop {
        tokio::select! {
            out = to_srv.recv() => match out {
                Some(b) => {
                    if write.send(Message::Binary(b.into())).await.is_err() {
                        break;
                    }
                }
                None => break, // client closed
            },
            msg = read.next() => match msg {
                Some(Ok(Message::Binary(b))) => {
                    if from_srv.send(b.into()).await.is_err() {
                        break;
                    }
                }
                Some(Ok(Message::Close(_))) | None => break,
                Some(Ok(_)) => {} // ignore text/ping/pong
                Some(Err(_)) => break,
            },
        }
    }
    let _ = write.send(Message::Close(None)).await; // best-effort: close handshake; peer may be gone
}

/// The exec bridge task: pump WebSocket frames ⇄ the [`ExecSession`] channels.
/// Generic over the socket so the concrete (TLS) stream type need not be named.
/// `tty` selects the wire framing: raw bytes (PTY) vs 1-byte stream-id prefixes
/// (non-PTY — stdin gets a `0` prefix; only stdout/exit are surfaced, stderr is
/// dropped so a non-PTY consumer like the bridge sees a clean byte stream).
async fn drive_exec<S>(
    ws: S,
    tty: bool,
    frames_tx: tokio::sync::mpsc::Sender<ExecFrame>,
    mut control_rx: tokio::sync::mpsc::Receiver<ExecControl>,
    sid_tx: tokio::sync::watch::Sender<Option<String>>,
) where
    S: futures_util::Sink<tokio_tungstenite::tungstenite::Message>
        + futures_util::Stream<
            Item = std::result::Result<
                tokio_tungstenite::tungstenite::Message,
                tokio_tungstenite::tungstenite::Error,
            >,
        > + Unpin,
{
    use futures_util::{SinkExt, StreamExt};
    use tokio_tungstenite::tungstenite::Message;
    let (mut write, mut read) = ws.split();
    loop {
        tokio::select! {
            ctrl = control_rx.recv() => match ctrl {
                Some(ExecControl::Stdin(b)) => {
                    let frame = if tty { b } else { encode_stream_stdin(&b) };
                    if write.send(Message::Binary(frame.into())).await.is_err() {
                        break;
                    }
                }
                Some(ExecControl::Resize { cols, rows }) => {
                    let _ = write.send(Message::Text(resize_json(cols, rows).into())).await; // best-effort: consumer may be gone
                }
                Some(ExecControl::Close) | None => {
                    let _ = write.send(Message::Close(None)).await; // best-effort: peer may be gone
                    break;
                }
            },
            msg = read.next() => match msg {
                Some(Ok(Message::Binary(b))) => {
                    if tty {
                        if frames_tx.send(ExecFrame::Stdout(b.into())).await.is_err() {
                            break;
                        }
                    } else {
                        match decode_stream_frame(&b) {
                            StreamMsg::Stdout(d) => {
                                if frames_tx.send(ExecFrame::Stdout(d)).await.is_err() {
                                    break;
                                }
                            }
                            StreamMsg::Exit(code) => {
                                let _ = frames_tx.send(ExecFrame::Exit(code)).await; // best-effort: caller may have dropped the stream
                                break;
                            }
                            // stderr (agent logs) and unknown ids: not part of the
                            // consumer's byte stream.
                            StreamMsg::Stderr(_) | StreamMsg::Ignore => {}
                        }
                    }
                }
                Some(Ok(Message::Text(t))) => match parse_sprite_ctrl(t.as_str()) {
                    SpriteCtrl::Session(s) => {
                        let _ = sid_tx.send(Some(s)); // best-effort: waiter may have timed out
                    }
                    SpriteCtrl::Exit(code) => {
                        let _ = frames_tx.send(ExecFrame::Exit(code)).await; // best-effort: caller may have dropped the stream
                        break;
                    }
                    SpriteCtrl::Ignore => {}
                },
                Some(Ok(Message::Ping(p))) => {
                    let _ = write.send(Message::Pong(p)).await; // best-effort: peer may be gone
                }
                Some(Ok(Message::Close(_))) | None => {
                    break;
                }
                Some(Ok(_)) => {}
                Some(Err(_)) => {
                    break;
                }
            },
        }
    }
}

/// The generic managed-sandbox provider dispatcher. Lifecycle methods delegate to
/// the variant's [`RemoteProvider`]; axis methods delegate to the variant's
/// sub-trait impl or return a clear "unsupported" error (gate on `caps`).
pub enum Provider {
    Daytona(DaytonaProvider),
    Sprites(SpritesProvider),
    /// Commodity VPS vendors (Hetzner today) — lifecycle over REST, exec/files
    /// over plain ssh. No checkpoints by construction: a powered-off VPS still
    /// bills, so the only free state is destroyed (see `crate::vps`).
    Vps(crate::vps::VpsProvider),
    /// Fly.io machines (CLI-free) — Machines REST lifecycle, reachability over a
    /// WireGuard tunnel into Fly's 6PN mesh (`fly ssh console` without flyctl).
    /// Container-native, so it's not a `VpsKind`; scale-to-zero via stop/start
    /// (see `crate::fly`). Exec/attach is the reachability spike boundary.
    Fly(crate::fly::FlyProvider),
    /// machine0 (`machine0.io`) VMs — lifecycle/images/keys over machine0's
    /// remote **MCP** endpoint (no `machine0` CLI), exec/files over plain ssh
    /// with thegn's managed key (`ExecKind::Ssh`, VPS-shaped). Native image
    /// snapshots (checkpoints) + `vm_suspend` scale-to-zero; NixOS via a
    /// post-create `nixos-rebuild --flake` over ssh (see `crate::machine0`).
    Machine0(crate::machine0::Machine0Provider),
    /// An **exec-only** transport to a sandbox reached over the iroh call-home
    /// reach (`crate::iroh_reach`): the sandbox dialed home and its interactive
    /// panes ride the iroh connection. The sandbox is created/destroyed through
    /// its real provider (e.g. `Fly`) elsewhere; this variant is only ever
    /// constructed for the pane exec path, so `open_exec` routes to the home
    /// endpoint by sandbox id and the lifecycle/fs methods are not applicable.
    Iroh {
        home: std::sync::Arc<crate::iroh_reach::IrohHome>,
        sandbox: String,
    },
}

/// The error every non-exec `Provider` method returns for the exec-only iroh
/// reach (lifecycle + fs go through the sandbox's real provider, e.g. Fly).
fn iroh_exec_only(op: &str) -> anyhow::Error {
    anyhow!("iroh reach is exec-only; {op} goes through the sandbox's underlying provider")
}

impl Provider {
    /// What this provider supports beyond lifecycle.
    pub fn caps(&self) -> ProviderCaps {
        // Single source of truth for the scale-to-zero classification (core), so
        // the runtime caps and the warm-pool policy can never disagree.
        let scale_to_zero = thegn_core::config::provider_scale_to_zero(self.name());
        match self {
            Provider::Daytona(_) => ProviderCaps {
                scale_to_zero,
                ..ProviderCaps::default()
            },
            // Flags flip on as each Sprites axis lands.
            Provider::Sprites(_) => ProviderCaps {
                egress: true,
                checkpoints: true,
                files: true,
                exec_api: true,
                scale_to_zero,
            },
            // files via the ssh shim; checkpoints deliberately absent (no
            // suspend on a VPS — the recycle path must destroy, not restore).
            Provider::Vps(_) => ProviderCaps {
                files: true,
                scale_to_zero,
                ..ProviderCaps::default()
            },
            // files over the WireGuard/hallpass transport; scale-to-zero via
            // stop/start; no checkpoints, no native WSS exec/egress API.
            Provider::Fly(_) => ProviderCaps {
                files: true,
                scale_to_zero,
                ..ProviderCaps::default()
            },
            // files over ssh; checkpoints via machine0 images; scale-to-zero via
            // vm_suspend. No egress translation / no native WSS exec (pane rides
            // ssh, same as VPS).
            Provider::Machine0(_) => ProviderCaps {
                files: true,
                checkpoints: true,
                scale_to_zero,
                ..ProviderCaps::default()
            },
            // Exec-only: no files/checkpoints/egress of its own (those go through
            // the underlying provider that created the sandbox).
            Provider::Iroh { .. } => ProviderCaps {
                exec_api: true,
                scale_to_zero,
                ..ProviderCaps::default()
            },
        }
    }

    pub async fn create(&self) -> Result<SandboxHandle> {
        self.remote()
            .ok_or_else(|| iroh_exec_only("create"))?
            .create()
            .await
    }

    pub async fn destroy(&self, id: &str) -> Result<()> {
        self.remote()
            .ok_or_else(|| iroh_exec_only("destroy"))?
            .destroy(id)
            .await
    }

    pub async fn list(&self) -> Result<Vec<String>> {
        match self.remote() {
            Some(p) => p.list().await,
            // Exec-only: it manages no sandboxes of its own, so it lists none.
            None => Ok(Vec::new()),
        }
    }

    /// The provider's short name (error messages / capability gating).
    fn name(&self) -> &'static str {
        match self {
            Provider::Daytona(_) => "daytona",
            Provider::Sprites(_) => "sprites",
            Provider::Vps(p) => p.spec().kind.as_str(),
            Provider::Fly(_) => "fly",
            Provider::Machine0(_) => "machine0",
            Provider::Iroh { .. } => "iroh",
        }
    }

    // --- accessor-based routing: each axis method resolves the variant's
    // trait impl once, here, instead of re-matching in every method. `Some`
    // exactly where an impl exists (mirroring `caps()`); a new variant only
    // adds arms here, not one per method. -----------------------------------

    /// The variant's lifecycle impl — `None` only for the exec-only iroh reach.
    fn remote(&self) -> Option<&dyn RemoteProvider> {
        match self {
            Provider::Daytona(p) => Some(p),
            Provider::Sprites(p) => Some(p),
            Provider::Vps(p) => Some(p),
            Provider::Fly(p) => Some(p),
            Provider::Machine0(p) => Some(p),
            Provider::Iroh { .. } => None,
        }
    }

    /// The variant's file API, where one exists (mirrors `caps().files`).
    fn files_ops(&self) -> Option<&dyn ProviderFiles> {
        match self {
            Provider::Sprites(p) => Some(p),
            Provider::Vps(p) => Some(p),
            Provider::Fly(p) => Some(p),
            Provider::Machine0(p) => Some(p),
            Provider::Daytona(_) | Provider::Iroh { .. } => None,
        }
    }

    /// The variant's egress-translation API (mirrors `caps().egress`).
    fn egress_ops(&self) -> Option<&dyn ProviderEgress> {
        match self {
            Provider::Sprites(p) => Some(p),
            _ => None,
        }
    }

    /// The variant's checkpoint API (mirrors `caps().checkpoints`).
    fn checkpoint_ops(&self) -> Option<&dyn ProviderCheckpoints> {
        match self {
            Provider::Sprites(p) => Some(p),
            Provider::Machine0(p) => Some(p),
            _ => None,
        }
    }

    /// The uniform "no checkpoints" error (same text the old per-method arms
    /// returned).
    fn no_checkpoints(&self) -> anyhow::Error {
        anyhow!("provider '{}' does not support checkpoints", self.name())
    }

    /// The per-variant "no file API" error: the exec-only iroh reach keeps its
    /// exec-only wording; everyone else names the provider.
    fn no_files(&self) -> anyhow::Error {
        match self {
            Provider::Iroh { .. } => iroh_exec_only("file sync"),
            _ => anyhow!("provider '{}' does not support file sync", self.name()),
        }
    }

    /// Idempotently ensure a sandbox named `name` exists — the warm-on-open
    /// lifecycle. Returns `true` if it created one, `false` if it already existed.
    /// (List-then-create; a provider with a truly idempotent create can override.)
    pub async fn ensure_exists(&self, name: &str) -> Result<bool> {
        if let Some(owner) = self.binding_owner() {
            let name = name.to_owned();
            let owner = owner.to_owned();
            tokio::task::spawn_blocking(move || crate::vps::registry::check_owner(&name, &owner))
                .await??;
        }
        if self.list().await?.iter().any(|n| n == name) {
            return Ok(false);
        }
        self.create().await?;
        Ok(true)
    }

    fn binding_owner(&self) -> Option<&str> {
        match self {
            Self::Vps(provider) => provider.owner(),
            Self::Fly(provider) => provider.owner(),
            _ => None,
        }
    }

    /// Park a scale-to-zero sandbox to stop paying for idle compute (the
    /// warm/idle policy calls this for providers that DON'T self-suspend —
    /// `machine0` `vm_suspend`, `fly` stop). No-op for self-suspending providers
    /// (sprites) or those without the notion. Idempotent.
    pub async fn suspend(&self, id: &str) -> Result<()> {
        match self {
            Provider::Machine0(p) => p.suspend(id).await,
            Provider::Fly(p) => p.stop(id).await,
            _ => Ok(()),
        }
    }

    /// Resume a parked sandbox (the claim/reopen path — `machine0` `vm_start`,
    /// `fly` start), waiting until it is reachable. No-op for providers without
    /// the notion. Idempotent (a running sandbox stays running).
    pub async fn resume(&self, id: &str) -> Result<()> {
        match self {
            Provider::Machine0(p) => p.resume(id).await,
            Provider::Fly(p) => p.start(id).await,
            _ => Ok(()),
        }
    }

    /// Wait (bounded by `budget`) until a freshly-created sandbox is booted enough
    /// to accept fs/exec calls. No-op for providers without a readiness notion (or
    /// without the fs API used to probe). Call this once after a create returns
    /// `true` from [`Self::ensure_exists`], before the first `read`/exec — it prevents
    /// the post-create boot race that stranded sprite provisioning on startup.
    pub async fn wait_ready(&self, id: &str, budget: std::time::Duration) -> Result<()> {
        match self {
            Provider::Sprites(p) => p.wait_ready(id, budget).await,
            _ => Ok(()),
        }
    }

    /// Lower allow/block lists to the provider's network policy (egress translate).
    pub async fn set_network_policy(
        &self,
        id: &str,
        allow: &[String],
        block: &[String],
    ) -> Result<()> {
        self.egress_ops()
            .ok_or_else(|| {
                anyhow!(
                    "provider '{}' does not support egress translation",
                    self.name()
                )
            })?
            .set_network_policy(id, allow, block)
            .await
    }

    /// Read back the provider-native network policy for a sandbox.
    pub async fn get_network_policy(&self, id: &str) -> Result<Vec<PolicyRule>> {
        self.egress_ops()
            .ok_or_else(|| {
                anyhow!(
                    "provider '{}' does not support egress translation",
                    self.name()
                )
            })?
            .get_network_policy(id)
            .await
    }

    /// Create a checkpoint of the sandbox, returning its id.
    pub async fn checkpoint(&self, id: &str, label: Option<&str>) -> Result<String> {
        self.checkpoint_ops()
            .ok_or_else(|| self.no_checkpoints())?
            .checkpoint(id, label)
            .await
    }

    pub async fn list_checkpoints(&self, id: &str) -> Result<Vec<CheckpointInfo>> {
        self.checkpoint_ops()
            .ok_or_else(|| self.no_checkpoints())?
            .list_checkpoints(id)
            .await
    }

    pub async fn restore(&self, id: &str, checkpoint: &str) -> Result<()> {
        self.checkpoint_ops()
            .ok_or_else(|| self.no_checkpoints())?
            .restore(id, checkpoint)
            .await
    }

    /// Read a file from the sandbox fs.
    pub async fn read(&self, id: &str, path: &str) -> Result<Vec<u8>> {
        self.files_ops()
            .ok_or_else(|| self.no_files())?
            .read(id, path)
            .await
    }

    /// Write a file into the sandbox fs.
    pub async fn write(&self, id: &str, path: &str, data: &[u8]) -> Result<()> {
        self.files_ops()
            .ok_or_else(|| self.no_files())?
            .write(id, path, data)
            .await
    }

    /// Write an executable into the sandbox fs (mode 0755 where supported).
    pub async fn write_exec(&self, id: &str, path: &str, data: &[u8]) -> Result<()> {
        self.files_ops()
            .ok_or_else(|| self.no_files())?
            .write_exec(id, path, data)
            .await
    }

    /// Idempotently install the executable `data` at `path` in the sandbox.
    /// SHA-256 is checked against the remote bytes themselves; uploads go to a
    /// staging sibling, are read back and verified, then published with `mv -f`.
    /// The legacy `<path>.fp` sidecar is ignored and best-effort removed. Returns
    /// whether it pushed (`true`) or the remote executable was already current
    /// (`false`). Requires the provider's file API (`caps().files`).
    pub async fn ensure_executable(&self, id: &str, path: &str, data: &[u8]) -> Result<bool> {
        if !self.caps().files {
            return Err(anyhow!("provider does not support a file push"));
        }
        let files = self.files_ops().ok_or_else(|| self.no_files())?;
        install_executable_verified(files, id, path, data, |stage, dest| async move {
            let (code, output) = self
                .run_exec(
                    id,
                    &["mv".into(), "-f".into(), "--".into(), stage, dest],
                    None,
                    &[],
                )
                .await?;
            if code != 0 {
                let mut tail_start = output.len().saturating_sub(200);
                while !output.is_char_boundary(tail_start) {
                    tail_start += 1;
                }
                return Err(anyhow!("mv exited {code}: {}", &output[tail_start..]));
            }
            Ok(())
        })
        .await
    }

    /// Push a local directory into the sandbox fs (provider `sync` projection).
    pub async fn upload_dir(&self, id: &str, local: &Path, remote: &str) -> Result<()> {
        self.files_ops()
            .ok_or_else(|| self.no_files())?
            .upload_dir(id, local, remote)
            .await
    }

    /// Pull the sandbox fs back into a local directory.
    pub async fn download_dir(&self, id: &str, remote: &str, local: &Path) -> Result<()> {
        self.files_ops()
            .ok_or_else(|| self.no_files())?
            .download_dir(id, remote, local)
            .await
    }

    /// Open a native PTY exec session (the `exec_api` capability), so an
    /// interactive pane attaches over the provider API with no vendor CLI. Gate
    /// on `caps().exec_api`.
    pub async fn open_exec(&self, id: &str, spec: &ExecSpec) -> Result<ExecSession> {
        match self {
            Provider::Sprites(p) => p.open_exec(id, spec).await,
            // The iroh reach carries interactive panes: route to the home endpoint.
            Provider::Iroh { home, .. } => home.open_exec(id, spec.clone()).await,
            _ => Err(anyhow!("provider '{}' has no native exec API", self.name())),
        }
    }

    /// Open a raw TCP relay to an in-sandbox `host:port` over the provider's
    /// TCP-over-WebSocket proxy. Used to tunnel `ssh` to an in-sandbox `sshd`
    /// (`[env.<name>.provider] connect = "ssh"`).
    pub async fn open_proxy(&self, id: &str, host: &str, port: u16) -> Result<ProxyStream> {
        match self {
            Provider::Sprites(p) => p.open_proxy(id, host, port).await,
            _ => Err(anyhow!("provider '{}' has no TCP proxy API", self.name())),
        }
    }

    /// Run a one-shot command (non-tty) and collect `(exit_code, output)`. Used by
    /// the env provisioner to set up the sandbox without a CLI/bridge.
    pub async fn run_exec(
        &self,
        id: &str,
        argv: &[String],
        cwd: Option<&str>,
        env: &[(String, String)],
    ) -> Result<(i32, String)> {
        match self {
            Provider::Sprites(p) => p.run_exec(id, argv, cwd, env).await,
            Provider::Vps(p) => p.run_exec(id, argv, cwd, env).await,
            Provider::Fly(p) => p.run_exec(id, argv, cwd, env).await,
            Provider::Machine0(p) => p.run_exec(id, argv, cwd, env).await,
            Provider::Daytona(_) => Err(anyhow!("provider 'daytona' has no native exec API")),
            // One-shot capture over iroh: open a non-tty exec and drain it.
            Provider::Iroh { home, .. } => {
                let spec = ExecSpec {
                    argv: argv.to_vec(),
                    tty: false,
                    cols: 80,
                    rows: 24,
                    env: env.to_vec(),
                    cwd: cwd.map(str::to_string),
                };
                let mut session = home.open_exec(id, spec).await?;
                let mut out = Vec::new();
                let mut exit_code = None;
                while let Some(frame) = session.frames.recv().await {
                    match frame {
                        ExecFrame::Stdout(b) => out.extend(b),
                        ExecFrame::Exit(c) => {
                            exit_code = Some(c);
                            break;
                        }
                    }
                }
                let Some(code) = exit_code else {
                    return Err(anyhow!("exec stream ended without an exit status"));
                };
                Ok((code, String::from_utf8_lossy(&out).into_owned()))
            }
        }
    }

    /// Reattach to a persisted exec session (replays scrollback). `exec_api` only.
    pub async fn attach_exec(
        &self,
        id: &str,
        session: &str,
        cols: u16,
        rows: u16,
    ) -> Result<ExecSession> {
        match self {
            Provider::Sprites(p) => p.attach_exec(id, session, cols, rows).await,
            _ => Err(anyhow!("provider '{}' has no native exec API", self.name())),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn provider() -> DaytonaProvider {
        DaytonaProvider::new("https://app.daytona.io/api/", "tok", "debian:stable")
    }

    fn sprites() -> SpritesProvider {
        SpritesProvider::new("https://api.sprites.dev/v1/", "tok", "my-sprite")
    }

    #[test]
    fn sprites_default_base_and_urls() {
        let p = sprites();
        assert_eq!(
            p.sprites_url().unwrap(),
            "https://api.sprites.dev/v1/sprites"
        );
        assert_eq!(
            p.sprite_name_url("my-sprite").unwrap(),
            "https://api.sprites.dev/v1/sprites/my-sprite"
        );
        // Empty api_base falls back to the documented default.
        let d = SpritesProvider::new("", "t", "s");
        assert_eq!(
            d.sprites_url().unwrap(),
            "https://api.sprites.dev/v1/sprites"
        );
    }

    #[test]
    fn configured_api_base_requires_safe_authority() {
        for rejected in [
            "http://user:secret@example.com/api",
            "https://user:pw@provider.example/api",
            "https://user@provider.example/api",
            "http://provider.example/api",
            "https://provider.example/api?token=x",
            "https://provider.example/api#fragment",
            "ftp://provider.example/api",
            "",
        ] {
            assert!(
                validate_api_base(rejected).is_err(),
                "accepted {rejected:?}"
            );
        }
        let credential_error = validate_api_base("http://user:s3cr3t-pw@127.0.0.1:1234")
            .unwrap_err()
            .to_string();
        assert!(credential_error.contains("embedded credentials"));
        assert!(!credential_error.contains("s3cr3t-pw"));
        assert!(!credential_error.contains("user:"));
        assert_eq!(
            validate_api_base("https://app.daytona.io/api/").unwrap(),
            "https://app.daytona.io/api"
        );
        assert_eq!(
            validate_api_base("http://127.0.0.1:1234").unwrap(),
            "http://127.0.0.1:1234"
        );
        assert_eq!(
            validate_api_base("http://[::1]:1234/").unwrap(),
            "http://[::1]:1234"
        );
    }

    #[tokio::test]
    async fn invalid_daytona_base_fails_before_connecting() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let provider = DaytonaProvider::new(&format!("http://u:p@{address}"), "token", "snapshot");
        assert!(provider.list().await.is_err());
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(100), listener.accept())
                .await
                .is_err(),
            "invalid base unexpectedly opened a connection"
        );
    }

    #[test]
    fn control_gate_enforces_authority_and_global_caps() {
        let gate = ControlGate::new(2, 1);
        let first = gate.try_acquire("a").unwrap();
        assert!(gate.try_acquire("a").is_err());
        let second = gate.try_acquire("b").unwrap();
        assert!(gate.try_acquire("c").is_err());
        drop(first);
        let same_authority_again = gate.try_acquire("a").unwrap();
        drop((second, same_authority_again));
    }

    #[test]
    fn held_transfer_permit_does_not_block_control_gate() {
        let authority = "transfer-isolation-test.invalid:443";
        let _transfer = TRANSFER_GATE.try_acquire(authority).unwrap();

        assert!(CONTROL_GATE.try_acquire(authority).is_ok());
    }

    struct MemFiles {
        files: std::sync::Mutex<HashMap<String, Vec<u8>>>,
        corrupt_stage: bool,
    }

    impl MemFiles {
        fn new(files: HashMap<String, Vec<u8>>, corrupt_stage: bool) -> Self {
            Self {
                files: std::sync::Mutex::new(files),
                corrupt_stage,
            }
        }

        fn publish(&self, stage: &str, dest: &str) -> Result<()> {
            let mut files = self
                .files
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let bytes = files
                .remove(stage)
                .ok_or_else(|| anyhow!("missing staging file"))?;
            files.insert(dest.to_string(), bytes);
            Ok(())
        }

        fn contains_stage(&self) -> bool {
            self.files
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .keys()
                .any(|path| path.contains(".tg-stage-"))
        }
    }

    impl ProviderFiles for MemFiles {
        fn read<'a>(&'a self, _id: &'a str, path: &'a str) -> BoxFuture<'a, Result<Vec<u8>>> {
            Box::pin(async move {
                self.files
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .get(path)
                    .cloned()
                    .ok_or_else(|| anyhow!("missing file"))
            })
        }

        fn write<'a>(
            &'a self,
            _id: &'a str,
            path: &'a str,
            data: &'a [u8],
        ) -> BoxFuture<'a, Result<()>> {
            Box::pin(async move {
                let bytes = if self.corrupt_stage && path.contains(".tg-stage-") {
                    data[..data.len().saturating_sub(1)].to_vec()
                } else {
                    data.to_vec()
                };
                self.files
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .insert(path.to_string(), bytes);
                Ok(())
            })
        }

        fn list_dir<'a>(
            &'a self,
            _id: &'a str,
            _path: &'a str,
        ) -> BoxFuture<'a, Result<Vec<FileEntry>>> {
            Box::pin(async { Ok(Vec::new()) })
        }

        fn delete<'a>(&'a self, _id: &'a str, path: &'a str) -> BoxFuture<'a, Result<()>> {
            Box::pin(async move {
                self.files
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .remove(path);
                Ok(())
            })
        }
    }

    fn legacy_fnv_hex(data: &[u8]) -> String {
        let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
        for byte in data {
            hash ^= u64::from(*byte);
            hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
        }
        format!("{hash:016x}")
    }

    #[tokio::test]
    async fn exec_install_planted_fp_with_different_bytes_reuploads() {
        let path = "/w/.tg/thegn";
        let mut initial = HashMap::new();
        initial.insert(path.into(), b"evil".to_vec());
        initial.insert(format!("{path}.fp"), legacy_fnv_hex(b"good").into_bytes());
        let files = MemFiles::new(initial, false);
        let published = std::sync::atomic::AtomicBool::new(false);
        let mem = &files;
        let ran = &published;
        let result =
            install_executable_verified(&files, "id", path, b"good", |stage, dest| async move {
                ran.store(true, std::sync::atomic::Ordering::SeqCst);
                mem.publish(&stage, &dest)
            })
            .await
            .unwrap();
        let stored = files
            .files
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        assert!(result);
        assert!(published.load(std::sync::atomic::Ordering::SeqCst));
        assert_eq!(stored.get(path).map(Vec::as_slice), Some(&b"good"[..]));
        assert!(!stored.contains_key(&format!("{path}.fp")));
        drop(stored);
        assert!(!files.contains_stage());
    }

    #[tokio::test]
    async fn exec_install_matching_bytes_is_current() {
        let path = "/w/.tg/thegn";
        let files = MemFiles::new(HashMap::from([(path.into(), b"same".to_vec())]), false);
        let published = std::sync::atomic::AtomicBool::new(false);
        let ran = &published;
        let result = install_executable_verified(&files, "id", path, b"same", |_, _| async move {
            ran.store(true, std::sync::atomic::Ordering::SeqCst);
            Ok(())
        })
        .await
        .unwrap();
        assert!(!result);
        assert!(!published.load(std::sync::atomic::Ordering::SeqCst));
    }

    #[tokio::test]
    async fn exec_install_missing_remote_uploads() {
        let path = "/w/.tg/thegn";
        let files = MemFiles::new(HashMap::new(), false);
        let mem = &files;
        let result = install_executable_verified(
            &files,
            "id",
            path,
            b"new binary",
            |stage, dest| async move { mem.publish(&stage, &dest) },
        )
        .await
        .unwrap();
        assert!(result);
        assert_eq!(
            files
                .files
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .get(path)
                .map(Vec::as_slice),
            Some(&b"new binary"[..])
        );
    }

    #[tokio::test]
    async fn exec_install_truncated_stage_is_never_published() {
        let path = "/w/.tg/thegn";
        let files = MemFiles::new(HashMap::from([(path.into(), b"prior".to_vec())]), true);
        let published = std::sync::atomic::AtomicBool::new(false);
        let mem = &files;
        let ran = &published;
        let result = install_executable_verified(
            &files,
            "id",
            path,
            b"new binary",
            |stage, dest| async move {
                ran.store(true, std::sync::atomic::Ordering::SeqCst);
                mem.publish(&stage, &dest)
            },
        )
        .await;
        assert!(matches!(result, Err(error) if error.to_string().contains("failed verification")));
        assert!(!published.load(std::sync::atomic::Ordering::SeqCst));
        assert_eq!(
            files
                .files
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .get(path)
                .map(Vec::as_slice),
            Some(&b"prior"[..])
        );
        assert!(!files.contains_stage());
    }

    #[tokio::test]
    async fn exec_install_publish_failure_removes_stage() {
        let path = "/w/.tg/thegn";
        let files = MemFiles::new(HashMap::from([(path.into(), b"prior".to_vec())]), false);
        let published = std::sync::atomic::AtomicBool::new(false);
        let ran = &published;
        let result =
            install_executable_verified(&files, "id", path, b"new binary", |_, _| async move {
                ran.store(true, std::sync::atomic::Ordering::SeqCst);
                Err(anyhow!("publish refused"))
            })
            .await;
        assert!(
            matches!(result, Err(error) if error.to_string().contains("publish bridge executable"))
        );
        assert!(published.load(std::sync::atomic::Ordering::SeqCst));
        assert_eq!(
            files
                .files
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .get(path)
                .map(Vec::as_slice),
            Some(&b"prior"[..])
        );
        assert!(!files.contains_stage());
    }

    #[test]
    fn sprites_create_body_names_the_sprite() {
        assert_eq!(
            SpritesProvider::create_body("dev"),
            serde_json::json!({"name": "dev"})
        );
    }

    #[test]
    fn sprites_parse_name_flat_and_enveloped() {
        assert_eq!(
            SpritesProvider::parse_name(&serde_json::json!({"name": "a"})).as_deref(),
            Some("a")
        );
        assert_eq!(
            SpritesProvider::parse_name(&serde_json::json!({"sprite": {"name": "b"}})).as_deref(),
            Some("b")
        );
        assert_eq!(
            SpritesProvider::parse_name(&serde_json::json!({"id": 1})),
            None
        );
    }

    #[test]
    fn sprites_parse_list_array_and_envelope() {
        let arr = serde_json::json!([{"name": "a"}, {"name": "b"}, {"id": "no-name"}]);
        assert_eq!(SpritesProvider::parse_list(&arr), vec!["a", "b"]);
        let env = serde_json::json!({"sprites": [{"name": "c"}]});
        assert_eq!(SpritesProvider::parse_list(&env), vec!["c"]);
        assert!(SpritesProvider::parse_list(&serde_json::json!({})).is_empty());
    }

    #[test]
    fn sprites_exec_builds_cli_pty_bridge() {
        assert_eq!(
            SpritesProvider::exec_for("dev"),
            ExecKind::Command(vec![
                "sprite".into(),
                "exec".into(),
                "-s".into(),
                "dev".into(),
                "--tty".into(),
                "--".into(),
            ])
        );
    }

    #[test]
    fn sprites_has_native_exec_capability() {
        assert!(Provider::Sprites(sprites()).caps().exec_api);
        assert!(!Provider::Daytona(provider()).caps().exec_api);
    }

    #[test]
    fn only_sprites_is_scale_to_zero() {
        // Sprites hibernate for free (idle-park the pool); Daytona has no confirmed
        // free idle. Sourced from the core classifier, so caps + pool policy agree.
        assert!(Provider::Sprites(sprites()).caps().scale_to_zero);
        assert!(!Provider::Daytona(provider()).caps().scale_to_zero);
    }

    #[test]
    fn destroy_retries_transient_server_errors_only() {
        use reqwest::StatusCode;
        // Transient: teardown should retry rather than leak a paid sandbox.
        assert!(transient_status(StatusCode::INTERNAL_SERVER_ERROR)); // the observed 500
        assert!(transient_status(StatusCode::BAD_GATEWAY));
        assert!(transient_status(StatusCode::SERVICE_UNAVAILABLE));
        assert!(transient_status(StatusCode::GATEWAY_TIMEOUT));
        assert!(transient_status(StatusCode::TOO_MANY_REQUESTS));
        assert!(transient_status(StatusCode::REQUEST_TIMEOUT));
        // Terminal client errors: retrying won't help, stop early.
        assert!(!transient_status(StatusCode::UNAUTHORIZED));
        assert!(!transient_status(StatusCode::FORBIDDEN));
        assert!(!transient_status(StatusCode::BAD_REQUEST));
        assert!(!transient_status(StatusCode::CONFLICT));
        // 404 is handled as already-gone before this is consulted, but for safety
        // it is not itself "retryable".
        assert!(!transient_status(StatusCode::NOT_FOUND));
    }

    #[test]
    fn ws_scheme_rewrites_http_family_only() {
        assert_eq!(
            ws_scheme("https://api.sprites.dev/v1"),
            "wss://api.sprites.dev/v1"
        );
        assert_eq!(ws_scheme("http://localhost:8080"), "ws://localhost:8080");
        // Already a ws(s) base is left alone.
        assert_eq!(ws_scheme("wss://x/y"), "wss://x/y");
    }

    #[test]
    fn exec_ws_url_carries_cmd_tty_dims_and_env() {
        let spec = ExecSpec {
            argv: vec!["/bin/sh".into(), "-lc".into(), "exec bash -l".into()],
            tty: true,
            cols: 120,
            rows: 40,
            env: vec![("FOO".into(), "bar baz".into())],
            cwd: None,
        };
        let url = exec_ws_url("https://api.sprites.dev/v1", "dev", &spec).unwrap();
        assert!(url.starts_with("wss://api.sprites.dev/v1/sprites/dev/exec?"));
        // Each arg is a repeated, percent-encoded `cmd` pair (insertion order).
        assert!(url.contains("cmd=%2Fbin%2Fsh"));
        assert!(url.contains("cmd=-lc"));
        assert!(url.contains("cmd=exec+bash+-l"));
        assert!(url.contains("tty=true"));
        assert!(url.contains("stdin=true"));
        assert!(url.contains("cols=120"));
        assert!(url.contains("rows=40"));
        assert!(url.contains("env=FOO%3Dbar+baz"));
    }

    #[test]
    fn exec_ws_url_wraps_cwd_as_separate_positional_arguments() {
        let spec = ExecSpec {
            argv: vec!["git".into(), "status".into()],
            tty: false,
            cols: 0,
            rows: 0,
            env: Vec::new(),
            cwd: Some("/work/a b".into()),
        };
        let url =
            reqwest::Url::parse(&exec_ws_url("https://api.sprites.dev/v1", "dev", &spec).unwrap())
                .unwrap();
        let commands = url
            .query_pairs()
            .filter(|(key, _)| key == "cmd")
            .map(|(_, value)| value.into_owned())
            .collect::<Vec<_>>();
        assert_eq!(
            commands,
            vec![
                "/bin/sh",
                "-c",
                EXEC_CWD_NON_TTY_SCRIPT,
                "thegn-exec",
                "/work/a b",
                "git",
                "status"
            ]
        );

        let tty_spec = ExecSpec { tty: true, ..spec };
        let tty_url = reqwest::Url::parse(
            &exec_ws_url("https://api.sprites.dev/v1", "dev", &tty_spec).unwrap(),
        )
        .unwrap();
        let tty_commands = tty_url
            .query_pairs()
            .filter(|(key, _)| key == "cmd")
            .map(|(_, value)| value.into_owned())
            .collect::<Vec<_>>();
        assert_eq!(
            tty_commands,
            vec![
                "/bin/sh",
                "-c",
                EXEC_CWD_TTY_SCRIPT,
                "thegn-exec",
                "/work/a b",
                "git",
                "status"
            ]
        );
    }

    #[test]
    fn exec_cwd_validation_fails_closed_and_blank_cwd_is_unchanged() {
        let base = ExecSpec {
            argv: vec!["echo".into(), "ok".into()],
            tty: false,
            cols: 0,
            rows: 0,
            env: Vec::new(),
            cwd: Some("relative".into()),
        };
        assert!(exec_argv(&base).is_err());

        let nul_cwd = ExecSpec {
            cwd: Some("/a\0b".into()),
            ..base.clone()
        };
        assert!(exec_argv(&nul_cwd).is_err());

        let blank_cwd = ExecSpec {
            cwd: Some("  \t".into()),
            ..base.clone()
        };
        assert_eq!(exec_argv(&blank_cwd).unwrap(), base.argv);

        let empty_argv = ExecSpec {
            argv: Vec::new(),
            cwd: Some("/work".into()),
            ..base
        };
        assert!(exec_argv(&empty_argv).is_err());
    }

    #[test]
    fn exec_tty_with_relative_cwd_keeps_original_argv() {
        let spec = ExecSpec {
            argv: vec!["sh".into(), "-lc".into(), "exec app".into()],
            tty: true,
            cols: 80,
            rows: 24,
            env: Vec::new(),
            cwd: Some("~/code".into()),
        };

        assert_eq!(exec_argv(&spec).unwrap(), spec.argv);
    }

    #[test]
    fn exec_websocket_config_has_explicit_limits() {
        let config = exec_ws_config();
        assert_eq!(config.max_message_size, Some(16 << 20));
        assert_eq!(config.max_frame_size, Some(4 << 20));
        assert_eq!(config.max_write_buffer_size, 8 << 20);
    }

    #[test]
    fn connect_waits_never_exceed_the_remaining_budget() {
        use std::time::Duration;

        assert_eq!(
            next_connect_wait(
                Duration::ZERO,
                Duration::from_secs(90),
                Duration::from_secs(8)
            ),
            Some(Duration::from_secs(8))
        );
        assert_eq!(
            next_connect_wait(
                Duration::from_secs(88),
                Duration::from_secs(90),
                Duration::from_secs(3)
            ),
            Some(Duration::from_secs(2))
        );
        assert_eq!(
            next_connect_wait(
                Duration::from_secs(90),
                Duration::from_secs(90),
                Duration::from_secs(3)
            ),
            None
        );
        assert_eq!(
            next_connect_wait(
                Duration::from_secs(95),
                Duration::from_secs(90),
                Duration::from_secs(3)
            ),
            None
        );
    }

    #[tokio::test]
    async fn proxy_connect_deadline_bounds_a_silent_listener() {
        use std::time::Duration;
        use tokio_tungstenite::tungstenite::client::IntoClientRequest;

        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let accept_thread = std::thread::spawn(move || listener.accept().unwrap());
        let req = format!("ws://{address}/x").into_client_request().unwrap();
        let result = tokio::time::timeout(
            Duration::from_secs(2),
            connect_proxy_ws(req, "{}".into(), Duration::from_millis(200)),
        )
        .await;
        let accepted = accept_thread.join().unwrap().0;
        assert!(matches!(
            result,
            Ok(Err(error))
                if error.downcast_ref::<ProxyConnectTimeout>().is_some_and(|timeout|
                    timeout.deadline == Duration::from_millis(200))
        ));
        drop(accepted);
    }

    #[test]
    fn attach_ws_url_targets_session_with_dims() {
        let url = attach_ws_url("https://api.sprites.dev/v1", "dev", "sess-123", 80, 24).unwrap();
        assert!(url.starts_with("wss://api.sprites.dev/v1/sprites/dev/exec/sess-123?"));
        assert!(url.contains("cols=80"));
        assert!(url.contains("rows=24"));
    }

    #[test]
    fn resize_json_is_the_documented_frame() {
        assert_eq!(
            resize_json(120, 40),
            r#"{"type":"resize","cols":120,"rows":40}"#
        );
    }

    #[test]
    fn non_pty_stream_framing_roundtrips() {
        // stdin is prefixed with the stdin stream id (0).
        assert_eq!(encode_stream_stdin(b"hi"), vec![0, b'h', b'i']);
        // server frames split on the 1-byte stream id.
        assert_eq!(
            decode_stream_frame(&[1, b'o', b'k']),
            StreamMsg::Stdout(b"ok".to_vec())
        );
        assert_eq!(
            decode_stream_frame(&[2, b'e', b'r', b'r']),
            StreamMsg::Stderr(b"err".to_vec())
        );
        assert_eq!(
            decode_stream_frame(&[3, b'1', b'3', b'7']),
            StreamMsg::Exit(137)
        );
        assert_eq!(
            decode_stream_frame(&[3, b'x']),
            StreamMsg::Exit(EXEC_EXIT_STATUS_UNKNOWN)
        );
        assert_eq!(decode_stream_frame(&[]), StreamMsg::Ignore);
        assert_eq!(decode_stream_frame(&[9, 1, 2]), StreamMsg::Ignore);
    }

    #[test]
    fn parse_sprite_ctrl_classifies_frames() {
        assert_eq!(
            parse_sprite_ctrl(r#"{"type":"exit","exit_code":0}"#),
            SpriteCtrl::Exit(0)
        );
        assert_eq!(
            parse_sprite_ctrl(r#"{"type":"exit","exit_code":137}"#),
            SpriteCtrl::Exit(137)
        );
        assert_eq!(
            parse_sprite_ctrl(r#"{"type":"exit"}"#),
            SpriteCtrl::Exit(EXEC_EXIT_STATUS_UNKNOWN)
        );
        assert_eq!(
            parse_sprite_ctrl(r#"{"type":"exit","exit_code":2147483648}"#),
            SpriteCtrl::Exit(EXEC_EXIT_STATUS_UNKNOWN)
        );
        assert_eq!(
            parse_sprite_ctrl(r#"{"session_id":"abc","tty":true,"cols":80}"#),
            SpriteCtrl::Session("abc".into())
        );
        // Port notifications and garbage are ignored, not misclassified.
        assert_eq!(
            parse_sprite_ctrl(r#"{"type":"port_opened","port":8080}"#),
            SpriteCtrl::Ignore
        );
        assert_eq!(parse_sprite_ctrl("not json"), SpriteCtrl::Ignore);
    }

    #[tokio::test]
    async fn drive_exec_tty_close_drops_frames_without_fabricating_exit() {
        use futures_util::SinkExt;
        use tokio_tungstenite::tungstenite::{Message, protocol::Role};

        let (client_io, server_io) = tokio::io::duplex(64 * 1024);
        let client =
            tokio_tungstenite::WebSocketStream::from_raw_socket(client_io, Role::Client, None)
                .await;
        let mut server =
            tokio_tungstenite::WebSocketStream::from_raw_socket(server_io, Role::Server, None)
                .await;
        let (frames_tx, mut frames_rx) = tokio::sync::mpsc::channel(4);
        let (_control_tx, control_rx) = tokio::sync::mpsc::channel(4);
        let (sid_tx, _sid_rx) = tokio::sync::watch::channel(None);
        let driver = tokio::spawn(drive_exec(client, true, frames_tx, control_rx, sid_tx));

        server
            .send(Message::Binary(vec![b'o', b'k'].into()))
            .await
            .unwrap();
        server.send(Message::Close(None)).await.unwrap();

        assert_eq!(
            frames_rx.recv().await,
            Some(ExecFrame::Stdout(b"ok".to_vec()))
        );
        assert_eq!(frames_rx.recv().await, None);
        driver.await.unwrap();
    }

    #[tokio::test]
    async fn drive_exec_abrupt_socket_drop_closes_frames_without_exit() {
        use tokio_tungstenite::tungstenite::protocol::Role;

        let (client_io, server_io) = tokio::io::duplex(64 * 1024);
        let client =
            tokio_tungstenite::WebSocketStream::from_raw_socket(client_io, Role::Client, None)
                .await;
        let server =
            tokio_tungstenite::WebSocketStream::from_raw_socket(server_io, Role::Server, None)
                .await;
        let (frames_tx, mut frames_rx) = tokio::sync::mpsc::channel(4);
        let (_control_tx, control_rx) = tokio::sync::mpsc::channel(4);
        let (sid_tx, _sid_rx) = tokio::sync::watch::channel(None);
        let driver = tokio::spawn(drive_exec(client, true, frames_tx, control_rx, sid_tx));

        drop(server);

        assert_eq!(frames_rx.recv().await, None);
        driver.await.unwrap();
    }

    #[tokio::test]
    async fn drive_exec_preserves_real_exit_frame() {
        use futures_util::SinkExt;
        use tokio_tungstenite::tungstenite::{Message, protocol::Role};

        let (client_io, server_io) = tokio::io::duplex(64 * 1024);
        let client =
            tokio_tungstenite::WebSocketStream::from_raw_socket(client_io, Role::Client, None)
                .await;
        let mut server =
            tokio_tungstenite::WebSocketStream::from_raw_socket(server_io, Role::Server, None)
                .await;
        let (frames_tx, mut frames_rx) = tokio::sync::mpsc::channel(4);
        let (_control_tx, control_rx) = tokio::sync::mpsc::channel(4);
        let (sid_tx, _sid_rx) = tokio::sync::watch::channel(None);
        let driver = tokio::spawn(drive_exec(client, true, frames_tx, control_rx, sid_tx));

        server
            .send(Message::Text(r#"{"type":"exit","exit_code":3}"#.into()))
            .await
            .unwrap();

        assert_eq!(frames_rx.recv().await, Some(ExecFrame::Exit(3)));
        assert_eq!(frames_rx.recv().await, None);
        driver.await.unwrap();
    }

    #[tokio::test]
    async fn drive_exec_maps_unparseable_non_tty_exit_to_unknown_sentinel() {
        use futures_util::SinkExt;
        use tokio_tungstenite::tungstenite::{Message, protocol::Role};

        let (client_io, server_io) = tokio::io::duplex(64 * 1024);
        let client =
            tokio_tungstenite::WebSocketStream::from_raw_socket(client_io, Role::Client, None)
                .await;
        let mut server =
            tokio_tungstenite::WebSocketStream::from_raw_socket(server_io, Role::Server, None)
                .await;
        let (frames_tx, mut frames_rx) = tokio::sync::mpsc::channel(4);
        let (_control_tx, control_rx) = tokio::sync::mpsc::channel(4);
        let (sid_tx, _sid_rx) = tokio::sync::watch::channel(None);
        let driver = tokio::spawn(drive_exec(client, false, frames_tx, control_rx, sid_tx));

        server
            .send(Message::Binary(vec![3, b'x'].into()))
            .await
            .unwrap();

        assert_eq!(
            frames_rx.recv().await,
            Some(ExecFrame::Exit(EXEC_EXIT_STATUS_UNKNOWN))
        );
        assert_eq!(frames_rx.recv().await, None);
        driver.await.unwrap();
    }

    #[test]
    fn rules_from_empty_is_allow_all() {
        assert!(rules_from(&[], &[]).is_empty());
    }

    #[test]
    fn rules_from_block_only_denies_listed_no_default() {
        let r = rules_from(&[], &["evil.com".into()]);
        assert_eq!(
            r,
            vec![PolicyRule {
                domain: "evil.com".into(),
                action: "deny".into()
            }]
        );
    }

    #[test]
    fn rules_from_allow_list_adds_default_deny_and_block_wins_first() {
        let r = rules_from(
            &["github.com".into(), " *.npmjs.org ".into(), "  ".into()],
            &["bad.com".into()],
        );
        // deny(block) first, then allow rules, then trailing default-deny.
        assert_eq!(
            r,
            vec![
                PolicyRule {
                    domain: "bad.com".into(),
                    action: "deny".into()
                },
                PolicyRule {
                    domain: "github.com".into(),
                    action: "allow".into()
                },
                PolicyRule {
                    domain: "*.npmjs.org".into(),
                    action: "allow".into()
                },
                PolicyRule {
                    domain: "*".into(),
                    action: "deny".into()
                },
            ]
        );
    }

    #[test]
    fn sprites_policy_url_and_caps() {
        assert_eq!(
            sprites().policy_network_url("dev").unwrap(),
            "https://api.sprites.dev/v1/sprites/dev/policy/network"
        );
        assert!(Provider::Sprites(sprites()).caps().egress);
        assert!(!Provider::Daytona(provider()).caps().egress);
    }

    #[test]
    fn sprites_checkpoint_urls_and_parse() {
        let p = sprites();
        // List is plural; create is SINGULAR; restore is plural/{id}/restore.
        assert_eq!(
            p.checkpoints_url("dev").unwrap(),
            "https://api.sprites.dev/v1/sprites/dev/checkpoints"
        );
        assert_eq!(
            p.checkpoint_create_url("dev").unwrap(),
            "https://api.sprites.dev/v1/sprites/dev/checkpoint"
        );
        assert_eq!(
            p.restore_url("dev", "v1").unwrap(),
            "https://api.sprites.dev/v1/sprites/dev/checkpoints/v1/restore"
        );
        // Real list element shape: flat {id, create_time, is_auto[, comment]}.
        assert_eq!(
            SpritesProvider::parse_checkpoint(
                &serde_json::json!({"id":"v0","create_time":"2026-06-27T05:08:24Z","is_auto":false})
            ),
            Some(CheckpointInfo {
                id: "v0".into(),
                label: None
            })
        );
        assert_eq!(
            SpritesProvider::parse_checkpoint(&serde_json::json!({"id":"c1","comment":"before"})),
            Some(CheckpointInfo {
                id: "c1".into(),
                label: Some("before".into())
            })
        );
        assert!(SpritesProvider::parse_checkpoint(&serde_json::json!({"x":1})).is_none());
        assert!(Provider::Sprites(sprites()).caps().checkpoints);
    }

    #[test]
    fn checkpoint_stream_id_from_real_ndjson() {
        // The exact NDJSON the live API streams from POST /checkpoint.
        let body = "\
{\"type\":\"info\",\"data\":\"Creating checkpoint...\"}\n\
{\"type\":\"info\",\"data\":\"  ID: v1\"}\n\
{\"type\":\"complete\",\"data\":\"Checkpoint v1 created successfully\"}\n";
        assert_eq!(
            SpritesProvider::parse_checkpoint_stream(body).as_deref(),
            Some("v1")
        );
        // Falls back to the `complete` message when there's no explicit ID line.
        let body2 = "{\"type\":\"complete\",\"data\":\"Checkpoint v7 created successfully\"}";
        assert_eq!(
            SpritesProvider::parse_checkpoint_stream(body2).as_deref(),
            Some("v7")
        );
        assert!(SpritesProvider::parse_checkpoint_stream("garbage").is_none());
    }

    #[test]
    fn join_remote_cleans_slashes() {
        assert_eq!(
            join_remote("/workspace/", "src/main.rs"),
            "/workspace/src/main.rs"
        );
        assert_eq!(join_remote("/workspace", "/a/b"), "/workspace/a/b");
        assert_eq!(join_remote("/workspace", ""), "/workspace");
    }

    #[test]
    fn sprites_fs_url_and_entry_parse() {
        let p = sprites();
        assert_eq!(
            p.fs_op_url("dev", "read").unwrap(),
            "https://api.sprites.dev/v1/sprites/dev/fs/read"
        );
        // The live API uses camelCase `isDir` and numeric `size`.
        assert_eq!(
            SpritesProvider::parse_entry(&serde_json::json!({"name":"src","isDir":true})),
            Some(FileEntry {
                name: "src".into(),
                is_dir: true,
                size: 0
            })
        );
        assert_eq!(
            SpritesProvider::parse_entry(
                &serde_json::json!({"name":"a.rs","isDir":false,"size":12})
            ),
            Some(FileEntry {
                name: "a.rs".into(),
                is_dir: false,
                size: 12
            })
        );
        assert!(Provider::Sprites(sprites()).caps().files);
    }

    #[test]
    fn collect_files_skips_git_and_relativizes() {
        let dir = std::env::temp_dir().join(format!("tg-prov-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir); // best-effort: test tmp cleanup
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::create_dir_all(dir.join(".git")).unwrap();
        std::fs::write(dir.join("src/main.rs"), b"fn main(){}").unwrap();
        std::fs::write(dir.join("README.md"), b"hi").unwrap();
        std::fs::write(dir.join(".git/config"), b"x").unwrap();
        let mut rels: Vec<String> = collect_files(&dir)
            .unwrap()
            .into_iter()
            .map(|(_, r)| r)
            .collect();
        rels.sort();
        assert_eq!(
            rels,
            vec!["README.md".to_string(), "src/main.rs".to_string()]
        );
        let _ = std::fs::remove_dir_all(&dir); // best-effort: test tmp cleanup
    }

    #[test]
    fn daytona_rejects_egress_translation() {
        let p = Provider::Daytona(provider());
        let rt = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        let err = rt
            .block_on(p.set_network_policy("x", &[], &[]))
            .unwrap_err();
        assert!(err.to_string().contains("does not support egress"));
    }

    #[test]
    fn urls_trim_trailing_slash() {
        let p = provider();
        assert_eq!(
            p.sandbox_url().unwrap(),
            "https://app.daytona.io/api/sandbox"
        );
        assert_eq!(
            p.sandbox_id_url("sb-1").unwrap(),
            "https://app.daytona.io/api/sandbox/sb-1"
        );
    }

    #[test]
    fn create_body_carries_snapshot_when_set() {
        let p = provider();
        assert_eq!(
            p.create_body(),
            serde_json::json!({"snapshot": "debian:stable"})
        );
        let bare = DaytonaProvider::new("x", "t", "  ");
        assert_eq!(bare.create_body(), serde_json::json!({}));
    }

    #[test]
    fn proxy_url_and_init_frame() {
        assert_eq!(
            proxy_ws_url("https://api.sprites.dev/v1", "s1").unwrap(),
            "wss://api.sprites.dev/v1/sprites/s1/proxy"
        );
        // init frame selects the in-sandbox target; host is JSON-escaped.
        assert_eq!(
            proxy_init_json("127.0.0.1", 22),
            r#"{"host":"127.0.0.1","port":22}"#
        );
    }

    #[test]
    fn parse_id_handles_flat_and_enveloped() {
        assert_eq!(
            DaytonaProvider::parse_id(&serde_json::json!({"id": "abc"})).as_deref(),
            Some("abc")
        );
        assert_eq!(
            DaytonaProvider::parse_id(&serde_json::json!({"sandbox": {"id": "xyz"}})).as_deref(),
            Some("xyz")
        );
        assert_eq!(
            DaytonaProvider::parse_id(&serde_json::json!({"x": 1})),
            None
        );
    }

    #[test]
    fn parse_list_handles_array_and_envelope() {
        let arr = serde_json::json!([{"id": "a"}, {"id": "b"}, {"name": "no-id"}]);
        assert_eq!(DaytonaProvider::parse_list(&arr), vec!["a", "b"]);
        let env = serde_json::json!({"sandboxes": [{"id": "c"}]});
        assert_eq!(DaytonaProvider::parse_list(&env), vec!["c"]);
        assert!(DaytonaProvider::parse_list(&serde_json::json!({})).is_empty());
    }

    #[test]
    fn exec_for_builds_daytona_ssh_bridge() {
        assert_eq!(
            DaytonaProvider::exec_for("sb-9"),
            ExecKind::Command(vec![
                "daytona".into(),
                "ssh".into(),
                "sb-9".into(),
                "--".into()
            ])
        );
    }

    #[test]
    fn remote_download_components_reject_traversal_and_platform_prefixes() {
        for name in [
            "",
            ".",
            "..",
            "../escape",
            "nested/file",
            "nested\\file",
            "C:escape",
            "\\\\server\\share",
            "bad\0name",
            "bad\nname",
        ] {
            assert!(
                !valid_remote_component(name),
                "accepted unsafe component {name:?}"
            );
        }
        assert!(valid_remote_component("nested-file.txt"));
        assert!(valid_remote_component("λ.txt"));
        assert!(valid_remote_component("~tilde.txt"));
        assert!(valid_remote_component("foo:bar.txt"));
    }

    #[test]
    fn download_publication_refuses_non_file_leaf_and_replaces_regular_file() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("root");
        std::fs::create_dir(&root).unwrap();
        let directory = root.join("directory");
        std::fs::create_dir(&directory).unwrap();
        assert!(publish_download_file(&directory, b"overwrite").is_err());
        let file = root.join("file");
        std::fs::write(&file, b"old").unwrap();
        publish_download_file(&file, b"new").unwrap();
        assert_eq!(std::fs::read(&file).unwrap(), b"new");
    }

    #[test]
    fn download_publication_refuses_symlink_leaf() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("root");
        std::fs::create_dir(&root).unwrap();
        let target = temp.path().join("target");
        std::fs::write(&target, b"safe").unwrap();
        let link = root.join("leaf");
        if let Err(error) = thegn_core::fsperm::create_symlink(&target, &link, false) {
            if matches!(
                error.kind(),
                std::io::ErrorKind::Unsupported | std::io::ErrorKind::PermissionDenied
            ) {
                return;
            }
            panic!("create symlink fixture: {error}");
        }
        assert!(publish_download_file(&link, b"overwrite").is_err());
        assert_eq!(std::fs::read(&target).unwrap(), b"safe");
    }

    #[tokio::test]
    async fn download_dir_refuses_symlinked_intermediate_and_root() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("root");
        let outside = temp.path().join("outside");
        std::fs::create_dir(&root).unwrap();
        std::fs::create_dir(&outside).unwrap();
        let nested_link = root.join("nested");
        if let Err(error) = thegn_core::fsperm::create_symlink(&outside, &nested_link, true) {
            if matches!(
                error.kind(),
                std::io::ErrorKind::Unsupported | std::io::ErrorKind::PermissionDenied
            ) {
                return;
            }
            panic!("create directory symlink fixture: {error}");
        }
        let provider = ListingProvider::new(
            vec![FileEntry {
                name: "nested".into(),
                is_dir: true,
                size: 0,
            }],
            vec![FileEntry {
                name: "file".into(),
                is_dir: false,
                size: 4,
            }],
        );
        assert!(provider.download_dir("id", "/", &root).await.is_err());
        assert!(!outside.join("file").exists());

        let leaf_root = temp.path().join("leaf-root");
        std::fs::create_dir(&leaf_root).unwrap();
        let leaf_target = temp.path().join("leaf-target");
        std::fs::write(&leaf_target, b"safe").unwrap();
        thegn_core::fsperm::create_symlink(&leaf_target, &leaf_root.join("file"), false).unwrap();
        let leaf_provider = ListingProvider::new(
            vec![FileEntry {
                name: "file".into(),
                is_dir: false,
                size: 4,
            }],
            Vec::new(),
        );
        assert!(
            leaf_provider
                .download_dir("id", "/", &leaf_root)
                .await
                .is_err()
        );
        assert_eq!(std::fs::read(&leaf_target).unwrap(), b"safe");

        let root_link = temp.path().join("root-link");
        thegn_core::fsperm::create_symlink(&root, &root_link, true).unwrap();
        assert!(provider.download_dir("id", "/", &root_link).await.is_err());
    }

    struct ListingProvider {
        root: Vec<FileEntry>,
        nested: Vec<FileEntry>,
    }

    impl ListingProvider {
        fn new(root: Vec<FileEntry>, nested: Vec<FileEntry>) -> Self {
            Self { root, nested }
        }
    }

    impl ProviderFiles for ListingProvider {
        fn read<'a>(&'a self, _id: &'a str, _path: &'a str) -> BoxFuture<'a, Result<Vec<u8>>> {
            Box::pin(async { Ok(b"data".to_vec()) })
        }

        fn write<'a>(
            &'a self,
            _id: &'a str,
            _path: &'a str,
            _data: &'a [u8],
        ) -> BoxFuture<'a, Result<()>> {
            Box::pin(async { Ok(()) })
        }

        fn list_dir<'a>(
            &'a self,
            _id: &'a str,
            path: &'a str,
        ) -> BoxFuture<'a, Result<Vec<FileEntry>>> {
            let entries = if path.ends_with("/nested") {
                self.nested.clone()
            } else {
                self.root.clone()
            };
            Box::pin(async move { Ok(entries) })
        }

        fn delete<'a>(&'a self, _id: &'a str, _path: &'a str) -> BoxFuture<'a, Result<()>> {
            Box::pin(async { Ok(()) })
        }
    }

    #[tokio::test]
    async fn download_dir_rejects_hostile_components_and_exact_duplicates() {
        for entries in [
            vec![FileEntry {
                name: "..".into(),
                is_dir: false,
                size: 4,
            }],
            vec![FileEntry {
                name: "a/b".into(),
                is_dir: false,
                size: 4,
            }],
            vec![FileEntry {
                name: "C:x".into(),
                is_dir: false,
                size: 4,
            }],
            vec![
                FileEntry {
                    name: "same".into(),
                    is_dir: false,
                    size: 4,
                },
                FileEntry {
                    name: "same".into(),
                    is_dir: false,
                    size: 4,
                },
            ],
        ] {
            let temp = tempfile::tempdir().unwrap();
            let provider = ListingProvider::new(entries, Vec::new());
            assert!(provider.download_dir("id", "/", temp.path()).await.is_err());
            assert_eq!(std::fs::read_dir(temp.path()).unwrap().count(), 0);
        }
    }

    #[tokio::test]
    async fn download_dir_accepts_case_distinct_linux_names() {
        let temp = tempfile::tempdir().unwrap();
        let provider = ListingProvider::new(
            vec![
                FileEntry {
                    name: "Makefile".into(),
                    is_dir: false,
                    size: 4,
                },
                FileEntry {
                    name: "makefile".into(),
                    is_dir: false,
                    size: 4,
                },
            ],
            Vec::new(),
        );
        provider.download_dir("id", "/", temp.path()).await.unwrap();
        assert_eq!(
            std::fs::read(temp.path().join("Makefile")).unwrap(),
            b"data"
        );
        assert_eq!(
            std::fs::read(temp.path().join("makefile")).unwrap(),
            b"data"
        );
    }

    #[tokio::test]
    async fn control_json_reader_rejects_oversized_chunked_response() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = [0u8; 1024];
            let n = socket.read(&mut request).await.unwrap();
            assert!(n > 0);
            socket
                .write_all(
                    b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n",
                )
                .await
                .unwrap();
            for _ in 0..(CONTROL_BODY_LIMIT / 1024 + 2) {
                if socket.write_all(b"400\r\n").await.is_err()
                    || socket.write_all(&[b'x'; 1024]).await.is_err()
                    || socket.write_all(b"\r\n").await.is_err()
                {
                    break;
                }
            }
        });
        let response = provider_http_client()
            .get(format!("http://{address}/"))
            .timeout(std::time::Duration::from_secs(3))
            .send()
            .await
            .unwrap();
        let error = response_json(response, "test", "GET").await.unwrap_err();
        assert!(error.to_string().contains("response exceeded the"));
        server.await.unwrap();
    }

    #[tokio::test]
    async fn control_json_reader_accepts_empty_created_response() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = [0u8; 1024];
            let n = socket.read(&mut request).await.unwrap();
            assert!(n > 0);
            socket
                .write_all(
                    b"HTTP/1.1 201 Created\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                )
                .await
                .unwrap();
        });
        let response = provider_http_client()
            .get(format!("http://{address}/"))
            .timeout(std::time::Duration::from_secs(3))
            .send()
            .await
            .unwrap();
        assert_eq!(
            response_json(response, "test", "POST").await.unwrap(),
            serde_json::Value::Null
        );
        server.await.unwrap();
    }

    #[tokio::test]
    async fn download_dir_skips_size_drift_using_actual_bytes() {
        let temp = tempfile::tempdir().unwrap();
        let provider = ListingProvider::new(
            vec![FileEntry {
                name: "file".into(),
                is_dir: false,
                size: 0,
            }],
            Vec::new(),
        );
        provider.download_dir("id", "/", temp.path()).await.unwrap();
        assert!(!temp.path().join("file").exists());
    }
}
