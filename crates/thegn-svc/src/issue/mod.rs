//! Generic issue tracker trait + provider router.
//!
//! The `IssueBackend` trait is the single seam that all providers implement.
//! `IssueRouter` is the host-facing entry point: it reads `IssuesConfig`,
//! constructs the right backend, and forwards calls — returning empty
//! collections when no provider is configured rather than erroring, so the
//! panel always has something to render.

pub mod github;
pub mod jira;
pub mod kaneo;
pub mod kaneo_auth;
pub mod linear;

use futures_util::future::BoxFuture;
use thegn_core::config::{IssueAccount, IssueProviderKind, IssuesConfig, expand_env_ref};
use thegn_core::issue::{Issue, IssueDetail, IssueDraft, IssueFilter, IssuePatch};

/// Errors from any issue backend.
#[derive(Debug)]
pub enum IssueError {
    NotConfigured,
    Network(reqwest::Error),
    Auth(String),
    Api(String),
    Subprocess(String),
    Parse(String),
}

impl std::fmt::Display for IssueError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            IssueError::NotConfigured => write!(f, "no issue provider configured"),
            IssueError::Network(e) => write!(f, "network: {e}"),
            IssueError::Auth(s) => write!(f, "auth: {s}"),
            IssueError::Api(s) => write!(f, "api: {s}"),
            IssueError::Subprocess(s) => write!(f, "subprocess: {s}"),
            IssueError::Parse(s) => write!(f, "parse: {s}"),
        }
    }
}

impl std::error::Error for IssueError {}

impl IssueError {
    /// Whether this is a transient connectivity failure (connect/timeout) — as
    /// opposed to auth/parse/not-configured. Feeds the connectivity holder so a
    /// dropped link (not a bad token) is what flips the app offline.
    pub fn is_transient(&self) -> bool {
        matches!(self, IssueError::Network(e) if e.is_connect() || e.is_timeout())
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

/// Provider-agnostic issue tracker seam.
///
/// Methods return [`BoxFuture`]s (not native `async fn`) so the trait stays
/// object-safe — the router dispatches over `Box<dyn IssueBackend>`.
pub trait IssueBackend: Send + Sync {
    fn provider_id(&self) -> &'static str;

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
        Box::pin(async move {
            Err(IssueError::Api(
                "comments not supported by this provider".into(),
            ))
        })
    }
    /// Attach a label (by name) to an issue, creating it if the provider allows.
    fn attach_label<'a>(
        &'a self,
        _id: &'a str,
        _label: &'a str,
    ) -> BoxFuture<'a, Result<(), IssueError>> {
        Box::pin(async move {
            Err(IssueError::Api(
                "labels not supported by this provider".into(),
            ))
        })
    }
    /// Remove a label (by name) from an issue.
    fn detach_label<'a>(
        &'a self,
        _id: &'a str,
        _label: &'a str,
    ) -> BoxFuture<'a, Result<(), IssueError>> {
        Box::pin(async move {
            Err(IssueError::Api(
                "labels not supported by this provider".into(),
            ))
        })
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
pub(crate) fn backend_from_account(
    a: &IssueAccount,
    dir: Option<&std::path::Path>,
) -> Option<Box<dyn IssueBackend>> {
    match a.provider {
        IssueProviderKind::Linear => {
            let api_key = expand_env_ref(&a.token).unwrap_or_default();
            let team_id = (!a.team_id.is_empty()).then(|| a.team_id.clone());
            Some(Box::new(linear::LinearBackend::new(api_key, team_id)))
        }
        IssueProviderKind::Github => {
            let mut b = github::GitHubIssuesBackend::new(a.extra_flags.clone());
            b.set_dir(dir.map(std::path::Path::to_path_buf));
            Some(Box::new(b))
        }
        IssueProviderKind::Jira => {
            let api_token = expand_env_ref(&a.token).unwrap_or_default();
            Some(Box::new(jira::JiraBackend::new(
                a.base_url.clone(),
                a.email.clone(),
                api_token,
                (!a.project_key.is_empty()).then(|| a.project_key.clone()),
            )))
        }
        IssueProviderKind::Kaneo => {
            let mut api_key = expand_env_ref(&a.token).unwrap_or_default();
            // No configured key ⇒ fall back to a token stored by
            // `thegn kaneo login` (device flow) for this instance.
            if api_key.is_empty() {
                api_key = kaneo_stored_token(&a.base_url).unwrap_or_default();
            }
            Some(Box::new(kaneo::KaneoBackend::new(
                a.base_url.clone(),
                api_key,
                (!a.workspace_id.is_empty()).then(|| a.workspace_id.clone()),
                (!a.project_id.is_empty()).then(|| a.project_id.clone()),
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
        Self::from_config_at(cfg, None)
    }

    /// Like [`from_config`](Self::from_config), but anchors subprocess-backed
    /// providers (GitHub's `gh`) to `dir` so calls without an explicit `--repo`
    /// resolve against that worktree instead of the process cwd. Callers
    /// fetching for a specific worktree should prefer this.
    pub fn from_config_at(cfg: &IssuesConfig, dir: Option<&std::path::Path>) -> Self {
        let inner = cfg
            .active_accounts()
            .into_iter()
            .filter_map(|acct| {
                backend_from_account(&acct, dir).map(|inner| AccountBackend {
                    account: acct.name,
                    inner,
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

    /// Locate the backend owning an id of the form `"<provider>:<key>"`. When
    /// multiple accounts share the provider this picks the first — get/update by
    /// bare id can't disambiguate accounts (a known multi-account limitation).
    fn backend_for_id(&self, id: &str) -> Option<&dyn IssueBackend> {
        let prefix = id.split_once(':').map(|(p, _)| p).unwrap_or(id);
        self.inner
            .iter()
            .find(|b| b.inner.provider_id() == prefix)
            .map(|b| b.inner.as_ref())
    }

    /// List issues across all accounts, concatenated. A failing account logs
    /// and contributes nothing rather than failing the whole call.
    pub async fn list_issues(&self, filter: &IssueFilter) -> Result<Vec<Issue>, IssueError> {
        let mut all = Vec::new();
        for b in &self.inner {
            match b.inner.list_issues(filter).await {
                Ok(mut issues) => all.append(&mut issues),
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
            out.push((
                b.account.clone(),
                b.inner.provider_id(),
                b.inner.list_issues(filter).await,
            ));
        }
        out
    }

    pub async fn get_issue(&self, id: &str) -> Result<IssueDetail, IssueError> {
        match self.backend_for_id(id) {
            Some(b) => b.get_issue(id).await,
            None => Err(IssueError::NotConfigured),
        }
    }

    /// Create an issue on the first configured provider.
    pub async fn create_issue(&self, draft: &IssueDraft) -> Result<Issue, IssueError> {
        match self.inner.first() {
            Some(b) => b.inner.create_issue(draft).await,
            None => Err(IssueError::NotConfigured),
        }
    }

    pub async fn update_issue(&self, id: &str, patch: &IssuePatch) -> Result<Issue, IssueError> {
        match self.backend_for_id(id) {
            Some(b) => b.update_issue(id, patch).await,
            None => Err(IssueError::NotConfigured),
        }
    }

    /// Post a comment on the issue identified by a `"<provider>:<key>"` id.
    pub async fn add_comment(&self, id: &str, body: &str) -> Result<(), IssueError> {
        match self.backend_for_id(id) {
            Some(b) => b.add_comment(id, body).await,
            None => Err(IssueError::NotConfigured),
        }
    }

    /// Attach a label (by name) to the issue identified by its id.
    pub async fn attach_label(&self, id: &str, label: &str) -> Result<(), IssueError> {
        match self.backend_for_id(id) {
            Some(b) => b.attach_label(id, label).await,
            None => Err(IssueError::NotConfigured),
        }
    }

    /// Remove a label (by name) from the issue identified by its id.
    pub async fn detach_label(&self, id: &str, label: &str) -> Result<(), IssueError> {
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
                Ok(mut issues) => all.append(&mut issues),
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
        // A bare id with no prefix also routes nowhere.
        assert!(r.backend_for_id("nonsense").is_none());
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
