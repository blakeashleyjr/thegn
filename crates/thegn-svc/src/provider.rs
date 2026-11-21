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
use std::time::Duration;
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
    provider_http_client_with_deadline(CONTROL_TIMEOUT)
}

/// Same policy with an injectable total deadline; tests use a short one.
fn provider_http_client_with_deadline(total: std::time::Duration) -> reqwest::Client {
    reqwest::Client::builder()
        .connect_timeout(std::time::Duration::from_secs(15))
        .timeout(total)
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
pub(crate) const CONTROL_GLOBAL_PRIORITY_RESERVE: usize = 2;
pub(crate) const CONTROL_PER_AUTHORITY_PRIORITY_RESERVE: usize = 1;
/// File contents use a separate, longer policy than metadata/control calls.
pub(crate) const FILE_TRANSFER_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(600);
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
    if let Some(declared) = response.content_length()
        && declared > limit as u64
    {
        anyhow::bail!(
            "{provider}: {operation} response exceeded the {limit}-byte body limit (declared {declared} bytes)"
        );
    }

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

const DESTROY_CONFIRM_BUDGET: Duration = Duration::from_secs(30);
const DESTROY_CONFIRM_POLL: Duration = Duration::from_secs(2);

/// Overall ceiling for one-shot exec (THE-533); above the 30 min per-step
/// provisioning ceiling.
pub const RUN_EXEC_DEADLINE: Duration = Duration::from_secs(31 * 60);

/// Bounds simultaneous provider control calls across provider instances.
///
/// Ordinary calls use only the normal global and per-authority pools. Priority
/// calls first try those pools without waiting, then use the small reserved
/// global and per-authority pools so destructive cleanup and reconciliation can
/// make progress when ordinary reads have filled the normal capacity.
pub(crate) struct ControlGate {
    global: Arc<tokio::sync::Semaphore>,
    per_authority: Mutex<HashMap<String, Arc<tokio::sync::Semaphore>>>,
    priority_global: Arc<tokio::sync::Semaphore>,
    priority_per_authority: Mutex<HashMap<String, Arc<tokio::sync::Semaphore>>>,
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
            priority_global: Arc::new(tokio::sync::Semaphore::new(CONTROL_GLOBAL_PRIORITY_RESERVE)),
            priority_per_authority: Mutex::new(HashMap::new()),
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

    fn priority_authority_gate(&self, authority: &str) -> Arc<tokio::sync::Semaphore> {
        let mut gates = self
            .priority_per_authority
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        Arc::clone(gates.entry(authority.to_string()).or_insert_with(|| {
            Arc::new(tokio::sync::Semaphore::new(
                CONTROL_PER_AUTHORITY_PRIORITY_RESERVE,
            ))
        }))
    }

    fn try_acquire_pair(
        authority: Arc<tokio::sync::Semaphore>,
        global: Arc<tokio::sync::Semaphore>,
    ) -> Option<ControlPermit> {
        let authority_permit = authority.try_acquire_owned().ok()?;
        let global_permit = global.try_acquire_owned().ok()?;
        Some(ControlPermit {
            _authority: authority_permit,
            _global: global_permit,
        })
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

    pub(crate) async fn acquire_priority(&self, authority: &str) -> Result<ControlPermit> {
        let authority_gate = self.authority_gate(authority);
        if let Some(permit) =
            Self::try_acquire_pair(Arc::clone(&authority_gate), Arc::clone(&self.global))
        {
            return Ok(permit);
        }

        let priority_authority = self.priority_authority_gate(authority);
        let authority_permit = tokio::time::timeout(
            CONTROL_TIMEOUT,
            Arc::clone(&priority_authority).acquire_owned(),
        )
        .await
        .map_err(|_| anyhow!("provider priority concurrency cap reached"))?
        .map_err(|_| anyhow!("provider priority concurrency gate closed"))?;
        let global_permit = tokio::time::timeout(
            CONTROL_TIMEOUT,
            Arc::clone(&self.priority_global).acquire_owned(),
        )
        .await
        .map_err(|_| anyhow!("provider priority concurrency cap reached"))?
        .map_err(|_| anyhow!("provider priority concurrency gate closed"))?;
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

    /// Parse a complete list response (an array, or `{ "sandboxes": [...] }`).
    /// Test-only since the typed inventory replaced its production caller.
    #[cfg(test)]
    fn parse_list(v: &serde_json::Value) -> Result<Vec<String>> {
        Ok(Self::parse_infos(v)?
            .into_iter()
            .map(|row| row.id)
            .collect())
    }

    /// The typed sibling of the test-only `parse_list` (same envelope and id
    /// refusals): optional `status|state`, `created_at|createdAt`,
    /// `class|size` and `region|target` are read tolerantly (absent or
    /// wrong-typed ⇒ `None`).
    fn parse_infos(v: &serde_json::Value) -> Result<Vec<SandboxInfo>> {
        let arr = v
            .as_array()
            .or_else(|| v.get("sandboxes").and_then(serde_json::Value::as_array))
            .ok_or_else(|| {
                anyhow!("daytona: GET /sandbox returned an unrecognised list envelope")
            })?;
        arr.iter()
            .enumerate()
            .map(|(index, sandbox)| {
                let id = Self::parse_id(sandbox)
                    .filter(|id| !id.is_empty())
                    .ok_or_else(|| {
                        anyhow!("daytona: GET /sandbox element {index} has no sandbox id")
                    })?;
                let obj = sandbox
                    .get("sandbox")
                    .filter(|inner| inner.is_object())
                    .unwrap_or(sandbox);
                Ok(SandboxInfo {
                    id,
                    name: None,
                    state: inventory_text(obj, &["status", "state"]),
                    size: inventory_text(obj, &["class", "size"]),
                    region: inventory_text(obj, &["region", "target"]),
                    created_at_ms: inventory_time_ms(obj, &["created_at", "createdAt"]),
                })
            })
            .collect()
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
            let _permit = CONTROL_GATE.acquire_priority(&self.authority()?).await?;
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
            let (inventory, total) = self.list_inventory_detail().await;
            match inventory {
                crate::vps::Inventory::Complete(ids) => Ok(ids),
                crate::vps::Inventory::Partial { rows, .. } => Err(anyhow!(
                    "daytona: GET /sandbox returned a partial inventory (total {}, got {})",
                    total.unwrap_or_default(),
                    rows.len()
                )),
                crate::vps::Inventory::Unknown(reason) => Err(anyhow!(reason)),
            }
        })
    }
}

impl DaytonaProvider {
    /// The sandbox id inventory with its completeness made explicit.
    pub async fn list_inventory(&self) -> crate::vps::Inventory<String> {
        self.list_inventory_detail().await.0
    }

    /// The typed `thegn fleet` inventory (one unpaged `GET /sandbox`).
    pub async fn sandbox_inventory(&self) -> Result<InventoryReport> {
        InventoryReport::from_inventory(self.list_infos_detail().await.0)
    }

    /// The id inventory plus, for `TotalMismatch`, the announced total.
    async fn list_inventory_detail(&self) -> (crate::vps::Inventory<String>, Option<u64>) {
        let (inventory, total) = self.list_infos_detail().await;
        (inventory.map_rows(|row| row.id), total)
    }

    /// The typed inventory plus, for `TotalMismatch`, the announced total.
    async fn list_infos_detail(&self) -> (crate::vps::Inventory<SandboxInfo>, Option<u64>) {
        use crate::vps::{Inventory, PartialReason};
        let fetched: Result<serde_json::Value> = async {
            let _permit = CONTROL_GATE.acquire(&self.authority()?).await?;
            let resp = self
                .client
                .get(self.sandbox_url()?)
                .bearer_auth(&self.token)
                .timeout(CONTROL_TIMEOUT)
                .send()
                .await
                .context("daytona: GET /sandbox")?;
            let status = resp.status();
            if !status.is_success() {
                return Err(anyhow!("daytona list failed ({status})"));
            }
            response_json(resp, "daytona", "GET /sandbox").await
        }
        .await;
        let body = match fetched {
            Ok(body) => body,
            Err(error) => return (Inventory::Unknown(format!("{error:#}")), None),
        };
        let ids = match Self::parse_infos(&body) {
            Ok(ids) => ids,
            Err(error) => return (Inventory::Unknown(format!("{error:#}")), None),
        };
        let announced = body
            .get("total")
            .or_else(|| body.get("totalItems"))
            .and_then(serde_json::Value::as_u64);
        match announced {
            Some(total) if total != ids.len() as u64 => (
                Inventory::Partial {
                    rows: ids,
                    reason: PartialReason::TotalMismatch,
                },
                Some(total),
            ),
            _ => (Inventory::Complete(ids), None),
        }
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
    /// Who this host is in the sprites ownership ledger. `None` refuses both
    /// create (an unrecorded sprite) and destroy (no proof of ownership).
    scope: Option<crate::sprites::ledger::OwnerScope>,
    /// Provider ids named by this host's worktree location rows: the proof for
    /// sprites created before the ledger existed.
    legacy_ids: Vec<String>,
    /// The memory policy applied after create (`0` = leave the default).
    memory_limit_mb: u32,
    /// Optional page size for list requests (`None` preserves the API default).
    list_page_size: Option<u32>,
    /// The ledger reserve cap (`0` = no cap).
    max_instances: usize,
    /// The ledger directory (`None` = `vps::registry::dir()`); tests isolate it.
    ledger_dir: Option<std::path::PathBuf>,
    destroy_budget: Duration,
    destroy_poll: Duration,
    client: reqwest::Client,
}

#[derive(Debug, PartialEq, Eq)]
struct SpritesListPage {
    rows: Vec<SandboxInfo>,
    next: Option<String>,
}

#[derive(Debug, PartialEq, Eq)]
struct CheckpointPage {
    items: Vec<CheckpointInfo>,
    next: Option<String>,
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
            scope: None,
            legacy_ids: Vec::new(),
            memory_limit_mb: 0,
            list_page_size: None,
            max_instances: 0,
            ledger_dir: None,
            destroy_budget: DESTROY_CONFIRM_BUDGET,
            destroy_poll: DESTROY_CONFIRM_POLL,
            client: provider_http_client(),
        }
    }

    /// The ownership scope create records under and destroy proves against.
    pub fn with_scope(mut self, scope: crate::sprites::ledger::OwnerScope) -> Self {
        self.scope = Some(scope);
        self
    }

    /// Scope this provider to the host's controller id and the account
    /// fingerprint of `credential_ref` (the token's ref name, never the token).
    /// Fails when the controller id cannot be loaded or the API base is bad.
    pub fn with_host_scope(self, credential_ref: &str) -> Result<Self> {
        let owner = crate::vps::controller::controller_id()?;
        let account = self.account(credential_ref)?;
        Ok(self.with_scope(crate::sprites::ledger::OwnerScope { owner, account }))
    }

    /// The credential-free account fingerprint for the ledger: the API
    /// authority and the credential ref name (never the token).
    pub fn account(&self, credential_ref: &str) -> Result<String> {
        Ok(crate::vps::registry::account_fingerprint(
            crate::sprites::ledger::LEDGER_PROVIDER,
            &self.authority()?,
            credential_ref,
        ))
    }

    /// Provider ids of this host's worktree location rows (legacy proof).
    pub fn with_legacy(mut self, ids: Vec<String>) -> Self {
        self.legacy_ids = ids;
        self
    }

    /// The memory policy limit applied after create (`0` = none).
    pub fn with_memory_limit_mb(mut self, limit_mb: u32) -> Self {
        self.memory_limit_mb = limit_mb;
        self
    }

    /// Set the maximum number of sprites returned by each list request (clamped to the API's 1..=500).
    pub fn with_list_page_size(mut self, n: u32) -> Self {
        self.list_page_size = Some(n.clamp(1, 500));
        self
    }

    /// The ledger reserve cap (`0` = no cap, today's behaviour for sprites).
    pub fn with_max_instances(mut self, max_instances: u32) -> Self {
        self.max_instances = usize::try_from(max_instances).unwrap_or(usize::MAX);
        self
    }

    /// Use `dir` as the ownership ledger instead of the state directory.
    pub fn with_ledger_dir(mut self, dir: std::path::PathBuf) -> Self {
        self.ledger_dir = Some(dir);
        self
    }

    /// Override the post-DELETE confirmation budget and poll (tests; the default is 30 s / 2 s).
    pub fn with_destroy_confirm(mut self, budget: Duration, poll: Duration) -> Self {
        self.destroy_budget = budget;
        self.destroy_poll = poll;
        self
    }

    /// The ownership scope, if this provider may create and destroy.
    pub fn scope(&self) -> Option<&crate::sprites::ledger::OwnerScope> {
        self.scope.as_ref()
    }

    /// The memory policy limit applied after create (`0` = none).
    pub fn memory_limit_mb(&self) -> u32 {
        self.memory_limit_mb
    }

    fn ledger_dir(&self) -> std::path::PathBuf {
        self.ledger_dir
            .clone()
            .unwrap_or_else(crate::vps::registry::dir)
    }

    /// The validated HTTP context for this provider's own sprite.
    pub(crate) fn sprites_http(&self) -> Result<crate::sprites::SpritesHttp<'_>> {
        Ok(crate::sprites::SpritesHttp {
            client: &self.client,
            base: self.base()?,
            token: &self.token,
            name: &self.name,
        })
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

    fn sprites_list_url(&self, token: Option<&str>) -> Result<String> {
        let url = self.sprites_url()?;
        if token.is_none() && self.list_page_size.is_none() {
            return Ok(url);
        }
        let mut url = reqwest::Url::parse(&url)
            .map_err(|error| anyhow!("sprites: invalid list URL: {error}"))?;
        if let Some(page_size) = self.list_page_size {
            url.query_pairs_mut()
                .append_pair("max_results", &page_size.to_string());
        }
        if let Some(token) = token {
            url.query_pairs_mut()
                .append_pair("continuation_token", token);
        }
        Ok(url.to_string())
    }

    fn checkpoints_list_url(&self, name: &str, token: Option<&str>) -> Result<String> {
        let url = self.checkpoints_url(name)?;
        let Some(token) = token else {
            return Ok(url);
        };
        let mut url = reqwest::Url::parse(&url)
            .map_err(|error| anyhow!("sprites: invalid checkpoint list URL: {error}"))?;
        url.query_pairs_mut()
            .append_pair("continuation_token", token);
        Ok(url.to_string())
    }

    fn sprite_name_url(&self, name: &str) -> Result<String> {
        let name = checked_sprite_name(name)?;
        Ok(format!("{}/sprites/{name}", self.base()?))
    }

    /// The create body — sprites are named by the caller.
    fn create_body(name: &str) -> serde_json::Value {
        serde_json::json!({ "name": name, "url_settings": { "auth": "sprite" } })
    }

    /// Pull a sprite name from a create/list element (`{ "name": … }`, tolerating
    /// a `{ "sprite": { "name" } }` envelope).
    fn parse_name(v: &serde_json::Value) -> Option<String> {
        v.get("name")
            .or_else(|| v.get("sprite").and_then(|s| s.get("name")))
            .and_then(|n| n.as_str())
            .map(str::to_string)
    }

    fn continuation(v: &serde_json::Value, what: &str) -> Result<Option<String>> {
        match v.get("has_more") {
            None | Some(serde_json::Value::Bool(false)) => {
                if v.get("next_continuation_token")
                    .and_then(serde_json::Value::as_str)
                    .is_some_and(|token| !token.is_empty())
                {
                    return Err(anyhow!(
                        "{what} returned a continuation token without has_more; refusing an ambiguous inventory"
                    ));
                }
                Ok(None)
            }
            Some(serde_json::Value::Bool(true)) => {
                let token = v
                    .get("next_continuation_token")
                    .and_then(serde_json::Value::as_str)
                    .filter(|token| !token.is_empty())
                    .ok_or_else(|| {
                        anyhow!(
                            "{what} returned a partial inventory (has_more without next_continuation_token)"
                        )
                    })?;
                Ok(Some(token.to_string()))
            }
            Some(_) => Err(anyhow!(
                "{what} returned a partial inventory (invalid has_more)"
            )),
        }
    }

    /// Parse one list page (an array, or `{ "sprites": [...] }`).
    fn parse_list_page(v: &serde_json::Value) -> Result<SpritesListPage> {
        let next = Self::continuation(v, "sprites: GET /sprites")?;
        let arr = v
            .as_array()
            .or_else(|| v.get("sprites").and_then(serde_json::Value::as_array))
            .ok_or_else(|| {
                anyhow!("sprites: GET /sprites returned an unrecognised list envelope")
            })?;
        let rows = arr
            .iter()
            .enumerate()
            .map(|(index, sprite)| {
                let name = Self::parse_name(sprite)
                    .filter(|name| !name.is_empty())
                    .ok_or_else(|| {
                        anyhow!("sprites: GET /sprites element {index} has no sprite name")
                    })?;
                // Optional detail, read tolerantly (absent or wrong-typed ⇒
                // `None`); only the name is strict.
                let obj = sprite
                    .get("sprite")
                    .filter(|inner| inner.is_object())
                    .unwrap_or(sprite);
                Ok(SandboxInfo {
                    id: name,
                    name: inventory_text(obj, &["id"]),
                    state: inventory_text(obj, &["status", "state"]),
                    size: inventory_text(obj, &["class", "size"]),
                    region: inventory_text(obj, &["region", "target"]),
                    created_at_ms: inventory_time_ms(obj, &["created_at", "createdAt"]),
                })
            })
            .collect::<Result<Vec<_>>>()?;
        Ok(SpritesListPage { rows, next })
    }

    async fn get_list_page(&self, url: &str, operation: &str) -> Result<serde_json::Value> {
        let mut last: Option<String> = None;
        for attempt in 0..CONTROL_ATTEMPTS {
            match self
                .client
                .get(url)
                .bearer_auth(&self.token)
                .timeout(CONTROL_TIMEOUT)
                .send()
                .await
            {
                Ok(resp) => {
                    let status = resp.status();
                    if status.is_success() {
                        return response_json(resp, "sprites", operation).await;
                    }
                    last = Some(status.to_string());
                    if !transient_status(status) {
                        break;
                    }
                }
                Err(error) => {
                    last = Some(error.to_string());
                    if !transient_err(&error) {
                        break;
                    }
                }
            }
            if attempt + 1 < CONTROL_ATTEMPTS {
                tokio::time::sleep(CONTROL_BACKOFF).await;
            }
        }
        Err(anyhow!(
            "sprites {operation} failed ({})",
            last.unwrap_or_else(|| "no response".into())
        ))
    }

    /// Drop `name`'s ledger record off the async runtime (best-effort: a
    /// leftover record only makes the reaper look again).
    async fn forget_record(&self, dir: &std::path::Path, name: &str) {
        let Some(scope) = self.scope.clone() else {
            tracing::warn!(target: "thegn::sprites", name, "sprites: no ownership scope; ledger record left alone");
            return;
        };
        let (dir, name) = (dir.to_path_buf(), name.to_string());
        match tokio::task::spawn_blocking(move || {
            crate::sprites::ledger::forget_at(&dir, &scope, &name)
        })
        .await
        {
            Ok(Ok(())) => {}
            Ok(Err(error)) => {
                tracing::warn!(target: "thegn::sprites", error = %format!("{error:#}"), "sprites: ledger record left in place");
            }
            Err(error) => {
                tracing::warn!(target: "thegn::sprites", %error, "sprites: ledger cleanup worker failed");
            }
        }
    }

    /// The exec handle for a sprite: thegn's own `sprite-exec` self-bridge, which
    /// streams a PTY over the native exec API (reattaching across network blips),
    /// so the handle never depends on the vendor `sprite` CLI being installed.
    fn exec_for(name: &str) -> ExecKind {
        ExecKind::Command(vec![
            thegn_core::util::self_exe_str(),
            "sprite-exec".into(),
            name.to_string(),
            "--".into(),
        ])
    }

    /// After an accepted DELETE, poll until the control plane gives definite
    /// proof that the sprite is gone. The caller holds the priority permit for
    /// the DELETE and this confirmation, so the GET must not acquire another.
    async fn confirm_sprite_gone(
        &self,
        id: &str,
        budget: Duration,
        poll: Duration,
    ) -> Result<bool> {
        let deadline = tokio::time::Instant::now() + budget;
        loop {
            if tokio::time::Instant::now() >= deadline {
                return Ok(false);
            }
            let response = match tokio::time::timeout_at(
                deadline,
                Box::pin(
                    self.client
                        .get(self.sprite_name_url(id)?)
                        .bearer_auth(&self.token)
                        .timeout(CONTROL_TIMEOUT)
                        .send(),
                ),
            )
            .await
            {
                Ok(response) => response.context("sprites: GET /sprites/{name} after DELETE")?,
                Err(_) => return Ok(false),
            };
            if response.status() == reqwest::StatusCode::NOT_FOUND {
                return Ok(true);
            }

            let now = tokio::time::Instant::now();
            if now >= deadline {
                return Ok(false);
            }
            tokio::time::sleep(poll.min(deadline - now)).await;
        }
    }

    /// A listed sprite is used only when this instance's ledger proves it
    /// created it: provisioning uploads login files into it. Refuses with the
    /// `thegn env adopt` hint otherwise. Never sends a mutating request.
    pub(crate) async fn prove_listed_sprite(&self, name: &str) -> Result<()> {
        let scope = self.scope.clone().ok_or_else(|| {
            anyhow!(
                "sprites: no controller id; refusing to use sprite {name}, which this host cannot prove it created"
            )
        })?;
        let dir = self.ledger_dir();
        let error_name = name.to_string();
        let (ledger, name, legacy) = (dir, error_name.clone(), self.legacy_ids.clone());
        let proof = tokio::task::spawn_blocking(move || {
            crate::sprites::ledger::prove_owned_at(&ledger, &scope, &name, &legacy)
        })
        .await
        .context("sprites: ledger worker failed")?;
        proof.map(|_| ()).map_err(|refusal| {
            tracing::debug!(target: "thegn::sprites", sprite = %error_name, "reuse refused: {refusal:#}");
            anyhow!(
                "sprites: a sprite named {error_name} exists in this Sprites account but this thegn instance has no record of creating it; refusing to use it (it may belong to another thegn instance or another person). If it is yours: thegn env adopt {error_name} --env <env>"
            )
        })
    }

    async fn destroy_confirmed(&self, id: &str, budget: Duration, poll: Duration) -> Result<()> {
        // Ownership before the irreversible delete: thegn never deletes a
        // sprite it cannot prove it created.
        let scope = self.scope.clone().ok_or_else(|| {
            anyhow!("sprites: no controller id; refusing to delete {id}, which this host cannot prove it created")
        })?;
        let dir = self.ledger_dir();
        {
            let (ledger, name, legacy) = (dir.clone(), id.to_string(), self.legacy_ids.clone());
            let proof = tokio::task::spawn_blocking(move || {
                crate::sprites::ledger::prove_owned_at(&ledger, &scope, &name, &legacy)
            })
            .await
            .context("sprites: ledger worker failed")?;
            if let Err(refusal) = proof {
                // No proof. Teardown stays idempotent: a record dropped by an
                // earlier destroy or by `env reap --apply` must not turn a
                // second teardown into an error. One non-mutating GET decides;
                // only a definite 404 (nothing to delete) is OK. The DELETE is
                // never sent on this path.
                let gone = match self
                    .client
                    .get(self.sprite_name_url(id)?)
                    .bearer_auth(&self.token)
                    .timeout(CONTROL_TIMEOUT)
                    .send()
                    .await
                {
                    Ok(resp) => resp.status() == reqwest::StatusCode::NOT_FOUND,
                    Err(_) => false,
                };
                if gone {
                    return Ok(());
                }
                return Err(refusal.context(format!("sprites: refusing to delete {id}")));
            }
        }
        let _permit = CONTROL_GATE.acquire_priority(&self.authority()?).await?;
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
            if status == reqwest::StatusCode::NOT_FOUND {
                if let Some(account) = custody_account.as_deref() {
                    thegn_core::managed_ssh::record_revoked("sprites", account, id)
                        .context("sprites: retire managed SSH custody")?;
                }
                self.forget_record(&dir, id).await;
                return Ok(());
            }
            if status.is_success() {
                let confirmed = match self.confirm_sprite_gone(id, budget, poll).await {
                    Ok(confirmed) => confirmed,
                    Err(error) => {
                        tracing::debug!(target: "thegn::sprites", %error, "sprites: delete confirmation GET failed");
                        false
                    }
                };
                if confirmed {
                    if let Some(account) = custody_account.as_deref() {
                        thegn_core::managed_ssh::record_revoked("sprites", account, id)
                            .context("sprites: retire managed SSH custody")?;
                    }
                    self.forget_record(&dir, id).await;
                    return Ok(());
                }
                return Err(anyhow!(
                    "sprites: DELETE for {id} was accepted but the sprite still exists after {}s; its ledger record is kept so `thegn wt rm` or `thegn env reap --provider sprites` can finish it",
                    budget.as_secs()
                ));
            }
            last_status = Some(status);
            if !transient_status(status) {
                break;
            }
            if attempt + 1 < ATTEMPTS {
                tokio::time::sleep(Duration::from_millis(500)).await;
            }
        }
        Err(anyhow!(
            "sprites destroy failed ({})",
            last_status
                .map(|s| s.to_string())
                .unwrap_or_else(|| "no response".into())
        ))
    }
}

/// A sprite name safe to splice into a URL path: one DNS-style name
/// (`crate::vps::registry::checked_name`), so no `/`, `?`, `#` or `..`.
fn checked_sprite_name(name: &str) -> Result<&str> {
    crate::vps::registry::checked_name(name).map_err(|_| anyhow!("sprites: invalid sprite name"))
}

/// An exec session id safe as one URL path segment: 1..=128 of
/// `[A-Za-z0-9_.-]`, not `.` or `..`.
fn checked_exec_session(session: &str) -> Result<&str> {
    if (1..=128).contains(&session.len())
        && session != "."
        && session != ".."
        && session
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'.' | b'-'))
    {
        Ok(session)
    } else {
        Err(anyhow!("sprites: invalid exec session id"))
    }
}

impl RemoteProvider for SpritesProvider {
    fn create<'a>(&'a self) -> BoxFuture<'a, Result<SandboxHandle>> {
        Box::pin(async move {
            // Ownership first: a sprite is never created without a ledger
            // record, so a missing scope refuses before any request.
            let scope = self.scope.clone().ok_or_else(|| {
                anyhow!("sprites: no controller id; refusing to create an unrecorded sprite")
            })?;
            let _permit = CONTROL_GATE.acquire(&self.authority()?).await?;
            if self.name.is_empty() {
                return Err(anyhow!(
                    "sprites: set `[env.<name>.provider] id = \"<sprite-name>\"` — sprites are named"
                ));
            }
            let dir = self.ledger_dir();
            // Proven before the reserve, so a 409 can tell "ours from an earlier
            // attempt" from "someone else's name": the fresh intent written
            // below must never be what proves a foreign sprite ours.
            let prior = {
                let (dir, scope, name, legacy) = (
                    dir.clone(),
                    scope.clone(),
                    self.name.clone(),
                    self.legacy_ids.clone(),
                );
                tokio::task::spawn_blocking(move || {
                    crate::sprites::ledger::prove_owned_at(&dir, &scope, &name, &legacy)
                })
                .await
                .context("sprites: ledger worker failed")?
                .is_ok()
            };
            let mut guard = {
                let (dir, scope, name, cap) = (
                    dir.clone(),
                    scope.clone(),
                    self.name.clone(),
                    self.max_instances,
                );
                tokio::task::spawn_blocking(move || {
                    crate::sprites::ledger::reserve_at(&dir, &scope, &name, cap)
                })
                .await
                .context("sprites: ledger worker failed")?
                .context("sprites: could not record the create intent; refusing to create")?
            };
            // From here the POST may take effect, and this future may be
            // cancelled mid-request: keep the intent unless the create was
            // definitively refused (removed explicitly below), so the reaper can
            // resolve an ambiguous outcome.
            guard.preserve();
            drop(guard);

            // Bounded per-request timeout + a small retry budget: a cold-booting
            // control plane can stall or transiently 5xx, and an untimed create is
            // exactly what hung provisioning "forever" on startup.
            let mut last: Option<String> = None;
            let mut refused = false;
            let mut created: Option<String> = None;
            let mut ambiguous = false;
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
                            if let Some(echoed) =
                                Self::parse_name(&body).filter(|echoed| *echoed != self.name)
                            {
                                self.forget_record(&dir, &self.name).await;
                                let shown = echoed
                                    .chars()
                                    .filter(|character| !character.is_control())
                                    .take(64)
                                    .collect::<String>();
                                return Err(anyhow!(
                                    "sprites: create answered with sprite name {shown:?}, not the requested {requested}; refusing it (if a sprite was made, thegn env reap --provider sprites --unrecorded lists it)",
                                    requested = self.name
                                ));
                            }
                            // The server may omit the name; use the requested one then.
                            created = Some(self.name.clone());
                            break;
                        }
                        if status == reqwest::StatusCode::CONFLICT {
                            if prior {
                                // Ours from an earlier (ambiguous) attempt.
                                created = Some(self.name.clone());
                                break;
                            }
                            // Someone else's name: drop the fresh intent so it
                            // can never prove a foreign sprite ours.
                            self.forget_record(&dir, &self.name).await;
                            let ambiguity = if ambiguous {
                                " (an earlier attempt in this create may have made it: thegn env reap --provider sprites --unrecorded lists it, and thegn env adopt records it)"
                            } else {
                                ""
                            };
                            return Err(anyhow!(
                                "sprites: a sprite named {} already exists and is not recorded by this host; refusing to adopt it{}",
                                self.name,
                                ambiguity
                            ));
                        }
                        last = Some(status.to_string());
                        if transient_status(status) {
                            ambiguous = true;
                        }
                        if !transient_status(status) {
                            // A definite 4xx: nothing was created.
                            refused = status.is_client_error();
                            break;
                        }
                    }
                    Err(e) => {
                        ambiguous = true;
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
            let Some(name) = created else {
                if refused {
                    self.forget_record(&dir, &self.name).await;
                }
                // Otherwise (transport failure, exhausted 5xx) the intent stays
                // for the reaper: the sprite may exist.
                return Err(anyhow!(
                    "sprites create failed ({})",
                    last.unwrap_or_else(|| "no response".into())
                ));
            };

            let committed = {
                let (dir, scope, name) = (dir.clone(), scope.clone(), self.name.clone());
                tokio::task::spawn_blocking(move || {
                    crate::sprites::ledger::commit_at(&dir, &scope, &name)
                })
                .await
                .context("sprites: ledger worker failed")
                .and_then(|result| result)
            };
            committed.with_context(|| {
                format!(
                    "sprites: {name} was created but could not be marked ready in the ledger (its intent is kept)"
                )
            })?;

            if self.memory_limit_mb > 0 {
                crate::sprites::resources::apply(&self.sprites_http()?, self.memory_limit_mb)
                    .await
                    .with_context(|| {
                        format!(
                            "sprites: {name} was created and recorded, but its memory policy ({} MiB) could not be applied",
                            self.memory_limit_mb
                        )
                    })?;
            }
            let exec = Self::exec_for(&name);
            Ok(SandboxHandle { id: name, exec })
        })
    }

    fn destroy<'a>(&'a self, id: &'a str) -> BoxFuture<'a, Result<()>> {
        Box::pin(self.destroy_confirmed(id, self.destroy_budget, self.destroy_poll))
    }

    fn list<'a>(&'a self) -> BoxFuture<'a, Result<Vec<String>>> {
        Box::pin(async move {
            let (inventory, duplicate) = self.list_inventory_detail().await;
            match inventory {
                crate::vps::Inventory::Complete(names) => Ok(names),
                crate::vps::Inventory::Partial { reason, .. } => Err(match reason {
                    crate::vps::PartialReason::PageCap => {
                        anyhow!("sprites list exceeded the page limit; refusing a partial list")
                    }
                    crate::vps::PartialReason::RowCap => {
                        anyhow!("sprites list exceeded the row limit; refusing a partial list")
                    }
                    crate::vps::PartialReason::DuplicateId => anyhow!(
                        "sprites list returned duplicate sprite name {} (the continuation token may have been ignored)",
                        duplicate.unwrap_or_default()
                    ),
                    crate::vps::PartialReason::Budget
                    | crate::vps::PartialReason::TotalMismatch => anyhow!(
                        "sprites list exceeded the {} second budget; refusing a partial list",
                        crate::vps::INVENTORY_BUDGET.as_secs()
                    ),
                }),
                crate::vps::Inventory::Unknown(reason) => Err(anyhow!(reason)),
            }
        })
    }
}

impl SpritesProvider {
    /// The sprite name inventory with its completeness made explicit.
    pub async fn list_inventory(&self) -> crate::vps::Inventory<String> {
        self.list_inventory_detail().await.0
    }

    /// The typed `thegn fleet` inventory (the same paged, bounded listing).
    pub async fn sandbox_inventory(&self) -> Result<InventoryReport> {
        InventoryReport::from_inventory(self.list_infos_detail().await.0)
    }

    /// The name inventory plus, for `DuplicateId`, the offending name (so
    /// `list()` can keep its legacy message).
    async fn list_inventory_detail(&self) -> (crate::vps::Inventory<String>, Option<String>) {
        let (inventory, duplicate) = self.list_infos_detail().await;
        (inventory.map_rows(|row| row.id), duplicate)
    }

    /// The typed inventory plus, for `DuplicateId`, the offending name.
    async fn list_infos_detail(&self) -> (crate::vps::Inventory<SandboxInfo>, Option<String>) {
        use crate::vps::{Inventory, PartialReason};
        let authority = match self.authority() {
            Ok(authority) => authority,
            Err(error) => return (Inventory::Unknown(format!("{error:#}")), None),
        };
        let _permit = match CONTROL_GATE.acquire(&authority).await {
            Ok(permit) => permit,
            Err(error) => return (Inventory::Unknown(format!("{error:#}")), None),
        };
        let listing = async {
            let mut names: Vec<SandboxInfo> = Vec::new();
            let mut seen_names = std::collections::HashSet::new();
            let mut seen_tokens = std::collections::HashSet::new();
            let mut token: Option<String> = None;
            let mut pages: u32 = 0;
            loop {
                if pages >= crate::vps::MAX_INVENTORY_PAGES {
                    return (
                        Inventory::Partial {
                            rows: names,
                            reason: PartialReason::PageCap,
                        },
                        None,
                    );
                }
                pages += 1;
                let page = async {
                    let url = self.sprites_list_url(token.as_deref())?;
                    let body = self.get_list_page(&url, "GET /sprites").await?;
                    Self::parse_list_page(&body)
                }
                .await;
                let page = match page {
                    Ok(page) => page,
                    Err(error) => return (Inventory::Unknown(format!("{error:#}")), None),
                };
                if names.len().saturating_add(page.rows.len()) > crate::vps::MAX_INVENTORY_ROWS {
                    return (
                        Inventory::Partial {
                            rows: names,
                            reason: PartialReason::RowCap,
                        },
                        None,
                    );
                }
                for row in page.rows {
                    if !seen_names.insert(row.id.clone()) {
                        return (
                            Inventory::Partial {
                                rows: names,
                                reason: PartialReason::DuplicateId,
                            },
                            Some(row.id),
                        );
                    }
                    names.push(row);
                }
                let Some(next) = page.next else {
                    return (Inventory::Complete(names), None);
                };
                if !seen_tokens.insert(next.clone()) {
                    return (
                        Inventory::Unknown(
                            "sprites list returned a repeated continuation token".to_string(),
                        ),
                        None,
                    );
                }
                token = Some(next);
            }
        };
        // Rows collected inside a timed-out future are lost with it, so a
        // budget overrun reports no rows.
        match tokio::time::timeout(crate::vps::INVENTORY_BUDGET, listing).await {
            Ok(result) => result,
            Err(_) => (
                Inventory::Partial {
                    rows: Vec::new(),
                    reason: PartialReason::Budget,
                },
                None,
            ),
        }
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

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PolicyRuleError {
    NonAscii { entry: String },
    IpLiteral { entry: String },
    Malformed { entry: String, reason: &'static str },
}

impl std::fmt::Display for PolicyRuleError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NonAscii { entry } => write!(
                formatter,
                "network policy entry {entry:?} is not ASCII; use the punycode (xn--) form"
            ),
            Self::IpLiteral { entry } => write!(
                formatter,
                "network policy entry {entry:?} is an IP literal; provider rules require domains"
            ),
            Self::Malformed { entry, reason } => {
                write!(
                    formatter,
                    "network policy entry {entry:?} is malformed: {reason}"
                )
            }
        }
    }
}

impl std::error::Error for PolicyRuleError {}

fn canonical_domain(entry: &str) -> std::result::Result<Option<String>, PolicyRuleError> {
    let trimmed = entry.trim();
    if trimmed.is_empty() {
        return Ok(None);
    }
    if !trimmed.is_ascii() {
        return Err(PolicyRuleError::NonAscii {
            entry: trimmed.to_string(),
        });
    }
    if trimmed == "*." {
        return Err(PolicyRuleError::Malformed {
            entry: trimmed.to_ascii_lowercase(),
            reason: "wildcard must be followed by a domain",
        });
    }

    let without_dot = trimmed.strip_suffix('.').unwrap_or(trimmed);
    let lowered = without_dot.to_ascii_lowercase();
    if lowered.parse::<std::net::IpAddr>().is_ok() || lowered.starts_with('[') {
        return Err(PolicyRuleError::IpLiteral { entry: lowered });
    }
    if lowered == "*" {
        return Ok(Some(lowered));
    }

    let domain = lowered.strip_prefix("*.").unwrap_or(&lowered);
    if domain.contains('*') {
        return Err(PolicyRuleError::Malformed {
            entry: lowered,
            reason: "wildcards are only valid as * or a leading *.",
        });
    }
    if lowered.len() > 253 {
        return Err(PolicyRuleError::Malformed {
            entry: lowered,
            reason: "domain pattern exceeds 253 bytes",
        });
    }
    for label in domain.split('.') {
        if label.is_empty() {
            return Err(PolicyRuleError::Malformed {
                entry: lowered,
                reason: "domain contains an empty label",
            });
        }
        if label.len() > 63 {
            return Err(PolicyRuleError::Malformed {
                entry: lowered,
                reason: "label exceeds 63 bytes",
            });
        }
        if label.starts_with('-') || label.ends_with('-') {
            return Err(PolicyRuleError::Malformed {
                entry: lowered,
                reason: "labels may not start or end with a hyphen",
            });
        }
        if !label.bytes().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-' || byte == b'_'
        }) {
            return Err(PolicyRuleError::Malformed {
                entry: lowered,
                reason: "labels may contain only ASCII letters, digits, hyphens and underscores",
            });
        }
    }
    if domain
        .rsplit('.')
        .next()
        .is_some_and(|label| label.bytes().all(|byte| byte.is_ascii_digit()))
    {
        return Err(PolicyRuleError::Malformed {
            entry: lowered,
            reason: "final domain label may not be numeric",
        });
    }
    Ok(Some(lowered))
}

/// Lower thegn's `network_allow`/`network_block` domain lists to a provider
/// network policy. **Pure** (unit-tested). Order encodes the precedence:
/// deny rules first (a block beats an allow), then allow rules, then a trailing
/// default-`deny *` **only when an allow-list is present** (an allow-list means
/// "deny everything else") — exactly the host-pattern semantics
/// ([`thegn_core::egress::pattern::HostRules`]). Empty + empty ⇒ no rules (allow all).
pub fn rules_from(
    allow: &[String],
    block: &[String],
) -> std::result::Result<Vec<PolicyRule>, PolicyRuleError> {
    let mut rules = Vec::new();
    let mut denied = std::collections::HashSet::new();
    for d in block {
        if let Some(domain) = canonical_domain(d)?
            && denied.insert(domain.clone())
        {
            rules.push(PolicyRule {
                domain,
                action: "deny".into(),
            });
        }
    }
    let mut any_allow = false;
    let mut allowed = std::collections::HashSet::new();
    for d in allow {
        let Some(domain) = canonical_domain(d)? else {
            continue;
        };
        // Any allow entry selects allow-list mode, even when every entry is
        // shadowed by a block: dropping the catch-all deny would fail open.
        any_allow = true;
        if !denied.contains(&domain) && allowed.insert(domain.clone()) {
            rules.push(PolicyRule {
                domain,
                action: "allow".into(),
            });
        }
    }
    if any_allow {
        rules.push(PolicyRule {
            domain: "*".into(),
            action: "deny".into(),
        });
    }
    Ok(rules)
}

pub(crate) fn parse_policy_rules(body: &serde_json::Value) -> Result<Vec<PolicyRule>> {
    let rules = body
        .get("rules")
        .and_then(serde_json::Value::as_array)
        .ok_or_else(|| anyhow!("sprites GET /policy/network: response has no rules array"))?;
    rules
        .iter()
        .enumerate()
        .map(|(index, value)| {
            let object = value.as_object().ok_or_else(|| {
                anyhow!("sprites GET /policy/network: rule {index} is not an object")
            })?;
            let domain = object
                .get("domain")
                .and_then(serde_json::Value::as_str)
                .ok_or_else(|| {
                    anyhow!("sprites GET /policy/network: rule {index} has no string domain")
                })?;
            let action = object
                .get("action")
                .and_then(serde_json::Value::as_str)
                .ok_or_else(|| {
                    anyhow!("sprites GET /policy/network: rule {index} has no string action")
                })?;
            if action != "allow" && action != "deny" {
                return Err(anyhow!(
                    "sprites GET /policy/network: rule {index} has unknown action {action:?}"
                ));
            }
            Ok(PolicyRule {
                domain: domain.to_string(),
                action: action.to_string(),
            })
        })
        .collect()
}

/// Egress-policy translation — realizes `EgressKind::Translate`
/// (`thegn_core::capabilities`): lower allow/block lists to the provider's own
/// network controls; thegn has no in-box egress datapath.
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
        let name = checked_sprite_name(name)?;
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
            let rules = rules_from(allow, block)?;
            let _permit = CONTROL_GATE.acquire(&self.authority()?).await?;
            let body = serde_json::json!({ "rules": rules });
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
            parse_policy_rules(&body)
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
        let name = checked_sprite_name(name)?;
        Ok(format!("{}/sprites/{name}/checkpoints", self.base()?))
    }
    /// Create endpoint (SINGULAR): `…/checkpoint` — returns an NDJSON stream.
    fn checkpoint_create_url(&self, name: &str) -> Result<String> {
        let name = checked_sprite_name(name)?;
        Ok(format!("{}/sprites/{name}/checkpoint", self.base()?))
    }
    fn restore_url(&self, name: &str, cp: &str) -> Result<String> {
        let name = checked_sprite_name(name)?;
        let cp = checked_sprite_name(cp)?;
        Ok(format!(
            "{}/sprites/{name}/checkpoints/{cp}/restore",
            self.base()?
        ))
    }
    fn checkpoint_url(&self, name: &str, cp: &str) -> Result<String> {
        let name = checked_sprite_name(name)?;
        let cp = checked_sprite_name(cp)?;
        Ok(format!("{}/sprites/{name}/checkpoints/{cp}", self.base()?))
    }
    /// Parse a checkpoint list element (`{ "id", "comment"? }`).
    fn parse_checkpoint(v: &serde_json::Value) -> Option<CheckpointInfo> {
        let id = v
            .get("id")
            .and_then(|i| i.as_str())
            .filter(|id| !id.is_empty())?
            .to_string();
        let label = v
            .get("comment")
            .or_else(|| v.get("label"))
            .and_then(|c| c.as_str())
            .filter(|s| !s.is_empty())
            .map(str::to_string);
        Some(CheckpointInfo { id, label })
    }

    /// Parse one checkpoint list page (an array, or `{ "checkpoints": [...] }`).
    fn parse_checkpoint_page(v: &serde_json::Value) -> Result<CheckpointPage> {
        let next = Self::continuation(v, "sprites: GET /checkpoints")?;
        let arr = v
            .as_array()
            .or_else(|| v.get("checkpoints").and_then(serde_json::Value::as_array))
            .ok_or_else(|| {
                anyhow!("sprites: GET /checkpoints returned an unrecognised list envelope")
            })?;
        let items = arr
            .iter()
            .enumerate()
            .map(|(index, checkpoint)| {
                Self::parse_checkpoint(checkpoint).ok_or_else(|| {
                    anyhow!("sprites: GET /checkpoints element {index} has no checkpoint id")
                })
            })
            .collect::<Result<Vec<_>>>()?;
        Ok(CheckpointPage { items, next })
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

    /// The outcome of a Sprites NDJSON operation stream (POST /checkpoint,
    /// POST …/restore): `Err(text)` for the first `{"type":"error"}` event
    /// (its `error` field, control characters dropped, clipped to 200), else
    /// `Ok(true)` when a `{"type":"complete"}` event arrived, `Ok(false)` when
    /// none did. Unparseable lines are skipped.
    fn stream_outcome(body: &str) -> std::result::Result<bool, String> {
        let mut complete = false;
        for line in body.lines() {
            let Ok(value) = serde_json::from_str::<serde_json::Value>(line.trim()) else {
                continue;
            };
            match value.get("type").and_then(serde_json::Value::as_str) {
                Some("error") => {
                    let error = value
                        .get("error")
                        .and_then(serde_json::Value::as_str)
                        .unwrap_or("")
                        .chars()
                        .filter(|character| !character.is_control())
                        .take(200)
                        .collect();
                    return Err(error);
                }
                Some("complete") => complete = true,
                _ => {}
            }
        }
        Ok(complete)
    }
}

impl ProviderCheckpoints for SpritesProvider {
    fn checkpoint<'a>(
        &'a self,
        id: &'a str,
        label: Option<&'a str>,
    ) -> BoxFuture<'a, Result<String>> {
        Box::pin(async move {
            self.prove_listed_sprite(id).await?;
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
            match Self::stream_outcome(&text) {
                Err(error) => return Err(anyhow!("sprites checkpoint failed: {error}")),
                Ok(false) => {
                    return Err(anyhow!(
                        "sprites checkpoint stream ended without a completion event"
                    ));
                }
                Ok(true) => {}
            }
            Self::parse_checkpoint_stream(&text)
                .ok_or_else(|| anyhow!("sprites checkpoint response did not contain an id"))
        })
    }

    fn list_checkpoints<'a>(&'a self, id: &'a str) -> BoxFuture<'a, Result<Vec<CheckpointInfo>>> {
        Box::pin(async move {
            let _permit = CONTROL_GATE.acquire(&self.authority()?).await?;
            let listing = async {
                let mut checkpoints = Vec::new();
                let mut seen_ids = std::collections::HashSet::new();
                let mut seen_tokens = std::collections::HashSet::new();
                let mut token: Option<String> = None;
                let mut pages: u32 = 0;
                loop {
                    if pages >= crate::vps::MAX_INVENTORY_PAGES {
                        return Err(anyhow!(
                            "sprites checkpoint list exceeded the page limit; refusing a partial list"
                        ));
                    }
                    pages += 1;
                    let url = self.checkpoints_list_url(id, token.as_deref())?;
                    let body = self.get_list_page(&url, "GET /checkpoints").await?;
                    let page = Self::parse_checkpoint_page(&body)?;
                    if checkpoints.len().saturating_add(page.items.len())
                        > crate::vps::MAX_INVENTORY_ROWS
                    {
                        return Err(anyhow!(
                            "sprites checkpoint list exceeded the row limit; refusing a partial list"
                        ));
                    }
                    for checkpoint in page.items {
                        if !seen_ids.insert(checkpoint.id.clone()) {
                            return Err(anyhow!(
                                "sprites checkpoint list returned duplicate checkpoint id {} (the continuation token may have been ignored)",
                                checkpoint.id
                            ));
                        }
                        checkpoints.push(checkpoint);
                    }
                    let Some(next) = page.next else {
                        return Ok(checkpoints);
                    };
                    if !seen_tokens.insert(next.clone()) {
                        return Err(anyhow!(
                            "sprites checkpoint list returned a repeated continuation token"
                        ));
                    }
                    token = Some(next);
                }
            };
            tokio::time::timeout(crate::vps::INVENTORY_BUDGET, listing)
                .await
                .map_err(|_| {
                    anyhow!(
                        "sprites checkpoint list exceeded the {} second budget; refusing a partial list",
                        crate::vps::INVENTORY_BUDGET.as_secs()
                    )
                })?
        })
    }

    fn restore<'a>(&'a self, id: &'a str, checkpoint: &'a str) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            self.prove_listed_sprite(id).await?;
            let _permit = CONTROL_GATE.acquire(&self.authority()?).await?;
            let checkpoint_resp = self
                .client
                .get(self.checkpoint_url(id, checkpoint)?)
                .bearer_auth(&self.token)
                .timeout(CONTROL_TIMEOUT)
                .send()
                .await
                .context("sprites: GET /checkpoints/{id}")?;
            let checkpoint_status = checkpoint_resp.status();
            if checkpoint_status == reqwest::StatusCode::NOT_FOUND {
                return Err(anyhow!(
                    "sprites: no checkpoint {checkpoint} on {id}; not restoring"
                ));
            }
            if checkpoint_status.is_success() {
                let body =
                    response_text(checkpoint_resp, "sprites", "GET /checkpoints/{id}").await?;
                let body: serde_json::Value = serde_json::from_str(&body)
                    .context("sprites: decode GET /checkpoints/{id} response JSON")?;
                if let Some(health) = body
                    .get("health")
                    .and_then(serde_json::Value::as_str)
                    .filter(|health| !health.is_empty())
                {
                    let health: String = health
                        .chars()
                        .filter(|character| !character.is_control())
                        .take(64)
                        .collect();
                    return Err(anyhow!(
                        "sprites: checkpoint {checkpoint} is unhealthy ({health}); not restoring it"
                    ));
                }
            } else {
                return Err(anyhow!(
                    "sprites: checkpoint check failed ({checkpoint_status})"
                ));
            }
            let resp = self
                .client
                .post(self.restore_url(id, checkpoint)?)
                .bearer_auth(&self.token)
                .timeout(CONTROL_TIMEOUT)
                .send()
                .await
                .context("sprites: POST /checkpoints/{id}/restore")?;
            if resp.status().is_success() {
                let text = response_text(resp, "sprites", "POST /checkpoints/{id}/restore").await?;
                match Self::stream_outcome(&text) {
                    Err(error) => Err(anyhow!("sprites restore failed: {error}")),
                    Ok(false) => Err(anyhow!(
                        "sprites restore of {checkpoint} ended without a completion event; the sprite's state is unknown"
                    )),
                    Ok(true) => Ok(()),
                }
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

/// SHA-256 of each file exactly as the sync upload sent it, keyed by its
/// `/`-separated path relative to the synced root.
pub type SyncBaseline = std::collections::BTreeMap<String, [u8; 32]>;

/// A close-time sync found files changed on both sides of the upload baseline.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SyncConflict {
    pub paths: Vec<String>,
}

impl std::fmt::Display for SyncConflict {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let first_ten = self
            .paths
            .iter()
            .take(10)
            .cloned()
            .collect::<Vec<_>>()
            .join(", ");
        let more = self.paths.len().saturating_sub(10);
        let suffix = if more > 0 {
            format!(", and {more} more")
        } else {
            String::new()
        };
        write!(
            formatter,
            "provider sync refused: {} file(s) changed both locally and in the sandbox since the upload: {first_ten}{suffix}; nothing was written locally and the sandbox copy is kept; make each file match one copy (or restore it) and close again",
            self.paths.len()
        )
    }
}

impl std::error::Error for SyncConflict {}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ReconcileAction {
    Skip,
    Publish,
    Conflict,
}

fn reconcile_action(
    base: Option<&[u8; 32]>,
    local: Option<&[u8; 32]>,
    remote: &[u8; 32],
) -> ReconcileAction {
    if base == Some(remote) || local == Some(remote) {
        ReconcileAction::Skip
    } else if (base.is_some() && local == base) || (base.is_none() && local.is_none()) {
        ReconcileAction::Publish
    } else {
        ReconcileAction::Conflict
    }
}

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

    /// Write `data` readable by its owner only (mode 0600 where the provider's
    /// file API supports a mode): agent login files (OAuth tokens). Providers
    /// whose file API takes no mode keep their default; the dir is the user's
    /// own VM.
    fn write_private<'a>(
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
        Box::pin(async move { upload_tree(self, id, local, remote).await.map(|_| ()) })
    }

    /// Recursively download the sandbox `remote` dir into `local`.
    fn download_dir<'a>(
        &'a self,
        id: &'a str,
        remote: &'a str,
        local: &'a Path,
    ) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            let root: thegn_core::fsperm::RootedDir = thegn_core::fsperm::open_rooted_dir(local)
                .with_context(|| {
                    format!(
                        "download destination is not a real directory: {}",
                        local.display()
                    )
                })?;
            // Children are opened only when popped, so the live handles are the current ancestry.
            let mut stack: Vec<(String, Arc<thegn_core::fsperm::RootedDir>, Option<String>)> =
                vec![(String::new(), Arc::new(root), None)];
            let mut seen = std::collections::HashSet::new();
            let mut file_count = 0usize;
            let mut total_bytes = 0u64;
            while let Some((rel, parent, name)) = stack.pop() {
                let dir: Arc<thegn_core::fsperm::RootedDir> = match name {
                    Some(name) => Arc::new(parent.ensure_child_dir(&name).with_context(|| {
                        format!(
                            "open download destination directory {}",
                            local.join(&rel).display()
                        )
                    })?),
                    None => parent,
                };
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
                    if e.is_dir {
                        stack.push((child, Arc::clone(&dir), Some(e.name)));
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
                        dir.publish_file(&e.name, &data).with_context(|| {
                            format!(
                                "publish download destination {}",
                                local.join(&child).display()
                            )
                        })?;
                    }
                }
            }
            Ok(())
        })
    }
}

/// Upload the selected local files and retain the exact bytes' digests only
/// after each corresponding provider write succeeds.
pub(crate) async fn upload_tree<F: ProviderFiles + ?Sized>(
    files: &F,
    id: &str,
    local: &Path,
    remote: &str,
) -> Result<SyncBaseline> {
    use sha2::{Digest, Sha256};

    let mut baseline = SyncBaseline::new();
    for (abs, rel) in collect_files(local)? {
        let data = std::fs::read(&abs).with_context(|| format!("read {}", abs.display()))?;
        files.write(id, &join_remote(remote, &rel), &data).await?;
        baseline.insert(rel, Sha256::digest(&data).into());
    }
    Ok(baseline)
}

/// Reconcile a provider tree against the upload baseline without changing the
/// local tree until every remote file has been read and classified. The staged
/// publish set is bounded by `DOWNLOAD_MAX_TOTAL_BYTES`.
pub(crate) async fn download_reconciled<F: ProviderFiles + ?Sized>(
    files: &F,
    id: &str,
    remote: &str,
    local: &Path,
    baseline: &SyncBaseline,
) -> Result<()> {
    use sha2::{Digest, Sha256};

    let root: Arc<thegn_core::fsperm::RootedDir> =
        Arc::new(thegn_core::fsperm::open_rooted_dir(local).with_context(|| {
            format!(
                "download destination is not a real directory: {}",
                local.display()
            )
        })?);
    let mut stack: Vec<(
        String,
        Option<Arc<thegn_core::fsperm::RootedDir>>,
        Option<String>,
    )> = vec![(String::new(), Some(root.clone()), None)];
    let mut seen = std::collections::HashSet::new();
    let mut file_count = 0usize;
    let mut total_bytes = 0u64;
    let mut publishes: Vec<(String, Vec<u8>)> = Vec::new();
    let mut conflicts = Vec::new();
    let mut ignore = LocalIgnore::new(local)?;

    while let Some((rel, parent, name)) = stack.pop() {
        let dir = match (parent, name) {
            (Some(parent), Some(name)) => parent
                .open_child_dir(&name)
                .with_context(|| {
                    format!(
                        "open existing comparison directory {}",
                        local.join(&rel).display()
                    )
                })?
                .map(Arc::new),
            (parent, None) => parent,
            (None, Some(_)) => None,
        };
        let depth = if rel.is_empty() {
            0
        } else {
            rel.split('/').count()
        };
        anyhow::ensure!(
            depth <= DOWNLOAD_MAX_DEPTH,
            "provider listing exceeded maximum directory depth"
        );
        let listing = files.list_dir(id, &join_remote(remote, &rel)).await?;
        let mut siblings = std::collections::HashSet::new();
        for entry in &listing {
            if entry.name == ".git" {
                continue;
            }
            anyhow::ensure!(
                valid_remote_component(&entry.name),
                "provider returned an unsafe path component"
            );
            anyhow::ensure!(
                siblings.insert(entry.name.clone()),
                "provider returned a duplicate directory entry"
            );
            let child = if rel.is_empty() {
                entry.name.clone()
            } else {
                format!("{rel}/{}", entry.name)
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
        for entry in listing {
            if entry.name == ".git" {
                continue;
            }
            let child = if rel.is_empty() {
                entry.name.clone()
            } else {
                format!("{rel}/{}", entry.name)
            };
            if entry.is_dir {
                if ignore.is_ignored(&child, true)? {
                    continue;
                }
                stack.push((child, dir.clone(), Some(entry.name)));
                continue;
            }
            if ignore.is_ignored(&child, false)? {
                continue;
            }
            file_count += 1;
            anyhow::ensure!(
                file_count <= DOWNLOAD_MAX_FILES,
                "provider listing exceeded maximum file count"
            );
            let data = files.read(id, &join_remote(remote, &child)).await?;
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
            if actual_size != entry.size {
                tracing::warn!(target: "thegn::provider", path = %child,
                    listed_size = entry.size, actual_size, "provider file changed between listing and read; entry skipped");
                continue;
            }
            let remote_digest: [u8; 32] = Sha256::digest(&data).into();
            let local_data = match &dir {
                Some(dir) => dir
                    .read_regular(&entry.name, DOWNLOAD_MAX_FILE_BYTES)
                    .with_context(|| {
                        format!(
                            "read local comparison file {}",
                            local.join(&child).display()
                        )
                    })?,
                None => None,
            };
            let local_digest = local_data
                .as_ref()
                .map(|bytes| <[u8; 32]>::from(Sha256::digest(bytes)));
            match reconcile_action(baseline.get(&child), local_digest.as_ref(), &remote_digest) {
                ReconcileAction::Skip => {}
                ReconcileAction::Publish => publishes.push((child, data)),
                ReconcileAction::Conflict => conflicts.push(child),
            }
        }
    }
    if !conflicts.is_empty() {
        conflicts.sort();
        return Err(SyncConflict { paths: conflicts }.into());
    }

    publishes.sort_by(|left, right| left.0.cmp(&right.0));
    let mut ancestry = vec![(String::new(), root)];
    for (child, data) in publishes {
        let (parent_rel, name) = child.rsplit_once('/').unwrap_or(("", child.as_str()));
        let components: Vec<&str> = parent_rel
            .split('/')
            .filter(|part| !part.is_empty())
            .collect();
        let mut shared: usize = 0;
        while shared < components.len()
            && shared < ancestry.len().saturating_sub(1)
            && ancestry[shared + 1].0.rsplit('/').next() == Some(components[shared])
        {
            shared += 1;
        }
        ancestry.truncate(shared + 1);
        for component in components.iter().skip(shared) {
            let current_rel = ancestry.last().expect("root directory retained").0.clone();
            let next_rel = if current_rel.is_empty() {
                (*component).to_string()
            } else {
                format!("{current_rel}/{component}")
            };
            let parent = ancestry
                .last()
                .expect("parent directory retained")
                .1
                .clone();
            let opened = parent.ensure_child_dir(component).with_context(|| {
                format!(
                    "open download destination directory {}",
                    local.join(&next_rel).display()
                )
            })?;
            ancestry.push((next_rel, Arc::new(opened)));
        }
        ancestry
            .last()
            .expect("publish directory retained")
            .1
            .publish_file(name, &data)
            .with_context(|| {
                format!(
                    "publish download destination {}",
                    local.join(&child).display()
                )
            })?;
    }
    Ok(())
}

/// Local ignore rules for close-time reconciliation, mirroring the upload walk:
/// each directory's `.ignore` and `.gitignore` apply below it, deeper
/// directories win, and `.ignore` wins over `.gitignore` within one directory.
/// Per-directory rules take precedence over `.git/info/exclude`, which takes
/// precedence over the global excludes file.
struct LocalIgnore {
    root: std::path::PathBuf,
    dirs: HashMap<String, Vec<ignore::gitignore::Gitignore>>,
    exclude: Option<ignore::gitignore::Gitignore>,
    global: ignore::gitignore::Gitignore,
}

fn handle_ignore_error(path: &Path, error: ignore::Error) -> Result<()> {
    if error.is_io() {
        return Err(anyhow::Error::msg(error.to_string()))
            .with_context(|| format!("read {}", path.display()));
    }
    tracing::warn!(
        target: "thegn::provider",
        path = %path.display(),
        error = %error,
        "ignore file contains invalid rules; applying parsed rules"
    );
    Ok(())
}

impl LocalIgnore {
    fn new(root: &Path) -> Result<Self> {
        let (global, error) = ignore::gitignore::GitignoreBuilder::new(root).build_global();
        if let Some(error) = error {
            let path =
                ignore::gitignore::gitconfig_excludes_path().unwrap_or_else(|| root.to_path_buf());
            handle_ignore_error(&path, error)?;
        }
        Self::with_global(root, global)
    }

    fn with_global(root: &Path, global: ignore::gitignore::Gitignore) -> Result<Self> {
        let exclude_path = thegn_core::util::git_common_dir(root)
            .join("info")
            .join("exclude");
        let exclude = if std::fs::metadata(&exclude_path).is_ok_and(|metadata| metadata.is_file()) {
            let mut builder = ignore::gitignore::GitignoreBuilder::new(root);
            if let Some(error) = builder.add(&exclude_path) {
                handle_ignore_error(&exclude_path, error)?;
            }
            Some(
                builder
                    .build()
                    .map_err(|error| anyhow::Error::msg(error.to_string()))
                    .with_context(|| format!("read {}", exclude_path.display()))?,
            )
        } else {
            None
        };
        Ok(Self {
            root: root.to_path_buf(),
            dirs: HashMap::new(),
            exclude,
            global,
        })
    }

    /// Matchers for one directory (`""` means the worktree root), loaded on
    /// first use in `.ignore` then `.gitignore` order.
    fn matchers(&mut self, dir: &str) -> Result<&[ignore::gitignore::Gitignore]> {
        if !self.dirs.contains_key(dir) {
            let directory = self.root.join(dir);
            let mut matchers = Vec::new();
            for name in [".ignore", ".gitignore"] {
                let path = directory.join(name);
                if std::fs::metadata(&path).is_ok_and(|metadata| metadata.is_file()) {
                    let (matcher, error) = ignore::gitignore::Gitignore::new(&path);
                    if let Some(error) = error {
                        handle_ignore_error(&path, error)?;
                    }
                    matchers.push(matcher);
                }
            }
            self.dirs.insert(dir.to_string(), matchers);
        }
        Ok(self.dirs.get(dir).expect("matcher entry inserted"))
    }

    /// Whether a slash-separated path beneath the worktree is excluded.
    fn is_ignored(&mut self, rel: &str, is_dir: bool) -> Result<bool> {
        let candidate = self.root.join(rel);
        let components: Vec<&str> = rel.split('/').collect();
        let mut dirs = Vec::with_capacity(components.len());
        for end in (0..components.len()).rev() {
            dirs.push(components[..end].join("/"));
        }
        for dir in dirs {
            for matcher in self.matchers(&dir)? {
                let matched = matcher.matched_path_or_any_parents(&candidate, is_dir);
                if matched.is_ignore() {
                    return Ok(true);
                }
                if matched.is_whitelist() {
                    return Ok(false);
                }
            }
        }
        for matcher in self.exclude.iter().chain(std::iter::once(&self.global)) {
            let matched = matcher.matched_path_or_any_parents(&candidate, is_dir);
            if matched.is_ignore() {
                return Ok(true);
            }
            if matched.is_whitelist() {
                return Ok(false);
            }
        }
        Ok(false)
    }
}

impl SpritesProvider {
    /// The fs operation endpoint: `…/sprites/{name}/fs/{op}` (op = read|write|list
    /// |delete). The target path is a **query param** (`?path=…&workingDir=/`),
    /// not part of the URL — matching the live v1 API (and the `sprites-py` SDK).
    fn fs_op_url(&self, name: &str, op: &str) -> Result<String> {
        let name = checked_sprite_name(name)?;
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
    /// uses `0644`; `write_exec` uses `0755` so a pushed binary is runnable;
    /// `write_private` uses `0600` for login files.
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
                // The published parameter (openapi PUT /v1/sprites/{name}/fs/write).
                ("mkdir", "true"),
                // Kept: the live API created nested parents with it (agent_configs.rs:1702).
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

    fn write_private<'a>(
        &'a self,
        id: &'a str,
        path: &'a str,
        data: &'a [u8],
    ) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move { self.write_with_mode(id, path, data, "0600").await })
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
                // UNVERIFIED which form the server honours; send both the live
                // query parameters and the documented JSON body.
                .query(&[("path", path), ("workingDir", "/"), ("recursive", "true")])
                .json(&serde_json::json!({
                    "path": path,
                    "workingDir": "/",
                    "recursive": true,
                    "asRoot": false,
                }))
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

impl ProviderCaps {
    /// Static caps of a provider kind: the one source `Provider::caps()` reads.
    pub fn for_kind(kind: thegn_core::config::EnvProviderKind) -> ProviderCaps {
        ProviderCaps {
            files: kind.files(),
            checkpoints: kind.checkpoints(),
            egress: kind.egress_api(),
            exec_api: kind.exec_api(),
            scale_to_zero: kind.scale_to_zero(),
        }
    }
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

const INSTALL_LEASE_TTL_MINUTES: &str = "5";
const INSTALL_LEASE_ACQUIRE_SCRIPT: &str = r#"
if mkdir -m 0700 -- "$1" 2>/dev/null; then
    if printf '%s\n' "$2" > "$1/owner"; then
        exit 0
    fi
    rmdir -- "$1" 2>/dev/null || true
    exit 1
fi
if [ -d "$1" ] && [ "$(find "$1" -maxdepth 0 -mmin +$3 -print 2>/dev/null)" = "$1" ]; then
    if mkdir -- "$1.reclaim" 2>/dev/null; then
        if [ -d "$1" ] && [ "$(find "$1" -maxdepth 0 -mmin +"$3" -print 2>/dev/null)" = "$1" ]; then
            rm -rf -- "$1"
            if mkdir -m 0700 -- "$1" 2>/dev/null && printf '%s\n' "$2" > "$1/owner"; then
                rmdir -- "$1.reclaim"
                exit 0
            fi
        fi
        rmdir -- "$1.reclaim"
    fi
fi
exit 75
"#;
const INSTALL_LEASE_RELEASE_SCRIPT: &str = r#"
if [ -f "$1/owner" ] && [ "$(cat "$1/owner" 2>/dev/null)" = "$2" ]; then
    rm -f -- "$1/owner" || exit 1
    rmdir -- "$1" || exit 1
fi
exit 0
"#;
const INSTALL_PRESERVE_SCRIPT: &str = r#"
rm -f -- "$2" || exit 1
if [ -e "$1" ]; then
    ln -- "$1" "$2" 2>/dev/null || cp -p -- "$1" "$2" || exit 1
fi
"#;
const INSTALL_RESTORE_SCRIPT: &str = r#"
if [ -e "$2" ]; then
    mv -f -- "$2" "$1"
else
    rm -f -- "$1"
fi
"#;
const INSTALL_REMOVE_PREVIOUS_SCRIPT: &str = r#"
rm -f -- "$1"
"#;

fn install_shell_argv(name: &str, script: &str, args: &[&str]) -> Vec<String> {
    let mut argv = vec!["/bin/sh".to_string(), "-c".to_string(), script.to_string()];
    argv.push(name.to_string());
    argv.extend(args.iter().map(|arg| (*arg).to_string()));
    argv
}

#[derive(Debug)]
enum InstallError {
    Busy,
    Lease(anyhow::Error),
    Previous(anyhow::Error),
    StageWrite(anyhow::Error),
    StageRead(anyhow::Error),
    StageVerification(String),
    Publish(anyhow::Error),
    PublishedVerification(String),
}

impl std::fmt::Display for InstallError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Busy => write!(f, "bridge executable install is busy"),
            Self::Lease(error) => write!(f, "lease bridge executable install: {error:#}"),
            Self::Previous(error) => write!(f, "preserve previous bridge executable: {error:#}"),
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

impl std::error::Error for InstallError {}

async fn delete_install_stage(files: &dyn ProviderFiles, id: &str, stage: &str) {
    if let Err(error) = files.delete(id, stage).await {
        tracing::warn!(target: "thegn::provider", %error, "staged executable cleanup failed");
    }
}

async fn release_install_lease<R, Fut>(release: &mut Option<R>)
where
    R: FnOnce() -> Fut,
    Fut: std::future::Future<Output = Result<()>>,
{
    if let Some(release) = release.take()
        && let Err(error) = release().await
    {
        tracing::warn!(target: "thegn::provider", %error, "executable install lease release failed");
    }
}

#[allow(clippy::too_many_arguments)] // each install step is an injected seam for the in-memory tests
async fn install_executable_verified<A, AFut, R, RFut, B, BFut, P, PFut, T, TFut, C, CFut>(
    files: &dyn ProviderFiles,
    id: &str,
    path: &str,
    data: &[u8],
    acquire: A,
    release: R,
    preserve: B,
    publish: P,
    rollback: T,
    remove_previous: C,
) -> Result<bool>
where
    A: FnOnce() -> AFut,
    AFut: std::future::Future<Output = Result<bool>>,
    R: FnOnce() -> RFut,
    RFut: std::future::Future<Output = Result<()>>,
    B: FnOnce() -> BFut,
    BFut: std::future::Future<Output = Result<()>>,
    P: FnOnce(String, String) -> PFut,
    PFut: std::future::Future<Output = Result<()>>,
    T: FnOnce() -> TFut,
    TFut: std::future::Future<Output = Result<()>>,
    C: FnOnce() -> CFut,
    CFut: std::future::Future<Output = Result<()>>,
{
    let want = sha256_hex(data);
    if let Ok(remote) = files.read(id, path).await
        && remote.len() == data.len()
        && sha256_hex(&remote) == want
    {
        return Ok(false);
    }

    let mut release = Some(release);
    match acquire().await {
        Ok(true) => {}
        Ok(false) => return Err(InstallError::Busy.into()),
        Err(error) => return Err(InstallError::Lease(error).into()),
    }

    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |duration| duration.as_nanos());
    let stage = format!("{path}.tg-stage-{}-{nanos}", std::process::id());
    if let Err(error) = files.write_exec(id, &stage, data).await {
        delete_install_stage(files, id, &stage).await;
        release_install_lease(&mut release).await;
        return Err(InstallError::StageWrite(error).into());
    }

    match files.read(id, &stage).await {
        Ok(bytes) if bytes.len() == data.len() && sha256_hex(&bytes) == want => {}
        Ok(bytes) => {
            delete_install_stage(files, id, &stage).await;
            release_install_lease(&mut release).await;
            return Err(InstallError::StageVerification(format!(
                "expected {} bytes with SHA-256 {want}, got {} bytes with SHA-256 {}",
                data.len(),
                bytes.len(),
                sha256_hex(&bytes)
            ))
            .into());
        }
        Err(error) => {
            delete_install_stage(files, id, &stage).await;
            release_install_lease(&mut release).await;
            return Err(InstallError::StageRead(error).into());
        }
    };

    if let Err(error) = preserve().await {
        delete_install_stage(files, id, &stage).await;
        release_install_lease(&mut release).await;
        return Err(InstallError::Previous(error).into());
    }

    if let Err(error) = publish(stage.clone(), path.to_string()).await {
        if let Err(rollback_error) = rollback().await {
            tracing::warn!(target: "thegn::provider", %rollback_error, "bridge executable rollback after publish failure failed");
        }
        delete_install_stage(files, id, &stage).await;
        release_install_lease(&mut release).await;
        return Err(InstallError::Publish(error).into());
    }

    match files.read(id, path).await {
        Ok(bytes) if bytes.len() == data.len() && sha256_hex(&bytes) == want => {}
        Ok(bytes) => {
            if let Err(error) = rollback().await {
                tracing::warn!(target: "thegn::provider", %error, "bridge executable rollback after verification failure failed");
            }
            delete_install_stage(files, id, &stage).await;
            release_install_lease(&mut release).await;
            return Err(InstallError::PublishedVerification(format!(
                "expected {} bytes with SHA-256 {want}, got {} bytes with SHA-256 {}",
                data.len(),
                bytes.len(),
                sha256_hex(&bytes)
            ))
            .into());
        }
        Err(error) => {
            if let Err(rollback_error) = rollback().await {
                tracing::warn!(target: "thegn::provider", %rollback_error, "bridge executable rollback after read-back failure failed");
            }
            delete_install_stage(files, id, &stage).await;
            release_install_lease(&mut release).await;
            return Err(InstallError::PublishedVerification(format!(
                "read-back failed: {error:#}"
            ))
            .into());
        }
    };

    if let Err(error) = remove_previous().await {
        tracing::warn!(target: "thegn::provider", %error, "previous executable generation cleanup failed");
    }
    let fp_path = format!("{path}.fp");
    if let Err(error) = files.delete(id, &fp_path).await {
        tracing::debug!(target: "thegn::provider", %error, "best-effort legacy executable fingerprint cleanup failed");
    }
    release_install_lease(&mut release).await;
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

/// A Sprites exec WebSocket message that breaks the documented frame
/// protocol (AsyncAPI exec, THE-533); unknown well-formed control types are tolerated and ignored.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExecProtocolError {
    EmptyFrame,
    UnknownStream(u8),
    MalformedExit,
    /// The server repeats the exit in binary and JSON forms; a repeat with the
    /// same code is accepted, but a different code is a protocol error.
    DuplicateExit,
    OutputAfterExit,
    MalformedControl,
}

impl std::fmt::Display for ExecProtocolError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let message = match self {
            Self::EmptyFrame => "a binary frame has no stream id".to_string(),
            Self::UnknownStream(id) => format!("stream id {id} is not a server stream"),
            Self::MalformedExit => "the exit frame is malformed".to_string(),
            Self::DuplicateExit => "a second exit arrived with a different code".to_string(),
            Self::OutputAfterExit => "output arrived after the exit".to_string(),
            Self::MalformedControl => "a control message is malformed".to_string(),
        };
        write!(f, "sprites exec: protocol error: {message}")
    }
}

impl std::error::Error for ExecProtocolError {}

/// Exit status used when a provider reports an absent or invalid exit code.
/// Matches the host daemon adapter's unknown-exit sentinel.
pub const EXEC_EXIT_STATUS_UNKNOWN: i32 = 254;

/// Bounded output collected from one-shot exec frames.
pub(crate) struct ExecCollected {
    pub(crate) code: Option<i32>,
    pub(crate) output: Vec<u8>,
    pub(crate) dropped: u64,
}

/// Bounded tail of a non-TTY exec's stderr (stream id 2).
#[derive(Default)]
pub(crate) struct StderrTail {
    bytes: std::collections::VecDeque<u8>,
    total: u64,
}

pub(crate) const STDERR_TAIL_CAP: usize = 8 * 1024;

impl StderrTail {
    pub(crate) fn push(&mut self, chunk: &[u8]) {
        self.total = self.total.saturating_add(chunk.len() as u64);
        if chunk.len() >= STDERR_TAIL_CAP {
            self.bytes.clear();
            self.bytes.extend(&chunk[chunk.len() - STDERR_TAIL_CAP..]);
            return;
        }
        let excess = self
            .bytes
            .len()
            .saturating_add(chunk.len())
            .saturating_sub(STDERR_TAIL_CAP);
        self.bytes.drain(..excess);
        self.bytes.extend(chunk);
    }

    pub(crate) fn render(&self) -> String {
        sanitize_stderr(&self.bytes.iter().copied().collect::<Vec<_>>())
    }
}

/// Bounded tail (last `STDERR_TAIL_CAP` bytes) of a streaming exec's stderr.
pub struct ExecStderr(StderrSink);

impl ExecStderr {
    /// The sanitized tail (controls stripped, lossy UTF-8).
    pub fn render(&self) -> String {
        self.0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .render()
    }

    /// Total stderr bytes seen.
    pub fn total(&self) -> u64 {
        self.0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .total
    }
}

/// A non-tty exec whose stdout the caller streams itself (no output cap).
pub struct StreamingExec {
    pub session: ExecSession,
    pub stderr: ExecStderr,
    /// `Some(error)` when the driver ended on a protocol violation.
    pub verdict: tokio::sync::oneshot::Receiver<Option<ExecProtocolError>>,
}

type StderrSink = Arc<Mutex<StderrTail>>;
type ExecDone = tokio::sync::oneshot::Receiver<Option<ExecProtocolError>>;
type CapturedExec = (ExecSession, StderrSink, ExecDone);

/// Remove ESC and C0 controls except \n and \t; lossy UTF-8.
pub(crate) fn sanitize_stderr(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes)
        .chars()
        .filter(|c| !c.is_control() || matches!(c, '\n' | '\t'))
        .collect()
}

fn append_stderr_tail(mut output: String, code: i32, stderr: &StderrSink) -> String {
    if code == 0 {
        return output;
    }
    let tail = stderr
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if tail.bytes.is_empty() {
        return output;
    }
    let n = tail.total.min(STDERR_TAIL_CAP as u64);
    output.push_str(&format!("\n[stderr, last {n} bytes]\n{}", tail.render()));
    output
}

fn collect_exec_frame(
    frame: ExecFrame,
    cap: usize,
    output: &mut std::collections::VecDeque<u8>,
    code: &mut Option<i32>,
    dropped: &mut u64,
) -> bool {
    match frame {
        ExecFrame::Stdout(bytes) if cap == 0 => {
            *dropped = dropped.saturating_add(bytes.len() as u64);
        }
        ExecFrame::Stdout(bytes) if bytes.len() >= cap => {
            *dropped = dropped
                .saturating_add(output.len() as u64)
                .saturating_add((bytes.len() - cap) as u64);
            output.clear();
            output.extend(&bytes[bytes.len() - cap..]);
        }
        ExecFrame::Stdout(bytes) => {
            let excess = output.len().saturating_add(bytes.len()).saturating_sub(cap);
            output.drain(..excess);
            *dropped = dropped.saturating_add(excess as u64);
            output.extend(bytes);
        }
        ExecFrame::Exit(exit_code) => {
            *code = Some(exit_code);
            return true;
        }
    }
    false
}

/// Drain one-shot exec frames while retaining only the last `cap` stdout bytes.
pub(crate) async fn collect_exec_output(
    frames: &mut tokio::sync::mpsc::Receiver<ExecFrame>,
    cap: usize,
) -> ExecCollected {
    let mut output = std::collections::VecDeque::new();
    let mut code = None;
    let mut dropped = 0_u64;
    while let Some(frame) = frames.recv().await {
        if collect_exec_frame(frame, cap, &mut output, &mut code, &mut dropped) {
            break;
        }
    }
    ExecCollected {
        code,
        output: output.into_iter().collect(),
        dropped,
    }
}

/// Pure seam: collect one-shot exec output until an absolute deadline. `None`
/// means the deadline fired before the stream ended or reported an exit.
pub(crate) async fn collect_exec_output_until(
    frames: &mut tokio::sync::mpsc::Receiver<ExecFrame>,
    cap: usize,
    until: tokio::time::Instant,
) -> Option<ExecCollected> {
    let mut output = std::collections::VecDeque::new();
    let mut code = None;
    let mut dropped = 0_u64;
    loop {
        let frame = match tokio::time::timeout_at(until, frames.recv()).await {
            Ok(frame) => frame,
            Err(_) => return None,
        };
        let Some(frame) = frame else { break };
        if collect_exec_frame(frame, cap, &mut output, &mut code, &mut dropped) {
            break;
        }
    }
    Some(ExecCollected {
        code,
        output: output.into_iter().collect(),
        dropped,
    })
}

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
    /// Resolves once the driver stops; a dropped sender means it ended abnormally.
    pub end: tokio::sync::oneshot::Receiver<ProxyEnd>,
}

/// The terminal condition reported by a provider TCP proxy driver.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProxyEnd {
    LocalClosed,
    PeerClosed,
    RemoteRejected { code: u16, reason: String },
    TransportDropped,
    TransportError(String),
}

impl ProxyEnd {
    /// Convert a terminal condition into the result expected by a proxy client.
    pub fn into_result(self) -> anyhow::Result<()> {
        let message = match self {
            Self::LocalClosed | Self::PeerClosed => return Ok(()),
            Self::RemoteRejected { code, reason } => {
                let prefix = format!("provider proxy closed by the remote: code {code} (");
                let suffix = ")";
                let reason = truncate_proxy_text(reason, 256 - prefix.len() - suffix.len());
                format!("{prefix}{reason}{suffix}")
            }
            Self::TransportDropped => {
                "provider proxy connection dropped without a close frame".to_string()
            }
            Self::TransportError(error) => format!("provider proxy transport error: {error}"),
        };
        Err(anyhow::anyhow!(truncate_proxy_text(message, 256)))
    }
}

fn truncate_proxy_text(mut text: String, max_bytes: usize) -> String {
    if text.len() > max_bytes {
        let boundary = text
            .char_indices()
            .map(|(index, _)| index)
            .take_while(|index| *index <= max_bytes)
            .last()
            .unwrap_or(0);
        text.truncate(boundary);
    }
    text
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
pub(crate) const EXEC_WS_MAX_MESSAGE_SIZE: usize = 16 << 20;
pub(crate) const EXEC_WS_MAX_FRAME_SIZE: usize = 4 << 20;
pub(crate) const EXEC_WS_MAX_WRITE_BUFFER_SIZE: usize = 8 << 20;
/// Maximum provider output retained by one session while its consumer is slow.
const EXEC_DATA_BYTE_BUDGET: usize = 8 << 20;
/// Largest Sprites JSON control text frame accepted (THE-533).
const EXEC_CONTROL_TEXT_MAX: usize = 64 << 10;
/// How long the exec driver keeps reading after the exit to catch a
/// duplicate exit or late output (THE-533).
const EXEC_EXIT_DRAIN: Duration = Duration::from_secs(2);
/// Input with no inbound frame this long sends a WebSocket Ping.
const EXEC_PROBE_AFTER: Duration = Duration::from_secs(5);
/// A Ping with no inbound frame this long ends the session as dropped.
const EXEC_PROBE_DEAD: Duration = Duration::from_secs(5);
/// Item capacity of the provider output channels.
const EXEC_CHANNEL_CAPACITY: usize = 256;
/// Largest provider output item queued at once; with EXEC_CHANNEL_CAPACITY
/// items this bounds retained output to EXEC_DATA_BYTE_BUDGET.
const EXEC_DATA_CHUNK: usize = 32 << 10;
const _: () = assert!(EXEC_DATA_CHUNK * EXEC_CHANNEL_CAPACITY <= EXEC_DATA_BYTE_BUDGET);
const PROXY_CONNECT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(15);

/// Byte and message counters for one exec socket, logged at debug: one line
/// per MiB received (instantaneous rate) and a summary when the driver ends.
/// Integer adds per WebSocket message in the driver task; nothing on the UI loop.
struct ExecWireStats {
    started: tokio::time::Instant,
    in_msgs: u64,
    in_bytes: u64,
    out_msgs: u64,
    out_bytes: u64,
    max_in: usize,
    mark_bytes: u64,
    mark_at: tokio::time::Instant,
    consumer_wait: Duration,
    mark_wait: Duration,
}

impl ExecWireStats {
    const MARK: u64 = 1 << 20;

    fn new(now: tokio::time::Instant) -> Self {
        Self {
            started: now,
            in_msgs: 0,
            in_bytes: 0,
            out_msgs: 0,
            out_bytes: 0,
            max_in: 0,
            mark_bytes: 0,
            mark_at: now,
            consumer_wait: Duration::ZERO,
            mark_wait: Duration::ZERO,
        }
    }

    fn waited(&mut self, duration: Duration) {
        self.consumer_wait += duration;
        self.mark_wait += duration;
    }

    /// Count one inbound message; `Some((bytes_since_mark, ms_since_mark,
    /// wait_ms_since_mark))` when a MiB boundary was crossed.
    fn inbound(&mut self, len: usize, now: tokio::time::Instant) -> Option<(u64, u64, u64)> {
        self.in_msgs += 1;
        self.in_bytes += len as u64;
        self.max_in = self.max_in.max(len);
        if self.in_bytes.saturating_sub(self.mark_bytes) < Self::MARK {
            return None;
        }
        let bytes = self.in_bytes - self.mark_bytes;
        let ms = now.duration_since(self.mark_at).as_millis() as u64;
        let wait_ms = self.mark_wait.as_millis() as u64;
        self.mark_bytes = self.in_bytes;
        self.mark_at = now;
        self.mark_wait = Duration::ZERO;
        Some((bytes, ms, wait_ms))
    }

    fn outbound(&mut self, len: usize) {
        self.out_msgs += 1;
        self.out_bytes += len as u64;
    }
}

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

pub(crate) fn exec_ws_config() -> tokio_tungstenite::tungstenite::protocol::WebSocketConfig {
    tokio_tungstenite::tungstenite::protocol::WebSocketConfig::default()
        .max_message_size(Some(EXEC_WS_MAX_MESSAGE_SIZE))
        .max_frame_size(Some(EXEC_WS_MAX_FRAME_SIZE))
        .max_write_buffer_size(EXEC_WS_MAX_WRITE_BUFFER_SIZE)
}

/// Forward one provider payload as items of at most `EXEC_DATA_CHUNK` bytes.
/// `send().await` is the backpressure: a slow consumer stalls the reader
/// instead of ending the session. Returns `false` once the receiver is gone.
async fn send_chunked<T>(
    sender: &tokio::sync::mpsc::Sender<T>,
    bytes: Vec<u8>,
    wrap: impl Fn(Vec<u8>) -> T,
) -> bool {
    if bytes.len() <= EXEC_DATA_CHUNK {
        return sender.send(wrap(bytes)).await.is_ok();
    }
    for chunk in bytes.chunks(EXEC_DATA_CHUNK) {
        if sender.send(wrap(chunk.to_vec())).await.is_err() {
            return false;
        }
    }
    true
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
    checked_sprite_name(id)?;
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
    checked_sprite_name(id)?;
    let session = checked_exec_session(session)?;
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
    checked_sprite_name(id)?;
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

/// Queue the PTY size as the first control message of a tty exec. Measured
/// (batch 186 parity P03): the Sprites server left the PTY at 0x0 although the
/// open URL carried cols/rows, so an interactive transport states its size
/// explicitly after connect. A zero dimension is not a size: skip.
fn send_initial_size(session: &ExecSession, cols: u16, rows: u16) {
    if cols == 0 || rows == 0 {
        return;
    }
    if let Err(error) = session.control.try_send(ExecControl::Resize { cols, rows }) {
        tracing::debug!(
            target: "thegn::provider",
            %error,
            "initial exec resize could not be queued"
        );
    }
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

/// The non-PTY stdin EOF frame. It is sent once on local control close, before
/// the WebSocket close handshake; half-close is not implemented.
fn encode_stream_stdin_eof() -> Vec<u8> {
    vec![4u8]
}

/// A decoded non-PTY server→client binary frame.
#[derive(Debug, PartialEq, Eq)]
enum StreamMsg {
    Stdout(Vec<u8>),
    Stderr(Vec<u8>),
    Exit(i32),
}

/// Split a non-PTY server→client binary frame on its 1-byte stream id.
fn decode_stream_frame(b: &[u8]) -> std::result::Result<StreamMsg, ExecProtocolError> {
    let Some((&id, rest)) = b.split_first() else {
        return Err(ExecProtocolError::EmptyFrame);
    };
    Ok(match id {
        1 => StreamMsg::Stdout(rest.to_vec()),
        2 => StreamMsg::Stderr(rest.to_vec()),
        3 => {
            if rest.len() != 1 {
                return Err(ExecProtocolError::MalformedExit);
            }
            // AsyncAPI exec: exit code (single byte), frame prefix 0x03.
            StreamMsg::Exit(i32::from(rest[0]))
        }
        _ => return Err(ExecProtocolError::UnknownStream(id)),
    })
}

/// A parsed Sprites exec text (control) frame.
#[derive(Debug, PartialEq, Eq)]
enum SpriteCtrl {
    /// The session-info frame announced its `session_id` (for reattach).
    Session(String),
    /// The command exited with this code.
    Exit(i32),
    /// A non-JSON-object text frame that may be output in TTY mode.
    Forward,
    /// A port notification that does not affect the exec session.
    Notice,
}

fn sanitize_control_type(t: &str) -> String {
    if t.len() <= 32
        && t.chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
    {
        t.to_string()
    } else {
        "<invalid>".to_string()
    }
}

/// Classify a Sprites exec text frame according to the documented control
/// protocol; unknown well-formed control types are tolerated and ignored. Non-JSON text is output only for TTY sessions.
fn parse_sprite_ctrl(text: &str, tty: bool) -> std::result::Result<SpriteCtrl, ExecProtocolError> {
    if text.len() > EXEC_CONTROL_TEXT_MAX {
        return if tty {
            Ok(SpriteCtrl::Forward)
        } else {
            Err(ExecProtocolError::MalformedControl)
        };
    }
    let Ok(v) = serde_json::from_str::<serde_json::Value>(text) else {
        return if tty {
            Ok(SpriteCtrl::Forward)
        } else {
            Err(ExecProtocolError::MalformedControl)
        };
    };
    let Some(object) = v.as_object() else {
        return if tty {
            Ok(SpriteCtrl::Forward)
        } else {
            Err(ExecProtocolError::MalformedControl)
        };
    };
    let Some(t) = object.get("type").and_then(|t| t.as_str()) else {
        return Err(ExecProtocolError::MalformedControl);
    };
    match t {
        "exit" => {
            let code = object
                .get("exit_code")
                .and_then(|code| code.as_i64())
                .and_then(|code| i32::try_from(code).ok())
                .ok_or(ExecProtocolError::MalformedExit)?;
            Ok(SpriteCtrl::Exit(code))
        }
        "session_info" => {
            let Some(session_id) = object
                .get("session_id")
                .and_then(|session_id| session_id.as_str())
                .filter(|session_id| !session_id.is_empty())
            else {
                return Err(ExecProtocolError::MalformedControl);
            };
            tracing::debug!(
                target: "thegn::provider",
                cols = object.get("cols").and_then(serde_json::Value::as_u64),
                rows = object.get("rows").and_then(serde_json::Value::as_u64),
                "sprites exec session_info size"
            );
            Ok(SpriteCtrl::Session(session_id.to_string()))
        }
        "port_opened" | "port_closed" => Ok(SpriteCtrl::Notice),
        _ => {
            // The server sends control types the AsyncAPI exec document does not list
            // (seen live: "debug"). A well-formed control message of an unknown type
            // is tolerated so a new server message cannot break every command.
            tracing::debug!(
                target: "thegn::provider",
                control = %sanitize_control_type(t),
                "sprites exec: ignoring an unknown control message type"
            );
            Ok(SpriteCtrl::Notice)
        }
    }
}

impl SpritesProvider {
    /// Open a native PTY exec session over the Sprites WSS exec API — no `sprite`
    /// CLI. The returned [`ExecSession`]'s driver task lives on the ambient tokio
    /// runtime until the socket closes or `ExecControl::Close` is sent.
    pub async fn open_exec(&self, id: &str, spec: &ExecSpec) -> Result<ExecSession> {
        let url = exec_ws_url(self.base()?, id, spec)?;
        let session = Box::pin(self.start_session(url, spec.tty, None, None)).await?;
        if spec.tty {
            send_initial_size(&session, spec.cols, spec.rows);
        }
        Ok(session)
    }

    pub async fn open_exec_streaming(&self, id: &str, spec: &ExecSpec) -> Result<StreamingExec> {
        anyhow::ensure!(!spec.tty, "sprites: a streaming exec is non-tty");
        let (session, stderr, verdict) = Box::pin(self.open_exec_capturing(id, spec)).await?;
        Ok(StreamingExec {
            session,
            stderr: ExecStderr(stderr),
            verdict,
        })
    }

    async fn open_exec_capturing(&self, id: &str, spec: &ExecSpec) -> Result<CapturedExec> {
        let url = exec_ws_url(self.base()?, id, spec)?;
        let stderr = Arc::new(Mutex::new(StderrTail::default()));
        let (done_tx, done_rx) = tokio::sync::oneshot::channel();
        let session =
            Box::pin(self.start_session(url, spec.tty, Some(Arc::clone(&stderr)), Some(done_tx)))
                .await?;
        Ok((session, stderr, done_rx))
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
        let session = Box::pin(self.connect_exec(url, true, false, None, None)).await?;
        send_initial_size(&session, cols, rows);
        Ok(session)
    }

    /// Run a one-shot command in the sprite over the WSS exec API (NON-tty) and
    /// collect its combined output + exit code. This is the CLI-free / bridge-free
    /// channel the env provisioner uses to set the sandbox up (the non-tty framing
    /// surfaces stdout, stderr, and the exit without mixing stderr into stdout).
    /// Returns `(exit_code, output)`.
    pub async fn run_exec(
        &self,
        id: &str,
        argv: &[String],
        cwd: Option<&str>,
        env: &[(String, String)],
    ) -> Result<(i32, String)> {
        Box::pin(self.run_exec_within(id, argv, cwd, env, RUN_EXEC_DEADLINE)).await
    }

    /// Run one-shot exec with an overall deadline covering open and output.
    pub async fn run_exec_within(
        &self,
        id: &str,
        argv: &[String],
        cwd: Option<&str>,
        env: &[(String, String)],
        deadline: Duration,
    ) -> Result<(i32, String)> {
        let spec = ExecSpec {
            argv: argv.to_vec(),
            tty: false,
            cols: 0,
            rows: 0,
            env: env.to_vec(),
            cwd: cwd.map(str::to_string),
        };
        let until = tokio::time::Instant::now() + deadline;
        let (mut sess, stderr, done) =
            match tokio::time::timeout_at(until, Box::pin(self.open_exec_capturing(id, &spec)))
                .await
            {
                Ok(session) => session?,
                Err(_) => {
                    return Err(anyhow!(
                        "sprites: exec exceeded its {}s deadline; the session was closed",
                        deadline.as_secs()
                    ));
                }
            };
        let collected = match Box::pin(collect_exec_output_until(
            &mut sess.frames,
            crate::vps::ssh_shim::EXEC_OUTPUT_CAP,
            until,
        ))
        .await
        {
            Some(collected) => collected,
            None => {
                if let Err(error) = sess.control.send(ExecControl::Close).await {
                    tracing::debug!(target: "thegn::provider", %error, "best-effort exec close after deadline failed");
                }
                return Err(anyhow!(
                    "sprites: exec exceeded its {}s deadline; the session was closed",
                    deadline.as_secs()
                ));
            }
        };
        if let Err(error) = sess.control.send(ExecControl::Close).await {
            tracing::debug!(target: "thegn::provider", %error, "best-effort exec close failed");
        }
        // No verdict (driver gone or still draining) falls through to the
        // exit-status check below; the driver's own drain bound ends it.
        if let Ok(Ok(Some(fault))) = tokio::time::timeout_at(
            until.min(tokio::time::Instant::now() + EXEC_EXIT_DRAIN + Duration::from_secs(1)),
            done,
        )
        .await
        {
            return Err(anyhow::Error::new(fault));
        }
        let Some(code) = collected.code else {
            return Err(anyhow!("exec stream ended without an exit status"));
        };
        let output =
            crate::vps::ssh_shim::render_capped_output(&collected.output, collected.dropped);
        Ok((code, append_stderr_tail(output, code, &stderr)))
    }

    /// Run the WSS handshake (bearer auth) and spawn the bridge task. `tty`
    /// selects the wire framing: raw (PTY) vs 1-byte stream-id prefixes (non-PTY).
    async fn start_session(
        &self,
        url: String,
        tty: bool,
        stderr: Option<StderrSink>,
        done: Option<tokio::sync::oneshot::Sender<Option<ExecProtocolError>>>,
    ) -> Result<ExecSession> {
        Box::pin(self.connect_exec(url, tty, true, stderr, done)).await
    }

    // Fresh exec can wait for cold boot. Reattach is already retried by the
    // session relay and must not multiply its budget by an inner 90s loop.
    async fn connect_exec(
        &self,
        url: String,
        tty: bool,
        cold_open: bool,
        stderr: Option<StderrSink>,
        done: Option<tokio::sync::oneshot::Sender<Option<ExecProtocolError>>>,
    ) -> Result<ExecSession> {
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
            // Interactive keystrokes should not wait behind Nagle, as with ssh.
            match tokio::time::timeout(
                attempt_timeout,
                Box::pin(tokio_tungstenite::connect_async_with_config(
                    req,
                    Some(exec_ws_config()),
                    true,
                )),
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

        let (frames_tx, frames_rx) = tokio::sync::mpsc::channel::<ExecFrame>(EXEC_CHANNEL_CAPACITY);
        let (control_tx, control_rx) = tokio::sync::mpsc::channel::<ExecControl>(256);
        let (sid_tx, sid_rx) = tokio::sync::watch::channel::<Option<String>>(None);

        tokio::spawn(Box::pin(drive_exec(
            ws, tty, frames_tx, control_rx, sid_tx, stderr, done,
        )));

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
        let (from_srv_tx, from_srv_rx) =
            tokio::sync::mpsc::channel::<Vec<u8>>(EXEC_CHANNEL_CAPACITY);
        let (end_tx, end_rx) = tokio::sync::oneshot::channel();
        tokio::spawn(Box::pin(drive_proxy(ws, to_srv_rx, from_srv_tx, end_tx)));
        Ok(ProxyStream {
            rx: from_srv_rx,
            tx: to_srv_tx,
            end: end_rx,
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

    let result = tokio::time::timeout(
        deadline,
        Box::pin(async move {
            // Interactive keystrokes should not wait behind Nagle, as with ssh.
            let (mut ws, _resp) =
                tokio_tungstenite::connect_async_with_config(req, Some(exec_ws_config()), true)
                    .await
                    .context("sprites: proxy ws connect")?;
            // The init frame selects the target; after it, the socket is a raw relay.
            ws.send(Message::Text(init.into()))
                .await
                .context("sprites: proxy init")?;
            Ok::<_, anyhow::Error>(ws)
        }),
    )
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
    end: tokio::sync::oneshot::Sender<ProxyEnd>,
) where
    S: futures_util::Sink<tokio_tungstenite::tungstenite::Message>
        + futures_util::Stream<
            Item = std::result::Result<
                tokio_tungstenite::tungstenite::Message,
                tokio_tungstenite::tungstenite::Error,
            >,
        > + Unpin,
    S::Error: std::fmt::Debug,
{
    use futures_util::{SinkExt, StreamExt};
    use tokio_tungstenite::tungstenite::{Message, protocol::frame::coding::CloseCode};
    let (mut write, mut read) = ws.split();
    let terminal = loop {
        tokio::select! {
            out = to_srv.recv() => match out {
                Some(b) => {
                    if let Err(error) = write.send(Message::Binary(b.into())).await {
                        break ProxyEnd::TransportError(truncate_proxy_text(format!("{error:?}"), 256));
                    }
                }
                None => break ProxyEnd::LocalClosed,
            },
            msg = read.next() => match msg {
                Some(Ok(Message::Binary(b))) => {
                    if !send_chunked(&from_srv, b.into(), std::convert::identity).await {
                        break ProxyEnd::LocalClosed;
                    }
                }
                Some(Ok(Message::Close(frame))) => {
                    break match frame {
                        None => ProxyEnd::PeerClosed,
                        Some(frame) => {
                            let code = u16::from(frame.code);
                            if matches!(frame.code, CloseCode::Normal | CloseCode::Away) {
                                ProxyEnd::PeerClosed
                            } else {
                                ProxyEnd::RemoteRejected { code, reason: truncate_proxy_text(frame.reason.to_string(), 256) }
                            }
                        }
                    };
                }
                None => break ProxyEnd::TransportDropped,
                Some(Ok(_)) => {} // ignore text/ping/pong
                Some(Err(error)) => {
                    break ProxyEnd::TransportError(truncate_proxy_text(error.to_string(), 256));
                }
            },
        }
    };
    let _ = write.send(Message::Close(None)).await; // best-effort: close handshake; peer may be gone
    match end.send(terminal) {
        Ok(()) => {}
        Err(_) => tracing::debug!("provider proxy terminal receiver was dropped"),
    }
}

/// The exec bridge task: pump WebSocket frames ⇄ the [`ExecSession`] channels.
/// Generic over the socket so the concrete (TLS) stream type need not be named.
/// `tty` selects the wire framing: raw bytes (PTY) vs 1-byte stream-id prefixes
/// (non-PTY — stdin gets a `0` prefix; only stdout/exit are surfaced, while an
/// optional stderr sink captures stream id 2). The server repeats the exit in
/// binary and JSON forms; a repeat with the same code is accepted, while a
/// different code is a protocol error.
/// The self-calibrating liveness rule is provider-neutral and can be adopted by
/// any WebSocket exec transport.
async fn drive_exec<S>(
    ws: S,
    tty: bool,
    frames_tx: tokio::sync::mpsc::Sender<ExecFrame>,
    mut control_rx: tokio::sync::mpsc::Receiver<ExecControl>,
    sid_tx: tokio::sync::watch::Sender<Option<String>>,
    stderr: Option<StderrSink>,
    done: Option<tokio::sync::oneshot::Sender<Option<ExecProtocolError>>>,
) -> Option<ExecProtocolError>
where
    S: futures_util::Sink<tokio_tungstenite::tungstenite::Message>
        + futures_util::Stream<
            Item = std::result::Result<
                tokio_tungstenite::tungstenite::Message,
                tokio_tungstenite::tungstenite::Error,
            >,
        > + Unpin,
{
    use futures_util::{SinkExt, StreamExt};
    use tokio_tungstenite::tungstenite::{
        Message,
        protocol::{CloseFrame, frame::coding::CloseCode},
    };
    let (mut write, mut read) = ws.split();
    let mut stats = ExecWireStats::new(tokio::time::Instant::now());
    let mut exited = None;
    let mut drain_until = None;
    let mut probe_at: Option<tokio::time::Instant> = None;
    let mut probed = false;
    let mut pong_seen = false;
    let mut probe_disabled_logged = false;
    let mut control_closed = false;
    let mut fault = None;

    macro_rules! protocol_fault {
        ($error:expr) => {{
            let error = $error;
            tracing::warn!(
                target: "thegn::provider",
                error = %error,
                "sprites exec protocol error; closing the session"
            );
            if write
                .send(Message::Close(Some(CloseFrame {
                    code: CloseCode::Protocol,
                    reason: "".into(),
                })))
                .await
                .is_err()
            {
                tracing::debug!(
                    target: "thegn::provider",
                    "provider exec protocol close could not be sent"
                );
            }
            fault = Some(error);
            break;
        }};
    }

    let initial_ping_failed = write
        .send(Message::Ping(b"thegn".to_vec().into()))
        .await
        .is_err();
    if initial_ping_failed {
        tracing::debug!(target: "thegn::provider", "provider exec initial ping could not be sent");
    }

    loop {
        if initial_ping_failed {
            break;
        }
        tokio::select! {
            _ = async {
                if let Some(deadline) = drain_until {
                    tokio::time::sleep_until(deadline).await;
                } else {
                    std::future::pending::<()>().await;
                }
            } => break,
            _ = async {
                if let Some(deadline) = probe_at {
                    tokio::time::sleep_until(deadline).await;
                } else {
                    std::future::pending::<()>().await;
                }
            }, if exited.is_none() => {
                if !pong_seen {
                    if !probe_disabled_logged {
                        tracing::debug!(
                            target: "thegn::provider",
                            "peer does not answer pings; liveness probe disabled for this connection"
                        );
                        probe_disabled_logged = true;
                    }
                    probe_at = None;
                    probed = false;
                } else if !probed {
                    if write
                        .send(Message::Ping(b"thegn".to_vec().into()))
                        .await
                        .is_err()
                    {
                        break;
                    }
                    probed = true;
                    probe_at = Some(tokio::time::Instant::now() + EXEC_PROBE_DEAD);
                } else {
                    tracing::debug!(
                        target: "thegn::provider",
                        "exec socket unanswered after input; treating as dropped"
                    );
                    break;
                }
            },
            ctrl = control_rx.recv(), if !control_closed => {
                if exited.is_some() {
                    match ctrl {
                        Some(ExecControl::Stdin(_)) => {
                            tracing::debug!(target: "thegn::provider", "dropping exec stdin after exit");
                        }
                        Some(ExecControl::Resize { .. }) => {
                            tracing::debug!(target: "thegn::provider", "dropping exec resize after exit");
                        }
                        Some(ExecControl::Close) | None => {
                            if write.send(Message::Close(None)).await.is_err() {
                                tracing::debug!(target: "thegn::provider", "provider exec close could not be sent");
                            }
                            control_closed = true;
                        }
                    }
                } else {
                    match ctrl {
                        Some(ExecControl::Stdin(b)) => {
                            let frame = if tty { b } else { encode_stream_stdin(&b) };
                            let frame_len = frame.len();
                            if write.send(Message::Binary(frame.into())).await.is_err() {
                                break;
                            }
                            stats.outbound(frame_len);
                            if tty && !probe_disabled_logged {
                                let now = tokio::time::Instant::now();
                                match probe_at {
                                    None => probe_at = Some(now + EXEC_PROBE_AFTER),
                                    // Time spent blocked in our own send is not time the peer failed to answer.
                                    Some(at) if at <= now => {
                                        probe_at = Some(now + EXEC_PROBE_DEAD)
                                    }
                                    Some(_) => {}
                                }
                            }
                        }
                        Some(ExecControl::Resize { cols, rows }) => {
                            if write.send(Message::Text(resize_json(cols, rows).into())).await.is_err() {
                                tracing::debug!(target: "thegn::provider", "provider exec resize could not be sent");
                            }
                        }
                        Some(ExecControl::Close) => {
                            if !tty
                                && write
                                    .send(Message::Binary(encode_stream_stdin_eof().into()))
                                    .await
                                    .is_err()
                            {
                                tracing::debug!(target: "thegn::provider", "provider exec stdin EOF could not be sent");
                            }
                            if write.send(Message::Close(None)).await.is_err() {
                                tracing::debug!(target: "thegn::provider", "provider exec close could not be sent");
                            }
                            break;
                        }
                        None => {
                            if !tty
                                && write
                                    .send(Message::Binary(encode_stream_stdin_eof().into()))
                                    .await
                                    .is_err()
                            {
                                tracing::debug!(target: "thegn::provider", "provider exec stdin EOF could not be sent");
                                break;
                            }
                            control_closed = true;
                        }
                    }
                }
            },
            msg = read.next() => {
                if matches!(&msg, Some(Ok(_))) {
                    probe_at = None;
                    probed = false;
                }
                match msg {
                Some(Ok(Message::Binary(b))) => {
                    if let Some((bytes, ms, wait_ms)) =
                        stats.inbound(b.len(), tokio::time::Instant::now())
                    {
                        tracing::debug!(
                            target: "thegn::provider",
                            kib = bytes / 1024,
                            ms,
                            wait_ms,
                            in_msgs = stats.in_msgs,
                            max_in = stats.max_in,
                            "sprites exec inbound MiB mark"
                        );
                    }
                    if tracing::enabled!(target: "thegn::provider", tracing::Level::TRACE) {
                        let preview = &b[..b.len().min(64)];
                        tracing::trace!(
                            target: "thegn::provider",
                            kind = "binary",
                            len = b.len(),
                            bytes = %preview.escape_ascii(),
                            "sprites exec frame"
                        );
                    }
                    if tty {
                        if exited.is_some() {
                            protocol_fault!(ExecProtocolError::OutputAfterExit);
                        } else if !b.is_empty() {
                            let began = tokio::time::Instant::now();
                            let sent =
                                send_chunked(&frames_tx, b.into(), ExecFrame::Stdout).await;
                            stats.waited(began.elapsed());
                            if !sent {
                                break;
                            }
                        }
                    } else {
                        let decoded = match decode_stream_frame(&b) {
                            Ok(decoded) => decoded,
                            Err(error) => protocol_fault!(error),
                        };
                        match decoded {
                            StreamMsg::Stdout(d) => {
                                if exited.is_some() {
                                    protocol_fault!(ExecProtocolError::OutputAfterExit);
                                } else if !d.is_empty() {
                                    let began = tokio::time::Instant::now();
                                    let sent = send_chunked(&frames_tx, d, ExecFrame::Stdout).await;
                                    stats.waited(began.elapsed());
                                    if !sent {
                                        break;
                                    }
                                }
                            }
                            StreamMsg::Exit(code) => {
                                match record_exec_exit(&mut exited, code) {
                                    Ok(true) => {}
                                    Ok(false) => continue,
                                    Err(error) => protocol_fault!(error),
                                }
                                if frames_tx.send(ExecFrame::Exit(code)).await.is_err() {
                                    tracing::debug!(target: "thegn::provider", "provider exec exit frame could not be sent");
                                }
                                drain_until = Some(tokio::time::Instant::now() + EXEC_EXIT_DRAIN);
                                if !tty
                                    && control_closed
                                    && write.send(Message::Close(None)).await.is_err()
                                {
                                    tracing::debug!(target: "thegn::provider", "provider exec close could not be sent");
                                }
                            }
                            // Stderr is never part of the consumer's byte stream.
                            StreamMsg::Stderr(d) => {
                                if exited.is_some() {
                                    protocol_fault!(ExecProtocolError::OutputAfterExit);
                                } else if let Some(stderr) = &stderr {
                                    stderr
                                        .lock()
                                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                                        .push(&d);
                                }
                            }
                        }
                    }
                }
                Some(Ok(Message::Text(t))) => {
                    if let Some((bytes, ms, wait_ms)) =
                        stats.inbound(t.len(), tokio::time::Instant::now())
                    {
                        tracing::debug!(
                            target: "thegn::provider",
                            kib = bytes / 1024,
                            ms,
                            wait_ms,
                            in_msgs = stats.in_msgs,
                            max_in = stats.max_in,
                            "sprites exec inbound MiB mark"
                        );
                    }
                    if tracing::enabled!(target: "thegn::provider", tracing::Level::TRACE) {
                        let bytes = t.as_bytes();
                        let preview = &bytes[..bytes.len().min(64)];
                        tracing::trace!(
                            target: "thegn::provider",
                            kind = "text",
                            len = bytes.len(),
                            bytes = %preview.escape_ascii(),
                            "sprites exec frame"
                        );
                    }
                    let parsed = match parse_sprite_ctrl(t.as_str(), tty) {
                        Ok(parsed) => parsed,
                        Err(error) => protocol_fault!(error),
                    };
                    match parsed {
                        SpriteCtrl::Session(s) => {
                            if sid_tx.send(Some(s)).is_err() {
                                tracing::debug!(target: "thegn::provider", "provider exec session id receiver was dropped");
                            }
                        }
                        SpriteCtrl::Exit(code) => {
                            match record_exec_exit(&mut exited, code) {
                                Ok(true) => {}
                                Ok(false) => continue,
                                Err(error) => protocol_fault!(error),
                            }
                            if frames_tx.send(ExecFrame::Exit(code)).await.is_err() {
                                tracing::debug!(target: "thegn::provider", "provider exec exit frame could not be sent");
                            }
                            drain_until = Some(tokio::time::Instant::now() + EXEC_EXIT_DRAIN);
                            if !tty
                                && control_closed
                                && write.send(Message::Close(None)).await.is_err()
                            {
                                tracing::debug!(target: "thegn::provider", "provider exec close could not be sent");
                            }
                        }
                        SpriteCtrl::Forward if tty => {
                            if exited.is_some() {
                                protocol_fault!(ExecProtocolError::OutputAfterExit);
                            }
                            if !t.is_empty() {
                                let began = tokio::time::Instant::now();
                                let sent = send_chunked(
                                    &frames_tx,
                                    t.as_bytes().to_vec(),
                                    ExecFrame::Stdout,
                                )
                                .await;
                                stats.waited(began.elapsed());
                                if !sent {
                                    break;
                                }
                            }
                        }
                        SpriteCtrl::Forward => {}
                        SpriteCtrl::Notice => {}
                    }
                }
                Some(Ok(Message::Ping(p))) => {
                    if write.send(Message::Pong(p)).await.is_err() {
                        tracing::debug!(target: "thegn::provider", "provider exec pong could not be sent");
                    }
                }
                Some(Ok(Message::Pong(_))) => {
                    pong_seen = true;
                }
                Some(Ok(Message::Close(_))) | None => {
                    break;
                }
                Some(Ok(_)) => {}
                Some(Err(_)) => {
                    break;
                }
                }
            },
        }
    }

    let secs = stats.started.elapsed().as_secs_f64();
    tracing::debug!(
        target: "thegn::provider",
        in_msgs = stats.in_msgs,
        in_bytes = stats.in_bytes,
        out_msgs = stats.out_msgs,
        out_bytes = stats.out_bytes,
        max_in = stats.max_in,
        consumer_wait_ms = stats.consumer_wait.as_millis() as u64,
        secs,
        "sprites exec wire stats"
    );
    if let Some(done) = done
        && done.send(fault.clone()).is_err()
    {
        tracing::debug!(target: "thegn::provider", "provider exec verdict receiver was dropped");
    }
    fault
}

fn record_exec_exit(exited: &mut Option<i32>, code: i32) -> Result<bool, ExecProtocolError> {
    match *exited {
        Some(previous) if previous == code => {
            tracing::trace!(
                target: "thegn::provider",
                "sprites exec: the exit was repeated in its other form"
            );
            Ok(false)
        }
        Some(_) => Err(ExecProtocolError::DuplicateExit),
        None => {
            *exited = Some(code);
            Ok(true)
        }
    }
}

/// One sandbox as a provider's inventory reports it (`thegn fleet`). `id` is
/// the thegn sandbox id — what [`Provider::list`] returns and what a
/// worktree's binding records. The remaining fields are `None` when the
/// provider has no typed inventory yet (the id-only fallback).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SandboxInfo {
    pub id: String,
    /// The provider's own handle for the resource (Fly: the machine id).
    pub name: Option<String>,
    pub state: Option<String>,
    /// The size key the bundled price table uses (Fly:
    /// `shared-cpu-1x:256MB`; VPS: the vendor slug).
    pub size: Option<String>,
    pub region: Option<String>,
    pub created_at_ms: Option<i64>,
}

impl SandboxInfo {
    /// A row with only its id known.
    pub fn id_only(id: impl Into<String>) -> Self {
        SandboxInfo {
            id: id.into(),
            ..SandboxInfo::default()
        }
    }
}

/// The first non-empty string among `keys` on `obj` — the tolerant reader
/// for optional inventory fields. A missing key or a wrong-typed value is
/// `None`, never an error (only a row's identity is strict).
pub(crate) fn inventory_text(obj: &serde_json::Value, keys: &[&str]) -> Option<String> {
    keys.iter().find_map(|key| {
        obj.get(*key)
            .and_then(serde_json::Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
    })
}

/// A creation time as unix milliseconds from the first usable key: an
/// RFC 3339 string, or an epoch number (seconds, or milliseconds when it is
/// too large to be seconds — past year 5138). Anything else is `None`.
pub(crate) fn inventory_time_ms(obj: &serde_json::Value, keys: &[&str]) -> Option<i64> {
    // 1e11 seconds is ~year 5138; any real epoch above it is milliseconds.
    const SECONDS_CEILING: i64 = 100_000_000_000;
    let epoch = |n: i64| {
        if n.abs() >= SECONDS_CEILING {
            Some(n)
        } else {
            n.checked_mul(1000)
        }
    };
    keys.iter().find_map(|key| match obj.get(*key)? {
        serde_json::Value::String(s) => chrono::DateTime::parse_from_rfc3339(s.trim())
            .ok()
            .map(|t| t.timestamp_millis())
            .or_else(|| s.trim().parse::<i64>().ok().and_then(epoch)),
        serde_json::Value::Number(n) => n
            .as_i64()
            .or_else(|| n.as_f64().filter(|f| f.is_finite()).map(|f| f as i64))
            .and_then(epoch),
        _ => None,
    })
}

/// A typed inventory with its completeness explicit (mirrors
/// [`crate::vps::Inventory`]): only `Complete` may support an absence claim.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InventoryReport {
    Complete(Vec<SandboxInfo>),
    Partial {
        rows: Vec<SandboxInfo>,
        reason: String,
    },
}

impl InventoryReport {
    /// The id-only fallback for a provider without a typed inventory.
    pub fn from_ids(ids: Vec<String>) -> Self {
        InventoryReport::Complete(ids.into_iter().map(SandboxInfo::id_only).collect())
    }

    /// Lift a typed provider inventory: `Partial` keeps its rows with the
    /// reason's text; `Unknown` (nothing is known) is an error.
    pub fn from_inventory(inventory: crate::vps::Inventory<SandboxInfo>) -> Result<Self> {
        use crate::vps::Inventory;
        match inventory {
            Inventory::Complete(rows) => Ok(InventoryReport::Complete(rows)),
            Inventory::Partial { rows, reason } => Ok(InventoryReport::Partial {
                rows,
                reason: reason.message().to_string(),
            }),
            Inventory::Unknown(reason) => Err(anyhow!(reason)),
        }
    }
}

/// The generic managed-sandbox provider dispatcher. Lifecycle methods delegate to
/// the variant's [`RemoteProvider`]; axis methods delegate to the variant's
/// sub-trait impl or return a clear "unsupported" error (gate on `caps`).
#[allow(
    clippy::large_enum_variant,
    reason = "one long-lived provider per env; boxing a variant would ripple through every match"
)]
pub enum Provider {
    Daytona(DaytonaProvider),
    Sprites(SpritesProvider),
    /// Commodity VPS vendors (Hetzner today) — lifecycle over REST, exec/files
    /// over plain ssh. No checkpoints by construction: a powered-off VPS still
    /// bills, so the only free state is destroyed (see `crate::vps`).
    Vps(crate::vps::VpsProvider),
    /// Fly.io machines (CLI-free): Machines REST lifecycle; reach is plain ssh
    /// to the app's dedicated IPv4 with thegn's managed key via the vps ssh
    /// shim; optional iroh call-home (`fly/mod.rs`). Scale-to-zero via
    /// stop/start. Container-native, so it's not a `VpsKind` (see `crate::fly`).
    Fly(crate::fly::FlyProvider),
    /// machine0 (`machine0.io`) VMs — lifecycle/images/keys over machine0's
    /// remote **MCP** endpoint (no `machine0` CLI), exec/files over plain ssh
    /// with thegn's managed key (`ExecKind::Ssh`, VPS-shaped). Native image
    /// snapshots (checkpoints) + `vm_suspend` scale-to-zero; NixOS via a
    /// post-create `nixos-rebuild --flake` over ssh (see `crate::machine0`).
    Machine0(crate::machine0::Machine0Provider),
    /// Vercel Sandbox (`provider = "vercel"`) — HTTP control plane, exec over
    /// the command API and `/interactive` WebSocket through the `thegn
    /// vercel-exec` self-bridge (no vendor CLI, no ssh). Persistent sandboxes
    /// stop at their session timeout and resume by name (see `crate::vercel`).
    Vercel(crate::vercel::VercelProvider),
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
    ///
    /// Derived from the provider kind's facts in core
    /// ([`ProviderCaps::for_kind`]), so a capability claim and the warnings
    /// that read the same fact can never disagree. Per kind: Sprites has every
    /// axis; a VPS has files via the ssh shim and no checkpoints (a powered-off
    /// VPS still bills, so recycle destroys); Fly has files over ssh to the
    /// machine's dedicated IPv4 (vps ssh shim) and scale-to-zero via
    /// stop/start; machine0 has files over ssh, checkpoints via images and
    /// scale-to-zero via `vm_suspend`; Vercel has files over fs/read + a
    /// gzipped-tar fs/write, egress via its allow-list policy, no checkpoints,
    /// and no native exec (the pane rides the `vercel-exec` self-bridge).
    pub fn caps(&self) -> ProviderCaps {
        match self.kind() {
            Some(kind) => ProviderCaps::for_kind(kind),
            // Exec-only: no files/checkpoints/egress of its own (those go through
            // the underlying provider that created the sandbox).
            None => ProviderCaps {
                exec_api: true,
                scale_to_zero: false,
                ..ProviderCaps::default()
            },
        }
    }

    /// The config provider kind of this variant; `None` for the exec-only
    /// iroh reach (not a config kind).
    pub fn kind(&self) -> Option<thegn_core::config::EnvProviderKind> {
        use thegn_core::config::EnvProviderKind as K;
        Some(match self {
            Provider::Daytona(_) => K::Daytona,
            Provider::Sprites(_) => K::Sprites,
            Provider::Vps(p) => p.spec().kind.env_kind(),
            Provider::Fly(_) => K::Fly,
            Provider::Machine0(_) => K::Machine0,
            Provider::Vercel(_) => K::Vercel,
            Provider::Iroh { .. } => return None,
        })
    }

    pub async fn create(&self) -> Result<SandboxHandle> {
        self.remote()
            .ok_or_else(|| iroh_exec_only("create"))?
            .create()
            .await
    }

    pub async fn destroy(&self, id: &str) -> Result<()> {
        Box::pin(async move {
            self.remote()
                .ok_or_else(|| iroh_exec_only("destroy"))?
                .destroy(id)
                .await
        })
        .await
    }

    pub async fn list(&self) -> Result<Vec<String>> {
        match self.remote() {
            Some(p) => p.list().await,
            // Exec-only: listing belongs to the sandbox's underlying provider.
            None => Err(iroh_exec_only("list")),
        }
    }

    /// The provider's sandboxes with whatever detail it can type (`thegn
    /// fleet`): every provider reads its own listing (size, region, state,
    /// creation time where the vendor reports them; unknown fields `None`).
    /// The exec-only iroh reach refuses, like `list`.
    pub async fn inventory(&self) -> Result<InventoryReport> {
        match self {
            Provider::Fly(p) => p.sandbox_inventory().await,
            Provider::Vps(p) => p.sandbox_inventory().await,
            Provider::Machine0(p) => p.sandbox_inventory().await,
            Provider::Sprites(p) => p.sandbox_inventory().await,
            Provider::Daytona(p) => p.sandbox_inventory().await,
            Provider::Vercel(p) => p.sandbox_inventory().await,
            Provider::Iroh { .. } => Err(iroh_exec_only("inventory")),
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
            Provider::Vercel(_) => "vercel",
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
            Provider::Vercel(p) => Some(p),
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
            Provider::Vercel(p) => Some(p),
            Provider::Daytona(_) | Provider::Iroh { .. } => None,
        }
    }

    /// The variant's egress-translation API (mirrors `caps().egress`).
    fn egress_ops(&self) -> Option<&dyn ProviderEgress> {
        match self {
            Provider::Sprites(p) => Some(p),
            Provider::Vercel(p) => Some(p),
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
    /// For machine0, a listed VM that still holds this machine's pending create
    /// record is adopted (readiness, provisioning, custody) and the record retired;
    /// an adoption failure is an error and keeps the record.
    pub async fn ensure_exists(&self, name: &str) -> Result<bool> {
        if let Some(owner) = self.binding_owner() {
            let name = name.to_owned();
            let owner = owner.to_owned();
            tokio::task::spawn_blocking(move || crate::vps::registry::check_owner(&name, &owner))
                .await??;
        }
        if self.list().await?.iter().any(|n| n == name) {
            if let Provider::Machine0(provider) = self {
                provider.adopt_listed_pending(name).await?;
            }
            if let Provider::Sprites(provider) = self {
                provider.prove_listed_sprite(name).await?;
            }
            return Ok(false);
        }
        self.create().await?;
        Ok(true)
    }

    fn binding_owner(&self) -> Option<&str> {
        match self {
            Self::Vps(provider) => provider.owner(),
            Self::Fly(provider) => provider.owner(),
            Self::Vercel(provider) => provider.owner(),
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
            Provider::Vercel(p) => p.stop_session(id).await,
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
            Provider::Vercel(p) => p.resume(id).await,
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
        Box::pin(async move {
            self.files_ops()
                .ok_or_else(|| self.no_files())?
                .write(id, path, data)
                .await
        })
        .await
    }

    /// Write an executable into the sandbox fs (mode 0755 where supported).
    pub async fn write_exec(&self, id: &str, path: &str, data: &[u8]) -> Result<()> {
        self.files_ops()
            .ok_or_else(|| self.no_files())?
            .write_exec(id, path, data)
            .await
    }

    /// Write an owner-only file into the sandbox fs (mode 0600 where supported;
    /// see [`ProviderFiles::write_private`]).
    pub async fn write_private(&self, id: &str, path: &str, data: &[u8]) -> Result<()> {
        Box::pin(async move {
            self.files_ops()
                .ok_or_else(|| self.no_files())?
                .write_private(id, path, data)
                .await
        })
        .await
    }

    /// Idempotently install the executable `data` at `path` in the sandbox.
    /// A per-destination lease serializes installers; stale leases expire after
    /// five minutes. SHA-256 is checked against the remote bytes themselves;
    /// uploads go to a staging sibling, are read back and verified, then the
    /// prior generation is hard-linked to `<path>.tg-prev` before publishing with
    /// `mv -f`. A failed post-publish verification restores that generation.
    /// A crashed lease reclaimer leaves `<path>.tg-lease.reclaim`, which reports
    /// Busy until it is removed.
    /// The legacy `<path>.fp` sidecar is ignored and best-effort removed. Returns
    /// whether it pushed (`true`) or the remote executable was already current
    /// (`false`). Requires the provider's file API (`caps().files`).
    pub async fn ensure_executable(&self, id: &str, path: &str, data: &[u8]) -> Result<bool> {
        if !self.caps().files {
            return Err(anyhow!("provider does not support a file push"));
        }
        let files = self.files_ops().ok_or_else(|| self.no_files())?;
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |duration| duration.as_nanos());
        let lease_path = format!("{path}.tg-lease");
        let previous_path = format!("{path}.tg-prev");
        let owner = format!("{}-{nanos}", std::process::id());
        let lease_id = id.to_string();
        let lease_path_for_acquire = lease_path.clone();
        let owner_for_acquire = owner.clone();
        let acquired = Box::pin(install_executable_verified(
            files,
            id,
            path,
            data,
            move || async move {
                let (code, output) = Box::pin(self.run_exec(
                    &lease_id,
                    &install_shell_argv(
                        "thegn-install-lease",
                        INSTALL_LEASE_ACQUIRE_SCRIPT,
                        &[
                            &lease_path_for_acquire,
                            &owner_for_acquire,
                            INSTALL_LEASE_TTL_MINUTES,
                        ],
                    ),
                    None,
                    &[],
                ))
                .await?;
                match code {
                    0 => Ok(true),
                    75 => Ok(false),
                    _ => Err(anyhow!("install lease command exited {code}: {output}")),
                }
            },
            {
                let lease_id = id.to_string();
                let lease_path = lease_path.clone();
                let owner = owner.clone();
                move || async move {
                    let (code, output) = Box::pin(self.run_exec(
                        &lease_id,
                        &install_shell_argv(
                            "thegn-install-release",
                            INSTALL_LEASE_RELEASE_SCRIPT,
                            &[&lease_path, &owner],
                        ),
                        None,
                        &[],
                    ))
                    .await?;
                    if code == 0 {
                        Ok(())
                    } else {
                        Err(anyhow!("install lease release exited {code}: {output}"))
                    }
                }
            },
            {
                let previous_id = id.to_string();
                let previous_path = previous_path.clone();
                let target_path = path.to_string();
                move || async move {
                    let (code, output) = Box::pin(self.run_exec(
                        &previous_id,
                        &install_shell_argv(
                            "thegn-install-preserve",
                            INSTALL_PRESERVE_SCRIPT,
                            &[&target_path, &previous_path],
                        ),
                        None,
                        &[],
                    ))
                    .await?;
                    if code == 0 {
                        Ok(())
                    } else {
                        Err(anyhow!(
                            "preserve previous executable exited {code}: {output}"
                        ))
                    }
                }
            },
            |stage, dest| async move {
                let (code, output) = Box::pin(self.run_exec(
                    id,
                    &[
                        "/bin/sh".into(),
                        "-c".into(),
                        "mv -f -- \"$1\" \"$2\"".into(),
                        "thegn-install-publish".into(),
                        stage,
                        dest,
                    ],
                    None,
                    &[],
                ))
                .await?;
                if code != 0 {
                    let mut tail_start = output.len().saturating_sub(200);
                    while !output.is_char_boundary(tail_start) {
                        tail_start += 1;
                    }
                    return Err(anyhow!("mv exited {code}: {}", &output[tail_start..]));
                }
                Ok(())
            },
            {
                let rollback_id = id.to_string();
                let previous_path = previous_path.clone();
                let target_path = path.to_string();
                move || async move {
                    let (code, output) = Box::pin(self.run_exec(
                        &rollback_id,
                        &install_shell_argv(
                            "thegn-install-restore",
                            INSTALL_RESTORE_SCRIPT,
                            &[&target_path, &previous_path],
                        ),
                        None,
                        &[],
                    ))
                    .await?;
                    if code == 0 {
                        Ok(())
                    } else {
                        Err(anyhow!(
                            "restore previous executable exited {code}: {output}"
                        ))
                    }
                }
            },
            {
                let cleanup_id = id.to_string();
                move || async move {
                    let (code, output) = Box::pin(self.run_exec(
                        &cleanup_id,
                        &install_shell_argv(
                            "thegn-install-cleanup",
                            INSTALL_REMOVE_PREVIOUS_SCRIPT,
                            &[&previous_path],
                        ),
                        None,
                        &[],
                    ))
                    .await?;
                    if code == 0 {
                        Ok(())
                    } else {
                        Err(anyhow!(
                            "remove previous executable exited {code}: {output}"
                        ))
                    }
                }
            },
        ))
        .await?;
        Ok(acquired)
    }

    /// Create and validate private directories before installing an executable.
    /// Symlinks, non-directories and directories owned by another user refuse.
    pub async fn ensure_private_exec_dir(&self, id: &str, root: &str, dir: &str) -> Result<()> {
        let (code, output) =
            Box::pin(self.run_exec(id, &private_exec_dir_argv(root, dir), None, &[])).await?;
        if code != 0 {
            let mut tail_start = output.len().saturating_sub(200);
            while !output.is_char_boundary(tail_start) {
                tail_start += 1;
            }
            return Err(anyhow!(
                "private bridge directory refused (exit {code}): {}",
                &output[tail_start..]
            ));
        }
        Ok(())
    }

    /// Push a local directory into the sandbox fs (provider `sync` projection).
    pub async fn upload_dir(&self, id: &str, local: &Path, remote: &str) -> Result<()> {
        self.files_ops()
            .ok_or_else(|| self.no_files())?
            .upload_dir(id, local, remote)
            .await
    }

    /// Push a local directory and return the file-content baseline for close
    /// time reconciliation.
    pub async fn upload_dir_with_baseline(
        &self,
        id: &str,
        local: &Path,
        remote: &str,
    ) -> Result<SyncBaseline> {
        upload_tree(
            self.files_ops().ok_or_else(|| self.no_files())?,
            id,
            local,
            remote,
        )
        .await
    }

    /// Pull the sandbox fs back into a local directory.
    pub async fn download_dir(&self, id: &str, remote: &str, local: &Path) -> Result<()> {
        self.files_ops()
            .ok_or_else(|| self.no_files())?
            .download_dir(id, remote, local)
            .await
    }

    /// Reconcile a close-time download against the upload baseline when one is
    /// available; legacy records retain the original overlay behaviour.
    pub async fn download_dir_reconciled(
        &self,
        id: &str,
        remote: &str,
        local: &Path,
        baseline: Option<&SyncBaseline>,
    ) -> Result<()> {
        let files = self.files_ops().ok_or_else(|| self.no_files())?;
        match baseline {
            Some(baseline) => download_reconciled(files, id, remote, local, baseline).await,
            None => files.download_dir(id, remote, local).await,
        }
    }

    /// Open a native PTY exec session (the `exec_api` capability), so an
    /// interactive pane attaches over the provider API with no vendor CLI. Gate
    /// on `caps().exec_api`.
    pub async fn open_exec(&self, id: &str, spec: &ExecSpec) -> Result<ExecSession> {
        match self {
            Provider::Sprites(p) => p.open_exec(id, spec).await,
            // In-process callers only (the `vercel-exec` bridge): native panes
            // never reach here because `caps().exec_api` is false.
            Provider::Vercel(p) => p.open_exec(id, spec).await,
            // The iroh reach carries interactive panes: route to the home endpoint.
            Provider::Iroh { home, .. } => home.open_exec(id, spec.clone()).await,
            _ => Err(anyhow!("provider '{}' has no native exec API", self.name())),
        }
    }

    pub async fn open_exec_streaming(&self, id: &str, spec: &ExecSpec) -> Result<StreamingExec> {
        match self {
            Provider::Sprites(p) => p.open_exec_streaming(id, spec).await,
            _ => Err(anyhow!("provider '{}' has no streaming exec", self.name())),
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
        Box::pin(self.run_exec_within(id, argv, cwd, env, RUN_EXEC_DEADLINE)).await
    }

    /// Run one-shot exec with a provider-specific overall deadline.
    pub async fn run_exec_within(
        &self,
        id: &str,
        argv: &[String],
        cwd: Option<&str>,
        env: &[(String, String)],
        deadline: Duration,
    ) -> Result<(i32, String)> {
        let timeout_error = || {
            anyhow!(
                "provider '{}' exec exceeded its {}s deadline",
                self.name(),
                deadline.as_secs()
            )
        };
        match self {
            Provider::Sprites(p) => Box::pin(p.run_exec_within(id, argv, cwd, env, deadline)).await,
            Provider::Vps(p) => {
                tokio::time::timeout(deadline, Box::pin(p.run_exec(id, argv, cwd, env)))
                    .await
                    .map_err(|_| timeout_error())?
            }
            Provider::Fly(p) => {
                tokio::time::timeout(deadline, Box::pin(p.run_exec(id, argv, cwd, env)))
                    .await
                    .map_err(|_| timeout_error())?
            }
            Provider::Machine0(p) => {
                tokio::time::timeout(deadline, Box::pin(p.run_exec(id, argv, cwd, env)))
                    .await
                    .map_err(|_| timeout_error())?
            }
            Provider::Vercel(p) => tokio::time::timeout(
                deadline,
                Box::pin(p.run_exec(id, argv, cwd, env, crate::vercel::Wake::Allow)),
            )
            .await
            .map_err(|_| timeout_error())?,
            Provider::Daytona(_) => Err(anyhow!("provider 'daytona' has no native exec API")),
            // One-shot capture over iroh: open a non-tty exec and drain it.
            Provider::Iroh { home, .. } => tokio::time::timeout(
                deadline,
                Box::pin(async {
                    let spec = ExecSpec {
                        argv: argv.to_vec(),
                        tty: false,
                        cols: 80,
                        rows: 24,
                        env: env.to_vec(),
                        cwd: cwd.map(str::to_string),
                    };
                    let mut session = Box::pin(home.open_exec(id, spec)).await?;
                    let collected = Box::pin(collect_exec_output(
                        &mut session.frames,
                        crate::vps::ssh_shim::EXEC_OUTPUT_CAP,
                    ))
                    .await;
                    let Some(code) = collected.code else {
                        return Err(anyhow!("exec stream ended without an exit status"));
                    };
                    Ok((
                        code,
                        crate::vps::ssh_shim::render_capped_output(
                            &collected.output,
                            collected.dropped,
                        ),
                    ))
                }),
            )
            .await
            .map_err(|_| timeout_error())?,
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

const PRIVATE_DIR_SCRIPT: &str = r#"umask 077
for d in "$1" "$2"; do
    if [ -L "$d" ]; then echo 'thegn: private bridge directory is a symlink' >&2; exit 3; fi
    if [ ! -e "$d" ]; then mkdir -m 0700 -- "$d" || { echo 'thegn: cannot create private bridge directory' >&2; exit 4; }; fi
    if [ ! -d "$d" ]; then echo 'thegn: private bridge path is not a directory' >&2; exit 5; fi
    if [ ! -O "$d" ]; then echo 'thegn: private bridge directory has a foreign owner' >&2; exit 6; fi
    chmod 0700 -- "$d" || { echo 'thegn: cannot secure private bridge directory' >&2; exit 7; }
done"#;

pub(crate) fn private_exec_dir_argv(root: &str, dir: &str) -> Vec<String> {
    vec![
        "/bin/sh".to_string(),
        "-c".to_string(),
        PRIVATE_DIR_SCRIPT.to_string(),
        "thegn-bridge-dir".to_string(),
        root.to_string(),
        dir.to_string(),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn publish_download_file(path: &Path, bytes: &[u8]) -> Result<()> {
        let parent = path
            .parent()
            .context("download destination has no parent")?;
        let name: String = path
            .file_name()
            .context("download destination has no file name")?
            .to_string_lossy()
            .into_owned();
        thegn_core::fsperm::open_rooted_dir(parent)?
            .publish_file(&name, bytes)
            .with_context(|| format!("publish {}", path.display()))
    }

    fn provider() -> DaytonaProvider {
        DaytonaProvider::new("https://app.daytona.io/api/", "tok", "debian:stable")
    }

    fn sprites() -> SpritesProvider {
        SpritesProvider::new("https://api.sprites.dev/v1/", "tok", "my-sprite")
    }

    #[test]
    fn sprites_ensure_executable_future_stays_small() {
        let provider = Provider::Sprites(sprites());
        let future = provider.ensure_executable("sprite", "/workspace/.tg/thegn", b"binary");
        assert!(std::mem::size_of_val(&future) < 16 * 1024);
        drop(future);
    }

    #[test]
    fn provider_run_exec_future_stays_small() {
        let provider = Provider::Sprites(sprites());
        let argv = vec!["/bin/true".to_string()];
        let future = provider.run_exec("sprite", &argv, None, &[]);
        assert!(std::mem::size_of_val(&future) < 16 * 1024);
        drop(future);
    }

    async fn list_response(status: &str, body: &str) -> (String, tokio::task::JoinHandle<()>) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let body = body.to_string();
        let status = status.to_string();
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = [0u8; 1024];
            let n = socket.read(&mut request).await.unwrap();
            assert!(n > 0);
            let response = format!(
                "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            socket.write_all(response.as_bytes()).await.unwrap();
        });
        (format!("http://{address}"), server)
    }

    async fn paged_list_server(
        bodies: Vec<String>,
    ) -> (String, tokio::task::JoinHandle<Vec<String>>) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let mut request_lines = Vec::with_capacity(bodies.len());
            for body in bodies {
                let (mut socket, _) = listener.accept().await.unwrap();
                let mut request = Vec::new();
                let mut buffer = [0_u8; 4096];
                while !request.windows(4).any(|window| window == b"\r\n\r\n") {
                    let read = socket.read(&mut buffer).await.unwrap();
                    assert!(read > 0, "request ended before headers completed");
                    request.extend_from_slice(&buffer[..read]);
                }
                let request_text = String::from_utf8_lossy(&request);
                let request_line = request_text
                    .lines()
                    .next()
                    .expect("HTTP request must contain a request line");
                request_lines.push(request_line.to_string());
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                socket.write_all(response.as_bytes()).await.unwrap();
            }
            request_lines
        });
        (format!("http://{address}"), server)
    }

    type HttpResponse = (String, String);
    type HttpSequence = tokio::task::JoinHandle<Vec<String>>;

    async fn http_sequence_server(
        responses: Vec<HttpResponse>,
        repeat: Option<HttpResponse>,
    ) -> (String, HttpSequence) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let mut request_lines = Vec::new();
            let mut responses = responses.into_iter();
            loop {
                let response = match responses.next() {
                    Some(response) => response,
                    None => match repeat.clone() {
                        Some(response) => response,
                        None => return request_lines,
                    },
                };
                let (mut socket, _) = listener.accept().await.unwrap();
                let mut request = Vec::new();
                let mut buffer = [0_u8; 4096];
                while !request.windows(4).any(|window| window == b"\r\n\r\n") {
                    let read = socket.read(&mut buffer).await.unwrap();
                    assert!(read > 0, "request ended before headers completed");
                    request.extend_from_slice(&buffer[..read]);
                }
                request_lines.push(
                    String::from_utf8_lossy(&request)
                        .lines()
                        .next()
                        .expect("HTTP request must contain a request line")
                        .to_string(),
                );
                let (status, body) = response;
                let response = format!(
                    "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                socket.write_all(response.as_bytes()).await.unwrap();
            }
        });
        (format!("http://{address}"), server)
    }

    fn ledgered_sprites(base: &str, ledger_dir: &Path, name: &str) -> SpritesProvider {
        let scope = crate::sprites::ledger::OwnerScope {
            owner: "test-owner".into(),
            account: "test-account".into(),
        };
        let mut intent = crate::sprites::ledger::reserve_at(ledger_dir, &scope, name, 0).unwrap();
        intent.preserve();
        drop(intent);
        crate::sprites::ledger::commit_at(ledger_dir, &scope, name).unwrap();
        SpritesProvider::new(base, "tok", name)
            .with_scope(scope)
            .with_ledger_dir(ledger_dir.to_path_buf())
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
    fn checked_sprite_name_rejects_path_tricks() {
        let too_long = "a".repeat(64);
        for rejected in ["a/b", "a?b", "..", ""] {
            assert!(
                checked_sprite_name(rejected).is_err(),
                "accepted {rejected:?}"
            );
        }
        assert!(checked_sprite_name(&too_long).is_err());
        assert_eq!(checked_sprite_name("tg-abc123").unwrap(), "tg-abc123");
    }

    #[test]
    fn sprite_name_url_refuses_a_slash() {
        assert!(sprites().sprite_name_url("a/b").is_err());
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

    #[tokio::test(start_paused = true)]
    async fn control_gate_priority_uses_and_releases_reserved_capacity() {
        let gate = ControlGate::new(2, 1);
        let normal_a = gate.try_acquire("a").unwrap();
        let normal_b = gate.try_acquire("b").unwrap();

        assert!(
            tokio::time::timeout(std::time::Duration::from_secs(1), gate.acquire("a"))
                .await
                .is_err(),
            "ordinary acquisition must not use the priority reserve"
        );

        let priority_a = gate.acquire_priority("a").await.unwrap();
        let priority_b = gate.acquire_priority("b").await.unwrap();
        assert!(
            tokio::time::timeout(
                std::time::Duration::from_secs(1),
                gate.acquire_priority("a"),
            )
            .await
            .is_err(),
            "one priority caller must not exceed the per-authority reserve"
        );
        assert!(
            tokio::time::timeout(
                std::time::Duration::from_secs(1),
                gate.acquire_priority("c"),
            )
            .await
            .is_err(),
            "priority callers must not exceed the global reserve"
        );

        drop((normal_a, normal_b, priority_a, priority_b));
        assert!(gate.acquire_priority("a").await.is_ok());
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
        fail_remove_previous: bool,
    }

    impl MemFiles {
        fn new(files: HashMap<String, Vec<u8>>, corrupt_stage: bool) -> Self {
            Self::with_remove_previous_failure(files, corrupt_stage, false)
        }

        fn with_remove_previous_failure(
            files: HashMap<String, Vec<u8>>,
            corrupt_stage: bool,
            fail_remove_previous: bool,
        ) -> Self {
            Self {
                files: std::sync::Mutex::new(files),
                corrupt_stage,
                fail_remove_previous,
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

        fn preserve(&self, path: &str, previous: &str) -> Result<()> {
            let mut files = self
                .files
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            files.remove(previous);
            if let Some(bytes) = files.get(path).cloned() {
                files.insert(previous.to_string(), bytes);
            }
            Ok(())
        }

        fn restore(&self, path: &str, previous: &str) -> Result<()> {
            let mut files = self
                .files
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if let Some(bytes) = files.remove(previous) {
                files.insert(path.to_string(), bytes);
            } else {
                files.remove(path);
            }
            Ok(())
        }

        fn remove_previous(&self, previous: &str) -> Result<()> {
            if self.fail_remove_previous {
                return Err(anyhow!("remove previous refused"));
            }
            self.files
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .remove(previous);
            Ok(())
        }
    }

    #[test]
    fn install_lease_reclaim_rechecks_staleness_under_atomic_guard() {
        assert!(
            INSTALL_LEASE_ACQUIRE_SCRIPT.contains("if mkdir -- \"$1.reclaim\" 2>/dev/null; then")
        );
        assert!(
            INSTALL_LEASE_ACQUIRE_SCRIPT
                .contains("find \"$1\" -maxdepth 0 -mmin +\"$3\" -print 2>/dev/null")
        );
    }

    #[test]
    fn preserve_clears_stale_previous_when_target_is_absent() {
        let path = "/w/.tg/thegn";
        let previous = format!("{path}.tg-prev");
        let files = MemFiles::new(
            HashMap::from([(previous.clone(), b"stale".to_vec())]),
            false,
        );

        files.preserve(path, &previous).unwrap();

        let stored = files
            .files
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        assert!(!stored.contains_key(&previous));
        assert!(INSTALL_PRESERVE_SCRIPT.contains("rm -f -- \"$2\" || exit 1"));
        assert!(INSTALL_PRESERVE_SCRIPT.contains("ln -- \"$1\" \"$2\""));
    }

    #[derive(Clone)]
    struct TestLease {
        held: std::sync::Arc<std::sync::atomic::AtomicBool>,
        stale: std::sync::Arc<std::sync::atomic::AtomicBool>,
        acquired: std::sync::Arc<tokio::sync::Notify>,
        releases: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    }

    impl TestLease {
        fn new() -> Self {
            Self {
                held: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
                stale: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
                acquired: std::sync::Arc::new(tokio::sync::Notify::new()),
                releases: std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            }
        }

        fn acquire(&self) -> bool {
            if self.stale.swap(false, std::sync::atomic::Ordering::SeqCst) {
                self.held.store(false, std::sync::atomic::Ordering::SeqCst);
            }
            let acquired = self
                .held
                .compare_exchange(
                    false,
                    true,
                    std::sync::atomic::Ordering::SeqCst,
                    std::sync::atomic::Ordering::SeqCst,
                )
                .is_ok();
            if acquired {
                self.acquired.notify_one();
            }
            acquired
        }

        fn release(&self) {
            self.held.store(false, std::sync::atomic::Ordering::SeqCst);
            self.releases
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        }
    }

    async fn test_install<P, PFut>(
        files: &MemFiles,
        id: &str,
        path: &str,
        data: &[u8],
        lease: TestLease,
        publish: P,
    ) -> Result<bool>
    where
        P: FnOnce(String, String) -> PFut,
        PFut: std::future::Future<Output = Result<()>>,
    {
        let previous = format!("{path}.tg-prev");
        let acquire_lease = lease.clone();
        let release_lease = lease;
        install_executable_verified(
            files,
            id,
            path,
            data,
            move || async move { Ok(acquire_lease.acquire()) },
            move || async move {
                release_lease.release();
                Ok(())
            },
            {
                let path = path.to_string();
                let previous = previous.clone();
                move || async move { files.preserve(&path, &previous) }
            },
            publish,
            {
                let path = path.to_string();
                move || {
                    let previous = format!("{path}.tg-prev");
                    async move { files.restore(&path, &previous) }
                }
            },
            move || async move { files.remove_previous(&previous) },
        )
        .await
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
        let result = test_install(
            &files,
            "id",
            path,
            b"good",
            TestLease::new(),
            |stage, dest| async move {
                ran.store(true, std::sync::atomic::Ordering::SeqCst);
                mem.publish(&stage, &dest)
            },
        )
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
        let result = test_install(
            &files,
            "id",
            path,
            b"same",
            TestLease::new(),
            |_, _| async move {
                ran.store(true, std::sync::atomic::Ordering::SeqCst);
                Ok(())
            },
        )
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
        let result = test_install(
            &files,
            "id",
            path,
            b"new binary",
            TestLease::new(),
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
        let lease = TestLease::new();
        let result = test_install(
            &files,
            "id",
            path,
            b"new binary",
            lease.clone(),
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
        assert_eq!(lease.releases.load(std::sync::atomic::Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn exec_install_publish_failure_removes_stage() {
        let path = "/w/.tg/thegn";
        let files = MemFiles::new(HashMap::from([(path.into(), b"prior".to_vec())]), false);
        let published = std::sync::atomic::AtomicBool::new(false);
        let ran = &published;
        let lease = TestLease::new();
        let result = test_install(
            &files,
            "id",
            path,
            b"new binary",
            lease.clone(),
            |_, _| async move {
                ran.store(true, std::sync::atomic::Ordering::SeqCst);
                Err(anyhow!("publish refused"))
            },
        )
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
        assert_eq!(lease.releases.load(std::sync::atomic::Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn exec_install_concurrent_second_is_busy_without_torn_file() {
        let path = "/w/.tg/thegn";
        let files = std::sync::Arc::new(MemFiles::new(
            HashMap::from([(path.into(), b"prior".to_vec())]),
            false,
        ));
        let lease = TestLease::new();
        let publish_gate = std::sync::Arc::new(tokio::sync::Notify::new());
        let first_files = std::sync::Arc::clone(&files);
        let first_publish_files = std::sync::Arc::clone(&files);
        let first_lease = lease.clone();
        let first_gate = std::sync::Arc::clone(&publish_gate);
        let first = tokio::spawn(async move {
            test_install(
                first_files.as_ref(),
                "id",
                path,
                b"new binary",
                first_lease,
                move |stage, dest| {
                    let files = std::sync::Arc::clone(&first_publish_files);
                    async move {
                        first_gate.notified().await;
                        files.publish(&stage, &dest)
                    }
                },
            )
            .await
        });

        lease.acquired.notified().await;
        let second = test_install(
            files.as_ref(),
            "id",
            path,
            b"new binary",
            lease.clone(),
            |_, _| async move { Ok(()) },
        )
        .await;
        let error = second.expect_err("the second installer must observe the lease");
        assert!(matches!(
            error.downcast_ref::<InstallError>(),
            Some(InstallError::Busy)
        ));

        publish_gate.notify_one();
        assert!(first.await.unwrap().unwrap());
        assert_eq!(
            files
                .files
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .get(path)
                .map(Vec::as_slice),
            Some(&b"new binary"[..])
        );
        assert_eq!(lease.releases.load(std::sync::atomic::Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn exec_install_stale_lease_is_reclaimed() {
        let path = "/w/.tg/thegn";
        let files = MemFiles::new(HashMap::new(), false);
        let lease = TestLease::new();
        let mem = &files;
        lease.held.store(true, std::sync::atomic::Ordering::SeqCst);
        lease.stale.store(true, std::sync::atomic::Ordering::SeqCst);
        let result = test_install(
            &files,
            "id",
            path,
            b"new binary",
            lease.clone(),
            |stage, dest| async move { mem.publish(&stage, &dest) },
        )
        .await
        .unwrap();
        assert!(result);
        assert_eq!(lease.releases.load(std::sync::atomic::Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn exec_install_publish_verification_failure_restores_previous_bytes() {
        let path = "/w/.tg/thegn";
        let files = MemFiles::new(HashMap::from([(path.into(), b"prior".to_vec())]), false);
        let lease = TestLease::new();
        let mem = &files;
        let result = test_install(
            &files,
            "id",
            path,
            b"new binary",
            lease.clone(),
            |stage, dest| async move {
                mem.publish(&stage, &dest)?;
                mem.files
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .insert(dest, b"corrupt after publish".to_vec());
                Ok(())
            },
        )
        .await;
        assert!(matches!(
            result,
            Err(error) if error.to_string().contains("published bridge executable failed verification")
        ));
        let stored = files
            .files
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        assert_eq!(stored.get(path).map(Vec::as_slice), Some(&b"prior"[..]));
        assert!(!stored.contains_key(&format!("{path}.tg-prev")));
        drop(stored);
        assert_eq!(lease.releases.load(std::sync::atomic::Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn exec_install_stage_error_releases_lease() {
        let path = "/w/.tg/thegn";
        let files = MemFiles::new(HashMap::from([(path.into(), b"prior".to_vec())]), true);
        let lease = TestLease::new();
        let mem = &files;
        let result = test_install(
            &files,
            "id",
            path,
            b"new binary",
            lease.clone(),
            |stage, dest| async move { mem.publish(&stage, &dest) },
        )
        .await;
        assert!(result.is_err());
        assert_eq!(lease.releases.load(std::sync::atomic::Ordering::SeqCst), 1);
        assert!(!files.contains_stage());
    }

    #[tokio::test]
    async fn exec_install_previous_cleanup_failure_is_nonfatal() {
        let path = "/w/.tg/thegn";
        let files = MemFiles::with_remove_previous_failure(
            HashMap::from([(path.into(), b"prior".to_vec())]),
            false,
            true,
        );
        let mem = &files;
        let lease = TestLease::new();
        let result = test_install(
            &files,
            "id",
            path,
            b"new binary",
            lease.clone(),
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
        assert_eq!(lease.releases.load(std::sync::atomic::Ordering::SeqCst), 1);
    }

    #[test]
    fn private_exec_dir_argv_shape() {
        let argv = private_exec_dir_argv("/var/tmp/thegn-bridge", "/var/tmp/thegn-bridge/abc");
        assert_eq!(&argv[..2], &["/bin/sh", "-c"]);
        assert!(argv[2].contains("[ -L \"$d\" ]"));
        assert_eq!(
            &argv[3..],
            &[
                "thegn-bridge-dir",
                "/var/tmp/thegn-bridge",
                "/var/tmp/thegn-bridge/abc"
            ]
        );
    }

    #[test]
    fn private_exec_dir_script_refuses_symlinks_and_leaves_0700() {
        if !Path::new("/bin/sh").exists() {
            // This host has no local POSIX shell for the script test.
            return;
        }
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("fresh-root");
        let leaf = root.join("leaf");
        let argv = private_exec_dir_argv(root.to_str().unwrap(), leaf.to_str().unwrap());
        let mut command = std::process::Command::new(&argv[0]);
        command.args(&argv[1..]);
        let created = command.output().unwrap();
        assert!(
            created.status.success(),
            "{}",
            String::from_utf8_lossy(&created.stderr)
        );
        assert_eq!(thegn_core::fsperm::mode_bits(&root).unwrap(), Some(0o700));
        assert_eq!(thegn_core::fsperm::mode_bits(&leaf).unwrap(), Some(0o700));

        let target = temp.path().join("link-target");
        let linked_root = temp.path().join("linked-root");
        let link = std::process::Command::new("/bin/sh")
            .args([
                "-c",
                "ln -s -- \"$1\" \"$2\"",
                "symlink-root",
                target.to_str().unwrap(),
                linked_root.to_str().unwrap(),
            ])
            .status()
            .unwrap();
        assert!(
            link.success(),
            "local shell could not create the symlink fixture"
        );
        let target_leaf = target.join("leaf");
        let argv =
            private_exec_dir_argv(linked_root.to_str().unwrap(), target_leaf.to_str().unwrap());
        let mut command = std::process::Command::new(&argv[0]);
        command.args(&argv[1..]);
        let refused = command.output().unwrap();
        assert_eq!(refused.status.code(), Some(3));
        assert!(!target.exists());
    }

    #[test]
    fn sprites_create_body_names_the_sprite() {
        assert_eq!(
            SpritesProvider::create_body("dev"),
            serde_json::json!({"name": "dev", "url_settings": {"auth": "sprite"}})
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

    fn sprite_names(page: &SpritesListPage) -> Vec<&str> {
        page.rows.iter().map(|row| row.id.as_str()).collect()
    }

    #[test]
    fn sprites_parse_list_array_and_envelope() {
        let arr = serde_json::json!([{"name": "a"}, {"name": "b"}]);
        let parsed = SpritesProvider::parse_list_page(&arr).unwrap();
        assert_eq!(sprite_names(&parsed), vec!["a", "b"]);
        assert_eq!(parsed.next, None);
        let env = serde_json::json!({"sprites": [{"name": "c"}]});
        let parsed = SpritesProvider::parse_list_page(&env).unwrap();
        assert_eq!(sprite_names(&parsed), vec!["c"]);
        assert_eq!(parsed.next, None);
        assert!(SpritesProvider::parse_list_page(&serde_json::json!([{"id": "no-name"}])).is_err());
        assert!(SpritesProvider::parse_list_page(&serde_json::json!({})).is_err());
    }

    #[test]
    fn sprites_parse_list_is_strict() {
        assert!(SpritesProvider::parse_list_page(&serde_json::json!({})).is_err());
        let partial = SpritesProvider::parse_list_page(&serde_json::json!({
            "sprites": [{"name": "a"}],
            "has_more": true
        }));
        assert!(partial.unwrap_err().to_string().contains("has_more"));
        assert!(
            SpritesProvider::parse_list_page(&serde_json::json!({
                "sprites": [],
                "has_more": "no"
            }))
            .is_err()
        );
        let real_shape = serde_json::json!({
            "name": "blake-270",
            "sprites": [{"id": "sprite-abc", "name": "tgtest1", "status": "warm"}],
            "has_more": false
        });
        assert_eq!(
            sprite_names(&SpritesProvider::parse_list_page(&real_shape).unwrap()),
            vec!["tgtest1"]
        );
        assert!(SpritesProvider::parse_list_page(&serde_json::json!([{"id": "no-name"}])).is_err());
    }

    #[test]
    fn sprites_list_page_refuses_has_more_without_a_token() {
        let result = SpritesProvider::parse_list_page(&serde_json::json!({
            "sprites": [],
            "has_more": true
        }));
        assert!(result.unwrap_err().to_string().contains("has_more"));
    }

    #[test]
    fn sprites_list_page_refuses_token_without_has_more() {
        let result = SpritesProvider::parse_list_page(&serde_json::json!({
            "sprites": [],
            "has_more": false,
            "next_continuation_token": "t1"
        }));
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("continuation token without has_more")
        );
    }

    #[tokio::test]
    async fn sprites_inventory_complete_across_two_pages() {
        let (base, server) = paged_list_server(vec![
            r#"{"sprites":[{"name":"a"}],"has_more":true,"next_continuation_token":"t1"}"#.into(),
            r#"{"sprites":[{"name":"b"}],"has_more":false}"#.into(),
        ])
        .await;
        match SpritesProvider::new(&base, "tok", "x")
            .list_inventory()
            .await
        {
            crate::vps::Inventory::Complete(rows) => assert_eq!(rows, vec!["a", "b"]),
            other => panic!("expected Complete, got {other:?}"),
        }
        server.await.unwrap();
    }

    #[tokio::test]
    async fn sprites_inventory_page_cap_is_partial_and_list_keeps_old_text() {
        let bodies = |n: u32| -> Vec<String> {
            (0..n)
                .map(|page| {
                    format!(
                        "{{\"sprites\":[{{\"name\":\"s-{page}\"}}],\"has_more\":true,\"next_continuation_token\":\"t-{page}\"}}"
                    )
                })
                .collect()
        };
        let (base, server) = paged_list_server(bodies(crate::vps::MAX_INVENTORY_PAGES)).await;
        match SpritesProvider::new(&base, "tok", "x")
            .list_inventory()
            .await
        {
            crate::vps::Inventory::Partial { rows, reason } => {
                assert_eq!(reason, crate::vps::PartialReason::PageCap);
                assert_eq!(rows.len(), crate::vps::MAX_INVENTORY_PAGES as usize);
            }
            other => panic!("expected Partial, got {other:?}"),
        }
        server.await.unwrap();
        let (base, server) = paged_list_server(bodies(crate::vps::MAX_INVENTORY_PAGES)).await;
        let error = SpritesProvider::new(&base, "tok", "x").list().await;
        assert!(error.unwrap_err().to_string().contains("page limit"));
        server.await.unwrap();
    }

    #[tokio::test]
    async fn sprites_inventory_duplicate_is_partial_and_list_names_it() {
        let pages = || -> Vec<String> {
            vec![
                r#"{"sprites":[{"name":"a"}],"has_more":true,"next_continuation_token":"t1"}"#
                    .into(),
                r#"{"sprites":[{"name":"a"}],"has_more":false}"#.into(),
            ]
        };
        let (base, server) = paged_list_server(pages()).await;
        match SpritesProvider::new(&base, "tok", "x")
            .list_inventory()
            .await
        {
            crate::vps::Inventory::Partial { rows, reason } => {
                assert_eq!(reason, crate::vps::PartialReason::DuplicateId);
                assert_eq!(rows, vec!["a"]);
            }
            other => panic!("expected Partial, got {other:?}"),
        }
        server.await.unwrap();
        let (base, server) = paged_list_server(pages()).await;
        let message = SpritesProvider::new(&base, "tok", "x")
            .list()
            .await
            .unwrap_err()
            .to_string();
        assert!(message.contains("duplicate sprite name a"), "{message}");
        server.await.unwrap();
    }

    #[tokio::test]
    async fn sprites_inventory_repeated_token_is_unknown() {
        let pages = || -> Vec<String> {
            vec![
                r#"{"sprites":[{"name":"a"}],"has_more":true,"next_continuation_token":"t1"}"#
                    .into(),
                r#"{"sprites":[{"name":"b"}],"has_more":true,"next_continuation_token":"t1"}"#
                    .into(),
            ]
        };
        let (base, server) = paged_list_server(pages()).await;
        match SpritesProvider::new(&base, "tok", "x")
            .list_inventory()
            .await
        {
            crate::vps::Inventory::Unknown(reason) => {
                assert!(reason.contains("repeated continuation token"), "{reason}");
            }
            other => panic!("expected Unknown, got {other:?}"),
        }
        server.await.unwrap();
    }

    #[tokio::test]
    async fn sprites_inventory_non_2xx_is_unknown() {
        let (base, server) = list_response("500 Internal Server Error", "{}").await;
        let inventory = SpritesProvider::new(&base, "tok", "x")
            .list_inventory()
            .await;
        assert!(
            matches!(inventory, crate::vps::Inventory::Unknown(_)),
            "{inventory:?}"
        );
        server.await.unwrap();
    }

    #[tokio::test]
    async fn daytona_inventory_array_is_complete_pretty_and_minified() {
        let value = serde_json::json!([{"id": "d1"}, {"id": "d2"}]);
        for body in [
            value.to_string(),
            serde_json::to_string_pretty(&value).unwrap(),
        ] {
            let (base, server) = list_response("200 OK", &body).await;
            match DaytonaProvider::new(&base, "tok", "img")
                .list_inventory()
                .await
            {
                crate::vps::Inventory::Complete(rows) => assert_eq!(rows, vec!["d1", "d2"]),
                other => panic!("expected Complete, got {other:?}"),
            }
            server.await.unwrap();
        }
    }

    #[tokio::test]
    async fn daytona_inventory_total_mismatch_is_partial() {
        let value = serde_json::json!({"sandboxes": [{"id": "d1"}, {"id": "d2"}], "total": 3});
        for body in [
            value.to_string(),
            serde_json::to_string_pretty(&value).unwrap(),
        ] {
            let (base, server) = list_response("200 OK", &body).await;
            match DaytonaProvider::new(&base, "tok", "img")
                .list_inventory()
                .await
            {
                crate::vps::Inventory::Partial { rows, reason } => {
                    assert_eq!(reason, crate::vps::PartialReason::TotalMismatch);
                    assert_eq!(rows.len(), 2);
                }
                other => panic!("expected Partial, got {other:?}"),
            }
            server.await.unwrap();
            let (base, server) = list_response("200 OK", &body).await;
            let message = DaytonaProvider::new(&base, "tok", "img")
                .list()
                .await
                .unwrap_err()
                .to_string();
            assert_eq!(
                message,
                "daytona: GET /sandbox returned a partial inventory (total 3, got 2)"
            );
            server.await.unwrap();
        }
    }

    #[tokio::test]
    async fn daytona_inventory_malformed_envelope_is_unknown() {
        let (base, server) = list_response("200 OK", r#"{"unexpected": true}"#).await;
        let inventory = DaytonaProvider::new(&base, "tok", "img")
            .list_inventory()
            .await;
        assert!(
            matches!(inventory, crate::vps::Inventory::Unknown(_)),
            "{inventory:?}"
        );
        server.await.unwrap();
    }

    #[tokio::test]
    async fn sprites_list_follows_continuation_pages() {
        let (base, server) = paged_list_server(vec![
            r#"{"sprites":[{"name":"a"}],"has_more":true,"next_continuation_token":"t1"}"#.into(),
            r#"{"sprites":[{"name":"b"}],"has_more":false}"#.into(),
        ])
        .await;
        let names = SpritesProvider::new(&base, "tok", "x")
            .list()
            .await
            .unwrap();
        assert_eq!(names, vec!["a", "b"]);
        let requests = server.await.unwrap();
        let second = requests
            .iter()
            .position(|line| line.contains("continuation_token="))
            .expect("second page request must carry its continuation token");
        assert!(requests[second].contains("continuation_token=t1"));
        assert!(!requests[0].contains('?'));
    }

    #[tokio::test]
    async fn sprites_list_url_carries_max_results_on_every_page() {
        let (base, server) = paged_list_server(vec![
            r#"{"sprites":[{"name":"a"}],"has_more":true,"next_continuation_token":"t1"}"#.into(),
            r#"{"sprites":[{"name":"b"}],"has_more":false}"#.into(),
        ])
        .await;
        let names = SpritesProvider::new(&base, "tok", "x")
            .with_list_page_size(2)
            .list()
            .await
            .unwrap();
        assert_eq!(names, vec!["a", "b"]);
        let requests = server.await.unwrap();
        assert_eq!(requests.len(), 2);
        assert!(
            requests
                .iter()
                .all(|request| request.contains("max_results=2"))
        );
        assert!(requests[1].contains("continuation_token=t1"));
    }

    #[test]
    fn sprites_list_url_without_page_size_is_unchanged() {
        let p = sprites();
        assert_eq!(
            p.sprites_list_url(None).unwrap(),
            "https://api.sprites.dev/v1/sprites"
        );
        assert_eq!(
            p.sprites_list_url(Some("t1")).unwrap(),
            "https://api.sprites.dev/v1/sprites?continuation_token=t1"
        );
    }

    #[tokio::test]
    async fn sprites_list_refuses_a_repeated_continuation_token() {
        let (base, server) = paged_list_server(vec![
            r#"{"sprites":[{"name":"a"}],"has_more":true,"next_continuation_token":"t1"}"#.into(),
            r#"{"sprites":[{"name":"b"}],"has_more":true,"next_continuation_token":"t1"}"#.into(),
        ])
        .await;
        let result = SpritesProvider::new(&base, "tok", "x").list().await;
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("repeated continuation token")
        );
        server.await.unwrap();
    }

    #[tokio::test]
    async fn sprites_list_refuses_a_duplicate_name_across_pages() {
        let (base, server) = paged_list_server(vec![
            r#"{"sprites":[{"name":"a"}],"has_more":true,"next_continuation_token":"t1"}"#.into(),
            r#"{"sprites":[{"name":"a"}],"has_more":false}"#.into(),
        ])
        .await;
        let result = SpritesProvider::new(&base, "tok", "x").list().await;
        assert!(result.unwrap_err().to_string().contains("duplicate"));
        server.await.unwrap();
    }

    #[test]
    fn sprites_exec_builds_self_bridge_not_vendor_cli() {
        let exec = SpritesProvider::exec_for("dev");
        assert_eq!(
            exec,
            ExecKind::Command(vec![
                thegn_core::util::self_exe_str(),
                "sprite-exec".into(),
                "dev".into(),
                "--".into(),
            ])
        );
        if let ExecKind::Command(argv) = exec {
            assert!(
                argv.iter().all(|a| a != "sprite"),
                "the create handle must not name the vendor `sprite` CLI: {argv:?}"
            );
        }
    }

    #[test]
    fn sprites_has_native_exec_capability() {
        assert!(Provider::Sprites(sprites()).caps().exec_api);
        assert!(!Provider::Daytona(provider()).caps().exec_api);
    }

    #[test]
    fn caps_match_for_kind_for_every_constructible_variant() {
        use thegn_core::config::EnvProviderKind as K;
        let vps = |kind: crate::vps::VpsKind| {
            Provider::Vps(crate::vps::VpsProvider::new(crate::vps::VpsSpec {
                kind,
                api_base: String::new(),
                token: "t".into(),
                name: "tg-vps-1".into(),
                region: String::new(),
                size: String::new(),
                image: String::new(),
                max_instances: 0,
                max_lifetime_secs: 0,
                key_path: "/k".into(),
                pubkey: String::new(),
                skip_ready_wait: true,
            }))
        };
        let fly = Provider::Fly(crate::fly::FlyProvider::new(crate::fly::FlySpec {
            api_base: String::new(),
            graphql_url: String::new(),
            token: "t".into(),
            org_slug: String::new(),
            name: "tg-fly-1".into(),
            region: String::new(),
            size: String::new(),
            image: String::new(),
            max_instances: 0,
            max_lifetime_secs: 0,
            key_path: "/k".into(),
            pubkey: String::new(),
            iroh: None,
            skip_ready_wait: true,
        }));
        let machine0 = Provider::Machine0(crate::machine0::Machine0Provider::new(
            crate::machine0::Machine0Spec {
                endpoint: String::new(),
                api_key: "k".into(),
                name: "tg-m0-1".into(),
                image: String::new(),
                size: String::new(),
                size_req: crate::machine0::SizeReq::default(),
                region: String::new(),
                provision_flake: String::new(),
                ssh_user: String::new(),
                key_path: "/k".into(),
                pubkey: String::new(),
                max_instances: 0,
                max_lifetime_secs: 0,
                skip_ready_wait: true,
            },
        ));
        // Constructing a Vercel provider only admits the spec and builds the
        // HTTP client; no request is made.
        let vercel = Provider::Vercel(
            crate::vercel::VercelProvider::new(crate::vercel::VercelSpec {
                api_base: String::new(),
                token: crate::vercel::VercelToken::new("vcp_test_token".into()),
                credential_ref: "VERCEL_TOKEN".into(),
                scope: thegn_core::vercel::api::Scope {
                    team_id: "team_test".into(),
                    project_id: "prj_test".into(),
                },
                name: "tg-vercel-1".into(),
                vcpus: 2,
                image: None,
                region: None,
                max_instances: 0,
                max_lifetime_secs: 0,
                interactive_host_suffixes: Vec::new(),
                model_credentials: thegn_core::config::ModelCredentials::default(),
            })
            .expect("a well-formed vercel spec is admitted offline"),
        );
        let cases = [
            (Provider::Daytona(provider()), K::Daytona),
            (Provider::Sprites(sprites()), K::Sprites),
            (vps(crate::vps::VpsKind::Hetzner), K::Hetzner),
            (vps(crate::vps::VpsKind::DigitalOcean), K::DigitalOcean),
            (fly, K::Fly),
            (machine0, K::Machine0),
            (vercel, K::Vercel),
        ];
        for (p, kind) in &cases {
            assert_eq!(p.kind(), Some(*kind));
            assert_eq!(p.caps(), ProviderCaps::for_kind(*kind), "{kind:?}");
        }
        // Today's table, byte-identical to the old per-variant literals.
        let caps = |files, checkpoints, egress, exec_api, scale_to_zero| ProviderCaps {
            files,
            checkpoints,
            egress,
            exec_api,
            scale_to_zero,
        };
        assert_eq!(cases[0].0.caps(), caps(false, false, false, false, false));
        assert_eq!(cases[1].0.caps(), caps(true, true, true, true, true));
        assert_eq!(cases[2].0.caps(), caps(true, false, false, false, false));
        assert_eq!(cases[3].0.caps(), caps(true, false, false, false, false));
        assert_eq!(cases[4].0.caps(), caps(true, false, false, false, true));
        assert_eq!(cases[5].0.caps(), caps(true, true, false, false, true));
        // Vercel: files (fs/read + tar fs/write), egress (allow-list policy),
        // scale-to-zero; no checkpoints and no native exec.
        assert_eq!(cases[6].0.caps(), caps(true, false, true, false, true));
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

    #[tokio::test]
    async fn sprites_destroy_confirms_accepted_delete_before_forgetting_record() {
        let _state = thegn_core::testenv::IsolatedState::new();
        let ledger = tempfile::tempdir().unwrap();
        let (base, server) = http_sequence_server(
            vec![
                ("202 Accepted".into(), String::new()),
                ("200 OK".into(), r#"{"name":"sprite"}"#.into()),
                ("404 Not Found".into(), String::new()),
            ],
            None,
        )
        .await;
        let provider = ledgered_sprites(&base, ledger.path(), "sprite");

        provider
            .destroy_confirmed("sprite", Duration::from_secs(1), Duration::ZERO)
            .await
            .unwrap();

        let requests = server.await.unwrap();
        assert_eq!(
            requests,
            vec![
                "DELETE /sprites/sprite HTTP/1.1",
                "GET /sprites/sprite HTTP/1.1",
                "GET /sprites/sprite HTTP/1.1",
            ]
        );
        assert!(
            crate::sprites::ledger::prove_owned_at(
                ledger.path(),
                &crate::sprites::ledger::OwnerScope {
                    owner: "test-owner".into(),
                    account: "test-account".into(),
                },
                "sprite",
                &[],
            )
            .is_err(),
            "confirmed deletion must forget the ledger record"
        );
    }

    #[tokio::test]
    async fn sprites_destroy_keeps_record_and_custody_when_confirmation_times_out() {
        let _state = thegn_core::testenv::IsolatedState::new();
        let ledger = tempfile::tempdir().unwrap();
        let (base, server) = http_sequence_server(
            vec![
                ("202 Accepted".into(), String::new()),
                ("200 OK".into(), r#"{"name":"sprite"}"#.into()),
            ],
            Some(("200 OK".into(), r#"{"name":"sprite"}"#.into())),
        )
        .await;
        let provider = ledgered_sprites(&base, ledger.path(), "sprite")
            .with_custody_accounts(["test-custody".into()]);
        thegn_core::managed_ssh::record_authorized(
            "sprites",
            "test-custody",
            "sprite",
            Path::new("/tmp/thegn-test-key"),
            "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIIgVgF3FLyN2aHUalBpkk3cMVfTgD+7TrbdfTAcSvLvB thegn",
        )
        .unwrap();

        let error = provider
            .destroy_confirmed(
                "sprite",
                Duration::from_millis(30),
                Duration::from_millis(15),
            )
            .await
            .unwrap_err();
        assert!(error.to_string().contains("sprite still exists"));
        assert!(
            crate::sprites::ledger::prove_owned_at(
                ledger.path(),
                &crate::sprites::ledger::OwnerScope {
                    owner: "test-owner".into(),
                    account: "test-account".into(),
                },
                "sprite",
                &[],
            )
            .is_ok(),
            "an unproven deletion must keep the ledger record"
        );
        assert!(
            thegn_core::managed_ssh::read("sprites", "test-custody", "sprite")
                .unwrap()
                .is_some(),
            "an unproven deletion must keep managed SSH custody"
        );
        server.abort();
        let _ = server.await;
    }

    #[tokio::test]
    async fn sprites_second_destroy_retries_after_kept_record_and_proves_absence() {
        let _state = thegn_core::testenv::IsolatedState::new();
        let ledger = tempfile::tempdir().unwrap();
        let (base, server) = http_sequence_server(
            vec![
                ("202 Accepted".into(), String::new()),
                ("200 OK".into(), r#"{"name":"sprite"}"#.into()),
                ("202 Accepted".into(), String::new()),
                ("404 Not Found".into(), String::new()),
            ],
            None,
        )
        .await;
        let provider = ledgered_sprites(&base, ledger.path(), "sprite");
        let first = provider
            .destroy_confirmed("sprite", Duration::from_nanos(1), Duration::ZERO)
            .await
            .unwrap_err();
        assert!(first.to_string().contains("still exists"));
        provider
            .destroy_confirmed("sprite", Duration::from_secs(1), Duration::ZERO)
            .await
            .unwrap();

        let requests = server.await.unwrap();
        assert_eq!(
            requests
                .iter()
                .filter(|request| request.starts_with("DELETE "))
                .count(),
            2,
            "a kept record must permit a second DELETE"
        );
        assert!(
            crate::sprites::ledger::prove_owned_at(
                ledger.path(),
                &crate::sprites::ledger::OwnerScope {
                    owner: "test-owner".into(),
                    account: "test-account".into(),
                },
                "sprite",
                &[],
            )
            .is_err()
        );
    }

    #[tokio::test]
    async fn sprites_destroy_404_forgets_record_without_confirmation_get() {
        let _state = thegn_core::testenv::IsolatedState::new();
        let ledger = tempfile::tempdir().unwrap();
        let (base, server) =
            http_sequence_server(vec![("404 Not Found".into(), String::new())], None).await;
        let provider = ledgered_sprites(&base, ledger.path(), "sprite");
        provider
            .destroy_confirmed("sprite", Duration::ZERO, Duration::ZERO)
            .await
            .unwrap();
        assert_eq!(
            server.await.unwrap(),
            vec!["DELETE /sprites/sprite HTTP/1.1"]
        );
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
    fn attach_ws_url_refuses_a_session_id_that_is_not_one_segment() {
        for session in ["a/b", "..", "a?b", ""] {
            let error =
                attach_ws_url("https://api.sprites.dev/v1", "dev", session, 80, 24).unwrap_err();
            assert_eq!(error.to_string(), "sprites: invalid exec session id");
        }
        let error = attach_ws_url(
            "https://api.sprites.dev/v1",
            "dev",
            &"a".repeat(129),
            80,
            24,
        )
        .unwrap_err();
        assert_eq!(error.to_string(), "sprites: invalid exec session id");
    }

    #[tokio::test]
    async fn with_destroy_confirm_sets_the_budget() {
        let _state = thegn_core::testenv::IsolatedState::new();
        let ledger = tempfile::tempdir().unwrap();
        let (base, server) = http_sequence_server(
            vec![("202 Accepted".into(), String::new())],
            Some(("200 OK".into(), r#"{"name":"sprite"}"#.into())),
        )
        .await;
        let provider = ledgered_sprites(&base, ledger.path(), "sprite")
            .with_destroy_confirm(Duration::from_millis(50), Duration::from_millis(10));
        let result = tokio::time::timeout(Duration::from_secs(1), provider.destroy("sprite"))
            .await
            .expect("destroy confirmation must honor the injected budget");
        let error = result.expect_err("destroy unexpectedly confirmed");
        assert!(
            error
                .to_string()
                .contains("was accepted but the sprite still exists"),
            "{error:#}"
        );
        server.abort();
        let _ = server.await;
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
        assert_eq!(encode_stream_stdin_eof(), vec![4]);
        // server frames split on the 1-byte stream id.
        assert_eq!(
            decode_stream_frame(&[1, b'o', b'k']),
            Ok(StreamMsg::Stdout(b"ok".to_vec()))
        );
        assert_eq!(
            decode_stream_frame(&[2, b'e', b'r', b'r']),
            Ok(StreamMsg::Stderr(b"err".to_vec()))
        );
        assert_eq!(
            decode_stream_frame(&[3, b'1', b'3', b'7']),
            Err(ExecProtocolError::MalformedExit)
        );
        assert_eq!(
            decode_stream_frame(&[3]),
            Err(ExecProtocolError::MalformedExit)
        );
        assert_eq!(
            decode_stream_frame(&[3, 0, 0]),
            Err(ExecProtocolError::MalformedExit)
        );
        // A one-byte ASCII digit is the documented binary form, not text.
        assert_eq!(decode_stream_frame(&[3, b'1']), Ok(StreamMsg::Exit(49)));
        assert_eq!(decode_stream_frame(&[]), Err(ExecProtocolError::EmptyFrame));
        assert_eq!(
            decode_stream_frame(&[9, 1, 2]),
            Err(ExecProtocolError::UnknownStream(9))
        );
        assert_eq!(
            decode_stream_frame(&[0, b'x']),
            Err(ExecProtocolError::UnknownStream(0))
        );
        assert_eq!(
            decode_stream_frame(&[4]),
            Err(ExecProtocolError::UnknownStream(4))
        );
        assert_eq!(decode_stream_frame(&[1]), Ok(StreamMsg::Stdout(vec![])));
    }

    #[test]
    fn parse_sprite_ctrl_classifies_frames() {
        for tty in [false, true] {
            assert_eq!(
                parse_sprite_ctrl(r#"{"type":"exit","exit_code":0}"#, tty),
                Ok(SpriteCtrl::Exit(0))
            );
            assert_eq!(
                parse_sprite_ctrl(r#"{"type":"exit","exit_code":137}"#, tty),
                Ok(SpriteCtrl::Exit(137))
            );
            assert_eq!(
                parse_sprite_ctrl(r#"{"type":"exit"}"#, tty),
                Err(ExecProtocolError::MalformedExit)
            );
            assert_eq!(
                parse_sprite_ctrl(r#"{"type":"exit","exit_code":2147483648}"#, tty),
                Err(ExecProtocolError::MalformedExit)
            );
            assert_eq!(
                parse_sprite_ctrl(r#"{"session_id":"abc","tty":true,"cols":80}"#, tty),
                Err(ExecProtocolError::MalformedControl)
            );
            assert_eq!(
                parse_sprite_ctrl(r#"{"type":"session_info","session_id":"abc"}"#, tty),
                Ok(SpriteCtrl::Session("abc".into()))
            );
            assert_eq!(
                parse_sprite_ctrl(r#"{"type":"port_opened","port":8080}"#, tty),
                Ok(SpriteCtrl::Notice)
            );
            assert_eq!(
                parse_sprite_ctrl(r#"{"message":"not a control frame"}"#, tty),
                Err(ExecProtocolError::MalformedControl)
            );
            assert_eq!(
                parse_sprite_ctrl(r#"{"type":"Debug!x"}"#, tty),
                Ok(SpriteCtrl::Notice)
            );
            assert_eq!(
                parse_sprite_ctrl(r#"{"type":"debug"}"#, tty),
                Ok(SpriteCtrl::Notice)
            );
            assert_eq!(
                parse_sprite_ctrl("not json", tty),
                if tty {
                    Ok(SpriteCtrl::Forward)
                } else {
                    Err(ExecProtocolError::MalformedControl)
                }
            );
            let large = "x".repeat(65 * 1024);
            assert_eq!(
                parse_sprite_ctrl(&large, tty),
                if tty {
                    Ok(SpriteCtrl::Forward)
                } else {
                    Err(ExecProtocolError::MalformedControl)
                }
            );
        }
    }

    #[test]
    fn send_initial_size_queues_one_resize() {
        let (frames_tx, frames_rx) = tokio::sync::mpsc::channel(1);
        let (control_tx, mut control_rx) = tokio::sync::mpsc::channel(4);
        let (_sid_tx, session_id) = tokio::sync::watch::channel(None::<String>);
        let session = ExecSession {
            frames: frames_rx,
            control: control_tx,
            session_id,
        };

        send_initial_size(&session, 120, 40);
        assert_eq!(
            control_rx.try_recv(),
            Ok(ExecControl::Resize {
                cols: 120,
                rows: 40
            })
        );
        send_initial_size(&session, 0, 40);
        send_initial_size(&session, 120, 0);
        assert!(matches!(
            control_rx.try_recv(),
            Err(tokio::sync::mpsc::error::TryRecvError::Empty)
        ));

        drop(frames_tx);
    }

    #[test]
    fn exec_wire_stats_marks_each_mib() {
        let start = tokio::time::Instant::now();
        let later = start + Duration::from_millis(37);
        let mut stats = ExecWireStats::new(start);
        let half_mib = 512 * 1024;

        assert_eq!(stats.inbound(half_mib, start), None);
        assert_eq!(
            stats.inbound(half_mib, later),
            Some((ExecWireStats::MARK, 37, 0))
        );
        assert_eq!(stats.inbound(half_mib, later), None);
        assert_eq!(stats.in_msgs, 3);
        assert_eq!(stats.in_bytes, 3 * half_mib as u64);
        assert_eq!(stats.max_in, half_mib);

        stats.outbound(17);
        assert_eq!(stats.out_msgs, 1);
        assert_eq!(stats.out_bytes, 17);
    }

    #[tokio::test]
    async fn streaming_exec_refuses_a_tty_spec() {
        let provider = sprites();
        let spec = ExecSpec {
            argv: vec!["/bin/true".into()],
            tty: true,
            cols: 80,
            rows: 24,
            env: Vec::new(),
            cwd: None,
        };
        let result = provider.open_exec_streaming("sprite", &spec).await;
        let error = match result {
            Err(error) => error,
            Ok(_) => panic!("a tty spec must be refused"),
        };
        assert_eq!(error.to_string(), "sprites: a streaming exec is non-tty");
    }

    #[test]
    fn exec_wire_stats_accumulates_consumer_wait() {
        let start = tokio::time::Instant::now();
        let mut stats = ExecWireStats::new(start);

        stats.waited(Duration::from_millis(7));
        assert_eq!(
            stats.inbound(ExecWireStats::MARK as usize, start),
            Some((ExecWireStats::MARK, 0, 7))
        );
        stats.waited(Duration::from_millis(11));
        assert_eq!(
            stats.inbound(ExecWireStats::MARK as usize, start),
            Some((ExecWireStats::MARK, 0, 11))
        );
        assert_eq!(stats.consumer_wait, Duration::from_millis(18));
        assert_eq!(stats.mark_wait, Duration::ZERO);
    }

    #[tokio::test]
    async fn provider_output_is_chunked_to_the_item_bound() {
        let (tx, mut rx) = tokio::sync::mpsc::channel(8);
        assert!(send_chunked(&tx, vec![7u8; 2 * EXEC_DATA_CHUNK + 1], ExecFrame::Stdout).await);
        for want in [EXEC_DATA_CHUNK, EXEC_DATA_CHUNK, 1] {
            match rx.recv().await {
                Some(ExecFrame::Stdout(b)) => assert_eq!(b.len(), want),
                other => panic!("unexpected frame {other:?}"),
            }
        }
    }

    #[tokio::test]
    async fn provider_slow_consumer_backpressures_instead_of_ending() {
        let (tx, mut rx) = tokio::sync::mpsc::channel::<Vec<u8>>(1);
        let task = tokio::spawn(async move {
            send_chunked(&tx, vec![1u8; 3 * EXEC_DATA_CHUNK], std::convert::identity).await
        });
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        assert!(!task.is_finished());
        for _ in 0..3 {
            assert_eq!(rx.recv().await.map(|b| b.len()), Some(EXEC_DATA_CHUNK));
        }
        assert!(task.await.unwrap());
    }

    #[tokio::test]
    async fn provider_output_reports_a_gone_receiver() {
        let (tx, rx) = tokio::sync::mpsc::channel::<Vec<u8>>(1);
        drop(rx);
        assert!(!send_chunked(&tx, vec![1u8; 4], std::convert::identity).await);
    }

    async fn fake_non_tty_output(stderr: &[u8], stdout: &[u8], code: i32) -> String {
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
        let stderr_tail = Arc::new(Mutex::new(StderrTail::default()));
        let driver = tokio::spawn(drive_exec(
            client,
            false,
            frames_tx,
            control_rx,
            sid_tx,
            Some(Arc::clone(&stderr_tail)),
            None,
        ));

        let mut stderr_frame = Vec::with_capacity(stderr.len() + 1);
        stderr_frame.push(2);
        stderr_frame.extend_from_slice(stderr);
        server
            .send(Message::Binary(stderr_frame.into()))
            .await
            .unwrap();
        let mut stdout_frame = Vec::with_capacity(stdout.len() + 1);
        stdout_frame.push(1);
        stdout_frame.extend_from_slice(stdout);
        server
            .send(Message::Binary(stdout_frame.into()))
            .await
            .unwrap();
        let exit_frame = vec![0x03, code as u8];
        server
            .send(Message::Binary(exit_frame.into()))
            .await
            .unwrap();

        let collected =
            collect_exec_output(&mut frames_rx, crate::vps::ssh_shim::EXEC_OUTPUT_CAP).await;
        driver.await.unwrap();
        let code = collected.code.expect("fake exec sent an exit");
        let output =
            crate::vps::ssh_shim::render_capped_output(&collected.output, collected.dropped);
        append_stderr_tail(output, code, &stderr_tail)
    }

    #[tokio::test]
    async fn nonzero_exit_appends_sanitized_stderr_tail() {
        let output = fake_non_tty_output(b"err\x1b[31m\r\n", b"stdout", 7).await;
        assert_eq!(output, "stdout\n[stderr, last 10 bytes]\nerr[31m\n");
    }

    #[tokio::test]
    async fn zero_exit_output_is_unchanged_with_stderr() {
        let output = fake_non_tty_output(b"warning\n", b"stdout", 0).await;
        assert_eq!(output, "stdout");
    }

    #[test]
    fn stderr_tail_keeps_the_last_8_kib() {
        let mut tail = StderrTail::default();
        tail.push(&vec![b'a'; STDERR_TAIL_CAP]);
        tail.push(b"tail");

        assert_eq!(tail.bytes.len(), STDERR_TAIL_CAP);
        assert_eq!(tail.total, (STDERR_TAIL_CAP + 4) as u64);
        assert!(tail.render().ends_with("tail"));
        assert_eq!(tail.render().len(), STDERR_TAIL_CAP);
    }

    #[test]
    fn sanitize_stderr_drops_escapes_and_controls() {
        assert_eq!(
            sanitize_stderr(b"ok\x00\x01\x08\x0b\x0c\r\x1b[31m\n\t\xff"),
            "ok[31m\n\t�"
        );
    }

    async fn drive_exec_messages(
        tty: bool,
        messages: Vec<tokio_tungstenite::tungstenite::Message>,
        close_stdin: bool,
    ) -> (
        Vec<ExecFrame>,
        Option<ExecProtocolError>,
        Option<tokio_tungstenite::tungstenite::Message>,
    ) {
        use futures_util::SinkExt;
        use tokio_tungstenite::tungstenite::{Message, protocol::Role};

        let (client_io, server_io) = tokio::io::duplex(64 * 1024);
        let client =
            tokio_tungstenite::WebSocketStream::from_raw_socket(client_io, Role::Client, None)
                .await;
        let mut server =
            tokio_tungstenite::WebSocketStream::from_raw_socket(server_io, Role::Server, None)
                .await;
        let (frames_tx, mut frames_rx) = tokio::sync::mpsc::channel(16);
        let (control_tx, control_rx) = tokio::sync::mpsc::channel(4);
        let _control_guard = (!close_stdin).then_some(control_tx);
        let (sid_tx, _sid_rx) = tokio::sync::watch::channel(None);
        let driver = tokio::spawn(drive_exec(
            client, tty, frames_tx, control_rx, sid_tx, None, None,
        ));

        if close_stdin {
            assert_eq!(
                next_non_ping(&mut server).await,
                Some(Message::Binary(vec![4].into()))
            );
        }
        for message in messages {
            if server.send(message).await.is_err() {
                break;
            }
        }
        let client_close = if close_stdin {
            tokio::time::timeout(Duration::from_secs(1), next_non_ping(&mut server))
                .await
                .ok()
                .flatten()
        } else {
            None
        };
        if server.send(Message::Close(None)).await.is_err() {
            tracing::debug!(target: "thegn::provider", "test server close could not be sent");
        }

        let mut frames = Vec::new();
        while let Some(frame) = frames_rx.recv().await {
            frames.push(frame);
        }
        let fault = driver.await.unwrap();
        (frames, fault, client_close)
    }

    #[tokio::test]
    async fn drive_exec_non_tty_half_closes_stdin_then_drains_exact_exit() {
        use tokio_tungstenite::tungstenite::Message;

        let (frames, fault, client_close) = drive_exec_messages(
            false,
            vec![
                Message::Binary([vec![1], b"recorded output".to_vec()].concat().into()),
                Message::Binary(vec![3, 7].into()),
            ],
            true,
        )
        .await;
        assert_eq!(
            frames,
            vec![
                ExecFrame::Stdout(b"recorded output".to_vec()),
                ExecFrame::Exit(7),
            ]
        );
        assert_eq!(fault, None);
        assert_eq!(client_close, Some(Message::Close(None)));
    }

    async fn next_non_ping<S>(
        server: &mut tokio_tungstenite::WebSocketStream<S>,
    ) -> Option<tokio_tungstenite::tungstenite::Message>
    where
        S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
    {
        use futures_util::StreamExt;
        use tokio_tungstenite::tungstenite::Message;

        loop {
            match server.next().await {
                None => return None,
                Some(Ok(Message::Ping(_))) | Some(Ok(Message::Pong(_))) => {}
                Some(Ok(message)) => return Some(message),
                Some(Err(_)) => {}
            }
        }
    }

    #[tokio::test(start_paused = true)]
    async fn drive_exec_probe_without_pong_keeps_a_quiet_session() {
        use tokio_tungstenite::tungstenite::protocol::Role;

        let (client_io, server_io) = tokio::io::duplex(64 * 1024);
        let client =
            tokio_tungstenite::WebSocketStream::from_raw_socket(client_io, Role::Client, None)
                .await;
        let _server =
            tokio_tungstenite::WebSocketStream::from_raw_socket(server_io, Role::Server, None)
                .await;
        let (frames_tx, _frames_rx) = tokio::sync::mpsc::channel(4);
        let (control_tx, control_rx) = tokio::sync::mpsc::channel(4);
        let (sid_tx, _sid_rx) = tokio::sync::watch::channel(None);
        let driver = tokio::spawn(drive_exec(
            client, true, frames_tx, control_rx, sid_tx, None, None,
        ));

        control_tx
            .send(ExecControl::Stdin(b"x".to_vec()))
            .await
            .unwrap();
        tokio::task::yield_now().await;
        tokio::time::advance(Duration::from_secs(6)).await;
        tokio::task::yield_now().await;

        assert!(!driver.is_finished());
        control_tx.send(ExecControl::Close).await.unwrap();
        assert_eq!(driver.await.unwrap(), None);
    }

    #[tokio::test(start_paused = true)]
    async fn drive_exec_probe_after_initial_pong_ends_silent_session() {
        use futures_util::{SinkExt, StreamExt};
        use tokio_tungstenite::tungstenite::{Message, protocol::Role};

        let (client_io, server_io) = tokio::io::duplex(64 * 1024);
        let client =
            tokio_tungstenite::WebSocketStream::from_raw_socket(client_io, Role::Client, None)
                .await;
        let mut server =
            tokio_tungstenite::WebSocketStream::from_raw_socket(server_io, Role::Server, None)
                .await;
        let (frames_tx, _frames_rx) = tokio::sync::mpsc::channel(4);
        let (control_tx, control_rx) = tokio::sync::mpsc::channel(4);
        let (sid_tx, _sid_rx) = tokio::sync::watch::channel(None);
        let driver = tokio::spawn(drive_exec(
            client, true, frames_tx, control_rx, sid_tx, None, None,
        ));

        assert!(matches!(
            server.next().await.unwrap().unwrap(),
            Message::Ping(_)
        ));
        server
            .send(Message::Pong(b"thegn".to_vec().into()))
            .await
            .unwrap();
        for _ in 0..4 {
            tokio::task::yield_now().await;
        }

        control_tx
            .send(ExecControl::Stdin(b"x".to_vec()))
            .await
            .unwrap();
        assert_eq!(
            next_non_ping(&mut server).await.unwrap(),
            Message::Binary(b"x".to_vec().into())
        );

        tokio::time::advance(Duration::from_secs(5)).await;
        for _ in 0..4 {
            tokio::task::yield_now().await;
        }
        assert!(matches!(
            server.next().await.unwrap().unwrap(),
            Message::Ping(_)
        ));
        tokio::time::advance(Duration::from_secs(5)).await;
        for _ in 0..4 {
            tokio::task::yield_now().await;
        }

        assert!(driver.is_finished());
        assert_eq!(driver.await.unwrap(), None);
    }

    #[tokio::test(start_paused = true)]
    async fn drive_exec_probe_is_disarmed_by_any_inbound_frame() {
        use futures_util::{SinkExt, StreamExt};
        use tokio_tungstenite::tungstenite::{Message, protocol::Role};

        let (client_io, server_io) = tokio::io::duplex(64 * 1024);
        let client =
            tokio_tungstenite::WebSocketStream::from_raw_socket(client_io, Role::Client, None)
                .await;
        let mut server =
            tokio_tungstenite::WebSocketStream::from_raw_socket(server_io, Role::Server, None)
                .await;
        let (frames_tx, mut frames_rx) = tokio::sync::mpsc::channel(4);
        let (control_tx, control_rx) = tokio::sync::mpsc::channel(4);
        let (sid_tx, _sid_rx) = tokio::sync::watch::channel(None);
        let driver = tokio::spawn(drive_exec(
            client, true, frames_tx, control_rx, sid_tx, None, None,
        ));

        assert!(matches!(
            server.next().await.unwrap().unwrap(),
            Message::Ping(_)
        ));
        server
            .send(Message::Pong(b"thegn".to_vec().into()))
            .await
            .unwrap();
        for _ in 0..4 {
            tokio::task::yield_now().await;
        }
        control_tx
            .send(ExecControl::Stdin(b"x".to_vec()))
            .await
            .unwrap();
        assert_eq!(
            next_non_ping(&mut server).await.unwrap(),
            Message::Binary(vec![b'x'].into())
        );
        server
            .send(Message::Binary(vec![b'a'].into()))
            .await
            .unwrap();
        assert_eq!(
            frames_rx.recv().await,
            Some(ExecFrame::Stdout(b"a".to_vec()))
        );

        for _ in 0..6 {
            tokio::time::advance(Duration::from_secs(5)).await;
            tokio::task::yield_now().await;
        }
        tokio::task::yield_now().await;
        assert!(!driver.is_finished());
        assert!(
            control_tx
                .send(ExecControl::Stdin(b"y".to_vec()))
                .await
                .is_ok()
        );

        control_tx.send(ExecControl::Close).await.unwrap();
        assert_eq!(driver.await.unwrap(), None);
    }

    #[tokio::test(start_paused = true)]
    async fn drive_exec_probe_answered_by_pong_keeps_a_quiet_session() {
        use tokio_tungstenite::tungstenite::protocol::Role;

        let (client_io, server_io) = tokio::io::duplex(64 * 1024);
        let client =
            tokio_tungstenite::WebSocketStream::from_raw_socket(client_io, Role::Client, None)
                .await;
        let mut server =
            tokio_tungstenite::WebSocketStream::from_raw_socket(server_io, Role::Server, None)
                .await;
        let server_task =
            tokio::spawn(async move { while next_non_ping(&mut server).await.is_some() {} });
        let (frames_tx, _frames_rx) = tokio::sync::mpsc::channel(4);
        let (control_tx, control_rx) = tokio::sync::mpsc::channel(4);
        let (sid_tx, _sid_rx) = tokio::sync::watch::channel(None);
        let driver = tokio::spawn(drive_exec(
            client, true, frames_tx, control_rx, sid_tx, None, None,
        ));

        control_tx
            .send(ExecControl::Stdin(b"x".to_vec()))
            .await
            .unwrap();
        for _ in 0..12 {
            tokio::time::advance(Duration::from_secs(5)).await;
            for _ in 0..4 {
                tokio::task::yield_now().await;
            }
        }
        assert!(!driver.is_finished());

        control_tx.send(ExecControl::Close).await.unwrap();
        assert_eq!(driver.await.unwrap(), None);
        server_task.abort();
    }

    #[tokio::test(start_paused = true)]
    async fn drive_exec_non_tty_never_arms_probe() {
        use futures_util::StreamExt;
        use tokio_tungstenite::tungstenite::{Message, protocol::Role};

        let (client_io, server_io) = tokio::io::duplex(64 * 1024);
        let client =
            tokio_tungstenite::WebSocketStream::from_raw_socket(client_io, Role::Client, None)
                .await;
        let mut server =
            tokio_tungstenite::WebSocketStream::from_raw_socket(server_io, Role::Server, None)
                .await;
        let (frames_tx, _frames_rx) = tokio::sync::mpsc::channel(4);
        let (control_tx, control_rx) = tokio::sync::mpsc::channel(4);
        let (sid_tx, _sid_rx) = tokio::sync::watch::channel(None);
        let driver = tokio::spawn(drive_exec(
            client, false, frames_tx, control_rx, sid_tx, None, None,
        ));

        assert!(matches!(
            server.next().await.unwrap().unwrap(),
            Message::Ping(_)
        ));
        control_tx
            .send(ExecControl::Stdin(b"x".to_vec()))
            .await
            .unwrap();
        assert_eq!(
            server.next().await.unwrap().unwrap(),
            Message::Binary(vec![0, b'x'].into())
        );

        let observed = tokio::time::timeout(Duration::from_secs(60), server.next());
        tokio::pin!(observed);
        tokio::task::yield_now().await;
        tokio::time::advance(Duration::from_secs(60)).await;
        assert!(
            observed.await.is_err(),
            "non-tty exec armed a liveness probe"
        );

        control_tx.send(ExecControl::Close).await.unwrap();
        assert_eq!(driver.await.unwrap(), None);
    }

    #[tokio::test]
    async fn drive_exec_duplicate_exit_is_a_protocol_error() {
        use tokio_tungstenite::tungstenite::Message;

        let (frames, fault, _) = drive_exec_messages(
            false,
            vec![
                Message::Binary(vec![3, 0].into()),
                Message::Binary(vec![3, 1].into()),
            ],
            false,
        )
        .await;
        assert_eq!(frames, vec![ExecFrame::Exit(0)]);
        assert_eq!(fault, Some(ExecProtocolError::DuplicateExit));
    }

    #[tokio::test]
    async fn drive_exec_accepts_the_exit_repeated_in_both_forms() {
        use tokio_tungstenite::tungstenite::Message;

        for messages in [
            vec![
                Message::Binary(vec![1, b'o', b'k'].into()),
                Message::Binary(vec![3, 7].into()),
                Message::Text(r#"{"type":"exit","exit_code":7}"#.into()),
            ],
            vec![
                Message::Binary(vec![1, b'o', b'k'].into()),
                Message::Text(r#"{"type":"exit","exit_code":7}"#.into()),
                Message::Binary(vec![3, 7].into()),
            ],
        ] {
            let (frames, fault, _) = drive_exec_messages(false, messages, false).await;
            assert_eq!(
                frames,
                vec![ExecFrame::Stdout(b"ok".to_vec()), ExecFrame::Exit(7)]
            );
            assert_eq!(fault, None);
        }
    }

    #[tokio::test]
    async fn drive_exec_output_after_exit_is_a_protocol_error() {
        use tokio_tungstenite::tungstenite::Message;

        let (frames, fault, _) = drive_exec_messages(
            false,
            vec![
                Message::Binary(vec![3, 0].into()),
                Message::Binary(vec![1, b'x'].into()),
            ],
            false,
        )
        .await;
        assert_eq!(frames, vec![ExecFrame::Exit(0)]);
        assert_eq!(fault, Some(ExecProtocolError::OutputAfterExit));

        let (frames, fault, _) = drive_exec_messages(
            true,
            vec![
                Message::Text(r#"{"type":"exit","exit_code":0}"#.into()),
                Message::Binary(b"x".to_vec().into()),
            ],
            false,
        )
        .await;
        assert_eq!(frames, vec![ExecFrame::Exit(0)]);
        assert_eq!(fault, Some(ExecProtocolError::OutputAfterExit));
    }

    #[tokio::test]
    async fn drive_exec_port_closed_after_exit_is_accepted() {
        use tokio_tungstenite::tungstenite::Message;

        let (frames, fault, _) = drive_exec_messages(
            true,
            vec![
                Message::Text(r#"{"type":"exit","exit_code":0}"#.into()),
                Message::Text(r#"{"type":"port_closed","port":8080}"#.into()),
            ],
            false,
        )
        .await;
        assert_eq!(frames, vec![ExecFrame::Exit(0)]);
        assert_eq!(fault, None);
    }

    #[tokio::test]
    async fn drive_exec_ignores_an_unknown_control_type() {
        use tokio_tungstenite::tungstenite::Message;

        let (frames, fault, _) = drive_exec_messages(
            false,
            vec![
                Message::Text(r#"{"type":"debug","message":"x"}"#.into()),
                Message::Binary(vec![1, b'o', b'k'].into()),
                Message::Binary(vec![3, 0].into()),
            ],
            false,
        )
        .await;
        assert_eq!(
            frames,
            vec![ExecFrame::Stdout(b"ok".to_vec()), ExecFrame::Exit(0)]
        );
        assert_eq!(fault, None);
    }

    #[tokio::test]
    async fn drive_exec_unknown_stream_id_is_refused() {
        use tokio_tungstenite::tungstenite::Message;

        let (frames, fault, _) =
            drive_exec_messages(false, vec![Message::Binary(vec![5, b'x'].into())], false).await;
        assert!(frames.is_empty());
        assert_eq!(fault, Some(ExecProtocolError::UnknownStream(5)));
    }

    #[tokio::test]
    async fn drive_exec_does_not_assume_line_boundaries() {
        use tokio_tungstenite::tungstenite::Message;

        let (frames, fault, _) = drive_exec_messages(
            false,
            vec![
                Message::Binary([vec![1], b"a\nb\nc".to_vec()].concat().into()),
                Message::Binary([vec![1], b"par".to_vec()].concat().into()),
                Message::Binary([vec![1], b"tial\n".to_vec()].concat().into()),
                Message::Binary(vec![1].into()),
                Message::Binary(vec![3, 0].into()),
            ],
            false,
        )
        .await;
        let output: Vec<u8> = frames
            .iter()
            .filter_map(|frame| match frame {
                ExecFrame::Stdout(bytes) => Some(bytes.as_slice()),
                ExecFrame::Exit(_) => None,
            })
            .flatten()
            .copied()
            .collect();
        assert_eq!(output, b"a\nb\ncpartial\n");
        assert_eq!(frames.last(), Some(&ExecFrame::Exit(0)));
        assert_eq!(fault, None);
    }

    #[tokio::test]
    async fn drive_exec_exit_drain_is_bounded() {
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
        let driver = tokio::spawn(drive_exec(
            client, false, frames_tx, control_rx, sid_tx, None, None,
        ));
        server
            .send(Message::Binary(vec![3, 0].into()))
            .await
            .unwrap();
        assert_eq!(frames_rx.recv().await, Some(ExecFrame::Exit(0)));
        let result = tokio::time::timeout(EXEC_EXIT_DRAIN + Duration::from_secs(2), driver).await;
        assert_eq!(result.unwrap().unwrap(), None);
    }

    #[tokio::test]
    async fn drive_exec_reports_the_fault_on_done() {
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
        let (done_tx, done_rx) = tokio::sync::oneshot::channel();
        let driver = tokio::spawn(drive_exec(
            client,
            false,
            frames_tx,
            control_rx,
            sid_tx,
            None,
            Some(done_tx),
        ));
        server
            .send(Message::Binary(vec![3, 0].into()))
            .await
            .unwrap();
        server
            .send(Message::Binary(vec![3, 1].into()))
            .await
            .unwrap();
        if server.send(Message::Close(None)).await.is_err() {
            tracing::debug!(target: "thegn::provider", "test server close could not be sent");
        }
        assert_eq!(frames_rx.recv().await, Some(ExecFrame::Exit(0)));
        assert_eq!(frames_rx.recv().await, None);
        assert_eq!(
            done_rx.await.unwrap(),
            Some(ExecProtocolError::DuplicateExit)
        );
        assert_eq!(
            driver.await.unwrap(),
            Some(ExecProtocolError::DuplicateExit)
        );
    }

    #[tokio::test]
    async fn drive_exec_sends_stdin_eof_exactly_once_on_control_close() {
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
        let (control_tx, control_rx) = tokio::sync::mpsc::channel(4);
        let (sid_tx, _sid_rx) = tokio::sync::watch::channel(None);
        let driver = tokio::spawn(drive_exec(
            client, false, frames_tx, control_rx, sid_tx, None, None,
        ));
        drop(control_tx);

        assert_eq!(
            next_non_ping(&mut server).await.unwrap(),
            Message::Binary(vec![4].into())
        );
        assert!(!driver.is_finished());
        server
            .send(Message::Binary(vec![3, 0].into()))
            .await
            .unwrap();
        assert_eq!(frames_rx.recv().await, Some(ExecFrame::Exit(0)));
        server.send(Message::Close(None)).await.unwrap();
        driver.await.unwrap();
    }

    #[tokio::test]
    async fn drive_exec_tty_keeps_reading_after_control_close() {
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
        let (control_tx, control_rx) = tokio::sync::mpsc::channel(4);
        let (sid_tx, _sid_rx) = tokio::sync::watch::channel(None);
        let driver = tokio::spawn(drive_exec(
            client, true, frames_tx, control_rx, sid_tx, None, None,
        ));
        drop(control_tx);

        server
            .send(Message::Binary(b"ok".to_vec().into()))
            .await
            .unwrap();
        assert_eq!(
            frames_rx.recv().await,
            Some(ExecFrame::Stdout(b"ok".to_vec()))
        );
        server
            .send(Message::Text(r#"{"type":"exit","exit_code":0}"#.into()))
            .await
            .unwrap();
        assert_eq!(frames_rx.recv().await, Some(ExecFrame::Exit(0)));
        server.send(Message::Close(None)).await.unwrap();
        driver.await.unwrap();
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
        let driver = tokio::spawn(drive_exec(
            client, true, frames_tx, control_rx, sid_tx, None, None,
        ));

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
    async fn drive_exec_tty_forwards_non_json_text_frames() {
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
        let driver = tokio::spawn(drive_exec(
            client, true, frames_tx, control_rx, sid_tx, None, None,
        ));

        server
            .send(Message::Binary(b"/dev/pts/0".to_vec().into()))
            .await
            .unwrap();
        server.send(Message::Text("\r\n".into())).await.unwrap();
        server
            .send(Message::Binary(b"ok\r\n".to_vec().into()))
            .await
            .unwrap();
        server
            .send(Message::Text(r#"{"type":"exit","exit_code":0}"#.into()))
            .await
            .unwrap();

        let collected = collect_exec_output(&mut frames_rx, usize::MAX).await;
        assert_eq!(collected.code, Some(0));
        assert_eq!(collected.output, b"/dev/pts/0\r\nok\r\n");
        driver.await.unwrap();
    }

    #[tokio::test]
    async fn drive_exec_non_tty_refuses_non_json_text_frames() {
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
        let driver = tokio::spawn(drive_exec(
            client, false, frames_tx, control_rx, sid_tx, None, None,
        ));

        let mut first = vec![1];
        first.extend_from_slice(b"/dev/pts/0");
        server.send(Message::Binary(first.into())).await.unwrap();
        server.send(Message::Text("\r\n".into())).await.unwrap();
        assert_eq!(
            frames_rx.recv().await,
            Some(ExecFrame::Stdout(b"/dev/pts/0".to_vec()))
        );
        assert_eq!(frames_rx.recv().await, None);
        assert_eq!(
            driver.await.unwrap(),
            Some(ExecProtocolError::MalformedControl)
        );
    }

    #[tokio::test]
    async fn drive_exec_tty_does_not_forward_json_text_frames() {
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
        let driver = tokio::spawn(drive_exec(
            client, true, frames_tx, control_rx, sid_tx, None, None,
        ));

        server
            .send(Message::Text(
                r#"{"type":"session_info","session_id":"sess-1"}"#.into(),
            ))
            .await
            .unwrap();
        server
            .send(Message::Text(r#"{"type":"exit","exit_code":0}"#.into()))
            .await
            .unwrap();

        let collected = collect_exec_output(&mut frames_rx, usize::MAX).await;
        assert_eq!(collected.code, Some(0));
        assert!(collected.output.is_empty());
        driver.await.unwrap();
    }

    #[tokio::test]
    async fn drive_proxy_reports_a_peer_close_as_a_clean_end() {
        use futures_util::SinkExt;
        use tokio_tungstenite::tungstenite::{Message, protocol::Role};

        let (client_io, server_io) = tokio::io::duplex(64 * 1024);
        let client =
            tokio_tungstenite::WebSocketStream::from_raw_socket(client_io, Role::Client, None)
                .await;
        let mut server =
            tokio_tungstenite::WebSocketStream::from_raw_socket(server_io, Role::Server, None)
                .await;
        let (_to_srv_tx, to_srv_rx) = tokio::sync::mpsc::channel(4);
        let (from_srv_tx, mut from_srv_rx) = tokio::sync::mpsc::channel(4);
        let (end_tx, end_rx) = tokio::sync::oneshot::channel();
        let driver = tokio::spawn(drive_proxy(client, to_srv_rx, from_srv_tx, end_tx));

        server
            .send(Message::Binary(b"ok".to_vec().into()))
            .await
            .unwrap();
        server.send(Message::Close(None)).await.unwrap();

        assert_eq!(from_srv_rx.recv().await, Some(b"ok".to_vec()));
        assert_eq!(from_srv_rx.recv().await, None);
        let end = end_rx.await.unwrap();
        assert_eq!(end, ProxyEnd::PeerClosed);
        assert!(end.into_result().is_ok());
        driver.await.unwrap();
    }

    #[tokio::test]
    async fn drive_proxy_reports_an_abrupt_drop_as_transport_dropped() {
        use tokio_tungstenite::tungstenite::protocol::Role;

        let (client_io, server_io) = tokio::io::duplex(64 * 1024);
        let client =
            tokio_tungstenite::WebSocketStream::from_raw_socket(client_io, Role::Client, None)
                .await;
        let server =
            tokio_tungstenite::WebSocketStream::from_raw_socket(server_io, Role::Server, None)
                .await;
        let (_to_srv_tx, to_srv_rx) = tokio::sync::mpsc::channel(4);
        let (from_srv_tx, mut from_srv_rx) = tokio::sync::mpsc::channel(4);
        let (end_tx, end_rx) = tokio::sync::oneshot::channel();
        let driver = tokio::spawn(drive_proxy(client, to_srv_rx, from_srv_tx, end_tx));
        drop(server);

        assert_eq!(from_srv_rx.recv().await, None);
        let end = end_rx.await.unwrap();
        assert!(matches!(
            end,
            ProxyEnd::TransportDropped | ProxyEnd::TransportError(_)
        ));
        assert!(end.into_result().is_err());
        driver.await.unwrap();
    }

    #[tokio::test]
    async fn drive_proxy_reports_an_abnormal_close_code_as_a_rejection() {
        use futures_util::SinkExt;
        use tokio_tungstenite::tungstenite::{
            Message,
            protocol::{CloseFrame, Role, frame::coding::CloseCode},
        };

        let (client_io, server_io) = tokio::io::duplex(64 * 1024);
        let client =
            tokio_tungstenite::WebSocketStream::from_raw_socket(client_io, Role::Client, None)
                .await;
        let mut server =
            tokio_tungstenite::WebSocketStream::from_raw_socket(server_io, Role::Server, None)
                .await;
        let (_to_srv_tx, to_srv_rx) = tokio::sync::mpsc::channel(4);
        let (from_srv_tx, mut from_srv_rx) = tokio::sync::mpsc::channel(4);
        let (end_tx, end_rx) = tokio::sync::oneshot::channel();
        let driver = tokio::spawn(drive_proxy(client, to_srv_rx, from_srv_tx, end_tx));

        server
            .send(Message::Close(Some(CloseFrame {
                code: CloseCode::Policy,
                reason: "denied".into(),
            })))
            .await
            .unwrap();

        assert_eq!(from_srv_rx.recv().await, None);
        let end = end_rx.await.unwrap();
        assert_eq!(
            end,
            ProxyEnd::RemoteRejected {
                code: 1008,
                reason: "denied".to_string()
            }
        );
        assert!(end.into_result().unwrap_err().to_string().contains("1008"));
        driver.await.unwrap();
    }

    #[tokio::test]
    async fn drive_proxy_reports_a_local_close() {
        use tokio_tungstenite::tungstenite::protocol::Role;

        let (client_io, server_io) = tokio::io::duplex(64 * 1024);
        let client =
            tokio_tungstenite::WebSocketStream::from_raw_socket(client_io, Role::Client, None)
                .await;
        let _server =
            tokio_tungstenite::WebSocketStream::from_raw_socket(server_io, Role::Server, None)
                .await;
        let (to_srv_tx, to_srv_rx) = tokio::sync::mpsc::channel(4);
        let (from_srv_tx, _from_srv_rx) = tokio::sync::mpsc::channel(4);
        let (end_tx, end_rx) = tokio::sync::oneshot::channel();
        let driver = tokio::spawn(drive_proxy(client, to_srv_rx, from_srv_tx, end_tx));
        drop(to_srv_tx);

        assert_eq!(end_rx.await.unwrap(), ProxyEnd::LocalClosed);
        driver.await.unwrap();
    }

    #[test]
    fn proxy_end_into_result_is_ok_only_for_clean_ends() {
        let cases = [
            (ProxyEnd::LocalClosed, true),
            (ProxyEnd::PeerClosed, true),
            (
                ProxyEnd::RemoteRejected {
                    code: 1008,
                    reason: "denied".to_string(),
                },
                false,
            ),
            (ProxyEnd::TransportDropped, false),
            (ProxyEnd::TransportError("broken".to_string()), false),
        ];
        for (end, is_ok) in cases {
            assert_eq!(end.into_result().is_ok(), is_ok);
        }

        let long_reason = "é".repeat(1_000);
        let error = ProxyEnd::RemoteRejected {
            code: 1008,
            reason: long_reason,
        }
        .into_result()
        .unwrap_err()
        .to_string();
        assert!(error.len() <= 256);
        assert!(error.ends_with("é)"));

        let transport_error = ProxyEnd::TransportError("界".repeat(1_000))
            .into_result()
            .unwrap_err()
            .to_string();
        assert!(transport_error.len() <= 256);
        assert!(transport_error.ends_with('界'));
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
        let driver = tokio::spawn(drive_exec(
            client, true, frames_tx, control_rx, sid_tx, None, None,
        ));

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
        let driver = tokio::spawn(drive_exec(
            client, true, frames_tx, control_rx, sid_tx, None, None,
        ));

        server
            .send(Message::Text(r#"{"type":"exit","exit_code":3}"#.into()))
            .await
            .unwrap();

        assert_eq!(frames_rx.recv().await, Some(ExecFrame::Exit(3)));
        assert_eq!(frames_rx.recv().await, None);
        driver.await.unwrap();
    }

    #[tokio::test]
    async fn drive_exec_refuses_a_malformed_non_tty_exit() {
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
        let driver = tokio::spawn(drive_exec(
            client, false, frames_tx, control_rx, sid_tx, None, None,
        ));

        server
            .send(Message::Binary(vec![3, b'x', b'!'].into()))
            .await
            .unwrap();

        assert_eq!(frames_rx.recv().await, None);
        assert_eq!(
            driver.await.unwrap(),
            Some(ExecProtocolError::MalformedExit)
        );
    }

    #[test]
    fn rules_from_empty_is_allow_all() {
        assert!(rules_from(&[], &[]).unwrap().is_empty());
    }

    #[test]
    fn rules_from_block_only_denies_listed_no_default() {
        let r = rules_from(&[], &["evil.com".into()]).unwrap();
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
        )
        .unwrap();
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
    fn rules_from_canonicalizes_case_trailing_dot_and_duplicates() {
        assert_eq!(
            rules_from(&["GitHub.COM.".into(), "github.com".into()], &[]).unwrap(),
            vec![
                PolicyRule {
                    domain: "github.com".into(),
                    action: "allow".into(),
                },
                PolicyRule {
                    domain: "*".into(),
                    action: "deny".into(),
                },
            ]
        );
    }

    #[test]
    fn rules_from_drops_an_allow_shadowed_by_the_same_block() {
        assert_eq!(
            rules_from(&["a.test".into()], &["A.test".into()]).unwrap(),
            vec![
                PolicyRule {
                    domain: "a.test".into(),
                    action: "deny".into(),
                },
                PolicyRule {
                    domain: "*".into(),
                    action: "deny".into(),
                },
            ]
        );
    }

    #[test]
    fn rules_from_refuses_non_ascii_ip_and_misplaced_wildcards() {
        assert!(matches!(
            rules_from(&["bücher.example".into()], &[]),
            Err(PolicyRuleError::NonAscii { .. })
        ));
        for entry in ["192.0.2.1", "2001:db8::1", "[2001:db8::1]"] {
            assert!(matches!(
                rules_from(&[entry.into()], &[]),
                Err(PolicyRuleError::IpLiteral { .. })
            ));
        }
        for entry in ["a.*.example", "*example.com", "*.", "a..b", "-a.example"] {
            assert!(matches!(
                rules_from(&[entry.into()], &[]),
                Err(PolicyRuleError::Malformed { .. })
            ));
        }
    }

    #[test]
    fn rules_from_refuses_domains_with_numeric_final_labels() {
        for entry in ["1.2.3", "0x7f.1", "*.example.123"] {
            assert!(
                matches!(
                    rules_from(&[entry.into()], &[]),
                    Err(PolicyRuleError::Malformed { .. })
                ),
                "accepted numeric final label in {entry:?}"
            );
        }
    }

    #[test]
    fn rules_from_keeps_star_and_wildcard_forms() {
        assert_eq!(
            rules_from(&[], &["*".into()]).unwrap(),
            vec![PolicyRule {
                domain: "*".into(),
                action: "deny".into(),
            }]
        );
        assert_eq!(
            rules_from(&["*.npmjs.org".into()], &[]).unwrap(),
            vec![
                PolicyRule {
                    domain: "*.npmjs.org".into(),
                    action: "allow".into(),
                },
                PolicyRule {
                    domain: "*".into(),
                    action: "deny".into(),
                },
            ]
        );
    }

    #[test]
    fn parse_policy_rules_refuses_missing_rules_and_malformed_elements() {
        for body in [
            serde_json::json!({}),
            serde_json::json!({"rules": null}),
            serde_json::json!({"rules": {}}),
            serde_json::json!({"rules": [{"domain": "a.test"}]}),
            serde_json::json!({"rules": [{"action": "allow"}]}),
            serde_json::json!({"rules": [{"domain": "a.test", "action": null}]}),
            serde_json::json!({"rules": [{"domain": "a.test", "action": "maybe"}]}),
            serde_json::json!({"rules": [1]}),
        ] {
            assert!(parse_policy_rules(&body).is_err(), "accepted {body}");
        }
        assert!(
            parse_policy_rules(&serde_json::json!({"rules": []}))
                .unwrap()
                .is_empty()
        );
        assert_eq!(
            parse_policy_rules(&serde_json::json!({"rules": [
                {"domain": "github.com", "action": "allow"},
                {"domain": "*", "action": "deny"}
            ]}))
            .unwrap()
            .len(),
            2
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
    fn stream_outcome_reads_error_complete_and_absent() {
        let body = "{\"type\":\"info\",\"data\":\"Creating checkpoint...\"}\n{\"type\":\"info\",\"data\":\"  ID: v1\"}\n{\"type\":\"complete\",\"data\":\"Checkpoint v1 created successfully\"}\n";
        assert_eq!(SpritesProvider::stream_outcome(body), Ok(true));

        let error = SpritesProvider::stream_outcome(
            r#"{"type":"error","error":"Checkpoint failed: disk full","time":"t"}"#,
        )
        .unwrap_err();
        assert!(error.contains("disk full"));
        assert_eq!(
            SpritesProvider::stream_outcome(
                "{\"type\":\"info\",\"data\":\"still working\"}\ngarbage",
            ),
            Ok(false)
        );
        let error = SpritesProvider::stream_outcome(
            "{\"type\":\"error\",\"error\":\"bad\\u0007news\",\"time\":\"t\"}",
        )
        .unwrap_err();
        assert_eq!(error, "badnews");
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
        let arr = serde_json::json!([{"id": "a"}, {"id": "b"}]);
        assert_eq!(DaytonaProvider::parse_list(&arr).unwrap(), vec!["a", "b"]);
        let env = serde_json::json!({"sandboxes": [{"id": "c"}]});
        assert_eq!(DaytonaProvider::parse_list(&env).unwrap(), vec!["c"]);
        assert!(DaytonaProvider::parse_list(&serde_json::json!([{"name": "no-id"}])).is_err());
        assert!(DaytonaProvider::parse_list(&serde_json::json!({})).is_err());
    }

    #[test]
    fn daytona_parse_list_is_strict() {
        assert!(DaytonaProvider::parse_list(&serde_json::json!({})).is_err());
        assert!(DaytonaProvider::parse_list(&serde_json::Value::Null).is_err());
        let malformed = DaytonaProvider::parse_list(&serde_json::json!([
            {"id": "a"},
            {"name": "no-id"}
        ]));
        assert!(malformed.unwrap_err().to_string().contains("element 1"));
        assert!(DaytonaProvider::parse_list(&serde_json::json!([{"id": ""}])).is_err());
        assert_eq!(
            DaytonaProvider::parse_list(&serde_json::json!([{"id": "a"}])).unwrap(),
            vec!["a"]
        );
        assert_eq!(
            DaytonaProvider::parse_list(&serde_json::json!({"sandboxes": [{"id": "c"}]})).unwrap(),
            vec!["c"]
        );
    }

    #[test]
    fn sprites_checkpoint_list_parse_is_strict() {
        let checkpoints = serde_json::json!([
            {"id": "cp-1", "create_time": "2025-01-01", "is_auto": true},
            {"id": "cp-2", "create_time": "2025-01-02", "is_auto": false}
        ]);
        let parsed = SpritesProvider::parse_checkpoint_page(&checkpoints).unwrap();
        assert_eq!(parsed.items.len(), 2);
        assert!(
            parsed
                .items
                .iter()
                .all(|checkpoint| checkpoint.label.is_none())
        );
        assert_eq!(parsed.next, None);
        assert!(
            SpritesProvider::parse_checkpoint_page(&serde_json::json!({"checkpoints": {}}))
                .is_err()
        );
        assert!(SpritesProvider::parse_checkpoint_page(&serde_json::json!([{"x": 1}])).is_err());
        assert!(SpritesProvider::parse_checkpoint_page(&serde_json::json!([{"id": ""}])).is_err());
    }

    #[test]
    fn sprites_checkpoint_list_refuses_has_more() {
        assert!(
            SpritesProvider::parse_checkpoint_page(&serde_json::json!({
                "checkpoints": [{"id": "cp-1"}],
                "has_more": true
            }))
            .unwrap_err()
            .to_string()
            .contains("has_more without next_continuation_token")
        );
        assert!(
            SpritesProvider::parse_checkpoint_page(&serde_json::json!({
                "checkpoints": [],
                "has_more": "no"
            }))
            .unwrap_err()
            .to_string()
            .contains("invalid has_more")
        );
        assert_eq!(
            SpritesProvider::parse_checkpoint_page(&serde_json::json!({
                "checkpoints": [{"id": "cp-1"}],
                "has_more": false
            }))
            .unwrap()
            .items
            .len(),
            1
        );
    }

    #[test]
    fn sprites_checkpoint_list_refuses_token_without_has_more() {
        let result = SpritesProvider::parse_checkpoint_page(&serde_json::json!({
            "checkpoints": [],
            "has_more": false,
            "next_continuation_token": "t1"
        }));
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("continuation token without has_more")
        );
    }

    #[tokio::test]
    async fn sprites_checkpoint_list_follows_continuation_pages() {
        let (base, server) = paged_list_server(vec![
            r#"{"checkpoints":[{"id":"cp-1"}],"has_more":true,"next_continuation_token":"t1"}"#
                .into(),
            r#"{"checkpoints":[{"id":"cp-2","label":"base"}],"has_more":false}"#.into(),
        ])
        .await;
        let checkpoints = SpritesProvider::new(&base, "tok", "sprite")
            .list_checkpoints("sprite")
            .await
            .unwrap();
        assert_eq!(
            checkpoints
                .iter()
                .map(|checkpoint| checkpoint.id.as_str())
                .collect::<Vec<_>>(),
            vec!["cp-1", "cp-2"]
        );
        assert_eq!(checkpoints[1].label.as_deref(), Some("base"));
        let requests = server.await.unwrap();
        let second = requests
            .iter()
            .position(|line| line.contains("continuation_token="))
            .expect("second page request must carry its continuation token");
        assert!(requests[second].contains("continuation_token=t1"));
        assert!(!requests[0].contains('?'));
    }

    #[tokio::test]
    async fn sprites_checkpoint_list_refuses_a_repeated_continuation_token() {
        let (base, server) = paged_list_server(vec![
            r#"{"checkpoints":[{"id":"cp-1"}],"has_more":true,"next_continuation_token":"t1"}"#
                .into(),
            r#"{"checkpoints":[{"id":"cp-2"}],"has_more":true,"next_continuation_token":"t1"}"#
                .into(),
        ])
        .await;
        let result = SpritesProvider::new(&base, "tok", "sprite")
            .list_checkpoints("sprite")
            .await;
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("repeated continuation token")
        );
        server.await.unwrap();
    }

    #[tokio::test]
    async fn sprites_checkpoint_list_refuses_a_duplicate_id_across_pages() {
        let (base, server) = paged_list_server(vec![
            r#"{"checkpoints":[{"id":"cp-1"}],"has_more":true,"next_continuation_token":"t1"}"#
                .into(),
            r#"{"checkpoints":[{"id":"cp-1"}],"has_more":false}"#.into(),
        ])
        .await;
        let result = SpritesProvider::new(&base, "tok", "sprite")
            .list_checkpoints("sprite")
            .await;
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("duplicate checkpoint id")
        );
        server.await.unwrap();
    }

    #[tokio::test]
    async fn sprites_checkpoint_list_refuses_the_page_cap() {
        let bodies = (0..crate::vps::MAX_INVENTORY_PAGES)
            .map(|page| {
                format!(
                    "{{\"checkpoints\":[{{\"id\":\"cp-{page}\"}}],\"has_more\":true,\"next_continuation_token\":\"t-{page}\"}}"
                )
            })
            .collect();
        let (base, server) = paged_list_server(bodies).await;
        let result = SpritesProvider::new(&base, "tok", "sprite")
            .list_checkpoints("sprite")
            .await;
        assert!(result.unwrap_err().to_string().contains("page limit"));
        server.await.unwrap();
    }

    #[tokio::test]
    async fn sprites_checkpoint_list_refuses_the_row_cap() {
        let checkpoints: Vec<_> = (0..=crate::vps::MAX_INVENTORY_ROWS)
            .map(|index| serde_json::json!({"id": format!("cp-{index}")}))
            .collect();
        let body = serde_json::json!({"checkpoints": checkpoints}).to_string();
        let (base, server) = list_response("200 OK", &body).await;
        let result = SpritesProvider::new(&base, "tok", "sprite")
            .list_checkpoints("sprite")
            .await;
        assert!(result.unwrap_err().to_string().contains("row limit"));
        server.await.unwrap();
    }

    #[tokio::test]
    async fn sprites_checkpoint_list_refuses_partial_inventory() {
        let (base, server) = list_response(
            "200 OK",
            r#"{"checkpoints":[{"id":"cp-1"}],"has_more":true}"#,
        )
        .await;
        let result = SpritesProvider::new(&base, "tok", "sprite")
            .list_checkpoints("sprite")
            .await;
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("partial inventory")
        );
        server.await.unwrap();
    }

    #[tokio::test]
    async fn daytona_list_refuses_a_non_success_status() {
        let (base, server) =
            list_response("500 Internal Server Error", r#"{"sandboxes":[]}"#).await;
        let error = DaytonaProvider::new(&base, "tok", "snap")
            .list()
            .await
            .unwrap_err();
        assert!(error.to_string().contains("500"));
        server.await.unwrap();
    }

    #[tokio::test]
    async fn sprites_checkpoint_list_refuses_a_non_success_status() {
        let (base, server) = list_response("403 Forbidden", "[]").await;
        let error = SpritesProvider::new(&base, "tok", "sprite")
            .list_checkpoints("sprite")
            .await
            .unwrap_err();
        assert!(error.to_string().contains("403"));
        server.await.unwrap();
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
    async fn control_client_deadline_bounds_delayed_response_headers() {
        use tokio::io::AsyncReadExt;

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let (release_tx, release_rx) = tokio::sync::oneshot::channel();
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = [0u8; 1024];
            let n = socket.read(&mut request).await.unwrap();
            assert!(n > 0);
            release_rx.await.unwrap();
        });

        let started = std::time::Instant::now();
        let result = provider_http_client_with_deadline(std::time::Duration::from_millis(300))
            .get(format!("http://{address}/"))
            .send()
            .await;
        let elapsed = started.elapsed();
        release_tx.send(()).unwrap();
        server.await.unwrap();

        let error = match result {
            Err(error) => error,
            Ok(_) => panic!("the client must time out while waiting for response headers"),
        };
        assert!(
            redacted_transport_error("test", "GET", &error)
                .to_string()
                .contains("timed out")
        );
        assert!(elapsed < std::time::Duration::from_secs(5));
    }

    #[tokio::test]
    async fn control_client_deadline_bounds_a_slow_body() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let (release_tx, mut release_rx) = tokio::sync::oneshot::channel();
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = [0u8; 1024];
            let n = socket.read(&mut request).await.unwrap();
            assert!(n > 0);
            socket
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 64\r\n\r\n")
                .await
                .unwrap();
            for _ in 0..64 {
                tokio::select! {
                    result = &mut release_rx => {
                        result.unwrap();
                        return;
                    }
                    _ = tokio::time::sleep(std::time::Duration::from_millis(100)) => {
                        if socket.write_all(b"x").await.is_err() {
                            break;
                        }
                    }
                }
            }
            release_rx.await.unwrap();
        });

        let started = std::time::Instant::now();
        let response = provider_http_client_with_deadline(std::time::Duration::from_millis(300))
            .get(format!("http://{address}/"))
            .send()
            .await
            .unwrap();
        let result = response_json(response, "test", "GET").await;
        let elapsed = started.elapsed();
        release_tx.send(()).unwrap();
        server.await.unwrap();

        let error = result.unwrap_err().to_string();
        assert!(error.contains("test: GET provider endpoint"));
        assert!(error.contains("timed out") || error.contains("response body failed"));
        assert!(elapsed < std::time::Duration::from_secs(5));
    }

    #[tokio::test]
    async fn control_client_returns_a_redirect_without_following_it() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let target = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let target_address = target.local_addr().unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let (release_tx, release_rx) = tokio::sync::oneshot::channel();
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = [0u8; 1024];
            let n = socket.read(&mut request).await.unwrap();
            assert!(n > 0);
            let response = format!(
                "HTTP/1.1 302 Found\r\nLocation: http://{target_address}/\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
            );
            socket.write_all(response.as_bytes()).await.unwrap();
            release_rx.await.unwrap();
        });

        // Redirects are already disabled by the production client policy; this is a positive control.
        let response = provider_http_client()
            .get(format!("http://{address}/"))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), reqwest::StatusCode::FOUND);
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(300), target.accept())
                .await
                .is_err()
        );
        release_tx.send(()).unwrap();
        server.await.unwrap();
    }

    #[tokio::test]
    async fn control_reader_refuses_a_declared_oversized_body_before_reading_it() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let (release_tx, release_rx) = tokio::sync::oneshot::channel();
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = [0u8; 1024];
            let n = socket.read(&mut request).await.unwrap();
            assert!(n > 0);
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n",
                CONTROL_BODY_LIMIT + 1
            );
            socket.write_all(response.as_bytes()).await.unwrap();
            release_rx.await.unwrap();
        });

        let started = std::time::Instant::now();
        let response = provider_http_client()
            .get(format!("http://{address}/"))
            .send()
            .await
            .unwrap();
        let result = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            response_json(response, "test", "GET"),
        )
        .await;
        let elapsed = started.elapsed();
        release_tx.send(()).unwrap();
        server.await.unwrap();

        let error = result
            .expect("a declared oversized body must be refused without waiting for bytes")
            .unwrap_err()
            .to_string();
        assert!(error.contains("response exceeded the"));
        assert!(error.contains("declared"));
        assert!(elapsed < std::time::Duration::from_secs(5));
    }

    #[tokio::test]
    async fn control_client_deadline_bounds_a_stalled_tls_handshake() {
        use tokio::io::AsyncReadExt;

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let (release_tx, release_rx) = tokio::sync::oneshot::channel();
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut client_hello = [0u8; 1024];
            let n = socket.read(&mut client_hello).await.unwrap();
            assert!(n > 0);
            release_rx.await.unwrap();
        });

        // Positive control: the configured total deadline already covers the TLS handshake.
        let started = std::time::Instant::now();
        let result = provider_http_client_with_deadline(std::time::Duration::from_millis(300))
            .get(format!("https://{address}/"))
            .send()
            .await;
        let elapsed = started.elapsed();
        release_tx.send(()).unwrap();
        server.await.unwrap();

        let error = match result {
            Err(error) => error,
            Ok(_) => panic!("the client must time out during the stalled TLS handshake"),
        };
        assert!(
            redacted_transport_error("test", "GET", &error)
                .to_string()
                .contains("timed out")
        );
        assert!(elapsed < std::time::Duration::from_secs(5));
    }

    #[tokio::test]
    async fn control_reader_reports_a_connection_cut_mid_body_as_a_transport_error() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = [0u8; 1024];
            let n = socket.read(&mut request).await.unwrap();
            assert!(n > 0);
            socket
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 64\r\n\r\n{\"partial\"")
                .await
                .unwrap();
        });

        let response = provider_http_client()
            .get(format!("http://{address}/"))
            .send()
            .await
            .unwrap();
        let result = response_json(response, "test", "GET").await;
        server.await.unwrap();

        let error = result.unwrap_err().to_string();
        assert!(error.contains("test: GET provider endpoint"));
        assert!(!error.contains("decode GET response JSON"));
    }

    #[tokio::test]
    async fn control_reader_returns_an_encoded_body_undecoded() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let encoded_body: &[u8] = b"\x1f\x8b\x08\x00not-really-gzip";
        assert_eq!(encoded_body.len(), 19);
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = [0u8; 1024];
            let n = socket.read(&mut request).await.unwrap();
            assert!(n > 0);
            socket
                .write_all(
                    b"HTTP/1.1 200 OK\r\nContent-Encoding: gzip\r\nContent-Length: 19\r\n\r\n\x1f\x8b\x08\x00not-really-gzip",
                )
                .await
                .unwrap();
        });

        // The client enables no decompression, so CONTROL_BODY_LIMIT bounds held bytes; if
        // decompression is enabled, this test must gain a decompressed-size ceiling.
        let response = provider_http_client()
            .get(format!("http://{address}/"))
            .send()
            .await
            .unwrap();
        let body = response_bytes(response, "test", "GET", CONTROL_BODY_LIMIT)
            .await
            .unwrap();
        server.await.unwrap();

        assert_eq!(body.as_slice(), encoded_body);
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

    #[tokio::test]
    async fn collect_exec_output_keeps_the_tail_and_counts_dropped_bytes() {
        let (tx, mut rx) = tokio::sync::mpsc::channel(8);
        let sender = tokio::spawn(async move {
            tx.send(ExecFrame::Stdout(b"0123".to_vec())).await.unwrap();
            tx.send(ExecFrame::Stdout(b"456789".to_vec()))
                .await
                .unwrap();
            tx.send(ExecFrame::Exit(3)).await.unwrap();
        });

        let collected = collect_exec_output(&mut rx, 4).await;
        sender.await.unwrap();
        assert_eq!(collected.code, Some(3));
        assert_eq!(collected.output, b"6789");
        assert_eq!(collected.dropped, 6);
        assert!(collected.output.len() <= 4);
    }

    #[tokio::test]
    async fn collect_exec_output_without_exit_reports_none() {
        let (tx, mut rx) = tokio::sync::mpsc::channel(8);
        let sender = tokio::spawn(async move {
            tx.send(ExecFrame::Stdout(b"kept".to_vec())).await.unwrap();
        });

        let collected = collect_exec_output(&mut rx, 8).await;
        sender.await.unwrap();
        assert_eq!(collected.code, None);
        assert_eq!(collected.output, b"kept");
        assert_eq!(collected.dropped, 0);
    }

    #[tokio::test]
    async fn collect_exec_output_stops_at_exit() {
        let (tx, mut rx) = tokio::sync::mpsc::channel(8);
        let sender = tokio::spawn(async move {
            tx.send(ExecFrame::Exit(0)).await.unwrap();
            tx.send(ExecFrame::Stdout(b"late".to_vec())).await.unwrap();
        });

        let collected = collect_exec_output(&mut rx, 8).await;
        sender.await.unwrap();
        assert_eq!(collected.code, Some(0));
        assert_eq!(collected.output, b"");
        assert_eq!(rx.try_recv().unwrap(), ExecFrame::Stdout(b"late".to_vec()));
    }

    #[tokio::test(start_paused = true)]
    async fn collect_exec_output_until_returns_none_at_deadline_without_exit() {
        let (tx, mut rx) = tokio::sync::mpsc::channel(8);
        tx.send(ExecFrame::Stdout(b"kept".to_vec())).await.unwrap();
        let until = tokio::time::Instant::now() + Duration::from_secs(2);
        assert!(collect_exec_output_until(&mut rx, 8, until).await.is_none());
        drop(tx);
    }

    #[tokio::test(start_paused = true)]
    async fn collect_exec_output_until_returns_exit_before_deadline() {
        let (tx, mut rx) = tokio::sync::mpsc::channel(8);
        tx.send(ExecFrame::Exit(0)).await.unwrap();
        let until = tokio::time::Instant::now() + Duration::from_secs(2);
        let collected = collect_exec_output_until(&mut rx, 8, until)
            .await
            .expect("exit arrived before deadline");
        assert_eq!(collected.code, Some(0));
        drop(tx);
    }

    #[test]
    fn run_exec_deadline_exceeds_host_provision_step_timeout() {
        // The host's provision_step_timeout ceiling is 1800 s; one-shot exec
        // has a larger overall deadline so the host's shorter timeout wins.
        const _: () = assert!(RUN_EXEC_DEADLINE.as_secs() > 1800);
    }

    struct SwappingProvider {
        root: std::path::PathBuf,
        outside: std::path::PathBuf,
    }

    struct ManyDirectoriesProvider;

    impl ProviderFiles for ManyDirectoriesProvider {
        fn read<'a>(&'a self, _id: &'a str, _path: &'a str) -> BoxFuture<'a, Result<Vec<u8>>> {
            Box::pin(async { Ok(Vec::new()) })
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
            let entries = if path.is_empty() {
                (0..300)
                    .map(|index| FileEntry {
                        name: format!("directory-{index}"),
                        is_dir: true,
                        size: 0,
                    })
                    .collect()
            } else {
                Vec::new()
            };
            Box::pin(async move { Ok(entries) })
        }

        fn delete<'a>(&'a self, _id: &'a str, _path: &'a str) -> BoxFuture<'a, Result<()>> {
            Box::pin(async { Ok(()) })
        }
    }

    impl ProviderFiles for SwappingProvider {
        fn read<'a>(&'a self, _id: &'a str, _path: &'a str) -> BoxFuture<'a, Result<Vec<u8>>> {
            Box::pin(async move {
                let nested = self.root.join("nested");
                std::fs::rename(&nested, self.root.join("moved"))?;
                thegn_core::fsperm::create_symlink(&self.outside, &nested, true)?;
                Ok(b"data".to_vec())
            })
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
                vec![FileEntry {
                    name: "file".into(),
                    is_dir: false,
                    size: 4,
                }]
            } else {
                vec![FileEntry {
                    name: "nested".into(),
                    is_dir: true,
                    size: 0,
                }]
            };
            Box::pin(async move { Ok(entries) })
        }

        fn delete<'a>(&'a self, _id: &'a str, _path: &'a str) -> BoxFuture<'a, Result<()>> {
            Box::pin(async { Ok(()) })
        }
    }

    #[tokio::test]
    async fn download_dir_rooted_swap_between_listing_and_read_cannot_escape() {
        if !cfg!(unix) {
            return;
        }

        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("root");
        let outside = temp.path().join("outside");
        std::fs::create_dir(&root).unwrap();
        std::fs::create_dir(root.join("nested")).unwrap();
        std::fs::create_dir(&outside).unwrap();

        let probe = temp.path().join("symlink-probe");
        if let Err(error) = thegn_core::fsperm::create_symlink(&outside, &probe, true) {
            if matches!(
                error.kind(),
                std::io::ErrorKind::Unsupported | std::io::ErrorKind::PermissionDenied
            ) {
                return;
            }
            panic!("create directory symlink fixture: {error}");
        }
        std::fs::remove_file(probe).unwrap();

        let provider = SwappingProvider {
            root: root.clone(),
            outside: outside.clone(),
        };
        let result = provider.download_dir("id", "/", &root).await;
        assert!(
            root.join("moved").is_dir(),
            "read callback performed the directory swap"
        );
        assert!(
            std::fs::symlink_metadata(root.join("nested"))
                .unwrap()
                .file_type()
                .is_symlink(),
            "read callback planted the attacker-controlled symlink"
        );
        assert!(
            !outside.join("file").exists(),
            "a directory swapped during the awaited read must not redirect publication outside the root"
        );
        if result.is_ok() {
            assert_eq!(std::fs::read(root.join("moved/file")).unwrap(), b"data");
        }
    }

    #[tokio::test]
    async fn download_dir_handles_many_sibling_directories() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("root");
        std::fs::create_dir(&root).unwrap();

        ManyDirectoriesProvider
            .download_dir("id", "/", &root)
            .await
            .unwrap();

        for index in 0..300 {
            assert!(root.join(format!("directory-{index}")).is_dir());
        }
    }

    struct HandleCountingProvider {
        high_water: std::sync::Mutex<usize>,
    }

    impl ProviderFiles for HandleCountingProvider {
        fn read<'a>(&'a self, _id: &'a str, _path: &'a str) -> BoxFuture<'a, Result<Vec<u8>>> {
            Box::pin(async { Ok(Vec::new()) })
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
            let count = std::fs::read_dir("/proc/self/fd").map(|entries| entries.count());
            if let Ok(count) = count {
                let mut high_water = self
                    .high_water
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                *high_water = (*high_water).max(count);
            }
            let entries = if path.is_empty() {
                (0..300)
                    .map(|index| FileEntry {
                        name: format!("directory-{index}"),
                        is_dir: true,
                        size: 0,
                    })
                    .collect()
            } else {
                Vec::new()
            };
            Box::pin(async move { Ok(entries) })
        }

        fn delete<'a>(&'a self, _id: &'a str, _path: &'a str) -> BoxFuture<'a, Result<()>> {
            Box::pin(async { Ok(()) })
        }
    }

    /// Regression gate for the fd bound: opening siblings only when popped keeps descriptors bounded by depth.
    #[tokio::test]
    async fn download_dir_open_handles_stay_bounded_by_depth() {
        if !cfg!(unix) {
            return;
        }

        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("root");
        std::fs::create_dir(&root).unwrap();
        let Ok(entries) = std::fs::read_dir("/proc/self/fd") else {
            return;
        };
        let baseline = entries.count();
        let provider = HandleCountingProvider {
            high_water: std::sync::Mutex::new(baseline),
        };

        provider.download_dir("id", "/", &root).await.unwrap();

        let high_water = *provider
            .high_water
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        assert!(
            high_water <= baseline + 16,
            "open handle high-water {high_water} exceeded baseline {baseline} by more than 16"
        );
    }

    struct TreeFiles {
        files: std::sync::Mutex<std::collections::BTreeMap<String, Vec<u8>>>,
    }

    impl TreeFiles {
        fn new() -> Self {
            Self {
                files: std::sync::Mutex::new(std::collections::BTreeMap::new()),
            }
        }

        fn set(&self, path: &str, bytes: &[u8]) {
            self.files
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .insert(path.to_string(), bytes.to_vec());
        }
    }

    impl ProviderFiles for TreeFiles {
        fn read<'a>(&'a self, _id: &'a str, path: &'a str) -> BoxFuture<'a, Result<Vec<u8>>> {
            let result = self
                .files
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .get(path)
                .cloned()
                .ok_or_else(|| anyhow!("missing file"));
            Box::pin(async move { result })
        }

        fn write<'a>(
            &'a self,
            _id: &'a str,
            path: &'a str,
            data: &'a [u8],
        ) -> BoxFuture<'a, Result<()>> {
            self.set(path, data);
            Box::pin(async { Ok(()) })
        }

        fn list_dir<'a>(
            &'a self,
            _id: &'a str,
            path: &'a str,
        ) -> BoxFuture<'a, Result<Vec<FileEntry>>> {
            let prefix = format!("{}/", path.trim_end_matches('/'));
            let files = self
                .files
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let mut children = std::collections::BTreeMap::<String, FileEntry>::new();
            for (remote_path, bytes) in files.iter() {
                let Some(remainder) = remote_path.strip_prefix(&prefix) else {
                    continue;
                };
                let (name, is_dir, size) = match remainder.split_once('/') {
                    Some((name, _)) => (name, true, 0),
                    None => (remainder, false, bytes.len() as u64),
                };
                children
                    .entry(name.to_string())
                    .or_insert_with(|| FileEntry {
                        name: name.to_string(),
                        is_dir,
                        size,
                    });
            }
            let entries: Vec<FileEntry> = children.into_values().collect();
            Box::pin(async move { Ok(entries) })
        }

        fn delete<'a>(&'a self, _id: &'a str, path: &'a str) -> BoxFuture<'a, Result<()>> {
            self.files
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .remove(path);
            Box::pin(async { Ok(()) })
        }
    }

    #[test]
    fn sync_reconcile_action_table() {
        let b = [1; 32];
        let l = [2; 32];
        let r = [3; 32];
        let cases = [
            (Some(&b), Some(&b), &b, ReconcileAction::Skip),
            (Some(&b), Some(&l), &b, ReconcileAction::Skip),
            (Some(&b), None, &b, ReconcileAction::Skip),
            (Some(&b), Some(&b), &r, ReconcileAction::Publish),
            (None, None, &r, ReconcileAction::Publish),
            (Some(&b), Some(&r), &r, ReconcileAction::Skip),
            (None, Some(&r), &r, ReconcileAction::Skip),
            (Some(&b), Some(&l), &r, ReconcileAction::Conflict),
            (Some(&b), None, &r, ReconcileAction::Conflict),
            (None, Some(&l), &r, ReconcileAction::Conflict),
        ];
        for (base, local, remote, expected) in cases {
            assert_eq!(reconcile_action(base, local, remote), expected);
        }
    }

    #[tokio::test]
    async fn sync_reconcile_upload_records_a_baseline_for_every_uploaded_file() {
        use sha2::{Digest, Sha256};

        let _state = thegn_core::testenv::IsolatedState::new();
        let temp = tempfile::tempdir().unwrap();
        std::fs::create_dir(temp.path().join("d")).unwrap();
        std::fs::write(temp.path().join("a.txt"), b"one").unwrap();
        std::fs::write(temp.path().join("d/b.txt"), b"two").unwrap();
        let files = TreeFiles::new();
        let baseline = upload_tree(&files, "id", temp.path(), "/w").await.unwrap();
        let expected = std::collections::BTreeMap::from([
            ("a.txt".to_string(), Sha256::digest(b"one").into()),
            ("d/b.txt".to_string(), Sha256::digest(b"two").into()),
        ]);
        assert_eq!(baseline, expected);
        let uploaded = files.files.lock().unwrap();
        assert_eq!(
            uploaded.get("/w/a.txt").map(Vec::as_slice),
            Some(&b"one"[..])
        );
        assert_eq!(
            uploaded.get("/w/d/b.txt").map(Vec::as_slice),
            Some(&b"two"[..])
        );
    }

    #[tokio::test]
    async fn sync_reconcile_keeps_a_local_edit_when_the_sandbox_copy_is_unchanged() {
        let _state = thegn_core::testenv::IsolatedState::new();
        let temp = tempfile::tempdir().unwrap();
        std::fs::write(temp.path().join("a.txt"), b"one").unwrap();
        let files = TreeFiles::new();
        let baseline = upload_tree(&files, "id", temp.path(), "/w").await.unwrap();
        std::fs::write(temp.path().join("a.txt"), b"mine").unwrap();
        download_reconciled(&files, "id", "/w", temp.path(), &baseline)
            .await
            .unwrap();
        assert_eq!(std::fs::read(temp.path().join("a.txt")).unwrap(), b"mine");
    }

    #[tokio::test]
    async fn sync_reconcile_fast_forwards_an_untouched_local_file() {
        let _state = thegn_core::testenv::IsolatedState::new();
        let temp = tempfile::tempdir().unwrap();
        std::fs::write(temp.path().join("a.txt"), b"one").unwrap();
        let files = TreeFiles::new();
        let baseline = upload_tree(&files, "id", temp.path(), "/w").await.unwrap();
        files.set("/w/a.txt", b"theirs");
        download_reconciled(&files, "id", "/w", temp.path(), &baseline)
            .await
            .unwrap();
        assert_eq!(std::fs::read(temp.path().join("a.txt")).unwrap(), b"theirs");
    }

    #[tokio::test]
    async fn sync_reconcile_publishes_a_new_sandbox_file() {
        let _state = thegn_core::testenv::IsolatedState::new();
        let temp = tempfile::tempdir().unwrap();
        let files = TreeFiles::new();
        let baseline = upload_tree(&files, "id", temp.path(), "/w").await.unwrap();
        files.set("/w/n/new.txt", b"x");
        download_reconciled(&files, "id", "/w", temp.path(), &baseline)
            .await
            .unwrap();
        assert_eq!(std::fs::read(temp.path().join("n/new.txt")).unwrap(), b"x");
    }

    #[tokio::test]
    async fn sync_reconcile_refuses_a_both_sides_edit_before_writing_anything() {
        let _state = thegn_core::testenv::IsolatedState::new();
        let temp = tempfile::tempdir().unwrap();
        std::fs::create_dir(temp.path().join("d")).unwrap();
        std::fs::write(temp.path().join("a.txt"), b"one").unwrap();
        std::fs::write(temp.path().join("d/b.txt"), b"two").unwrap();
        let files = TreeFiles::new();
        let baseline = upload_tree(&files, "id", temp.path(), "/w").await.unwrap();
        std::fs::write(temp.path().join("a.txt"), b"mine").unwrap();
        files.set("/w/a.txt", b"theirs");
        files.set("/w/d/b.txt", b"fast");
        files.set("/w/z/new.txt", b"x");
        let result = download_reconciled(&files, "id", "/w", temp.path(), &baseline).await;
        let conflict = result.unwrap_err().downcast_ref::<SyncConflict>().cloned();
        assert_eq!(
            conflict.map(|error| error.paths),
            Some(vec!["a.txt".to_string()])
        );
        assert_eq!(std::fs::read(temp.path().join("a.txt")).unwrap(), b"mine");
        assert_eq!(std::fs::read(temp.path().join("d/b.txt")).unwrap(), b"two");
        assert!(!temp.path().join("z").exists());
    }

    #[tokio::test]
    async fn sync_reconcile_keeps_ignored_local_build_output() {
        let _state = thegn_core::testenv::IsolatedState::new();
        let temp = tempfile::tempdir().unwrap();
        std::fs::write(temp.path().join(".gitignore"), "out/\n").unwrap();
        std::fs::create_dir(temp.path().join("out")).unwrap();
        std::fs::write(temp.path().join("out/x"), b"local build output").unwrap();
        let files = TreeFiles::new();
        files.set("/w/out/x", b"sandbox build output");

        download_reconciled(&files, "id", "/w", temp.path(), &SyncBaseline::new())
            .await
            .unwrap();

        assert_eq!(
            std::fs::read(temp.path().join("out/x")).unwrap(),
            b"local build output",
            "ignored local output is excluded from upload conflicts and stays local"
        );
    }

    #[tokio::test]
    async fn sync_reconcile_skips_an_ignored_sandbox_directory() {
        let _state = thegn_core::testenv::IsolatedState::new();
        let temp = tempfile::tempdir().unwrap();
        std::fs::write(temp.path().join(".gitignore"), "out/\n").unwrap();
        let files = TreeFiles::new();
        files.set("/w/out/new.txt", b"x");
        files.set("/w/keep.txt", b"k");

        download_reconciled(&files, "id", "/w", temp.path(), &SyncBaseline::new())
            .await
            .unwrap();

        assert!(!temp.path().join("out").exists());
        assert_eq!(std::fs::read(temp.path().join("keep.txt")).unwrap(), b"k");
    }

    #[tokio::test]
    async fn sync_reconcile_honours_a_nested_gitignore() {
        let _state = thegn_core::testenv::IsolatedState::new();
        let temp = tempfile::tempdir().unwrap();
        std::fs::create_dir(temp.path().join("sub")).unwrap();
        std::fs::create_dir(temp.path().join("sub/build")).unwrap();
        std::fs::write(temp.path().join("sub/.gitignore"), "build/\n/gen\n").unwrap();
        std::fs::write(temp.path().join("sub/build/x"), b"mine").unwrap();
        std::fs::write(temp.path().join("sub/gen"), b"mine").unwrap();
        let files = TreeFiles::new();
        files.set("/w/sub/build/x", b"theirs");
        files.set("/w/sub/gen", b"theirs");
        files.set("/w/gen", b"top");

        download_reconciled(&files, "id", "/w", temp.path(), &SyncBaseline::new())
            .await
            .unwrap();

        assert_eq!(
            std::fs::read(temp.path().join("sub/build/x")).unwrap(),
            b"mine"
        );
        assert_eq!(std::fs::read(temp.path().join("sub/gen")).unwrap(), b"mine");
        assert_eq!(std::fs::read(temp.path().join("gen")).unwrap(), b"top");
    }

    #[tokio::test]
    async fn sync_reconcile_honours_a_dot_ignore_file() {
        let _state = thegn_core::testenv::IsolatedState::new();
        let temp = tempfile::tempdir().unwrap();
        std::fs::write(temp.path().join(".ignore"), "out/\n").unwrap();
        std::fs::create_dir(temp.path().join("out")).unwrap();
        std::fs::write(temp.path().join("out/x"), b"mine").unwrap();
        let files = TreeFiles::new();
        files.set("/w/out/x", b"theirs");

        download_reconciled(&files, "id", "/w", temp.path(), &SyncBaseline::new())
            .await
            .unwrap();

        assert_eq!(std::fs::read(temp.path().join("out/x")).unwrap(), b"mine");
    }

    // Positive control: this negated path is not ignored and still conflicts.
    #[tokio::test]
    async fn sync_reconcile_still_conflicts_on_a_negated_path() {
        let _state = thegn_core::testenv::IsolatedState::new();
        let temp = tempfile::tempdir().unwrap();
        std::fs::write(temp.path().join(".gitignore"), "out/*\n!out/keep\n").unwrap();
        std::fs::create_dir(temp.path().join("out")).unwrap();
        std::fs::write(temp.path().join("out/keep"), b"mine").unwrap();
        let files = TreeFiles::new();
        files.set("/w/out/keep", b"theirs");

        let result =
            download_reconciled(&files, "id", "/w", temp.path(), &SyncBaseline::new()).await;

        let conflict = result.unwrap_err().downcast_ref::<SyncConflict>().cloned();
        assert_eq!(
            conflict.map(|error| error.paths),
            Some(vec!["out/keep".to_string()])
        );
        assert_eq!(
            std::fs::read(temp.path().join("out/keep")).unwrap(),
            b"mine"
        );
    }

    #[tokio::test]
    async fn sync_reconcile_skips_output_excluded_by_info_exclude() {
        let _state = thegn_core::testenv::IsolatedState::new();
        let temp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(temp.path().join(".git/info")).unwrap();
        std::fs::write(temp.path().join(".git/info/exclude"), "out/\n").unwrap();
        std::fs::create_dir(temp.path().join("out")).unwrap();
        std::fs::write(temp.path().join("out/x"), b"mine").unwrap();
        let files = TreeFiles::new();
        files.set("/w/out/x", b"theirs");

        download_reconciled(&files, "id", "/w", temp.path(), &SyncBaseline::new())
            .await
            .unwrap();

        assert_eq!(std::fs::read(temp.path().join("out/x")).unwrap(), b"mine");
    }

    #[tokio::test]
    async fn sync_reconcile_applies_info_exclude_rules_after_one_invalid_line() {
        let _state = thegn_core::testenv::IsolatedState::new();
        let temp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(temp.path().join(".git/info")).unwrap();
        std::fs::write(temp.path().join(".git/info/exclude"), "a/**b[\nout/\n").unwrap();
        std::fs::create_dir(temp.path().join("out")).unwrap();
        std::fs::write(temp.path().join("out/x"), b"mine").unwrap();
        let files = TreeFiles::new();
        files.set("/w/out/x", b"theirs");

        download_reconciled(&files, "id", "/w", temp.path(), &SyncBaseline::new())
            .await
            .unwrap();

        assert_eq!(
            std::fs::read(temp.path().join("out/x")).unwrap(),
            b"mine",
            "a single invalid exclude line must not discard the valid out/ rule"
        );
    }

    #[tokio::test]
    async fn sync_reconcile_applies_gitignore_rules_after_one_invalid_line() {
        let _state = thegn_core::testenv::IsolatedState::new();
        let temp = tempfile::tempdir().unwrap();
        std::fs::write(temp.path().join(".gitignore"), "a/**b[\nout/\n").unwrap();
        std::fs::create_dir(temp.path().join("out")).unwrap();
        std::fs::write(temp.path().join("out/x"), b"mine").unwrap();
        let files = TreeFiles::new();
        files.set("/w/out/x", b"theirs");

        download_reconciled(&files, "id", "/w", temp.path(), &SyncBaseline::new())
            .await
            .unwrap();

        assert_eq!(
            std::fs::read(temp.path().join("out/x")).unwrap(),
            b"mine",
            "a single invalid ignore line must not discard the valid out/ rule"
        );
    }

    #[tokio::test]
    async fn sync_reconcile_skips_output_excluded_in_a_linked_worktree() {
        let _state = thegn_core::testenv::IsolatedState::new();
        let temp = tempfile::tempdir().unwrap();
        let main = temp.path().join("main");
        let worktree = temp.path().join("wt");
        let gitdir = main.join(".git/worktrees/wt");
        std::fs::create_dir_all(main.join(".git/info")).unwrap();
        std::fs::create_dir_all(&gitdir).unwrap();
        std::fs::write(main.join(".git/info/exclude"), "out/\n").unwrap();
        std::fs::write(gitdir.join("commondir"), "../..\n").unwrap();
        std::fs::create_dir_all(&worktree).unwrap();
        std::fs::write(
            worktree.join(".git"),
            format!("gitdir: {}\n", gitdir.display()),
        )
        .unwrap();
        std::fs::create_dir(worktree.join("out")).unwrap();
        std::fs::write(worktree.join("out/x"), b"mine").unwrap();
        let files = TreeFiles::new();
        files.set("/w/out/x", b"theirs");

        download_reconciled(&files, "id", "/w", &worktree, &SyncBaseline::new())
            .await
            .unwrap();

        assert_eq!(std::fs::read(worktree.join("out/x")).unwrap(), b"mine");
    }

    #[test]
    fn local_ignore_honours_a_global_excludes_matcher() {
        let temp = tempfile::tempdir().unwrap();
        let excludes = tempfile::tempdir().unwrap();
        let excludes_file = excludes.path().join("ignore");
        std::fs::write(&excludes_file, "target/\n").unwrap();
        let mut builder = ignore::gitignore::GitignoreBuilder::new(temp.path());
        assert!(builder.add(&excludes_file).is_none());
        let matcher = builder.build().unwrap();
        let mut ignore = LocalIgnore::with_global(temp.path(), matcher).unwrap();

        assert!(ignore.is_ignored("target/debug/x", false).unwrap());
        assert!(!ignore.is_ignored("src/x", false).unwrap());
    }

    #[test]
    fn local_ignore_gitignore_negation_beats_info_exclude() {
        let temp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(temp.path().join(".git/info")).unwrap();
        std::fs::write(temp.path().join(".git/info/exclude"), "out/*\n").unwrap();
        std::fs::write(temp.path().join(".gitignore"), "!out/keep\n").unwrap();
        let mut ignore =
            LocalIgnore::with_global(temp.path(), ignore::gitignore::Gitignore::empty()).unwrap();

        assert!(!ignore.is_ignored("out/keep", false).unwrap());
        assert!(ignore.is_ignored("out/other", false).unwrap());
    }

    #[test]
    fn inventory_report_from_ids_is_complete_and_id_only() {
        let report = InventoryReport::from_ids(vec!["a".into(), "b".into()]);
        assert_eq!(
            report,
            InventoryReport::Complete(vec![SandboxInfo::id_only("a"), SandboxInfo::id_only("b")])
        );
        let InventoryReport::Complete(rows) = report else {
            unreachable!()
        };
        assert!(
            rows.iter().all(|row| row.size.is_none()
                && row.state.is_none()
                && row.created_at_ms.is_none())
        );
    }

    #[test]
    fn inventory_report_from_inventory_maps_each_completeness() {
        use crate::vps::{Inventory, PartialReason};
        assert_eq!(
            InventoryReport::from_inventory(Inventory::Complete(vec![SandboxInfo::id_only("a")]))
                .unwrap(),
            InventoryReport::Complete(vec![SandboxInfo::id_only("a")])
        );
        assert_eq!(
            InventoryReport::from_inventory(Inventory::Partial {
                rows: vec![SandboxInfo::id_only("a")],
                reason: PartialReason::RowCap,
            })
            .unwrap(),
            InventoryReport::Partial {
                rows: vec![SandboxInfo::id_only("a")],
                reason: PartialReason::RowCap.message().to_string(),
            }
        );
        let error =
            InventoryReport::from_inventory(Inventory::Unknown("listing failed (500)".into()))
                .unwrap_err();
        assert_eq!(error.to_string(), "listing failed (500)");
    }

    #[test]
    fn inventory_text_takes_the_first_usable_string() {
        let row = serde_json::json!({"status": 3, "state": " ready ", "blank": "", "plan": "x"});
        assert_eq!(
            inventory_text(&row, &["status", "state"]),
            Some("ready".into())
        );
        assert_eq!(inventory_text(&row, &["blank", "plan"]), Some("x".into()));
        assert_eq!(inventory_text(&row, &["missing"]), None);
        assert_eq!(inventory_text(&serde_json::json!("str"), &["status"]), None);
    }

    #[test]
    fn inventory_time_ms_reads_rfc3339_and_epochs() {
        let rfc = chrono::DateTime::parse_from_rfc3339("2026-11-16T10:00:00+02:00")
            .unwrap()
            .timestamp_millis();
        for (value, expected) in [
            (serde_json::json!("2026-11-16T10:00:00+02:00"), Some(rfc)),
            (serde_json::json!(1_700_000_000), Some(1_700_000_000_000)),
            (
                serde_json::json!(1_700_000_000_456_i64),
                Some(1_700_000_000_456),
            ),
            (serde_json::json!("1700000000"), Some(1_700_000_000_000)),
            (serde_json::json!(1_700_000_000.9), Some(1_700_000_000_000)),
            (serde_json::json!("not a time"), None),
            (serde_json::json!(true), None),
            (serde_json::json!({"seconds": 1}), None),
            (serde_json::Value::Null, None),
        ] {
            let row = serde_json::json!({ "created_at": value.clone() });
            assert_eq!(
                inventory_time_ms(&row, &["created_at"]),
                expected,
                "value {value}"
            );
        }
        // The first key holding a usable value wins; wrong types fall through.
        let row = serde_json::json!({"created_at": [1], "createdAt": 1_700_000_000});
        assert_eq!(
            inventory_time_ms(&row, &["created_at", "createdAt"]),
            Some(1_700_000_000_000)
        );
    }

    #[test]
    fn daytona_parse_infos_reads_optional_fields_tolerantly() {
        let rows = DaytonaProvider::parse_infos(&serde_json::json!({"sandboxes": [
            {"id": "a", "state": "started", "class": "small", "target": "eu",
             "createdAt": "2026-11-16T10:00:00Z"},
            {"sandbox": {"id": "b", "status": "stopped", "size": "large", "region": "us",
                         "created_at": 1_700_000_000}},
            {"id": "c"},
            {"id": "d", "state": 1, "class": {"cpu": 2}, "target": ["eu"], "createdAt": "soon"}
        ]}))
        .unwrap();
        let rfc = chrono::DateTime::parse_from_rfc3339("2026-11-16T10:00:00Z")
            .unwrap()
            .timestamp_millis();
        assert_eq!(
            rows,
            vec![
                SandboxInfo {
                    id: "a".into(),
                    name: None,
                    state: Some("started".into()),
                    size: Some("small".into()),
                    region: Some("eu".into()),
                    created_at_ms: Some(rfc),
                },
                SandboxInfo {
                    id: "b".into(),
                    name: None,
                    state: Some("stopped".into()),
                    size: Some("large".into()),
                    region: Some("us".into()),
                    created_at_ms: Some(1_700_000_000_000),
                },
                SandboxInfo::id_only("c"),
                SandboxInfo::id_only("d"),
            ]
        );
        // The strict id and envelope refusals are unchanged.
        assert!(DaytonaProvider::parse_infos(&serde_json::json!([{"state": "started"}])).is_err());
        assert!(DaytonaProvider::parse_infos(&serde_json::json!([{"id": ""}])).is_err());
        assert!(DaytonaProvider::parse_infos(&serde_json::json!({"items": []})).is_err());
        assert_eq!(
            DaytonaProvider::parse_list(&serde_json::json!([{"id": "a", "state": 1}])).unwrap(),
            vec!["a"]
        );
    }

    #[test]
    fn sprites_parse_list_page_reads_optional_fields_tolerantly() {
        let page = SpritesProvider::parse_list_page(&serde_json::json!({
            "sprites": [
                {"id": "sprite-1", "name": "tg-a", "status": "warm", "class": "standard",
                 "region": "ord", "created_at": "2026-11-16T10:00:00Z"},
                {"name": "tg-b"},
                {"name": "tg-c", "id": 5, "status": false, "region": {"code": "ord"},
                 "created_at": "later"}
            ],
            "has_more": false
        }))
        .unwrap();
        let rfc = chrono::DateTime::parse_from_rfc3339("2026-11-16T10:00:00Z")
            .unwrap()
            .timestamp_millis();
        assert_eq!(
            page.rows,
            vec![
                SandboxInfo {
                    id: "tg-a".into(),
                    name: Some("sprite-1".into()),
                    state: Some("warm".into()),
                    size: Some("standard".into()),
                    region: Some("ord".into()),
                    created_at_ms: Some(rfc),
                },
                SandboxInfo::id_only("tg-b"),
                SandboxInfo::id_only("tg-c"),
            ]
        );
        assert!(
            SpritesProvider::parse_list_page(&serde_json::json!([{"id": "x", "status": "warm"}]))
                .is_err()
        );
    }
}
