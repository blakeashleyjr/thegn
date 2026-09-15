//! Generic issue tracker trait + provider router.
//!
//! The `IssueBackend` trait is the single seam that all providers implement.
//! `IssueRouter` is the host-facing entry point: it reads `IssuesConfig`,
//! constructs the right backend, and forwards calls — returning empty
//! collections when no provider is configured rather than erroring, so the
//! panel always has something to render.

pub mod capabilities;
pub mod github;
pub(crate) mod http;
pub(crate) mod identity;
pub mod jira;
pub mod kaneo;
pub mod kaneo_auth;
pub mod linear;
pub mod secret;

use futures_util::future::BoxFuture;
use std::sync::Arc;
use thegn_core::config::{IssueAccount, IssueProviderKind, IssuesConfig, expand_env_ref};
use thegn_core::issue::{Issue, IssueDetail, IssueDraft, IssueFilter, IssuePatch};
use thegn_core::seam::{ErrorClass, SeamError};

pub use capabilities::IssueCaps;

/// Errors from any issue backend.
pub enum IssueError {
    NotConfigured,
    Unsupported(&'static str),
    Network(reqwest::Error),
    Auth(String),
    Api(String),
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
            Self::Unsupported(op) => f.debug_tuple("IssueError::Unsupported").field(op).finish(),
            Self::Network(_) => f.write_str("IssueError::Network(<redacted>)"),
            Self::Auth(message) => f.debug_tuple("IssueError::Auth").field(message).finish(),
            Self::Api(message) => f.debug_tuple("IssueError::Api").field(message).finish(),
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
            IssueError::Unsupported(op) => write!(f, "{op} is not supported by this provider"),
            // Reqwest errors may carry the full request URL, including query
            // material. Keep tracker diagnostics static and redacted.
            IssueError::Network(_) => write!(f, "network: tracker request failed"),
            IssueError::Auth(s) => write!(f, "auth: {s}"),
            IssueError::Api(s) => write!(f, "api: {s}"),
            IssueError::Subprocess(s) => write!(f, "subprocess: {s}"),
            IssueError::Parse(s) => write!(f, "parse: {s}"),
            IssueError::Policy(s) => write!(f, "tracker policy: {s}"),
            IssueError::Timeout(s) => write!(f, "tracker timeout: {s}"),
            IssueError::BodyLimit(s) => write!(f, "tracker body limit: {s}"),
        }
    }
}

impl std::error::Error for IssueError {}

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
            IssueError::NotConfigured => ErrorClass::NotConfigured,
            IssueError::Auth(_) => ErrorClass::Auth,
            IssueError::Network(e) if e.is_connect() || e.is_timeout() => ErrorClass::Transient,
            IssueError::Timeout(_) => ErrorClass::Transient,
            IssueError::Network(_) => ErrorClass::Other,
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
        identity::builtin_segment(plugin_id, "plugin namespace").map_err(IssueError::Parse)?;
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
        || parsed.query().is_some()
        || parsed.fragment().is_some()
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
        identity::builtin_segment(plugin_namespace, "plugin namespace")
            .map_err(IssueError::Parse)?;
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
    fn provider_id(&self) -> &'static str;
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

/// Routes issue requests across every configured provider. `list`/`search` fan
/// out and merge; `get`/`update` dispatch by the `"<provider>:"` id prefix.
/// Returns empty results (not errors) when nothing is configured — the panel
/// renders gracefully regardless. A single provider failing never breaks the
/// others: it logs and contributes nothing to the merged result.
pub struct IssueRouter {
    inner: Vec<AccountBackend>,
}

impl IssueRouter {
    /// Append a dynamically-provided backend (provider-as-plugin): `account`
    /// labels its rows like a configured account's name would.
    pub fn push_backend(&mut self, account: String, inner: Box<dyn IssueBackend>) {
        self.inner.push(AccountBackend { account, inner });
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
    pub fn provider_id(&self) -> &'static str {
        self.inner
            .first()
            .map(|b| b.inner.provider_id())
            .unwrap_or("none")
    }

    /// Every configured provider id, in config order (may repeat when several
    /// accounts share a provider).
    pub fn provider_ids(&self) -> Vec<&'static str> {
        self.inner.iter().map(|b| b.inner.provider_id()).collect()
    }

    pub fn is_configured(&self) -> bool {
        !self.inner.is_empty()
    }

    /// Locate the backend owning an id. Provider namespaces are matched by
    /// longest registered prefix so `plugin:demo:key` reaches `plugin:demo`.
    /// A bare legacy id is accepted only when there is exactly one backend;
    /// namespaced ids never fall through to another provider.
    fn backend_for_id(&self, id: &str) -> Option<&dyn IssueBackend> {
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
            // `find` deliberately preserves the established first-account
            // behavior when equal-length provider namespaces are duplicated.
            return self
                .inner
                .iter()
                .find(|b| matches(b) && b.inner.provider_id().len() == longest)
                .map(|b| b.inner.as_ref());
        }
        // Preserve the audited legacy bare-id compatibility only at the
        // unambiguous sole-backend boundary. A namespaced id never falls back.
        if !id.contains(':') && self.inner.len() == 1 {
            return self.inner.first().map(|b| b.inner.as_ref());
        }
        None
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
        let mut ids: Vec<&'static str> = Vec::new();
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

    /// List issues across all accounts, concatenated. A failing account logs
    /// and contributes nothing rather than failing the whole call.
    ///
    /// **Best-effort by design, and deliberately so** — this is the background
    /// fan-out's entry point, where one bad account must not blank the three
    /// good ones. The flip side is that the error is only a `tracing::warn!`,
    /// which goes nowhere with `THEGN_LOG` unset: a caller on the primary path
    /// of a **user-invoked** action must not use this, or a 400 reads as an
    /// empty tracker. Use [`list_per_provider`](Self::list_per_provider) there
    /// and report the per-account `Err`s (see `cmd::issue::list_tracker_issues`).
    pub async fn list_issues(&self, filter: &IssueFilter) -> Result<Vec<Issue>, IssueError> {
        let mut all = Vec::new();
        for b in &self.inner {
            match b.inner.list_issues(filter).await {
                Ok(issues) => match issues
                    .into_iter()
                    .map(|issue| {
                        validate_issue_identity(&issue)?;
                        Ok(issue)
                    })
                    .collect::<Result<Vec<_>, IssueError>>()
                {
                    Ok(mut valid) => all.append(&mut valid),
                    Err(e) => {
                        tracing::warn!(provider = b.inner.provider_id(), error = %e, "issue identity rejected")
                    }
                },
                Err(e) => {
                    tracing::warn!(account = %b.account, provider = b.inner.provider_id(), error = %e, "issue list failed")
                }
            }
        }
        Ok(all)
    }

    /// Per-account results, so callers (the cache refresh) can store and diff
    /// each account under its own `(repo_root, provider, account)` key.
    pub async fn list_per_provider(
        &self,
        filter: &IssueFilter,
    ) -> Vec<(String, &'static str, Result<Vec<Issue>, IssueError>)> {
        let mut out = Vec::with_capacity(self.inner.len());
        for b in &self.inner {
            let result = b.inner.list_issues(filter).await.and_then(|issues| {
                issues
                    .into_iter()
                    .map(|issue| {
                        validate_issue_identity(&issue)?;
                        Ok(issue)
                    })
                    .collect()
            });
            out.push((b.account.clone(), b.inner.provider_id(), result));
        }
        out
    }

    pub async fn get_issue(&self, id: &str) -> Result<IssueDetail, IssueError> {
        validate_control_issue_id(id)?;
        match self.backend_for_id(id) {
            Some(b) => b.get_issue(id).await.and_then(|detail| {
                validate_issue_identity(&detail.issue)?;
                Ok(detail)
            }),
            None => Err(self.id_miss(id)),
        }
    }

    /// Create an issue on the first configured provider.
    pub async fn create_issue(&self, draft: &IssueDraft) -> Result<Issue, IssueError> {
        match self.inner.first() {
            Some(b) => b.inner.create_issue(draft).await.and_then(|issue| {
                validate_issue_identity(&issue)?;
                Ok(issue)
            }),
            None => Err(IssueError::NotConfigured),
        }
    }

    pub async fn update_issue(&self, id: &str, patch: &IssuePatch) -> Result<Issue, IssueError> {
        validate_control_issue_id(id)?;
        match self.backend_for_id(id) {
            Some(b) => b.update_issue(id, patch).await.and_then(|issue| {
                validate_issue_identity(&issue)?;
                Ok(issue)
            }),
            None => Err(self.id_miss(id)),
        }
    }

    /// Post a comment on the issue identified by a `"<provider>:<key>"` id.
    pub async fn add_comment(&self, id: &str, body: &str) -> Result<(), IssueError> {
        validate_control_issue_id(id)?;
        match self.backend_for_id(id) {
            Some(b) => b.add_comment(id, body).await,
            None => Err(IssueError::NotConfigured),
        }
    }

    /// Attach a label (by name) to the issue identified by its id.
    pub async fn attach_label(&self, id: &str, label: &str) -> Result<(), IssueError> {
        validate_control_issue_id(id)?;
        match self.backend_for_id(id) {
            Some(b) => b.attach_label(id, label).await,
            None => Err(IssueError::NotConfigured),
        }
    }

    /// Remove a label (by name) from the issue identified by its id.
    pub async fn detach_label(&self, id: &str, label: &str) -> Result<(), IssueError> {
        validate_control_issue_id(id)?;
        match self.backend_for_id(id) {
            Some(b) => b.detach_label(id, label).await,
            None => Err(IssueError::NotConfigured),
        }
    }

    /// The first configured Kaneo backend, for board/project browsing.
    pub fn kaneo(&self) -> Option<&kaneo::KaneoBackend> {
        self.inner.iter().find_map(|b| b.inner.as_kaneo())
    }

    /// Search across all accounts, concatenated.
    pub async fn search(&self, query: &str, limit: usize) -> Result<Vec<Issue>, IssueError> {
        let mut all = Vec::new();
        for b in &self.inner {
            match b.inner.search(query, limit).await {
                Ok(issues) => match issues
                    .into_iter()
                    .map(|issue| {
                        validate_issue_identity(&issue)?;
                        Ok(issue)
                    })
                    .collect::<Result<Vec<_>, IssueError>>()
                {
                    Ok(mut valid) => all.append(&mut valid),
                    Err(e) => {
                        tracing::warn!(provider = b.inner.provider_id(), error = %e, "issue identity rejected")
                    }
                },
                Err(e) => {
                    tracing::warn!(account = %b.account, provider = b.inner.provider_id(), error = %e, "issue search failed")
                }
            }
        }
        Ok(all)
    }
}

#[cfg(test)]
mod spec {
    use super::*;
    use thegn_core::config::IssueProviderKind;

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
            r.backend_for_id("jira:PROJ-1").map(|b| b.provider_id()),
            Some("jira")
        );
        assert_eq!(
            r.backend_for_id("linear:ABC-9").map(|b| b.provider_id()),
            Some("linear")
        );
        assert_eq!(
            r.backend_for_id("kaneo:abc123").map(|b| b.provider_id()),
            Some("kaneo")
        );
        // An id for a provider that isn't configured routes nowhere.
        assert!(r.backend_for_id("github:42").is_none());
        // A bare id with no prefix also routes nowhere — three backends, so
        // the single-backend fallback below does not apply.
        assert!(r.backend_for_id("nonsense").is_none());
    }

    #[test]
    fn bare_id_routes_to_the_only_backend() {
        let r = IssueRouter::from_config(&cfg_with(vec![IssueProviderKind::Linear]));
        // `wt new --from-issue THE-72` on a single-tracker machine.
        assert_eq!(
            r.backend_for_id("THE-72").map(|b| b.provider_id()),
            Some("linear")
        );
        // An explicit prefix still wins outright (same backend here).
        assert_eq!(
            r.backend_for_id("linear:THE-72").map(|b| b.provider_id()),
            Some("linear")
        );
    }

    #[test]
    fn bare_id_with_two_backends_routes_nowhere() {
        let r = IssueRouter::from_config(&cfg_with(vec![
            IssueProviderKind::Linear,
            IssueProviderKind::Jira,
        ]));
        assert!(r.backend_for_id("THE-72").is_none());
        // …but an explicit prefix wins over the (absent) fallback, and picks
        // the named provider rather than the first configured one.
        assert_eq!(
            r.backend_for_id("jira:THE-72").map(|b| b.provider_id()),
            Some("jira")
        );
    }

    #[test]
    fn single_backend_does_not_fallback_foreign_namespace() {
        let r = IssueRouter::from_config(&cfg_with(vec![IssueProviderKind::Linear]));
        assert!(r.backend_for_id("jira:PROJ-1").is_none());
    }

    struct CountingBackend {
        updates: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    }

    impl IssueBackend for CountingBackend {
        fn provider_id(&self) -> &'static str {
            "linear"
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

    struct PluginMarker {
        id: &'static str,
    }

    impl IssueBackend for PluginMarker {
        fn provider_id(&self) -> &'static str {
            self.id
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
        router.push_backend("demo".into(), Box::new(PluginMarker { id: "plugin:demo" }));
        router.push_backend(
            "nested".into(),
            Box::new(PluginMarker {
                id: "plugin:demo:extra",
            }),
        );
        assert_eq!(
            router
                .backend_for_id("plugin:demo:opaque/key#7")
                .map(|b| b.provider_id()),
            Some("plugin:demo")
        );
        assert_eq!(
            router
                .backend_for_id("plugin:demo:extra:key")
                .map(|b| b.provider_id()),
            Some("plugin:demo:extra")
        );
        assert!(router.backend_for_id("plugin:other:opaque").is_none());
        assert!(validate_control_issue_id("plugin:demo:opaque/key#7").is_ok());
    }

    #[test]
    fn malformed_control_id_reaches_no_fake_provider_effect() {
        let updates = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let mut router = IssueRouter::from_config(&IssuesConfig::default());
        router.push_backend(
            "fake".into(),
            Box::new(CountingBackend {
                updates: updates.clone(),
            }),
        );
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let patch = IssuePatch::default();
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
        assert_eq!(updates.load(std::sync::atomic::Ordering::SeqCst), 1);
    }

    #[test]
    fn duplicate_provider_ids_keep_the_first_account_for_mutations() {
        let first = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let second = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let mut router = IssueRouter::from_config(&IssuesConfig::default());
        router.push_backend(
            "first".into(),
            Box::new(CountingBackend {
                updates: first.clone(),
            }),
        );
        router.push_backend(
            "second".into(),
            Box::new(CountingBackend {
                updates: second.clone(),
            }),
        );
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let result = runtime.block_on(router.update_issue("linear:ABC-9", &IssuePatch::default()));
        assert!(result.is_err());
        assert_eq!(first.load(std::sync::atomic::Ordering::SeqCst), 1);
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
            "url": ""
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
}
