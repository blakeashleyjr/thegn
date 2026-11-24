//! Vercel Sandbox provider (`provider = "vercel"`): an HTTP control plane,
//! exec over the command API and the `/interactive` WebSocket through the
//! `thegn vercel-exec` self-bridge, and fly-style spend safety (an
//! intent-before-create ledger, an account fingerprint and a lifetime reaper).
//!
//! The thegn sandbox id is the Vercel sandbox **name**; the session id changes
//! on every resume and is resolved per call (cached in-process only).

pub mod client;
pub mod files;
pub mod login;
pub mod pty;
pub mod stream;
pub mod usage;

use std::collections::{BTreeSet, HashMap, HashSet};
use std::fmt;
use std::path::PathBuf;
use std::sync::{LazyLock, Mutex};
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use futures_util::future::BoxFuture;
use thegn_core::vercel::api::{self, SandboxView, Scope, SessionStatus};
use thegn_core::vercel::usage::{SessionUsage, SnapshotUsage};
use thegn_core::vercel::{admit, policy};

use crate::provider::{
    ExecKind, InventoryReport, PolicyRule, ProviderEgress, RemoteProvider, SandboxHandle,
    SandboxInfo,
};
use crate::vps::registry;
use client::checked_name;

/// The ledger `provider` value and the account-fingerprint kind.
const LEDGER_PROVIDER: &str = "vercel";
/// The stable error text for a stopped sandbox under [`Wake::Never`]; the
/// `vercel-exec` bridge maps it to exit 75.
pub const STOPPED_TEXT: &str = "vercel sandbox stopped";
/// The most list pages one inventory read follows; a hit is `Partial`.
const MAX_LIST_PAGES: usize = 50;
/// How long a resume (or a pending session) may take to reach `running`.
const RESUME_BUDGET: Duration = Duration::from_secs(120);
/// The status poll interval while waiting for `running`.
const POLL_INTERVAL: Duration = Duration::from_secs(2);
/// How long an accepted delete may take to become visible as gone.
const DESTROY_CONFIRM_BUDGET: Duration = Duration::from_secs(30);
/// The captured-output cap of a one-shot command.
const EXEC_OUT_CAP: usize = 4 << 20;

fn append_vercel_stderr(mut out: String, code: i32, stderr: &str) -> String {
    if code == 0 || stderr.is_empty() {
        return out;
    }
    out.push_str(&format!(
        "\n[stderr, last {} bytes]\n{}",
        stderr.len(),
        stderr
    ));
    out
}

/// The legal vCPU counts.
const LEGAL_VCPUS: [u32; 6] = [1, 2, 4, 8, 16, 32];

/// Whether a call may resume a stopped sandbox (which buys compute).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Wake {
    /// Resume a stopped sandbox if needed (explicit open, interactive panes).
    Allow,
    /// Never resume: a stopped sandbox is an error (chrome reads, reaper).
    Never,
}

/// The Vercel API token. Redacted `Debug`; no `Display`/`Serialize`. Sent only
/// as the `Authorization` header to the validated `api_base` authority.
pub struct VercelToken(zeroize::Zeroizing<String>);

impl VercelToken {
    pub fn new(token: String) -> Self {
        VercelToken(zeroize::Zeroizing::new(token))
    }

    /// The raw token, for the `Authorization` header only.
    pub(crate) fn expose(&self) -> &str {
        self.0.as_str()
    }
}

impl fmt::Debug for VercelToken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("VercelToken(<redacted>)")
    }
}

/// Everything needed to drive one Vercel env.
#[derive(Debug)]
pub struct VercelSpec {
    /// Control-plane base (empty ⇒ [`thegn_core::vercel::api::DEFAULT_API_BASE`]).
    pub api_base: String,
    pub token: VercelToken,
    /// The configured secret ref name (value-free account identity).
    pub credential_ref: String,
    pub scope: Scope,
    /// The sandbox name this provider creates (empty for inventory-only use).
    pub name: String,
    pub vcpus: u32,
    pub image: Option<String>,
    pub region: Option<String>,
    pub max_instances: u32,
    pub max_lifetime_secs: u64,
    /// `[env.<name>.provider] interactive_host_suffixes` (empty ⇒ defaults).
    pub interactive_host_suffixes: Vec<String>,
    /// `[env.<name>.provider] model_credentials` (admitted: `none`/`upload`).
    pub model_credentials: thegn_core::config::ModelCredentials,
}

/// The control-plane calls the provider makes. [`client::VercelClient`] is the
/// production implementation; unit tests substitute an in-memory fake.
trait Api: Send + Sync {
    fn create<'a>(&'a self, body: &'a serde_json::Value) -> BoxFuture<'a, Result<SandboxView>>;
    fn get<'a>(&'a self, name: &'a str, resume: bool)
    -> BoxFuture<'a, Result<Option<SandboxView>>>;
    fn list_page<'a>(
        &'a self,
        cursor: Option<&'a str>,
        name_prefix: &'a str,
    ) -> BoxFuture<'a, Result<(Vec<SandboxView>, Option<String>)>>;
    fn delete<'a>(&'a self, name: &'a str) -> BoxFuture<'a, Result<()>>;
    fn stop<'a>(&'a self, session: &'a str) -> BoxFuture<'a, Result<()>>;
    fn extend<'a>(
        &'a self,
        session: &'a str,
        ms: u64,
    ) -> BoxFuture<'a, Result<client::ExtendOutcome>>;
    fn set_network_policy<'a>(
        &'a self,
        name: &'a str,
        body: &'a serde_json::Value,
    ) -> BoxFuture<'a, Result<()>>;
    fn run_wait<'a>(
        &'a self,
        session: &'a str,
        body: &'a serde_json::Value,
        out_cap: usize,
    ) -> BoxFuture<'a, Result<(i32, Vec<u8>)>>;
    fn run_wait_with_stderr<'a>(
        &'a self,
        session: &'a str,
        body: &'a serde_json::Value,
        out_cap: usize,
    ) -> BoxFuture<'a, Result<(i32, Vec<u8>, String)>> {
        Box::pin(async move {
            let (code, out) = self.run_wait(session, body, out_cap).await?;
            Ok((code, out, String::new()))
        })
    }
    fn run_detached<'a>(
        &'a self,
        session: &'a str,
        body: &'a serde_json::Value,
    ) -> BoxFuture<'a, Result<String>>;
    fn logs<'a>(
        &'a self,
        session: &'a str,
        cmd: &'a str,
    ) -> BoxFuture<'a, Result<client::LogStream>>;
    fn interactive<'a>(
        &'a self,
        session: &'a str,
    ) -> BoxFuture<'a, Result<client::InteractiveTicket>>;
    fn fs_write_targz<'a>(
        &'a self,
        session: &'a str,
        cwd: &'a str,
        targz: Vec<u8>,
    ) -> BoxFuture<'a, Result<()>>;
    fn fs_read<'a>(
        &'a self,
        session: &'a str,
        path: &'a str,
        cap: usize,
    ) -> BoxFuture<'a, Result<Option<Vec<u8>>>>;
    fn fs_mkdir<'a>(&'a self, session: &'a str, path: &'a str) -> BoxFuture<'a, Result<()>>;
    fn snapshots_for<'a>(&'a self, name: &'a str) -> BoxFuture<'a, Result<(Vec<String>, bool)>>;
    fn delete_snapshot<'a>(&'a self, id: &'a str) -> BoxFuture<'a, Result<()>>;
    /// The current session's usage (a GET; never resumes).
    fn usage<'a>(&'a self, name: &'a str) -> BoxFuture<'a, Result<Option<SessionUsage>>>;
    /// The sandbox's snapshots with sizes and status.
    fn snapshot_usage<'a>(
        &'a self,
        name: &'a str,
    ) -> BoxFuture<'a, Result<(Vec<SnapshotUsage>, bool)>>;
}

impl Api for client::VercelClient {
    fn create<'a>(&'a self, body: &'a serde_json::Value) -> BoxFuture<'a, Result<SandboxView>> {
        Box::pin(client::VercelClient::create(self, body))
    }
    fn get<'a>(
        &'a self,
        name: &'a str,
        resume: bool,
    ) -> BoxFuture<'a, Result<Option<SandboxView>>> {
        Box::pin(client::VercelClient::get(self, name, resume))
    }
    fn list_page<'a>(
        &'a self,
        cursor: Option<&'a str>,
        name_prefix: &'a str,
    ) -> BoxFuture<'a, Result<(Vec<SandboxView>, Option<String>)>> {
        Box::pin(client::VercelClient::list_page(self, cursor, name_prefix))
    }
    fn delete<'a>(&'a self, name: &'a str) -> BoxFuture<'a, Result<()>> {
        Box::pin(client::VercelClient::delete(self, name))
    }
    fn stop<'a>(&'a self, session: &'a str) -> BoxFuture<'a, Result<()>> {
        Box::pin(client::VercelClient::stop(self, session))
    }
    fn extend<'a>(
        &'a self,
        session: &'a str,
        ms: u64,
    ) -> BoxFuture<'a, Result<client::ExtendOutcome>> {
        Box::pin(client::VercelClient::extend(self, session, ms))
    }
    fn set_network_policy<'a>(
        &'a self,
        name: &'a str,
        body: &'a serde_json::Value,
    ) -> BoxFuture<'a, Result<()>> {
        Box::pin(client::VercelClient::set_network_policy(self, name, body))
    }
    fn run_wait<'a>(
        &'a self,
        session: &'a str,
        body: &'a serde_json::Value,
        out_cap: usize,
    ) -> BoxFuture<'a, Result<(i32, Vec<u8>)>> {
        Box::pin(client::VercelClient::run_wait(self, session, body, out_cap))
    }
    fn run_wait_with_stderr<'a>(
        &'a self,
        session: &'a str,
        body: &'a serde_json::Value,
        out_cap: usize,
    ) -> BoxFuture<'a, Result<(i32, Vec<u8>, String)>> {
        Box::pin(client::VercelClient::run_wait_with_stderr(
            self, session, body, out_cap,
        ))
    }
    fn run_detached<'a>(
        &'a self,
        session: &'a str,
        body: &'a serde_json::Value,
    ) -> BoxFuture<'a, Result<String>> {
        Box::pin(client::VercelClient::run_detached(self, session, body))
    }
    fn logs<'a>(
        &'a self,
        session: &'a str,
        cmd: &'a str,
    ) -> BoxFuture<'a, Result<client::LogStream>> {
        Box::pin(client::VercelClient::logs(self, session, cmd))
    }
    fn interactive<'a>(
        &'a self,
        session: &'a str,
    ) -> BoxFuture<'a, Result<client::InteractiveTicket>> {
        Box::pin(client::VercelClient::interactive(self, session))
    }
    fn fs_write_targz<'a>(
        &'a self,
        session: &'a str,
        cwd: &'a str,
        targz: Vec<u8>,
    ) -> BoxFuture<'a, Result<()>> {
        Box::pin(client::VercelClient::fs_write_targz(
            self, session, cwd, targz,
        ))
    }
    fn fs_read<'a>(
        &'a self,
        session: &'a str,
        path: &'a str,
        cap: usize,
    ) -> BoxFuture<'a, Result<Option<Vec<u8>>>> {
        Box::pin(client::VercelClient::fs_read(self, session, path, cap))
    }
    fn fs_mkdir<'a>(&'a self, session: &'a str, path: &'a str) -> BoxFuture<'a, Result<()>> {
        Box::pin(client::VercelClient::fs_mkdir(self, session, path))
    }
    fn snapshots_for<'a>(&'a self, name: &'a str) -> BoxFuture<'a, Result<(Vec<String>, bool)>> {
        Box::pin(client::VercelClient::snapshots_for(self, name))
    }
    fn delete_snapshot<'a>(&'a self, id: &'a str) -> BoxFuture<'a, Result<()>> {
        Box::pin(client::VercelClient::delete_snapshot(self, id))
    }
    fn usage<'a>(&'a self, name: &'a str) -> BoxFuture<'a, Result<Option<SessionUsage>>> {
        Box::pin(client::VercelClient::usage(self, name))
    }
    fn snapshot_usage<'a>(
        &'a self,
        name: &'a str,
    ) -> BoxFuture<'a, Result<(Vec<SnapshotUsage>, bool)>> {
        Box::pin(client::VercelClient::snapshot_usage(self, name))
    }
}

/// Serializes vercel creates in this process, so two concurrent creates
/// cannot both pass the remote hard-cap count before either reserves.
static CREATE_LOCK: LazyLock<tokio::sync::Mutex<()>> =
    LazyLock::new(|| tokio::sync::Mutex::new(()));

/// Sandboxes already warned about reaching the plan's extension ceiling (one
/// `warn` per process per sandbox).
static CEILING_WARNED: LazyLock<Mutex<HashSet<String>>> =
    LazyLock::new(|| Mutex::new(HashSet::new()));

/// Removes a freshly reserved intent unless creation reached the POST (the
/// `_at` twin of `registry::IntentGuard`, bound to this provider's ledger dir).
struct IntentGuard {
    dir: PathBuf,
    name: String,
    armed: bool,
}

impl IntentGuard {
    fn preserve(&mut self) {
        self.armed = false;
    }
}

impl Drop for IntentGuard {
    fn drop(&mut self) {
        if self.armed {
            registry::remove_at(&self.dir, &self.name);
        }
    }
}

/// The admission `VercelProvider::new` runs: scope ids, the sandbox name (when
/// set) and the vCPU count. Errors carry a field and a fixed reason only.
fn admit_spec(spec: &VercelSpec) -> Result<()> {
    if !admit::valid_team_id(spec.scope.team_id.trim()) {
        bail!("vercel: provider.team_id must look like team_<alphanumeric>");
    }
    if !admit::valid_project_id(spec.scope.project_id.trim()) {
        bail!("vercel: provider.project_id must look like prj_<alphanumeric>");
    }
    if !spec.name.is_empty() && !admit::valid_sandbox_name(&spec.name) {
        bail!("vercel: the sandbox name is not a valid vercel sandbox name");
    }
    if !LEGAL_VCPUS.contains(&spec.vcpus) {
        bail!("vercel: provider.size must be one of 1, 2, 4, 8, 16 or 32 vCPUs");
    }
    Ok(())
}

/// The HTTP status a control-plane error carries, if it names one: a
/// `reqwest` status in the chain, else a 3-digit code after `(`, `HTTP ` or
/// `status ` in the rendered text (`… failed (422 Unprocessable Entity)`).
fn error_status(err: &anyhow::Error) -> Option<u16> {
    for cause in err.chain() {
        if let Some(status) = cause
            .downcast_ref::<reqwest::Error>()
            .and_then(reqwest::Error::status)
        {
            return Some(status.as_u16());
        }
    }
    let text = format!("{err:#}");
    for marker in ["HTTP ", "(", "status "] {
        let mut rest = text.as_str();
        while let Some(at) = rest.find(marker) {
            rest = &rest[at + marker.len()..];
            let digits: String = rest.chars().take_while(char::is_ascii_digit).collect();
            if digits.len() == 3 {
                return Result::ok(digits.parse());
            }
        }
    }
    None
}

/// A definite client-side rejection of a create: the request was refused, so
/// no resource exists. 408/429 are transient (the create may still land) and
/// 409 means a sandbox with this name already exists (perhaps ours), so none of
/// those count.
fn definite_create_rejection(err: &anyhow::Error) -> bool {
    error_status(err)
        .is_some_and(|code| (400..500).contains(&code) && ![408, 409, 429].contains(&code))
}

/// Env keys a sandbox command may receive: terminal identity and `THEGN_*`
/// context, never anything credential-shaped. Everything else is dropped. The
/// one allowlist: every env map entering a sandbox (one-shot and detached
/// commands, the PTY start frame, provisioning steps, and the `vercel-exec`
/// bridge's host env) passes it.
pub fn env_key_allowed(key: &str) -> bool {
    if matches!(key, "TERM" | "LANG" | "LC_ALL" | "LC_CTYPE" | "COLORTERM") {
        return true;
    }
    // The command body's key shape (`[A-Z_][A-Z0-9_]*`): a key it would
    // refuse is dropped here rather than failing the whole command.
    if !key.starts_with("THEGN_")
        || !key
            .bytes()
            .all(|b| b.is_ascii_uppercase() || b.is_ascii_digit() || b == b'_')
    {
        return false;
    }
    !["TOKEN", "SECRET", "KEY", "PASSWORD", "CREDENTIAL", "AUTH"]
        .iter()
        .any(|deny| key.contains(deny))
}

/// Whether `key` names the configured token ref (`VERCEL_TOKEN`, or the tail
/// of a scheme-prefixed ref such as `env:MY_VAR`).
fn names_token_ref(key: &str, token_ref: &str) -> bool {
    let token_ref = token_ref.trim();
    !token_ref.is_empty()
        && (key == token_ref || token_ref.rsplit(':').next().is_some_and(|tail| tail == key))
}

/// The allowlisted subset of `env`, also dropping the configured Vercel token
/// ref name (`token_ref`); dropped keys are traced by name only. The one
/// exception: `CLAUDE_CONFIG_DIR`/`CODEX_HOME` pass when the value is a path
/// under `/dev/shm/thegn-login-` (the `upload` login dir; a path, not a
/// secret).
pub(crate) fn env_allowlisted(env: &[(String, String)], token_ref: &str) -> Vec<(String, String)> {
    let mut kept = Vec::new();
    let mut dropped = Vec::new();
    for (key, value) in env {
        if (env_key_allowed(key) || login::login_env_allowed(key, value))
            && !names_token_ref(key, token_ref)
        {
            kept.push((key.clone(), value.clone()));
        } else {
            dropped.push(key.as_str());
        }
    }
    if !dropped.is_empty() {
        tracing::debug!(keys = ?dropped, "vercel: dropped env keys outside the allowlist");
    }
    kept
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .ok()
        .and_then(|d| i64::try_from(d.as_millis()).ok())
        .unwrap_or(0)
}

/// Whether `view` carries this controller's ownership tag, read through the
/// same [`policy::tags_prove_ours`] the reaper uses (so the value compared is
/// exactly what `create` wrote via [`policy::tags_for`]). The sandbox name is
/// not part of the proof: env sandbox names are user templates
/// (`{repo}-{worktree}-{hash}` by default), not a fixed prefix.
fn is_ours(view: &SandboxView, controller: &str) -> bool {
    admit::valid_sandbox_name(&view.name) && policy::tags_prove_ours(&view.tags, controller, "")
}

fn status_text(status: SessionStatus) -> String {
    format!("{status:?}").to_ascii_lowercase()
}

/// The fleet row for one listed sandbox. Every network number is checked.
fn sandbox_info(view: &SandboxView) -> SandboxInfo {
    SandboxInfo {
        id: view.name.clone(),
        name: Some(view.name.clone()),
        state: Some(status_text(view.status)),
        size: view
            .vcpus
            .filter(|vcpus| LEGAL_VCPUS.contains(vcpus))
            .map(policy::size_key),
        region: view.region.clone(),
        created_at_ms: view.created_at_ms.filter(|ms| *ms > 0),
    }
}

/// The ledger `created_at` (unix seconds) from a provider `createdAt` in ms.
fn created_at_secs(created_at_ms: Option<i64>) -> i64 {
    created_at_ms
        .filter(|ms| *ms > 0)
        .and_then(|ms| ms.checked_div(1000))
        .filter(|secs| *secs > 0)
        .unwrap_or_else(thegn_core::util::now)
}

/// The Vercel provider. The token lives only in `client`; `spec.token` is
/// emptied at construction.
pub struct VercelProvider {
    spec: VercelSpec,
    client: client::VercelClient,
    owner: Option<String>,
    /// name -> current session id (in-process cache only).
    sessions: Mutex<HashMap<String, String>>,
    /// name -> (session id, estimated expiry in unix ms). Sessions this process
    /// did not start are estimated as already due, so the bridge extends early
    /// rather than late (the lifetime ceiling still bounds every extension).
    expiry: Mutex<HashMap<String, (String, i64)>>,
    /// name -> the network rules last applied by this process.
    policies: Mutex<HashMap<String, Vec<PolicyRule>>>,
    /// The ledger directory (`None` ⇒ `registry::dir()`).
    ledger_dir: Option<PathBuf>,
    poll: Duration,
    destroy_budget: Duration,
    #[cfg(test)]
    fake: Option<std::sync::Arc<dyn Api>>,
}

impl VercelProvider {
    pub fn new(mut spec: VercelSpec) -> Result<Self> {
        admit_spec(&spec)?;
        let token = std::mem::replace(&mut spec.token, VercelToken::new(String::new()));
        let base = if spec.api_base.trim().is_empty() {
            thegn_core::vercel::api::DEFAULT_API_BASE.to_string()
        } else {
            spec.api_base.trim().to_string()
        };
        let client = client::VercelClient::new(&base, token, spec.scope.clone())?
            .with_interactive_hosts(spec.interactive_host_suffixes.clone());
        Ok(VercelProvider {
            spec,
            client,
            owner: None,
            sessions: Mutex::new(HashMap::new()),
            expiry: Mutex::new(HashMap::new()),
            policies: Mutex::new(HashMap::new()),
            ledger_dir: None,
            poll: POLL_INTERVAL,
            destroy_budget: DESTROY_CONFIRM_BUDGET,
            #[cfg(test)]
            fake: None,
        })
    }

    /// A provider driving `fake` instead of the HTTP client, with its ledger
    /// at `ledger_dir`.
    #[cfg(test)]
    fn with_client_for_test(
        spec: VercelSpec,
        fake: std::sync::Arc<dyn Api>,
        ledger_dir: PathBuf,
    ) -> Result<Self> {
        let mut provider = Self::new(spec)?;
        provider.fake = Some(fake);
        provider.ledger_dir = Some(ledger_dir);
        provider.poll = Duration::from_millis(1);
        provider.destroy_budget = Duration::from_millis(50);
        Ok(provider)
    }

    fn api(&self) -> &dyn Api {
        #[cfg(test)]
        if let Some(fake) = self.fake.as_deref() {
            return fake;
        }
        &self.client
    }

    fn ledger_dir(&self) -> PathBuf {
        self.ledger_dir.clone().unwrap_or_else(registry::dir)
    }

    #[must_use]
    pub fn with_owner(mut self, owner: String) -> Self {
        self.owner = Some(owner);
        self
    }

    pub fn owner(&self) -> Option<&str> {
        self.owner.as_deref()
    }

    fn api_base(&self) -> &str {
        let base = self.spec.api_base.trim();
        if base.is_empty() {
            thegn_core::vercel::api::DEFAULT_API_BASE
        } else {
            base
        }
    }

    /// Value-free account fingerprint for the ledger: the API authority, team
    /// and project, and the credential ref name (never the token).
    pub fn account(&self) -> Result<String> {
        let authority = crate::provider::authority_key(self.api_base())?;
        let scope = format!(
            "{authority}/{}/{}",
            self.spec.scope.team_id.trim(),
            self.spec.scope.project_id.trim()
        );
        Ok(registry::account_fingerprint(
            LEDGER_PROVIDER,
            &scope,
            &self.spec.credential_ref,
        ))
    }

    async fn controller(&self) -> Result<String> {
        tokio::task::spawn_blocking(crate::vps::controller::controller_id)
            .await
            .context("vercel: controller id worker failed")?
    }

    /// Our controller-tagged sandboxes from the paged project listing (no name
    /// prefix: names are user templates). A page cap hit is `Partial`; a
    /// failed or looping listing is `Unknown`.
    async fn listed(&self, controller: &str) -> crate::vps::Inventory<SandboxView> {
        use crate::vps::{Inventory, PartialReason};
        let mut rows: Vec<SandboxView> = Vec::new();
        let mut seen = BTreeSet::new();
        let mut cursors = HashSet::new();
        let mut cursor: Option<String> = None;
        for _ in 0..MAX_LIST_PAGES {
            let (page, next) = match self.api().list_page(cursor.as_deref(), "").await {
                Ok(page) => page,
                Err(error) => {
                    return Inventory::Unknown(format!(
                        "vercel: sandbox listing failed: {error:#}"
                    ));
                }
            };
            for view in page {
                if !is_ours(&view, controller) {
                    continue;
                }
                if !seen.insert(view.name.clone()) {
                    return Inventory::Partial {
                        rows,
                        reason: PartialReason::DuplicateId,
                    };
                }
                rows.push(view);
            }
            match next {
                None => return Inventory::Complete(rows),
                Some(next) => {
                    if !cursors.insert(next.clone()) {
                        return Inventory::Unknown(
                            "vercel: sandbox listing repeated a cursor".into(),
                        );
                    }
                    cursor = Some(next);
                }
            }
        }
        Inventory::Partial {
            rows,
            reason: PartialReason::PageCap,
        }
    }

    /// Our sandbox names (the `list()` view).
    pub async fn list_inventory(&self) -> crate::vps::Inventory<String> {
        let controller = match self.controller().await {
            Ok(controller) => controller,
            Err(error) => return crate::vps::Inventory::Unknown(format!("{error:#}")),
        };
        self.listed(&controller).await.map_rows(|view| view.name)
    }

    /// `thegn fleet` inventory. Never creates or resumes.
    pub async fn sandbox_inventory(&self) -> Result<InventoryReport> {
        let controller = self.controller().await?;
        let listed = self.listed(&controller).await;
        InventoryReport::from_inventory(listed.map_rows(|view| sandbox_info(&view)))
    }

    fn remember_session(&self, name: &str, session: &str, started_here: bool) {
        self.sessions
            .lock()
            .unwrap()
            .insert(name.to_string(), session.to_string());
        let mut expiry = self.expiry.lock().unwrap();
        let fresh = expiry.get(name).is_none_or(|(known, _)| known != session);
        if started_here || fresh {
            // A session this process did not start is treated as already due.
            let est = if started_here {
                now_ms().saturating_add(policy::INITIAL_TIMEOUT_MS as i64)
            } else {
                now_ms()
            };
            expiry.insert(name.to_string(), (session.to_string(), est));
        }
    }

    fn forget_session(&self, name: &str) {
        self.sessions.lock().unwrap().remove(name);
        self.expiry.lock().unwrap().remove(name);
    }

    /// Whether a session-scoped request was refused because the session stopped
    /// before the provider accepted the request.
    pub(crate) fn is_session_stopped(error: &anyhow::Error) -> bool {
        error.downcast_ref::<client::SessionStopped>().is_some()
    }

    /// The current session id for `name`, resuming only under
    /// [`Wake::Allow`]. A pending session is waited for (it is already
    /// billing); a stopped one under [`Wake::Never`] is [`STOPPED_TEXT`].
    pub async fn session_for(&self, name: &str, wake: Wake) -> Result<String> {
        checked_name(name)?;
        let deadline = tokio::time::Instant::now() + RESUME_BUDGET;
        let mut resumed = false;
        loop {
            let view = self
                .api()
                .get(name, false)
                .await?
                .ok_or_else(|| anyhow!("vercel: sandbox {name} not found"))?;
            match view.status {
                SessionStatus::Running => {
                    if let Some(session) = view.session_id.as_deref() {
                        self.remember_session(name, session, resumed);
                        return Ok(session.to_string());
                    }
                }
                SessionStatus::Pending => {}
                SessionStatus::Stopped | SessionStatus::Failed | SessionStatus::Aborted => {
                    if wake == Wake::Never {
                        self.forget_session(name);
                        bail!(STOPPED_TEXT);
                    }
                    if !resumed {
                        resumed = true;
                        self.forget_session(name);
                        let view =
                            self.api().get(name, true).await?.ok_or_else(|| {
                                anyhow!("vercel: sandbox {name} not found on resume")
                            })?;
                        if view.status == SessionStatus::Running
                            && let Some(session) = view.session_id.as_deref()
                        {
                            self.remember_session(name, session, true);
                            return Ok(session.to_string());
                        }
                    }
                }
                SessionStatus::Stopping | SessionStatus::Snapshotting | SessionStatus::Unknown => {
                    if wake == Wake::Never {
                        self.forget_session(name);
                        bail!(STOPPED_TEXT);
                    }
                }
            }
            if tokio::time::Instant::now() >= deadline {
                bail!("vercel: sandbox {name} did not reach running within 120s");
            }
            tokio::time::sleep(self.poll).await;
        }
    }

    /// Resume a stopped sandbox.
    pub async fn resume(&self, name: &str) -> Result<()> {
        self.session_for(name, Wake::Allow).await.map(|_| ())
    }

    /// The session for an interactive open (PTY or stream). Unlike
    /// `session_for(.., Wake::Allow)`, it never resumes a stopped sandbox past its
    /// lifetime ceiling: an auto-respawned pane would otherwise buy a fresh session
    /// at every timeout, unattended. File transfer (`thegn env pull`) still may.
    pub(crate) async fn interactive_session(&self, name: &str) -> Result<String> {
        checked_name(name)?;
        if let Some(view) = self.api().get(name, false).await?
            && !matches!(view.status, SessionStatus::Running | SessionStatus::Pending)
        {
            let cap_ms = i64::try_from(policy::max_lifetime_secs(self.spec.max_lifetime_secs))
                .unwrap_or(i64::MAX)
                .saturating_mul(1000);
            if view
                .created_at_ms
                .is_some_and(|c| now_ms().saturating_sub(c) >= cap_ms)
            {
                bail!(
                    "vercel: sandbox {name} is past its max_lifetime_secs ceiling and is not resumed for a terminal; `thegn env pull` to recover work, then destroy it or raise max_lifetime_secs"
                );
            }
        }
        self.session_for(name, Wake::Allow).await
    }

    /// Stop the current session (compute to $0; the filesystem persists).
    /// Never resumes; an already stopped or missing sandbox is success.
    pub async fn stop_session(&self, name: &str) -> Result<()> {
        checked_name(name)?;
        let Some(view) = self.api().get(name, false).await? else {
            self.forget_session(name);
            return Ok(());
        };
        if matches!(view.status, SessionStatus::Running | SessionStatus::Pending)
            && let Some(session) = view.session_id.as_deref()
        {
            self.api().stop(session).await?;
        }
        self.forget_session(name);
        Ok(())
    }

    /// Extend the session timeout when due (bridge and one-shot exec). `Some(ms)` is the
    /// extension made; `None` = not due, not running, unbounded by a known
    /// creation time, or at the plan ceiling (warned once per sandbox).
    pub async fn extend_if_due(&self, name: &str) -> Result<Option<u64>> {
        checked_name(name)?;
        if CEILING_WARNED.lock().unwrap().contains(name) {
            return Ok(None);
        }
        let Some(view) = self.api().get(name, false).await? else {
            return Ok(None);
        };
        let (SessionStatus::Running, Some(session)) = (view.status, view.session_id.as_deref())
        else {
            return Ok(None);
        };
        // Without a creation time the lifetime ceiling cannot bound us.
        let Some(created_at_ms) = view.created_at_ms.filter(|ms| *ms > 0) else {
            return Ok(None);
        };
        self.remember_session(name, session, false);
        let now = now_ms();
        // A GET just saw this session Running, so it has not expired: an estimate in
        // the past is stale bookkeeping, not expiry. The lifetime ceiling still bounds it.
        let est = self
            .expiry
            .lock()
            .unwrap()
            .get(name)
            .map(|(_, est)| *est)
            .unwrap_or(now)
            .max(now);
        let Some(ms) = policy::next_extension(
            now,
            est,
            created_at_ms,
            policy::max_lifetime_secs(self.spec.max_lifetime_secs),
        ) else {
            return Ok(None);
        };
        match self.api().extend(session, ms).await? {
            client::ExtendOutcome::Extended => {
                if let Some((_, est)) = self.expiry.lock().unwrap().get_mut(name) {
                    let step = i64::try_from(ms).unwrap_or(i64::MAX);
                    *est = est.saturating_add(step);
                }
                Ok(Some(ms))
            }
            client::ExtendOutcome::AtCeiling => {
                if CEILING_WARNED.lock().unwrap().insert(name.to_string()) {
                    tracing::warn!(
                        sandbox = %name,
                        "vercel: session timeout is at the plan ceiling; no further extensions"
                    );
                }
                Ok(None)
            }
        }
    }

    /// The env map that may enter this provider's sandboxes: `env` through
    /// [`env_allowlisted`] with this provider's token ref name.
    pub(crate) fn sandbox_env(&self, env: &[(String, String)]) -> Vec<(String, String)> {
        env_allowlisted(env, &self.spec.credential_ref)
    }

    /// A command body (one-shot when `wait`, else detached) with the env
    /// allowlisted. Refuses before any request.
    pub(crate) fn command_body(
        &self,
        argv: &[String],
        cwd: Option<&str>,
        env: &[(String, String)],
        wait: bool,
    ) -> Result<serde_json::Value> {
        api::command_body(argv, cwd, &self.sandbox_env(env), wait)
            .map_err(|e| anyhow!("vercel: command refused: {e}"))
    }

    /// One `/cmd` run on `session` with the keep-alive checks (today's loop body).
    async fn run_exec_once(
        &self,
        name: &str,
        session: &str,
        body: &serde_json::Value,
    ) -> Result<(i32, Vec<u8>, String)> {
        let run = self.api().run_wait_with_stderr(session, body, EXEC_OUT_CAP);
        tokio::pin!(run);
        let mut check =
            tokio::time::interval(std::time::Duration::from_secs(policy::EXTEND_CHECK_SECS));
        check.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tokio::select! {
                biased;
                result = &mut run => break result,
                _ = check.tick() => {
                    // A long provisioning step must not outlive its session; the
                    // lifetime ceiling inside `extend_if_due` still bounds the spend.
                    match tokio::time::timeout(std::time::Duration::from_secs(30), self.extend_if_due(name)).await {
                        Ok(Ok(_)) => {}
                        Ok(Err(error)) if error.downcast_ref::<client::SessionGone>().is_some() => return Err(error),
                        Ok(Err(error)) => tracing::debug!(target: "thegn::vercel", sandbox = %name, error = %format!("{error:#}"), "vercel: session keep-alive check failed"),
                        Err(_) => tracing::debug!(target: "thegn::vercel", sandbox = %name, "vercel: session keep-alive check timed out"),
                    }
                }
            }
        }
    }

    /// One-shot command: `(exit_code, output)`. The env is allowlisted.
    /// A refusal because the session stopped (`client::SessionStopped`: the command never
    /// started) resolves the session again and retries once under `Wake::Allow` (the SDK's
    /// withResume rule).
    pub async fn run_exec(
        &self,
        name: &str,
        argv: &[String],
        cwd: Option<&str>,
        env: &[(String, String)],
        wake: Wake,
    ) -> Result<(i32, String)> {
        let body = self.command_body(argv, cwd, env, true)?;
        let mut retried = false;
        loop {
            let session = self.session_for(name, wake).await?;
            match self.run_exec_once(name, &session, &body).await {
                Err(error) if error.downcast_ref::<client::SessionStopped>().is_some() => {
                    self.forget_session(name);
                    if wake == Wake::Never {
                        bail!(STOPPED_TEXT);
                    }
                    if retried {
                        return Err(error);
                    }
                    retried = true;
                }
                result => {
                    let (code, out, stderr) = result?;
                    let out = String::from_utf8_lossy(&out).into_owned();
                    return Ok((code, append_vercel_stderr(out, code, &stderr)));
                }
            }
        }
    }

    /// Delete the sandbox, then every snapshot of it (404-tolerant). Never
    /// checkpoints first.
    pub async fn delete_with_snapshots(&self, name: &str) -> Result<()> {
        checked_name(name)?;
        match self.api().get(name, false).await? {
            Some(view) => {
                let controller = self.controller().await?;
                if !is_ours(&view, &controller) {
                    bail!(
                        "vercel: refusing to delete {name}: it does not carry this thegn controller's tag"
                    );
                }
                self.api().delete(name).await?;
                self.confirm_gone(name).await?;
            }
            None => {
                // Already gone: only clear snapshots of a name our ledger holds.
                let dir = self.ledger_dir();
                let owned = name.to_string();
                let recorded = tokio::task::spawn_blocking(move || registry::read_at(&dir, &owned))
                    .await
                    .context("vercel: ledger worker failed")?
                    .is_some_and(|r| r.provider == LEDGER_PROVIDER);
                if !recorded {
                    self.forget_session(name);
                    return Ok(());
                }
            }
        }
        self.forget_session(name);
        self.policies.lock().unwrap().remove(name);
        let (snapshots, complete) = self.api().snapshots_for(name).await?;
        let mut failed = 0usize;
        for id in &snapshots {
            if let Err(error) = self.api().delete_snapshot(id).await {
                failed = failed.saturating_add(1);
                tracing::warn!(sandbox = %name, "vercel: snapshot delete failed: {error:#}");
            }
        }
        if failed > 0 {
            bail!(
                "vercel: {failed} of {} snapshots of {name} could not be deleted",
                snapshots.len()
            );
        }
        if !complete {
            bail!(
                "vercel: the snapshot listing of {name} did not finish (page cap or repeated cursor); {} listed snapshots were deleted; run thegn env down again to continue",
                snapshots.len()
            );
        }
        Ok(())
    }

    async fn confirm_gone(&self, name: &str) -> Result<()> {
        let deadline = tokio::time::Instant::now() + self.destroy_budget;
        let budget_secs = self.destroy_budget.as_secs();
        loop {
            let Some(view) = self.api().get(name, false).await.with_context(|| {
                format!(
                    "vercel: delete of {name} was accepted but its absence could not be confirmed; its ledger record is kept; retry thegn env down or thegn env reap --provider vercel --apply"
                )
            })?
            else {
                return Ok(());
            };
            if tokio::time::Instant::now() >= deadline {
                bail!(
                    "vercel: delete of {name} was accepted but the sandbox is still present after {}s (status {}); its ledger record is kept; retry thegn env down or thegn env reap --provider vercel --apply",
                    budget_secs,
                    status_text(view.status)
                );
            }
            let now = tokio::time::Instant::now();
            tokio::time::sleep(self.poll.min(deadline - now)).await;
        }
    }

    /// The reaper's read of one sandbox (never resumes).
    pub async fn reaper_view(&self, name: &str) -> Result<Option<SandboxView>> {
        checked_name(name)?;
        self.api().get(name, false).await
    }

    /// Ledger names of vercel records counting against this account.
    async fn ledger_vercel_names(&self, account: &str) -> Result<Vec<String>> {
        let dir = self.ledger_dir();
        let account = account.to_string();
        tokio::task::spawn_blocking(move || {
            registry::list_at(&dir)
                .into_iter()
                .filter(|r| r.provider == LEDGER_PROVIDER)
                .filter(|r| r.account.is_empty() || account.is_empty() || r.account == account)
                .map(|r| r.name)
                .collect::<Vec<String>>()
        })
        .await
        .context("vercel: ledger worker failed")
    }

    async fn remove_record(&self, name: &str) {
        let dir = self.ledger_dir();
        let name = name.to_string();
        if tokio::task::spawn_blocking(move || registry::remove_at(&dir, &name))
            .await
            .is_err()
        {
            tracing::debug!(
                "vercel: ledger remove worker failed; the reaper reconciles the stale record"
            );
        }
    }

    fn at_cap(occupied: usize, cap: usize) -> anyhow::Error {
        anyhow!(
            "vercel: {occupied} managed sandboxes already exist (max_instances = {cap}); \
             destroy one or raise `[env.<name>.provider] max_instances`"
        )
    }
}

impl RemoteProvider for VercelProvider {
    fn create<'a>(&'a self) -> BoxFuture<'a, Result<SandboxHandle>> {
        Box::pin(async move {
            let name = checked_name(&self.spec.name)?.to_string();
            // 1. Controller id (refuses before any request when unreadable).
            let controller = self.controller().await?;
            // 2. The plan and its body, rendered before any ledger write.
            let owner = self.owner.clone().unwrap_or_default();
            let plan = policy::CreatePlan {
                name: name.clone(),
                vcpus: self.spec.vcpus,
                image: self.spec.image.clone(),
                region: self.spec.region.clone(),
                timeout_ms: policy::INITIAL_TIMEOUT_MS,
                tags: policy::tags_for(&controller, &owner, &crate::vps::host_label()),
                egress: Vec::new(),
            };
            let body = api::create_body(&self.spec.scope, &plan);
            if !body.is_object() {
                bail!("vercel: could not render the create body");
            }
            let account = self.account()?;
            let cap = policy::max_instances(self.spec.max_instances) as usize;

            let create_lock = CREATE_LOCK.lock().await;
            // The process-global hard cap: the ledger first (no request at
            // all when it is already full), then the union with the
            // controller-tagged sandboxes Vercel lists, which also sees
            // sandboxes another host created.
            let ledger = self.ledger_vercel_names(&account).await?;
            if ledger.len() >= cap {
                return Err(Self::at_cap(ledger.len(), cap));
            }
            let remote = match self.listed(&controller).await {
                crate::vps::Inventory::Complete(rows) => rows,
                crate::vps::Inventory::Partial { reason, .. } => {
                    bail!(
                        "vercel: cannot count existing sandboxes ({}); refusing to create",
                        reason.message()
                    );
                }
                crate::vps::Inventory::Unknown(reason) => {
                    bail!("vercel: cannot count existing sandboxes ({reason}); refusing to create");
                }
            };
            let occupied: BTreeSet<String> = ledger
                .into_iter()
                .chain(remote.into_iter().map(|view| view.name))
                .collect();
            if occupied.len() >= cap {
                return Err(Self::at_cap(occupied.len(), cap));
            }

            // 3. Reserve capacity and publish the intent.
            let dir = self.ledger_dir();
            let intent = registry::VpsRecord {
                name: name.clone(),
                provider: LEDGER_PROVIDER.into(),
                state: "creating".into(),
                instance_id: String::new(),
                ip: String::new(),
                created_at: thegn_core::util::now(),
                owner: owner.clone(),
                account: account.clone(),
            };
            let reserve_dir = dir.clone();
            let reserve_result = tokio::task::spawn_blocking(move || {
                registry::reserve_at(
                    &reserve_dir,
                    &intent,
                    cap,
                    registry::ReserveScope::Provider(LEDGER_PROVIDER),
                )
            })
            .await
            .context("vercel: reservation worker failed")?;
            match reserve_result {
                Ok(()) => {}
                Err(registry::ReserveError::AtCap { occupied, cap }) => {
                    return Err(Self::at_cap(occupied, cap));
                }
                Err(error @ registry::ReserveError::OwnedByOther { .. }) => {
                    return Err(anyhow!(error));
                }
                Err(error @ registry::ReserveError::OtherAccount { .. }) => {
                    return Err(anyhow!(error));
                }
                Err(registry::ReserveError::Store(error)) => {
                    return Err(error
                        .context("vercel: could not reserve ledger capacity; refusing to create"));
                }
            }
            let mut guard = IntentGuard {
                dir: dir.clone(),
                name: name.clone(),
                armed: true,
            };
            drop(create_lock);

            // 4. The POST. From here the intent survives unless the create was
            // definitively refused or a cleanup DELETE landed.
            guard.preserve();
            let created = match self.api().create(&body).await {
                Ok(view) => view,
                Err(error) if error_status(&error) == Some(409) => {
                    // The name already exists (perhaps ours from an earlier
                    // attempt, perhaps another host's): never delete it, and
                    // keep the intent so the reaper can reconcile it.
                    return Err(error.context("vercel: a sandbox with this name already exists"));
                }
                Err(error) if definite_create_rejection(&error) => {
                    self.remove_record(&name).await;
                    return Err(error.context("vercel: create refused"));
                }
                Err(error) => {
                    self.cleanup_after_failed_create(&name).await;
                    return Err(error.context("vercel: create failed"));
                }
            };

            // 5. Record the ready sandbox.
            let ready = registry::VpsRecord {
                name: name.clone(),
                provider: LEDGER_PROVIDER.into(),
                state: "ready".into(),
                instance_id: name.clone(),
                ip: String::new(),
                created_at: created_at_secs(created.created_at_ms),
                owner,
                account,
            };
            let write_dir = dir.clone();
            let written =
                tokio::task::spawn_blocking(move || registry::write_at(&write_dir, &ready))
                    .await
                    .context("vercel: ledger worker failed")
                    .and_then(|r| r);
            if let Err(error) = written {
                self.cleanup_after_failed_create(&name).await;
                return Err(error.context("vercel: could not record the created sandbox"));
            }
            if created.status == SessionStatus::Running
                && let Some(session) = created.session_id.as_deref()
            {
                self.remember_session(&name, session, true);
            }

            // 6. The handle rides the `vercel-exec` self-bridge.
            Ok(SandboxHandle {
                id: name.clone(),
                exec: ExecKind::Command(vec![
                    thegn_core::util::self_exe_str(),
                    "vercel-exec".into(),
                    name,
                    "--".into(),
                ]),
            })
        })
    }

    fn destroy<'a>(&'a self, id: &'a str) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            checked_name(id)?;
            self.delete_with_snapshots(id).await?;
            self.remove_record(id).await;
            Ok(())
        })
    }

    fn list<'a>(&'a self) -> BoxFuture<'a, Result<Vec<String>>> {
        Box::pin(async move { self.list_inventory().await.into_complete() })
    }
}

impl VercelProvider {
    /// After a create POST that may have taken effect: best-effort delete,
    /// clearing the intent only when the delete landed. A failed delete keeps
    /// the record so the reaper and `destroy` can reconcile the leak.
    async fn cleanup_after_failed_create(&self, name: &str) {
        match self.delete_with_snapshots(name).await {
            Ok(()) => self.remove_record(name).await,
            Err(error) => tracing::warn!(
                sandbox = %name,
                "vercel: create failed and cleanup DELETE failed; intent record kept for \
                 reconciliation: {error:#}"
            ),
        }
    }
}

impl ProviderEgress for VercelProvider {
    fn set_network_policy<'a>(
        &'a self,
        id: &'a str,
        allow: &'a [String],
        block: &'a [String],
    ) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            checked_name(id)?;
            let rules = crate::provider::rules_from(allow, block).map_err(|e| anyhow!("{e}"))?;
            let domains: Vec<String> = rules
                .iter()
                .filter(|r| r.action == "allow")
                .map(|r| r.domain.clone())
                .collect();
            // Vercel's custom policy is an allow list (an empty list is sent as deny-all; ["*"] as allow-all; anything unlisted is denied, so blocks are implied). A
            // block-only policy cannot be expressed, and an allow list emptied
            // by blocks would fail open: both are refused.
            if domains.is_empty() && !rules.is_empty() {
                bail!(
                    "vercel: egress supports allow lists only; refusing a policy with no allowed \
                     domain"
                );
            }
            let covered = rules
                .iter()
                .filter(|r| r.action == "deny" && r.domain != "*")
                .any(|d| {
                    domains.iter().any(|a| {
                        a == "*"
                            || a.strip_prefix("*.").is_some_and(|s| {
                                d.domain == s || d.domain.ends_with(&format!(".{s}"))
                            })
                    })
                });
            if covered {
                bail!(
                    "vercel: egress cannot block a domain inside an allowed wildcard; narrow the \
                     allow list"
                );
            }
            let body = api::network_policy_body(&domains);
            self.api().set_network_policy(id, &body).await?;
            self.policies.lock().unwrap().insert(id.to_string(), rules);
            Ok(())
        })
    }

    fn get_network_policy<'a>(&'a self, id: &'a str) -> BoxFuture<'a, Result<Vec<PolicyRule>>> {
        Box::pin(async move {
            checked_name(id)?;
            // Vercel's readback is sanitized, so only what this process
            // applied is known.
            self.policies
                .lock()
                .unwrap()
                .get(id)
                .cloned()
                .ok_or_else(|| {
                    anyhow!("vercel: no network policy applied by this process for {id}")
                })
        })
    }
}

#[cfg(test)]
mod conformance_tests;
#[cfg(test)]
mod provider_tests;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn token_debug_is_redacted() {
        let token = VercelToken::new("vcp_secret_value".into());
        let shown = format!("{token:?}");
        assert!(!shown.contains("vcp_secret_value"), "{shown}");
        assert!(shown.contains("redacted"), "{shown}");
    }

    #[test]
    fn spec_token_is_emptied_into_the_client() {
        let provider = VercelProvider::new(VercelSpec {
            api_base: String::new(),
            token: VercelToken::new("vcp_secret_value".into()),
            credential_ref: "VERCEL_TOKEN".into(),
            scope: Scope {
                team_id: "team_a".into(),
                project_id: "prj_a".into(),
            },
            name: "tg-a".into(),
            vcpus: 2,
            image: None,
            region: None,
            max_instances: 2,
            max_lifetime_secs: 3600,
            interactive_host_suffixes: Vec::new(),
            model_credentials: thegn_core::config::ModelCredentials::None,
        })
        .expect("a valid spec constructs");
        assert_eq!(provider.spec.token.expose(), "");
        assert!(!format!("{:?}", provider.spec).contains("vcp_secret_value"));
        let owned = provider.with_owner("wt-1".into());
        assert_eq!(owned.owner(), Some("wt-1"));
    }

    #[test]
    fn append_vercel_stderr_only_on_failure() {
        assert_eq!(append_vercel_stderr("out".into(), 0, "err"), "out");
        assert_eq!(append_vercel_stderr("out".into(), 1, ""), "out");
        assert_eq!(
            append_vercel_stderr("out".into(), 1, "err"),
            "out\n[stderr, last 3 bytes]\nerr"
        );
    }
}
