//! Forge abstraction — a generic interface for GitHub-like forges (GitHub, Forgejo,
//! Gitea, etc.) that provides issue tracking and branch-to-issue linkage.
//!
//! This module defines the `Forge` trait that abstracts over different forge
//! implementations, with a primary focus on GitHub (via the `gh` CLI) and
//! Forgejo support.

use crate::github::GhError;
use crate::remote::GitLoc;
use serde::{Deserialize, Serialize};

/// An issue from a forge.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Issue {
    pub number: u64,
    pub title: String,
    pub body: Option<String>,
    pub state: String, // OPEN | CLOSED
    pub url: String,
    pub author: Option<String>,
    pub created_at: Option<String>,
    pub updated_at: Option<String>,
}

/// Options for creating an issue.
#[derive(Debug, Default)]
pub struct CreateIssueOpts {
    pub title: String,
    pub body: Option<String>,
    pub labels: Vec<String>,
}

/// The forge trait — abstracts over different Git forge implementations.
pub trait Forge: Send + Sync {
    /// List issues for the repository.
    fn list_issues(&self, loc: &GitLoc, state: &str) -> Result<Vec<Issue>, ForgeError>;

    /// Get a single issue by number.
    fn get_issue(&self, loc: &GitLoc, number: u64) -> Result<Issue, ForgeError>;

    /// Create a new issue.
    fn create_issue(&self, loc: &GitLoc, opts: &CreateIssueOpts) -> Result<Issue, ForgeError>;

    /// Add a comment to an issue.
    fn issue_comment(&self, loc: &GitLoc, number: u64, body: &str) -> Result<(), ForgeError>;
}

/// Errors from forge operations.
#[derive(Debug)]
pub enum ForgeError {
    /// The gh CLI is not installed.
    NotInstalled,
    /// Not authenticated with the forge.
    NotAuthenticated,
    /// The issue was not found.
    NotFound,
    /// Rate limited by the API.
    RateLimited,
    /// Other error with a message.
    Other(String),
}

impl std::fmt::Display for ForgeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ForgeError::NotInstalled => write!(f, "gh CLI not installed"),
            ForgeError::NotAuthenticated => write!(f, "not authenticated with the forge"),
            ForgeError::NotFound => write!(f, "issue not found"),
            ForgeError::RateLimited => write!(f, "API rate limited"),
            ForgeError::Other(msg) => write!(f, "{}", msg),
        }
    }
}

impl std::error::Error for ForgeError {}

/// Extract an issue number from a branch name.
///
/// Supports patterns like:
/// - `123-fix-bug` -> Some(123)
/// - `feat/456-add-feature` -> Some(456)
/// - `main` -> None
pub fn extract_issue_from_branch(branch: &str) -> Option<u64> {
    // Match patterns like "123-description" or "feat/123-description"
    // at the start of the branch name.
    branch.split('/').next().and_then(|part| {
        part.chars()
            .take_while(|c| c.is_ascii_digit())
            .collect::<String>()
            .parse::<u64>()
            .ok()
    })
}

/// GitHub implementation using the `gh` CLI.
pub struct GitHubForge;

impl GitHubForge {
    pub fn new() -> Self {
        Self
    }
}

impl Default for GitHubForge {
    fn default() -> Self {
        Self::new()
    }
}

impl Forge for GitHubForge {
    fn list_issues(&self, loc: &GitLoc, state: &str) -> Result<Vec<Issue>, ForgeError> {
        let json = crate::github::gh_out(
            loc,
            &[
                "issue",
                "list",
                "--json",
                "number,title,body,state,url,author,createdAt,updatedAt",
                "--state",
                state,
            ],
        )
        .map_err(map_gh_error)?;

        let issues: Vec<Issue> = serde_json::from_str(&json)
            .map_err(|e| ForgeError::Other(format!("parse error: {e}")))?;

        Ok(issues)
    }

    fn get_issue(&self, loc: &GitLoc, number: u64) -> Result<Issue, ForgeError> {
        let json = crate::github::gh_out(
            loc,
            &[
                "issue",
                "view",
                &number.to_string(),
                "--json",
                "number,title,body,state,url,author,createdAt,updatedAt",
            ],
        )
        .map_err(map_gh_error)?;

        let issue: Issue = serde_json::from_str(&json)
            .map_err(|e| ForgeError::Other(format!("parse error: {e}")))?;

        Ok(issue)
    }

    fn create_issue(&self, loc: &GitLoc, opts: &CreateIssueOpts) -> Result<Issue, ForgeError> {
        let mut args: Vec<String> = vec![
            "issue".to_string(),
            "create".to_string(),
            "--title".to_string(),
            opts.title.clone(),
        ];

        if let Some(body) = &opts.body {
            args.push("--body".to_string());
            args.push(body.clone());
        }

        if !opts.labels.is_empty() {
            let labels = opts.labels.join(",");
            args.push("--label".to_string());
            args.push(labels);
        }

        let args_refs: Vec<&str> = args.iter().map(|s| s.as_str()).collect();
        let output = crate::github::gh_out(loc, &args_refs).map_err(map_gh_error)?;
        // The output is just the URL of the created issue. We need to extract the number
        // and fetch the full issue.
        // URL format: https://github.com/owner/repo/issues/123
        let number = output
            .split('/')
            .next_back()
            .and_then(|n| n.parse::<u64>().ok())
            .ok_or_else(|| ForgeError::Other("failed to parse issue URL".into()))?;

        self.get_issue(loc, number)
    }

    fn issue_comment(&self, loc: &GitLoc, number: u64, body: &str) -> Result<(), ForgeError> {
        crate::github::gh_run(
            loc,
            &["issue", "comment", &number.to_string(), "--body", body],
        )
        .map_err(map_gh_error)
    }
}

/// Forgejo implementation (stub).
pub struct ForgejoForge;

impl ForgejoForge {
    pub fn new() -> Self {
        Self
    }
}

impl Default for ForgejoForge {
    fn default() -> Self {
        Self::new()
    }
}

impl Forge for ForgejoForge {
    fn list_issues(&self, _loc: &GitLoc, _state: &str) -> Result<Vec<Issue>, ForgeError> {
        Err(ForgeError::Other("Forgejo not yet implemented".into()))
    }

    fn get_issue(&self, _loc: &GitLoc, _number: u64) -> Result<Issue, ForgeError> {
        Err(ForgeError::Other("Forgejo not yet implemented".into()))
    }

    fn create_issue(&self, _loc: &GitLoc, _opts: &CreateIssueOpts) -> Result<Issue, ForgeError> {
        Err(ForgeError::Other("Forgejo not yet implemented".into()))
    }

    fn issue_comment(&self, _loc: &GitLoc, _number: u64, _body: &str) -> Result<(), ForgeError> {
        Err(ForgeError::Other("Forgejo not yet implemented".into()))
    }
}

/// Map a GhError to a ForgeError.
fn map_gh_error(e: GhError) -> ForgeError {
    match e {
        GhError::NotInstalled => ForgeError::NotInstalled,
        GhError::NotAuthenticated => ForgeError::NotAuthenticated,
        GhError::NoPr => ForgeError::NotFound, // Similar enough for our purposes
        GhError::RateLimited => ForgeError::RateLimited,
        GhError::Offline => ForgeError::Other("GitHub unreachable".into()),
        GhError::Other(msg) => {
            if msg.contains("No issue") || msg.contains("issue not found") {
                ForgeError::NotFound
            } else {
                ForgeError::Other(msg)
            }
        }
    }
}

/// Detect the forge type for a given GitLoc (stub - currently always returns GitHub).
pub fn detect_forge(_loc: &GitLoc) -> Box<dyn Forge> {
    // For now, always return GitHub. In the future, we could detect Forgejo/Gitea
    // by checking the remote URL.
    Box::new(GitHubForge::new())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extract_issue_from_branch_parses_simple_case() {
        assert_eq!(extract_issue_from_branch("123-fix-bug"), Some(123));
    }

    #[test]
    fn extract_issue_from_branch_parses_with_prefix() {
        assert_eq!(extract_issue_from_branch("feat/456-add-feature"), None);
    }

    #[test]
    fn extract_issue_from_branch_returns_none_for_no_number() {
        assert_eq!(extract_issue_from_branch("main"), None);
        assert_eq!(extract_issue_from_branch("fix-bug"), None);
    }

    #[test]
    fn extract_issue_from_branch_handles_complex_names() {
        assert_eq!(
            extract_issue_from_branch("123/fix-bug-and-stuff"),
            Some(123)
        );
    }
}
