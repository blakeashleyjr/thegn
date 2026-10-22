//! Generic issue tracker trait + provider router.
//!
//! The `IssueBackend` trait is the single seam that all providers implement.
//! `IssueRouter` is the host-facing entry point: it reads `IssuesConfig`,
//! constructs the right backend, and forwards calls — returning empty
//! collections when no provider is configured rather than erroring, so the
//! panel always has something to render.

pub mod capabilities;
pub(crate) mod gh_run;
pub mod github;
pub(crate) mod http;
pub(crate) mod identity;
pub mod jira;
pub mod kaneo;
pub mod kaneo_auth;
pub mod linear;
pub mod secret;

use futures_util::future::BoxFuture;
use futures_util::stream::{self, StreamExt};
use std::collections::HashSet;
use std::future::Future;
use std::sync::Arc;
use std::time::Duration;
use thegn_core::config::{IssueAccount, IssueProviderKind, IssuesConfig, expand_env_ref};
use thegn_core::issue::{Issue, IssueDetail, IssueDraft, IssueFilter, IssuePatch, IssueRef};
use thegn_core::seam::{ErrorClass, SeamError};

/// Four account reads bound network fan-out without serializing normal configs.
/// `buffered` is load-bearing: unlike `buffer_unordered`, it preserves config
/// order while reads overlap. Every backend, including the plugin bridge,
/// awaits without blocking the executor, so reads overlap up to this bound.
const MAX_CONCURRENT_ACCOUNTS: usize = 4;
/// One total budget for an issue read across every configured account.
const FANOUT_DEADLINE: Duration = Duration::from_secs(30);

pub use capabilities::IssueCaps;
pub use http::block_on_tracker;

/// Errors from any issue backend.
pub enum IssueError {
    NotConfigured,
    AmbiguousAccount {
        provider: String,
        accounts: Vec<String>,
    },
    UnknownAccount {
        provider: String,
        account: String,
    },
    NoCreateTarget {
        provider: String,
        account: Option<String>,
    },
    Unsupported(&'static str),
    UnsupportedPatchFields(Vec<&'static str>),
    EmptyPatch,
    Network(reqwest::Error),
    Auth(String),
    Api(String),
    PartialUpdate {
        applied: Vec<&'static str>,
        unapplied: Vec<&'static str>,
        unverified: Vec<&'static str>,
        source: Box<IssueError>,
    },
    /// A bounded listing stopped before Linear reported its final page.
    PartialList {
        issues: Vec<Issue>,
        reason: &'static str,
    },
    Subprocess(String),
    Parse(String),
    Policy(&'static str),
    Timeout(&'static str),
    BodyLimit(&'static str),
}

impl std::fmt::Debug for IssueError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotConfigured => f.write_str("IssueError::NotConfigured"),
            Self::AmbiguousAccount { provider, accounts } => f
                .debug_struct("IssueError::AmbiguousAccount")
                .field("provider", provider)
                .field("accounts", accounts)
                .finish(),
            Self::NoCreateTarget { provider, account } => f
                .debug_struct("IssueError::NoCreateTarget")
                .field("provider", provider)
                .field("account", account)
                .finish(),
            Self::UnknownAccount { provider, account } => f
                .debug_struct("IssueError::UnknownAccount")
                .field("provider", provider)
                .field("account", account)
                .finish(),
            Self::Unsupported(op) => f.debug_tuple("IssueError::Unsupported").field(op).finish(),
            Self::UnsupportedPatchFields(fields) => f
                .debug_tuple("IssueError::UnsupportedPatchFields")
                .field(fields)
                .finish(),
            Self::EmptyPatch => f.write_str("IssueError::EmptyPatch"),
            Self::Network(_) => f.write_str("IssueError::Network(<redacted>)"),
            Self::Auth(message) => f.debug_tuple("IssueError::Auth").field(message).finish(),
            Self::Api(message) => f.debug_tuple("IssueError::Api").field(message).finish(),
            Self::PartialUpdate {
                applied,
                unapplied,
                unverified,
                source,
            } => f
                .debug_struct("IssueError::PartialUpdate")
                .field("applied", applied)
                .field("unapplied", unapplied)
                .field("unverified", unverified)
                .field("source", source)
                .finish(),
            Self::PartialList { issues, reason } => f
                .debug_struct("IssueError::PartialList")
                .field("issue_count", &issues.len())
                .field("reason", reason)
                .finish(),
            Self::Subprocess(message) => f
                .debug_tuple("IssueError::Subprocess")
                .field(message)
                .finish(),
            Self::Parse(message) => f.debug_tuple("IssueError::Parse").field(message).finish(),
            Self::Policy(message) => f.debug_tuple("IssueError::Policy").field(message).finish(),
            Self::Timeout(message) => f.debug_tuple("IssueError::Timeout").field(message).finish(),
            Self::BodyLimit(message) => f
                .debug_tuple("IssueError::BodyLimit")
                .field(message)
                .finish(),
        }
    }
}

impl std::fmt::Display for IssueError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            IssueError::NotConfigured => write!(f, "no issue provider configured"),
            IssueError::AmbiguousAccount { provider, accounts } => write!(
                f,
                "{provider} issue id is ambiguous across accounts: {}; set enabled = false on all but one [[issues.issue_accounts]] entry for {provider}, or restrict this repo with accounts = [\"<name>\"]",
                accounts.join(", ")
            ),
            IssueError::NoCreateTarget { provider, account } => {
                write!(f, "no configured {provider} issue account")?;
                if let Some(account) = account {
                    write!(f, " named {account}")?;
                }
                write!(f, " to create on")
            }
            IssueError::UnknownAccount { provider, account } => {
                write!(f, "no configured {provider} issue account named {account}")
            }
            IssueError::Unsupported(op) => write!(f, "{op} is not supported by this provider"),
            IssueError::UnsupportedPatchFields(fields) => write!(
                f,
                "patch fields are not supported by this provider: {}",
                fields.join(", ")
            ),
            IssueError::EmptyPatch => write!(f, "issue patch contains no fields"),
            // Reqwest errors may carry the full request URL, including query
            // material. Keep tracker diagnostics static and redacted.
            IssueError::Network(_) => write!(f, "network: tracker request failed"),
            IssueError::Auth(s) => write!(f, "auth: {s}"),
            IssueError::Api(s) => write!(f, "api: {s}"),
            IssueError::PartialUpdate {
                applied,
                unapplied,
                unverified,
                source,
            } => {
                write!(
                    f,
                    "partial update (applied: {}; unapplied: {}; unverified: {})",
                    applied.join(", "),
                    unapplied.join(", "),
                    unverified.join(", "),
                )?;
                if !unverified.is_empty() {
                    write!(f, " — verify before retrying")?;
                }
                write!(f, ": {source}")
            }
            IssueError::PartialList { issues, reason } => {
                write!(f, "partial issue list ({} issues): {reason}", issues.len())
            }
            IssueError::Subprocess(s) => write!(f, "subprocess: {s}"),
            IssueError::Parse(s) => write!(f, "parse: {s}"),
            IssueError::Policy(s) => write!(f, "tracker policy: {s}"),
            IssueError::Timeout(s) => write!(f, "tracker timeout: {s}"),
            IssueError::BodyLimit(s) => write!(f, "tracker body limit: {s}"),
        }
    }
}

impl std::error::Error for IssueError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            IssueError::PartialUpdate { source, .. } => Some(source.as_ref()),
            _ => None,
        }
    }
}

impl IssueError {
    /// Construct the typed error returned by an absent optional operation.
    pub fn unsupported(op: &'static str) -> Self {
        IssueError::Unsupported(op)
    }

    /// Whether this is a transient connectivity failure (connect/timeout) — as
    /// opposed to auth/parse/not-configured. Feeds the connectivity holder so a
    /// dropped link (not a bad token) is what flips the app offline.
    pub fn is_transient(&self) -> bool {
        <Self as SeamError>::is_transient(self)
    }
}

impl SeamError for IssueError {
    fn class(&self) -> ErrorClass {
        match self {
            IssueError::Unsupported(_) => ErrorClass::Unsupported,
            IssueError::UnsupportedPatchFields(_) | IssueError::EmptyPatch => {
                ErrorClass::Unsupported
            }
            IssueError::NotConfigured => ErrorClass::NotConfigured,
            IssueError::AmbiguousAccount { .. } => ErrorClass::Other,
            IssueError::UnknownAccount { .. } => ErrorClass::Other,
            IssueError::Auth(_) => ErrorClass::Auth,
            IssueError::Network(e) if e.is_connect() || e.is_timeout() => ErrorClass::Transient,
            IssueError::Timeout(_) => ErrorClass::Transient,
            IssueError::Network(_) => ErrorClass::Other,
            IssueError::PartialUpdate { source, .. } => source.class(),
            IssueError::PartialList { .. } => ErrorClass::Other,
            IssueError::Subprocess(message)
                if message
                    .to_ascii_lowercase()
                    .contains("no such file or directory")
                    || message.to_ascii_lowercase().contains("program not found") =>
            {
                ErrorClass::NotInstalled
            }
            IssueError::Api(_)
            | IssueError::Subprocess(_)
            | IssueError::Parse(_)
            | IssueError::NoCreateTarget { .. }
            | IssueError::Policy(_)
            | IssueError::BodyLimit(_) => ErrorClass::Other,
        }
    }

    fn unsupported(op: &'static str) -> Self {
        Self::unsupported(op)
    }
}

impl From<reqwest::Error> for IssueError {
    fn from(e: reqwest::Error) -> Self {
        IssueError::Network(e)
    }
}

/// Parse a provider due date into unix milliseconds: date-only `YYYY-MM-DD`
/// (Linear `dueDate`, Jira `duedate`) resolves to midnight UTC of that day;
/// full RFC3339 timestamps (Kaneo) pass through. `None` on anything else.
pub(crate) fn parse_due_date_ms(s: &str) -> Option<i64> {
    if let Ok(d) = chrono::NaiveDate::parse_from_str(s, "%Y-%m-%d") {
        return Some(d.and_hms_opt(0, 0, 0)?.and_utc().timestamp_millis());
    }
    chrono::DateTime::parse_from_rfc3339(s)
        .ok()
        .map(|dt| dt.timestamp_millis())
}

/// Validate one provider-scoped issue id at an input or response boundary.
/// This is syntax-only; account ownership remains THE-324.
pub fn validate_issue_id(id: &str) -> Result<(), IssueError> {
    identity::complete_identity(id).map_err(IssueError::Parse)?;
    if let Some(rest) = id.strip_prefix("plugin:") {
        let (plugin_id, key) = rest.split_once(':').ok_or_else(|| {
            IssueError::Parse("plugin issue id must use plugin:<namespace>:<key>".into())
        })?;
        identity::plugin_namespace(plugin_id).map_err(IssueError::Parse)?;
        identity::plugin_key(key).map_err(IssueError::Parse)?;
        return Ok(());
    }
    let (provider, key) = id
        .split_once(':')
        .ok_or_else(|| IssueError::Parse("issue id must use provider:key syntax".into()))?;
    if provider.is_empty() || key.is_empty() {
        return Err(IssueError::Parse(
            "issue id has an empty provider or key".into(),
        ));
    }
    match provider {
        "github" => {
            if let Some((repo, number)) = key.rsplit_once('#') {
                identity::github_repo(repo).map_err(IssueError::Parse)?;
                identity::github_number(number).map_err(IssueError::Parse)?;
            } else {
                identity::github_number(key).map_err(IssueError::Parse)?;
            }
        }
        "jira" => {
            identity::jira_key(key).map_err(IssueError::Parse)?;
        }
        "kaneo" => {
            identity::kaneo_id(key, "Kaneo task id").map_err(IssueError::Parse)?;
        }
        "linear" => {
            identity::builtin_identity(key).map_err(IssueError::Parse)?;
        }
        _ => return Err(IssueError::Parse("unknown issue provider namespace".into())),
    }
    Ok(())
}

fn validate_public_url(url: &str) -> Result<(), IssueError> {
    if url.is_empty() {
        return Ok(()); // legacy/fake providers may omit a browse URL
    }
    let parsed = reqwest::Url::parse(url)
        .map_err(|e| IssueError::Parse(format!("invalid issue URL: {e}")))?;
    if !matches!(parsed.scheme(), "http" | "https")
        || parsed.host_str().is_none()
        || !parsed.username().is_empty()
        || parsed.password().is_some()
    {
        return Err(IssueError::Parse(
            "issue URL has an invalid public authority".into(),
        ));
    }
    Ok(())
}

/// Validate an issue row before it enters a router, cache, or panel.
pub fn validate_control_issue_id(id: &str) -> Result<(), IssueError> {
    if id.contains(':') {
        validate_issue_id(id)
    } else {
        identity::builtin_identity(id)
            .map_err(IssueError::Parse)
            .map(|_| ())
    }
}

pub fn validate_issue_identity(issue: &Issue) -> Result<(), IssueError> {
    if let Some(plugin_namespace) = issue.provider.strip_prefix("plugin:") {
        identity::plugin_namespace(plugin_namespace).map_err(IssueError::Parse)?;
    }
    let expected = format!("{}:", issue.provider);
    if !issue.id.starts_with(&expected) {
        return Err(IssueError::Parse(
            "issue id/provider namespace mismatch".into(),
        ));
    }
    validate_issue_id(&issue.id)?;
    let (_, key) = issue.id.split_once(':').expect("validate_issue_id checked");
    match issue.provider.as_str() {
        "github" => {
            let number = key.rsplit_once('#').map(|(_, n)| n).unwrap_or(key);
            identity::github_number(number).map_err(IssueError::Parse)?;
            identity::github_number(&issue.number).map_err(IssueError::Parse)?;
        }
        "jira" => {
            identity::jira_key(key).map_err(IssueError::Parse)?;
            identity::jira_key(&issue.number).map_err(IssueError::Parse)?;
        }
        "kaneo" => {
            identity::kaneo_id(key, "Kaneo task id").map_err(IssueError::Parse)?;
            identity::kaneo_id(&issue.number, "Kaneo issue number").map_err(IssueError::Parse)?;
        }
        _ if issue.provider.starts_with("plugin:") => {
            let prefix = format!("{}:", issue.provider);
            let native = issue
                .id
                .strip_prefix(&prefix)
                .ok_or_else(|| IssueError::Parse("issue id/provider namespace mismatch".into()))?;
            identity::plugin_key(native).map_err(IssueError::Parse)?;
            identity::plugin_key(&issue.number).map_err(IssueError::Parse)?;
        }
        "linear" => {
            identity::builtin_identity(&issue.number).map_err(IssueError::Parse)?;
        }
        _ => return Err(IssueError::Parse("unknown issue provider namespace".into())),
    }
    for project in &issue.project_ids {
        if issue.provider.starts_with("plugin:") {
            identity::plugin_key(project).map_err(IssueError::Parse)?;
        } else {
            identity::builtin_identity(project).map_err(IssueError::Parse)?;
        }
    }
    for blocked in &issue.blocked_by {
        validate_issue_id(blocked)?;
    }
    validate_public_url(&issue.url)
}

/// Provider-agnostic issue tracker seam.
///
/// Methods return [`BoxFuture`]s (not native `async fn`) so the trait stays
/// object-safe — the router dispatches over `Box<dyn IssueBackend>`.
pub trait IssueBackend: Send + Sync {
    fn provider_id(&self) -> &str;
    fn caps(&self) -> IssueCaps;

    fn list_issues<'a>(
        &'a self,
        filter: &'a IssueFilter,
    ) -> BoxFuture<'a, Result<Vec<Issue>, IssueError>>;
    fn get_issue<'a>(&'a self, id: &'a str) -> BoxFuture<'a, Result<IssueDetail, IssueError>>;
    fn create_issue<'a>(
        &'a self,
        draft: &'a IssueDraft,
    ) -> BoxFuture<'a, Result<Issue, IssueError>>;
    fn update_issue<'a>(
        &'a self,
        id: &'a str,
        patch: &'a IssuePatch,
    ) -> BoxFuture<'a, Result<Issue, IssueError>>;
    /// `limit == 0` means one bounded default page per provider; a bounded search
    /// that stops early returns `IssueError::PartialList` with the rows so far.
    fn search<'a>(
        &'a self,
        query: &'a str,
        limit: usize,
    ) -> BoxFuture<'a, Result<Vec<Issue>, IssueError>>;

    // ---- optional project-management extras (default: unsupported) ----------
    // Providers that can write comments / manage labels override these; the rest
    // inherit the default and the panel/CLI reports the capability as absent.

    /// Post a comment on an issue.
    fn add_comment<'a>(
        &'a self,
        _id: &'a str,
        _body: &'a str,
    ) -> BoxFuture<'a, Result<(), IssueError>> {
        Box::pin(async move { Err(IssueError::unsupported("add_comment")) })
    }
    /// Attach a label (by name) to an issue, creating it if the provider allows.
    fn attach_label<'a>(
        &'a self,
        _id: &'a str,
        _label: &'a str,
    ) -> BoxFuture<'a, Result<(), IssueError>> {
        Box::pin(async move { Err(IssueError::unsupported("attach_label")) })
    }
    /// Remove a label (by name) from an issue.
    fn detach_label<'a>(
        &'a self,
        _id: &'a str,
        _label: &'a str,
    ) -> BoxFuture<'a, Result<(), IssueError>> {
        Box::pin(async move { Err(IssueError::unsupported("detach_label")) })
    }

    /// Downcast to the concrete Kaneo backend for board/project browsing, which
    /// is Kaneo-shaped (columns per project) rather than provider-agnostic.
    fn as_kaneo(&self) -> Option<&kaneo::KaneoBackend> {
        None
    }
}

/// Build a backend from one named account's token + scope. Returns `None` for
/// a `None`-provider account. Subprocess-backed providers (GitHub's `gh`) are
/// anchored to `dir` so calls without an explicit `--repo` resolve against
/// that worktree instead of the process cwd.
///
/// Tokens go through [`secret::resolve_account_token`], so `keyring:` refs
/// reach the host's credential broker instead of being sent to the provider as
/// a literal API key.
pub(crate) fn backend_from_account(
    a: &IssueAccount,
    dir: Option<&std::path::Path>,
    http_budget: Arc<http::TrackerHttpBudget>,
) -> Option<Box<dyn IssueBackend>> {
    match a.provider {
        IssueProviderKind::Linear => {
            let api_key = secret::resolve_account_token(&a.token, "linear").unwrap_or_default();
            let team_id = (!a.team_id.is_empty()).then(|| a.team_id.clone());
            Some(Box::new(linear::LinearBackend::new_with_budget(
                api_key,
                team_id,
                http_budget,
            )))
        }
        IssueProviderKind::Github => {
            let mut b = github::GitHubIssuesBackend::new(a.extra_flags.clone());
            b.set_dir(dir.map(std::path::Path::to_path_buf));
            Some(Box::new(b))
        }
        IssueProviderKind::Jira => {
            let api_token = secret::resolve_account_token(&a.token, "jira").unwrap_or_default();
            Some(Box::new(jira::JiraBackend::new_with_budget(
                a.base_url.clone(),
                a.email.clone(),
                api_token,
                (!a.project_key.is_empty()).then(|| a.project_key.clone()),
                http_budget,
            )))
        }
        IssueProviderKind::Kaneo => {
            let mut api_key = secret::resolve_account_token(&a.token, "kaneo").unwrap_or_default();
            // No configured key ⇒ fall back to a token stored by
            // `thegn kaneo login` (device flow) for this instance.
            if api_key.is_empty() {
                api_key = kaneo_stored_token(&a.base_url).unwrap_or_default();
            }
            Some(Box::new(kaneo::KaneoBackend::new_with_budget(
                a.base_url.clone(),
                api_key,
                (!a.workspace_id.is_empty()).then(|| a.workspace_id.clone()),
                (!a.project_id.is_empty()).then(|| a.project_id.clone()),
                http_budget,
            )))
        }
        IssueProviderKind::None => None,
    }
}

/// Read the device-flow access token stored by `thegn kaneo login` for a Kaneo
/// instance. Best-effort: a missing DB / no login yields `None`, and the
/// backend then runs unauthenticated (and the panel shows it empty).
///
/// The DB no longer holds the raw token (THE-66): `thegn kaneo login` stores it
/// in the broker and records a `file:`/`env:` SecretRef in `kaneo_auth`. Resolve
/// that ref through `expand_env_ref`. A legacy row that still holds a bare raw
/// token resolves as itself (read-through fallback for one release).
fn kaneo_stored_token(base_url: &str) -> Option<String> {
    use thegn_core::store::CacheStore;
    let base = base_url.trim_end_matches('/');
    let (stored, _) = thegn_core::db::Db::open()
        .ok()?
        .get_kaneo_token(base)
        .ok()
        .flatten()?;
    expand_env_ref(&stored).filter(|s| !s.trim().is_empty())
}

/// A configured backend tagged with the account name it was built from, so the
/// cache and "My Work" feed can key each provider's issues by `(provider,
/// account)` — supporting multiple accounts of the same provider.
struct AccountBackend {
    account: String,
    inner: Box<dyn IssueBackend>,
}

/// The explicit provider and optional account that may receive an issue
/// creation request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CreateTarget {
    pub provider: String,
    pub account: Option<String>,
}

/// Routes issue requests across every configured provider. `list`/`search` fan
/// out and merge; by-id routing refuses ambiguity, while by-ref routing is exact.
/// Returns empty results (not errors) when nothing is configured — the panel
/// renders gracefully regardless. A single provider failing never breaks the
/// others: it logs and contributes nothing to the merged result.
pub struct IssueRouter {
    inner: Vec<AccountBackend>,
}

impl IssueRouter {
    /// Append a dynamically-provided backend (provider-as-plugin): `account`
    /// labels its rows like a configured account's name would.
    pub fn push_backend(
        &mut self,
        account: String,
        inner: Box<dyn IssueBackend>,
    ) -> Result<(), IssueError> {
        if let Some(namespace) = inner.provider_id().strip_prefix("plugin:") {
            identity::plugin_namespace(namespace).map_err(IssueError::Parse)?;
        }
        if inner.provider_id().is_empty()
            || inner.provider_id().contains(':') && !inner.provider_id().starts_with("plugin:")
        {
            return Err(IssueError::Parse("invalid issue provider namespace".into()));
        }
        self.inner.push(AccountBackend { account, inner });
        Ok(())
    }

    pub fn from_config(cfg: &IssuesConfig) -> Self {
        Self::from_config_with_http_budget(cfg, http::TrackerHttpBudget::process())
    }

    /// Like [`from_config`](Self::from_config), but anchors subprocess-backed
    /// providers (GitHub's `gh`) to `dir` so calls without an explicit `--repo`
    /// resolve against that worktree instead of the process cwd. Callers
    /// fetching for a specific worktree should prefer this.
    pub fn from_config_at(cfg: &IssuesConfig, dir: Option<&std::path::Path>) -> Self {
        Self::from_config_at_with_http_budget(cfg, dir, http::TrackerHttpBudget::process())
    }

    pub(crate) fn from_config_with_http_budget(
        cfg: &IssuesConfig,
        http_budget: Arc<http::TrackerHttpBudget>,
    ) -> Self {
        Self::from_config_at_with_http_budget(cfg, None, http_budget)
    }

    pub(crate) fn from_config_at_with_http_budget(
        cfg: &IssuesConfig,
        dir: Option<&std::path::Path>,
        http_budget: Arc<http::TrackerHttpBudget>,
    ) -> Self {
        let inner = cfg
            .active_accounts()
            .into_iter()
            .filter_map(|acct| {
                backend_from_account(&acct, dir, Arc::clone(&http_budget)).map(|inner| {
                    AccountBackend {
                        account: acct.name,
                        inner,
                    }
                })
            })
            .collect();
        IssueRouter { inner }
    }

    /// The provider id of the first configured backend (`"none"` when empty).
    /// Retained for callers that only need a representative id.
    pub fn provider_id(&self) -> &str {
        self.inner
            .first()
            .map(|b| b.inner.provider_id())
            .unwrap_or("none")
    }

    /// Every configured provider id, in config order (may repeat when several
    /// accounts share a provider).
    pub fn provider_ids(&self) -> Vec<&str> {
        self.inner.iter().map(|b| b.inner.provider_id()).collect()
    }

    pub fn is_configured(&self) -> bool {
        !self.inner.is_empty()
    }

    /// Locate the backend owning an id. Provider namespaces are matched by
    /// longest registered prefix so `plugin:demo:key` reaches `plugin:demo`.
    /// A bare legacy id is accepted only when there is exactly one backend;
    /// namespaced ids never fall through to another provider.
    fn backend_for_id(&self, id: &str) -> Result<Option<&AccountBackend>, IssueError> {
        let matches = |b: &&AccountBackend| {
            id == b.inner.provider_id()
                || id
                    .strip_prefix(b.inner.provider_id())
                    .is_some_and(|rest| rest.starts_with(':'))
        };
        let longest = self
            .inner
            .iter()
            .filter(matches)
            .map(|b| b.inner.provider_id().len())
            .max();
        if let Some(longest) = longest {
            let matching: Vec<&AccountBackend> = self
                .inner
                .iter()
                .filter(|b| matches(b) && b.inner.provider_id().len() == longest)
                .collect();
            if matching.len() > 1 {
                return Err(IssueError::AmbiguousAccount {
                    provider: matching[0].inner.provider_id().to_string(),
                    accounts: matching.iter().map(|b| b.account.clone()).collect(),
                });
            }
            return Ok(matching.first().copied());
        }
        // Preserve the audited legacy bare-id compatibility only at the
        // unambiguous sole-backend boundary. A namespaced id never falls back.
        if !id.contains(':') && self.inner.len() == 1 {
            return Ok(self.inner.first());
        }
        Ok(None)
    }

    fn backend_for_ref(&self, issue_ref: &IssueRef) -> Result<&AccountBackend, IssueError> {
        let matching: Vec<&AccountBackend> = self
            .inner
            .iter()
            .filter(|backend| {
                backend.inner.provider_id() == issue_ref.provider
                    && backend.account == issue_ref.account
            })
            .collect();
        match matching.as_slice() {
            [backend] => Ok(*backend),
            [] => Err(IssueError::UnknownAccount {
                provider: issue_ref.provider.clone(),
                account: issue_ref.account.clone(),
            }),
            backends => Err(IssueError::AmbiguousAccount {
                provider: issue_ref.provider.clone(),
                accounts: backends
                    .iter()
                    .map(|backend| backend.account.clone())
                    .collect(),
            }),
        }
    }

    /// The error for an id that routed nowhere. An empty router is genuinely
    /// [`IssueError::NotConfigured`]; anything else names the expected form and
    /// the providers actually configured, so the message stops claiming "no
    /// issue provider configured" on a machine where one is.
    fn id_miss(&self, id: &str) -> IssueError {
        if self.inner.is_empty() {
            return IssueError::NotConfigured;
        }
        // Order-preserving dedupe: several accounts may share a provider, and
        // naming it three times helps nobody.
        let mut ids: Vec<&str> = Vec::new();
        for p in self.provider_ids() {
            if !ids.contains(&p) {
                ids.push(p);
            }
        }
        let shown: String = id.chars().take(256).collect();
        IssueError::Api(format!(
            "`{shown}` does not name a configured tracker — use \"<provider>:<key>\" \
             (configured: {})",
            ids.join(", ")
        ))
    }

    /// List issues across all accounts with bounded concurrent reads, merged
    /// in configuration order. A failing account logs and contributes nothing
    /// rather than failing the whole call.
    ///
    /// **Best-effort by design, and deliberately so** — this is the background
    /// fan-out's entry point, where one bad account must not blank the three
    /// good ones. The flip side is that the error is only a `tracing::warn!`,
    /// which goes nowhere with `THEGN_LOG` unset: a caller on the primary path
    /// of a **user-invoked** action must not use this, or a 400 reads as an
    /// empty tracker. Use [`list_per_provider`](Self::list_per_provider) there
    /// and report the per-account `Err`s (see `cmd::issue::list_tracker_issues`).
    pub async fn list_issues(&self, filter: &IssueFilter) -> Result<Vec<Issue>, IssueError> {
        let mut per_account = Vec::new();
        let futures = self
            .inner
            .iter()
            .map(|b| (b, async move { b.inner.list_issues(filter).await }))
            .collect();
        let results = fan_out_until(futures, FANOUT_DEADLINE).await;
        for (b, result) in results {
            let result = match result {
                Ok(result) => result,
                Err(error) => {
                    tracing::warn!(account = %b.account, provider = b.inner.provider_id(), error = %error, "issue list account exceeded fan-out deadline");
                    continue;
                }
            };
            match result {
                Ok(issues) => match validate_issues(issues) {
                    Ok(valid) => per_account.push(stamp_issues(valid, &b.account)),
                    Err(e) => {
                        tracing::warn!(provider = b.inner.provider_id(), error = %e, "issue identity rejected")
                    }
                },
                Err(IssueError::PartialList { issues, reason }) => {
                    let valid = issues
                        .into_iter()
                        .filter_map(|issue| match validate_issue_identity(&issue) {
                            Ok(()) => Some(stamp_issue(issue, &b.account)),
                            Err(error) => {
                                tracing::warn!(
                                    account = %b.account,
                                    provider = b.inner.provider_id(),
                                    error = %error,
                                    "partial issue identity rejected"
                                );
                                None
                            }
                        })
                        .collect::<Vec<_>>();
                    tracing::warn!(
                        account = %b.account,
                        provider = b.inner.provider_id(),
                        partial_count = valid.len(),
                        reason,
                        "issue list is partial; retaining rows without caching it as complete"
                    );
                    per_account.push(valid);
                }
                Err(e) => {
                    tracing::warn!(account = %b.account, provider = b.inner.provider_id(), error = %e, "issue list failed")
                }
            }
        }
        Ok(merge_fair(per_account, filter.limit))
    }

    /// Per-account results in configuration order, so callers (the cache
    /// refresh) can store and diff each account under its own
    /// `(repo_root, provider, account)` key.
    pub async fn list_per_provider(
        &self,
        filter: &IssueFilter,
    ) -> Vec<(String, &str, Result<Vec<Issue>, IssueError>)> {
        let futures = self
            .inner
            .iter()
            .map(|b| {
                let metadata = (b.account.clone(), b.inner.provider_id());
                let account = b.account.clone();
                (metadata, async move {
                    // Keep PartialList as an error so cache refreshes never persist it as complete.
                    match b.inner.list_issues(filter).await {
                        Ok(issues) => {
                            validate_issues(issues).map(|issues| stamp_issues(issues, &account))
                        }
                        Err(IssueError::PartialList { issues, reason }) => {
                            Err(IssueError::PartialList {
                                issues: stamp_issues(issues, &account),
                                reason,
                            })
                        }
                        Err(error) => Err(error),
                    }
                })
            })
            .collect();
        fan_out_until(futures, FANOUT_DEADLINE)
            .await
            .into_iter()
            .map(|((account, provider), result)| {
                let result = result.unwrap_or_else(Err);
                (account, provider, result)
            })
            .collect()
    }

    pub async fn get_issue(&self, id: &str) -> Result<IssueDetail, IssueError> {
        validate_control_issue_id(id)?;
        match self.backend_for_id(id) {
            Ok(Some(b)) => b.inner.get_issue(id).await.and_then(|mut detail| {
                validate_issue_identity(&detail.issue)?;
                detail.issue.account = Some(b.account.clone());
                Ok(detail)
            }),
            Ok(None) => Err(self.id_miss(id)),
            Err(error) => Err(error),
        }
    }

    pub async fn get_issue_ref(&self, issue_ref: &IssueRef) -> Result<IssueDetail, IssueError> {
        validate_issue_ref(issue_ref)?;
        let backend = self.backend_for_ref(issue_ref)?;
        let mut detail = backend.inner.get_issue(&issue_ref.id).await?;
        validate_issue_identity(&detail.issue)?;
        if detail.issue.id != issue_ref.id {
            return Err(IssueError::Policy(
                "tracker returned a different issue than the ref",
            ));
        }
        detail.issue.account = Some(issue_ref.account.clone());
        Ok(detail)
    }

    pub async fn update_issue_ref(
        &self,
        issue_ref: &IssueRef,
        patch: &IssuePatch,
    ) -> Result<Issue, IssueError> {
        validate_issue_ref(issue_ref)?;
        let backend = self.backend_for_ref(issue_ref)?;
        validate_patch_capabilities(backend.inner.caps(), patch)?;
        let issue = backend.inner.update_issue(&issue_ref.id, patch).await?;
        validate_issue_identity(&issue)?;
        if issue.id != issue_ref.id {
            return Err(IssueError::Policy(
                "tracker returned a different issue than the ref",
            ));
        }
        Ok(stamp_issue(issue, &issue_ref.account))
    }

    pub async fn add_comment_ref(
        &self,
        issue_ref: &IssueRef,
        body: &str,
    ) -> Result<(), IssueError> {
        validate_issue_ref(issue_ref)?;
        self.backend_for_ref(issue_ref)?
            .inner
            .add_comment(&issue_ref.id, body)
            .await
    }

    pub async fn attach_label_ref(
        &self,
        issue_ref: &IssueRef,
        label: &str,
    ) -> Result<(), IssueError> {
        validate_issue_ref(issue_ref)?;
        self.backend_for_ref(issue_ref)?
            .inner
            .attach_label(&issue_ref.id, label)
            .await
    }

    pub async fn detach_label_ref(
        &self,
        issue_ref: &IssueRef,
        label: &str,
    ) -> Result<(), IssueError> {
        validate_issue_ref(issue_ref)?;
        self.backend_for_ref(issue_ref)?
            .inner
            .detach_label(&issue_ref.id, label)
            .await
    }

    /// Create an issue only when this router has exactly one configured account.
    pub async fn create_issue(&self, draft: &IssueDraft) -> Result<Issue, IssueError> {
        match self.inner.as_slice() {
            [] => Err(IssueError::NotConfigured),
            [backend] => {
                let target = CreateTarget {
                    provider: backend.inner.provider_id().to_string(),
                    account: Some(backend.account.clone()),
                };
                self.create_issue_on(&target, draft).await
            }
            backends => Err(IssueError::AmbiguousAccount {
                provider: "any".into(),
                accounts: backends
                    .iter()
                    .map(|backend| backend.account.clone())
                    .collect(),
            }),
        }
    }

    /// Create an issue on the explicitly selected provider account. The
    /// target must resolve uniquely before the backend is called.
    pub async fn create_issue_on(
        &self,
        target: &CreateTarget,
        draft: &IssueDraft,
    ) -> Result<Issue, IssueError> {
        let matches: Vec<&AccountBackend> = self
            .inner
            .iter()
            .filter(|backend| {
                backend.inner.provider_id() == target.provider.as_str()
                    && target
                        .account
                        .as_ref()
                        .is_none_or(|account| backend.account.as_str() == account.as_str())
            })
            .collect();
        let backend = match matches.as_slice() {
            [backend] => *backend,
            [] if self.inner.is_empty() => return Err(IssueError::NotConfigured),
            [] => {
                return Err(IssueError::NoCreateTarget {
                    provider: target.provider.clone(),
                    account: target.account.clone(),
                });
            }
            backends => {
                return Err(IssueError::AmbiguousAccount {
                    provider: target.provider.clone(),
                    accounts: backends
                        .iter()
                        .map(|backend| backend.account.clone())
                        .collect(),
                });
            }
        };
        let issue = backend.inner.create_issue(draft).await?;
        if issue.provider != target.provider {
            return Err(IssueError::Policy(
                "tracker created the issue on a different provider",
            ));
        }
        validate_issue_identity(&issue)?;
        Ok(stamp_issue(issue, &backend.account))
    }

    pub async fn update_issue(&self, id: &str, patch: &IssuePatch) -> Result<Issue, IssueError> {
        validate_control_issue_id(id)?;
        match self.backend_for_id(id) {
            Ok(Some(b)) => {
                validate_patch_capabilities(b.inner.caps(), patch)?;
                b.inner.update_issue(id, patch).await.and_then(|issue| {
                    validate_issue_identity(&issue)?;
                    Ok(stamp_issue(issue, &b.account))
                })
            }
            Ok(None) => Err(self.id_miss(id)),
            Err(error) => Err(error),
        }
    }

    /// Post a comment on the issue identified by a `"<provider>:<key>"` id.
    pub async fn add_comment(&self, id: &str, body: &str) -> Result<(), IssueError> {
        validate_control_issue_id(id)?;
        match self.backend_for_id(id) {
            Ok(Some(b)) => b.inner.add_comment(id, body).await,
            Ok(None) => Err(IssueError::NotConfigured),
            Err(error) => Err(error),
        }
    }

    /// Attach a label (by name) to the issue identified by its id.
    pub async fn attach_label(&self, id: &str, label: &str) -> Result<(), IssueError> {
        validate_control_issue_id(id)?;
        match self.backend_for_id(id) {
            Ok(Some(b)) => b.inner.attach_label(id, label).await,
            Ok(None) => Err(IssueError::NotConfigured),
            Err(error) => Err(error),
        }
    }

    /// Remove a label (by name) from the issue identified by its id.
    pub async fn detach_label(&self, id: &str, label: &str) -> Result<(), IssueError> {
        validate_control_issue_id(id)?;
        match self.backend_for_id(id) {
            Ok(Some(b)) => b.inner.detach_label(id, label).await,
            Ok(None) => Err(IssueError::NotConfigured),
            Err(error) => Err(error),
        }
    }

    /// The first configured Kaneo backend, for board/project browsing.
    pub fn kaneo(&self) -> Option<&kaneo::KaneoBackend> {
        self.inner.iter().find_map(|b| b.inner.as_kaneo())
    }

    /// Search across all accounts concurrently with bounded reads, then
    /// interleave ranked rows in configuration order with one global cap.
    pub async fn search(&self, query: &str, limit: usize) -> Result<Vec<Issue>, IssueError> {
        let futures = self
            .inner
            .iter()
            .map(|b| (b, async move { b.inner.search(query, limit).await }))
            .collect();
        let results = fan_out_until(futures, FANOUT_DEADLINE).await;
        let mut per_account = Vec::new();
        for (b, result) in results {
            let result = match result {
                Err(error) => {
                    tracing::warn!(account = %b.account, provider = b.inner.provider_id(), error = %error, "issue search account exceeded fan-out deadline");
                    continue;
                }
                Ok(result) => result,
            };
            match result {
                Ok(issues) => match validate_issues(issues) {
                    Ok(valid) => per_account.push(stamp_issues(valid, &b.account)),
                    Err(e) => {
                        tracing::warn!(provider = b.inner.provider_id(), error = %e, "issue identity rejected")
                    }
                },
                Err(IssueError::PartialList { issues, reason }) => {
                    let valid = issues
                        .into_iter()
                        .filter_map(|issue| match validate_issue_identity(&issue) {
                            Ok(()) => Some(stamp_issue(issue, &b.account)),
                            Err(error) => {
                                tracing::warn!(
                                    account = %b.account,
                                    provider = b.inner.provider_id(),
                                    error = %error,
                                    "partial issue identity rejected"
                                );
                                None
                            }
                        })
                        .collect::<Vec<_>>();
                    tracing::warn!(
                        account = %b.account,
                        provider = b.inner.provider_id(),
                        partial_count = valid.len(),
                        reason,
                        "issue search is partial; retaining rows"
                    );
                    per_account.push(valid);
                }
                Err(e) => {
                    tracing::warn!(account = %b.account, provider = b.inner.provider_id(), error = %e, "issue search failed")
                }
            }
        }
        Ok(merge_fair(per_account, limit))
    }
}

fn validate_issue_ref(issue_ref: &IssueRef) -> Result<(), IssueError> {
    issue_ref
        .validate()
        .map_err(|error| IssueError::Parse(error.to_string()))?;
    validate_control_issue_id(&issue_ref.id)
}

fn stamp_issue(mut issue: Issue, account: &str) -> Issue {
    issue.account = Some(account.to_string());
    issue
}

fn stamp_issues(issues: Vec<Issue>, account: &str) -> Vec<Issue> {
    issues
        .into_iter()
        .map(|issue| stamp_issue(issue, account))
        .collect()
}

async fn fan_out_until<M, T, Fut>(
    futures: Vec<(M, Fut)>,
    deadline: Duration,
) -> Vec<(M, Result<T, IssueError>)>
where
    Fut: Future<Output = T>,
{
    let deadline_at = tokio::time::Instant::now() + deadline;
    stream::iter(futures.into_iter().map(|(metadata, future)| async move {
        let result = tokio::time::timeout_at(deadline_at, future)
            .await
            .map_err(|_| IssueError::Timeout("account read exceeded fan-out deadline"));
        (metadata, result)
    }))
    .buffered(MAX_CONCURRENT_ACCOUNTS)
    .collect()
    .await
}

/// Interleave account-ranked rows fairly, retaining the first row for each
/// stable issue id. Dedup identity is `Issue.id`; cross-workspace Linear key
/// collisions are conflated, as elsewhere. A zero limit means unbounded,
/// matching provider semantics.
fn merge_fair(per_account: Vec<Vec<Issue>>, limit: usize) -> Vec<Issue> {
    let mut positions = vec![0; per_account.len()];
    let mut seen = HashSet::new();
    let mut merged = Vec::new();
    loop {
        let mut advanced = false;
        for (account, issues) in per_account.iter().enumerate() {
            let Some(issue) = issues.get(positions[account]) else {
                continue;
            };
            positions[account] += 1;
            advanced = true;
            if seen.insert(issue.id.clone()) {
                merged.push(issue.clone());
                if limit > 0 && merged.len() == limit {
                    return merged;
                }
            }
        }
        if !advanced {
            return merged;
        }
    }
}

fn validate_issues(issues: Vec<Issue>) -> Result<Vec<Issue>, IssueError> {
    issues
        .into_iter()
        .map(|issue| {
            validate_issue_identity(&issue)?;
            Ok(issue)
        })
        .collect()
}

fn validate_patch_capabilities(caps: IssueCaps, patch: &IssuePatch) -> Result<(), IssueError> {
    let mut requested = Vec::new();
    if patch.status.is_some() {
        requested.push(("status", caps.patch_status));
    }
    if patch.title.is_some() {
        requested.push(("title", caps.patch_title));
    }
    if patch.assignee_me.is_some() {
        requested.push(("assignee_me", caps.patch_assignee_me));
    }
    if patch.priority.is_some() {
        requested.push(("priority", caps.patch_priority));
    }
    if requested.is_empty() {
        return Err(IssueError::EmptyPatch);
    }
    let unsupported: Vec<_> = requested
        .into_iter()
        .filter_map(|(field, supported)| (!supported).then_some(field))
        .collect();
    if unsupported.is_empty() {
        Ok(())
    } else {
        Err(IssueError::UnsupportedPatchFields(unsupported))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use thegn_core::config::IssueProviderKind;

    struct CreateBackend {
        provider: String,
        issue: Issue,
        calls: Arc<std::sync::atomic::AtomicUsize>,
    }

    impl IssueBackend for CreateBackend {
        fn provider_id(&self) -> &str {
            &self.provider
        }

        fn caps(&self) -> IssueCaps {
            IssueCaps::default()
        }

        fn list_issues<'a>(
            &'a self,
            _filter: &'a IssueFilter,
        ) -> BoxFuture<'a, Result<Vec<Issue>, IssueError>> {
            Box::pin(async { Ok(Vec::new()) })
        }

        fn get_issue<'a>(&'a self, _id: &'a str) -> BoxFuture<'a, Result<IssueDetail, IssueError>> {
            Box::pin(async { Err(IssueError::Api("not used".into())) })
        }

        fn create_issue<'a>(
            &'a self,
            _draft: &'a IssueDraft,
        ) -> BoxFuture<'a, Result<Issue, IssueError>> {
            let issue = self.issue.clone();
            let calls = Arc::clone(&self.calls);
            Box::pin(async move {
                calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                Ok(issue)
            })
        }

        fn update_issue<'a>(
            &'a self,
            _id: &'a str,
            _patch: &'a IssuePatch,
        ) -> BoxFuture<'a, Result<Issue, IssueError>> {
            Box::pin(async { Err(IssueError::Api("not used".into())) })
        }

        fn search<'a>(
            &'a self,
            _query: &'a str,
            _limit: usize,
        ) -> BoxFuture<'a, Result<Vec<Issue>, IssueError>> {
            Box::pin(async { Ok(Vec::new()) })
        }
    }

    fn create_backend(
        provider: &str,
        key: &str,
    ) -> (Box<dyn IssueBackend>, Arc<std::sync::atomic::AtomicUsize>) {
        let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let issue = Issue {
            id: format!("{provider}:{key}"),
            number: key.into(),
            provider: provider.into(),
            title: key.into(),
            status: thegn_core::issue::IssueStatus::Todo,
            priority: thegn_core::issue::IssuePriority::Low,
            url: "https://example.com/issues/item".into(),
            ..Default::default()
        };
        (
            Box::new(CreateBackend {
                provider: provider.into(),
                issue,
                calls: Arc::clone(&calls),
            }),
            calls,
        )
    }

    fn create_runtime() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
    }

    fn create_target(provider: &str, account: Option<&str>) -> CreateTarget {
        CreateTarget {
            provider: provider.into(),
            account: account.map(str::to_owned),
        }
    }

    #[test]
    fn create_on_targets_kaneo_even_when_linear_is_first() {
        let runtime = create_runtime();
        let mut router = IssueRouter::from_config(&IssuesConfig::default());
        let (linear, linear_calls) = create_backend("linear", "THE-1");
        let (kaneo, kaneo_calls) = create_backend("kaneo", "task-1");
        router
            .push_backend("linear-account".into(), linear)
            .unwrap();
        router.push_backend("kaneo-account".into(), kaneo).unwrap();

        let created = runtime
            .block_on(router.create_issue_on(&create_target("kaneo", None), &IssueDraft::default()))
            .unwrap();

        assert_eq!(created.provider, "kaneo");
        assert_eq!(created.account.as_deref(), Some("kaneo-account"));
        assert_eq!(linear_calls.load(std::sync::atomic::Ordering::SeqCst), 0);
        assert_eq!(kaneo_calls.load(std::sync::atomic::Ordering::SeqCst), 1);
    }

    #[test]
    fn create_on_two_kaneo_accounts_without_an_account_is_ambiguous_and_calls_nothing() {
        let runtime = create_runtime();
        let mut router = IssueRouter::from_config(&IssuesConfig::default());
        let (first, first_calls) = create_backend("kaneo", "task-1");
        let (second, second_calls) = create_backend("kaneo", "task-2");
        router.push_backend("personal".into(), first).unwrap();
        router.push_backend("work".into(), second).unwrap();

        let result = runtime.block_on(
            router.create_issue_on(&create_target("kaneo", None), &IssueDraft::default()),
        );

        assert!(
            matches!(result, Err(IssueError::AmbiguousAccount { provider, accounts })
            if provider == "kaneo"
                && accounts == vec!["personal".to_string(), "work".to_string()])
        );
        assert_eq!(first_calls.load(std::sync::atomic::Ordering::SeqCst), 0);
        assert_eq!(second_calls.load(std::sync::atomic::Ordering::SeqCst), 0);
    }

    #[test]
    fn create_on_named_account_picks_that_account_regardless_of_order() {
        let runtime = create_runtime();
        for reverse in [false, true] {
            let mut router = IssueRouter::from_config(&IssuesConfig::default());
            let (personal, personal_calls) = create_backend("kaneo", "task-personal");
            let (work, work_calls) = create_backend("kaneo", "task-work");
            let entries = if reverse {
                [("work", work), ("personal", personal)]
            } else {
                [("personal", personal), ("work", work)]
            };
            for (account, backend) in entries {
                router.push_backend(account.into(), backend).unwrap();
            }

            let created = runtime
                .block_on(router.create_issue_on(
                    &create_target("kaneo", Some("work")),
                    &IssueDraft::default(),
                ))
                .unwrap();

            assert_eq!(created.id, "kaneo:task-work");
            assert_eq!(created.account.as_deref(), Some("work"));
            assert_eq!(personal_calls.load(std::sync::atomic::Ordering::SeqCst), 0);
            assert_eq!(work_calls.load(std::sync::atomic::Ordering::SeqCst), 1);
        }
    }

    #[test]
    fn create_on_rejects_a_wrong_origin_issue() {
        let runtime = create_runtime();
        // The backend is registered as Kaneo but incorrectly returns Linear origin.
        let wrong_origin = Issue {
            provider: "linear".into(),
            id: "linear:THE-1".into(),
            number: "THE-1".into(),
            ..Default::default()
        };
        let mut wrong_router = IssueRouter::from_config(&IssuesConfig::default());
        let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        wrong_router
            .push_backend(
                "kaneo-account".into(),
                Box::new(CreateBackend {
                    provider: "kaneo".into(),
                    issue: wrong_origin,
                    calls,
                }),
            )
            .unwrap();

        assert!(matches!(
            runtime.block_on(
                wrong_router
                    .create_issue_on(&create_target("kaneo", None), &IssueDraft::default(),)
            ),
            Err(IssueError::Policy(
                "tracker created the issue on a different provider"
            ))
        ));
    }

    #[test]
    fn create_on_an_empty_router_is_not_configured() {
        let runtime = create_runtime();
        let router = IssueRouter::from_config(&IssuesConfig::default());
        assert!(matches!(
            runtime.block_on(
                router.create_issue_on(&create_target("kaneo", None), &IssueDraft::default(),)
            ),
            Err(IssueError::NotConfigured)
        ));
    }

    #[test]
    fn bare_create_uses_the_only_configured_account() {
        let runtime = create_runtime();
        let mut router = IssueRouter::from_config(&IssuesConfig::default());
        let (backend, calls) = create_backend("kaneo", "task-1");
        router
            .push_backend("kaneo-account".into(), backend)
            .unwrap();

        let created = runtime
            .block_on(router.create_issue(&IssueDraft::default()))
            .unwrap();

        assert_eq!(created.id, "kaneo:task-1");
        assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 1);
    }

    #[test]
    fn bare_create_with_two_backends_is_ambiguous() {
        let runtime = create_runtime();
        let mut router = IssueRouter::from_config(&IssuesConfig::default());
        let (linear, linear_calls) = create_backend("linear", "THE-1");
        let (kaneo, kaneo_calls) = create_backend("kaneo", "task-1");
        router
            .push_backend("linear-account".into(), linear)
            .unwrap();
        router.push_backend("kaneo-account".into(), kaneo).unwrap();

        assert!(matches!(
            runtime.block_on(router.create_issue(&IssueDraft::default())),
            Err(IssueError::AmbiguousAccount { provider, accounts })
                if provider == "any"
                    && accounts == vec!["linear-account".to_string(), "kaneo-account".to_string()]
        ));
        assert_eq!(linear_calls.load(std::sync::atomic::Ordering::SeqCst), 0);
        assert_eq!(kaneo_calls.load(std::sync::atomic::Ordering::SeqCst), 0);
    }

    #[test]
    fn create_on_missing_target_names_provider_and_account() {
        let runtime = create_runtime();
        let mut router = IssueRouter::from_config(&IssuesConfig::default());
        let (backend, calls) = create_backend("linear", "THE-1");
        router
            .push_backend("linear-account".into(), backend)
            .unwrap();

        assert!(matches!(
            runtime.block_on(router.create_issue_on(
                &create_target("kaneo", Some("project")),
                &IssueDraft::default(),
            )),
            Err(IssueError::NoCreateTarget { provider, account })
                if provider == "kaneo" && account.as_deref() == Some("project")
        ));
        assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 0);
    }

    #[test]
    fn create_on_missing_provider_without_account_names_provider_and_calls_nothing() {
        let runtime = create_runtime();
        let mut router = IssueRouter::from_config(&IssuesConfig::default());
        let (backend, calls) = create_backend("linear", "THE-1");
        router
            .push_backend("linear-account".into(), backend)
            .unwrap();

        let result = runtime.block_on(
            router.create_issue_on(&create_target("kaneo", None), &IssueDraft::default()),
        );

        assert!(matches!(
            result,
            Err(IssueError::NoCreateTarget { provider, account })
                if provider == "kaneo" && account.is_none()
        ));
        assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 0);
    }

    fn cfg_with(providers: Vec<IssueProviderKind>) -> IssuesConfig {
        IssuesConfig {
            providers,
            ..Default::default()
        }
    }

    #[test]
    fn unconfigured_router_is_empty() {
        let r = IssueRouter::from_config(&IssuesConfig::default());
        assert!(!r.is_configured());
        assert!(r.provider_ids().is_empty());
        assert_eq!(r.provider_id(), "none");
    }

    #[test]
    fn single_provider_back_compat() {
        let cfg = IssuesConfig {
            provider: IssueProviderKind::Linear,
            ..Default::default()
        };
        let r = IssueRouter::from_config(&cfg);
        assert!(r.is_configured());
        assert_eq!(r.provider_ids(), vec!["linear"]);
    }

    #[test]
    fn builds_one_backend_per_active_provider() {
        let r = IssueRouter::from_config(&cfg_with(vec![
            IssueProviderKind::Linear,
            IssueProviderKind::Jira,
            IssueProviderKind::Github,
            IssueProviderKind::Kaneo,
        ]));
        assert_eq!(r.provider_ids(), vec!["linear", "jira", "github", "kaneo"]);
        // The representative id is the first configured provider.
        assert_eq!(r.provider_id(), "linear");
    }

    #[test]
    fn multiple_accounts_of_one_provider_each_build_a_backend() {
        use thegn_core::config::IssueAccount;
        let cfg = IssuesConfig {
            issue_accounts: vec![
                IssueAccount {
                    name: "personal".into(),
                    provider: IssueProviderKind::Linear,
                    ..Default::default()
                },
                IssueAccount {
                    name: "work".into(),
                    provider: IssueProviderKind::Linear,
                    ..Default::default()
                },
                // A disabled account is skipped.
                IssueAccount {
                    name: "old".into(),
                    provider: IssueProviderKind::Jira,
                    enabled: false,
                    ..Default::default()
                },
            ],
            ..Default::default()
        };
        let r = IssueRouter::from_config(&cfg);
        assert_eq!(r.provider_ids(), vec!["linear", "linear"]);
    }

    #[test]
    fn dispatch_by_id_prefix() {
        let r = IssueRouter::from_config(&cfg_with(vec![
            IssueProviderKind::Linear,
            IssueProviderKind::Jira,
            IssueProviderKind::Kaneo,
        ]));
        assert_eq!(
            r.backend_for_id("jira:PROJ-1")
                .unwrap()
                .map(|b| b.inner.provider_id()),
            Some("jira")
        );
        assert_eq!(
            r.backend_for_id("linear:ABC-9")
                .unwrap()
                .map(|b| b.inner.provider_id()),
            Some("linear")
        );
        assert_eq!(
            r.backend_for_id("kaneo:abc123")
                .unwrap()
                .map(|b| b.inner.provider_id()),
            Some("kaneo")
        );
        // An id for a provider that isn't configured routes nowhere.
        assert!(r.backend_for_id("github:42").unwrap().is_none());
        // A bare id with no prefix also routes nowhere — three backends, so
        // the single-backend fallback below does not apply.
        assert!(r.backend_for_id("nonsense").unwrap().is_none());
    }

    #[test]
    fn bare_id_routes_to_the_only_backend() {
        let r = IssueRouter::from_config(&cfg_with(vec![IssueProviderKind::Linear]));
        // `wt new --from-issue THE-72` on a single-tracker machine.
        assert_eq!(
            r.backend_for_id("THE-72")
                .unwrap()
                .map(|b| b.inner.provider_id()),
            Some("linear")
        );
        // An explicit prefix still wins outright (same backend here).
        assert_eq!(
            r.backend_for_id("linear:THE-72")
                .unwrap()
                .map(|b| b.inner.provider_id()),
            Some("linear")
        );
    }

    #[test]
    fn bare_id_with_two_backends_routes_nowhere() {
        let r = IssueRouter::from_config(&cfg_with(vec![
            IssueProviderKind::Linear,
            IssueProviderKind::Jira,
        ]));
        assert!(r.backend_for_id("THE-72").unwrap().is_none());
        // …but an explicit prefix wins over the (absent) fallback, and picks
        // the named provider rather than the first configured one.
        assert_eq!(
            r.backend_for_id("jira:THE-72")
                .unwrap()
                .map(|b| b.inner.provider_id()),
            Some("jira")
        );
    }

    #[test]
    fn single_backend_does_not_fallback_foreign_namespace() {
        let r = IssueRouter::from_config(&cfg_with(vec![IssueProviderKind::Linear]));
        assert!(r.backend_for_id("jira:PROJ-1").unwrap().is_none());
    }

    #[test]
    fn builtin_patch_capability_matrix_matches_implemented_fields() {
        let providers: Vec<(Box<dyn IssueBackend>, bool)> = vec![
            (
                Box::new(github::GitHubIssuesBackend::new(Vec::new())),
                false,
            ),
            (
                Box::new(jira::JiraBackend::new(
                    "https://example.atlassian.net".into(),
                    "user".into(),
                    "token".into(),
                    None,
                )),
                false,
            ),
            (
                Box::new(linear::LinearBackend::new("key".into(), None)),
                true,
            ),
            (
                Box::new(kaneo::KaneoBackend::new(
                    "https://kaneo.example".into(),
                    "key".into(),
                    None,
                    None,
                )),
                true,
            ),
        ];
        for (backend, supports_priority) in providers {
            let caps = backend.caps();
            assert!(caps.patch_status, "{} status", backend.provider_id());
            assert!(caps.patch_title, "{} title", backend.provider_id());
            assert!(
                !caps.patch_assignee_me,
                "{} assignee",
                backend.provider_id()
            );
            assert_eq!(
                caps.patch_priority,
                supports_priority,
                "{} priority",
                backend.provider_id()
            );
            assert!(
                validate_patch_capabilities(
                    caps,
                    &IssuePatch {
                        status: Some(thegn_core::issue::IssueStatus::Todo),
                        ..Default::default()
                    }
                )
                .is_ok()
            );
            assert!(matches!(
                validate_patch_capabilities(
                    caps,
                    &IssuePatch {
                        assignee_me: Some(true),
                        ..Default::default()
                    }
                ),
                Err(IssueError::UnsupportedPatchFields(fields))
                    if fields == vec!["assignee_me"]
            ));
        }
    }

    struct CountingBackend {
        updates: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    }

    impl IssueBackend for CountingBackend {
        fn provider_id(&self) -> &str {
            "linear"
        }

        fn caps(&self) -> IssueCaps {
            IssueCaps {
                patch_status: true,
                patch_title: true,
                ..Default::default()
            }
        }

        fn list_issues<'a>(
            &'a self,
            _filter: &'a IssueFilter,
        ) -> BoxFuture<'a, Result<Vec<Issue>, IssueError>> {
            Box::pin(async { Ok(Vec::new()) })
        }

        fn get_issue<'a>(&'a self, _id: &'a str) -> BoxFuture<'a, Result<IssueDetail, IssueError>> {
            Box::pin(async { Err(IssueError::Api("fake get".into())) })
        }

        fn create_issue<'a>(
            &'a self,
            _draft: &'a IssueDraft,
        ) -> BoxFuture<'a, Result<Issue, IssueError>> {
            Box::pin(async { Err(IssueError::Api("fake create".into())) })
        }

        fn update_issue<'a>(
            &'a self,
            _id: &'a str,
            _patch: &'a IssuePatch,
        ) -> BoxFuture<'a, Result<Issue, IssueError>> {
            let updates = self.updates.clone();
            Box::pin(async move {
                updates.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                Err(IssueError::Api("fake update".into()))
            })
        }

        fn search<'a>(
            &'a self,
            _query: &'a str,
            _limit: usize,
        ) -> BoxFuture<'a, Result<Vec<Issue>, IssueError>> {
            Box::pin(async { Ok(Vec::new()) })
        }
    }

    struct PartialListBackend;

    impl PartialListBackend {
        fn partial_issue() -> Issue {
            Issue {
                id: "linear:ABC-1".into(),
                number: "1".into(),
                provider: "linear".into(),
                title: "partial row".into(),
                status: thegn_core::issue::IssueStatus::Todo,
                priority: thegn_core::issue::IssuePriority::Low,
                url: "https://linear.app/acme/issue/ABC-1".into(),
                ..Default::default()
            }
        }
    }

    impl IssueBackend for PartialListBackend {
        fn provider_id(&self) -> &str {
            "linear"
        }

        fn caps(&self) -> IssueCaps {
            IssueCaps::default()
        }

        fn list_issues<'a>(
            &'a self,
            _filter: &'a IssueFilter,
        ) -> BoxFuture<'a, Result<Vec<Issue>, IssueError>> {
            let issue = Self::partial_issue();
            Box::pin(async move {
                Err(IssueError::PartialList {
                    issues: vec![issue],
                    reason: "test partial page",
                })
            })
        }

        fn get_issue<'a>(&'a self, _id: &'a str) -> BoxFuture<'a, Result<IssueDetail, IssueError>> {
            Box::pin(async { Err(IssueError::Api("not used".into())) })
        }

        fn create_issue<'a>(
            &'a self,
            _draft: &'a IssueDraft,
        ) -> BoxFuture<'a, Result<Issue, IssueError>> {
            Box::pin(async { Err(IssueError::Api("not used".into())) })
        }

        fn update_issue<'a>(
            &'a self,
            _id: &'a str,
            _patch: &'a IssuePatch,
        ) -> BoxFuture<'a, Result<Issue, IssueError>> {
            Box::pin(async { Err(IssueError::Api("not used".into())) })
        }

        fn search<'a>(
            &'a self,
            _query: &'a str,
            _limit: usize,
        ) -> BoxFuture<'a, Result<Vec<Issue>, IssueError>> {
            let issue = Self::partial_issue();
            Box::pin(async move {
                Err(IssueError::PartialList {
                    issues: vec![issue],
                    reason: "test partial page",
                })
            })
        }
    }

    struct BarrierBackend {
        barrier: Arc<tokio::sync::Barrier>,
        notify: Option<Arc<tokio::sync::Notify>>,
        notify_signal: bool,
        issue: Issue,
    }

    struct FixedBackend {
        id: String,
        issues: Vec<Issue>,
    }

    impl IssueBackend for FixedBackend {
        fn provider_id(&self) -> &str {
            &self.id
        }

        fn caps(&self) -> IssueCaps {
            IssueCaps::default()
        }

        fn list_issues<'a>(
            &'a self,
            filter: &'a IssueFilter,
        ) -> BoxFuture<'a, Result<Vec<Issue>, IssueError>> {
            Box::pin(async move {
                let mut issues = self.issues.clone();
                if filter.limit > 0 {
                    issues.truncate(filter.limit);
                }
                Ok(issues)
            })
        }

        fn get_issue<'a>(&'a self, _id: &'a str) -> BoxFuture<'a, Result<IssueDetail, IssueError>> {
            Box::pin(async { Err(IssueError::Api("not used".into())) })
        }

        fn create_issue<'a>(
            &'a self,
            _draft: &'a IssueDraft,
        ) -> BoxFuture<'a, Result<Issue, IssueError>> {
            Box::pin(async { Err(IssueError::Api("not used".into())) })
        }

        fn update_issue<'a>(
            &'a self,
            _id: &'a str,
            _patch: &'a IssuePatch,
        ) -> BoxFuture<'a, Result<Issue, IssueError>> {
            Box::pin(async { Err(IssueError::Api("not used".into())) })
        }

        fn search<'a>(
            &'a self,
            _query: &'a str,
            limit: usize,
        ) -> BoxFuture<'a, Result<Vec<Issue>, IssueError>> {
            Box::pin(async move {
                let mut issues = self.issues.clone();
                if limit > 0 {
                    issues.truncate(limit);
                }
                Ok(issues)
            })
        }
    }

    fn fixed_issue(id: &str) -> Issue {
        Issue {
            id: format!("linear:{id}"),
            number: id.into(),
            provider: "linear".into(),
            title: id.into(),
            status: thegn_core::issue::IssueStatus::Todo,
            priority: thegn_core::issue::IssuePriority::Low,
            url: format!("https://linear.app/acme/issue/{id}"),
            ..Default::default()
        }
    }

    fn fixed_router(accounts: Vec<(&str, Vec<Issue>)>) -> IssueRouter {
        let mut router = IssueRouter::from_config(&IssuesConfig::default());
        for (account, issues) in accounts {
            router
                .push_backend(
                    account.into(),
                    Box::new(FixedBackend {
                        id: "linear".into(),
                        issues,
                    }),
                )
                .unwrap();
        }
        router
    }

    impl IssueBackend for BarrierBackend {
        fn provider_id(&self) -> &str {
            "linear"
        }

        fn caps(&self) -> IssueCaps {
            IssueCaps::default()
        }

        fn list_issues<'a>(
            &'a self,
            _filter: &'a IssueFilter,
        ) -> BoxFuture<'a, Result<Vec<Issue>, IssueError>> {
            Box::pin(async move {
                if let Some(notify) = &self.notify {
                    if self.notify_signal {
                        notify.notify_one();
                    } else {
                        notify.notified().await;
                    }
                } else {
                    self.barrier.wait().await;
                }
                Ok(vec![self.issue.clone()])
            })
        }

        fn get_issue<'a>(&'a self, _id: &'a str) -> BoxFuture<'a, Result<IssueDetail, IssueError>> {
            Box::pin(async { Err(IssueError::Api("not used".into())) })
        }

        fn create_issue<'a>(
            &'a self,
            _draft: &'a IssueDraft,
        ) -> BoxFuture<'a, Result<Issue, IssueError>> {
            Box::pin(async { Err(IssueError::Api("not used".into())) })
        }

        fn update_issue<'a>(
            &'a self,
            _id: &'a str,
            _patch: &'a IssuePatch,
        ) -> BoxFuture<'a, Result<Issue, IssueError>> {
            Box::pin(async { Err(IssueError::Api("not used".into())) })
        }

        fn search<'a>(
            &'a self,
            _query: &'a str,
            _limit: usize,
        ) -> BoxFuture<'a, Result<Vec<Issue>, IssueError>> {
            Box::pin(async move {
                self.barrier.wait().await;
                Ok(vec![self.issue.clone()])
            })
        }
    }

    fn barrier_router(barrier: Arc<tokio::sync::Barrier>) -> IssueRouter {
        let mut router = IssueRouter::from_config(&IssuesConfig::default());
        for (account, suffix) in [("A", "1"), ("B", "2")] {
            let issue = Issue {
                id: format!("linear:ABC-{suffix}"),
                number: suffix.into(),
                provider: "linear".into(),
                title: account.into(),
                status: thegn_core::issue::IssueStatus::Todo,
                priority: thegn_core::issue::IssuePriority::Low,
                url: format!("https://linear.app/acme/issue/ABC-{suffix}"),
                ..Default::default()
            };
            router
                .push_backend(
                    account.into(),
                    Box::new(BarrierBackend {
                        barrier: barrier.clone(),
                        notify: None,
                        notify_signal: false,
                        issue,
                    }),
                )
                .unwrap();
        }
        router
    }

    fn ordering_router() -> IssueRouter {
        let mut router = IssueRouter::from_config(&IssuesConfig::default());
        let notify = Arc::new(tokio::sync::Notify::new());
        for (account, suffix, notify_signal) in [("A", "1", false), ("B", "2", true)] {
            let issue = Issue {
                id: format!("linear:ABC-{suffix}"),
                number: suffix.into(),
                provider: "linear".into(),
                title: account.into(),
                status: thegn_core::issue::IssueStatus::Todo,
                priority: thegn_core::issue::IssuePriority::Low,
                url: format!("https://linear.app/acme/issue/ABC-{suffix}"),
                ..Default::default()
            };
            router
                .push_backend(
                    account.into(),
                    Box::new(BarrierBackend {
                        barrier: Arc::new(tokio::sync::Barrier::new(1)),
                        notify: Some(notify.clone()),
                        notify_signal,
                        issue,
                    }),
                )
                .unwrap();
        }
        router
    }

    #[test]
    fn fan_out_futures_are_send() {
        fn assert_send<T: Send>(_: &T) {}

        let router = barrier_router(Arc::new(tokio::sync::Barrier::new(2)));
        let filter = IssueFilter::default();
        assert_send(&router.list_per_provider(&filter));
        assert_send(&router.list_issues(&filter));
        assert_send(&router.search("query", 10));
    }

    #[test]
    fn list_fanout_runs_concurrently_and_per_provider_keeps_config_order() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let filter = IssueFilter::default();
        let list_router = barrier_router(Arc::new(tokio::sync::Barrier::new(2)));
        let listed = runtime.block_on(async {
            tokio::time::timeout(
                std::time::Duration::from_secs(2),
                list_router.list_issues(&filter),
            )
            .await
        });
        assert!(
            listed.is_ok(),
            "list calls must overlap to release the barrier"
        );
        assert_eq!(listed.unwrap().unwrap().len(), 2);

        let provider_router = barrier_router(Arc::new(tokio::sync::Barrier::new(2)));
        let providers = runtime.block_on(async {
            tokio::time::timeout(
                std::time::Duration::from_secs(2),
                provider_router.list_per_provider(&filter),
            )
            .await
        });
        assert!(
            providers.is_ok(),
            "per-provider calls must overlap to release the barrier"
        );
        assert_eq!(providers.unwrap().len(), 2);
    }

    #[test]
    fn per_provider_results_keep_config_order_when_a_later_account_finishes_first() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let router = ordering_router();
        let providers = runtime.block_on(async {
            tokio::time::timeout(
                std::time::Duration::from_secs(2),
                router.list_per_provider(&IssueFilter::default()),
            )
            .await
        });
        assert!(
            providers.is_ok(),
            "the later account must run to release the first account"
        );
        let providers = providers.unwrap();
        assert_eq!(providers[0].0, "A");
        assert_eq!(providers[1].0, "B");
    }

    #[test]
    fn search_fanout_runs_concurrently() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let router = barrier_router(Arc::new(tokio::sync::Barrier::new(2)));
        let searched = runtime.block_on(async {
            tokio::time::timeout(
                std::time::Duration::from_secs(2),
                router.search("query", 10),
            )
            .await
        });
        assert!(
            searched.is_ok(),
            "search calls must overlap to release the barrier"
        );
        assert_eq!(searched.unwrap().unwrap().len(), 2);
    }

    #[test]
    fn search_caps_dedups_and_interleaves_accounts() {
        let router = fixed_router(vec![
            (
                "A",
                vec![
                    fixed_issue("a1"),
                    fixed_issue("a2"),
                    fixed_issue("a3"),
                    fixed_issue("dup"),
                ],
            ),
            ("B", vec![fixed_issue("b1"), fixed_issue("dup")]),
        ]);
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();

        let capped = runtime.block_on(router.search("q", 3)).unwrap();
        assert_eq!(
            capped
                .iter()
                .map(|issue| issue.number.as_str())
                .collect::<Vec<_>>(),
            vec!["a1", "b1", "a2"]
        );
        let unbounded = runtime.block_on(router.search("q", 0)).unwrap();
        assert_eq!(unbounded.len(), 5);
        assert_eq!(
            unbounded
                .iter()
                .filter(|issue| issue.number == "dup")
                .count(),
            1
        );
    }

    #[test]
    fn list_issues_applies_one_final_cap() {
        let router = fixed_router(vec![
            (
                "A",
                vec![fixed_issue("a1"), fixed_issue("a2"), fixed_issue("a3")],
            ),
            (
                "B",
                vec![fixed_issue("b1"), fixed_issue("b2"), fixed_issue("b3")],
            ),
        ]);
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let issues = runtime
            .block_on(router.list_issues(&IssueFilter::my_open(2)))
            .unwrap();
        assert_eq!(issues.len(), 2);
    }

    #[test]
    fn limit_exactly_equal_to_available_rows_and_single_account_keep_order() {
        let two_accounts = fixed_router(vec![
            (
                "A",
                vec![fixed_issue("a1"), fixed_issue("a2"), fixed_issue("a3")],
            ),
            (
                "B",
                vec![fixed_issue("b1"), fixed_issue("b2"), fixed_issue("b3")],
            ),
        ]);
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let all = runtime.block_on(two_accounts.search("q", 6)).unwrap();
        assert_eq!(all.len(), 6);
        assert_eq!(
            all.iter()
                .map(|issue| issue.number.as_str())
                .collect::<Vec<_>>(),
            vec!["a1", "b1", "a2", "b2", "a3", "b3"]
        );

        let one_account = fixed_router(vec![(
            "A",
            vec![fixed_issue("a1"), fixed_issue("a2"), fixed_issue("a3")],
        )]);
        let ordered = runtime.block_on(one_account.search("q", 3)).unwrap();
        assert_eq!(
            ordered
                .iter()
                .map(|issue| issue.number.as_str())
                .collect::<Vec<_>>(),
            vec!["a1", "a2", "a3"]
        );
    }

    #[test]
    fn hung_account_is_reported_as_timeout_without_blocking_healthy_ones() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let healthy: BoxFuture<'static, usize> = Box::pin(async { 7 });
        let hung: BoxFuture<'static, usize> = Box::pin(std::future::pending());
        let results = runtime.block_on(async {
            tokio::time::timeout(
                Duration::from_secs(2),
                fan_out_until(vec![("A", healthy), ("B", hung)], Duration::from_millis(50)),
            )
            .await
        });
        let results = results.expect("fan-out deadline must bound the hung account");
        assert_eq!(results[0].0, "A");
        assert!(matches!(&results[0].1, Ok(7)));
        assert_eq!(results[1].0, "B");
        assert!(matches!(&results[1].1, Err(IssueError::Timeout(_))));
    }

    #[test]
    fn best_effort_listing_retains_partial_rows_while_per_provider_keeps_error() {
        let mut router = IssueRouter::from_config(&IssuesConfig::default());
        router
            .push_backend("partial".into(), Box::new(PartialListBackend))
            .unwrap();
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let issues = runtime
            .block_on(router.list_issues(&IssueFilter::default()))
            .unwrap();
        assert_eq!(issues.len(), 1);
        assert_eq!(issues[0].title, "partial row");
        assert_eq!(issues[0].account.as_deref(), Some("partial"));
        let per_provider = runtime.block_on(router.list_per_provider(&IssueFilter::default()));
        assert!(matches!(
            &per_provider[0].2,
            Err(IssueError::PartialList { issues, .. })
                if issues.len() == 1 && issues[0].account.as_deref() == Some("partial")
        ));
    }

    #[test]
    fn best_effort_search_retains_partial_rows() {
        let mut router = IssueRouter::from_config(&IssuesConfig::default());
        router
            .push_backend("partial".into(), Box::new(PartialListBackend))
            .unwrap();
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let issues = runtime.block_on(router.search("partial", 10)).unwrap();
        assert_eq!(issues.len(), 1);
        assert_eq!(issues[0].title, "partial row");
        assert_eq!(issues[0].account.as_deref(), Some("partial"));
    }

    struct PluginMarker {
        id: String,
    }

    impl IssueBackend for PluginMarker {
        fn provider_id(&self) -> &str {
            &self.id
        }
        fn caps(&self) -> IssueCaps {
            IssueCaps::default()
        }
        fn list_issues<'a>(
            &'a self,
            _f: &'a IssueFilter,
        ) -> BoxFuture<'a, Result<Vec<Issue>, IssueError>> {
            Box::pin(async { Ok(Vec::new()) })
        }
        fn get_issue<'a>(&'a self, _id: &'a str) -> BoxFuture<'a, Result<IssueDetail, IssueError>> {
            Box::pin(async { Err(IssueError::Api("marker".into())) })
        }
        fn create_issue<'a>(
            &'a self,
            _d: &'a IssueDraft,
        ) -> BoxFuture<'a, Result<Issue, IssueError>> {
            Box::pin(async { Err(IssueError::Api("marker".into())) })
        }
        fn update_issue<'a>(
            &'a self,
            _id: &'a str,
            _p: &'a IssuePatch,
        ) -> BoxFuture<'a, Result<Issue, IssueError>> {
            Box::pin(async { Err(IssueError::Api("marker".into())) })
        }
        fn search<'a>(
            &'a self,
            _q: &'a str,
            _l: usize,
        ) -> BoxFuture<'a, Result<Vec<Issue>, IssueError>> {
            Box::pin(async { Ok(Vec::new()) })
        }
    }

    #[test]
    fn plugin_namespace_routes_by_complete_registered_prefix() {
        let mut router = IssueRouter::from_config(&IssuesConfig::default());
        router
            .push_backend(
                "demo".into(),
                Box::new(PluginMarker {
                    id: "plugin:demo".into(),
                }),
            )
            .unwrap();
        assert!(
            router
                .push_backend(
                    "nested".into(),
                    Box::new(PluginMarker {
                        id: "plugin:demo:extra".into(),
                    }),
                )
                .is_err()
        );
        assert_eq!(
            router
                .backend_for_id("plugin:demo:opaque/key#7")
                .unwrap()
                .map(|b| b.inner.provider_id()),
            Some("plugin:demo")
        );
        assert_eq!(
            router
                .backend_for_id("plugin:demo:opaque:key")
                .unwrap()
                .map(|b| b.inner.provider_id()),
            Some("plugin:demo")
        );
        assert!(
            router
                .backend_for_id("plugin:other:opaque")
                .unwrap()
                .is_none()
        );
        assert!(validate_control_issue_id("plugin:demo:opaque/key#7").is_ok());
    }

    #[test]
    fn patch_gate_rejects_empty_and_unsupported_before_provider_call() {
        let updates = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let mut router = IssueRouter::from_config(&IssuesConfig::default());
        router
            .push_backend(
                "fake".into(),
                Box::new(CountingBackend {
                    updates: updates.clone(),
                }),
            )
            .unwrap();
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let patch = IssuePatch {
            status: Some(thegn_core::issue::IssueStatus::Todo),
            ..Default::default()
        };
        assert!(
            runtime
                .block_on(router.update_issue("linear:bad key", &patch))
                .is_err()
        );
        assert_eq!(updates.load(std::sync::atomic::Ordering::SeqCst), 0);
        assert!(
            runtime
                .block_on(router.update_issue("linear:ABC-9", &patch))
                .is_err()
        );
        // Status is supported by this backend, so its rejected API response
        // still means the provider was called once.
        assert_eq!(updates.load(std::sync::atomic::Ordering::SeqCst), 1);
        assert!(matches!(
            runtime.block_on(router.update_issue("linear:ABC-9", &IssuePatch::default())),
            Err(IssueError::EmptyPatch)
        ));
        assert_eq!(updates.load(std::sync::atomic::Ordering::SeqCst), 1);
        assert!(matches!(
            runtime.block_on(router.update_issue(
                "linear:ABC-9",
                &IssuePatch {
                    priority: Some(thegn_core::issue::IssuePriority::High),
                    ..Default::default()
                }
            )),
            Err(IssueError::UnsupportedPatchFields(fields)) if fields == vec!["priority"]
        ));
        assert_eq!(updates.load(std::sync::atomic::Ordering::SeqCst), 1);
        assert!(matches!(
            runtime.block_on(router.update_issue("linear:ABC-9", &patch)),
            Err(IssueError::Api(_))
        ));
        assert_eq!(updates.load(std::sync::atomic::Ordering::SeqCst), 2);
    }

    #[test]
    fn duplicate_provider_ids_reject_by_id_mutations() {
        let first = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let second = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let mut router = IssueRouter::from_config(&IssuesConfig::default());
        router
            .push_backend(
                "first".into(),
                Box::new(CountingBackend {
                    updates: first.clone(),
                }),
            )
            .unwrap();
        router
            .push_backend(
                "second".into(),
                Box::new(CountingBackend {
                    updates: second.clone(),
                }),
            )
            .unwrap();
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let patch = IssuePatch {
            status: Some(thegn_core::issue::IssueStatus::Todo),
            ..Default::default()
        };
        let result = runtime.block_on(router.update_issue("linear:ABC-9", &patch));
        assert!(matches!(result, Err(IssueError::AmbiguousAccount { .. })));
        assert_eq!(first.load(std::sync::atomic::Ordering::SeqCst), 0);
        assert_eq!(second.load(std::sync::atomic::Ordering::SeqCst), 0);
    }

    #[test]
    fn plugin_identity_envelope_is_bounded_before_routing() {
        let oversized = format!("plugin:demo:{}", "x".repeat(500));
        assert!(validate_control_issue_id(&oversized).is_err());
        let opaque = format!("plugin:demo:{}", "客户/任务#7");
        assert!(validate_control_issue_id(&opaque).is_ok());
    }

    #[test]
    fn cache_boundary_rejects_cross_provider_and_malformed_ids() {
        let mut issue: Issue = serde_json::from_value(serde_json::json!({
            "id": "github:owner/repo#42",
            "number": "42",
            "provider": "github",
            "title": "ok",
            "status": "todo",
            "priority": "low",
            "url": "",
            "updated_at_ms": 0
        }))
        .unwrap();
        assert!(validate_issue_identity(&issue).is_ok());
        issue.number = "0".into();
        assert!(validate_issue_identity(&issue).is_err());
        issue.number = "42".into();
        issue.project_ids = vec!["../project".into()];
        assert!(validate_issue_identity(&issue).is_err());
        issue.project_ids.clear();
        issue.blocked_by = vec!["github:owner/repo#0".into()];
        assert!(validate_issue_identity(&issue).is_err());
        issue.blocked_by.clear();
        issue.url = "https://user:password@example.com/issue/42".into();
        assert!(validate_issue_identity(&issue).is_err());
        issue.url = "https://kaneo.example/dashboard/project/p1/board?task=t1#focus".into();
        assert!(validate_issue_identity(&issue).is_ok());
        issue.url.clear();
        issue.id = "github:owner/repo#0".into();
        assert!(validate_issue_identity(&issue).is_err());
        issue.provider = "jira".into();
        assert!(validate_issue_identity(&issue).is_err());
    }

    #[test]
    fn id_miss_message_names_the_form_and_the_providers() {
        // Unconfigured stays NotConfigured.
        let empty = IssueRouter::from_config(&IssuesConfig::default());
        assert!(matches!(empty.id_miss("THE-72"), IssueError::NotConfigured));
        // Configured-but-unroutable names the expected form + the providers,
        // instead of claiming nothing is configured.
        let r = IssueRouter::from_config(&cfg_with(vec![
            IssueProviderKind::Linear,
            IssueProviderKind::Jira,
        ]));
        let msg = r.id_miss("THE-72").to_string();
        assert!(msg.contains("<provider>:<key>"), "{msg}");
        assert!(msg.contains("linear"), "{msg}");
        assert!(msg.contains("jira"), "{msg}");
        assert!(!msg.contains("no issue provider configured"), "{msg}");
    }

    #[test]
    fn id_miss_dedupes_repeated_providers() {
        use thegn_core::config::IssueAccount;
        let cfg = IssuesConfig {
            issue_accounts: vec![
                IssueAccount {
                    name: "personal".into(),
                    provider: IssueProviderKind::Linear,
                    ..Default::default()
                },
                IssueAccount {
                    name: "work".into(),
                    provider: IssueProviderKind::Linear,
                    ..Default::default()
                },
            ],
            ..Default::default()
        };
        let r = IssueRouter::from_config(&cfg);
        let msg = r.id_miss("x:1").to_string();
        assert_eq!(msg.matches("linear").count(), 1, "{msg}");
    }

    #[test]
    fn parse_due_date_ms_handles_date_only_and_rfc3339() {
        // Date-only (Linear dueDate / Jira duedate) => midnight UTC.
        assert_eq!(parse_due_date_ms("2026-08-20"), Some(1_787_184_000_000));
        // Full RFC3339 (Kaneo) passes through.
        assert_eq!(
            parse_due_date_ms("2026-08-20T12:30:00Z"),
            Some(1_787_229_000_000)
        );
        // Garbage => None.
        assert_eq!(parse_due_date_ms("someday"), None);
        assert_eq!(parse_due_date_ms(""), None);
    }

    struct GitHubCallCounter(std::sync::Arc<std::sync::atomic::AtomicUsize>);

    impl IssueBackend for GitHubCallCounter {
        fn provider_id(&self) -> &str {
            "github"
        }

        fn caps(&self) -> IssueCaps {
            IssueCaps {
                patch_status: true,
                comments: true,
                ..Default::default()
            }
        }

        fn list_issues<'a>(
            &'a self,
            _filter: &'a IssueFilter,
        ) -> BoxFuture<'a, Result<Vec<Issue>, IssueError>> {
            Box::pin(async { Ok(Vec::new()) })
        }

        fn get_issue<'a>(&'a self, _id: &'a str) -> BoxFuture<'a, Result<IssueDetail, IssueError>> {
            self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Box::pin(async { Err(IssueError::Api("fake get called".into())) })
        }

        fn create_issue<'a>(
            &'a self,
            _draft: &'a IssueDraft,
        ) -> BoxFuture<'a, Result<Issue, IssueError>> {
            Box::pin(async { Err(IssueError::Api("unused fake create".into())) })
        }

        fn update_issue<'a>(
            &'a self,
            _id: &'a str,
            _patch: &'a IssuePatch,
        ) -> BoxFuture<'a, Result<Issue, IssueError>> {
            self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Box::pin(async { Err(IssueError::Api("fake update called".into())) })
        }

        fn search<'a>(
            &'a self,
            _query: &'a str,
            _limit: usize,
        ) -> BoxFuture<'a, Result<Vec<Issue>, IssueError>> {
            Box::pin(async { Ok(Vec::new()) })
        }

        fn add_comment<'a>(
            &'a self,
            _id: &'a str,
            _body: &'a str,
        ) -> BoxFuture<'a, Result<(), IssueError>> {
            self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Box::pin(async { Err(IssueError::Api("fake comment called".into())) })
        }
    }

    #[test]
    fn duplicate_github_accounts_fail_closed_for_by_id_actions() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        let first = std::sync::Arc::new(AtomicUsize::new(0));
        let second = std::sync::Arc::new(AtomicUsize::new(0));
        let mut router = IssueRouter::from_config(&IssuesConfig::default());
        router
            .push_backend(
                "personal".into(),
                Box::new(GitHubCallCounter(first.clone())),
            )
            .unwrap();
        router
            .push_backend("work".into(), Box::new(GitHubCallCounter(second.clone())))
            .unwrap();

        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let id = "github:owner/repo#1";
        let patch = IssuePatch {
            status: Some(thegn_core::issue::IssueStatus::Todo),
            ..Default::default()
        };
        let results = runtime.block_on(async {
            (
                router.get_issue(id).await,
                router.update_issue(id, &patch).await,
                router.add_comment(id, "hello").await,
            )
        });
        assert!(matches!(
            results.0,
            Err(IssueError::AmbiguousAccount { .. })
        ));
        assert!(matches!(
            results.1,
            Err(IssueError::AmbiguousAccount { .. })
        ));
        assert!(matches!(
            results.2,
            Err(IssueError::AmbiguousAccount { .. })
        ));
        assert_eq!(first.load(Ordering::SeqCst), 0);
        assert_eq!(second.load(Ordering::SeqCst), 0);

        let error = match router.backend_for_id(id) {
            Err(error) => error,
            Ok(_) => panic!("duplicate provider prefix unexpectedly routed"),
        };
        assert!(matches!(
            &error,
            IssueError::AmbiguousAccount { provider, accounts }
                if provider == "github"
                    && accounts.iter().map(String::as_str).eq(["personal", "work"])
        ));
        let message = error.to_string();
        assert!(message.contains("personal, work"), "{message}");
        assert!(
            message.contains(
                "set enabled = false on all but one [[issues.issue_accounts]] entry for github"
            ),
            "{message}"
        );
        assert!(
            message.contains("restrict this repo with accounts = [\"<name>\"]"),
            "{message}"
        );
    }

    struct RefCallBackend {
        issue: Issue,
        calls: Arc<std::sync::atomic::AtomicUsize>,
    }

    impl IssueBackend for RefCallBackend {
        fn provider_id(&self) -> &str {
            "github"
        }
        fn caps(&self) -> IssueCaps {
            IssueCaps {
                patch_status: true,
                comments: true,
                labels: true,
                ..Default::default()
            }
        }
        fn list_issues<'a>(
            &'a self,
            _filter: &'a IssueFilter,
        ) -> BoxFuture<'a, Result<Vec<Issue>, IssueError>> {
            Box::pin(async move { Ok(vec![self.issue.clone()]) })
        }
        fn get_issue<'a>(&'a self, _id: &'a str) -> BoxFuture<'a, Result<IssueDetail, IssueError>> {
            self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            let issue = self.issue.clone();
            Box::pin(async move {
                Ok(IssueDetail {
                    issue,
                    ..Default::default()
                })
            })
        }
        fn create_issue<'a>(
            &'a self,
            _draft: &'a IssueDraft,
        ) -> BoxFuture<'a, Result<Issue, IssueError>> {
            Box::pin(async { Err(IssueError::Api("unused".into())) })
        }
        fn update_issue<'a>(
            &'a self,
            _id: &'a str,
            _patch: &'a IssuePatch,
        ) -> BoxFuture<'a, Result<Issue, IssueError>> {
            self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            let issue = self.issue.clone();
            Box::pin(async move { Ok(issue) })
        }
        fn search<'a>(
            &'a self,
            _query: &'a str,
            _limit: usize,
        ) -> BoxFuture<'a, Result<Vec<Issue>, IssueError>> {
            Box::pin(async move { Ok(vec![self.issue.clone()]) })
        }
        fn add_comment<'a>(
            &'a self,
            _id: &'a str,
            _body: &'a str,
        ) -> BoxFuture<'a, Result<(), IssueError>> {
            self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Box::pin(async { Ok(()) })
        }
        fn attach_label<'a>(
            &'a self,
            _id: &'a str,
            _label: &'a str,
        ) -> BoxFuture<'a, Result<(), IssueError>> {
            self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Box::pin(async { Ok(()) })
        }
        fn detach_label<'a>(
            &'a self,
            _id: &'a str,
            _label: &'a str,
        ) -> BoxFuture<'a, Result<(), IssueError>> {
            self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Box::pin(async { Ok(()) })
        }
    }

    fn ref_issue(id: &str) -> Issue {
        Issue {
            id: id.into(),
            number: "1".into(),
            provider: "github".into(),
            title: "issue".into(),
            url: "https://github.com/owner/repo/issues/1".into(),
            account: Some("spoofed".into()),
            ..Default::default()
        }
    }

    fn ref_router(
        order: &[&str],
        id: &str,
    ) -> (IssueRouter, Vec<Arc<std::sync::atomic::AtomicUsize>>) {
        let mut router = IssueRouter::from_config(&IssuesConfig::default());
        let calls = (0..2)
            .map(|_| Arc::new(std::sync::atomic::AtomicUsize::new(0)))
            .collect::<Vec<_>>();
        for account in order {
            let slot = if *account == "personal" { 0 } else { 1 };
            router
                .push_backend(
                    (*account).into(),
                    Box::new(RefCallBackend {
                        issue: ref_issue(id),
                        calls: Arc::clone(&calls[slot]),
                    }),
                )
                .unwrap();
        }
        (router, calls)
    }

    #[test]
    fn issue_ref_routes_colliding_ids_to_the_exact_account() {
        use std::sync::atomic::Ordering;
        let (router, calls) = ref_router(&["personal", "work"], "github:owner/repo#1");
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let patch = IssuePatch {
            status: Some(thegn_core::issue::IssueStatus::Todo),
            ..Default::default()
        };
        for account in ["work", "personal"] {
            let issue_ref = IssueRef::new("github", account, "github:owner/repo#1").unwrap();
            runtime.block_on(async {
                router.get_issue_ref(&issue_ref).await.unwrap();
                let updated = router.update_issue_ref(&issue_ref, &patch).await.unwrap();
                assert_eq!(updated.account.as_deref(), Some(account));
                router.add_comment_ref(&issue_ref, "hello").await.unwrap();
                router.attach_label_ref(&issue_ref, "bug").await.unwrap();
                router.detach_label_ref(&issue_ref, "bug").await.unwrap();
            });
            let expected = if account == "personal" { 0 } else { 1 };
            let other = 1 - expected;
            assert_eq!(calls[expected].load(Ordering::SeqCst), 5);
            assert_eq!(calls[other].load(Ordering::SeqCst), 0);
            calls[expected].store(0, Ordering::SeqCst);
        }
    }

    #[test]
    fn issue_ref_routing_survives_account_reorder() {
        let (router, calls) = ref_router(&["work", "personal"], "github:owner/repo#1");
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let issue_ref = IssueRef::new("github", "work", "github:owner/repo#1").unwrap();
        runtime.block_on(router.get_issue_ref(&issue_ref)).unwrap();
        assert_eq!(calls[1].load(std::sync::atomic::Ordering::SeqCst), 1);
        assert_eq!(calls[0].load(std::sync::atomic::Ordering::SeqCst), 0);
    }

    #[test]
    fn issue_ref_unknown_account_is_typed_and_calls_nothing() {
        let (router, calls) = ref_router(&["personal", "work"], "github:owner/repo#1");
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let issue_ref = IssueRef::new("github", "gone", "github:owner/repo#1").unwrap();
        let result = runtime.block_on(router.get_issue_ref(&issue_ref));
        assert!(
            matches!(result, Err(IssueError::UnknownAccount { provider, account }) if provider == "github" && account == "gone")
        );
        assert!(
            calls
                .iter()
                .all(|count| count.load(std::sync::atomic::Ordering::SeqCst) == 0)
        );
    }

    #[test]
    fn issue_ref_router_stamps_the_producing_account_on_reads() {
        let (router, _) = ref_router(&["work"], "github:owner/repo#1");
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let listed = runtime
            .block_on(router.list_issues(&IssueFilter::default()))
            .unwrap();
        assert_eq!(listed[0].account.as_deref(), Some("work"));
        let per_provider = runtime.block_on(router.list_per_provider(&IssueFilter::default()));
        assert_eq!(
            per_provider[0].2.as_ref().unwrap()[0].account.as_deref(),
            Some("work")
        );
        let searched = runtime.block_on(router.search("q", 10)).unwrap();
        assert_eq!(searched[0].account.as_deref(), Some("work"));
        let issue_ref = IssueRef::new("github", "work", "github:owner/repo#1").unwrap();
        let detail = runtime.block_on(router.get_issue_ref(&issue_ref)).unwrap();
        assert_eq!(detail.issue.account.as_deref(), Some("work"));
        let detail = runtime
            .block_on(router.get_issue("github:owner/repo#1"))
            .unwrap();
        assert_eq!(detail.issue.account.as_deref(), Some("work"));
    }

    #[test]
    fn issue_ref_mismatched_returned_id_is_refused() {
        let (router, _) = ref_router(&["work"], "github:owner/repo#2");
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let issue_ref = IssueRef::new("github", "work", "github:owner/repo#1").unwrap();
        let result = runtime.block_on(router.get_issue_ref(&issue_ref));
        assert!(matches!(
            result,
            Err(IssueError::Policy(
                "tracker returned a different issue than the ref"
            ))
        ));
    }

    use std::sync::atomic::{AtomicUsize, Ordering};

    enum MatrixMode {
        Rows(Vec<Issue>),
        Fail,
        Hang,
    }

    struct MatrixBackend {
        mode: MatrixMode,
        started: Arc<AtomicUsize>,
        in_flight: Arc<AtomicUsize>,
        max_in_flight: Arc<AtomicUsize>,
    }

    struct MatrixInFlightGuard(Arc<AtomicUsize>);

    impl Drop for MatrixInFlightGuard {
        fn drop(&mut self) {
            self.0.fetch_sub(1, Ordering::SeqCst);
        }
    }

    impl MatrixBackend {
        fn new(
            mode: MatrixMode,
            started: Arc<AtomicUsize>,
            in_flight: Arc<AtomicUsize>,
            max_in_flight: Arc<AtomicUsize>,
        ) -> Self {
            Self {
                mode,
                started,
                in_flight,
                max_in_flight,
            }
        }

        fn read(&self, limit: usize) -> BoxFuture<'_, Result<Vec<Issue>, IssueError>> {
            Box::pin(async move {
                self.started.fetch_add(1, Ordering::SeqCst);
                let current = self.in_flight.fetch_add(1, Ordering::SeqCst) + 1;
                self.max_in_flight.fetch_max(current, Ordering::SeqCst);
                let _guard = MatrixInFlightGuard(Arc::clone(&self.in_flight));
                match &self.mode {
                    MatrixMode::Rows(rows) => {
                        for _ in 0..3 {
                            tokio::task::yield_now().await;
                        }
                        let mut rows = rows.clone();
                        if limit > 0 {
                            rows.truncate(limit);
                        }
                        Ok(rows)
                    }
                    MatrixMode::Fail => Err(IssueError::Api("matrix failure".into())),
                    MatrixMode::Hang => {
                        std::future::pending::<Result<Vec<Issue>, IssueError>>().await
                    }
                }
            })
        }
    }

    impl IssueBackend for MatrixBackend {
        fn provider_id(&self) -> &str {
            "linear"
        }

        fn caps(&self) -> IssueCaps {
            IssueCaps::default()
        }

        fn list_issues<'a>(
            &'a self,
            filter: &'a IssueFilter,
        ) -> BoxFuture<'a, Result<Vec<Issue>, IssueError>> {
            self.read(filter.limit)
        }

        fn get_issue<'a>(&'a self, _id: &'a str) -> BoxFuture<'a, Result<IssueDetail, IssueError>> {
            Box::pin(async { Err(IssueError::Api("not used".into())) })
        }

        fn create_issue<'a>(
            &'a self,
            _draft: &'a IssueDraft,
        ) -> BoxFuture<'a, Result<Issue, IssueError>> {
            Box::pin(async { Err(IssueError::Api("not used".into())) })
        }

        fn update_issue<'a>(
            &'a self,
            _id: &'a str,
            _patch: &'a IssuePatch,
        ) -> BoxFuture<'a, Result<Issue, IssueError>> {
            Box::pin(async { Err(IssueError::Api("not used".into())) })
        }

        fn search<'a>(
            &'a self,
            _query: &'a str,
            limit: usize,
        ) -> BoxFuture<'a, Result<Vec<Issue>, IssueError>> {
            self.read(limit)
        }
    }

    fn matrix_router(
        modes: Vec<(&str, MatrixMode)>,
    ) -> (
        IssueRouter,
        Arc<AtomicUsize>,
        Arc<AtomicUsize>,
        Arc<AtomicUsize>,
    ) {
        let started = Arc::new(AtomicUsize::new(0));
        let in_flight = Arc::new(AtomicUsize::new(0));
        let max_in_flight = Arc::new(AtomicUsize::new(0));
        let mut router = IssueRouter::from_config(&IssuesConfig::default());
        for (account, mode) in modes {
            router
                .push_backend(
                    account.into(),
                    Box::new(MatrixBackend::new(
                        mode,
                        Arc::clone(&started),
                        Arc::clone(&in_flight),
                        Arc::clone(&max_in_flight),
                    )),
                )
                .unwrap();
        }
        (router, started, in_flight, max_in_flight)
    }

    fn matrix_rows(prefix: &str, count: usize) -> Vec<Issue> {
        (0..count)
            .map(|index| fixed_issue(&format!("{prefix}-{index}")))
            .collect()
    }

    fn matrix_runtime() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .start_paused(true)
            .build()
            .unwrap()
    }

    fn mixed_matrix_router() -> IssueRouter {
        let (router, _, _, _) = matrix_router(vec![
            ("fail", MatrixMode::Fail),
            ("hang", MatrixMode::Hang),
            ("big", MatrixMode::Rows(matrix_rows("big", 300))),
            (
                "small",
                MatrixMode::Rows(vec![fixed_issue("big-0"), fixed_issue("small-1")]),
            ),
        ]);
        router
    }

    fn issue_numbers(issues: &[Issue]) -> Vec<String> {
        issues.iter().map(|issue| issue.number.clone()).collect()
    }

    #[test]
    fn fanout_matrix_concurrency_never_exceeds_the_account_bound() {
        let modes = ["a", "b", "c", "d", "e", "f"]
            .into_iter()
            .enumerate()
            .map(|(index, account)| {
                (
                    account,
                    MatrixMode::Rows(vec![fixed_issue(&format!("row-{index}"))]),
                )
            })
            .collect();
        let (router, _, _, max_in_flight) = matrix_router(modes);
        let runtime = matrix_runtime();
        let listed = runtime
            .block_on(router.list_issues(&IssueFilter::default()))
            .unwrap();
        assert_eq!(listed.len(), 6);
        let list_max = max_in_flight.load(Ordering::SeqCst);
        assert!(list_max >= 2, "list did not overlap; max={list_max}");
        assert!(
            list_max <= MAX_CONCURRENT_ACCOUNTS,
            "list exceeded bound; max={list_max}"
        );

        max_in_flight.store(0, Ordering::SeqCst);
        let searched = runtime.block_on(router.search("q", 0)).unwrap();
        assert_eq!(searched.len(), 6);
        let search_max = max_in_flight.load(Ordering::SeqCst);
        assert!(search_max >= 2, "search did not overlap; max={search_max}");
        assert!(
            search_max <= MAX_CONCURRENT_ACCOUNTS,
            "search exceeded bound; max={search_max}"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn fanout_matrix_dropping_the_router_future_cancels_in_flight_accounts() {
        let modes = ["a", "b", "c", "d", "e", "f"]
            .into_iter()
            .map(|account| (account, MatrixMode::Hang))
            .collect();
        let (router, started, in_flight, _) = matrix_router(modes);
        let result = tokio::time::timeout(
            Duration::from_secs(1),
            router.list_issues(&IssueFilter::default()),
        )
        .await;
        assert!(result.is_err(), "outer timeout should cancel the fan-out");
        assert_eq!(started.load(Ordering::SeqCst), MAX_CONCURRENT_ACCOUNTS);
        assert_eq!(in_flight.load(Ordering::SeqCst), 0);
    }

    #[tokio::test(start_paused = true)]
    async fn fanout_matrix_mixed_accounts_keep_healthy_rows_with_limit_zero() {
        let router = mixed_matrix_router();
        let listed = tokio::time::timeout(
            Duration::from_secs(60),
            router.list_issues(&IssueFilter {
                limit: 0,
                ..IssueFilter::default()
            }),
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(listed.len(), 301);
        assert_eq!(
            issue_numbers(&listed[..4]),
            [
                "big-0".to_string(),
                "big-1".to_string(),
                "small-1".to_string(),
                "big-2".to_string()
            ]
        );
        assert_eq!(
            issue_numbers(&listed)
                .iter()
                .filter(|number| *number == "big-0")
                .count(),
            1
        );
        assert!(
            listed
                .iter()
                .all(|issue| issue.account.as_deref() != Some("fail"))
        );
        assert!(
            listed
                .iter()
                .all(|issue| issue.account.as_deref() != Some("hang"))
        );

        let searched = tokio::time::timeout(Duration::from_secs(60), router.search("q", 0))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(searched.len(), 301);
        assert_eq!(
            issue_numbers(&searched[..4]),
            [
                "big-0".to_string(),
                "big-1".to_string(),
                "small-1".to_string(),
                "big-2".to_string()
            ]
        );
        assert_eq!(
            issue_numbers(&searched)
                .iter()
                .filter(|number| *number == "big-0")
                .count(),
            1
        );
        assert!(
            searched
                .iter()
                .all(|issue| issue.account.as_deref() != Some("fail"))
        );
        assert!(
            searched
                .iter()
                .all(|issue| issue.account.as_deref() != Some("hang"))
        );
    }

    #[tokio::test(start_paused = true)]
    async fn fanout_matrix_exact_limit_across_mixed_accounts() {
        let router = mixed_matrix_router();
        let expected = [
            "big-0".to_string(),
            "big-1".to_string(),
            "small-1".to_string(),
            "big-2".to_string(),
        ];
        let listed = tokio::time::timeout(
            Duration::from_secs(60),
            router.list_issues(&IssueFilter {
                limit: 4,
                ..IssueFilter::default()
            }),
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(issue_numbers(&listed), expected);
        let searched = tokio::time::timeout(Duration::from_secs(60), router.search("q", 4))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(issue_numbers(&searched), expected);
    }

    #[test]
    fn fanout_matrix_partial_and_failing_accounts_do_not_hide_each_other() {
        let (mut router, _, _, _) = matrix_router(vec![("fail", MatrixMode::Fail)]);
        router
            .push_backend("partial".into(), Box::new(PartialListBackend))
            .unwrap();
        router
            .push_backend(
                "healthy-rows".into(),
                Box::new(MatrixBackend::new(
                    MatrixMode::Rows(vec![fixed_issue("healthy-0"), fixed_issue("healthy-1")]),
                    Arc::new(AtomicUsize::new(0)),
                    Arc::new(AtomicUsize::new(0)),
                    Arc::new(AtomicUsize::new(0)),
                )),
            )
            .unwrap();
        let runtime = matrix_runtime();
        let listed = runtime
            .block_on(router.list_issues(&IssueFilter::default()))
            .unwrap();
        let numbers = issue_numbers(&listed);
        assert!(numbers.contains(&"healthy-0".to_string()));
        assert!(numbers.contains(&"healthy-1".to_string()));
        assert!(numbers.contains(&"1".to_string()));
        assert_eq!(numbers.len(), 3);
    }
}
