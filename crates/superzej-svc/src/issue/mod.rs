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

/// Routes issue requests to the configured backend. Returns empty results
/// (not errors) when no provider is configured — the panel renders
/// gracefully regardless.
pub struct IssueRouter {
    inner: Option<RouterInner>,
}

impl IssueRouter {
    pub fn from_config(cfg: &IssuesConfig) -> Self {
        let inner = match cfg.provider {
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
        };
        IssueRouter { inner }
    }

    pub fn provider_id(&self) -> &'static str {
        self.inner
            .as_ref()
            .map(|b| b.provider_id())
            .unwrap_or("none")
    }

    pub fn is_configured(&self) -> bool {
        self.inner.is_some()
    }

    pub async fn list_issues(&self, filter: &IssueFilter) -> Result<Vec<Issue>, IssueError> {
        match &self.inner {
            Some(b) => b.list_issues(filter).await,
            None => Ok(vec![]),
        }
    }

    pub async fn get_issue(&self, id: &str) -> Result<IssueDetail, IssueError> {
        match &self.inner {
            Some(b) => b.get_issue(id).await,
            None => Err(IssueError::NotConfigured),
        }
    }

    pub async fn create_issue(&self, draft: &IssueDraft) -> Result<Issue, IssueError> {
        match &self.inner {
            Some(b) => b.create_issue(draft).await,
            None => Err(IssueError::NotConfigured),
        }
    }

    pub async fn update_issue(&self, id: &str, patch: &IssuePatch) -> Result<Issue, IssueError> {
        match &self.inner {
            Some(b) => b.update_issue(id, patch).await,
            None => Err(IssueError::NotConfigured),
        }
    }

    pub async fn search(&self, query: &str, limit: usize) -> Result<Vec<Issue>, IssueError> {
        match &self.inner {
            Some(b) => b.search(query, limit).await,
            None => Ok(vec![]),
        }
    }
}
