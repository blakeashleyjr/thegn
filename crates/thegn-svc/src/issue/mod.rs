//! Generic issue tracker trait + provider router.
//!
//! The `IssueBackend` trait is the single seam that all providers implement.
//! `IssueRouter` is the host-facing entry point: it reads `IssuesConfig`,
//! constructs the right backend, and forwards calls — returning empty
//! collections when no provider is configured rather than erroring, so the
//! panel always has something to render.

pub mod github;
pub mod jira;
pub mod linear;

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

impl From<reqwest::Error> for IssueError {
    fn from(e: reqwest::Error) -> Self {
        IssueError::Network(e)
    }
}

/// Provider-agnostic issue tracker seam.
#[allow(async_fn_in_trait)]
pub trait IssueBackend: Send + Sync {
    fn provider_id(&self) -> &'static str;

    async fn list_issues(&self, filter: &IssueFilter) -> Result<Vec<Issue>, IssueError>;
    async fn get_issue(&self, id: &str) -> Result<IssueDetail, IssueError>;
    async fn create_issue(&self, draft: &IssueDraft) -> Result<Issue, IssueError>;
    async fn update_issue(&self, id: &str, patch: &IssuePatch) -> Result<Issue, IssueError>;
    async fn search(&self, query: &str, limit: usize) -> Result<Vec<Issue>, IssueError>;
}

/// Concrete backend variants (enum dispatch avoids dyn-incompatibility of
/// async fn in trait while still abstracting over providers).
enum RouterInner {
    Linear(linear::LinearBackend),
    Github(github::GitHubIssuesBackend),
    Jira(jira::JiraBackend),
}

impl RouterInner {
    fn provider_id(&self) -> &'static str {
        match self {
            RouterInner::Linear(_) => "linear",
            RouterInner::Github(_) => "github",
            RouterInner::Jira(_) => "jira",
        }
    }

    async fn list_issues(&self, filter: &IssueFilter) -> Result<Vec<Issue>, IssueError> {
        match self {
            RouterInner::Linear(b) => b.list_issues(filter).await,
            RouterInner::Github(b) => b.list_issues(filter).await,
            RouterInner::Jira(b) => b.list_issues(filter).await,
        }
    }

    async fn get_issue(&self, id: &str) -> Result<IssueDetail, IssueError> {
        match self {
            RouterInner::Linear(b) => b.get_issue(id).await,
            RouterInner::Github(b) => b.get_issue(id).await,
            RouterInner::Jira(b) => b.get_issue(id).await,
        }
    }

    async fn create_issue(&self, draft: &IssueDraft) -> Result<Issue, IssueError> {
        match self {
            RouterInner::Linear(b) => b.create_issue(draft).await,
            RouterInner::Github(b) => b.create_issue(draft).await,
            RouterInner::Jira(b) => b.create_issue(draft).await,
        }
    }

    async fn update_issue(&self, id: &str, patch: &IssuePatch) -> Result<Issue, IssueError> {
        match self {
            RouterInner::Linear(b) => b.update_issue(id, patch).await,
            RouterInner::Github(b) => b.update_issue(id, patch).await,
            RouterInner::Jira(b) => b.update_issue(id, patch).await,
        }
    }

    async fn search(&self, query: &str, limit: usize) -> Result<Vec<Issue>, IssueError> {
        match self {
            RouterInner::Linear(b) => b.search(query, limit).await,
            RouterInner::Github(b) => b.search(query, limit).await,
            RouterInner::Jira(b) => b.search(query, limit).await,
        }
    }
}

impl RouterInner {
    /// Build a backend from one named account's token + scope. Returns `None`
    /// for a `None`-provider account.
    fn from_account(a: &IssueAccount) -> Option<Self> {
        match a.provider {
            IssueProviderKind::Linear => {
                let api_key = expand_env_ref(&a.token).unwrap_or_default();
                let team_id = (!a.team_id.is_empty()).then(|| a.team_id.clone());
                Some(RouterInner::Linear(linear::LinearBackend::new(
                    api_key, team_id,
                )))
            }
            IssueProviderKind::Github => Some(RouterInner::Github(
                github::GitHubIssuesBackend::new(a.extra_flags.clone()),
            )),
            IssueProviderKind::Jira => {
                let api_token = expand_env_ref(&a.token).unwrap_or_default();
                Some(RouterInner::Jira(jira::JiraBackend::new(
                    a.base_url.clone(),
                    a.email.clone(),
                    api_token,
                    (!a.project_key.is_empty()).then(|| a.project_key.clone()),
                )))
            }
            IssueProviderKind::None => None,
        }
    }
}

/// A configured backend tagged with the account name it was built from, so the
/// cache and "My Work" feed can key each provider's issues by `(provider,
/// account)` — supporting multiple accounts of the same provider.
struct AccountBackend {
    account: String,
    inner: RouterInner,
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
    pub fn from_config(cfg: &IssuesConfig) -> Self {
        let inner = cfg
            .active_accounts()
            .into_iter()
            .filter_map(|acct| {
                RouterInner::from_account(&acct).map(|inner| AccountBackend {
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
    fn backend_for_id(&self, id: &str) -> Option<&RouterInner> {
        let prefix = id.split_once(':').map(|(p, _)| p).unwrap_or(id);
        self.inner
            .iter()
            .find(|b| b.inner.provider_id() == prefix)
            .map(|b| &b.inner)
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
        ]));
        assert_eq!(r.provider_ids(), vec!["linear", "jira", "github"]);
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
        ]));
        assert_eq!(
            r.backend_for_id("jira:PROJ-1").map(|b| b.provider_id()),
            Some("jira")
        );
        assert_eq!(
            r.backend_for_id("linear:ABC-9").map(|b| b.provider_id()),
            Some("linear")
        );
        // An id for a provider that isn't configured routes nowhere.
        assert!(r.backend_for_id("github:42").is_none());
        // A bare id with no prefix also routes nowhere.
        assert!(r.backend_for_id("nonsense").is_none());
    }
}
