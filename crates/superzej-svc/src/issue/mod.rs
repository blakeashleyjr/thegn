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

use superzej_core::config::{IssueProviderKind, IssuesConfig, expand_env_ref};
use superzej_core::issue::{Issue, IssueDetail, IssueDraft, IssueFilter, IssuePatch};

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
    fn from_kind(kind: IssueProviderKind, cfg: &IssuesConfig) -> Option<Self> {
        match kind {
            IssueProviderKind::Linear => {
                let api_key = expand_env_ref(&cfg.linear.api_key).unwrap_or_default();
                let team_id = if cfg.linear.team_id.is_empty() {
                    None
                } else {
                    Some(cfg.linear.team_id.clone())
                };
                Some(RouterInner::Linear(linear::LinearBackend::new(
                    api_key, team_id,
                )))
            }
            IssueProviderKind::Github => Some(RouterInner::Github(
                github::GitHubIssuesBackend::new(cfg.github_issues.extra_flags.clone()),
            )),
            IssueProviderKind::Jira => {
                let api_token = expand_env_ref(&cfg.jira.api_token).unwrap_or_default();
                Some(RouterInner::Jira(jira::JiraBackend::new(
                    cfg.jira.base_url.clone(),
                    cfg.jira.email.clone(),
                    api_token,
                    if cfg.jira.project_key.is_empty() {
                        None
                    } else {
                        Some(cfg.jira.project_key.clone())
                    },
                )))
            }
            IssueProviderKind::None => None,
        }
    }
}

/// Routes issue requests across every configured provider. `list`/`search` fan
/// out and merge; `get`/`update` dispatch by the `"<provider>:"` id prefix.
/// Returns empty results (not errors) when nothing is configured — the panel
/// renders gracefully regardless. A single provider failing never breaks the
/// others: it logs and contributes nothing to the merged result.
pub struct IssueRouter {
    inner: Vec<RouterInner>,
}

impl IssueRouter {
    pub fn from_config(cfg: &IssuesConfig) -> Self {
        let inner = cfg
            .active_providers()
            .into_iter()
            .filter_map(|kind| RouterInner::from_kind(kind, cfg))
            .collect();
        IssueRouter { inner }
    }

    /// The provider id of the first configured backend (`"none"` when empty).
    /// Retained for callers that only need a representative id.
    pub fn provider_id(&self) -> &'static str {
        self.inner
            .first()
            .map(|b| b.provider_id())
            .unwrap_or("none")
    }

    /// Every configured provider id, in config order.
    pub fn provider_ids(&self) -> Vec<&'static str> {
        self.inner.iter().map(|b| b.provider_id()).collect()
    }

    pub fn is_configured(&self) -> bool {
        !self.inner.is_empty()
    }

    /// Locate the backend owning an id of the form `"<provider>:<key>"`.
    fn backend_for_id(&self, id: &str) -> Option<&RouterInner> {
        let prefix = id.split_once(':').map(|(p, _)| p).unwrap_or(id);
        self.inner.iter().find(|b| b.provider_id() == prefix)
    }

    /// List issues across all providers, concatenated. A failing provider logs
    /// and contributes nothing rather than failing the whole call.
    pub async fn list_issues(&self, filter: &IssueFilter) -> Result<Vec<Issue>, IssueError> {
        let mut all = Vec::new();
        for b in &self.inner {
            match b.list_issues(filter).await {
                Ok(mut issues) => all.append(&mut issues),
                Err(e) => {
                    tracing::warn!(provider = b.provider_id(), error = %e, "issue list failed")
                }
            }
        }
        Ok(all)
    }

    /// Per-provider results, so callers (the cache refresh) can store and diff
    /// each provider under its own `(repo_root, provider)` key.
    pub async fn list_per_provider(
        &self,
        filter: &IssueFilter,
    ) -> Vec<(&'static str, Result<Vec<Issue>, IssueError>)> {
        let mut out = Vec::with_capacity(self.inner.len());
        for b in &self.inner {
            out.push((b.provider_id(), b.list_issues(filter).await));
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
            Some(b) => b.create_issue(draft).await,
            None => Err(IssueError::NotConfigured),
        }
    }

    pub async fn update_issue(&self, id: &str, patch: &IssuePatch) -> Result<Issue, IssueError> {
        match self.backend_for_id(id) {
            Some(b) => b.update_issue(id, patch).await,
            None => Err(IssueError::NotConfigured),
        }
    }

    /// Search across all providers, concatenated.
    pub async fn search(&self, query: &str, limit: usize) -> Result<Vec<Issue>, IssueError> {
        let mut all = Vec::new();
        for b in &self.inner {
            match b.search(query, limit).await {
                Ok(mut issues) => all.append(&mut issues),
                Err(e) => {
                    tracing::warn!(provider = b.provider_id(), error = %e, "issue search failed")
                }
            }
        }
        Ok(all)
    }
}

#[cfg(test)]
mod spec {
    use super::*;
    use superzej_core::config::IssueProviderKind;

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
    #[allow(clippy::field_reassign_with_default)]
    fn single_provider_back_compat() {
        let mut cfg = IssuesConfig::default();
        cfg.provider = IssueProviderKind::Linear;
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
